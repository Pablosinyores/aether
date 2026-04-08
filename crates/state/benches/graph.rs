use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use aether_common::types::{PoolId, ProtocolType};
use aether_state::price_graph::PriceGraph;
use alloy::primitives::{Address, U256};

fn make_pool_id(byte: u8, protocol: ProtocolType) -> PoolId {
    PoolId {
        address: Address::repeat_byte(byte),
        protocol,
    }
}

fn bench_add_edge(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_add_edge");

    for &n in &[100, 500, 1000] {
        group.bench_with_input(BenchmarkId::new("new_edges", n), &n, |b, &n| {
            b.iter(|| {
                let mut g = PriceGraph::new(n);
                for i in 0..n {
                    let j = (i + 1) % n;
                    let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
                    g.add_edge(
                        i,
                        j,
                        1.05,
                        pid,
                        Address::repeat_byte((i % 255) as u8),
                        ProtocolType::UniswapV2,
                        U256::from(1_000_000u64),
                    );
                }
                black_box(&g);
            })
        });
    }

    // Benchmark edge update (same pool_id overwrites existing edge)
    for &n in &[100, 500] {
        group.bench_with_input(BenchmarkId::new("update_existing", n), &n, |b, &n| {
            let mut g = PriceGraph::new(n);
            for i in 0..n {
                let j = (i + 1) % n;
                let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
                g.add_edge(
                    i,
                    j,
                    1.05,
                    pid,
                    Address::repeat_byte((i % 255) as u8),
                    ProtocolType::UniswapV2,
                    U256::from(1_000_000u64),
                );
            }
            g.clear_dirty();

            b.iter(|| {
                for i in 0..n {
                    let j = (i + 1) % n;
                    let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
                    g.add_edge(
                        i,
                        j,
                        1.06,
                        pid,
                        Address::repeat_byte((i % 255) as u8),
                        ProtocolType::UniswapV2,
                        U256::from(2_000_000u64),
                    );
                }
                black_box(&g);
            })
        });
    }

    group.finish();
}

fn bench_edges_from(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_edges_from");

    for &n in &[100, 500, 1000] {
        // Build graph with ~3 edges per vertex
        let mut g = PriceGraph::new(n);
        let protocols = [
            ProtocolType::UniswapV2,
            ProtocolType::UniswapV3,
            ProtocolType::SushiSwap,
        ];
        for i in 0..n {
            for (k, &proto) in protocols.iter().enumerate() {
                let j = (i + k + 1) % n;
                let pid = make_pool_id(((i * 3 + k) % 255) as u8, proto);
                g.add_edge(
                    i,
                    j,
                    1.05,
                    pid,
                    Address::repeat_byte(((i * 3 + k) % 255) as u8),
                    proto,
                    U256::from(1_000_000u64),
                );
            }
        }

        group.bench_with_input(BenchmarkId::new("lookup", n), &g, |b, g| {
            b.iter(|| {
                for i in 0..n {
                    black_box(g.edges_from(i));
                }
            })
        });
    }

    group.finish();
}

fn bench_affected_vertices(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_affected_vertices");

    for &n in &[100, 500, 1000] {
        let mut g = PriceGraph::new(n);
        for i in 0..n {
            let j = (i + 1) % n;
            let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
            g.add_edge(
                i,
                j,
                1.05,
                pid,
                Address::repeat_byte((i % 255) as u8),
                ProtocolType::UniswapV2,
                U256::from(1_000_000u64),
            );
        }
        // All edges are dirty (just added)

        group.bench_with_input(BenchmarkId::new("all_dirty", n), &g, |b, g| {
            b.iter(|| black_box(g.affected_vertices()))
        });
    }

    group.finish();
}

fn bench_clear_dirty(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_clear_dirty");

    for &n in &[100, 500, 1000] {
        let mut g = PriceGraph::new(n);
        for i in 0..n {
            let j = (i + 1) % n;
            let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
            g.add_edge(
                i,
                j,
                1.05,
                pid,
                Address::repeat_byte((i % 255) as u8),
                ProtocolType::UniswapV2,
                U256::from(1_000_000u64),
            );
        }

        group.bench_with_input(BenchmarkId::new("edges", n), &n, |b, _| {
            b.iter(|| {
                g.clear_dirty();
                // Re-dirty one edge for the next iter
                let pid = make_pool_id(0, ProtocolType::UniswapV2);
                g.add_edge(
                    0,
                    1,
                    1.06,
                    pid,
                    Address::repeat_byte(0),
                    ProtocolType::UniswapV2,
                    U256::from(1_000_000u64),
                );
            })
        });
    }

    group.finish();
}

fn bench_update_edge_from_reserves(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_update_reserves");

    for &n in &[100, 500] {
        let mut g = PriceGraph::new(n);
        let mut pool_ids = Vec::new();
        for i in 0..n {
            let j = (i + 1) % n;
            let pid = make_pool_id((i % 255) as u8, ProtocolType::UniswapV2);
            pool_ids.push((i, j, pid));
            g.add_edge(
                i,
                j,
                1.05,
                pid,
                Address::repeat_byte((i % 255) as u8),
                ProtocolType::UniswapV2,
                U256::from(1_000_000u64),
            );
        }
        g.clear_dirty();

        group.bench_with_input(BenchmarkId::new("batch", n), &pool_ids, |b, pool_ids| {
            b.iter(|| {
                for &(from, to, pid) in pool_ids.iter() {
                    g.update_edge_from_reserves(from, to, pid, 1000.0, 2000.0, 0.997);
                }
                black_box(&g);
            })
        });
    }

    group.finish();
}

/// Benchmark edge lookup via `update_edge_from_reserves` on a graph with 500+
/// edges. This is the hot-path operation that benefits from the `edge_index`
/// HashMap -- previously O(E) per update, now O(1).
fn bench_edge_index_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("price_graph_edge_index_lookup");

    for &n in &[500, 1000, 2000] {
        let num_vertices = n / 2;
        let mut g = PriceGraph::new(num_vertices);
        let mut pool_ids = Vec::with_capacity(n);
        let protocols = [
            ProtocolType::UniswapV2,
            ProtocolType::UniswapV3,
            ProtocolType::SushiSwap,
        ];

        // Build a graph with `n` edges across multiple protocols.
        for i in 0..n {
            let from = i % num_vertices;
            let to = (i + 1) % num_vertices;
            let proto = protocols[i % protocols.len()];
            let pid = PoolId {
                address: Address::from_slice(&[
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    (i >> 24) as u8,
                    (i >> 16) as u8,
                    (i >> 8) as u8,
                    (i & 0xff) as u8,
                    proto as u8,
                    0,
                ]),
                protocol: proto,
            };
            pool_ids.push((from, to, pid));
            g.add_edge(
                from,
                to,
                1.05,
                pid,
                pid.address,
                proto,
                U256::from(1_000_000u64),
            );
        }
        g.clear_dirty();

        group.bench_with_input(
            BenchmarkId::new("update_reserves", n),
            &pool_ids,
            |b, pool_ids| {
                b.iter(|| {
                    g.clear_dirty();
                    for &(from, to, pid) in pool_ids.iter() {
                        g.update_edge_from_reserves(from, to, pid, 1000.0, 2050.0, 0.997);
                    }
                    black_box(&g);
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("add_edge_update", n),
            &pool_ids,
            |b, pool_ids| {
                b.iter(|| {
                    g.clear_dirty();
                    for &(from, to, pid) in pool_ids.iter() {
                        g.add_edge(
                            from,
                            to,
                            1.06,
                            pid,
                            pid.address,
                            pid.protocol,
                            U256::from(2_000_000u64),
                        );
                    }
                    black_box(&g);
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_add_edge,
    bench_edges_from,
    bench_affected_vertices,
    bench_clear_dirty,
    bench_update_edge_from_reserves,
    bench_edge_index_lookup,
);
criterion_main!(benches);
