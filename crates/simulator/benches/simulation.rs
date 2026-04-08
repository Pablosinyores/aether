use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use aether_simulator::fork::{ForkedState, SimConfig};
use aether_simulator::EvmSimulator;
use alloy::primitives::{address, Address, U256};

/// Build a ForkedState with a funded caller and a simple contract that
/// performs SSTORE + SLOAD work (simulates real gas usage).
fn setup_sim_state(caller: Address) -> ForkedState {
    let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 30);
    state.insert_account_balance(caller, U256::from(100_000_000_000_000_000_000u128));

    // Contract: PUSH1 0x42 PUSH1 0x00 SSTORE PUSH1 0x00 SLOAD POP PUSH1 0x00 PUSH1 0x00 RETURN
    // Does an SSTORE + SLOAD to burn ~25k gas, then returns.
    let bytecode = vec![
        0x60, 0x42, // PUSH1 0x42
        0x60, 0x00, // PUSH1 0x00
        0x55, // SSTORE
        0x60, 0x00, // PUSH1 0x00
        0x54, // SLOAD
        0x50, // POP
        0x60, 0x00, // PUSH1 0x00
        0x60, 0x00, // PUSH1 0x00
        0xf3, // RETURN
    ];
    let contract = address!("1111111111111111111111111111111111111111");
    state.insert_account(
        contract,
        U256::ZERO,
        alloy::primitives::Bytes::from(bytecode),
    );

    state
}

fn bench_sequential_vs_parallel(c: &mut Criterion) {
    // NOTE: This benchmark measures CPU-bound revm simulation only (empty state,
    // no RPC). In production with RpcForkedState, cold RPC fetches dominate each
    // simulation (~8ms each), where parallelism delivers near-linear speedup by
    // overlapping network I/O across candidates. The pre-warmed CacheDB eliminates
    // repeated RPC round-trips for the same pool state within a block.
    let mut group = c.benchmark_group("simulation_parallelism");

    let caller = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let contract = address!("1111111111111111111111111111111111111111");
    let config = SimConfig {
        gas_limit: 200_000,
        chain_id: 1,
        caller,
        value: U256::ZERO,
    };

    for &n_candidates in [1, 2, 4, 8].iter() {
        // Benchmark sequential simulation.
        group.bench_with_input(
            BenchmarkId::new("sequential", n_candidates),
            &n_candidates,
            |b, &n| {
                let state = setup_sim_state(caller);
                let sim = EvmSimulator::new(config.clone());
                b.iter(|| {
                    for _ in 0..n {
                        let result = sim.simulate(
                            black_box(&state),
                            black_box(contract),
                            black_box(vec![]),
                        );
                        black_box(result);
                    }
                });
            },
        );

        // Benchmark parallel simulation (spawn_blocking via tokio).
        group.bench_with_input(
            BenchmarkId::new("parallel", n_candidates),
            &n_candidates,
            |b, &n| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .build()
                    .unwrap();

                b.iter(|| {
                    rt.block_on(async {
                        let handles: Vec<_> = (0..n)
                            .map(|_| {
                                let cfg = config.clone();
                                tokio::task::spawn_blocking(move || {
                                    let state = setup_sim_state(caller);
                                    let sim = EvmSimulator::new(cfg);
                                    let result = sim.simulate(
                                        black_box(&state),
                                        black_box(contract),
                                        black_box(vec![]),
                                    );
                                    black_box(result);
                                })
                            })
                            .collect();

                        for h in handles {
                            h.await.unwrap();
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_sequential_vs_parallel);
criterion_main!(benches);
