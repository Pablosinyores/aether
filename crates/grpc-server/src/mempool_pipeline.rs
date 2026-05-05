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

use aether_common::types::ProtocolType;
use aether_detector::bellman_ford::BellmanFord;
use aether_ingestion::subscription::{EventChannels, PendingTxEvent};
use aether_pools::router_decoder::{decode_pending, DecodeError, DecodedSwap, Protocol};
use aether_state::snapshot::SnapshotManager;
use aether_state::token_index::TokenIndex;
use alloy::primitives::{Address, U256};
use arc_swap::ArcSwap;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::engine::PoolMetadata;
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

/// State the post-state simulator needs to run after a successful decode.
/// Cheap to clone (everything is `Arc`), so the pipeline holds one
/// `Arc<SimContext>` and dispatches per-event work without re-locking.
pub struct SimContext {
    pub pool_registry: Arc<ArcSwap<HashMap<Address, PoolMetadata>>>,
    pub token_index: Arc<ArcSwap<TokenIndex>>,
    pub snapshot_manager: Arc<SnapshotManager>,
    pub detector: BellmanFord,
    /// Cached `(registry_ptr, PairIndex)` so the second and following pending
    /// swaps under the same registry generation lookup in O(1). The Mutex
    /// guards rebuild only — the steady-state path is `lock + ptr_eq + read`.
    pair_index_cache: Mutex<Option<(usize, Arc<PairIndex>)>>,
}

impl SimContext {
    pub fn new(
        pool_registry: Arc<ArcSwap<HashMap<Address, PoolMetadata>>>,
        token_index: Arc<ArcSwap<TokenIndex>>,
        snapshot_manager: Arc<SnapshotManager>,
        detector: BellmanFord,
    ) -> Self {
        Self {
            pool_registry,
            token_index,
            snapshot_manager,
            detector,
            pair_index_cache: Mutex::new(None),
        }
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
            let stale = cache.as_ref().map_or(true, |(p, _)| *p != registry_ptr);
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
                    Ok(event) => handle_event(&metrics, sim_ctx.as_deref(), event),
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
/// the full pipeline task.
fn handle_event(metrics: &EngineMetrics, sim_ctx: Option<&SimContext>, event: PendingTxEvent) {
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
                try_post_state_scan(metrics, ctx, &router_label, &swap);
            }
        }
        Err(err) => emit_failure(metrics, &router_label, &err),
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
fn try_post_state_scan(
    metrics: &EngineMetrics,
    ctx: &SimContext,
    router_label: &str,
    swap: &DecodedSwap,
) {
    // V2/Sushi only — V3/Curve/Balancer need the revm-backed sim.
    let target_protocol = match swap.protocol {
        Protocol::UniswapV2 => ProtocolType::UniswapV2,
        Protocol::SushiSwap => ProtocolType::SushiSwap,
        Protocol::UniswapV3 | Protocol::BalancerV2 => {
            metrics.inc_pending_arb_sim_skipped("protocol_unsupported");
            return;
        }
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

    // Apply the V2 constant-product math to the in/out reserves. `dx` is
    // the victim's amountIn — bound to f64 via `u256_to_f64_saturating`
    // since the f64 mantissa is enough for token amount magnitudes seen
    // on-chain (up to ~2^53 ≈ 9e15 units of the smallest decimal).
    let dx = u256_to_f64_saturating(swap.amount_in);
    let (post_in, post_out) =
        predict_v2_post_state(edge_fwd.reserve_in, edge_fwd.reserve_out, dx, fee_factor);

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

    if profitable.is_empty() {
        metrics.inc_pending_arb_sim_skipped("no_profitable_cycle");
        return;
    }

    for cycle in &profitable {
        let bucket = profit_bucket(cycle.profit_factor());
        metrics.inc_pending_arb_candidates(router_label, bucket);
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
        let metrics = EngineMetrics::new();
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
        let metrics = EngineMetrics::new();
        let mut calldata = vec![0xde, 0xad, 0xbe, 0xef];
        calldata.extend(std::iter::repeat_n(0u8, 64));
        let to = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        handle_event(&metrics, None, pending_event(Some(to), calldata));
    }

    #[test]
    fn handle_event_no_to_does_not_panic() {
        let metrics = EngineMetrics::new();
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
}
