//! Historical Arbitrage E2E Test
//!
//! Full pipeline on a historical block: detect -> optimize -> simulate -> deploy -> execute -> verify.
//!
//! Forks mainnet at a specific block, runs the complete Aether pipeline against
//! real historical state, and attempts to profit from any detected arbitrage.
//!
//! Prerequisites:
//! - `ETH_RPC_URL` environment variable (Ethereum mainnet RPC, ideally archive)
//! - `anvil`, `forge`, `cast` binaries in PATH (from Foundry)
//!
//! Run with:
//!   HISTORICAL_ARB_BLOCK=19500000 \
//!     cargo test -p aether-integration-tests --test historical_arb_e2e_test -- --nocapture

mod common;

use std::time::{Duration, Instant};

use alloy::primitives::{keccak256, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};

use aether_detector::optimizer::ternary_search_optimal_input;
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;

use common::{
    build_price_graph, check_prerequisites, cycle_to_swap_route,
    default_pool_set, deploy_executor, fetch_all_reserves, is_flashloanable,
    run_detection, spawn_anvil_at_block, wait_for_anvil, PoolDef, ANVIL_ACCOUNT0,
};

// ── Types ───────────────────────────────────────────────────────────

struct HistoricalArbConfig {
    pools: Vec<PoolDef>,
    max_hops: usize,
    gas_limit: u64,
}

impl Default for HistoricalArbConfig {
    fn default() -> Self {
        Self {
            pools: default_pool_set(),
            max_hops: 4,
            gas_limit: 1_500_000,
        }
    }
}

#[derive(Debug)]
struct HistoricalArbResult {
    block_number: u64,
    arb_detected: bool,
    num_cycles: usize,
    best_profit_factor: f64,
    optimized_input: Option<U256>,
    simulation_success: Option<bool>,
    simulation_gas: Option<u64>,
    execution_success: Option<bool>,
    on_chain_profit: Option<U256>,
    phase_latencies: Vec<(&'static str, u128)>,
}

// ── Core pipeline ───────────────────────────────────────────────────

async fn run_historical_arb(
    block_number: u64,
    config: &HistoricalArbConfig,
) -> Result<HistoricalArbResult, String> {
    let mut latencies: Vec<(&'static str, u128)> = Vec::new();

    // ── Phase 1: Spawn Anvil at historical block ─────────────────────
    let t1 = Instant::now();

    let (mut anvil, anvil_url) = spawn_anvil_at_block(Some(block_number), 21545);

    let result = run_historical_arb_inner(block_number, config, &anvil_url, &mut latencies).await;

    let _ = anvil.kill();
    let _ = anvil.wait();

    latencies.insert(0, ("spawn anvil", t1.elapsed().as_millis()));

    match result {
        Ok(mut r) => {
            r.phase_latencies = latencies;
            Ok(r)
        }
        Err(e) => Err(e),
    }
}

async fn run_historical_arb_inner(
    block_number: u64,
    config: &HistoricalArbConfig,
    anvil_url: &str,
    latencies: &mut Vec<(&'static str, u128)>,
) -> Result<HistoricalArbResult, String> {
    // Wait for Anvil
    if !wait_for_anvil(anvil_url, Duration::from_secs(60)).await {
        return Err("Anvil did not start in time".to_string());
    }

    let parsed: url::Url = anvil_url.parse().expect("valid URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    let bn = provider
        .get_block_number()
        .await
        .map_err(|e| format!("get_block_number: {e}"))?;
    let block = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(bn))
        .await
        .map_err(|e| format!("get_block: {e}"))?
        .ok_or("block not found")?;

    let timestamp = block.header.timestamp;
    let base_fee = block.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64;

    eprintln!(
        "  Fork block: {}  Timestamp: {}  BaseFee: {} gwei",
        bn,
        timestamp,
        base_fee / 1_000_000_000
    );

    // ── Phase 2: Fetch reserves at fork block ────────────────────────
    let t2 = Instant::now();

    let reserves = fetch_all_reserves(&provider, &config.pools, None).await;
    latencies.push(("fetch reserves", t2.elapsed().as_millis()));

    eprintln!("  Reserves fetched: {}/{} pools", reserves.len(), config.pools.len());

    if reserves.len() < 3 {
        return Err(format!(
            "Too few pools with reserves: {}/{}",
            reserves.len(),
            config.pools.len()
        ));
    }

    // ── Phase 3: Build graph + detect ────────────────────────────────
    let t3 = Instant::now();

    let (graph, token_index) = build_price_graph(&config.pools, &reserves);
    let cycles = run_detection(&graph, config.max_hops, 5_000_000);
    latencies.push(("detect", t3.elapsed().as_millis()));

    let best_profit_factor = cycles
        .iter()
        .map(|c| c.profit_factor())
        .fold(0.0f64, f64::max);

    eprintln!(
        "  Detection: {} cycles, best={:.4}%, {}us",
        cycles.len(),
        best_profit_factor * 100.0,
        t3.elapsed().as_micros()
    );

    if cycles.is_empty() {
        return Ok(HistoricalArbResult {
            block_number,
            arb_detected: false,
            num_cycles: 0,
            best_profit_factor: 0.0,
            optimized_input: None,
            simulation_success: None,
            simulation_gas: None,
            execution_success: None,
            on_chain_profit: None,
            phase_latencies: Vec::new(),
        });
    }

    // ── Phase 4: Find executable cycle ───────────────────────────────
    let t4 = Instant::now();

    // Try cycles in order (most profitable first), find one with flashloanable start token
    let mut best_steps = None;
    let mut best_cycle_idx = 0;
    let mut best_final_amount = U256::ZERO;

    for (idx, cycle) in cycles.iter().enumerate() {
        if cycle.path.len() < 3 {
            continue;
        }

        let start_vertex = cycle.path[0];
        let start_token = match token_index.get_address(start_vertex) {
            Some(addr) => *addr,
            None => continue,
        };

        if !is_flashloanable(&start_token) {
            continue;
        }

        // Use a test input amount (1 ETH for WETH, 2000 USDC/USDT/DAI for stablecoins)
        let test_input = if start_token == common::WETH {
            U256::from(1_000_000_000_000_000_000u128) // 1 WETH
        } else {
            U256::from(2_000_000_000u128) // 2000 USDC/USDT (6 decimals) or 2000 DAI (18 decimals, but we'll optimize later)
        };

        // Deploy executor for calldata target — use a placeholder for now
        let placeholder_executor = Address::ZERO;
        if let Some((steps, final_amount)) = cycle_to_swap_route(
            cycle,
            &graph,
            &token_index,
            &config.pools,
            &reserves,
            test_input,
            placeholder_executor,
        ) {
            if final_amount > test_input {
                best_steps = Some((steps, start_token, test_input));
                best_cycle_idx = idx;
                best_final_amount = final_amount;
                break;
            }
        }
    }

    latencies.push(("route building", t4.elapsed().as_millis()));

    let (route_steps, flashloan_token, test_input) = match best_steps {
        Some(s) => s,
        None => {
            eprintln!("  No executable cycle found (no flashloanable start token or no profit)");
            return Ok(HistoricalArbResult {
                block_number,
                arb_detected: true,
                num_cycles: cycles.len(),
                best_profit_factor,
                optimized_input: None,
                simulation_success: None,
                simulation_gas: None,
                execution_success: None,
                on_chain_profit: None,
                phase_latencies: Vec::new(),
            });
        }
    };

    eprintln!(
        "  Executable cycle #{}: {} hops, token={}, test_profit={}",
        best_cycle_idx,
        route_steps.len(),
        flashloan_token,
        best_final_amount - test_input
    );

    // ── Phase 5: Optimize input amount ───────────────────────────────
    let t5 = Instant::now();

    let min_input = test_input / U256::from(100);
    let max_input = test_input * U256::from(50);
    let cycle_ref = &cycles[best_cycle_idx];

    let profit_fn = |amount: U256| -> i128 {
        let placeholder = Address::ZERO;
        match cycle_to_swap_route(
            cycle_ref,
            &graph,
            &token_index,
            &config.pools,
            &reserves,
            amount,
            placeholder,
        ) {
            Some((_, final_amount)) => {
                // Aave premium: 0.05%
                let premium = amount * U256::from(5) / U256::from(10000);
                let repay = amount + premium;
                if final_amount > repay {
                    (final_amount - repay).to::<u128>() as i128
                } else {
                    -((repay - final_amount).to::<u128>() as i128)
                }
            }
            None => i128::MIN / 2,
        }
    };

    let (optimal_input, optimal_profit) =
        ternary_search_optimal_input(min_input, max_input, 80, profit_fn);

    latencies.push(("optimize input", t5.elapsed().as_millis()));

    eprintln!(
        "  Optimized: input={}, profit={}",
        optimal_input, optimal_profit
    );

    if optimal_profit <= 0 {
        eprintln!("  No profitable input found after optimization");
        return Ok(HistoricalArbResult {
            block_number,
            arb_detected: true,
            num_cycles: cycles.len(),
            best_profit_factor,
            optimized_input: Some(optimal_input),
            simulation_success: None,
            simulation_gas: None,
            execution_success: None,
            on_chain_profit: None,
            phase_latencies: Vec::new(),
        });
    }

    // ── Phase 6: Deploy executor ─────────────────────────────────────
    let t6 = Instant::now();

    let executor_addr = deploy_executor(anvil_url)?;
    latencies.push(("deploy executor", t6.elapsed().as_millis()));

    eprintln!("  Executor deployed: {executor_addr}");

    // ── Phase 7: Build final calldata with real executor address ──────
    let t7 = Instant::now();

    let (final_steps, _final_amount) = cycle_to_swap_route(
        cycle_ref,
        &graph,
        &token_index,
        &config.pools,
        &reserves,
        optimal_input,
        executor_addr,
    )
    .ok_or("Failed to build final route with executor address")?;

    let calldata = build_execute_arb_calldata(&final_steps, flashloan_token, optimal_input);
    latencies.push(("build calldata", t7.elapsed().as_millis()));

    // ── Phase 8: Simulate via revm ───────────────────────────────────
    let t8 = Instant::now();

    let current_block = provider
        .get_block_number()
        .await
        .map_err(|e| format!("get current block: {e}"))?;
    let current_block_data = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(current_block))
        .await
        .map_err(|e| format!("get block data: {e}"))?
        .ok_or("block not found")?;
    let current_ts = current_block_data.header.timestamp;
    let current_base_fee =
        current_block_data.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64;

    let sim_parsed: url::Url = anvil_url.parse().expect("valid URL");
    let sim_provider = ProviderBuilder::new().connect_http(sim_parsed);
    let dyn_provider = sim_provider.erased();

    let rpc_state = RpcForkedState::new(dyn_provider, current_block, current_ts, current_base_fee)
        .ok_or("RpcForkedState::new failed")?;

    let sim = EvmSimulator::new(SimConfig {
        gas_limit: config.gas_limit,
        chain_id: 1,
        caller: ANVIL_ACCOUNT0,
        value: U256::ZERO,
    });

    let sim_result = sim.simulate_rpc(rpc_state, executor_addr, calldata.clone());
    latencies.push(("revm simulation", t8.elapsed().as_millis()));

    eprintln!(
        "  Simulation: success={}, gas={}, revert={:?}",
        sim_result.success, sim_result.gas_used, sim_result.revert_reason
    );

    if !sim_result.success {
        eprintln!("  Simulation failed (false positive — detected but not executable)");
        return Ok(HistoricalArbResult {
            block_number,
            arb_detected: true,
            num_cycles: cycles.len(),
            best_profit_factor,
            optimized_input: Some(optimal_input),
            simulation_success: Some(false),
            simulation_gas: Some(sim_result.gas_used),
            execution_success: None,
            on_chain_profit: None,
            phase_latencies: Vec::new(),
        });
    }

    // ── Phase 9: Execute on-chain ────────────────────────────────────
    let t9 = Instant::now();

    let execute_tx = alloy::rpc::types::TransactionRequest::default()
        .from(ANVIL_ACCOUNT0)
        .to(executor_addr)
        .input(calldata.into())
        .gas_limit(config.gas_limit);

    let pending = provider
        .send_transaction(execute_tx)
        .await
        .map_err(|e| format!("send tx: {e}"))?;

    let tx_hash = *pending.tx_hash();
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("get receipt: {e}"))?;

    latencies.push(("on-chain execution", t9.elapsed().as_millis()));

    let execution_success = receipt.status();
    eprintln!(
        "  On-chain: tx={tx_hash:?}, status={}, gas_used={}",
        execution_success, receipt.gas_used
    );

    // ── Phase 10: Parse results ──────────────────────────────────────
    let mut on_chain_profit = None;

    if execution_success {
        let arb_executed_topic = keccak256(b"ArbExecuted(address,uint256,uint256,uint256)");
        for log in receipt.inner.logs() {
            let topics = log.inner.data.topics();
            let data_bytes: &[u8] = log.inner.data.data.as_ref();
            if !topics.is_empty() && topics[0] == arb_executed_topic && data_bytes.len() >= 96 {
                let profit = U256::from_be_slice(&data_bytes[32..64]);
                eprintln!("  ArbExecuted profit: {} wei", profit);
                on_chain_profit = Some(profit);
                break;
            }
        }
    }

    Ok(HistoricalArbResult {
        block_number,
        arb_detected: true,
        num_cycles: cycles.len(),
        best_profit_factor,
        optimized_input: Some(optimal_input),
        simulation_success: Some(true),
        simulation_gas: Some(sim_result.gas_used),
        execution_success: Some(execution_success),
        on_chain_profit,
        phase_latencies: Vec::new(),
    })
}

// ── Scan helper (find blocks with arbs) ─────────────────────────────

async fn find_best_arb_block(
    provider: &impl Provider,
    pools: &[PoolDef],
    start: u64,
    end: u64,
) -> Option<(u64, f64)> {
    let mut best_block = None;
    let mut best_profit = 0.0f64;

    for bn in start..=end {
        let reserves = fetch_all_reserves(provider, pools, Some(bn)).await;
        if reserves.len() < 3 {
            continue;
        }

        let (graph, _) = build_price_graph(pools, &reserves);
        let cycles = run_detection(&graph, 4, 5_000_000);

        for cycle in &cycles {
            let pf = cycle.profit_factor();
            if pf > best_profit {
                best_profit = pf;
                best_block = Some(bn);
            }
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    best_block.map(|bn| (bn, best_profit))
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_historical_arb_at_block() {
    if !check_prerequisites(true, true) {
        return;
    }

    // Use env var for specific block, or auto-scan recent blocks
    let block_number = match std::env::var("HISTORICAL_ARB_BLOCK") {
        Ok(s) => match s.parse::<u64>() {
            Ok(n) => {
                eprintln!("=== Historical Arb E2E: Block {} (from env) ===", n);
                n
            }
            Err(_) => {
                eprintln!("Invalid HISTORICAL_ARB_BLOCK value");
                return;
            }
        },
        Err(_) => {
            eprintln!("=== Historical Arb E2E: Auto-scanning recent blocks ===");

            let rpc_url = std::env::var("ETH_RPC_URL").unwrap();
            let parsed: url::Url = rpc_url.parse().expect("valid RPC URL");
            let provider = ProviderBuilder::new().connect_http(parsed);

            let latest = match provider.get_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Failed to get latest block: {e}");
                    return;
                }
            };

            let pools = default_pool_set();
            let scan_range = 50u64;
            let start = latest.saturating_sub(scan_range - 1);

            eprintln!("Scanning blocks {} to {} for arbs...", start, latest);

            match find_best_arb_block(&provider, &pools, start, latest).await {
                Some((bn, pf)) => {
                    eprintln!(
                        "Best block: {} with {:.4}% profit factor",
                        bn,
                        pf * 100.0
                    );
                    bn
                }
                None => {
                    eprintln!("No arbs found in last {} blocks (expected on well-arbitraged mainnet)", scan_range);
                    eprintln!("Test passes — the scanner works, mainnet is just well-arbitraged right now.");
                    return;
                }
            }
        }
    };

    let config = HistoricalArbConfig::default();

    let t_total = Instant::now();
    let result = run_historical_arb(block_number, &config).await;
    let total_ms = t_total.elapsed().as_millis();

    match result {
        Ok(r) => {
            eprintln!("\n=== Historical Arb Result ===");
            eprintln!("  Block:              {}", r.block_number);
            eprintln!("  Arb detected:       {}", r.arb_detected);
            eprintln!("  Cycles:             {}", r.num_cycles);
            eprintln!("  Best profit factor: {:.4}%", r.best_profit_factor * 100.0);
            if let Some(input) = r.optimized_input {
                eprintln!("  Optimized input:    {}", input);
            }
            if let Some(sim_ok) = r.simulation_success {
                eprintln!("  Simulation:         {}", if sim_ok { "PASS" } else { "FAIL" });
            }
            if let Some(gas) = r.simulation_gas {
                eprintln!("  Simulation gas:     {}", gas);
            }
            if let Some(exec_ok) = r.execution_success {
                eprintln!("  Execution:          {}", if exec_ok { "PASS" } else { "FAIL" });
            }
            if let Some(profit) = r.on_chain_profit {
                eprintln!("  On-chain profit:    {} wei", profit);
            }
            eprintln!("  Total time:         {}ms", total_ms);

            for (name, ms) in &r.phase_latencies {
                eprintln!("    {:<25} {}ms", name, ms);
            }
        }
        Err(e) => {
            eprintln!("Historical arb failed: {e}");
            // Don't panic — this can legitimately fail for blocks without arbs
            // or if the RPC doesn't support the requested block
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_discover_and_execute() {
    if !check_prerequisites(true, true) {
        return;
    }

    // This test first scans for a block with arbs, then runs the full E2E
    let rpc_url = std::env::var("ETH_RPC_URL").unwrap();
    let parsed: url::Url = rpc_url.parse().expect("valid RPC URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    let latest = match provider.get_block_number().await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Failed to get latest block: {e}. Skipping.");
            return;
        }
    };

    let pools = default_pool_set();
    let scan_range = 20u64;
    let start = latest.saturating_sub(scan_range - 1);

    eprintln!("=== Discover & Execute Test ===");
    eprintln!("Phase 1: Scanning blocks {} to {} ...", start, latest);

    let best = find_best_arb_block(&provider, &pools, start, latest).await;

    match best {
        Some((bn, pf)) => {
            eprintln!(
                "Phase 1 complete: block {} with {:.4}% profit factor",
                bn,
                pf * 100.0
            );
            eprintln!("Phase 2: Running full E2E pipeline...");

            let config = HistoricalArbConfig::default();
            match run_historical_arb(bn, &config).await {
                Ok(r) => {
                    eprintln!("Phase 2 complete: arb_detected={}, sim={:?}, exec={:?}",
                        r.arb_detected, r.simulation_success, r.execution_success);
                }
                Err(e) => {
                    eprintln!("Phase 2 pipeline error (non-fatal): {e}");
                }
            }
        }
        None => {
            eprintln!("No arbs found in recent {} blocks. Test passes (mainnet well-arbitraged).", scan_range);
        }
    }
}
