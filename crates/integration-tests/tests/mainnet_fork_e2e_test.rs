//! Mainnet fork end-to-end test: real reserves -> price graph -> detection -> RPC simulation.
//!
//! Prerequisites:
//! - `ETH_RPC_URL` environment variable pointing to an Ethereum mainnet RPC (e.g. Alchemy)
//! - `anvil` binary in PATH (from Foundry)
//!
//! Run with:
//!   ETH_RPC_URL="https://eth-mainnet.g.alchemy.com/v2/..." \
//!     cargo test -p aether-integration-tests --test mainnet_fork_e2e_test -- --nocapture

mod common;

use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolCall;

use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;

use common::{
    build_price_graph, check_prerequisites, default_pool_set, fetch_reserves,
    getReservesCall, spawn_anvil_at_block, wait_for_anvil,
    SUSHI_WETH_USDC, UNIV2_WETH_USDC,
};

/// Full E2E test: real reserves -> price graph -> detection -> RPC simulation.
///
/// Uses multi_thread flavor because WrapDatabaseAsync requires block_in_place.
#[tokio::test(flavor = "multi_thread")]
async fn test_mainnet_fork_e2e_pipeline() {
    if !check_prerequisites(false, false) {
        return;
    }

    let (mut anvil, url) = spawn_anvil_at_block(None, 19545);

    let result = async {
        assert!(
            wait_for_anvil(&url, Duration::from_secs(30)).await,
            "Anvil did not start in time"
        );

        let parsed: url::Url = url.parse().unwrap();
        let provider = ProviderBuilder::new().connect_http(parsed);

        let block_number = provider
            .get_block_number()
            .await
            .expect("should get block number");
        let block = provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(block_number))
            .await
            .expect("should get block")
            .expect("block should exist");

        let timestamp = block.header.timestamp;
        let base_fee = block.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u64;

        eprintln!("=== Mainnet Fork E2E Test ===");
        eprintln!("Block: {block_number}  Timestamp: {timestamp}  BaseFee: {base_fee}");

        let t_pipeline = Instant::now();

        // ── Step 1: Fetch real reserves ───────────────────────────────
        let t0 = Instant::now();

        let pools = default_pool_set();
        let mut reserves: Vec<(usize, U256, U256)> = Vec::new();
        for (i, pool) in pools.iter().enumerate() {
            if let Some((r0, r1)) = fetch_reserves(&provider, pool.address).await {
                eprintln!(
                    "  {:?} {}: r0={}, r1={}",
                    pool.protocol, pool.address, r0, r1
                );
                reserves.push((i, r0, r1));
            }
        }

        let step1_ms = t0.elapsed().as_millis();
        eprintln!(
            "Step 1 (fetch reserves): {}ms — {}/{} pools",
            step1_ms,
            reserves.len(),
            pools.len()
        );
        assert!(
            reserves.len() >= 3,
            "Should fetch reserves from at least 3 pools, got {}",
            reserves.len()
        );

        // ── Step 2: Build price graph ─────────────────────────────────
        let t1 = Instant::now();

        let (graph, token_index) = build_price_graph(&pools, &reserves);

        let step2_us = t1.elapsed().as_micros();
        eprintln!(
            "Step 2 (build graph): {}us — {} edges, {} tokens",
            step2_us,
            graph.num_edges(),
            token_index.len()
        );
        assert!(graph.num_edges() >= 6, "Should have at least 6 edges");

        // ── Step 3: Run Bellman-Ford detection ────────────────────────
        let t2 = Instant::now();

        let cycles = common::run_detection(&graph, 5, 3_000_000);

        let step3_us = t2.elapsed().as_micros();
        eprintln!(
            "Step 3 (Bellman-Ford): {}us — {} cycles found",
            step3_us,
            cycles.len()
        );

        // ── Step 4: RPC simulation against real fork state ────────────
        let t3 = Instant::now();

        let dyn_provider = provider.erased();

        let t4a_state = Instant::now();
        let rpc_state = RpcForkedState::new(
            dyn_provider.clone(),
            block_number,
            timestamp,
            base_fee,
        )
        .expect("RpcForkedState::new should succeed in multi_thread runtime");
        let state_creation_1_us = t4a_state.elapsed().as_micros();

        let get_reserves_calldata = getReservesCall {}.abi_encode();

        let sim = EvmSimulator::new(SimConfig {
            gas_limit: 100_000,
            chain_id: 1,
            caller: Address::ZERO,
            value: U256::ZERO,
        });

        let t4a_evm = Instant::now();
        let result = sim.simulate_rpc(rpc_state, UNIV2_WETH_USDC, get_reserves_calldata.clone());
        let evm_exec_1_us = t4a_evm.elapsed().as_micros();

        eprintln!(
            "Step 4a (simulate getReserves on UniV2 WETH/USDC): success={}, gas={}, state_create={}us, evm_exec={}us",
            result.success, result.gas_used, state_creation_1_us, evm_exec_1_us
        );
        assert!(
            result.success,
            "getReserves() simulation should succeed: {:?}",
            result.revert_reason
        );
        assert!(result.gas_used > 0, "Should consume gas");

        let t4b_state = Instant::now();
        let rpc_state2 = RpcForkedState::new(
            dyn_provider.clone(),
            block_number,
            timestamp,
            base_fee,
        )
        .expect("Second RpcForkedState should succeed");
        let state_creation_2_us = t4b_state.elapsed().as_micros();

        let t4b_evm = Instant::now();
        let result2 = sim.simulate_rpc(rpc_state2, SUSHI_WETH_USDC, get_reserves_calldata);
        let evm_exec_2_us = t4b_evm.elapsed().as_micros();

        eprintln!(
            "Step 4b (simulate getReserves on Sushi WETH/USDC): success={}, gas={}, state_create={}us, evm_exec={}us",
            result2.success, result2.gas_used, state_creation_2_us, evm_exec_2_us
        );
        assert!(
            result2.success,
            "Sushi getReserves() simulation should succeed: {:?}",
            result2.revert_reason
        );

        let step4_ms = t3.elapsed().as_millis();
        eprintln!("Step 4 (RPC simulation): {}ms total", step4_ms);

        // ── Summary ───────────────────────────────────────────────────
        let total_pipeline_ms = t_pipeline.elapsed().as_millis();

        eprintln!("=== E2E Test Complete ===");
        eprintln!("  Pools fetched: {}", reserves.len());
        eprintln!("  Graph edges: {}", graph.num_edges());
        eprintln!("  Cycles detected: {}", cycles.len());
        eprintln!("  RPC simulations: 2 passed");
        eprintln!();
        let compute_us = step2_us + step3_us + (step4_ms * 1000);
        eprintln!("=== Latency Breakdown ===");
        eprintln!("  Step 1 (fetch reserves):   {}ms", step1_ms);
        eprintln!("  Step 2 (build graph):      {}us", step2_us);
        eprintln!("  Step 3 (Bellman-Ford):     {}us", step3_us);
        eprintln!("  Step 4 (RPC simulation):   {}ms", step4_ms);
        eprintln!("    4a state create:         {}us", state_creation_1_us);
        eprintln!("    4a EVM execute:          {}us", evm_exec_1_us);
        eprintln!("    4b state create:         {}us", state_creation_2_us);
        eprintln!("    4b EVM execute:          {}us", evm_exec_2_us);
        eprintln!("  ─────────────────────────────────");
        eprintln!(
            "  Compute (2+3+4):           {}us (~{}ms)",
            compute_us,
            compute_us / 1000
        );
        eprintln!("  Total pipeline:            {}ms", total_pipeline_ms);
    }
    .await;

    let _ = anvil.kill();
    let _ = anvil.wait();
    result
}
