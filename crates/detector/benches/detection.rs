use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use aether_common::types::{PoolId, ProtocolType};
use aether_detector::bellman_ford::BellmanFord;
use aether_state::price_graph::PriceGraph;
use alloy::primitives::{Address, U256};

fn make_pool_id(byte: u8, protocol: ProtocolType) -> PoolId {
    PoolId {
        address: Address::repeat_byte(byte),
        protocol,
    }
}

/// Build a ring graph: 0→1→2→...→(n-1)→0 with the given exchange rate.
fn build_ring_graph(n: usize, rate: f64) -> PriceGraph {
    let mut g = PriceGraph::new(n);
    for i in 0..n {
        let j = (i + 1) % n;
        let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
        g.add_edge(
            i,
            j,
            rate,
            pid,
            Address::repeat_byte((i % 255) as u8),
            ProtocolType::UniswapV2,
            U256::from(1_000_000u64),
        );
    }
    g
}

/// Build a mesh graph: every vertex connects to 3-4 neighbors.
fn build_mesh_graph(n: usize, rate: f64) -> PriceGraph {
    let mut g = PriceGraph::new(n);
    let protocols = [
        ProtocolType::UniswapV2,
        ProtocolType::UniswapV3,
        ProtocolType::SushiSwap,
        ProtocolType::Curve,
    ];
    for i in 0..n {
        for offset in 1..=3 {
            let j = (i + offset) % n;
            let pid = make_pool_id(((i * 3 + offset) % 255) as u8, protocols[offset % 4]);
            g.add_edge(
                i,
                j,
                rate,
                pid,
                Address::repeat_byte(((i * 3 + offset) % 255) as u8),
                protocols[offset % 4],
                U256::from(1_000_000u64),
            );
        }
    }
    g
}

fn bench_detect_negative_cycles(c: &mut Criterion) {
    let mut group = c.benchmark_group("bellman_ford_full_scan");
    let bf = BellmanFord::new(6, 3_000_000); // 3ms budget

    for &n in &[50, 100, 500] {
        // Ring with profitable cycle (rate > 1)
        let graph = build_ring_graph(n, 1.01);
        group.bench_with_input(
            BenchmarkId::new("ring_profitable", n),
            &graph,
            |b, g| b.iter(|| bf.detect_negative_cycles(black_box(g))),
        );

        // Ring with no cycle (rate < 1)
        let graph = build_ring_graph(n, 0.99);
        group.bench_with_input(
            BenchmarkId::new("ring_no_cycle", n),
            &graph,
            |b, g| b.iter(|| bf.detect_negative_cycles(black_box(g))),
        );
    }

    for &n in &[50, 100, 500] {
        let graph = build_mesh_graph(n, 1.02);
        group.bench_with_input(
            BenchmarkId::new("mesh_profitable", n),
            &graph,
            |b, g| b.iter(|| bf.detect_negative_cycles(black_box(g))),
        );
    }

    group.finish();
}

fn bench_detect_from_affected(c: &mut Criterion) {
    let mut group = c.benchmark_group("bellman_ford_affected");
    let bf = BellmanFord::new(6, 3_000_000);

    for &n in &[100, 500, 1000] {
        let graph = build_ring_graph(n, 1.01);

        // Seed from a single affected vertex
        group.bench_with_input(
            BenchmarkId::new("single_affected", n),
            &graph,
            |b, g| b.iter(|| bf.detect_from_affected(black_box(g), &[0])),
        );

        // Seed from 5% of vertices
        let affected: Vec<usize> = (0..n).step_by(20).collect();
        group.bench_with_input(
            BenchmarkId::new("5pct_affected", n),
            &graph,
            |b, g| b.iter(|| bf.detect_from_affected(black_box(g), &affected)),
        );
    }

    group.finish();
}

fn bench_time_budget(c: &mut Criterion) {
    let mut group = c.benchmark_group("bellman_ford_time_budget");

    let graph = build_mesh_graph(500, 1.01);

    for &budget_us in &[100, 1000, 3000] {
        let bf = BellmanFord::new(6, budget_us);
        group.bench_with_input(
            BenchmarkId::new("mesh_500", budget_us),
            &graph,
            |b, g| b.iter(|| bf.detect_negative_cycles(black_box(g))),
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_detect_negative_cycles,
    bench_detect_from_affected,
    bench_time_budget,
);
criterion_main!(benches);
