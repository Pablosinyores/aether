//! Integration tests for the Aether arbitrage pipeline.
//! Tests the full flow: pool state -> price graph -> detection -> simulation.

use alloy::primitives::{address, Address, U256};
use aether_common::types::*;
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas;
use aether_detector::opportunity::{RankedOpportunity, TopKCollector};
use aether_detector::optimizer;
use aether_pools::balancer::BalancerPool;
use aether_pools::curve::CurvePool;
use aether_pools::registry::PoolRegistry;
use aether_pools::sushiswap::SushiSwapPool;
use aether_pools::uniswap_v2::UniswapV2Pool;
use aether_pools::Pool;
use aether_simulator::fork::{ForkedState, SimConfig};
use aether_simulator::EvmSimulator;
use aether_state::price_graph::PriceGraph;
use aether_state::snapshot::SnapshotManager;
use aether_state::token_index::TokenIndex;

const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

/// Test 1: Full pipeline - pool registry -> price graph -> detection
#[test]
fn test_full_arbitrage_detection_pipeline() {
    let mut registry = PoolRegistry::with_defaults();

    let mut univ2 = UniswapV2Pool::new(
        address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
        USDC,
        WETH,
        30,
    );
    univ2.update_state(
        U256::from(10_000_000_000_000u64),
        U256::from(5_000_000_000_000_000_000_000u128),
    );

    let mut sushi = SushiSwapPool::new(
        address!("397FF1542f962076d0BFE58eA045FfA2d347ACa0"),
        USDC,
        WETH,
        30,
    );
    sushi.update_state(
        U256::from(9_900_000_000_000u64),
        U256::from(5_000_000_000_000_000_000_000u128),
    );

    registry.register(Box::new(univ2.clone()), PoolTier::Hot);
    registry.register(Box::new(sushi.clone()), PoolTier::Hot);
    assert_eq!(registry.pool_count(), 2);

    // Build price graph
    let mut token_index = TokenIndex::new();
    let usdc_idx = token_index.get_or_insert(USDC);
    let weth_idx = token_index.get_or_insert(WETH);

    let mut graph = PriceGraph::new(token_index.len());

    let univ2_id = PoolId {
        address: univ2.address(),
        protocol: ProtocolType::UniswapV2,
    };
    graph.add_edge(
        usdc_idx,
        weth_idx,
        5000.0 / 10_000_000.0 * 0.997,
        univ2_id,
        univ2.address(),
        ProtocolType::UniswapV2,
        U256::ZERO,
    );
    graph.add_edge(
        weth_idx,
        usdc_idx,
        10_000_000.0 / 5000.0 * 0.997,
        univ2_id,
        univ2.address(),
        ProtocolType::UniswapV2,
        U256::ZERO,
    );

    let sushi_id = PoolId {
        address: sushi.address(),
        protocol: ProtocolType::SushiSwap,
    };
    graph.add_edge(
        usdc_idx,
        weth_idx,
        5000.0 / 9_900_000.0 * 0.997,
        sushi_id,
        sushi.address(),
        ProtocolType::SushiSwap,
        U256::ZERO,
    );
    graph.add_edge(
        weth_idx,
        usdc_idx,
        9_900_000.0 / 5000.0 * 0.997,
        sushi_id,
        sushi.address(),
        ProtocolType::SushiSwap,
        U256::ZERO,
    );

    assert_eq!(graph.num_edges(), 4);
    assert!(graph.has_dirty_edges());

    // Run Bellman-Ford
    let bf = BellmanFord::new(5, 3_000_000);
    let cycles = bf.detect_negative_cycles(&graph);
    // Detection completes without panic
    let _ = cycles.len();

    // MVCC snapshot
    let snapshot_mgr = SnapshotManager::new(graph.clone());
    let snap = snapshot_mgr.load();
    assert_eq!(snap.graph.num_edges(), 4);

    graph.clear_dirty();
    snapshot_mgr.publish(graph, 18_000_000, 1_700_000_000);
    assert_eq!(snapshot_mgr.version(), 1);
}

/// Test 2: Pool pricing consistency across DEX adapters
#[test]
fn test_pool_pricing_consistency() {
    let mut univ2 = UniswapV2Pool::new(Address::ZERO, USDC, WETH, 30);
    let mut sushi = SushiSwapPool::new(
        address!("0000000000000000000000000000000000000001"),
        USDC,
        WETH,
        30,
    );

    let r0 = U256::from(10_000_000_000_000u64);
    let r1 = U256::from(5_000_000_000_000_000_000_000u128);
    univ2.update_state(r0, r1);
    sushi.update_state(r0, r1);

    let amount = U256::from(1_000_000_000_000_000_000u64);
    assert_eq!(
        univ2.get_amount_out(WETH, amount),
        sushi.get_amount_out(WETH, amount),
    );
}

/// Test 3: Curve stableswap near-parity
#[test]
fn test_curve_stableswap_parity() {
    let mut curve = CurvePool::new(
        address!("0000000000000000000000000000000000000002"),
        vec![USDC, USDT],
        100,
        4,
    );
    curve.update_state(
        U256::from(10_000_000_000_000u64),
        U256::from(10_000_000_000_000u64),
    );

    let out = curve
        .get_amount_out(USDC, U256::from(1_000_000_000u64))
        .unwrap();
    assert!(out > U256::from(999_000_000u64));
    assert!(out < U256::from(1_000_000_000u64));
}

/// Test 4: Gas estimation for multi-hop routes
#[test]
fn test_gas_estimation_multi_hop() {
    let protocols = vec![
        ProtocolType::UniswapV2,
        ProtocolType::UniswapV3,
        ProtocolType::Curve,
    ];
    let tick_counts = vec![0, 5, 0];
    let total = gas::estimate_total_gas(&protocols, &tick_counts);
    let expected = 21_000 + 80_000 + 30_000 + 60_000 + (100_000 + 5 * 5_000) + 130_000;
    assert_eq!(total, expected);
}

/// Test 5: Optimizer ternary search finds peak
#[test]
fn test_optimizer_finds_peak() {
    let profit_fn = |x: U256| -> i128 {
        let v = x.to::<i128>();
        -(v - 500) * (v - 500) + 250000
    };
    let (optimal, max_profit) =
        optimizer::ternary_search_optimal_input(U256::from(1u64), U256::from(1000u64), 100, profit_fn);
    let v = optimal.to::<u64>();
    assert!((495..=505).contains(&v), "got {}", v);
    assert!(max_profit > 249_000);
}

/// Test 6: EVM simulator basic operation
#[test]
fn test_evm_simulator_integration() {
    let caller = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let contract = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
    state.insert_account_balance(caller, U256::from(10_000_000_000_000_000_000u128));
    state.insert_account(
        contract,
        U256::ZERO,
        alloy::primitives::Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xf3]),
    );

    let sim = EvmSimulator::new(SimConfig {
        gas_limit: 100_000,
        chain_id: 1,
        caller,
        value: U256::ZERO,
    });
    let result = sim.simulate(&state, contract, vec![]);
    assert!(result.success);
    assert!(result.gas_used > 0);
}

/// Test 7: Top-K collector ranking
#[test]
fn test_topk_opportunity_ranking() {
    let mut collector = TopKCollector::new(3);
    for i in 0..10 {
        let opp = ArbOpportunity {
            id: format!("arb-{}", i),
            hops: vec![],
            total_profit_wei: U256::from((i + 1) as u64 * 1_000_000_000_000_000u64),
            total_gas: 200_000,
            gas_cost_wei: U256::from(500_000_000_000_000u64),
            net_profit_wei: U256::from(i as u64 * 1_000_000_000_000_000u64),
            block_number: 18_000_000,
            timestamp_ns: 0,
        };
        collector.insert(RankedOpportunity::new(opp));
    }
    assert_eq!(collector.len(), 3);
    let r = collector.results();
    assert!(r[0].score >= r[1].score);
    assert!(r[1].score >= r[2].score);
}

/// Test 8: Token index consistency
#[test]
fn test_token_index_full_pipeline() {
    let mut index = TokenIndex::new();
    let tokens = [USDC, WETH, USDT, DAI];
    let indices: Vec<usize> = tokens.iter().map(|t| index.get_or_insert(*t)).collect();

    let mut unique = indices.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), indices.len());

    for (i, token) in tokens.iter().enumerate() {
        assert_eq!(index.get_address(indices[i]).unwrap(), token);
        assert_eq!(index.get_index(token).unwrap(), indices[i]);
    }

    assert_eq!(index.get_or_insert(USDC), indices[0]);
    assert_eq!(index.len(), 4);
}

/// Test 9: Bellman-Ford triangular arb detection
#[test]
fn test_bellman_ford_triangular_arb() {
    let mut graph = PriceGraph::new(3);
    let pool_a = PoolId {
        address: Address::ZERO,
        protocol: ProtocolType::UniswapV2,
    };
    let pool_b = PoolId {
        address: address!("0000000000000000000000000000000000000001"),
        protocol: ProtocolType::SushiSwap,
    };
    let pool_c = PoolId {
        address: address!("0000000000000000000000000000000000000002"),
        protocol: ProtocolType::Curve,
    };

    graph.add_edge(0, 1, 1.01, pool_a, Address::ZERO, ProtocolType::UniswapV2, U256::ZERO);
    graph.add_edge(
        1,
        2,
        1.01,
        pool_b,
        address!("0000000000000000000000000000000000000001"),
        ProtocolType::SushiSwap,
        U256::ZERO,
    );
    graph.add_edge(
        2,
        0,
        1.01,
        pool_c,
        address!("0000000000000000000000000000000000000002"),
        ProtocolType::Curve,
        U256::ZERO,
    );

    let bf = BellmanFord::new(5, 3_000_000);
    let cycles = bf.detect_negative_cycles(&graph);
    assert!(!cycles.is_empty(), "Should detect negative cycle");
    assert!(cycles[0].is_profitable());
}

/// Test 10: Snapshot concurrent access
#[test]
fn test_snapshot_concurrent_access() {
    let graph = PriceGraph::new(10);
    let mgr = std::sync::Arc::new(SnapshotManager::new(graph));
    let mut handles = vec![];

    for _ in 0..4 {
        let m = mgr.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let snap = m.load();
                let _ = snap.graph.num_vertices();
            }
        }));
    }

    for i in 0..50u64 {
        let mut g = PriceGraph::new(10);
        g.add_edge(
            0,
            1,
            1.0 + i as f64 * 0.01,
            PoolId {
                address: Address::ZERO,
                protocol: ProtocolType::UniswapV2,
            },
            Address::ZERO,
            ProtocolType::UniswapV2,
            U256::ZERO,
        );
        mgr.publish(g, i, 0);
    }

    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(mgr.version(), 50);
}
