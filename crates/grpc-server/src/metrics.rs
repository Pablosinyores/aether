use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry, TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub struct EngineMetrics {
    registry: Registry,
    detection_latency_ms: Histogram,
    simulation_latency_ms: Histogram,
    cycles_detected: IntCounter,
    simulations_run: IntCounter,
    arbs_published: IntCounter,
    blocks_processed: IntCounter,
    decode_errors: IntCounterVec,
    /// Counter for EVM fork-replay fallbacks invoked from the analytical
    /// post-state predictors. Bumped whenever an analytical predictor
    /// returns a low-confidence result and the caller escalates to an
    /// EVM fork. The label set is intentionally small so dashboards can
    /// stay stable across releases.
    ///
    /// Stable label set:
    ///   - `v3_tick_crossed` — V3 swap moved sqrt_price out of its
    ///     single-tick bucket
    ///   - `curve_unconverged` — Curve Newton iteration produced an
    ///     invalid post-state
    ///   - `balancer_unequal_weight` — Balancer first-order Taylor
    ///     approximation deemed too coarse under heavy weight skew
    ///   - `unknown_protocol` — protocol family with no analytical
    ///     predictor on this build
    sim_evm_fallback_total: IntCounterVec,
    /// Detected cycles dropped by the multi-signal candidate gating layer
    /// before they reach the EVM fork simulator. Each label corresponds to
    /// a specific corruption signature the gating recognises; the cardinality
    /// is fixed at compile time so dashboards can pre-render every panel.
    ///
    /// Stable label set:
    ///   - `profit_factor_impossible` — `profit_factor > 100.0` (10000%);
    ///     pure f64-overflow / NaN territory, never a real opportunity
    ///   - `reserves_too_low` — at least one edge in the cycle has
    ///     `reserve_in < MIN_RESERVE_F64` (default 1e6 wei); pool is
    ///     effectively empty and the implied rate is noise
    ///   - `fingerprint_cluster` — five or more sibling cycles share the
    ///     same `profit_factor` (quantised to 1e-6); signature of a single
    ///     corrupt edge being walked through many path permutations
    ///   - `revm_contradicts` — post-sim check: `actual_profit_wei` from
    ///     revm differs from `expected_net_wei` by more than 50%; either
    ///     the detector's local graph is wrong or the simulation reverted
    ///     in a way that masked the failure
    cycle_gate_dropped_total: IntCounterVec,
    /// Pending DEX-router txs forwarded by the mempool subscription, labelled
    /// by router (raw address) and the decoded protocol family. The
    /// `decoded` label distinguishes successful ABI parses from
    /// `decode_failure` so dashboards can surface decoder gaps directly.
    pending_dex_tx_total: IntCounterVec,
    /// Reason-tagged decoder failure counter. Reasons match
    /// `aether_pools::router_decoder::DecodeError` variants so a dashboard
    /// drill-down points at the exact path that needs work next.
    pending_decode_errors_total: IntCounterVec,
    /// Profitable cycles found by the post-state mempool simulator, labelled
    /// by router and a coarse profit bucket. Counts candidates only — these
    /// are not validated arbs and never get submitted; they prove the
    /// post-state pipeline produces non-empty output on real traffic.
    pending_arb_candidates_total: IntCounterVec,
    /// Reasons the post-state simulator skipped a decoded swap (no pool in
    /// registry, missing token index, no graph edge, zero reserves, etc.).
    /// Mirrors `pending_decode_errors_total` for the layer above the decoder.
    pending_arb_sim_skipped_total: IntCounterVec,
    /// Pending-tx broadcast events the decode pipeline failed to receive
    /// because it lagged behind the producer (tokio broadcast `Lagged(n)`).
    /// Bumped by the `n` returned by the broadcast receiver so dashboards
    /// can show *how many events* were dropped, not just how many lag
    /// events fired. Sustained non-zero growth = pipeline is the bottleneck;
    /// either widen the channel or shed mempool sources.
    pending_pipeline_lagged_total: IntCounter,
    /// Reason-tagged counter for pending swaps the pipeline drops *after*
    /// the router decoder succeeds but *before* the post-state simulator
    /// gets a chance to run. Distinct from `pending_arb_sim_skipped_total`
    /// (which fires once the sim task has started and discovered a missing
    /// graph edge / zero reserves / etc.). Bumping here short-circuits the
    /// 3.8 MB graph clone the sim does, so it is cheap to be aggressive.
    ///
    /// Stable label set:
    ///   - `not_in_registry` — neither (token_in, token_out, protocol)
    ///     tuple is present in `pool_registry`
    ///   - `same_token` — decoder returned a self-swap (likely
    ///     fee-on-transfer wrapper)
    ///   - `zero_amount` — `amount_in == 0` (no profit possible)
    mempool_filtered_total: IntCounterVec,
    /// Per-attempt revm validation latency for the mempool-backrun path,
    /// split by accept / reject. `accept` rows capture only successful
    /// double-tx sims (victim + arb), `reject` rows include every other
    /// outcome so dashboards can decompose tail behaviour by reason.
    mempool_backrun_validation_latency_ms: HistogramVec,
    /// Validated mempool-backrun candidates ready for executor publish,
    /// bucketed by predicted profit so dashboards can show distribution.
    mempool_backrun_validated_total: IntCounterVec,
    /// Reason-tagged reject counter for the mempool-backrun validator.
    ///
    /// Stable label set:
    ///   - `victim_reverted` — victim tx reverts when applied to the fork
    ///   - `victim_halted` — victim tx hit revm `Halt` (OOG, invalid opcode)
    ///   - `arb_reverted` — our arb tx reverts after the victim
    ///   - `arb_halted` — our arb tx hit revm `Halt`
    ///   - `negative_after_gas` — sim succeeded but `profit_wei <= gas_cost`
    ///   - `sim_timeout` — sim wall-clock exceeded `AETHER_MEMPOOL_SIM_TIMEOUT_MS`
    ///   - `victim_stale` — `seen_at` older than freshness threshold
    ///   - `sim_error` — non-revert / non-halt revm error
    mempool_backrun_rejected_total: IntCounterVec,
    /// In-flight mempool-backrun validation sims. Bounded by the
    /// `AETHER_MEMPOOL_SIM_CONCURRENCY` semaphore; exposed so dashboards can
    /// alert on saturation (gauge at ceiling = victims being dropped).
    mempool_backrun_sim_concurrent: IntGauge,
    /// Wall-clock delta between when this process first saw a pending tx
    /// (`PendingTxEvent::first_seen_unix_nanos` stamp at the ingestion
    /// boundary) and when that tx landed on chain (observed via
    /// `NewBlockEvent::tx_hashes`). Lower is better — measures how far
    /// behind block-builder mempool visibility we run. Tail latency
    /// (p95, p99) is the operational signal; sustained p99 > 1s means
    /// our gossip path is laggy vs. builders.
    mempool_first_seen_to_inclusion_ms: Histogram,
    /// Tracker-side bookkeeping so dashboards can sanity-check the
    /// histogram before reading deltas from it.
    ///
    /// Stable label set:
    ///   - `recorded` — pending tx stamped + inserted into the tracker
    ///   - `evicted_capacity` — oldest entry dropped to honour the
    ///     bounded-LRU capacity ceiling
    ///   - `matched` — block tx hash hit a tracked entry and produced a
    ///     histogram observation
    ///   - `unmatched` — block tx hash with no tracker entry (either
    ///     pre-tracker-start, private flow, or evicted before inclusion)
    ///   - `unstamped` — pending tx arrived with `first_seen_unix_nanos
    ///     == 0` (test fixture or upstream regression) and was skipped
    mempool_first_seen_events_total: IntCounterVec,
    /// Outcome counter for the post-state revm replay fallback. Bumped
    /// once per replay attempt. Sits one layer below
    /// `sim_evm_fallback_total`: the fallback metric records *why* the
    /// analytical predictor escalated, while this one records what the
    /// EVM fork-replay was able to do with the escalation.
    ///
    /// Stable label set:
    ///   - `success` — replay produced a usable `V3PostState`
    ///   - `victim_reverted` — victim tx reverted on the fork
    ///   - `victim_halted` — victim hit revm `Halt` (OOG, invalid opcode)
    ///   - `read_call_failed` — `slot0()` / `liquidity()` view call failed
    ///     post-victim (pool address has no bytecode, or returned a revert)
    ///   - `decode_failed` — view call succeeded but ABI decode of the
    ///     return tuple failed (corrupt pool or unexpected ABI shape)
    ///   - `sim_error` — non-revert / non-halt revm error (likely AlloyDB
    ///     RPC failure mid-execution)
    ///   - `unimplemented_protocol` — replayer invoked for Curve or
    ///     Balancer, which have not yet landed their view-call shapes
    ///   - `timeout` — replay wall-clock exceeded the configured budget
    mempool_post_state_replay_total: IntCounterVec,
    /// Wall-clock latency of post-state replay attempts. Captures both
    /// success and failure paths so the tail behaviour is visible — a
    /// replay that hits an RPC stall on a cold storage slot looks very
    /// different from one served entirely from the warm `CacheDB`.
    mempool_post_state_replay_latency_ms: Histogram,
    /// Counter for mempool-backrun shadow-sim attempts that received a
    /// pre-warmed bytecode + storage payload before the revm sim ran.
    /// `result=hit` when a `PrewarmedState` was loaded and injected,
    /// `result=miss` when the long-lived `ArcSwap` is still empty (the
    /// background refresher has not produced its first snapshot yet).
    mempool_prewarm_inject_total: IntCounterVec,
    /// Counter for the periodic mempool pre-warm refresh task. `result=ok`
    /// on a successful registry snapshot + RPC fetch, `result=error` is
    /// reserved for future refresh-side failures (the current implementation
    /// only logs warnings inside `prewarm_state`).
    mempool_prewarm_refresh_total: IntCounterVec,
    /// Wall-clock latency of one refresh cycle, ms. Buckets mirror
    /// `mempool_backrun_validation_latency_ms` so the two histograms share a
    /// dashboard layout.
    mempool_prewarm_refresh_duration_ms: Histogram,
    /// Pools whose bytecode + V2 reserve slots were warmed by the most
    /// recent refresh. Set to the snapshot's pool count on success.
    mempool_prewarm_warm_pools: IntGauge,
    /// Code addresses pre-warm served from the persistent bytecode cache
    /// without issuing `eth_getCode`. Counted per address per cycle so the
    /// rate metric tracks "RPC calls avoided", not just cycles touched.
    prewarm_bytecode_cache_hits_total: IntCounter,
    /// Code addresses pre-warm had to fetch via `eth_getCode` (cache miss
    /// or no cache configured). The ratio of this to
    /// `prewarm_bytecode_cache_hits_total` is the headline RPC-reduction
    /// dashboard.
    prewarm_bytecode_rpc_fetches_total: IntCounter,
    /// V2 pool addresses pre-warm served from the WS-fed reserves cache
    /// without issuing `eth_getStorageAt`.
    prewarm_v2_reserves_cache_hits_total: IntCounter,
    /// V2 pool addresses present in the WS cache but rejected by the
    /// freshness gate (older than `V2_RESERVES_MAX_LAG_BLOCKS`).
    /// Sustained non-zero growth = WS subscription lag or reorg churn.
    prewarm_v2_reserves_cache_stale_total: IntCounter,
    /// V2 pool addresses absent from the WS cache (never recorded by the
    /// `Sync` writer). Expected to decay toward zero as the cache warms.
    prewarm_v2_reserves_cache_missing_total: IntCounter,
    /// Multicall3 batches dispatched by the pre-warm storage path. A batch
    /// collapses N `eth_getStorageAt` round-trips into a single `eth_call`,
    /// so the rate of this counter is the rate of "N→1" RPC compressions.
    prewarm_multicall_batches_total: IntCounter,
    /// V2 pool reserves served via a Multicall3 sub-call rather than an
    /// individual `eth_getStorageAt`. Each entry saves ~20 Alchemy CU vs the
    /// per-pool path; the ratio of this to `prewarm_v2_reserves_cache_*` is
    /// the dashboard for "how many slots paid the eth_call price vs the
    /// per-storage price".
    prewarm_multicall_v2_slots_total: IntCounter,
    /// Multicall batches that errored end-to-end (network, decode, contract
    /// unreachable) and forced the per-pool stream to handle the entire
    /// batch. Should sit at zero in steady state — sustained growth is the
    /// signal that the configured Multicall3 deployment is unreachable on
    /// this chain.
    prewarm_multicall_fallbacks_total: IntCounter,
}

impl EngineMetrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let detection_latency_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_detection_latency_ms",
                "Detection latency in milliseconds",
            )
            .buckets(vec![0.1, 0.5, 1.0, 3.0, 5.0, 10.0, 50.0]),
        )
        .expect("aether_detection_latency_ms histogram");
        let simulation_latency_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_simulation_latency_ms",
                "EVM simulation latency in milliseconds",
            )
            .buckets(vec![
                0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0,
            ]),
        )
        .expect("aether_simulation_latency_ms histogram");
        let cycles_detected = IntCounter::new(
            "aether_cycles_detected_total",
            "Total negative cycles detected",
        )
        .expect("aether_cycles_detected_total counter");
        let simulations_run =
            IntCounter::new("aether_simulations_run_total", "Total simulations executed")
                .expect("aether_simulations_run_total counter");
        let arbs_published = IntCounter::new(
            "aether_arbs_published_total",
            "Total validated arbs published",
        )
        .expect("aether_arbs_published_total counter");
        let blocks_processed =
            IntCounter::new("aether_blocks_processed_total", "Total blocks processed")
                .expect("aether_blocks_processed_total counter");
        let decode_errors = IntCounterVec::new(
            Opts::new(
                "aether_decode_errors_total",
                "Total logs the event decoder could not parse, labelled by reason",
            ),
            &["reason"],
        )
        .expect("aether_decode_errors_total counter vec");
        let sim_evm_fallback_total = IntCounterVec::new(
            Opts::new(
                "aether_sim_evm_fallback_total",
                "EVM fork-replay fallbacks invoked from the analytical post-state predictors, by reason",
            ),
            &["reason"],
        )
        .expect("aether_sim_evm_fallback_total counter vec");
        let cycle_gate_dropped_total = IntCounterVec::new(
            Opts::new(
                "aether_cycle_gate_dropped_total",
                "Detected cycles dropped by the multi-signal candidate gating layer, by reason",
            ),
            &["reason"],
        )
        .expect("aether_cycle_gate_dropped_total counter vec");
        let pending_dex_tx_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_dex_tx_total",
                "Pending DEX-router txs forwarded by the mempool subscription, by router and decoded protocol",
            ),
            &["router", "protocol", "decoded"],
        )
        .expect("aether_pending_dex_tx_total counter vec");
        let pending_decode_errors_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_decode_errors_total",
                "Pending-tx calldata decoder failures, by reason",
            ),
            &["reason"],
        )
        .expect("aether_pending_decode_errors_total counter vec");
        let pending_arb_candidates_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_arb_candidates_total",
                "Profitable cycles found by the post-state mempool simulator, by router and profit bucket",
            ),
            &["router", "profit_bucket"],
        )
        .expect("aether_pending_arb_candidates_total counter vec");
        let pending_arb_sim_skipped_total = IntCounterVec::new(
            Opts::new(
                "aether_pending_arb_sim_skipped_total",
                "Decoded swaps the post-state simulator skipped, by reason",
            ),
            &["reason"],
        )
        .expect("aether_pending_arb_sim_skipped_total counter vec");
        let pending_pipeline_lagged_total = IntCounter::new(
            "aether_pending_pipeline_lagged_total",
            "Pending-tx events dropped because the decode pipeline lagged behind the broadcast",
        )
        .expect("aether_pending_pipeline_lagged_total counter");
        let mempool_filtered_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_filtered_total",
                "Decoded pending swaps dropped before the post-state simulator runs, by reason",
            ),
            &["reason"],
        )
        .expect("aether_mempool_filtered_total counter vec");
        let mempool_backrun_validation_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "aether_mempool_backrun_validation_latency_ms",
                "Per-attempt revm validation latency for the mempool-backrun path, ms",
            )
            .buckets(vec![
                0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 250.0, 500.0,
            ]),
            &["result"],
        )
        .expect("aether_mempool_backrun_validation_latency_ms histogram vec");
        let mempool_backrun_validated_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_backrun_validated_total",
                "Mempool-backrun candidates that passed revm validation, by profit bucket",
            ),
            &["profit_bucket"],
        )
        .expect("aether_mempool_backrun_validated_total counter vec");
        let mempool_backrun_rejected_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_backrun_rejected_total",
                "Mempool-backrun candidates rejected by the revm validator, by reason",
            ),
            &["reason"],
        )
        .expect("aether_mempool_backrun_rejected_total counter vec");
        let mempool_backrun_sim_concurrent = IntGauge::new(
            "aether_mempool_backrun_sim_concurrent",
            "In-flight mempool-backrun validation sims, bounded by the semaphore",
        )
        .expect("aether_mempool_backrun_sim_concurrent gauge");
        let mempool_first_seen_to_inclusion_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_mempool_first_seen_to_inclusion_ms",
                "Wall-clock ms from local first-seen of a pending tx to its on-chain inclusion",
            )
            // Cover sub-block (<12 s) and "we were way behind" tails. Buckets
            // hand-picked so the histogram is meaningful with as few as 200
            // samples — production traffic fills it in seconds.
            .buckets(vec![
                50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0, 60_000.0,
            ]),
        )
        .expect("aether_mempool_first_seen_to_inclusion_ms histogram");
        let mempool_first_seen_events_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_first_seen_events_total",
                "Tracker events for the mempool first-seen → inclusion histogram, by event kind",
            ),
            &["event"],
        )
        .expect("aether_mempool_first_seen_events_total counter vec");
        let mempool_post_state_replay_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_post_state_replay_total",
                "Outcomes of the revm post-state replay fallback, by outcome",
            ),
            &["outcome"],
        )
        .expect("aether_mempool_post_state_replay_total counter vec");
        let mempool_post_state_replay_latency_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_mempool_post_state_replay_latency_ms",
                "Wall-clock latency of post-state replay attempts, ms",
            )
            .buckets(vec![
                1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0,
            ]),
        )
        .expect("aether_mempool_post_state_replay_latency_ms histogram");
        let mempool_prewarm_inject_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_prewarm_inject_total",
                "Mempool-backrun shadow-sim attempts by pre-warm result (hit / miss)",
            ),
            &["result"],
        )
        .expect("aether_mempool_prewarm_inject_total counter vec");
        let mempool_prewarm_refresh_total = IntCounterVec::new(
            Opts::new(
                "aether_mempool_prewarm_refresh_total",
                "Periodic mempool pre-warm refresh attempts, by result (ok / error)",
            ),
            &["result"],
        )
        .expect("aether_mempool_prewarm_refresh_total counter vec");
        let mempool_prewarm_refresh_duration_ms = Histogram::with_opts(
            HistogramOpts::new(
                "aether_mempool_prewarm_refresh_duration_ms",
                "Wall-clock latency of one mempool pre-warm refresh cycle, ms",
            )
            .buckets(vec![
                0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 250.0, 500.0,
            ]),
        )
        .expect("aether_mempool_prewarm_refresh_duration_ms histogram");
        let mempool_prewarm_warm_pools = IntGauge::new(
            "aether_mempool_prewarm_warm_pools",
            "Pools whose bytecode + V2 reserve slots were warmed by the most recent refresh",
        )
        .expect("aether_mempool_prewarm_warm_pools gauge");
        let prewarm_bytecode_cache_hits_total = IntCounter::new(
            "aether_prewarm_bytecode_cache_hits_total",
            "Code addresses pre-warm served from the persistent bytecode cache without eth_getCode",
        )
        .expect("aether_prewarm_bytecode_cache_hits_total counter");
        let prewarm_bytecode_rpc_fetches_total = IntCounter::new(
            "aether_prewarm_bytecode_rpc_fetches_total",
            "Code addresses pre-warm fetched via eth_getCode (cache miss or cache disabled)",
        )
        .expect("aether_prewarm_bytecode_rpc_fetches_total counter");
        let prewarm_v2_reserves_cache_hits_total = IntCounter::new(
            "aether_prewarm_v2_reserves_cache_hits_total",
            "V2 pool addresses pre-warm served from the WS-fed reserves cache",
        )
        .expect("aether_prewarm_v2_reserves_cache_hits_total counter");
        let prewarm_v2_reserves_cache_stale_total = IntCounter::new(
            "aether_prewarm_v2_reserves_cache_stale_total",
            "V2 pool addresses present in cache but rejected by the freshness gate",
        )
        .expect("aether_prewarm_v2_reserves_cache_stale_total counter");
        let prewarm_v2_reserves_cache_missing_total = IntCounter::new(
            "aether_prewarm_v2_reserves_cache_missing_total",
            "V2 pool addresses never seen by the WS Sync writer",
        )
        .expect("aether_prewarm_v2_reserves_cache_missing_total counter");
        let prewarm_multicall_batches_total = IntCounter::new(
            "aether_prewarm_multicall_batches_total",
            "Multicall3 batches dispatched by the pre-warm storage path",
        )
        .expect("aether_prewarm_multicall_batches_total counter");
        let prewarm_multicall_v2_slots_total = IntCounter::new(
            "aether_prewarm_multicall_v2_slots_total",
            "V2 pool reserves served via a Multicall3 sub-call instead of eth_getStorageAt",
        )
        .expect("aether_prewarm_multicall_v2_slots_total counter");
        let prewarm_multicall_fallbacks_total = IntCounter::new(
            "aether_prewarm_multicall_fallbacks_total",
            "Multicall3 batches that errored and forced a per-pool eth_getStorageAt fallback",
        )
        .expect("aether_prewarm_multicall_fallbacks_total counter");

        registry
            .register(Box::new(detection_latency_ms.clone()))
            .expect("register aether_detection_latency_ms");
        registry
            .register(Box::new(simulation_latency_ms.clone()))
            .expect("register aether_simulation_latency_ms");
        registry
            .register(Box::new(cycles_detected.clone()))
            .expect("register aether_cycles_detected_total");
        registry
            .register(Box::new(simulations_run.clone()))
            .expect("register aether_simulations_run_total");
        registry
            .register(Box::new(arbs_published.clone()))
            .expect("register aether_arbs_published_total");
        registry
            .register(Box::new(blocks_processed.clone()))
            .expect("register aether_blocks_processed_total");
        registry
            .register(Box::new(decode_errors.clone()))
            .expect("register aether_decode_errors_total");
        registry
            .register(Box::new(sim_evm_fallback_total.clone()))
            .expect("register aether_sim_evm_fallback_total");
        registry
            .register(Box::new(cycle_gate_dropped_total.clone()))
            .expect("register aether_cycle_gate_dropped_total");
        registry
            .register(Box::new(pending_dex_tx_total.clone()))
            .expect("register aether_pending_dex_tx_total");
        registry
            .register(Box::new(pending_decode_errors_total.clone()))
            .expect("register aether_pending_decode_errors_total");
        registry
            .register(Box::new(pending_arb_candidates_total.clone()))
            .expect("register aether_pending_arb_candidates_total");
        registry
            .register(Box::new(pending_arb_sim_skipped_total.clone()))
            .expect("register aether_pending_arb_sim_skipped_total");
        registry
            .register(Box::new(pending_pipeline_lagged_total.clone()))
            .expect("register aether_pending_pipeline_lagged_total");
        registry
            .register(Box::new(mempool_filtered_total.clone()))
            .expect("register aether_mempool_filtered_total");
        registry
            .register(Box::new(mempool_backrun_validation_latency_ms.clone()))
            .expect("register aether_mempool_backrun_validation_latency_ms");
        registry
            .register(Box::new(mempool_backrun_validated_total.clone()))
            .expect("register aether_mempool_backrun_validated_total");
        registry
            .register(Box::new(mempool_backrun_rejected_total.clone()))
            .expect("register aether_mempool_backrun_rejected_total");
        registry
            .register(Box::new(mempool_backrun_sim_concurrent.clone()))
            .expect("register aether_mempool_backrun_sim_concurrent");
        registry
            .register(Box::new(mempool_first_seen_to_inclusion_ms.clone()))
            .expect("register aether_mempool_first_seen_to_inclusion_ms");
        registry
            .register(Box::new(mempool_first_seen_events_total.clone()))
            .expect("register aether_mempool_first_seen_events_total");
        registry
            .register(Box::new(mempool_post_state_replay_total.clone()))
            .expect("register aether_mempool_post_state_replay_total");
        registry
            .register(Box::new(mempool_post_state_replay_latency_ms.clone()))
            .expect("register aether_mempool_post_state_replay_latency_ms");
        registry
            .register(Box::new(mempool_prewarm_inject_total.clone()))
            .expect("register aether_mempool_prewarm_inject_total");
        registry
            .register(Box::new(mempool_prewarm_refresh_total.clone()))
            .expect("register aether_mempool_prewarm_refresh_total");
        registry
            .register(Box::new(mempool_prewarm_refresh_duration_ms.clone()))
            .expect("register aether_mempool_prewarm_refresh_duration_ms");
        registry
            .register(Box::new(mempool_prewarm_warm_pools.clone()))
            .expect("register aether_mempool_prewarm_warm_pools");
        registry
            .register(Box::new(prewarm_bytecode_cache_hits_total.clone()))
            .expect("register aether_prewarm_bytecode_cache_hits_total");
        registry
            .register(Box::new(prewarm_bytecode_rpc_fetches_total.clone()))
            .expect("register aether_prewarm_bytecode_rpc_fetches_total");
        registry
            .register(Box::new(prewarm_v2_reserves_cache_hits_total.clone()))
            .expect("register aether_prewarm_v2_reserves_cache_hits_total");
        registry
            .register(Box::new(prewarm_v2_reserves_cache_stale_total.clone()))
            .expect("register aether_prewarm_v2_reserves_cache_stale_total");
        registry
            .register(Box::new(prewarm_v2_reserves_cache_missing_total.clone()))
            .expect("register aether_prewarm_v2_reserves_cache_missing_total");
        registry
            .register(Box::new(prewarm_multicall_batches_total.clone()))
            .expect("register aether_prewarm_multicall_batches_total");
        registry
            .register(Box::new(prewarm_multicall_v2_slots_total.clone()))
            .expect("register aether_prewarm_multicall_v2_slots_total");
        registry
            .register(Box::new(prewarm_multicall_fallbacks_total.clone()))
            .expect("register aether_prewarm_multicall_fallbacks_total");

        // Pre-touch every label so dashboards see zero rows from boot.
        for ev in &[
            "recorded",
            "evicted_capacity",
            "matched",
            "unmatched",
            "unstamped",
        ] {
            mempool_first_seen_events_total.with_label_values(&[ev]);
        }

        // Pre-touch every outcome label so dashboards see a zero series
        // before the first replay rather than a missing one. Keeps panels
        // stable across cold starts.
        for outcome in [
            "success",
            "victim_reverted",
            "victim_halted",
            "read_call_failed",
            "decode_failed",
            "sim_error",
            "unimplemented_protocol",
            "timeout",
        ] {
            mempool_post_state_replay_total
                .with_label_values(&[outcome])
                .reset();
        }

        Self {
            registry,
            detection_latency_ms,
            simulation_latency_ms,
            cycles_detected,
            simulations_run,
            arbs_published,
            blocks_processed,
            decode_errors,
            sim_evm_fallback_total,
            cycle_gate_dropped_total,
            pending_dex_tx_total,
            pending_decode_errors_total,
            pending_arb_candidates_total,
            pending_arb_sim_skipped_total,
            pending_pipeline_lagged_total,
            mempool_filtered_total,
            mempool_backrun_validation_latency_ms,
            mempool_backrun_validated_total,
            mempool_backrun_rejected_total,
            mempool_backrun_sim_concurrent,
            mempool_first_seen_to_inclusion_ms,
            mempool_first_seen_events_total,
            mempool_post_state_replay_total,
            mempool_post_state_replay_latency_ms,
            mempool_prewarm_inject_total,
            mempool_prewarm_refresh_total,
            mempool_prewarm_refresh_duration_ms,
            mempool_prewarm_warm_pools,
            prewarm_bytecode_cache_hits_total,
            prewarm_bytecode_rpc_fetches_total,
            prewarm_v2_reserves_cache_hits_total,
            prewarm_v2_reserves_cache_stale_total,
            prewarm_v2_reserves_cache_missing_total,
            prewarm_multicall_batches_total,
            prewarm_multicall_v2_slots_total,
            prewarm_multicall_fallbacks_total,
        }
    }

    /// Apply one `PrewarmStats` snapshot to the five cache counters. Called
    /// by both the block-driven `Engine` pre-warm path and the mempool
    /// pre-warm refresher so the metric reflects all cycles, not just one.
    /// Zero-count fields are inc-by-zero (cheap no-op) — keeps the call
    /// site branch-free.
    pub fn record_prewarm_stats(&self, stats: aether_simulator::fork::PrewarmStats) {
        self.prewarm_bytecode_cache_hits_total
            .inc_by(stats.bytecode_cache_hits as u64);
        self.prewarm_bytecode_rpc_fetches_total
            .inc_by(stats.bytecode_rpc_fetches as u64);
        self.prewarm_v2_reserves_cache_hits_total
            .inc_by(stats.v2_reserves_cache_hits as u64);
        self.prewarm_v2_reserves_cache_stale_total
            .inc_by(stats.v2_reserves_cache_stale as u64);
        self.prewarm_v2_reserves_cache_missing_total
            .inc_by(stats.v2_reserves_cache_missing as u64);
        self.prewarm_multicall_batches_total
            .inc_by(stats.multicall_batches as u64);
        self.prewarm_multicall_v2_slots_total
            .inc_by(stats.multicall_v2_slots as u64);
        self.prewarm_multicall_fallbacks_total
            .inc_by(stats.multicall_fallbacks as u64);
    }

    /// Read the multicall batches counter. Public so tests can assert
    /// wiring without re-parsing Prometheus text.
    pub fn prewarm_multicall_batches_count(&self) -> u64 {
        self.prewarm_multicall_batches_total.get()
    }

    /// Read the multicall V2-slots-served counter.
    pub fn prewarm_multicall_v2_slots_count(&self) -> u64 {
        self.prewarm_multicall_v2_slots_total.get()
    }

    /// Read the multicall fallback counter.
    pub fn prewarm_multicall_fallbacks_count(&self) -> u64 {
        self.prewarm_multicall_fallbacks_total.get()
    }

    /// Read the bytecode-cache hit counter. Public so tests can assert
    /// cache wiring without re-parsing Prometheus text.
    pub fn prewarm_bytecode_cache_hits_count(&self) -> u64 {
        self.prewarm_bytecode_cache_hits_total.get()
    }

    /// Read the bytecode-cache RPC-fetch counter.
    pub fn prewarm_bytecode_rpc_fetches_count(&self) -> u64 {
        self.prewarm_bytecode_rpc_fetches_total.get()
    }

    /// Read the V2 reserves WS-hit counter.
    pub fn prewarm_v2_reserves_cache_hits_count(&self) -> u64 {
        self.prewarm_v2_reserves_cache_hits_total.get()
    }

    /// Read the V2 reserves stale-rejection counter.
    pub fn prewarm_v2_reserves_cache_stale_count(&self) -> u64 {
        self.prewarm_v2_reserves_cache_stale_total.get()
    }

    /// Read the V2 reserves missing-from-cache counter.
    pub fn prewarm_v2_reserves_cache_missing_count(&self) -> u64 {
        self.prewarm_v2_reserves_cache_missing_total.get()
    }

    /// Observe a successful first-seen → inclusion latency in ms.
    /// Caller is the [`first_seen_tracker`] block-side hook; do not call
    /// directly from random sites — the tracker enforces the "stamp was
    /// real, delta is positive" invariants.
    pub fn observe_mempool_first_seen_to_inclusion_ms(&self, ms: f64) {
        self.mempool_first_seen_to_inclusion_ms.observe(ms);
    }

    /// Bump `aether_mempool_first_seen_events_total{event="..."}`. See the
    /// struct field doc for the stable label set.
    pub fn inc_mempool_first_seen_event(&self, event: &str) {
        self.mempool_first_seen_events_total
            .with_label_values(&[event])
            .inc();
    }

    /// Bump `aether_mempool_post_state_replay_total{outcome="..."}`. The
    /// caller picks from the pre-touched label set documented on the
    /// field — any other string still records but breaks dashboards.
    pub fn inc_mempool_post_state_replay(&self, outcome: &str) {
        self.mempool_post_state_replay_total
            .with_label_values(&[outcome])
            .inc();
    }

    /// Read the current value of
    /// `aether_mempool_post_state_replay_total{outcome}`. Public so tests
    /// can assert replay outcomes without re-implementing Prometheus
    /// text parsing.
    pub fn mempool_post_state_replay_count(&self, outcome: &str) -> u64 {
        self.mempool_post_state_replay_total
            .with_label_values(&[outcome])
            .get()
    }

    /// Observe one replay's wall-clock latency in milliseconds.
    pub fn observe_mempool_post_state_replay_latency_ms(&self, ms: f64) {
        self.mempool_post_state_replay_latency_ms.observe(ms);
    }

    pub fn observe_detection_latency_us(&self, us: u128) {
        let ms = us as f64 / 1000.0;
        self.detection_latency_ms.observe(ms);
    }

    pub fn observe_simulation_latency_us(&self, us: u128) {
        let ms = us as f64 / 1000.0;
        self.simulation_latency_ms.observe(ms);
    }

    pub fn inc_cycles_detected(&self, count: u64) {
        if count > 0 {
            self.cycles_detected.inc_by(count);
        }
    }

    pub fn inc_simulations_run(&self, count: u64) {
        if count > 0 {
            self.simulations_run.inc_by(count);
        }
    }

    pub fn inc_arbs_published(&self, count: u64) {
        if count > 0 {
            self.arbs_published.inc_by(count);
        }
    }

    pub fn inc_blocks_processed(&self) {
        self.blocks_processed.inc();
    }

    /// Bump `aether_decode_errors_total{reason="..."}` for the given reason.
    /// Labels come from `DecodeReason::as_str()` so the label set stays
    /// stable and enumerable for dashboards / alerts.
    pub fn inc_decode_errors(&self, reason: &str) {
        self.decode_errors.with_label_values(&[reason]).inc();
    }

    /// Bump `aether_sim_evm_fallback_total{reason="..."}` for an EVM
    /// fork-replay fallback escalated from one of the analytical
    /// post-state predictors. See the field doc on
    /// `sim_evm_fallback_total` for the stable label set; passing a
    /// reason outside that set still records but breaks dashboards, so
    /// callers MUST pick from the documented enum.
    pub fn inc_sim_evm_fallback(&self, reason: &str) {
        self.sim_evm_fallback_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Read the current value of `aether_sim_evm_fallback_total{reason}`.
    /// `pub` so tests can assert fallback rates without re-implementing
    /// Prometheus text parsing.
    pub fn sim_evm_fallback_count(&self, reason: &str) -> u64 {
        self.sim_evm_fallback_total
            .with_label_values(&[reason])
            .get()
    }

    /// Bump `aether_cycle_gate_dropped_total{reason="..."}` for a cycle
    /// rejected by the candidate gating layer. See the field doc on
    /// `cycle_gate_dropped_total` for the stable label set; callers must
    /// pick from the documented reasons so dashboards stay enumerable.
    pub fn inc_cycle_gate_dropped(&self, reason: &str) {
        self.cycle_gate_dropped_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Read the current value of `aether_cycle_gate_dropped_total{reason}`.
    /// `pub` so tests can assert gating rates without re-implementing
    /// Prometheus text parsing.
    pub fn cycle_gate_dropped_count(&self, reason: &str) -> u64 {
        self.cycle_gate_dropped_total
            .with_label_values(&[reason])
            .get()
    }

    /// Borrow the underlying `Registry` so foreign metric families (e.g. the
    /// trade-ledger counters in `aether_common::db`) can register on the same
    /// scrape endpoint without standing up a second `/metrics` server.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Bump `aether_pending_dex_tx_total{router, protocol, decoded}` for a
    /// pending DEX-router tx the mempool source forwarded. `protocol` is
    /// `unknown` when decoding failed; `decoded` is `"true"` or `"false"`.
    pub fn inc_pending_dex_tx(&self, router: &str, protocol: &str, decoded: bool) {
        self.pending_dex_tx_total
            .with_label_values(&[router, protocol, if decoded { "true" } else { "false" }])
            .inc();
    }

    /// Bump `aether_pending_decode_errors_total{reason="..."}`. Reasons
    /// should be a small fixed set (`too_short`, `unknown_selector`,
    /// `abi_decode`, `empty_path`) so dashboards can rely on stable labels.
    pub fn inc_pending_decode_errors(&self, reason: &str) {
        self.pending_decode_errors_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Bump `aether_pending_arb_candidates_total{router, profit_bucket}`.
    /// Buckets are coarse (`<10bps`, `10-50bps`, `50-200bps`, `>200bps`) so
    /// the cardinality stays bounded.
    pub fn inc_pending_arb_candidates(&self, router: &str, profit_bucket: &str) {
        self.pending_arb_candidates_total
            .with_label_values(&[router, profit_bucket])
            .inc();
    }

    /// Bump `aether_pending_arb_sim_skipped_total{reason="..."}`.
    pub fn inc_pending_arb_sim_skipped(&self, reason: &str) {
        self.pending_arb_sim_skipped_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Read the current value of `aether_pending_arb_sim_skipped_total{reason}`.
    /// Used by the mempool-pipeline tests to assert that new skip reasons
    /// (e.g. `bancor_second_pool_not_found`, `bancor_multihop_low_confidence`)
    /// fire on the expected paths without re-implementing Prometheus text
    /// parsing.
    pub fn pending_arb_sim_skipped_count(&self, reason: &str) -> u64 {
        self.pending_arb_sim_skipped_total
            .with_label_values(&[reason])
            .get()
    }

    /// Add `n` to `aether_pending_pipeline_lagged_total`. Pass the count
    /// returned by `broadcast::error::RecvError::Lagged(n)` so the metric
    /// reflects events dropped, not lag events fired.
    pub fn add_pending_pipeline_lagged(&self, n: u64) {
        if n > 0 {
            self.pending_pipeline_lagged_total.inc_by(n);
        }
    }

    /// Bump `aether_mempool_filtered_total{reason="..."}` for a decoded
    /// pending swap the pipeline rejects before any sim work is scheduled.
    /// See the field doc on `mempool_filtered_total` for the stable label
    /// set.
    pub fn inc_mempool_filtered(&self, reason: &str) {
        self.mempool_filtered_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Read the current value of `aether_mempool_filtered_total{reason}`.
    /// Public so tests in the `aether-rust` bin (a separate crate from the
    /// `aether-grpc-server` lib) can assert filter behaviour without
    /// re-implementing Prometheus text parsing.
    pub fn mempool_filtered_count(&self, reason: &str) -> u64 {
        self.mempool_filtered_total
            .with_label_values(&[reason])
            .get()
    }

    /// Read the current value of
    /// `aether_pending_dex_tx_total{router, protocol, decoded}`. Used by
    /// the 1inch multi-record dispatch tests to assert one bump per
    /// peeled pool. `decoded` is `true` / `false` matching
    /// [`Self::inc_pending_dex_tx`].
    pub fn pending_dex_tx_count(&self, router: &str, protocol: &str, decoded: bool) -> u64 {
        self.pending_dex_tx_total
            .with_label_values(&[router, protocol, if decoded { "true" } else { "false" }])
            .get()
    }

    /// Observe one mempool-backrun validation latency sample. `result` is
    /// `"accept"` for a fully-successful sim (victim + arb both committed
    /// with positive net profit) or `"reject"` for every other outcome.
    pub fn observe_mempool_backrun_validation_latency_ms(&self, result: &str, ms: f64) {
        self.mempool_backrun_validation_latency_ms
            .with_label_values(&[result])
            .observe(ms);
    }

    /// Bump `aether_mempool_backrun_validated_total{profit_bucket}`. Buckets
    /// reuse the existing `pending_arb_candidates_total` cardinality so a
    /// single Grafana query joins the analytical-candidate funnel with the
    /// revm-validated funnel.
    pub fn inc_mempool_backrun_validated(&self, profit_bucket: &str) {
        self.mempool_backrun_validated_total
            .with_label_values(&[profit_bucket])
            .inc();
    }

    /// Bump `aether_mempool_backrun_rejected_total{reason}`. See the field
    /// doc on `mempool_backrun_rejected_total` for the stable label set.
    pub fn inc_mempool_backrun_rejected(&self, reason: &str) {
        self.mempool_backrun_rejected_total
            .with_label_values(&[reason])
            .inc();
    }

    /// Bump `aether_mempool_prewarm_inject_total{result="hit"}`.
    pub fn inc_mempool_prewarm_hit(&self) {
        self.mempool_prewarm_inject_total
            .with_label_values(&["hit"])
            .inc();
    }

    /// Bump `aether_mempool_prewarm_inject_total{result="miss"}`.
    pub fn inc_mempool_prewarm_miss(&self) {
        self.mempool_prewarm_inject_total
            .with_label_values(&["miss"])
            .inc();
    }

    /// Bump `aether_mempool_prewarm_refresh_total{result}`.
    pub fn inc_mempool_prewarm_refresh(&self, result: &str) {
        self.mempool_prewarm_refresh_total
            .with_label_values(&[result])
            .inc();
    }

    /// Observe wall-clock latency of one mempool pre-warm refresh cycle, ms.
    pub fn observe_mempool_prewarm_refresh_duration_ms(&self, ms: f64) {
        self.mempool_prewarm_refresh_duration_ms.observe(ms);
    }

    /// Set `aether_mempool_prewarm_warm_pools` to the most recent snapshot's
    /// pool count.
    pub fn set_mempool_prewarm_warm_pools(&self, n: i64) {
        self.mempool_prewarm_warm_pools.set(n);
    }

    /// Increment the in-flight mempool-backrun sim gauge by one. Returns a
    /// guard that decrements on drop so callers can wrap a sim in a
    /// `let _g = metrics.track_mempool_backrun_sim();` block and the
    /// counter is always balanced even on panic.
    pub fn track_mempool_backrun_sim(self: &Arc<Self>) -> MempoolBackrunSimGuard {
        self.mempool_backrun_sim_concurrent.inc();
        MempoolBackrunSimGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Read the current in-flight sim count. Public so tests can assert the
    /// gauge is balanced after a validation completes.
    pub fn mempool_backrun_sim_concurrent(&self) -> i64 {
        self.mempool_backrun_sim_concurrent.get()
    }

    /// Read the validated-candidate counter. Public so tests can assert
    /// the validator publishes on accept.
    pub fn mempool_backrun_validated_count(&self, profit_bucket: &str) -> u64 {
        self.mempool_backrun_validated_total
            .with_label_values(&[profit_bucket])
            .get()
    }

    /// Read the rejected-candidate counter for a specific reason.
    pub fn mempool_backrun_rejected_count(&self, reason: &str) -> u64 {
        self.mempool_backrun_rejected_total
            .with_label_values(&[reason])
            .get()
    }

    /// Render the registered metrics in Prometheus text exposition format.
    /// `pub(crate)` so sibling modules (`provider::tests`) can assert on
    /// rendered counter values without exposing the whole registry.
    pub(crate) fn render(&self) -> Vec<u8> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        if encoder.encode(&metric_families, &mut buffer).is_err() {
            return b"".to_vec();
        }
        buffer
    }
}

impl Default for EngineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that decrements `aether_mempool_backrun_sim_concurrent` on
/// drop. Returned by [`EngineMetrics::track_mempool_backrun_sim`]; the
/// caller binds it for the duration of a validation attempt and the gauge
/// is balanced even if the sim panics or returns early via `?`.
pub struct MempoolBackrunSimGuard {
    metrics: Arc<EngineMetrics>,
}

impl Drop for MempoolBackrunSimGuard {
    fn drop(&mut self) {
        self.metrics.mempool_backrun_sim_concurrent.dec();
    }
}

pub fn start_metrics_server(metrics: Arc<EngineMetrics>) {
    let addr = metrics_addr();

    tokio::spawn(async move {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "Metrics server listening");
                loop {
                    match listener.accept().await {
                        Ok((mut socket, _)) => {
                            let metrics = Arc::clone(&metrics);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(&mut socket, metrics).await {
                                    warn!(error = %e, "Metrics connection error");
                                }
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, "Metrics accept failed");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to bind metrics server");
            }
        }
    });
}

async fn handle_connection(
    socket: &mut tokio::net::TcpStream,
    metrics: Arc<EngineMetrics>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
        Ok(result) => result?,
        Err(_) => return Ok(()),
    };
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    if path != "/metrics" {
        let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
        socket.write_all(response.as_bytes()).await?;
        return Ok(());
    }

    let body = metrics.render();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(&body).await?;
    Ok(())
}

fn metrics_addr() -> SocketAddr {
    if let Ok(addr) = std::env::var("RUST_METRICS_ADDR") {
        if let Ok(parsed) = addr.parse() {
            return parsed;
        }
    }

    let port = std::env::var("RUST_METRICS_PORT").unwrap_or_else(|_| "9092".to_string());
    format!("0.0.0.0:{port}")
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:9092".parse().expect("default metrics addr"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_render_contains_required_names() {
        let metrics = EngineMetrics::new();

        metrics.observe_detection_latency_us(3000); // 3ms
        metrics.observe_simulation_latency_us(5000); // 5ms
        metrics.inc_cycles_detected(2);
        metrics.inc_simulations_run(3);
        metrics.inc_arbs_published(4);
        metrics.inc_blocks_processed();
        metrics.inc_decode_errors("unknown_topic");
        metrics.inc_decode_errors("malformed_payload");
        metrics.inc_decode_errors("insufficient_topics");

        let output = String::from_utf8(metrics.render()).expect("metrics output utf-8");

        for name in [
            "aether_detection_latency_ms",
            "aether_simulation_latency_ms",
            "aether_cycles_detected_total",
            "aether_simulations_run_total",
            "aether_arbs_published_total",
            "aether_blocks_processed_total",
            "aether_decode_errors_total",
        ] {
            assert!(output.contains(name), "missing metric {name}");
        }

        // Histogram emits _count and _sum
        assert!(output.contains("aether_detection_latency_ms_count 1"));
        assert!(output.contains("aether_detection_latency_ms_sum 3"));
        assert!(output.contains("aether_simulation_latency_ms_count 1"));
        assert!(output.contains("aether_simulation_latency_ms_sum 5"));
        assert!(output.contains("aether_cycles_detected_total 2"));
        assert!(output.contains("aether_simulations_run_total 3"));
        assert!(output.contains("aether_arbs_published_total 4"));
        assert!(output.contains("aether_blocks_processed_total 1"));
        assert!(output.contains(r#"aether_decode_errors_total{reason="unknown_topic"} 1"#));
        assert!(output.contains(r#"aether_decode_errors_total{reason="malformed_payload"} 1"#));
        assert!(output.contains(r#"aether_decode_errors_total{reason="insufficient_topics"} 1"#));
    }

    /// Apply a synthetic `PrewarmStats` payload and assert each of the
    /// cache + multicall counters records the right total. Mirrors the path
    /// the engine and mempool refresher take after `prewarm_state` returns.
    #[test]
    fn record_prewarm_stats_bumps_all_counters() {
        use aether_simulator::fork::PrewarmStats;
        let metrics = EngineMetrics::new();

        metrics.record_prewarm_stats(PrewarmStats {
            bytecode_cache_hits: 42,
            bytecode_rpc_fetches: 7,
            v2_reserves_cache_hits: 100,
            v2_reserves_cache_stale: 3,
            v2_reserves_cache_missing: 11,
            multicall_batches: 5,
            multicall_v2_slots: 60,
            multicall_fallbacks: 1,
        });
        // Second call must accumulate, not overwrite — counters are deltas.
        metrics.record_prewarm_stats(PrewarmStats {
            bytecode_cache_hits: 1,
            bytecode_rpc_fetches: 1,
            v2_reserves_cache_hits: 1,
            v2_reserves_cache_stale: 1,
            v2_reserves_cache_missing: 1,
            multicall_batches: 2,
            multicall_v2_slots: 13,
            multicall_fallbacks: 0,
        });

        assert_eq!(metrics.prewarm_bytecode_cache_hits_count(), 43);
        assert_eq!(metrics.prewarm_bytecode_rpc_fetches_count(), 8);
        assert_eq!(metrics.prewarm_v2_reserves_cache_hits_count(), 101);
        assert_eq!(metrics.prewarm_v2_reserves_cache_stale_count(), 4);
        assert_eq!(metrics.prewarm_v2_reserves_cache_missing_count(), 12);
        assert_eq!(metrics.prewarm_multicall_batches_count(), 7);
        assert_eq!(metrics.prewarm_multicall_v2_slots_count(), 73);
        assert_eq!(metrics.prewarm_multicall_fallbacks_count(), 1);

        let output = String::from_utf8(metrics.render()).expect("metrics output utf-8");
        for name in [
            "aether_prewarm_bytecode_cache_hits_total 43",
            "aether_prewarm_bytecode_rpc_fetches_total 8",
            "aether_prewarm_v2_reserves_cache_hits_total 101",
            "aether_prewarm_v2_reserves_cache_stale_total 4",
            "aether_prewarm_v2_reserves_cache_missing_total 12",
            "aether_prewarm_multicall_batches_total 7",
            "aether_prewarm_multicall_v2_slots_total 73",
            "aether_prewarm_multicall_fallbacks_total 1",
        ] {
            assert!(output.contains(name), "missing or wrong: {name}");
        }
    }
}
