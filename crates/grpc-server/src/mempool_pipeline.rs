//! Pipeline that consumes pending-tx events from the mempool subscription
//! and runs them through the router calldata decoder.
//!
//! When a [`SimContext`] is provided, decoded UniswapV2 / SushiSwap swaps
//! are also fed into an analytical post-state simulator: the victim's
//! constant-product swap is applied to a clone of the live price graph,
//! and Bellman-Ford runs over the affected vertices to surface profitable
//! cycles. Profitable cycles are counted in
//! `aether_pending_arb_candidates_total{router, profit_bucket}`. Nothing
//! is submitted — this is a *candidate* metric that proves the post-state
//! pipeline produces non-empty output on real traffic.
//!
//! UniswapV3 / Curve / Balancer post-state math is not implemented here;
//! those decode paths still bump `pending_dex_tx_total` and are skipped
//! at the simulator layer with a `protocol_unsupported` reason. A revm-
//! backed simulator covering every protocol is the planned follow-up
//! ("Phase B" in the issue) and reuses this same pipeline shape.
//!
//! The pipeline runs only when [`aether_ingestion::mempool::is_enabled`]
//! returns `true` (i.e. `MEMPOOL_TRACKING=1` in the environment), so default
//! `main`-branch behaviour is unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aether_common::types::ProtocolType;
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::opportunity::DetectedCycle;
use aether_ingestion::subscription::{EventChannels, PendingTxEvent};
use aether_pools::router_decoder::{decode_pending, DecodeError, DecodedSwap, Protocol};
use aether_pools::{predict_post_state_with_fallback, PoolStateCache, UnifiedPostState};
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::mempool_backrun::{
    validate_backrun_rpc, ArbTx, RejectReason, ValidatorParams, VictimTx,
};
use aether_state::price_graph::PriceGraph;
use aether_state::snapshot::SnapshotManager;
use aether_state::token_index::TokenIndex;
use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::DynProvider;
use arc_swap::ArcSwap;
use chrono::Utc;
use tokio::sync::{broadcast, watch, Semaphore};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::service::aether_proto;

use crate::engine::PoolMetadata;
use crate::mempool_writer::{
    MempoolPredictionSink, NewMempoolPrediction, PredictedPostState, PROTOCOL_BALANCER,
    PROTOCOL_BANCOR, PROTOCOL_CURVE, PROTOCOL_SUSHI, PROTOCOL_UNI_V2, PROTOCOL_UNI_V3,
};
use crate::EngineMetrics;

/// Pair-keyed pool index built from the live pool registry. Lookup is O(1)
/// vs the previous registry.values().find(...) which was O(N) per pending
/// swap and would dominate the per-event budget at 5000+ pools.
///
/// The key uses the canonical ordering (`min(token0, token1), max(...)`) so
/// either swap direction returns the same bucket.
type PairKey = (Address, Address, ProtocolType);
type PairIndex = HashMap<PairKey, Vec<PoolMetadata>>;

fn canonical_pair(a: Address, b: Address) -> (Address, Address) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn build_pair_index(registry: &HashMap<Address, PoolMetadata>) -> PairIndex {
    let mut idx: PairIndex = HashMap::with_capacity(registry.len());
    for meta in registry.values() {
        let (a, b) = canonical_pair(meta.token0, meta.token1);
        idx.entry((a, b, meta.protocol))
            .or_default()
            .push(meta.clone());
    }
    idx
}

/// Configuration for the mempool-backrun revm validator, populated by
/// `main.rs` from env vars and embedded inside `SimContext` so the
/// per-event hot path doesn't re-parse env on every swap.
///
/// All values are read once at startup. The semaphore is shared across
/// all in-flight validation attempts so we never burn more than
/// `sim_concurrency` revm forks at the same time.
#[derive(Clone)]
pub struct BackrunValidatorConfig {
    pub executor_address: Address,
    pub searcher_caller: Address,
    pub profit_token: Address,
    pub balance_slot: U256,
    pub chain_id: u64,
    pub min_profit_wei: U256,
    pub input_amount_wei: U256,
    pub sim_semaphore: Arc<Semaphore>,
    /// RPC provider used by [`validate_backrun_rpc`] to build
    /// [`aether_simulator::fork::RpcForkedState`] per attempt. When
    /// `None` the validator stays dormant — the pipeline counts a
    /// `provider_unavailable` reject and the analytical-only behaviour
    /// from `develop` is preserved.
    pub provider: Option<DynProvider<Ethereum>>,
}

/// State the post-state simulator needs to run after a successful decode.
/// Cheap to clone (everything is `Arc`), so the pipeline holds one
/// `Arc<SimContext>` and dispatches per-event work without re-locking.
pub struct SimContext {
    pub pool_registry: Arc<ArcSwap<HashMap<Address, PoolMetadata>>>,
    pub token_index: Arc<ArcSwap<TokenIndex>>,
    pub snapshot_manager: Arc<SnapshotManager>,
    pub detector: BellmanFord,
    /// Live per-pool analytical state (V3 sqrt + tick + liquidity, Curve A +
    /// balances, Balancer balances + weights) populated by the engine at
    /// bootstrap and refreshed on `PoolEvent` updates. Used by the V3 /
    /// Balancer mempool sim path to call `predict_post_state_with_fallback`
    /// without round-tripping through the pool registry RPC.
    pub pool_states: PoolStateCache,
    /// Broadcast sender for `ProtoValidatedArb` — when the revm validator
    /// accepts a backrun candidate the pipeline publishes here. The
    /// existing block-driven path shares the same channel so the Go
    /// executor consumes both sources uniformly.
    pub arb_publisher: Option<broadcast::Sender<aether_proto::ValidatedArb>>,
    /// Validator configuration. `None` when env vars required for the
    /// revm validator are absent (e.g. dev runs without an executor
    /// address) — in that case the analytical-only path is preserved.
    pub backrun: Option<BackrunValidatorConfig>,
    /// Optional persistence sink for mempool predictions. `Arc<NoopMempoolSink>`
    /// when `MEMPOOL_LEDGER_DSN` is unset (no DB writes, no behaviour
    /// change); `Arc<PgMempoolWriter>` when set. Always present so the
    /// post-state path can call `insert_prediction` unconditionally.
    pub prediction_sink: Arc<dyn MempoolPredictionSink>,
    /// Engine build's git sha, copied onto every persisted prediction so
    /// the reconciler / scorer can correlate row outcomes with the engine
    /// version that produced them. `None` when the env var is unset.
    pub engine_git_sha: Option<String>,
    /// Cached `(registry_ptr, PairIndex)` so the second and following pending
    /// swaps under the same registry generation lookup in O(1). The Mutex
    /// guards rebuild only — the steady-state path is `lock + ptr_eq + read`.
    pair_index_cache: Mutex<Option<(usize, Arc<PairIndex>)>>,
}

impl SimContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool_registry: Arc<ArcSwap<HashMap<Address, PoolMetadata>>>,
        token_index: Arc<ArcSwap<TokenIndex>>,
        snapshot_manager: Arc<SnapshotManager>,
        detector: BellmanFord,
        pool_states: PoolStateCache,
        prediction_sink: Arc<dyn MempoolPredictionSink>,
        engine_git_sha: Option<String>,
    ) -> Self {
        Self {
            pool_registry,
            token_index,
            snapshot_manager,
            detector,
            pool_states,
            arb_publisher: None,
            backrun: None,
            prediction_sink,
            engine_git_sha,
            pair_index_cache: Mutex::new(None),
        }
    }

    /// Attach the validated-arb broadcast sender so the revm validator can
    /// publish accepted backruns. Calling this without also calling
    /// [`SimContext::with_backrun_validator`] leaves the publisher
    /// unreachable from the pipeline — both are required for the live
    /// `MEMPOOL_BACKRUN` path.
    pub fn with_arb_publisher(
        mut self,
        publisher: broadcast::Sender<aether_proto::ValidatedArb>,
    ) -> Self {
        self.arb_publisher = Some(publisher);
        self
    }

    /// Attach the revm validator configuration. The pipeline ignores this
    /// when [`SimContext::arb_publisher`] is also unset.
    pub fn with_backrun_validator(mut self, cfg: BackrunValidatorConfig) -> Self {
        self.backrun = Some(cfg);
        self
    }

    /// Look up a pool by `(token_in, token_out, protocol)` in O(1).
    ///
    /// Rebuilds the pair index when the underlying `pool_registry` Arc has
    /// been swapped (detected via pointer comparison). All lookups under a
    /// single registry generation share one Arc<PairIndex>.
    fn lookup_pool(
        &self,
        token_in: Address,
        token_out: Address,
        protocol: ProtocolType,
    ) -> Option<PoolMetadata> {
        let registry_guard = self.pool_registry.load();
        let registry_ptr = Arc::as_ptr(&registry_guard) as usize;

        let index = {
            let mut cache = self
                .pair_index_cache
                .lock()
                .expect("pair_index_cache poisoned");
            let stale = cache.as_ref().is_none_or(|(p, _)| *p != registry_ptr);
            if stale {
                let fresh = Arc::new(build_pair_index(&registry_guard));
                *cache = Some((registry_ptr, Arc::clone(&fresh)));
                fresh
            } else {
                Arc::clone(&cache.as_ref().expect("populated above").1)
            }
        };

        let (a, b) = canonical_pair(token_in, token_out);
        index.get(&(a, b, protocol))?.first().cloned()
    }
}

/// Spawn the mempool decode pipeline as a tokio task.
///
/// When `sim_ctx` is `Some`, decoded V2/Sushi swaps are run through the
/// analytical post-state simulator. When `None`, behaviour is identical
/// to the prior log-only version.
pub fn spawn_mempool_pipeline(
    channels: Arc<EventChannels>,
    metrics: Arc<EngineMetrics>,
    sim_ctx: Option<Arc<SimContext>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = channels.subscribe_pending_txs();
        info!(
            target: "aether::mempool",
            sim = sim_ctx.is_some(),
            "mempool decode pipeline started"
        );
        loop {
            tokio::select! {
                next = rx.recv() => match next {
                    Ok(event) => handle_event(&metrics, sim_ctx.as_ref(), event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        metrics.add_pending_pipeline_lagged(n);
                        warn!(
                            target: "aether::mempool",
                            lagged = n,
                            "decode pipeline lagged behind broadcast; events dropped"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(target: "aether::mempool", "broadcast closed; pipeline exiting");
                        return;
                    }
                },
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!(target: "aether::mempool", "shutdown signalled; pipeline exiting");
                        return;
                    }
                }
            }
        }
    })
}

/// Decode one pending tx and update metrics + logs.
///
/// Pulled out as a free function so unit tests can drive it without spawning
/// the full pipeline task. The post-state scan (graph clone + Bellman-Ford)
/// is dispatched onto tokio's blocking pool to keep its CPU cost off the
/// main runtime workers — the engine's 3 ms p99 detection budget cannot
/// share worker threads with a 3.8 MB-per-event clone under load.
fn handle_event(
    metrics: &Arc<EngineMetrics>,
    sim_ctx: Option<&Arc<SimContext>>,
    event: PendingTxEvent,
) {
    let Some(to) = event.to else {
        // Contract creations and other anonymous calls don't have a router
        // to attribute to — bump a generic `no_to` failure and move on.
        metrics.inc_pending_decode_errors("no_to");
        return;
    };
    let router_label = format!("{:#x}", to);

    match decode_pending(to, &event.input) {
        Ok(swap) => {
            emit_decoded(metrics, &router_label, &swap, &event);
            if let Some(ctx) = sim_ctx {
                if !pre_sim_filter(metrics, ctx, &swap) {
                    return;
                }
                let metrics = Arc::clone(metrics);
                let ctx = Arc::clone(ctx);
                let swap = swap.clone();
                let router_label = router_label.clone();
                let event = event.clone();
                let tx_hash = event.tx_hash;
                tokio::task::spawn_blocking(move || {
                    try_post_state_scan(&metrics, &ctx, &router_label, &swap, tx_hash, to, &event);
                });
            }
        }
        Err(err) => emit_failure(metrics, &router_label, &err),
    }
}

/// Drop a decoded swap before any sim work is scheduled when it would land
/// nowhere useful: self-swap, zero amount, or a (token, token, protocol)
/// triple absent from the live `pool_registry`. Bumps
/// `aether_mempool_filtered_total{reason}` and returns `false` on drop;
/// returns `true` to pass the swap through to the sim task.
///
/// The pool-registry check is the load-bearing one — without it every
/// shitcoin V2 swap on mainnet would queue a `spawn_blocking` that clones
/// the live graph (~3.8 MB) only to bump
/// `pending_arb_sim_skipped{token_in_unknown}` and discard the work.
fn pre_sim_filter(metrics: &EngineMetrics, ctx: &SimContext, swap: &DecodedSwap) -> bool {
    if swap.token_in == swap.token_out {
        metrics.inc_mempool_filtered("same_token");
        info!(
            target: "aether::mempool",
            reason = "same_token",
            protocol = ?swap.protocol,
            token = %swap.token_in,
            "FILTER DROP"
        );
        return false;
    }
    if swap.amount_in.is_zero() {
        metrics.inc_mempool_filtered("zero_amount");
        info!(
            target: "aether::mempool",
            reason = "zero_amount",
            protocol = ?swap.protocol,
            token_in = %swap.token_in,
            token_out = %swap.token_out,
            "FILTER DROP"
        );
        return false;
    }
    let target_protocol = match decoder_protocol_to_type(swap.protocol) {
        Some(p) => p,
        // Decoder-side protocols with no analytical predictor (none today —
        // all four decoded variants land here). Pass through so the sim
        // task can bump `pending_arb_sim_skipped{protocol_unsupported}`
        // without double-counting under `mempool_filtered_total`.
        None => return true,
    };
    if ctx
        .lookup_pool(swap.token_in, swap.token_out, target_protocol)
        .is_none()
    {
        metrics.inc_mempool_filtered("not_in_registry");
        info!(
            target: "aether::mempool",
            reason = "not_in_registry",
            protocol = ?swap.protocol,
            token_in = %swap.token_in,
            token_out = %swap.token_out,
            "FILTER DROP"
        );
        return false;
    }
    info!(
        target: "aether::mempool",
        protocol = ?swap.protocol,
        token_in = %swap.token_in,
        token_out = %swap.token_out,
        amount_in = %swap.amount_in,
        "FILTER PASS"
    );
    true
}

/// Map the router decoder's `Protocol` (a parser-side enum) to the workspace
/// `ProtocolType` used in the pool registry. Returns `None` for protocols
/// the post-state simulator doesn't yet handle so callers can route those
/// through the existing `protocol_unsupported` skip path instead of the
/// mempool filter.
fn decoder_protocol_to_type(p: Protocol) -> Option<ProtocolType> {
    match p {
        Protocol::UniswapV2 => Some(ProtocolType::UniswapV2),
        Protocol::SushiSwap => Some(ProtocolType::SushiSwap),
        Protocol::UniswapV3 => Some(ProtocolType::UniswapV3),
        Protocol::BalancerV2 => Some(ProtocolType::BalancerV2),
        Protocol::Curve => Some(ProtocolType::Curve),
        Protocol::BancorV3 => Some(ProtocolType::BancorV3),
    }
}

fn emit_decoded(
    metrics: &EngineMetrics,
    router_label: &str,
    swap: &DecodedSwap,
    event: &PendingTxEvent,
) {
    metrics.inc_pending_dex_tx(router_label, protocol_label(swap.protocol), true);
    debug!(
        target: "aether::mempool",
        tx_hash = %event.tx_hash,
        router = %router_label,
        protocol = ?swap.protocol,
        token_in = %swap.token_in,
        token_out = %swap.token_out,
        amount_in = %swap.amount_in,
        fee_bps = swap.fee_bps,
        "PENDING DEX SWAP decoded"
    );
}

fn emit_failure(metrics: &EngineMetrics, router_label: &str, err: &DecodeError) {
    let reason = decode_error_label(err);
    metrics.inc_pending_dex_tx(router_label, "unknown", false);
    metrics.inc_pending_decode_errors(reason);
    debug!(
        target: "aether::mempool",
        router = %router_label,
        reason,
        error = %err,
        "pending tx decode failed"
    );
}

/// Try to run the V2/Sushi post-state simulation for a decoded swap.
///
/// On any miss (unsupported protocol, missing pool, missing token index,
/// no graph edge, zero reserves) bumps
/// `aether_pending_arb_sim_skipped_total{reason}` and returns. On success,
/// every profitable cycle increments
/// `aether_pending_arb_candidates_total{router, profit_bucket}` and is
/// logged at `info` so a tail of the log is enough to verify the path.
#[allow(clippy::too_many_arguments)]
fn try_post_state_scan(
    metrics: &EngineMetrics,
    ctx: &SimContext,
    router_label: &str,
    swap: &DecodedSwap,
    event_tx_hash: alloy::primitives::B256,
    event_to: Address,
    event: &PendingTxEvent,
) {
    // Curve post-state scan is intentionally deferred to a follow-up PR
    // (the V3/Balancer branch downstream needs a parallel Curve arm that
    // routes through `unified_to_post_reserves`'s Curve variant + the
    // PredictedPostState writer). For now, log the decoded swap via
    // `emit_decoded` upstream so `pending_dex_tx_total{protocol="curve"}`
    // climbs, then bail out with a dedicated skip reason. The decoder is
    // the unblock that matters here; turning that signal into a cycle
    // candidate is the next increment.
    if swap.protocol == Protocol::Curve {
        metrics.inc_pending_arb_sim_skipped("curve_post_state_pending");
        return;
    }
    // Bancor V3 post-state scan is intentionally deferred to a follow-up
    // PR. The BancorPool predictor lives in `aether-pools` (BNT-
    // intermediary swap math) but lacks an analytical post-state hook
    // through `predict_post_state_with_fallback`. Decoded swaps already
    // surface on `pending_dex_tx_total{protocol="bancor_v3"}` via
    // `emit_decoded` upstream; the cycle-scan integration is the next
    // increment. Same scope-cut as the Curve decoder PR.
    if swap.protocol == Protocol::BancorV3 {
        metrics.inc_pending_arb_sim_skipped("bancor_post_state_pending");
        return;
    }

    let target_protocol = match swap.protocol {
        Protocol::UniswapV2 => ProtocolType::UniswapV2,
        Protocol::SushiSwap => ProtocolType::SushiSwap,
        Protocol::UniswapV3 => ProtocolType::UniswapV3,
        Protocol::BalancerV2 => ProtocolType::BalancerV2,
        Protocol::Curve => unreachable!("curve early-returned above"),
        Protocol::BancorV3 => unreachable!("bancor early-returned above"),
    };

    let token_idx = ctx.token_index.load();
    let Some(in_idx) = token_idx.get_index(&swap.token_in) else {
        metrics.inc_pending_arb_sim_skipped("token_in_unknown");
        return;
    };
    let Some(out_idx) = token_idx.get_index(&swap.token_out) else {
        metrics.inc_pending_arb_sim_skipped("token_out_unknown");
        return;
    };

    // O(1) pair lookup via the cached PairIndex. The cache rebuilds only
    // when the underlying pool_registry Arc has been swapped, so steady-state
    // cost is one Mutex acquire + one HashMap probe — independent of the
    // number of registered pools.
    let Some(meta) = ctx.lookup_pool(swap.token_in, swap.token_out, target_protocol) else {
        metrics.inc_pending_arb_sim_skipped("pool_not_registered");
        return;
    };
    let pool_id = meta.pool_id;
    let fee_factor = meta.fee_factor();

    // Snapshot the live graph and find the edge for this swap direction so
    // we can read the current reserves. The reverse edge is updated in the
    // same `update_edge_from_reserves` call against the cloned graph.
    let snapshot = ctx.snapshot_manager.load_full();
    let edge_fwd = snapshot
        .graph
        .edges_from(in_idx)
        .iter()
        .find(|e| e.to == out_idx && e.pool_id == pool_id)
        .cloned();
    let Some(edge_fwd) = edge_fwd else {
        metrics.inc_pending_arb_sim_skipped("graph_edge_missing");
        return;
    };
    if edge_fwd.reserve_in <= 0.0 || edge_fwd.reserve_out <= 0.0 {
        metrics.inc_pending_arb_sim_skipped("reserves_zero");
        return;
    }

    // Compute the post-state reserves the graph clone should adopt. V2 /
    // Sushi reuse the inline constant-product formula because the predictor
    // for those protocols intentionally lives outside `aether-pools`. V3 /
    // Balancer route through the analytical post-state predictor in
    // `aether-pools` and the result is mapped back onto graph-edge reserves
    // so Bellman-Ford treats the two protocol families identically.
    let (post_in, post_out) = match swap.protocol {
        Protocol::UniswapV2 | Protocol::SushiSwap => {
            // V2 constant-product: `dx` is the victim's amountIn — bound to
            // f64 via `u256_to_f64_saturating` since the f64 mantissa is
            // enough for token amount magnitudes seen on-chain
            // (up to ~2^53 ≈ 9e15 units of the smallest decimal).
            let dx = u256_to_f64_saturating(swap.amount_in);
            predict_v2_post_state(edge_fwd.reserve_in, edge_fwd.reserve_out, dx, fee_factor)
        }
        Protocol::UniswapV3 | Protocol::BalancerV2 => {
            let pool_addr = meta.pool_id.address;
            let Some(state_arc) = ctx.pool_states.get(&pool_addr).map(|r| Arc::clone(r.value()))
            else {
                metrics.inc_pending_arb_sim_skipped("pool_state_missing");
                return;
            };
            let post = predict_post_state_with_fallback(
                &state_arc,
                swap.token_in,
                swap.amount_in,
                |reason| metrics.inc_sim_evm_fallback(reason),
            );
            let Some(unified) = post else {
                metrics.inc_pending_arb_sim_skipped("predictor_low_confidence");
                return;
            };
            let (pin, pout) = unified_to_post_reserves(swap.token_in, &meta, &unified);
            if pin <= 0.0 || pout <= 0.0 {
                metrics.inc_pending_arb_sim_skipped("post_state_invalid");
                return;
            }
            (pin, pout)
        }
        Protocol::Curve => unreachable!("curve early-returned above"),
        Protocol::BancorV3 => unreachable!("bancor early-returned above"),
    };

    // Clone the graph and apply the post-state to both directions of the
    // affected pair. update_edge_from_reserves is idempotent for a given
    // (from, to, pool_id) tuple and is a no-op if the edge is missing.
    let mut graph = snapshot.graph.clone();
    graph.update_edge_from_reserves(in_idx, out_idx, pool_id, post_in, post_out, fee_factor);
    graph.update_edge_from_reserves(out_idx, in_idx, pool_id, post_out, post_in, fee_factor);

    let cycles = ctx
        .detector
        .detect_from_affected(&graph, &[in_idx, out_idx]);
    let profitable: Vec<_> = cycles.into_iter().filter(|c| c.is_profitable()).collect();

    // Persist the prediction unconditionally — both profitable and
    // unprofitable swaps are useful signal for the reconciler (issue #131
    // Go half), which needs the full population of decoded mempool swaps
    // to compute block / ordering / pool-path accuracy. The
    // `profit_factor_predicted` column is the SQL signal that the engine
    // would have considered acting on the swap.
    let post_state_json = match swap.protocol {
        Protocol::UniswapV2 | Protocol::SushiSwap => PredictedPostState::V2 {
            reserve_in: post_in,
            reserve_out: post_out,
        },
        Protocol::UniswapV3 => PredictedPostState::V3 {
            reserve_in: post_in,
            reserve_out: post_out,
        },
        Protocol::BalancerV2 => PredictedPostState::Balancer {
            reserve_in: post_in,
            reserve_out: post_out,
        },
        Protocol::Curve => unreachable!("curve early-returned above"),
        Protocol::BancorV3 => unreachable!("bancor early-returned above"),
    }
    .into_json();
    let prediction = NewMempoolPrediction {
        prediction_id: Uuid::new_v4(),
        decoded_at: Utc::now(),
        pending_tx_hash: event_tx_hash,
        router_address: event_to,
        protocol: decoder_protocol_label(swap.protocol),
        token_in: swap.token_in,
        token_out: swap.token_out,
        amount_in: swap.amount_in,
        pool_address: Some(meta.pool_id.address),
        predicted_target_block: snapshot.block_number.saturating_add(1),
        predicted_post_state: post_state_json,
        profit_factor_predicted: profitable.first().map(|c| c.profit_factor()),
        // Reserved for the MEV-Share SSE path; Alchemy WS pendings carry
        // no builder-side timestamp today.
        detection_lead_ms: None,
        engine_git_sha: ctx.engine_git_sha.clone(),
    };
    ctx.prediction_sink.insert_prediction(prediction);

    if profitable.is_empty() {
        metrics.inc_pending_arb_sim_skipped("no_profitable_cycle");
        return;
    }

    for cycle in &profitable {
        let bucket = profit_bucket(cycle.profit_factor());
        metrics.inc_pending_arb_candidates(router_label, bucket);
    }

    // Hand the best profitable cycle to the revm validator when the
    // pipeline has both a configured validator and a broadcast publisher.
    // The validator returns the per-attempt outcome via metrics; this
    // call site is intentionally fire-and-forget so the analytical
    // candidate metric remains the contract for the dashboard.
    if let (Some(publisher), Some(cfg)) = (ctx.arb_publisher.as_ref(), ctx.backrun.as_ref()) {
        if let Some(best) = profitable.first() {
            let _ = run_backrun_validation(
                metrics,
                publisher,
                cfg,
                &snapshot.graph,
                ctx.token_index.load().as_ref(),
                best,
                event,
                router_label,
                snapshot.block_number,
            );
        }
    }

    info!(
        target: "aether::mempool",
        router = %router_label,
        protocol = ?swap.protocol,
        pool = %meta.pool_id.address,
        token_in = %swap.token_in,
        token_out = %swap.token_out,
        candidates = profitable.len(),
        best_profit_bps = (profitable[0].profit_factor() * 10_000.0) as i64,
        "MEMPOOL ARB CANDIDATE"
    );
}

/// Orchestrate the revm validator for one profitable cycle and publish a
/// `ValidatedArb` on accept.
///
/// Acquires the global validation semaphore (drops the call when full),
/// converts the cycle to executor calldata via the existing
/// `aether_simulator::calldata` builder, builds an `RpcForkedState` at
/// the snapshot's block, runs `validate_backrun_rpc`, and either publishes
/// or counts the rejection reason on the `aether_mempool_backrun_*`
/// metrics.
#[allow(clippy::too_many_arguments)]
fn run_backrun_validation(
    metrics: &EngineMetrics,
    publisher: &broadcast::Sender<aether_proto::ValidatedArb>,
    cfg: &BackrunValidatorConfig,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    cycle: &DetectedCycle,
    event: &PendingTxEvent,
    router_label: &str,
    block_number: u64,
) -> Option<()> {
    // Bounded concurrency. `try_acquire_owned` so a saturated semaphore
    // drops the attempt rather than queueing — the next pending swap will
    // bring its own validation candidate within tens of ms.
    let _permit = match cfg.sim_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            metrics.inc_mempool_backrun_rejected("sim_concurrency_saturated");
            return None;
        }
    };
    // Provider check. Without an RPC connection the validator stays
    // dormant — the analytical-only path from develop is preserved. The
    // reject reason is distinct from `sim_error` so dashboards can show
    // dormant-validator runs separately from real failures.
    let Some(provider) = cfg.provider.as_ref() else {
        metrics.inc_mempool_backrun_rejected("provider_unavailable");
        return None;
    };
    let started = Instant::now();

    // Cycle → SwapStep conversion. Walks the cycle vertices and picks the
    // first matching edge for each hop. When any leg lacks a graph edge
    // (e.g. cycle crosses a vertex with no outbound edge under the
    // current snapshot) the attempt rejects rather than publishing a
    // malformed bundle.
    let steps = match cycle_to_swap_steps(graph, token_index, cycle, cfg.input_amount_wei) {
        Some(s) if !s.is_empty() => s,
        _ => {
            metrics.inc_mempool_backrun_rejected("cycle_unbuildable");
            return None;
        }
    };
    let flashloan_token = steps[0].token_in;

    // Deadline: now + 24s (~2 blocks of mainnet slot time). Mirrors the
    // block-driven path's deadline convention.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let deadline = U256::from(now_secs + 24);
    let calldata = build_execute_arb_calldata(
        &steps,
        flashloan_token,
        cfg.input_amount_wei,
        deadline,
        cfg.min_profit_wei,
        U256::from(9000u64), // 90% tip share — conservative starting point
    );

    // Build the forked state at the snapshot block. `new_at_latest`
    // sidesteps the BlockId resolution issue when the provider is an
    // Anvil fork; on real mainnet RPC the difference is invisible
    // because `latest` and the snapshot block are typically within one
    // slot of each other.
    let state = match aether_simulator::fork::RpcForkedState::new_at_latest(
        provider.clone(),
        block_number,
        now_secs,
        // Base fee unknown at the snapshot — pass 1 gwei and rely on
        // `disable_base_fee = true` inside the simulator's cfg. This is
        // safe because the only effect of base_fee here is the EIP-1559
        // gas-price check, which the validator already disables.
        1_000_000_000,
    ) {
        Some(s) => s,
        None => {
            metrics.inc_mempool_backrun_rejected("fork_construction_failed");
            return None;
        }
    };

    let victim = VictimTx {
        from: event.from,
        to: event.to?,
        value: event.value,
        data: event.input.clone(),
        gas_price: event.gas_price,
        gas_limit: 1_000_000, // generous; victim's actual gas_limit not on the subscription stream
    };
    let arb = ArbTx {
        caller: cfg.searcher_caller,
        to: cfg.executor_address,
        data: calldata,
        gas_limit: 1_500_000,
    };
    let params = ValidatorParams {
        block_number,
        block_timestamp: now_secs,
        base_fee: 1_000_000_000,
        chain_id: cfg.chain_id,
        profit_token: cfg.profit_token,
        profit_recipient: cfg.searcher_caller,
        balance_slot: cfg.balance_slot,
    };

    let result = validate_backrun_rpc(state, &victim, &arb, &params);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;

    if !result.accepted {
        let reason = result
            .reject
            .as_ref()
            .map(|r| r.as_str())
            .unwrap_or(RejectReason::SimError.as_str());
        metrics.inc_mempool_backrun_rejected(reason);
        metrics.observe_mempool_backrun_validation_latency_ms("reject", elapsed_ms);
        debug!(
            target: "aether::mempool",
            tx_hash = %event.tx_hash,
            router = %router_label,
            reason,
            arb_gas_used = result.arb_gas_used,
            "BACKRUN VALIDATION REJECTED"
        );
        return None;
    }

    // Accept path — publish a ValidatedArb tagged MEMPOOL_BACKRUN.
    metrics.observe_mempool_backrun_validation_latency_ms("accept", elapsed_ms);
    let bucket = gross_profit_bucket(result.gross_profit_wei);
    metrics.inc_mempool_backrun_validated(bucket);

    let proto = aether_proto::ValidatedArb {
        id: format!(
            "mempool-{:#x}-{}",
            event.tx_hash,
            cycle
                .path
                .first()
                .copied()
                .unwrap_or(0)
        ),
        hops: vec![], // detailed hop info lives on the SwapStep list below
        total_profit_wei: u256_bytes(result.gross_profit_wei),
        total_gas: result.arb_gas_used,
        gas_cost_wei: u256_bytes(
            U256::from(result.arb_gas_used).saturating_mul(U256::from(params.base_fee)),
        ),
        net_profit_wei: u256_bytes(result.gross_profit_wei),
        block_number,
        timestamp_ns: now_secs as i64 * 1_000_000_000,
        flashloan_token: flashloan_token.to_vec().into(),
        flashloan_amount: u256_bytes(cfg.input_amount_wei),
        steps: steps.into_iter().map(swap_step_to_proto).collect(),
        calldata: arb.data.into(),
        source: aether_proto::ArbSource::MempoolBackrun as i32,
        victim_tx_hash: event.tx_hash.0.to_vec().into(),
        target_block: block_number.saturating_add(1),
    };

    if let Err(e) = publisher.send(proto) {
        debug!(
            target: "aether::mempool",
            error = %e,
            "BACKRUN VALIDATED — no arb subscribers connected"
        );
    } else {
        info!(
            target: "aether::mempool",
            tx_hash = %event.tx_hash,
            router = %router_label,
            arb_gas_used = result.arb_gas_used,
            gross_profit_wei = %result.gross_profit_wei,
            "BACKRUN VALIDATED — published to executor"
        );
    }
    Some(())
}

fn u256_bytes(v: U256) -> bytes::Bytes {
    let arr: [u8; 32] = v.to_be_bytes();
    bytes::Bytes::copy_from_slice(&arr)
}

fn swap_step_to_proto(s: aether_common::types::SwapStep) -> aether_proto::SwapStep {
    aether_proto::SwapStep {
        protocol: protocol_to_proto(s.protocol) as i32,
        pool_address: s.pool_address.to_vec().into(),
        token_in: s.token_in.to_vec().into(),
        token_out: s.token_out.to_vec().into(),
        amount_in: u256_bytes(s.amount_in),
        min_amount_out: u256_bytes(s.min_amount_out),
        calldata: s.calldata.into(),
    }
}

fn protocol_to_proto(p: ProtocolType) -> aether_proto::ProtocolType {
    match p {
        ProtocolType::UniswapV2 => aether_proto::ProtocolType::UniswapV2,
        ProtocolType::UniswapV3 => aether_proto::ProtocolType::UniswapV3,
        ProtocolType::SushiSwap => aether_proto::ProtocolType::Sushiswap,
        ProtocolType::Curve => aether_proto::ProtocolType::Curve,
        ProtocolType::BalancerV2 => aether_proto::ProtocolType::BalancerV2,
        ProtocolType::BancorV3 => aether_proto::ProtocolType::BancorV3,
    }
}

/// Map a gross profit value (wei) onto the same bucket cardinality used
/// by the analytical-candidate metric so dashboards can join the two
/// funnels in a single query.
fn gross_profit_bucket(profit_wei: U256) -> &'static str {
    let eth = u256_to_f64_saturating(profit_wei) / 1e18;
    // Compare against approximate USD ranges by mapping ETH → ~$3000:
    //   <0.001 ETH ≈ <$3      → lt_10bps   (vanishingly small)
    //   <0.01  ETH ≈ <$30     → 10_50bps   (sub-floor)
    //   <0.1   ETH ≈ <$300    → 50_200bps  (sensible)
    //   otherwise              → gt_200bps  (fat tail)
    if eth < 0.001 {
        "lt_10bps"
    } else if eth < 0.01 {
        "10_50bps"
    } else if eth < 0.1 {
        "50_200bps"
    } else {
        "gt_200bps"
    }
}

/// Convert a `DetectedCycle` (vertex-index path) into the `SwapStep`
/// sequence consumed by `build_execute_arb_calldata`. Picks the first
/// graph edge between each consecutive vertex pair. Returns `None` when
/// any token address or edge cannot be resolved against the current
/// snapshot.
fn cycle_to_swap_steps(
    graph: &PriceGraph,
    token_index: &TokenIndex,
    cycle: &DetectedCycle,
    input_amount_wei: U256,
) -> Option<Vec<aether_common::types::SwapStep>> {
    if cycle.path.len() < 2 {
        return None;
    }
    let mut steps = Vec::with_capacity(cycle.path.len() - 1);
    let mut current_amount = input_amount_wei;
    for window in cycle.path.windows(2) {
        let (from_idx, to_idx) = (window[0], window[1]);
        let from_addr = *token_index.get_address(from_idx)?;
        let to_addr = *token_index.get_address(to_idx)?;
        let edge = graph
            .edges_from(from_idx)
            .iter()
            .find(|e| e.to == to_idx)?;
        steps.push(aether_common::types::SwapStep {
            protocol: edge.protocol,
            pool_address: edge.pool_address,
            token_in: from_addr,
            token_out: to_addr,
            amount_in: current_amount,
            min_amount_out: U256::ZERO,
            calldata: Vec::new(),
        });
        // Approximate downstream amount via the edge's reserve ratio so
        // each hop carries a plausible amount_in. The executor contract
        // will overwrite intermediates from real on-chain reads anyway —
        // this is only used by sim path arithmetic that wants a non-zero
        // amount_in per step.
        if edge.reserve_in > 0.0 && edge.reserve_out > 0.0 {
            let ratio = edge.reserve_out / edge.reserve_in;
            let amount_f64 = u256_to_f64_saturating(current_amount) * ratio;
            current_amount = u256_from_f64_saturating(amount_f64);
        }
    }
    Some(steps)
}

/// Saturating f64 → U256. The inverse of `u256_to_f64_saturating`. Used
/// only for downstream hop amount approximation in `cycle_to_swap_steps`
/// — never for profit accounting.
fn u256_from_f64_saturating(v: f64) -> U256 {
    if !v.is_finite() || v <= 0.0 {
        return U256::ZERO;
    }
    if v >= u128::MAX as f64 {
        return U256::from(u128::MAX);
    }
    U256::from(v as u128)
}

/// Wire label for the `protocol` column on `mempool_predictions`. Pinned to
/// the strings declared in [`crate::mempool_writer`] so the writer and the
/// pipeline cannot drift. Matches issue #131's schema body.
fn decoder_protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::UniswapV2 => PROTOCOL_UNI_V2,
        Protocol::SushiSwap => PROTOCOL_SUSHI,
        Protocol::UniswapV3 => PROTOCOL_UNI_V3,
        Protocol::BalancerV2 => PROTOCOL_BALANCER,
        Protocol::Curve => PROTOCOL_CURVE,
        Protocol::BancorV3 => PROTOCOL_BANCOR,
    }
}

/// Map a V3 / Balancer post-state into the (post_in, post_out) reserves the
/// price graph stores per edge. Curve cannot reach here — the router
/// decoder rejects every Curve calldata shape with `CurveUnsupported`
/// before the pipeline sees it — but the variant is matched explicitly so
/// new protocol families fail the build instead of silently routing to
/// reserves of `(0.0, 0.0)`.
///
/// **V3 mapping.** The predictor returns `new_sqrt_price_x96`. The marginal
/// post-state spot price (token1 per token0) is `(sqrt / 2^96)^2`. The
/// graph's `update_edge_from_reserves` derives the edge weight as
/// `(reserve_out / reserve_in) * fee_factor`, so we set the synthetic pair
/// `(reserve_in, reserve_out) = (1.0, spot_price_post)` for the
/// `token0 → token1` direction and the inverse for the reverse direction.
/// `fee_factor` is applied at the graph layer, matching the bootstrap
/// path that originally seeded the V3 edge with `price * fee`.
///
/// **Balancer mapping.** For equal-weight 2-token pools the rate equals
/// `balance_out / balance_in` — directly usable as graph reserves with the
/// pool's `fee_factor`. The predictor only returns `analytical = true` for
/// the equal-weight case (unequal weights surface a low-confidence flag
/// and the call short-circuits to the EVM fallback metric).
fn unified_to_post_reserves(
    swap_token_in: Address,
    meta: &PoolMetadata,
    post: &UnifiedPostState,
) -> (f64, f64) {
    match post {
        UnifiedPostState::UniswapV3(v3) => {
            const TWO_POW_96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
            let sqrt_f = u256_to_f64_saturating(v3.new_sqrt_price_x96);
            let price_t1_per_t0 = (sqrt_f / TWO_POW_96).powi(2);
            if price_t1_per_t0 <= 0.0 {
                return (0.0, 0.0);
            }
            if swap_token_in == meta.token0 {
                (1.0, price_t1_per_t0)
            } else {
                (1.0, 1.0 / price_t1_per_t0)
            }
        }
        UnifiedPostState::Balancer(b) => {
            let b0 = u256_to_f64_saturating(b.new_balance0);
            let b1 = u256_to_f64_saturating(b.new_balance1);
            if swap_token_in == meta.token0 {
                (b0, b1)
            } else {
                (b1, b0)
            }
        }
        UnifiedPostState::Curve(_) => (0.0, 0.0),
    }
}

/// Predict V2 reserves after a swap of `dx` of `reserve_in` for `reserve_out`.
///
/// `fee_factor` is `(10_000 - fee_bps) / 10_000` (e.g. `0.997` for 30 bps).
/// Math: with effective input `dx_eff = dx * fee_factor`, the constant-
/// product invariant gives `dy = (dx_eff * y) / (x + dx_eff)`, then
/// `x' = x + dx`, `y' = y - dy`. Returns `(0.0, 0.0)` when inputs are
/// non-positive so callers can detect an invalid swap.
fn predict_v2_post_state(
    reserve_in: f64,
    reserve_out: f64,
    dx: f64,
    fee_factor: f64,
) -> (f64, f64) {
    if reserve_in <= 0.0 || reserve_out <= 0.0 || dx <= 0.0 || fee_factor <= 0.0 {
        return (0.0, 0.0);
    }
    let dx_eff = dx * fee_factor;
    let dy = (dx_eff * reserve_out) / (reserve_in + dx_eff);
    let post_in = reserve_in + dx;
    // dy is mathematically < reserve_out for any finite dx, but clamp to
    // a positive epsilon to defend against f64 catastrophic cancellation
    // on very large dx near reserve depletion.
    let post_out = (reserve_out - dy).max(1.0);
    (post_in, post_out)
}

/// Coarse profit bucket for the candidate metric. Bounded cardinality so
/// dashboards can sum across routers without label explosion.
fn profit_bucket(profit_factor: f64) -> &'static str {
    let bps = profit_factor * 10_000.0;
    if bps < 10.0 {
        "lt_10bps"
    } else if bps < 50.0 {
        "10_50bps"
    } else if bps < 200.0 {
        "50_200bps"
    } else {
        "gt_200bps"
    }
}

/// Saturating U256 → f64. The price graph already stores reserves as f64,
/// and Bellman-Ford runs in f64 weight space, so feeding the simulator a
/// f64 amount is consistent with the rest of the detection path.
///
/// **Precision contract.** f64 has a 53-bit mantissa (~9.0e15). Any U256
/// up to 2^53 - 1 round-trips losslessly. Above that the conversion
/// truncates lower bits — for an 18-decimal token this means amounts up to
/// ~9 million whole tokens are exact, and arbitrarily large amounts cap at
/// f64::MAX without panicking. Real on-chain swap sizes sit comfortably
/// below the lossless bound; the saturating return value protects against
/// adversarial calldata inflating dx beyond 2^256 → +inf in the math
/// kernel below.
fn u256_to_f64_saturating(v: U256) -> f64 {
    let limbs = v.as_limbs();
    let mut result = 0.0f64;
    let mut scale = 1.0f64;
    for limb in limbs.iter() {
        result += (*limb as f64) * scale;
        // 2^64 — multiplying out limbs in increasing significance.
        scale *= 18_446_744_073_709_551_616.0;
    }
    if result.is_finite() {
        result
    } else {
        f64::MAX
    }
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::UniswapV2 => "uniswap_v2",
        Protocol::UniswapV3 => "uniswap_v3",
        Protocol::SushiSwap => "sushiswap",
        Protocol::BalancerV2 => "balancer_v2",
        Protocol::Curve => "curve",
        Protocol::BancorV3 => "bancor_v3",
    }
}

fn decode_error_label(err: &DecodeError) -> &'static str {
    match err {
        DecodeError::TooShort => "too_short",
        DecodeError::UnknownSelector { .. } => "unknown_selector",
        DecodeError::AbiDecode(_) => "abi_decode",
        DecodeError::EmptyPath => "empty_path",
        DecodeError::CurveUnsupported(_) => "curve_unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_pools::router_decoder::IUniswapV2Router02::swapExactTokensForTokensCall;
    use alloy::primitives::{address, B256, U256};
    use alloy::sol_types::SolCall;

    fn pending_event(to: Option<alloy::primitives::Address>, input: Vec<u8>) -> PendingTxEvent {
        PendingTxEvent {
            tx_hash: B256::ZERO,
            from: alloy::primitives::Address::ZERO,
            to,
            value: U256::ZERO,
            input,
            gas_price: 0,
            first_seen_unix_nanos: 0,
        }
    }

    #[test]
    fn protocol_label_is_stable() {
        assert_eq!(protocol_label(Protocol::UniswapV2), "uniswap_v2");
        assert_eq!(protocol_label(Protocol::UniswapV3), "uniswap_v3");
        assert_eq!(protocol_label(Protocol::SushiSwap), "sushiswap");
        assert_eq!(protocol_label(Protocol::BalancerV2), "balancer_v2");
    }

    #[test]
    fn decode_error_label_covers_every_variant() {
        assert_eq!(decode_error_label(&DecodeError::TooShort), "too_short");
        assert_eq!(
            decode_error_label(&DecodeError::UnknownSelector { selector: [0; 4] }),
            "unknown_selector"
        );
        assert_eq!(
            decode_error_label(&DecodeError::AbiDecode("x".into())),
            "abi_decode"
        );
        assert_eq!(decode_error_label(&DecodeError::EmptyPath), "empty_path");
        assert_eq!(
            decode_error_label(&DecodeError::CurveUnsupported(alloy::primitives::Address::ZERO)),
            "curve_unsupported"
        );
    }

    #[test]
    fn handle_event_decoded_swap_does_not_panic() {
        let metrics = Arc::new(EngineMetrics::new());
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let calldata = swapExactTokensForTokensCall {
            amountIn: U256::from(1_000u64),
            amountOutMin: U256::from(900u64),
            path: vec![weth, usdc],
            to: alloy::primitives::Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let to = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        handle_event(&metrics, None, pending_event(Some(to), calldata));
    }

    #[test]
    fn handle_event_unknown_selector_does_not_panic() {
        let metrics = Arc::new(EngineMetrics::new());
        let mut calldata = vec![0xde, 0xad, 0xbe, 0xef];
        calldata.extend(std::iter::repeat_n(0u8, 64));
        let to = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        handle_event(&metrics, None, pending_event(Some(to), calldata));
    }

    #[test]
    fn handle_event_no_to_does_not_panic() {
        let metrics = Arc::new(EngineMetrics::new());
        handle_event(
            &metrics,
            None,
            pending_event(None, vec![0x12, 0x34, 0x56, 0x78]),
        );
    }

    // ----- predict_v2_post_state -----

    #[test]
    fn predict_v2_zero_inputs_return_zero() {
        assert_eq!(predict_v2_post_state(0.0, 1.0, 1.0, 0.997), (0.0, 0.0));
        assert_eq!(predict_v2_post_state(1.0, 0.0, 1.0, 0.997), (0.0, 0.0));
        assert_eq!(predict_v2_post_state(1.0, 1.0, 0.0, 0.997), (0.0, 0.0));
        assert_eq!(predict_v2_post_state(1.0, 1.0, 1.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn predict_v2_small_swap_matches_constant_product() {
        // x=1000, y=1000, dx=10, fee=0.3% -> dy = 10*0.997*1000/(1000+10*0.997)
        // dy ≈ 9.871
        let (post_in, post_out) = predict_v2_post_state(1000.0, 1000.0, 10.0, 0.997);
        assert!((post_in - 1010.0).abs() < 1e-9);
        let expected_dy = (10.0 * 0.997 * 1000.0) / (1000.0 + 10.0 * 0.997);
        assert!((post_out - (1000.0 - expected_dy)).abs() < 1e-9);
    }

    #[test]
    fn predict_v2_invariant_grows_by_fee() {
        // The k = x*y product increases by the fee accrual after a swap.
        let (post_in, post_out) = predict_v2_post_state(1000.0, 1000.0, 100.0, 0.997);
        let k_before = 1000.0 * 1000.0;
        let k_after = post_in * post_out;
        assert!(k_after > k_before, "fee should increase k");
    }

    // ----- profit_bucket -----

    #[test]
    fn profit_bucket_boundaries() {
        // 5 bps → < 10
        assert_eq!(profit_bucket(0.0005), "lt_10bps");
        // 25 bps
        assert_eq!(profit_bucket(0.0025), "10_50bps");
        // 100 bps
        assert_eq!(profit_bucket(0.0100), "50_200bps");
        // 500 bps
        assert_eq!(profit_bucket(0.0500), "gt_200bps");
        // exactly on boundary goes to upper bucket
        assert_eq!(profit_bucket(0.0010), "10_50bps");
        assert_eq!(profit_bucket(0.0050), "50_200bps");
        assert_eq!(profit_bucket(0.0200), "gt_200bps");
    }

    // ----- u256_to_f64_saturating -----

    #[test]
    fn u256_to_f64_small_value() {
        assert!((u256_to_f64_saturating(U256::from(1_000_000u64)) - 1_000_000.0).abs() < 1.0);
    }

    // ----- pre_sim_filter -----

    /// Build an empty SimContext suitable for tests that exercise the filter
    /// without needing real graph state — registry is empty, token index
    /// empty, snapshot has a zero-vertex graph. Any `lookup_pool` returns
    /// `None`, which is what the `not_in_registry` test wants anyway.
    fn empty_sim_ctx() -> Arc<SimContext> {
        use crate::mempool_writer::NoopMempoolSink;
        use aether_pools::new_pool_state_cache;
        use aether_state::price_graph::PriceGraph;
        Arc::new(SimContext::new(
            Arc::new(ArcSwap::from_pointee(HashMap::<Address, PoolMetadata>::new())),
            Arc::new(ArcSwap::from_pointee(TokenIndex::default())),
            Arc::new(SnapshotManager::new(PriceGraph::new(0))),
            BellmanFord::new(3, 1_000),
            new_pool_state_cache(),
            Arc::new(NoopMempoolSink::new()),
            None,
        ))
    }

    fn fake_swap(protocol: Protocol, token_in: Address, token_out: Address, amount_in: U256) -> DecodedSwap {
        DecodedSwap {
            protocol,
            router: Address::ZERO,
            token_in,
            token_out,
            amount_in,
            amount_out_min: U256::ZERO,
            recipient: Address::ZERO,
            fee_bps: 0,
            path_extra: vec![],
            curve_indices: None,
        }
    }

    fn filtered_count(metrics: &EngineMetrics, reason: &str) -> u64 {
        metrics.mempool_filtered_count(reason)
    }

    #[test]
    fn pre_sim_filter_drops_same_token_swaps() {
        let metrics = EngineMetrics::new();
        let ctx = empty_sim_ctx();
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let swap = fake_swap(Protocol::UniswapV2, weth, weth, U256::from(1u64));
        assert!(!pre_sim_filter(&metrics, &ctx, &swap));
        assert_eq!(filtered_count(&metrics, "same_token"), 1);
    }

    #[test]
    fn pre_sim_filter_drops_zero_amount() {
        let metrics = EngineMetrics::new();
        let ctx = empty_sim_ctx();
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let swap = fake_swap(Protocol::UniswapV2, weth, usdc, U256::ZERO);
        assert!(!pre_sim_filter(&metrics, &ctx, &swap));
        assert_eq!(filtered_count(&metrics, "zero_amount"), 1);
    }

    #[test]
    fn pre_sim_filter_drops_pair_absent_from_registry() {
        let metrics = EngineMetrics::new();
        let ctx = empty_sim_ctx();
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let swap = fake_swap(Protocol::UniswapV2, weth, usdc, U256::from(1_000u64));
        assert!(!pre_sim_filter(&metrics, &ctx, &swap));
        assert_eq!(filtered_count(&metrics, "not_in_registry"), 1);
    }

    #[test]
    fn pre_sim_filter_drops_v3_and_balancer_when_pair_not_registered() {
        // V3 / Balancer now route through the same registry-membership
        // check as V2 / Sushi: if the (token_in, token_out, protocol)
        // triple is absent from `pool_registry`, the filter drops the
        // swap under `not_in_registry` so the spawn_blocking + 3.8 MB
        // graph clone is never scheduled.
        let metrics = EngineMetrics::new();
        let ctx = empty_sim_ctx();
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        for proto in [Protocol::UniswapV3, Protocol::BalancerV2] {
            let swap = fake_swap(proto, weth, usdc, U256::from(1_000u64));
            assert!(!pre_sim_filter(&metrics, &ctx, &swap));
        }
        assert_eq!(filtered_count(&metrics, "not_in_registry"), 2);
        assert_eq!(filtered_count(&metrics, "same_token"), 0);
        assert_eq!(filtered_count(&metrics, "zero_amount"), 0);
    }

    #[test]
    fn decoder_protocol_to_type_maps_all_decoded_protocols() {
        assert_eq!(
            decoder_protocol_to_type(Protocol::UniswapV2),
            Some(ProtocolType::UniswapV2)
        );
        assert_eq!(
            decoder_protocol_to_type(Protocol::SushiSwap),
            Some(ProtocolType::SushiSwap)
        );
        assert_eq!(
            decoder_protocol_to_type(Protocol::UniswapV3),
            Some(ProtocolType::UniswapV3)
        );
        assert_eq!(
            decoder_protocol_to_type(Protocol::BalancerV2),
            Some(ProtocolType::BalancerV2)
        );
    }
}
