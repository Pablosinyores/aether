//! Multi-block backtester: aggregate metrics across a range of blocks.
//!
//! Scans a range of blocks for arb opportunities, then runs full E2E on the
//! most promising blocks (capped by `max_e2e_executions`).
//!
//! Prerequisites:
//! - `ETH_RPC_URL` environment variable (Ethereum mainnet RPC, ideally archive)
//! - `anvil`, `forge`, `cast` binaries in PATH (from Foundry)
//!
//! Run with:
//!   BACKTEST_BLOCKS=20 \
//!     cargo test -p aether-integration-tests --test multi_block_backtest_test -- --nocapture

mod common;

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};

use aether_detector::optimizer::ternary_search_optimal_input;
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;

use common::{
    build_price_graph, check_prerequisites, cycle_to_swap_route, default_pool_set,
    deploy_executor, fetch_all_reserves, is_flashloanable, run_detection,
    spawn_anvil_at_block, wait_for_anvil, PoolDef, ANVIL_ACCOUNT0,
};

// ── Types ───────────────────────────────────────────────────────────

struct ScanConfig {
    pools: Vec<PoolDef>,
    max_hops: usize,
    bf_timeout_us: u64,
    rate_limit_delay_ms: u64,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            pools: default_pool_set(),
            max_hops: 4,
            bf_timeout_us: 5_000_000,
            rate_limit_delay_ms: 100,
        }
    }
}

struct BacktestConfig {
    scan: ScanConfig,
    gas_limit: u64,
    max_e2e_executions: usize,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            scan: ScanConfig::default(),
            gas_limit: 1_500_000,
            max_e2e_executions: 3,
        }
    }
}

struct BlockScanEntry {
    block_number: u64,
    _pools_fetched: usize,
    cycles_detected: usize,
    best_profit_factor: f64,
    detection_us: u128,
}

#[derive(Debug, Default)]
struct BacktestMetrics {
    blocks_scanned: u64,
    blocks_with_arbs: u64,
    detection_rate_pct: f64,
    total_cycles: u64,
    simulations_run: u64,
    simulations_passed: u64,
    false_positive_rate_pct: f64,
    executions_run: u64,
    executions_passed: u64,
    profits_wei: Vec<U256>,
    gas_used: Vec<u64>,
    avg_detection_us: f64,
    total_elapsed_s: f64,
}

// ── Backtest runner ─────────────────────────────────────────────────

async fn run_backtest(
    config: &BacktestConfig,
    start_block: u64,
    end_block: u64,
) -> BacktestMetrics {
    let rpc_url = std::env::var("ETH_RPC_URL").unwrap();
    let parsed: url::Url = rpc_url.parse().expect("valid RPC URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    let t_total = Instant::now();
    let mut metrics = BacktestMetrics::default();

    // ── Phase 1: Scan all blocks ─────────────────────────────────────
    eprintln!("\n--- Phase 1: Scanning blocks {} to {} ---", start_block, end_block);

    let mut scan_results: Vec<BlockScanEntry> = Vec::new();

    for bn in start_block..=end_block {
        let reserves =
            fetch_all_reserves(&provider, &config.scan.pools, Some(bn)).await;

        let t_detect = Instant::now();
        let (graph, _) = build_price_graph(&config.scan.pools, &reserves);
        let cycles = run_detection(&graph, config.scan.max_hops, config.scan.bf_timeout_us);
        let detection_us = t_detect.elapsed().as_micros();

        let best_pf = cycles
            .iter()
            .map(|c| c.profit_factor())
            .fold(0.0f64, f64::max);

        let marker = if cycles.is_empty() {
            String::new()
        } else {
            format!(" ** {} cycles, {:.4}%", cycles.len(), best_pf * 100.0)
        };

        eprintln!(
            "  Block {}: {} pools, {}us{}",
            bn, reserves.len(), detection_us, marker
        );

        scan_results.push(BlockScanEntry {
            block_number: bn,
            _pools_fetched: reserves.len(),
            cycles_detected: cycles.len(),
            best_profit_factor: best_pf,
            detection_us,
        });

        if config.scan.rate_limit_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(config.scan.rate_limit_delay_ms)).await;
        }
    }

    metrics.blocks_scanned = scan_results.len() as u64;
    metrics.blocks_with_arbs = scan_results.iter().filter(|r| r.cycles_detected > 0).count() as u64;
    metrics.total_cycles = scan_results.iter().map(|r| r.cycles_detected as u64).sum();
    metrics.avg_detection_us = if scan_results.is_empty() {
        0.0
    } else {
        scan_results.iter().map(|r| r.detection_us as f64).sum::<f64>() / scan_results.len() as f64
    };
    metrics.detection_rate_pct = if metrics.blocks_scanned > 0 {
        (metrics.blocks_with_arbs as f64 / metrics.blocks_scanned as f64) * 100.0
    } else {
        0.0
    };

    // ── Phase 2: E2E on top-N blocks ─────────────────────────────────
    let mut candidates: Vec<&BlockScanEntry> = scan_results
        .iter()
        .filter(|r| r.cycles_detected > 0)
        .collect();

    candidates.sort_by(|a, b| {
        b.best_profit_factor
            .partial_cmp(&a.best_profit_factor)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let e2e_count = candidates.len().min(config.max_e2e_executions);
    let e2e_candidates: Vec<u64> = candidates.iter().take(e2e_count).map(|c| c.block_number).collect();

    if e2e_candidates.is_empty() {
        eprintln!("\n--- Phase 2: No arb blocks found, skipping E2E ---");
    } else {
        eprintln!(
            "\n--- Phase 2: Running E2E on {} best blocks ---",
            e2e_candidates.len()
        );

        for &bn in &e2e_candidates {
            eprintln!("\n  E2E for block {}...", bn);

            match run_single_e2e(bn, &config.scan.pools, config.gas_limit).await {
                Ok((sim_ok, exec_ok, profit, gas)) => {
                    metrics.simulations_run += 1;
                    if sim_ok {
                        metrics.simulations_passed += 1;
                    }
                    if let Some(exec) = exec_ok {
                        metrics.executions_run += 1;
                        if exec {
                            metrics.executions_passed += 1;
                        }
                    }
                    if let Some(p) = profit {
                        metrics.profits_wei.push(p);
                    }
                    if let Some(g) = gas {
                        metrics.gas_used.push(g);
                    }
                }
                Err(e) => {
                    eprintln!("    E2E error: {e}");
                }
            }
        }
    }

    metrics.false_positive_rate_pct = if metrics.simulations_run > 0 {
        let failures = metrics.simulations_run - metrics.simulations_passed;
        (failures as f64 / metrics.simulations_run as f64) * 100.0
    } else {
        0.0
    };

    metrics.total_elapsed_s = t_total.elapsed().as_secs_f64();

    metrics
}

/// Run a single E2E attempt on a historical block.
/// Returns (sim_success, execution_success, profit, gas_used).
async fn run_single_e2e(
    block_number: u64,
    pools: &[PoolDef],
    gas_limit: u64,
) -> Result<(bool, Option<bool>, Option<U256>, Option<u64>), String> {
    let (mut anvil, anvil_url) = spawn_anvil_at_block(Some(block_number), 22545);

    let result = run_single_e2e_inner(block_number, pools, gas_limit, &anvil_url).await;

    let _ = anvil.kill();
    let _ = anvil.wait();

    result
}

async fn run_single_e2e_inner(
    _block_number: u64,
    pools: &[PoolDef],
    gas_limit: u64,
    anvil_url: &str,
) -> Result<(bool, Option<bool>, Option<U256>, Option<u64>), String> {
    if !wait_for_anvil(anvil_url, Duration::from_secs(60)).await {
        return Err("Anvil did not start".to_string());
    }

    let parsed: url::Url = anvil_url.parse().expect("valid URL");
    let provider = ProviderBuilder::new().connect_http(parsed);

    let reserves = fetch_all_reserves(&provider, pools, None).await;
    let (graph, token_index) = build_price_graph(pools, &reserves);
    let cycles = run_detection(&graph, 4, 5_000_000);

    if cycles.is_empty() {
        return Err("No cycles at fork block".to_string());
    }

    // Find executable cycle
    let mut found_route = None;
    for cycle in &cycles {
        if cycle.path.len() < 3 {
            continue;
        }
        let start_token = match token_index.get_address(cycle.path[0]) {
            Some(addr) => *addr,
            None => continue,
        };
        if !is_flashloanable(&start_token) {
            continue;
        }

        let test_input = if start_token == common::WETH {
            U256::from(1_000_000_000_000_000_000u128)
        } else {
            U256::from(2_000_000_000u128)
        };

        if let Some((_, final_amount)) = cycle_to_swap_route(
            cycle, &graph, &token_index, pools, &reserves, test_input, Address::ZERO,
        ) {
            if final_amount > test_input {
                // Optimize
                let min_input = test_input / U256::from(100);
                let max_input = test_input * U256::from(50);
                let cycle_ref = cycle;
                let profit_fn = |amount: U256| -> i128 {
                    match cycle_to_swap_route(
                        cycle_ref, &graph, &token_index, pools, &reserves, amount, Address::ZERO,
                    ) {
                        Some((_, fa)) => {
                            let premium = amount * U256::from(5) / U256::from(10000);
                            let repay = amount + premium;
                            if fa > repay {
                                (fa - repay).to::<u128>() as i128
                            } else {
                                -((repay - fa).to::<u128>() as i128)
                            }
                        }
                        None => i128::MIN / 2,
                    }
                };

                let (optimal_input, optimal_profit) =
                    ternary_search_optimal_input(min_input, max_input, 80, profit_fn);

                if optimal_profit > 0 {
                    found_route = Some((cycle.clone(), start_token, optimal_input));
                    break;
                }
            }
        }
    }

    let (cycle, flashloan_token, optimal_input) = match found_route {
        Some(r) => r,
        None => return Err("No profitable executable route".to_string()),
    };

    // Deploy executor
    let executor_addr = deploy_executor(anvil_url)?;

    // Build calldata
    let (final_steps, _) = cycle_to_swap_route(
        &cycle, &graph, &token_index, pools, &reserves, optimal_input, executor_addr,
    )
    .ok_or("Route build failed")?;

    let deadline = U256::from(u64::MAX);
    let calldata = build_execute_arb_calldata(
        &final_steps,
        flashloan_token,
        optimal_input,
        deadline,
        U256::ZERO,
        U256::from(9000u64),
    );

    // Simulate
    let bn = provider.get_block_number().await.map_err(|e| format!("{e}"))?;
    let blk = provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(bn))
        .await
        .map_err(|e| format!("{e}"))?
        .ok_or("block not found")?;

    let sim_parsed: url::Url = anvil_url.parse().expect("valid URL");
    let sim_provider = ProviderBuilder::new().connect_http(sim_parsed);
    let dyn_provider = sim_provider.erased();

    let rpc_state = RpcForkedState::new(
        dyn_provider,
        bn,
        blk.header.timestamp,
        blk.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64,
    )
    .ok_or("RpcForkedState failed")?;

    let sim = EvmSimulator::new(SimConfig {
        gas_limit,
        chain_id: 1,
        caller: ANVIL_ACCOUNT0,
        value: U256::ZERO,
    });

    let sim_result = sim.simulate_rpc(rpc_state, executor_addr, calldata.clone());
    eprintln!(
        "    Sim: success={}, gas={}",
        sim_result.success, sim_result.gas_used
    );

    if !sim_result.success {
        return Ok((false, None, None, Some(sim_result.gas_used)));
    }

    // Execute on-chain
    let execute_tx = alloy::rpc::types::TransactionRequest::default()
        .from(ANVIL_ACCOUNT0)
        .to(executor_addr)
        .input(calldata.into())
        .gas_limit(gas_limit);

    let pending = provider
        .send_transaction(execute_tx)
        .await
        .map_err(|e| format!("send tx: {e}"))?;

    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("receipt: {e}"))?;

    let exec_success = receipt.status();
    let mut profit = None;

    if exec_success {
        let topic = alloy::primitives::keccak256(b"ArbExecuted(address,uint256,uint256,uint256)");
        for log in receipt.inner.logs() {
            let topics = log.inner.data.topics();
            let data_bytes: &[u8] = log.inner.data.data.as_ref();
            if !topics.is_empty() && topics[0] == topic && data_bytes.len() >= 96 {
                profit = Some(U256::from_be_slice(&data_bytes[32..64]));
                break;
            }
        }
    }

    eprintln!(
        "    Exec: success={}, gas={}, profit={:?}",
        exec_success, receipt.gas_used, profit
    );

    Ok((true, Some(exec_success), profit, Some(receipt.gas_used)))
}

fn print_metrics(metrics: &BacktestMetrics) {
    eprintln!("\n╔════════════════════════════════════════════════╗");
    eprintln!("║         BACKTEST METRICS SUMMARY               ║");
    eprintln!("╠════════════════════════════════════════════════╣");
    eprintln!("║ Blocks scanned:         {:>6}                 ║", metrics.blocks_scanned);
    eprintln!("║ Blocks with arbs:       {:>6}                 ║", metrics.blocks_with_arbs);
    eprintln!("║ Detection rate:         {:>6.1}%                ║", metrics.detection_rate_pct);
    eprintln!("║ Total cycles:           {:>6}                 ║", metrics.total_cycles);
    eprintln!("║ Avg detection time:     {:>6.0}us               ║", metrics.avg_detection_us);
    eprintln!("╠────────────────────────────────────────────────╣");
    eprintln!("║ Simulations run:        {:>6}                 ║", metrics.simulations_run);
    eprintln!("║ Simulations passed:     {:>6}                 ║", metrics.simulations_passed);
    eprintln!("║ False positive rate:    {:>6.1}%                ║", metrics.false_positive_rate_pct);
    eprintln!("╠────────────────────────────────────────────────╣");
    eprintln!("║ Executions run:         {:>6}                 ║", metrics.executions_run);
    eprintln!("║ Executions passed:      {:>6}                 ║", metrics.executions_passed);
    eprintln!("║ Profits collected:      {:>6}                 ║", metrics.profits_wei.len());

    if !metrics.gas_used.is_empty() {
        let avg_gas: f64 =
            metrics.gas_used.iter().map(|g| *g as f64).sum::<f64>() / metrics.gas_used.len() as f64;
        eprintln!("║ Avg gas used:           {:>6.0}                 ║", avg_gas);
    }

    eprintln!("╠────────────────────────────────────────────────╣");
    eprintln!("║ Total elapsed:          {:>6.1}s                ║", metrics.total_elapsed_s);
    eprintln!("╚════════════════════════════════════════════════╝");
}

// ── Test ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_multi_block_backtest() {
    if !check_prerequisites(true, true) {
        return;
    }

    let num_blocks: u64 = std::env::var("BACKTEST_BLOCKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

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

    let start = latest.saturating_sub(num_blocks - 1);

    eprintln!("=== Multi-Block Backtest ===");
    eprintln!("Blocks: {} to {} ({} blocks)", start, latest, num_blocks);

    let config = BacktestConfig {
        max_e2e_executions: 3,
        ..Default::default()
    };

    let metrics = run_backtest(&config, start, latest).await;
    print_metrics(&metrics);

    // Basic sanity: we should have scanned all requested blocks
    assert_eq!(
        metrics.blocks_scanned, num_blocks,
        "Should scan all {} blocks",
        num_blocks
    );
}
