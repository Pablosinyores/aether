//! Flash Loan Arbitrage E2E Integration Test
//!
//! Proves the full pipeline: deploy executor -> manufacture price discrepancy ->
//! detect arb with Bellman-Ford -> simulate via revm -> execute flash loan on-chain.
//!
//! Prerequisites:
//! - `ETH_RPC_URL` environment variable (Ethereum mainnet RPC)
//! - `anvil` binary in PATH (from Foundry)
//! - `forge` binary in PATH (from Foundry)
//!
//! Run with:
//!   set -a && source .env && set +a && \
//!     cargo test -p aether-integration-tests --test mainnet_fork_flashloan_test -- --nocapture

mod common;

use std::time::{Duration, Instant};

use alloy::primitives::{keccak256, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolCall;

use aether_common::types::{PoolId, ProtocolType, SwapStep};
use aether_detector::bellman_ford::BellmanFord;
use aether_simulator::calldata::{build_execute_arb_calldata, build_univ2_swap_calldata};
use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

use common::{
    check_prerequisites, contracts_root, deploy_executor, erc20_balance, erc20_balance_slot,
    fetch_reserves, get_amount_out, spawn_anvil_at_block, swapCall, transferCall, u256_to_f64,
    wait_for_anvil, ANVIL_ACCOUNT0, SUSHI_WETH_USDC,
    UNIV2_WETH_USDC, USDC, WETH, WETH_BALANCE_SLOT,
};

/// Dump 200 WETH on UniV2 to create ~8% price gap
const DUMP_AMOUNT_ETH: u128 = 200;
const DUMP_AMOUNT_WEI: u128 = DUMP_AMOUNT_ETH * 1_000_000_000_000_000_000;

/// Flash loan 10 WETH
const FLASHLOAN_AMOUNT_WEI: u128 = 10_000_000_000_000_000_000;

// ── Main test ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_flash_loan_arb_e2e() {
    if !check_prerequisites(true, true) {
        return;
    }

    let (mut anvil, anvil_url) = spawn_anvil_at_block(None, 20545);

    let test_result = run_flash_loan_test(&anvil_url).await;

    let _ = anvil.kill();
    let _ = anvil.wait();

    if let Err(msg) = test_result {
        panic!("{}", msg);
    }
}

async fn run_flash_loan_test(anvil_url: &str) -> Result<(), String> {
    let mut phase_times: Vec<(&str, u128)> = Vec::new();

    // ── Phase 1: Provider setup ──────────────────────────────────────
    let t_phase1 = Instant::now();

    assert!(
        wait_for_anvil(anvil_url, Duration::from_secs(30)).await,
        "Anvil did not start in time"
    );

    let parsed: url::Url = anvil_url.parse().expect("valid URL");
    let ro_provider = ProviderBuilder::new().connect_http(parsed.clone());

    let block_number = ro_provider
        .get_block_number()
        .await
        .map_err(|e| format!("get_block_number: {e}"))?;
    let block = ro_provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(block_number))
        .await
        .map_err(|e| format!("get_block: {e}"))?
        .ok_or("block not found")?;

    let timestamp = block.header.timestamp;
    let base_fee = block.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64;

    phase_times.push(("Phase 1 (provider setup)", t_phase1.elapsed().as_millis()));
    eprintln!("\n=== Flash Loan Arbitrage E2E Test ===");
    eprintln!(
        "Block: {}  Timestamp: {}  BaseFee: {} gwei",
        block_number,
        timestamp,
        base_fee / 1_000_000_000
    );

    // ── Phase 2: Deploy AetherExecutor ───────────────────────────────
    let t_phase2 = Instant::now();

    eprintln!("Deploying AetherExecutor from {:?}...", contracts_root());
    let executor_addr = deploy_executor(anvil_url)?;

    phase_times.push(("Phase 2 (deploy executor)", t_phase2.elapsed().as_millis()));
    eprintln!("AetherExecutor deployed at: {executor_addr}");

    // ── Phase 3: Fetch initial reserves ──────────────────────────────
    let t_phase3 = Instant::now();

    let (uni_r0_pre, uni_r1_pre) = fetch_reserves(&ro_provider, UNIV2_WETH_USDC)
        .await
        .ok_or("Failed to fetch UniV2 reserves")?;
    let (sushi_r0_pre, sushi_r1_pre) = fetch_reserves(&ro_provider, SUSHI_WETH_USDC)
        .await
        .ok_or("Failed to fetch Sushi reserves")?;

    eprintln!(
        "UniV2 pre-dump:  USDC={}, WETH={}",
        uni_r0_pre, uni_r1_pre
    );
    eprintln!(
        "Sushi pre-dump:  USDC={}, WETH={}",
        sushi_r0_pre, sushi_r1_pre
    );

    phase_times.push((
        "Phase 3 (fetch initial reserves)",
        t_phase3.elapsed().as_millis(),
    ));

    // ── Phase 4: Manufacture arbitrage ───────────────────────────────
    let t_phase4 = Instant::now();

    let dump_amount = U256::from(DUMP_AMOUNT_WEI);

    let balance_slot = erc20_balance_slot(ANVIL_ACCOUNT0, WETH_BALANCE_SLOT);
    let balance_value = dump_amount + U256::from(10_000_000_000_000_000_000u128);

    let slot_hex = format!("0x{:064x}", balance_slot);
    let value_hex = format!("0x{:064x}", balance_value);
    let weth_hex = format!("{}", WETH);

    let set_storage_output = std::process::Command::new("cast")
        .args([
            "rpc",
            "anvil_setStorageAt",
            &weth_hex,
            &slot_hex,
            &value_hex,
            "--rpc-url",
            anvil_url,
        ])
        .output()
        .map_err(|e| format!("cast rpc anvil_setStorageAt: {e}"))?;
    if !set_storage_output.status.success() {
        return Err(format!(
            "anvil_setStorageAt failed: {}",
            String::from_utf8_lossy(&set_storage_output.stderr)
        ));
    }

    let weth_bal = erc20_balance(&ro_provider, WETH, ANVIL_ACCOUNT0).await;
    eprintln!("Account0 WETH balance after setStorage: {weth_bal}");
    assert!(
        weth_bal >= dump_amount,
        "WETH balance should be >= dump amount"
    );

    let transfer_calldata = transferCall {
        to: UNIV2_WETH_USDC,
        amount: dump_amount,
    }
    .abi_encode();

    let transfer_tx = alloy::rpc::types::TransactionRequest::default()
        .from(ANVIL_ACCOUNT0)
        .to(WETH)
        .input(transfer_calldata.into());

    let transfer_hash = ro_provider
        .send_transaction(transfer_tx)
        .await
        .map_err(|e| format!("WETH transfer send: {e}"))?
        .watch()
        .await
        .map_err(|e| format!("WETH transfer watch: {e}"))?;
    eprintln!("WETH transfer to UniV2 pair: {transfer_hash:?}");

    let effective_weth_reserve = uni_r1_pre + dump_amount;
    let usdc_out = get_amount_out(dump_amount, effective_weth_reserve, uni_r0_pre);

    eprintln!(
        "Swap: {} WETH → {} USDC on UniV2",
        dump_amount, usdc_out
    );

    let swap_calldata = swapCall {
        amount0Out: usdc_out,
        amount1Out: U256::ZERO,
        to: ANVIL_ACCOUNT0,
        data: Bytes::new(),
    }
    .abi_encode();

    let swap_tx = alloy::rpc::types::TransactionRequest::default()
        .from(ANVIL_ACCOUNT0)
        .to(UNIV2_WETH_USDC)
        .input(swap_calldata.into())
        .gas_limit(300_000);

    let swap_hash = ro_provider
        .send_transaction(swap_tx)
        .await
        .map_err(|e| format!("UniV2 swap send: {e}"))?
        .watch()
        .await
        .map_err(|e| format!("UniV2 swap watch: {e}"))?;
    eprintln!("UniV2 dump swap tx: {swap_hash:?}");

    phase_times.push((
        "Phase 4 (manufacture arb)",
        t_phase4.elapsed().as_millis(),
    ));

    // ── Phase 5: Verify price gap ────────────────────────────────────
    let t_phase5 = Instant::now();

    let (uni_r0_post, uni_r1_post) = fetch_reserves(&ro_provider, UNIV2_WETH_USDC)
        .await
        .ok_or("Failed to fetch post-dump UniV2 reserves")?;
    let (sushi_r0_post, sushi_r1_post) = fetch_reserves(&ro_provider, SUSHI_WETH_USDC)
        .await
        .ok_or("Failed to fetch post-dump Sushi reserves")?;

    let uni_price = u256_to_f64(uni_r0_post) / u256_to_f64(uni_r1_post);
    let sushi_price = u256_to_f64(sushi_r0_post) / u256_to_f64(sushi_r1_post);
    let gap_pct = ((sushi_price - uni_price) / sushi_price) * 100.0;

    eprintln!("UniV2 post-dump:  USDC={}, WETH={}", uni_r0_post, uni_r1_post);
    eprintln!("Sushi post-dump:  USDC={}, WETH={}", sushi_r0_post, sushi_r1_post);
    eprintln!(
        "Prices: UniV2={:.2} USDC/WETH, Sushi={:.2} USDC/WETH, Gap={:.2}%",
        uni_price, sushi_price, gap_pct
    );

    assert!(
        sushi_price > uni_price,
        "Sushi WETH price should be higher than UniV2 after dump"
    );
    assert!(
        gap_pct > 3.0,
        "Price gap should be > 3%, got {:.2}%",
        gap_pct
    );

    phase_times.push(("Phase 5 (verify price gap)", t_phase5.elapsed().as_millis()));

    // ── Phase 6: Bellman-Ford detection ──────────────────────────────
    let t_phase6 = Instant::now();

    let mut token_index = TokenIndex::new();
    let usdc_idx = token_index.get_or_insert(USDC);
    let weth_idx = token_index.get_or_insert(WETH);

    let mut graph = PriceGraph::new(token_index.len());

    let uni_pool_id = PoolId {
        address: UNIV2_WETH_USDC,
        protocol: ProtocolType::UniswapV2,
    };
    let sushi_pool_id = PoolId {
        address: SUSHI_WETH_USDC,
        protocol: ProtocolType::SushiSwap,
    };

    let fee = 0.997;

    let uni_r0_f = u256_to_f64(uni_r0_post);
    let uni_r1_f = u256_to_f64(uni_r1_post);
    graph.add_edge(
        usdc_idx,
        weth_idx,
        (uni_r1_f / uni_r0_f) * fee,
        uni_pool_id,
        UNIV2_WETH_USDC,
        ProtocolType::UniswapV2,
        U256::ZERO,
    );
    graph.add_edge(
        weth_idx,
        usdc_idx,
        (uni_r0_f / uni_r1_f) * fee,
        uni_pool_id,
        UNIV2_WETH_USDC,
        ProtocolType::UniswapV2,
        U256::ZERO,
    );

    let sushi_r0_f = u256_to_f64(sushi_r0_post);
    let sushi_r1_f = u256_to_f64(sushi_r1_post);
    graph.add_edge(
        usdc_idx,
        weth_idx,
        (sushi_r1_f / sushi_r0_f) * fee,
        sushi_pool_id,
        SUSHI_WETH_USDC,
        ProtocolType::SushiSwap,
        U256::ZERO,
    );
    graph.add_edge(
        weth_idx,
        usdc_idx,
        (sushi_r0_f / sushi_r1_f) * fee,
        sushi_pool_id,
        SUSHI_WETH_USDC,
        ProtocolType::SushiSwap,
        U256::ZERO,
    );

    let bf = BellmanFord::new(3, 5_000_000);
    let cycles = bf.detect_negative_cycles(&graph);

    let phase6_us = t_phase6.elapsed().as_micros();
    phase_times.push(("Phase 6 (Bellman-Ford)", t_phase6.elapsed().as_millis()));
    eprintln!(
        "Bellman-Ford: {} cycles detected in {}us",
        cycles.len(),
        phase6_us
    );

    assert!(
        !cycles.is_empty(),
        "Should detect at least 1 negative cycle after price manipulation"
    );
    for (i, cycle) in cycles.iter().enumerate() {
        eprintln!(
            "  Cycle {}: path={:?}, weight={:.6}, profit_factor={:.4}%",
            i,
            cycle.path,
            cycle.total_weight,
            cycle.profit_factor() * 100.0
        );
    }

    // ── Phase 7: Build arb swap steps + calldata ─────────────────────
    let t_phase7 = Instant::now();

    let flashloan_amount = U256::from(FLASHLOAN_AMOUNT_WEI);

    let sushi_usdc_out = get_amount_out(flashloan_amount, sushi_r1_post, sushi_r0_post);
    let uni_weth_out = get_amount_out(sushi_usdc_out, uni_r0_post, uni_r1_post);

    let aave_premium = flashloan_amount * U256::from(5) / U256::from(10000);
    let total_repay = flashloan_amount + aave_premium;

    eprintln!(
        "Step 1: {} WETH → {} USDC on Sushi",
        flashloan_amount, sushi_usdc_out
    );
    eprintln!(
        "Step 2: {} USDC → {} WETH on UniV2",
        sushi_usdc_out, uni_weth_out
    );
    eprintln!(
        "Repay: {} + {} premium = {}",
        flashloan_amount, aave_premium, total_repay
    );

    assert!(
        uni_weth_out > total_repay,
        "Arb should be profitable: got {} WETH back but need {} to repay",
        uni_weth_out,
        total_repay
    );

    let expected_profit = uni_weth_out - total_repay;
    eprintln!("Expected profit: {} WETH wei", expected_profit);

    let step1_swap_data = build_univ2_swap_calldata(sushi_usdc_out, U256::ZERO, executor_addr);
    let step2_swap_data = build_univ2_swap_calldata(U256::ZERO, uni_weth_out, executor_addr);

    let step1_min = sushi_usdc_out * U256::from(99) / U256::from(100);
    let step2_min = uni_weth_out * U256::from(99) / U256::from(100);

    let steps = vec![
        SwapStep {
            protocol: ProtocolType::SushiSwap,
            pool_address: SUSHI_WETH_USDC,
            token_in: WETH,
            token_out: USDC,
            amount_in: flashloan_amount,
            min_amount_out: step1_min,
            calldata: step1_swap_data,
        },
        SwapStep {
            protocol: ProtocolType::UniswapV2,
            pool_address: UNIV2_WETH_USDC,
            token_in: USDC,
            token_out: WETH,
            amount_in: sushi_usdc_out,
            min_amount_out: step2_min,
            calldata: step2_swap_data,
        },
    ];

    let deadline = U256::from(u64::MAX);
    let execute_arb_calldata = build_execute_arb_calldata(
        &steps,
        WETH,
        flashloan_amount,
        deadline,
        U256::ZERO,
        U256::from(9000u64),
    );

    phase_times.push((
        "Phase 7 (build calldata)",
        t_phase7.elapsed().as_millis(),
    ));

    // ── Phase 8: Simulate via revm ───────────────────────────────────
    let t_phase8 = Instant::now();

    let current_block = ro_provider
        .get_block_number()
        .await
        .map_err(|e| format!("get_block_number for sim: {e}"))?;
    let current_block_data = ro_provider
        .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(current_block))
        .await
        .map_err(|e| format!("get_block for sim: {e}"))?
        .ok_or("block not found for sim")?;
    let current_ts = current_block_data.header.timestamp;
    let current_base_fee =
        current_block_data.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64;

    let sim_parsed: url::Url = anvil_url.parse().expect("valid URL");
    let sim_provider = ProviderBuilder::new().connect_http(sim_parsed);
    let dyn_provider = sim_provider.erased();

    let rpc_state = RpcForkedState::new(dyn_provider, current_block, current_ts, current_base_fee)
        .ok_or("RpcForkedState::new failed")?;

    let sim = EvmSimulator::new(SimConfig {
        gas_limit: 1_500_000,
        chain_id: 1,
        caller: ANVIL_ACCOUNT0,
        value: U256::ZERO,
    });

    let sim_result = sim.simulate_rpc(rpc_state, executor_addr, execute_arb_calldata.clone());

    phase_times.push(("Phase 8 (revm simulation)", t_phase8.elapsed().as_millis()));
    eprintln!(
        "revm simulation: success={}, gas_used={}, revert={:?}",
        sim_result.success, sim_result.gas_used, sim_result.revert_reason
    );

    assert!(
        sim_result.success,
        "revm simulation should succeed: {:?}",
        sim_result.revert_reason
    );

    // ── Phase 9: Execute flash loan on-chain ─────────────────────────
    let t_phase9 = Instant::now();

    let execute_tx = alloy::rpc::types::TransactionRequest::default()
        .from(ANVIL_ACCOUNT0)
        .to(executor_addr)
        .input(execute_arb_calldata.into())
        .gas_limit(1_500_000);

    let pending = ro_provider
        .send_transaction(execute_tx)
        .await
        .map_err(|e| format!("executeArb send: {e}"))?;

    let tx_hash = *pending.tx_hash();
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| format!("executeArb receipt: {e}"))?;

    phase_times.push((
        "Phase 9 (on-chain execution)",
        t_phase9.elapsed().as_millis(),
    ));
    eprintln!("executeArb tx: {tx_hash:?}");
    eprintln!(
        "Receipt: status={}, gas_used={}",
        receipt.status(),
        receipt.gas_used
    );

    assert!(receipt.status(), "executeArb should succeed on-chain");

    // ── Phase 10: Verify results ─────────────────────────────────────
    let t_phase10 = Instant::now();

    let arb_executed_topic = keccak256(b"ArbExecuted(address,uint256,uint256,uint256)");
    let mut found_event = false;
    let mut on_chain_profit = U256::ZERO;

    for log in receipt.inner.logs() {
        let topics = log.inner.data.topics();
        let data_bytes: &[u8] = log.inner.data.data.as_ref();
        if !topics.is_empty() && topics[0] == arb_executed_topic {
            found_event = true;
            if data_bytes.len() >= 96 {
                let _fl_amount = U256::from_be_slice(&data_bytes[0..32]);
                on_chain_profit = U256::from_be_slice(&data_bytes[32..64]);
                let _gas_used = U256::from_be_slice(&data_bytes[64..96]);
                eprintln!(
                    "ArbExecuted event: flashloan={}, profit={}, gasUsed={}",
                    _fl_amount, on_chain_profit, _gas_used
                );
            }
            break;
        }
    }

    assert!(found_event, "ArbExecuted event should be emitted");
    assert!(
        on_chain_profit > U256::ZERO,
        "On-chain profit should be > 0"
    );

    let owner_weth = erc20_balance(&ro_provider, WETH, ANVIL_ACCOUNT0).await;
    eprintln!("Owner WETH balance after arb: {owner_weth}");
    assert!(
        owner_weth > U256::ZERO,
        "Owner should have received WETH profit"
    );

    phase_times.push(("Phase 10 (verify results)", t_phase10.elapsed().as_millis()));

    // ── Summary ──────────────────────────────────────────────────────
    let total_ms: u128 = phase_times.iter().map(|(_, ms)| ms).sum();

    eprintln!("\n=== Latency Breakdown ===");
    for (name, ms) in &phase_times {
        eprintln!("  {:<35} {}ms", name, ms);
    }
    eprintln!("  {:<35} {}ms", "TOTAL", total_ms);

    eprintln!("\n=== Results ===");
    eprintln!("  Cycles detected:    {}", cycles.len());
    eprintln!("  Detection time:     {}us", phase6_us);
    eprintln!("  revm sim gas:       {}", sim_result.gas_used);
    eprintln!("  On-chain gas:       {}", receipt.gas_used);
    eprintln!("  On-chain profit:    {} WETH wei", on_chain_profit);
    eprintln!("  Price gap:          {:.2}%", gap_pct);
    eprintln!("\n=== Flash Loan E2E Test PASSED ===\n");

    Ok(())
}
