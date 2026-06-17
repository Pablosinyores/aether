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
use std::time::{Duration, Instant};

use aether_common::types::ProtocolType;
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas::{estimate_total_gas, gas_cost_wei};
use aether_detector::opportunity::DetectedCycle;
use aether_detector::optimizer::ternary_search_optimal_input;
use aether_ingestion::subscription::{EventChannels, PendingTxEvent};
use aether_pools::bancor::BNT_ADDRESS;
use aether_pools::uniswap_v2::UniswapV2Pool;
use aether_pools::router_decoder::{decode_pending_many, DecodeError, DecodedSwap, Protocol};
use aether_pools::{
    predict_post_state_with_replay, Pool, PoolState, PoolStateCache, ReplayProtocol,
    UnifiedPostState,
};
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::mempool_backrun::{
    validate_backrun_rpc, ArbTx, RejectReason, ValidatorParams, VictimTx,
};
use aether_simulator::post_state_replay::{
    replay_balancer_post_state_rpc, replay_curve_post_state_rpc, replay_v3_post_state_rpc,
    ReplayParams,
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
use aether_grpc_server::cycle_gating::{
    self, GatingConfig, PostSimGateVerdict, PreSimGateVerdict,
};
use aether_grpc_server::hot_token::{HotTokenConfig, HotTokenTracker};
use crate::mempool_writer::{
    MempoolPredictionSink, NewMempoolPrediction, PredictedPostState, PROTOCOL_BALANCER,
    PROTOCOL_BANCOR, PROTOCOL_CURVE, PROTOCOL_ONE_INCH_V6, PROTOCOL_SUSHI, PROTOCOL_UNI_V2, PROTOCOL_UNI_V3,
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

/// Aave V3 `flashLoanSimple` premium, in basis points (0.09%). Charged on
/// the borrowed (input) amount and repaid alongside principal inside
/// `AetherExecutor.executeOperation`, so it is a real cost the off-chain
/// sizing gate must subtract before declaring an arb profitable.
const AAVE_FLASHLOAN_PREMIUM_BPS: u128 = 9;

/// Slippage tolerance applied to each hop's optimizer-derived
/// `expected_out` to populate `min_amount_out`. 100 bps (1%) mirrors the
/// block-driven path's default `slippage_bps`.
const BACKRUN_SLIPPAGE_BPS: u32 = 100;

/// Lower bound of the input-sizing ternary search (0.001 ETH). The search
/// peak, not this floor, determines the chosen size; the floor only keeps
/// the optimizer away from dust inputs that always lose to fixed costs.
const OPTIMIZE_MIN_INPUT_WEI: u128 = 1_000_000_000_000_000;

/// Hard ceiling of the input-sizing ternary search (50 ETH). Matches the
/// risk layer's "max single trade 50 ETH" and the block path's `max_trade`.
/// The effective `max_input` is further clamped to half the shallowest hop
/// pool's input-side reserve (see [`optimize_cycle_input`]) so the optimizer
/// never sizes past the depth the (post-victim) pools can actually support.
const OPTIMIZE_MAX_INPUT_WEI: u128 = 50_000_000_000_000_000_000;

/// Number of ternary-search iterations. Matches the block-driven path; ~80
/// iterations converge a U256 range to within a couple of wei.
const OPTIMIZE_ITERATIONS: u32 = 80;

/// Gas limit used when replaying a victim tx in the shadow sim. The Alchemy
/// pending-tx subscription does not carry the victim's real gas_limit, and a
/// tight value (the old 1M) OOG-halted multicall / aggregator victims (1inch,
/// Universal Router) before their swap even executed — counted as
/// `victim_halted` and masking otherwise valid backruns. Use mainnet block-gas
/// headroom; the sim runs with `disable_balance_check` so the caller pays
/// nothing for it.
const MEMPOOL_VICTIM_GAS_LIMIT: u64 = 30_000_000;

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
    /// Mainnet gas price (gwei) used by the off-chain economic gate to
    /// price the arb's gas cost when sizing the input amount. The revm
    /// validator runs with `disable_base_fee`, so this value affects only
    /// the pre-revm profitability gate, not the simulated execution.
    pub gas_price_gwei: f64,
    pub sim_semaphore: Arc<Semaphore>,
    /// RPC provider used by [`validate_backrun_rpc`] to build
    /// [`aether_simulator::fork::RpcForkedState`] per attempt. When
    /// `None` the validator stays dormant — the pipeline counts a
    /// `provider_unavailable` reject and the analytical-only behaviour
    /// from `develop` is preserved.
    pub provider: Option<DynProvider<Ethereum>>,
    /// Shared handle to the long-lived pre-warmed bytecode + storage
    /// snapshot owned by the parent [`SimContext`]. Populated by
    /// [`SimContext::with_backrun_validator`]; remains a fresh empty
    /// `ArcSwap` when the validator is built outside a SimContext (only
    /// happens in tests and via `build_backrun_validator_config` before
    /// the SimContext attaches it). Atomic load on every shadow-sim.
    pub mempool_prewarm:
        Arc<ArcSwap<Option<Arc<aether_simulator::fork::PrewarmedState>>>>,
    /// Optional `AetherExecutor` runtime bytecode injected into the revm
    /// CacheDB at `executor_address` before each arb sim. Populated when
    /// running against a forked chain where the contract is not yet
    /// deployed (demo / shadow runs). `None` for production runs where the
    /// address resolves against on-chain bytecode.
    pub executor_bytecode: Option<alloy::primitives::Bytes>,
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
    /// Enable the revm-backed post-state replay fallback for V3 swaps
    /// the analytical predictor cannot settle (tick-crossing). When
    /// `false` (the default and the develop-branch behaviour) escalations
    /// keep bumping `aether_sim_evm_fallback_total` and skipping the
    /// candidate, preserving the current production semantics.
    ///
    /// Curve and Balancer escalations always skip until their reader
    /// hooks land in `aether_simulator::post_state_replay` — until then,
    /// flipping this flag only changes V3 behaviour.
    pub post_state_replay_enabled: bool,
    /// Long-lived snapshot of pre-fetched contract code + V2 reserve slots
    /// covering the tracked pool registry. Built and rotated by
    /// [`spawn_mempool_prewarm_refresher`]; injected into each per-pending-tx
    /// `RpcForkedState` so the mempool shadow-sim path stops re-fetching cold
    /// bytecode on every attempt. `None` until the first refresh lands.
    pub mempool_prewarm:
        Arc<ArcSwap<Option<Arc<aether_simulator::fork::PrewarmedState>>>>,
    /// Optional persistent bytecode cache. Threaded into `prewarm_state` so
    /// addresses already on disk skip the `eth_getCode` round-trip.
    pub bytecode_cache: Option<Arc<aether_simulator::bytecode_cache::BytecodeCache>>,
    /// WS-fed V2 reserves cache. Threaded into `prewarm_state` so pools
    /// whose latest `Sync` event arrived within
    /// `aether_simulator::fork::V2_RESERVES_MAX_LAG_BLOCKS` of the target
    /// block synthesise slot 8 locally instead of round-tripping
    /// `eth_getStorageAt`. `None` retains the pre-existing RPC-every-time
    /// behaviour.
    pub v2_reserves_cache: Option<aether_simulator::v2_reserves_cache::V2ReservesCache>,
    /// Time-decaying frequency counter over decoded mempool swap pairs. Every
    /// decoded swap is recorded here — including pairs whose pool is not yet in
    /// the registry (those are exactly the new-token admission candidates) —
    /// powering the periodic hot-token reporter. See the `hot_token` module.
    pub hot_tokens: Arc<HotTokenTracker>,
    /// Multi-signal candidate gating config. The block-driven path runs the
    /// same gates (`EngineConfig::gating`); the mempool path must apply them
    /// too — without this the mempool path would hand corrupt-edge cycles
    /// (collapsed `-ln(rate)` weights) straight to the revm validator. See
    /// [`crate::cycle_gating`].
    pub gating: GatingConfig,
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
            post_state_replay_enabled: false,
            mempool_prewarm: Arc::new(ArcSwap::from_pointee(None)),
            bytecode_cache: None,
            v2_reserves_cache: None,
            hot_tokens: Arc::new(HotTokenTracker::new(HotTokenConfig::default())),
            gating: GatingConfig::default(),
        }
    }

    /// Attach a persistent bytecode cache. Future `prewarm_state` calls that
    /// receive this `SimContext` will consult the cache before issuing
    /// `eth_getCode` and persist any freshly fetched bytecode back. Passing
    /// `None` is equivalent to the historical RPC-every-time behaviour.
    pub fn with_bytecode_cache(
        mut self,
        cache: Option<Arc<aether_simulator::bytecode_cache::BytecodeCache>>,
    ) -> Self {
        self.bytecode_cache = cache;
        self
    }

    /// Attach the engine's WS-fed V2 reserves cache. When set, the mempool
    /// pre-warm path consults the cache for slot 8 of every UniV2 / Sushi
    /// pool before issuing `eth_getStorageAt`.
    pub fn with_v2_reserves_cache(
        mut self,
        cache: Option<aether_simulator::v2_reserves_cache::V2ReservesCache>,
    ) -> Self {
        self.v2_reserves_cache = cache;
        self
    }

    /// Override the candidate gating config so the mempool path uses the
    /// same gates as the block-driven path (`EngineConfig::gating`). When
    /// unset, [`GatingConfig::default`] (the production-calibrated defaults)
    /// applies.
    pub fn with_gating(mut self, gating: GatingConfig) -> Self {
        self.gating = gating;
        self
    }

    /// Flip the revm post-state replay fallback on. Callers should also
    /// have a [`BackrunValidatorConfig`] attached with a populated
    /// `provider` — without one the V3 replay path stays dormant and
    /// every escalation counts as `unimplemented_protocol` because the
    /// provider check short-circuits before the EVM fork is built.
    pub fn with_post_state_replay(mut self, enabled: bool) -> Self {
        self.post_state_replay_enabled = enabled;
        self
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
    ///
    /// Shares the SimContext's `mempool_prewarm` handle into the cfg so the
    /// validator and the background refresher rotate the same `ArcSwap`.
    pub fn with_backrun_validator(mut self, mut cfg: BackrunValidatorConfig) -> Self {
        cfg.mempool_prewarm = Arc::clone(&self.mempool_prewarm);
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
        // Periodic hot-token visibility report (every 30s). First tick fires
        // immediately and no-ops while the table is empty.
        let mut report_tick = tokio::time::interval(Duration::from_secs(30));
        info!(
            target: "aether::mempool",
            sim = sim_ctx.is_some(),
            "mempool decode pipeline started"
        );
        loop {
            tokio::select! {
                _ = report_tick.tick() => {
                    if let Some(ctx) = sim_ctx.as_ref() {
                        report_hot_tokens(ctx);
                    }
                }
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

/// Spawn the background task that periodically refreshes
/// [`SimContext::mempool_prewarm`].
///
/// Runs one best-effort refresh on startup, then on every
/// `interval_blocks`-th `NewBlockEvent`. Each refresh snapshots the live
/// pool registry, collects code + V2-reserve addresses, fans the RPC
/// fetches out in parallel via
/// [`aether_simulator::fork::prewarm_state`], and atomically swaps the
/// result into the shared `ArcSwap`. Refresh failures are best-effort —
/// the stale snapshot is preserved so the validator keeps running warm.
///
/// Setting `interval_blocks = 0` is treated as `1`: at minimum one
/// refresh per new block. Tracked-pool bytecode rarely changes so the
/// default cadence (8 blocks, ~96 s on mainnet) is sufficient to
/// absorb registry growth without burning ~10 K RPCs/block on refresh.
pub fn spawn_mempool_prewarm_refresher(
    sim_ctx: Arc<SimContext>,
    provider: DynProvider<Ethereum>,
    channels: Arc<aether_ingestion::subscription::EventChannels>,
    metrics: Arc<EngineMetrics>,
    interval_blocks: u64,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut new_blocks = channels.subscribe_new_blocks();
        let interval = interval_blocks.max(1);
        // Delay the first refresh so it doesn't overlap with the engine's
        // own boot fetch — when both ran concurrently against a free-tier
        // RPC they doubled the burst and tripped 429s for ~30s after
        // startup. AETHER_PREWARM_INITIAL_DELAY_SECS lets the boot fetch
        // and the upstream's per-second quota settle before the prewarm
        // adds its ~126 calls. Default 20s; set 0 to keep the legacy
        // immediate behaviour when running against a private RPC.
        let initial_delay_secs = std::env::var("AETHER_PREWARM_INITIAL_DELAY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20);
        if initial_delay_secs > 0 {
            info!(
                target: "aether::mempool",
                delay_secs = initial_delay_secs,
                "delaying initial mempool prewarm to avoid boot-burst overlap"
            );
            tokio::time::sleep(std::time::Duration::from_secs(initial_delay_secs)).await;
        }
        run_prewarm_refresh(&sim_ctx, &provider, &metrics, 0).await;
        let mut last_refresh_block: u64 = 0;
        info!(
            target: "aether::mempool",
            interval_blocks = interval,
            "mempool prewarm refresher started"
        );

        loop {
            tokio::select! {
                next = new_blocks.recv() => match next {
                    Ok(block) => {
                        if block.block_number.saturating_sub(last_refresh_block) >= interval
                            || last_refresh_block == 0
                        {
                            run_prewarm_refresh(&sim_ctx, &provider, &metrics, block.block_number).await;
                            last_refresh_block = block.block_number;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            target: "aether::mempool",
                            lagged = n,
                            "prewarm refresher lagged on new_blocks; resuming"
                        );
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(target: "aether::mempool", "new_blocks closed; prewarm refresher exiting");
                        return;
                    }
                },
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!(target: "aether::mempool", "shutdown signalled; prewarm refresher exiting");
                        return;
                    }
                }
            }
        }
    })
}

/// One refresh cycle: snapshot the registry, fan out RPC fetches, swap
/// the result into the shared `ArcSwap`. Errors inside `prewarm_state`
/// are logged at warn level per-address; the cycle as a whole always
/// produces a snapshot (possibly with fewer entries on partial failure).
async fn run_prewarm_refresh(
    sim_ctx: &Arc<SimContext>,
    provider: &DynProvider<Ethereum>,
    metrics: &EngineMetrics,
    block_number: u64,
) {
    let started = Instant::now();
    let registry_guard = sim_ctx.pool_registry.load();
    let executor_addr = sim_ctx.backrun.as_ref().map(|c| c.executor_address);

    let mut code_addrs: Vec<Address> = Vec::with_capacity(registry_guard.len() + 1);
    if let Some(addr) = executor_addr {
        code_addrs.push(addr);
    }

    for meta in registry_guard.values() {
        code_addrs.push(meta.pool_id.address);
    }
    code_addrs.sort_unstable();
    code_addrs.dedup();

    let pool_count = registry_guard.len();
    drop(registry_guard);

    // Warm bytecode only. The backrun validator forks at `latest` and injects
    // code alone (see `PrewarmedState::inject_code_only`) so the victim replay
    // sees fresh reserves; pre-fetching the V2 reserve slot here would be
    // wasted (the consumer never injects it) and add to the boot-burst the
    // concurrency cap is fighting. Pass no V2 addresses so the storage fan-out
    // is a no-op — but still thread the persistent bytecode cache so repeat
    // code fetches are served from disk rather than re-hitting `eth_getCode`.
    let fresh = aether_simulator::fork::prewarm_state(
        provider,
        block_number,
        &code_addrs,
        &[],
        sim_ctx.bytecode_cache.as_deref(),
        None,
    )
    .await;
    metrics.record_prewarm_stats(fresh.stats);

    sim_ctx
        .mempool_prewarm
        .store(Arc::new(Some(Arc::new(fresh))));

    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    metrics.inc_mempool_prewarm_refresh("ok");
    metrics.observe_mempool_prewarm_refresh_duration_ms(elapsed_ms);
    metrics.set_mempool_prewarm_warm_pools(pool_count as i64);
    info!(
        target: "aether::mempool",
        block = block_number,
        pools = pool_count,
        elapsed_ms,
        "mempool prewarm refreshed"
    );
}

/// Decode one pending tx and update metrics + logs.
///
/// Pulled out as a free function so unit tests can drive it without spawning
/// the full pipeline task. The post-state scan (graph clone + Bellman-Ford)
/// is dispatched onto tokio's blocking pool to keep its CPU cost off the
/// main runtime workers — the engine's 3 ms p99 detection budget cannot
/// share worker threads with a 3.8 MB-per-event clone under load.
/// Wall-clock seconds since the Unix epoch. Saturates at 0 for the
/// (impossible in practice) pre-epoch case so the `u64` cast never wraps.
fn now_unix_secs() -> u64 {
    Utc::now().timestamp().max(0) as u64
}

/// Periodic operator-visibility log answering "which tokens is the mempool
/// trading most right now". Prunes cold pairs, then logs the top hot pairs and
/// the subset that look like pool-admission candidates (≥ `min_venues` distinct
/// venues). Pure logging — no submission, no registry mutation.
fn report_hot_tokens(ctx: &SimContext) {
    let now = now_unix_secs();
    let pruned = ctx.hot_tokens.prune(now);
    let tracked = ctx.hot_tokens.len();
    if tracked == 0 {
        return;
    }
    let top = ctx.hot_tokens.ranked(now);
    let candidates = ctx.hot_tokens.candidates(now, 10);
    for (rank, p) in top.iter().take(10).enumerate() {
        info!(
            target: "aether::mempool",
            rank = rank + 1,
            token_a = %p.token_a,
            token_b = %p.token_b,
            score = p.score,
            hits = p.hits,
            venues = p.venues,
            "HOT TOKEN"
        );
    }
    info!(
        target: "aether::mempool",
        tracked_pairs = tracked,
        pruned = pruned,
        candidates = candidates.len(),
        "hot-token report"
    );
}

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

    match decode_pending_many(to, &event.input) {
        Ok(mut swaps) => {
            if swaps.is_empty() {
                metrics.inc_pending_decode_errors("multicall_no_swaps");
                return;
            }
            // ETH-input swaps (swapExactETHForTokens, ethUnoswap*, etc.) carry
            // amount_in in msg.value, not calldata, so the decoder emits
            // amount_in = ZERO for them. The first emitted swap is the ETH-
            // consuming hop; subsequent swaps in a multicall operate on the
            // wrapped output. Backfill only swap[0] so the pre_sim_filter no
            // longer drops ETH-input flows as zero_amount.
            if !event.value.is_zero() {
                if let Some(first) = swaps.first_mut() {
                    if first.amount_in.is_zero() {
                        first.amount_in = event.value;
                    }
                }
            }
            for swap in swaps {
                emit_decoded(metrics, &router_label, &swap, &event);
                let Some(ctx) = sim_ctx else { continue };
                // Count every decoded swap — including pairs whose pool is not
                // yet registered (pre_sim_filter drops those below) — so the
                // hot-token tracker surfaces fresh, frequently-traded tokens as
                // pool-admission candidates.
                ctx.hot_tokens.record(
                    swap.token_in,
                    swap.token_out,
                    swap.protocol,
                    swap.pool_address,
                    now_unix_secs(),
                );
                if !pre_sim_filter(metrics, ctx, &swap) {
                    continue;
                }
                let metrics_c = Arc::clone(metrics);
                let ctx_c = Arc::clone(ctx);
                let router_label_c = router_label.clone();
                let event_c = event.clone();
                let tx_hash = event.tx_hash;
                tokio::task::spawn_blocking(move || {
                    try_post_state_scan(
                        &metrics_c,
                        &ctx_c,
                        &router_label_c,
                        &swap,
                        tx_hash,
                        to,
                        &event_c,
                    );
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
    // Bancor multi-hop special case: a victim swap of tokenA -> BNT -> tokenB
    // hits TWO pools (tokenA/BNT and tokenB/BNT) and the registry has no
    // direct (tokenA, tokenB, BancorV3) entry. The downstream
    // `try_post_state_scan` resolves both pools and emits two graph-edge
    // updates; here we just verify the two BNT-pair pools exist before
    // scheduling the spawn_blocking, mirroring the `not_in_registry` guard
    // for direct pairs.
    if target_protocol == ProtocolType::BancorV3
        && swap.token_in != BNT_ADDRESS
        && swap.token_out != BNT_ADDRESS
    {
        let leg_a = ctx.lookup_pool(swap.token_in, BNT_ADDRESS, ProtocolType::BancorV3);
        let leg_b = ctx.lookup_pool(swap.token_out, BNT_ADDRESS, ProtocolType::BancorV3);
        if leg_a.is_none() || leg_b.is_none() {
            metrics.inc_mempool_filtered("not_in_registry");
            info!(
                target: "aether::mempool",
                reason = "not_in_registry",
                protocol = ?swap.protocol,
                token_in = %swap.token_in,
                token_out = %swap.token_out,
                multihop = true,
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
            multihop = true,
            "FILTER PASS"
        );
        return true;
    }
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
        Protocol::OneInchV6 => None,
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
fn resolve_swap_pool(
    metrics: &EngineMetrics,
    ctx: &SimContext,
    swap: &DecodedSwap,
) -> Option<(PoolMetadata, Address, Address, ProtocolType)> {
    if swap.protocol == Protocol::OneInchV6 {
        let Some(pool_addr) = swap.pool_address else {
            metrics.inc_pending_arb_sim_skipped("unresolved_executor");
            return None;
        };
        let registry = ctx.pool_registry.load();
        let Some(meta) = registry.get(&pool_addr).cloned() else {
            metrics.inc_pending_arb_sim_skipped("pool_not_registered");
            return None;
        };
        let zero_for_one = swap.one_inch_zero_for_one.unwrap_or(true);
        let (token_in, token_out) = if zero_for_one {
            (meta.token0, meta.token1)
        } else {
            (meta.token1, meta.token0)
        };
        let protocol = meta.pool_id.protocol;
        return Some((meta, token_in, token_out, protocol));
    }
    let target_protocol = match swap.protocol {
        Protocol::UniswapV2 => ProtocolType::UniswapV2,
        Protocol::SushiSwap => ProtocolType::SushiSwap,
        Protocol::UniswapV3 => ProtocolType::UniswapV3,
        Protocol::BalancerV2 => ProtocolType::BalancerV2,
        Protocol::Curve => ProtocolType::Curve,
        Protocol::BancorV3 => ProtocolType::BancorV3,
        Protocol::OneInchV6 => unreachable!("OneInchV6 handled above"),
    };
    let meta = ctx.lookup_pool(swap.token_in, swap.token_out, target_protocol)?;
    if meta.pool_id.protocol != target_protocol {
        metrics.inc_pending_arb_sim_skipped("pool_not_registered");
        return None;
    }
    Some((meta, swap.token_in, swap.token_out, target_protocol))
}

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
    // 1inch v6 records carry the peeled pool address directly. Resolve
    // the pool's metadata + protocol up-front so the rest of this function
    // can run the same V2/V3/Curve dispatch as the native router decoders.
    // `pool_address = None` is the opaque `swap(executor, …)` arm — tagged
    // `unresolved_executor` and skipped.
    let (meta_resolved, swap_token_in, swap_token_out, target_protocol) =
        match resolve_swap_pool(metrics, ctx, swap) {
            Some(r) => r,
            None => return,
        };

    // Bancor multi-hop dispatch: tokenA -> BNT -> tokenB touches two pools
    // and no single (tokenA, tokenB, BancorV3) registry entry exists. The
    // multi-hop helper looks up both BNT pairs, runs the analytical
    // predictor on each leg, and emits two graph-edge updates + two
    // prediction rows. The single-leg path below intentionally keeps the
    // direct-pair predictor for the tokenA <-> BNT case.
    if target_protocol == ProtocolType::BancorV3
        && swap.token_in != BNT_ADDRESS
        && swap.token_out != BNT_ADDRESS
    {
        try_post_state_scan_bancor_multihop(
            metrics,
            ctx,
            router_label,
            swap,
            event_tx_hash,
            event_to,
            event,
        );
        return;
    }

    let token_idx = ctx.token_index.load();
    let Some(in_idx) = token_idx.get_index(&swap_token_in) else {
        metrics.inc_pending_arb_sim_skipped("token_in_unknown");
        return;
    };
    let Some(out_idx) = token_idx.get_index(&swap_token_out) else {
        metrics.inc_pending_arb_sim_skipped("token_out_unknown");
        return;
    };

    let meta = meta_resolved;
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
        Protocol::UniswapV3 | Protocol::BalancerV2 | Protocol::Curve | Protocol::BancorV3 | Protocol::OneInchV6 => {
            let pool_addr = meta.pool_id.address;
            let Some(state_arc) = ctx.pool_states.get(&pool_addr).map(|r| Arc::clone(r.value()))
            else {
                metrics.inc_pending_arb_sim_skipped("pool_state_missing");
                return;
            };
            let post = predict_post_state_with_replay(
                &state_arc,
                swap.token_in,
                swap.amount_in,
                |reason| metrics.inc_sim_evm_fallback(reason),
                |proto| {
                    try_post_state_replay(
                        metrics,
                        ctx,
                        proto,
                        pool_addr,
                        state_arc.as_ref(),
                        swap.token_in,
                        swap.token_out,
                        event,
                        snapshot.block_number,
                    )
                },
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
    let mut profitable: Vec<_> = cycles.into_iter().filter(|c| c.is_profitable()).collect();
    // Most-profitable first. `detect_from_affected` returns cycles in
    // traversal order, so the previous `profitable.first()` handed the
    // validator an arbitrary candidate rather than the best one.
    profitable.sort_by(|a, b| {
        b.profit_factor()
            .partial_cmp(&a.profit_factor())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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
        Protocol::Curve => PredictedPostState::Curve {
            reserve_in: post_in,
            reserve_out: post_out,
        },
        Protocol::BancorV3 => PredictedPostState::Bancor {
            reserve_in: post_in,
            reserve_out: post_out,
        },
        Protocol::OneInchV6 => PredictedPostState::OneInchV6 {
            reserve_in: post_in,
            reserve_out: post_out,
        },
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
    //
    // When AETHER_BACKRUN_SKIP_VICTIM=1 AND the victim is a V2/Sushi
    // single-hop swap, build a slot-8 storage override carrying the
    // analytical post-state reserves. The validator uses it to patch the
    // pair and skip the victim replay entirely. Outside of that narrow
    // case we pass None, preserving the original "replay victim then arb"
    // semantic.
    let victim_storage_overrides: Option<Vec<(Address, U256, U256)>> = (|| {
        if std::env::var("AETHER_BACKRUN_SKIP_VICTIM")
            .ok()
            .as_deref() != Some("1") {
            return None;
        }
        if !matches!(swap.protocol, Protocol::UniswapV2 | Protocol::SushiSwap) {
            return None;
        }
        if !swap.path_extra.is_empty() {
            return None;
        }
        let pool_addr = meta.pool_id.address;
        // Map analytical (post_in, post_out) to (R0', R1') of the pair.
        let (r0_f, r1_f) = if swap.token_in == meta.token0 {
            (post_in, post_out)
        } else {
            (post_out, post_in)
        };
        if !(r0_f.is_finite() && r1_f.is_finite()) || r0_f <= 0.0 || r1_f <= 0.0 {
            return None;
        }
        // V2 reserves are uint112 — clamp to that ceiling.
        let max_u112: u128 = (1u128 << 112) - 1;
        let r0 = (r0_f as u128).min(max_u112);
        let r1 = (r1_f as u128).min(max_u112);
        // Packed slot 8 layout: bits [0..112)=reserve0, [112..224)=reserve1,
        // [224..256)=blockTimestampLast. Timestamp affects only the TWAP
        // oracle accumulator, never the swap math — pass 0.
        let packed: U256 =
            U256::from(r0) | (U256::from(r1) << 112) | (U256::ZERO << 224);
        Some(vec![(pool_addr, U256::from(8u64), packed)])
    })();

    if let (Some(publisher), Some(cfg)) = (ctx.arb_publisher.as_ref(), ctx.backrun.as_ref()) {
        // Pre-sim gating, mirroring the block-driven path (engine.rs). Gate
        // against `graph` — the post-victim clone the cycle was detected on —
        // and hand the validator the best cycle that survives the gates,
        // dropping corrupt-edge / f64-overflow / fingerprint-cluster cycles
        // before they waste a revm fork.
        let fingerprint_index =
            cycle_gating::build_fingerprint_index(&profitable, &ctx.gating);
        let best = profitable.iter().find(|c| {
            matches!(
                cycle_gating::gate_pre_sim(c, &graph, &fingerprint_index, &ctx.gating, metrics),
                PreSimGateVerdict::Pass
            )
        });
        if let Some(best) = best {
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
                victim_storage_overrides,
                &ctx.pool_states,
                meta.pool_id.address,
                swap.token_in,
                post_in,
                post_out,
                ctx.gating,
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

/// Bancor V3 multi-hop post-state scan.
///
/// A victim swap `tokenA -> BNT -> tokenB` settles atomically through the
/// BNT intermediary across TWO pools. The decoder emits a single
/// `DecodedSwap` keyed on `(tokenA, tokenB)` with no pool address. This
/// function resolves both BNT-pair pools in the registry, runs the
/// analytical multi-hop predictor in [`BancorPool::predict_post_state_multihop`],
/// applies the resulting post-state to BOTH graph edges (tokenA <-> BNT
/// and tokenB <-> BNT, including their reverse directions), runs one
/// Bellman-Ford scan over the three affected vertices, and writes TWO
/// `mempool_predictions` rows — one per pool — sharing the victim's tx
/// hash. The writer table is keyed on a `prediction_id` UUID so multiple
/// rows per tx are first-class.
///
/// Skip reasons:
///   * `bancor_second_pool_not_found` — exactly one of the two BNT pairs
///     is missing from the registry. Distinct from `pool_not_registered`
///     so dashboards can tell missing-multi-hop apart from missing-single.
///   * `bancor_multihop_low_confidence` — the multi-hop predictor itself
///     returned `None` (uninitialised reserves, degenerate amounts, etc.).
///   * Existing reasons (`token_*_unknown`, `pool_state_missing`, etc.)
///     are reused where the failure shape matches the single-leg path.
#[allow(clippy::too_many_arguments)]
fn try_post_state_scan_bancor_multihop(
    metrics: &EngineMetrics,
    ctx: &SimContext,
    router_label: &str,
    swap: &DecodedSwap,
    event_tx_hash: alloy::primitives::B256,
    event_to: Address,
    event: &PendingTxEvent,
) {
    // Resolve token vertices first — the cycle scan needs BNT too because
    // the multi-hop path passes through it.
    let token_idx = ctx.token_index.load();
    let Some(in_idx) = token_idx.get_index(&swap.token_in) else {
        metrics.inc_pending_arb_sim_skipped("token_in_unknown");
        return;
    };
    let Some(out_idx) = token_idx.get_index(&swap.token_out) else {
        metrics.inc_pending_arb_sim_skipped("token_out_unknown");
        return;
    };
    let Some(bnt_idx) = token_idx.get_index(&BNT_ADDRESS) else {
        metrics.inc_pending_arb_sim_skipped("token_in_unknown");
        return;
    };

    // Look up both legs. `pre_sim_filter` already verifies both exist for
    // the steady-state path, but a registry swap between the filter and
    // the spawn_blocking is possible in principle, so the lookup repeats
    // here under a fresh `pool_registry` load.
    let leg_a_meta =
        match ctx.lookup_pool(swap.token_in, BNT_ADDRESS, ProtocolType::BancorV3) {
            Some(m) => m,
            None => {
                metrics.inc_pending_arb_sim_skipped("bancor_second_pool_not_found");
                return;
            }
        };
    let leg_b_meta =
        match ctx.lookup_pool(swap.token_out, BNT_ADDRESS, ProtocolType::BancorV3) {
            Some(m) => m,
            None => {
                metrics.inc_pending_arb_sim_skipped("bancor_second_pool_not_found");
                return;
            }
        };

    // Pull live PoolState for both pools — the analytical predictor needs
    // the up-to-date reserves the engine refreshes on every TokensTraded
    // event.
    let Some(leg_a_state) = ctx
        .pool_states
        .get(&leg_a_meta.pool_id.address)
        .map(|r| Arc::clone(r.value()))
    else {
        metrics.inc_pending_arb_sim_skipped("pool_state_missing");
        return;
    };
    let Some(leg_b_state) = ctx
        .pool_states
        .get(&leg_b_meta.pool_id.address)
        .map(|r| Arc::clone(r.value()))
    else {
        metrics.inc_pending_arb_sim_skipped("pool_state_missing");
        return;
    };
    let (leg_a_pool, leg_b_pool) = match (&*leg_a_state, &*leg_b_state) {
        (PoolState::Bancor(a), PoolState::Bancor(b)) => (a, b),
        _ => {
            // Registry / cache mismatch: the registry says BancorV3 but the
            // pool_states cache holds a different variant. Surface as a
            // low-confidence skip rather than panicking; the engine's
            // bootstrap normally keeps these in sync.
            metrics.inc_pending_arb_sim_skipped("bancor_multihop_low_confidence");
            return;
        }
    };

    let (leg_a_post, leg_b_post) = match leg_a_pool.predict_post_state_multihop(
        swap.token_in,
        swap.amount_in,
        swap.token_out,
        leg_b_pool,
    ) {
        Some(pair) => pair,
        None => {
            metrics.inc_pending_arb_sim_skipped("bancor_multihop_low_confidence");
            return;
        }
    };

    // Snapshot the live graph once and verify the four affected edges
    // (tokenA->BNT, BNT->tokenA, tokenB->BNT, BNT->tokenB) exist so the
    // graph clone can update them all. Missing any edge falls through to
    // `graph_edge_missing` mirroring the single-leg path.
    let snapshot = ctx.snapshot_manager.load_full();
    let leg_a_pool_id = leg_a_meta.pool_id;
    let leg_b_pool_id = leg_b_meta.pool_id;
    let leg_a_fee = leg_a_meta.fee_factor();
    let leg_b_fee = leg_b_meta.fee_factor();
    let has_edge = |from: usize, to: usize, pid| {
        snapshot
            .graph
            .edges_from(from)
            .iter()
            .any(|e| e.to == to && e.pool_id == pid)
    };
    if !has_edge(in_idx, bnt_idx, leg_a_pool_id)
        || !has_edge(bnt_idx, in_idx, leg_a_pool_id)
        || !has_edge(bnt_idx, out_idx, leg_b_pool_id)
        || !has_edge(out_idx, bnt_idx, leg_b_pool_id)
    {
        metrics.inc_pending_arb_sim_skipped("graph_edge_missing");
        return;
    }

    // Convert each leg's `BancorPostState` (aligned to the leg's swap
    // direction) into graph-edge reserve pairs.
    let leg_a_post_in = u256_to_f64_saturating(leg_a_post.new_balance_in);
    let leg_a_post_out = u256_to_f64_saturating(leg_a_post.new_balance_out);
    let leg_b_post_in = u256_to_f64_saturating(leg_b_post.new_balance_in);
    let leg_b_post_out = u256_to_f64_saturating(leg_b_post.new_balance_out);
    if leg_a_post_in <= 0.0
        || leg_a_post_out <= 0.0
        || leg_b_post_in <= 0.0
        || leg_b_post_out <= 0.0
    {
        metrics.inc_pending_arb_sim_skipped("post_state_invalid");
        return;
    }

    // Clone the graph and apply both legs' post-state. The reverse
    // directions are seeded so cycle scans across either side observe the
    // updated reserves in either traversal direction.
    let mut graph = snapshot.graph.clone();
    // Leg A: in_idx <-> bnt_idx (tokenA in, BNT out)
    graph.update_edge_from_reserves(
        in_idx,
        bnt_idx,
        leg_a_pool_id,
        leg_a_post_in,
        leg_a_post_out,
        leg_a_fee,
    );
    graph.update_edge_from_reserves(
        bnt_idx,
        in_idx,
        leg_a_pool_id,
        leg_a_post_out,
        leg_a_post_in,
        leg_a_fee,
    );
    // Leg B: bnt_idx <-> out_idx (BNT in, tokenB out). The predictor's
    // `new_balance_in` is on the BNT side (token_in to leg_b_pool's
    // `predict_post_state` was BNT), `new_balance_out` is on tokenB.
    graph.update_edge_from_reserves(
        bnt_idx,
        out_idx,
        leg_b_pool_id,
        leg_b_post_in,
        leg_b_post_out,
        leg_b_fee,
    );
    graph.update_edge_from_reserves(
        out_idx,
        bnt_idx,
        leg_b_pool_id,
        leg_b_post_out,
        leg_b_post_in,
        leg_b_fee,
    );

    let cycles = ctx
        .detector
        .detect_from_affected(&graph, &[in_idx, bnt_idx, out_idx]);
    let mut profitable: Vec<_> = cycles.into_iter().filter(|c| c.is_profitable()).collect();
    // Most-profitable first (see single-leg path for rationale).
    profitable.sort_by(|a, b| {
        b.profit_factor()
            .partial_cmp(&a.profit_factor())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Persist TWO prediction rows — one per affected pool. Both share the
    // victim's tx hash and predicted_target_block so the reconciler can
    // join them as siblings. `profit_factor_predicted` is the cycle's
    // factor (same on both rows when profitable) so dashboards see the
    // multi-hop trade contributed to the candidate funnel.
    let best_profit_factor = profitable.first().map(|c| c.profit_factor());
    let predicted_target_block = snapshot.block_number.saturating_add(1);
    for (meta, post_in, post_out) in [
        (&leg_a_meta, leg_a_post_in, leg_a_post_out),
        (&leg_b_meta, leg_b_post_in, leg_b_post_out),
    ] {
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
            predicted_target_block,
            predicted_post_state: PredictedPostState::Bancor {
                reserve_in: post_in,
                reserve_out: post_out,
            }
            .into_json(),
            profit_factor_predicted: best_profit_factor,
            detection_lead_ms: None,
            engine_git_sha: ctx.engine_git_sha.clone(),
        };
        ctx.prediction_sink.insert_prediction(prediction);
    }

    if profitable.is_empty() {
        metrics.inc_pending_arb_sim_skipped("no_profitable_cycle");
        return;
    }

    // Bump candidate counters once per profitable cycle, same shape as
    // the single-leg path. A multi-hop swap that surfaces N profitable
    // cycles contributes N to the metric — symmetrical to the single-leg
    // case, not doubled.
    for cycle in &profitable {
        let bucket = profit_bucket(cycle.profit_factor());
        metrics.inc_pending_arb_candidates(router_label, bucket);
    }

    if let (Some(publisher), Some(cfg)) = (ctx.arb_publisher.as_ref(), ctx.backrun.as_ref()) {
        // Pre-sim gating, mirroring the single-leg path. Gate against the
        // post-victim graph clone and pick the best surviving cycle.
        let fingerprint_index =
            cycle_gating::build_fingerprint_index(&profitable, &ctx.gating);
        let best = profitable.iter().find(|c| {
            matches!(
                cycle_gating::gate_pre_sim(c, &graph, &fingerprint_index, &ctx.gating, metrics),
                PreSimGateVerdict::Pass
            )
        });
        if let Some(best) = best {
            // post-state replay path uses revm directly against the live
            // fork; no V2 single-hop override shortcut applies here.
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
                None,
                &ctx.pool_states,
                Address::ZERO,
                swap.token_in,
                0.0,
                0.0,
                ctx.gating,
            );
        }
    }

    info!(
        target: "aether::mempool",
        router = %router_label,
        protocol = ?swap.protocol,
        leg_a_pool = %leg_a_meta.pool_id.address,
        leg_b_pool = %leg_b_meta.pool_id.address,
        token_in = %swap.token_in,
        token_out = %swap.token_out,
        candidates = profitable.len(),
        best_profit_bps = (profitable[0].profit_factor() * 10_000.0) as i64,
        multihop = true,
        "MEMPOOL ARB CANDIDATE"
    );
}

/// Try the revm-backed post-state replay fallback for a pending swap
/// whose analytical predictor returned a low-confidence flag.
///
/// Returns `Some(UnifiedPostState)` when the replay produced a usable
/// post-state, `None` otherwise (replay dormant, provider unavailable,
/// victim reverted, etc.). All outcomes bump
/// `aether_mempool_post_state_replay_total{outcome}` and observe the
/// per-attempt latency on
/// `aether_mempool_post_state_replay_latency_ms` so dashboards can
/// decompose the analytical-vs-revm path mix without re-parsing logs.
///
/// Concurrency is bounded by the same semaphore the backrun validator
/// uses — saturating it bumps `sim_error` so the caller path stays
/// consistent with other no-op exits.
///
/// Protocol dispatch: V3 routes through the `slot0() + liquidity()`
/// reader, Curve routes through the `balances(i)` reader (using the
/// cached `CurvePool.tokens` index to resolve `(i, j)` from the swap's
/// `token_in` / `token_out`), Balancer routes through the
/// `getPoolId() → Vault.getPoolTokens(poolId)` reader (passing the
/// cached `BalancerPool.token0` / `token1` so post-balances are
/// position-aligned with the consumer's existing convention). Bancor
/// is not surfaced — its analytical predictor is closed-form for the
/// single-pool case, and the multi-hop branch routes through a
/// separate path.
#[allow(clippy::too_many_arguments)]
fn try_post_state_replay(
    metrics: &EngineMetrics,
    ctx: &SimContext,
    protocol: ReplayProtocol,
    pool_addr: Address,
    pool_state: &PoolState,
    swap_token_in: Address,
    swap_token_out: Address,
    event: &PendingTxEvent,
    block_number: u64,
) -> Option<UnifiedPostState> {
    if !ctx.post_state_replay_enabled {
        metrics.inc_mempool_post_state_replay("unimplemented_protocol");
        return None;
    }
    let cfg = ctx.backrun.as_ref()?;
    let provider = cfg.provider.as_ref()?;
    let _permit = match cfg.sim_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            metrics.inc_mempool_post_state_replay("sim_error");
            return None;
        }
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let victim = VictimTx {
        from: event.from,
        to: event.to?,
        value: event.value,
        data: event.input.clone(),
        gas_price: event.gas_price,
        gas_limit: MEMPOOL_VICTIM_GAS_LIMIT,
    };
    let params = ReplayParams {
        block_number,
        block_timestamp: now_secs,
        base_fee: 1_000_000_000,
        chain_id: cfg.chain_id,
    };

    // Build a fresh `RpcForkedState` per replay. Cloning the provider's
    // Arc handle is cheap; the underlying `AlloyDB` cache is per-state and
    // fills lazily on the first read. Errors here surface as `sim_error`
    // so the dashboard separates dispatch-time failures from per-protocol
    // reader failures.
    let started = Instant::now();
    let result: Result<UnifiedPostState, &'static str> = match protocol {
        ReplayProtocol::UniswapV3 => {
            let state = match aether_simulator::fork::RpcForkedState::new_at_latest(
                provider.clone(),
                block_number,
                now_secs,
                1_000_000_000,
            ) {
                Some(s) => s,
                None => {
                    metrics.inc_mempool_post_state_replay("sim_error");
                    return None;
                }
            };
            match replay_v3_post_state_rpc(state, &victim, pool_addr, &params) {
                Ok(post) => Ok(UnifiedPostState::UniswapV3(post)),
                Err(e) => Err(e.as_str()),
            }
        }
        ReplayProtocol::Curve => {
            // Coin indices come from the cached `CurvePool.tokens`
            // ordering — the on-chain `balances(uint256 i)` view is
            // keyed on that same index. If either token is unknown the
            // replay can't read post-balances; surface `decode_failed`
            // so the metric label matches downstream reader failures.
            let PoolState::Curve(curve) = pool_state else {
                metrics.inc_mempool_post_state_replay("decode_failed");
                return None;
            };
            let Some(i) = curve.tokens.iter().position(|t| *t == swap_token_in) else {
                metrics.inc_mempool_post_state_replay("decode_failed");
                return None;
            };
            let Some(j) = curve.tokens.iter().position(|t| *t == swap_token_out) else {
                metrics.inc_mempool_post_state_replay("decode_failed");
                return None;
            };
            let state = match aether_simulator::fork::RpcForkedState::new_at_latest(
                provider.clone(),
                block_number,
                now_secs,
                1_000_000_000,
            ) {
                Some(s) => s,
                None => {
                    metrics.inc_mempool_post_state_replay("sim_error");
                    return None;
                }
            };
            match replay_curve_post_state_rpc(
                state,
                &victim,
                pool_addr,
                i as u8,
                j as u8,
                &params,
            ) {
                Ok(post) => Ok(UnifiedPostState::Curve(post)),
                Err(e) => Err(e.as_str()),
            }
        }
        ReplayProtocol::Balancer => {
            // Use the pool's canonical `(token0, token1)` ordering so
            // `BalancerPostState.new_balance0` aligns with `meta.token0`
            // — the consumer's `unified_to_post_reserves` re-derives
            // swap direction from that convention.
            let PoolState::Balancer(bal) = pool_state else {
                metrics.inc_mempool_post_state_replay("decode_failed");
                return None;
            };
            let state = match aether_simulator::fork::RpcForkedState::new_at_latest(
                provider.clone(),
                block_number,
                now_secs,
                1_000_000_000,
            ) {
                Some(s) => s,
                None => {
                    metrics.inc_mempool_post_state_replay("sim_error");
                    return None;
                }
            };
            match replay_balancer_post_state_rpc(
                state,
                &victim,
                pool_addr,
                bal.token0,
                bal.token1,
                &params,
            ) {
                Ok(post) => Ok(UnifiedPostState::Balancer(post)),
                Err(e) => Err(e.as_str()),
            }
        }
        ReplayProtocol::Bancor => {
            metrics.inc_mempool_post_state_replay("unimplemented_protocol");
            return None;
        }
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    metrics.observe_mempool_post_state_replay_latency_ms(elapsed_ms);
    match result {
        Ok(post) => {
            metrics.inc_mempool_post_state_replay("success");
            debug!(
                target: "aether::mempool",
                tx_hash = %event.tx_hash,
                pool = %pool_addr,
                elapsed_ms,
                ?protocol,
                "POST-STATE REPLAY succeeded"
            );
            Some(post)
        }
        Err(reason) => {
            metrics.inc_mempool_post_state_replay(reason);
            debug!(
                target: "aether::mempool",
                tx_hash = %event.tx_hash,
                pool = %pool_addr,
                reason,
                elapsed_ms,
                ?protocol,
                "POST-STATE REPLAY failed"
            );
            None
        }
    }
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
    victim_storage_overrides: Option<Vec<(Address, U256, U256)>>,
    pool_states: &PoolStateCache,
    victim_pool: Address,
    victim_token_in: Address,
    victim_post_in: f64,
    victim_post_out: f64,
    gating: GatingConfig,
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
    let mut steps = match cycle_to_swap_steps(graph, token_index, cycle, cfg.input_amount_wei) {
        Some(s) if !s.is_empty() => s,
        _ => {
            metrics.inc_mempool_backrun_rejected("cycle_unbuildable");
            return None;
        }
    };
    let flashloan_token = steps[0].token_in;

    // Size the flashloan to maximize realized net profit (gross − input − gas
    // − Aave premium) via exact V2/Sushi AMM math. Falls back to the fixed
    // size for non-V2/Sushi cycles or missing pool state; drops the candidate
    // pre-revm when no size clears `min_profit_wei`.
    // Detector/optimizer net-profit estimate, captured for the post-sim
    // cross-check (gate 4). Zero on the fallback path means the gate has
    // nothing to compare against and passes.
    let mut expected_net_wei: u128 = 0;
    let flashloan_amount = match optimize_cycle_input(
        &steps,
        pool_states,
        victim_pool,
        victim_token_in,
        victim_post_in,
        victim_post_out,
        cfg.gas_price_gwei,
        cfg.min_profit_wei,
    ) {
        SizingOutcome::Sized(sizing) => {
            debug!(
                input_amount = %sizing.input_amount,
                net_profit_wei = sizing.net_profit_wei,
                "mempool-backrun: optimized arb size"
            );
            expected_net_wei = sizing.net_profit_wei;
            // amount_in is an upper bound (the contract clamps to the real
            // post-prior-hop balance); min_amount_out is the slippage guard.
            let mut prev_in = sizing.input_amount;
            for (i, step) in steps.iter_mut().enumerate() {
                step.amount_in = prev_in;
                let expected = sizing.per_hop_expected_out[i];
                step.min_amount_out = expected
                    .saturating_mul(U256::from(10_000u32 - BACKRUN_SLIPPAGE_BPS))
                    / U256::from(10_000u32);
                prev_in = expected;
            }
            sizing.input_amount
        }
        SizingOutcome::BelowMinProfit => {
            metrics.inc_mempool_backrun_rejected("below_min_profit_optimized");
            return None;
        }
        SizingOutcome::Fallback => cfg.input_amount_wei,
    };

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
        flashloan_amount,
        deadline,
        cfg.min_profit_wei,
        U256::from(9000u64), // 90% tip share — conservative starting point
    );

    let victim = VictimTx {
        from: event.from,
        to: event.to?,
        value: event.value,
        data: event.input.clone(),
        gas_price: event.gas_price,
        gas_limit: MEMPOOL_VICTIM_GAS_LIMIT,
    };
    let arb = ArbTx {
        caller: cfg.searcher_caller,
        to: cfg.executor_address,
        // A flashloan + multi-hop arb (Aave ~80k + per-hop swaps + executor
        // overhead) routinely exceeds 1.5M for 3+ hops, OOG-halting a valid
        // arb leg. 3M matches the live-fork integration test's budget.
        data: calldata,
        gas_limit: 3_000_000,
    };
    let params = ValidatorParams {
        block_number,
        block_timestamp: now_secs,
        base_fee: 1_000_000_000,
        chain_id: cfg.chain_id,
        profit_token: cfg.profit_token,
        profit_recipient: cfg.searcher_caller,
        balance_slot: cfg.balance_slot,
        executor_bytecode: cfg.executor_bytecode.clone(),
        skip_victim_with_overrides: victim_storage_overrides,
    };

    // Build the forked state and validate, with bounded retry on transient RPC
    // transport errors. `validate_backrun_rpc` consumes the fork state, so each
    // attempt builds a fresh one. The fork is PINNED to the detected block by
    // default (the cycle was detected and sized against the snapshot graph at
    // `block_number` plus the analytical victim post-state, so the sim must
    // fork that same block — then replay the victim on top — to execute
    // against the exact state the candidate was sized for); the block-driven
    // path pins identically (engine.rs `RpcForkedState::new`). Forking
    // `latest` (chain head) lets blocks mined between detection and this
    // (spawn_blocking) sim drift the reserves out from under the
    // analytically-derived `min_amount_out` guards, which is the dominant
    // "passes detection then reverts" cause. `AETHER_BACKRUN_FORK_LATEST=1`
    // restores the `latest` behaviour for Anvil forks whose locally-mined
    // block numbers past the fork base may not resolve cleanly for state
    // queries.
    //
    // A cold-fetch stall — now bounded by the provider's per-request timeout
    // — surfaces as `RpcTransport`; retrying re-drives the fetches, which
    // usually succeed on a second attempt. Economic and revert rejections
    // return immediately.
    let warm_guard = cfg.mempool_prewarm.load();
    let max_retries = std::env::var("AETHER_MEMPOOL_SIM_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1);
    let fork_latest = std::env::var("AETHER_BACKRUN_FORK_LATEST")
        .ok()
        .as_deref()
        == Some("1");
    let mut attempt = 0u32;
    let result = loop {
        // Base fee unknown at the snapshot — pass 1 gwei and rely on
        // `disable_base_fee = true` inside the simulator's cfg.
        let fork_result = if fork_latest {
            aether_simulator::fork::RpcForkedState::new_at_latest(
                provider.clone(),
                block_number,
                now_secs,
                1_000_000_000,
            )
        } else {
            aether_simulator::fork::RpcForkedState::new(
                provider.clone(),
                block_number,
                now_secs,
                1_000_000_000,
            )
        };
        let mut state = match fork_result {
            Some(s) => s,
            None => {
                metrics.inc_mempool_backrun_rejected("fork_construction_failed");
                return None;
            }
        };
        // Inject ONLY the pre-warmed bytecode (see
        // `PrewarmedState::inject_code_only`): warming bytecode by code hash
        // eliminates the dominant cold-fetch cost without shadowing the fresh
        // reserve read the victim replay depends on.
        if let Some(warm) = warm_guard.as_ref() {
            warm.inject_code_only(&mut state);
            metrics.inc_mempool_prewarm_hit();
        } else {
            metrics.inc_mempool_prewarm_miss();
        }

        let r = validate_backrun_rpc(state, &victim, &arb, &params);
        if r.accepted
            || r.reject != Some(RejectReason::RpcTransport)
            || attempt >= max_retries
        {
            break r;
        }
        attempt += 1;
        debug!(
            target: "aether::mempool",
            tx_hash = %event.tx_hash,
            attempt,
            "mempool-backrun: retrying after RPC transport error"
        );
    };
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;

    if !result.accepted {
        let mut reason = result
            .reject
            .as_ref()
            .map(|r| r.as_str())
            .unwrap_or(RejectReason::SimError.as_str());
        // Relabel a slow transport/sim failure as `sim_timeout` when the
        // attempt blew the wall-clock budget, so RPC stalls show up distinctly
        // from fast deterministic sim errors on the funnel dashboard.
        let sim_timeout_ms = std::env::var("AETHER_MEMPOOL_SIM_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(5000.0);
        if (reason == RejectReason::RpcTransport.as_str()
            || reason == RejectReason::SimError.as_str())
            && elapsed_ms > sim_timeout_ms
        {
            reason = RejectReason::SimTimeout.as_str();
        }
        metrics.inc_mempool_backrun_rejected(reason);
        metrics.observe_mempool_backrun_validation_latency_ms("reject", elapsed_ms);
        // Record the gross / gas-cost / deficit triple so dashboards can
        // distinguish "missed breakeven by ε" (gas-model tuning win) from
        // "missed by miles" (paths fundamentally too thin) without
        // grepping `aether-rust` logs for the matching info! line. Gas cost
        // is pinned to `params.base_fee` so the histogram matches the
        // simulator's own NegativeAfterGas accounting; the executor's real
        // gas pricing lives on a separate counter on the Go side.
        let gas_cost_wei = alloy::primitives::U256::from(result.arb_gas_used)
            .saturating_mul(alloy::primitives::U256::from(params.base_fee));
        let gross_eth = EngineMetrics::wei_to_eth_f64(result.gross_profit_wei);
        let gas_cost_eth = EngineMetrics::wei_to_eth_f64(gas_cost_wei);
        let deficit_wei = gas_cost_wei.saturating_sub(result.gross_profit_wei);
        let deficit_eth = EngineMetrics::wei_to_eth_f64(deficit_wei);
        metrics.observe_mempool_backrun_gross_profit_eth(reason, gross_eth);
        metrics.observe_mempool_backrun_gas_cost_eth(reason, gas_cost_eth);
        // Only emit the deficit observation when the math is meaningful —
        // RpcTransport / SimTimeout / fork_construction-style rejects have
        // arb_gas_used == 0 and would skew the deficit histogram toward 0
        // with samples that aren't really "near breakeven".
        if result.arb_gas_used > 0 && deficit_wei > alloy::primitives::U256::ZERO {
            metrics.observe_mempool_backrun_deficit_eth(reason, deficit_eth);
        }
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

    // Post-sim cross-check (gate 4), mirroring the block-driven path
    // (engine.rs `gate_post_sim`). The validator only accepts profitable
    // sims, so this primarily catches the detector/optimizer wildly
    // over-estimating profit versus what revm actually realized against the
    // (pinned) forked state — a stale-snapshot signature. Drop before
    // publishing rather than handing the executor a contradicted arb.
    let actual_profit_u128: u128 = result.gross_profit_wei.try_into().unwrap_or(u128::MAX);
    if let PostSimGateVerdict::Drop(_) =
        cycle_gating::gate_post_sim(expected_net_wei, actual_profit_u128, &gating, metrics)
    {
        metrics.inc_mempool_backrun_rejected("revm_contradicts");
        return None;
    }

    // Accept path — publish a ValidatedArb tagged MEMPOOL_BACKRUN.
    metrics.observe_mempool_backrun_validation_latency_ms("accept", elapsed_ms);
    let bucket = gross_profit_bucket(result.gross_profit_wei);
    metrics.inc_mempool_backrun_validated(bucket);
    // Mirror the reject-path histogram observations under reason="accepted"
    // so a single PromQL series can compare survivor gross/gas distributions
    // against each rejected bucket without joining two metric names.
    let accept_gas_cost_wei = alloy::primitives::U256::from(result.arb_gas_used)
        .saturating_mul(alloy::primitives::U256::from(params.base_fee));
    metrics.observe_mempool_backrun_gross_profit_eth(
        "accepted",
        EngineMetrics::wei_to_eth_f64(result.gross_profit_wei),
    );
    metrics.observe_mempool_backrun_gas_cost_eth(
        "accepted",
        EngineMetrics::wei_to_eth_f64(accept_gas_cost_wei),
    );

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
        // Publish the OPTIMIZER-SIZED loan that was actually simulated, not
        // the fixed `cfg.input_amount_wei` default — otherwise the Go
        // executor would request a different loan than was validated, and
        // the reconciler would compare against the wrong size.
        flashloan_amount: u256_bytes(flashloan_amount),
        steps: steps.into_iter().map(swap_step_to_proto).collect(),
        calldata: arb.data.into(),
        source: aether_proto::ArbSource::MempoolBackrun as i32,
        victim_tx_hash: event.tx_hash.0.to_vec().into(),
        target_block: block_number.saturating_add(1),
        victim_raw_tx: event.raw_tx.clone().into(),
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

/// Outcome of attempting to optimally size a backrun cycle's input.
enum SizingOutcome {
    /// The cycle is not all-UniswapV2/SushiSwap, or a hop pool's exact
    /// state was unavailable. The caller falls back to the fixed-size
    /// path unchanged — never a silent drop.
    Fallback,
    /// The cycle is optimizable but the best realized net profit (after
    /// gas and the Aave premium) is below `min_profit_wei`. The caller
    /// drops the candidate and counts `below_min_profit_optimized`.
    BelowMinProfit,
    /// A profitable input size was found.
    Sized(OptimizedSizing),
}

/// Result of optimally sizing a backrun cycle's input amount.
struct OptimizedSizing {
    /// Flashloan / hop-0 input amount that maximizes realized net profit.
    input_amount: U256,
    /// Exact `get_amount_out` for each hop at `input_amount`, in hop order.
    /// `per_hop_expected_out[i]` is the output of hop `i` (== input of hop
    /// `i+1`); the last entry is the cycle's gross output.
    per_hop_expected_out: Vec<U256>,
    /// Realized net profit at `input_amount` after gas + Aave premium.
    net_profit_wei: u128,
}

/// Resolve each hop of a backrun cycle to a concrete [`UniswapV2Pool`] with
/// exact on-chain reserves, overlaying the post-victim reserves on the
/// victim's hop, and ternary-search the input amount that maximizes
/// realized net profit (gross output − input − gas − Aave premium).
///
/// Exact AMM quote for any pool protocol via its own `Pool::get_amount_out`,
/// so [`optimize_cycle_input`] can size mixed-protocol cycles exactly instead
/// of falling back to a blind fixed size.
fn poolstate_quote(ps: &PoolState, token_in: Address, amount_in: U256) -> Option<U256> {
    match ps {
        PoolState::UniswapV2(p) | PoolState::SushiSwap(p) => p.get_amount_out(token_in, amount_in),
        PoolState::UniswapV3(p) => p.get_amount_out(token_in, amount_in),
        PoolState::Curve(p) => p.get_amount_out(token_in, amount_in),
        PoolState::Balancer(p) => p.get_amount_out(token_in, amount_in),
        PoolState::Bancor(p) => p.get_amount_out(token_in, amount_in),
    }
}

/// Returns [`SizingOutcome::Fallback`] only when a hop's pool state is missing
/// from the cache; every protocol is quoted via its own exact
/// `Pool::get_amount_out`. AMM math uses the pools'
/// own exact-U256 `get_amount_out` (never the decimal-normalized
/// `PriceGraph` f64 reserves, which are unreliable for sizing).
///
/// The victim's pool reserves are read from `(victim_post_in,
/// victim_post_out)` — the analytical post-victim state computed upstream
/// in `predict_and_validate` — because the arb executes *after* the victim
/// swap lands, so the victim's pool is already shifted by the time the
/// backrun runs.
#[allow(clippy::too_many_arguments)]
fn optimize_cycle_input(
    steps: &[aether_common::types::SwapStep],
    pool_states: &PoolStateCache,
    victim_pool: Address,
    victim_token_in: Address,
    victim_post_in: f64,
    victim_post_out: f64,
    gas_price_gwei: f64,
    min_profit_wei: U256,
) -> SizingOutcome {
    if steps.is_empty() {
        return SizingOutcome::Fallback;
    }

    // Resolve every hop to its concrete pool state. Each hop is quoted with
    // its own exact `Pool::get_amount_out` (UniV3 tick math, Curve invariant,
    // Balancer weighted, Bancor curve), so mixed-protocol cycles are sized
    // exactly. Missing pool state is the only remaining reason to fall back —
    // without it we cannot run exact math, so the blind fixed-size path is the
    // safest behaviour.
    let mut hop_states: Vec<PoolState> = Vec::with_capacity(steps.len());
    for step in steps {
        let Some(entry) = pool_states.get(&step.pool_address) else {
            return SizingOutcome::Fallback;
        };
        let mut ps = entry.value().as_ref().clone();
        // Overlay the post-victim reserves on the victim's pool so sizing
        // reflects the state the arb will actually trade against. Only the
        // V2/Sushi post-state is a simple (r0, r1) overlay; for other victim
        // protocols the revm sim applies the true post-state, so the optimizer
        // sizes against the pre-victim state (the sim still bounds the result).
        if step.pool_address == victim_pool {
            if let PoolState::UniswapV2(p) | PoolState::SushiSwap(p) = &mut ps {
                if let Some((r0, r1)) =
                    victim_post_reserves(p, victim_token_in, victim_post_in, victim_post_out)
                {
                    p.update_state(r0, r1);
                }
            }
        }
        hop_states.push(ps);
    }

    // Gas cost mirrors the block path: per-protocol base gas + fixed
    // overheads, priced at the configured gwei. All hops are V2/Sushi, so
    // tick counts are zero.
    let protocols: Vec<ProtocolType> = steps.iter().map(|s| s.protocol).collect();
    let tick_counts = vec![0u32; protocols.len()];
    let total_gas = estimate_total_gas(&protocols, &tick_counts);
    let gas_cost = gas_cost_wei(total_gas, gas_price_gwei);

    // Run the exact-math hop chain for a candidate input, returning the
    // cycle's gross output (same token as the input) or `None` if any hop
    // cannot quote (zero amount / depleted reserve).
    let token_in0 = steps[0].token_in;
    let run_chain = |input: U256| -> Option<U256> {
        let mut current = input;
        let mut current_token = token_in0;
        for (i, ps) in hop_states.iter().enumerate() {
            current = poolstate_quote(ps, current_token, current)?;
            if current.is_zero() {
                return None;
            }
            current_token = steps[i].token_out;
        }
        Some(current)
    };

    // Net profit (signed wei) = gross_out − input − gas − premium.
    // Premium is the Aave V3 flashLoanSimple fee on the borrowed input.
    let profit_fn = |input: U256| -> i128 {
        let Some(gross_out) = run_chain(input) else {
            // Unquotable size: steer the search away from it.
            return i128::MIN / 2;
        };
        let premium = saturating_u256_to_i128(input)
            .saturating_mul(AAVE_FLASHLOAN_PREMIUM_BPS as i128)
            / 10_000;
        saturating_u256_to_i128(gross_out)
            .saturating_sub(saturating_u256_to_i128(input))
            .saturating_sub(gas_cost as i128)
            .saturating_sub(premium)
    };

    // max_input is bounded by the FIRST hop's input-side reserve — the only
    // hop denominated in the flashloan/input token. Intermediate hops are
    // priced in other tokens/decimals, so folding their raw reserves into
    // this cap would be a unit mismatch (e.g. USDC's 6-decimal reserve is
    // numerically tiny next to an 18-decimal WETH input and would wrongly
    // collapse the cap). Their depth is already captured by the
    // constant-product `profit_fn`, which sees marginal output collapse past
    // their liquidity. Allow at most half the input reserve, then clamp to
    // the hard ceiling.
    // Exact for V2/Sushi (reserve denominated in the input token); for other
    // protocols rely on the hard ceiling, since `profit_fn` already collapses
    // past their depth via the exact `get_amount_out`.
    let first_reserve_in = match &hop_states[0] {
        PoolState::UniswapV2(p) | PoolState::SushiSwap(p) => {
            if token_in0 == p.token0 {
                p.reserve0
            } else {
                p.reserve1
            }
        }
        _ => U256::from(OPTIMIZE_MAX_INPUT_WEI),
    };
    let depth_cap = (first_reserve_in / U256::from(2u64)).min(U256::from(OPTIMIZE_MAX_INPUT_WEI));

    let min_input = U256::from(OPTIMIZE_MIN_INPUT_WEI);
    let max_input = depth_cap.min(U256::from(OPTIMIZE_MAX_INPUT_WEI));

    let (optimal_input, net_profit_i128) = if min_input < max_input {
        ternary_search_optimal_input(min_input, max_input, OPTIMIZE_ITERATIONS, profit_fn)
    } else {
        (min_input, profit_fn(min_input))
    };

    if net_profit_i128 <= 0 {
        return SizingOutcome::BelowMinProfit;
    }
    let net_profit_wei = net_profit_i128 as u128;
    if U256::from(net_profit_wei) < min_profit_wei {
        return SizingOutcome::BelowMinProfit;
    }

    // Recompute the exact per-hop outputs at the chosen input.
    let mut per_hop_expected_out = Vec::with_capacity(hop_states.len());
    let mut current = optimal_input;
    let mut current_token = token_in0;
    for (i, ps) in hop_states.iter().enumerate() {
        let Some(out) = poolstate_quote(ps, current_token, current) else {
            // The optimum quoted moments ago; a None here means a
            // degenerate edge — fall back rather than publish junk.
            return SizingOutcome::Fallback;
        };
        per_hop_expected_out.push(out);
        current = out;
        current_token = steps[i].token_out;
    }

    SizingOutcome::Sized(OptimizedSizing {
        input_amount: optimal_input,
        per_hop_expected_out,
        net_profit_wei,
    })
}

/// Map the analytical post-victim reserves `(post_in, post_out)` — oriented
/// to the victim's `token_in -> token_out` direction — back onto a
/// [`UniswapV2Pool`]'s `(reserve0, reserve1)`. Floors each f64 reserve and
/// clamps to the uint112 ceiling that V2 pairs enforce on-chain. Returns
/// `None` for non-finite or non-positive reserves.
fn victim_post_reserves(
    pool: &UniswapV2Pool,
    victim_token_in: Address,
    post_in: f64,
    post_out: f64,
) -> Option<(U256, U256)> {
    if !(post_in.is_finite() && post_out.is_finite()) || post_in <= 0.0 || post_out <= 0.0 {
        return None;
    }
    let max_u112: u128 = (1u128 << 112) - 1;
    let to_reserve = |v: f64| -> U256 { U256::from((v as u128).min(max_u112)) };
    let (r_in, r_out) = (to_reserve(post_in), to_reserve(post_out));
    if victim_token_in == pool.token0 {
        Some((r_in, r_out))
    } else {
        Some((r_out, r_in))
    }
}

/// Saturating U256 → i128 for profit accounting. Token amounts seen here
/// (≤ 50 ETH of input, proportionate outputs) sit far below `u128::MAX`, so
/// this is exact in practice; the clamp only guards adversarial overflow.
fn saturating_u256_to_i128(v: U256) -> i128 {
    v.min(U256::from(i128::MAX as u128)).to::<u128>() as i128
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
        // Pick the SAME edge Bellman-Ford scored the cycle on: the
        // lowest-weight (best-rate) pool for this hop. Using the first
        // matching edge meant that when two pools connect the same token
        // pair, the cycle was scored on pool X's rate but the bundle swapped
        // through pool Y (`min_by` in bellman_ford.rs vs `.find` here).
        let edge = graph
            .edges_from(from_idx)
            .iter()
            .filter(|e| e.to == to_idx)
            .min_by(|a, b| {
                a.weight
                    .partial_cmp(&b.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
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
        Protocol::OneInchV6 => PROTOCOL_ONE_INCH_V6,
    }
}

/// Map a V3 / Balancer post-state into the (post_in, post_out) reserves the
/// price graph stores per edge. Curve cannot reach here — the router
/// decoder rejects every Curve calldata shape with `CurveUnsupported`
/// before the pipeline sees it — but the variant is matched explicitly so
/// new protocol families fail the build instead of silently routing to
/// reserves of `(0.0, 0.0)`.
///
/// **V3 mapping.** The predictor returns `new_sqrt_price_x96` and
/// `new_liquidity`. From those we derive virtual constant-product reserves
/// `(x_v, y_v) = (token0, token1)` raw units (`uniswap_v3::virtual_reserves`):
/// `x_v = L·2^96/sqrt`, `y_v = L·sqrt/2^96`. We assign `(x_v, y_v)` for the
/// `token0 → token1` direction and `(y_v, x_v)` for the reverse. Because
/// `y_v/x_v == spot`, the graph weight (`-ln((reserve_out/reserve_in)·fee)`)
/// is identical to the legacy `(1.0, spot)` seed — but const-product on the
/// virtual pair reproduces V3 single-tick math exactly, so the backrun
/// optimizer sizes the trade with real post-victim depth instead of an
/// infinitely-shallow curve. `fee_factor` is applied at the graph layer,
/// matching the engine's bootstrap and live V3-edge seeding. Zero post-state
/// liquidity yields `(0.0, 0.0)` (no tradable depth).
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
            // Virtual constant-product reserves (x_v, y_v) = (token0, token1)
            // raw units from the *post-swap* sqrtPrice + liquidity. These
            // reproduce the post-victim V3 curve exactly for the backrun
            // optimizer, matching the engine's edge-seeding convention
            // (`uniswap_v3::virtual_reserves`). The legacy `(1.0, spot)` seed
            // only carried the marginal rate, giving the const-product profit
            // function an infinitely-shallow curve and wrong backrun sizing.
            match aether_pools::uniswap_v3::virtual_reserves(
                v3.new_sqrt_price_x96,
                v3.new_liquidity,
            ) {
                Some((x_v, y_v)) => {
                    if swap_token_in == meta.token0 {
                        (x_v, y_v)
                    } else {
                        (y_v, x_v)
                    }
                }
                // Zero post-state liquidity / sqrtPrice → no tradable depth.
                None => (0.0, 0.0),
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
        UnifiedPostState::Curve(c) => {
            // `CurvePostState.i`/`.j` are the swap direction (token_in / token_out)
            // *as the predictor saw it*, regardless of the pool's underlying
            // token ordering — so `new_balance_in` is always for `swap_token_in`
            // and `new_balance_out` is always for the other side. No swap_token_in
            // vs meta.token0/token1 comparison is needed here.
            let post_in = u256_to_f64_saturating(c.new_balance_in);
            let post_out = u256_to_f64_saturating(c.new_balance_out);
            (post_in, post_out)
        }
        UnifiedPostState::Bancor(b) => {
            // Same shape as Curve: `BancorPostState.new_balance_in`/`new_balance_out`
            // are already aligned with the swap direction (the predictor checks
            // `token_in == self.token` vs `== self.bnt` upstream). Trust them
            // directly without re-deriving the direction from meta.token0/token1.
            let post_in = u256_to_f64_saturating(b.new_balance_in);
            let post_out = u256_to_f64_saturating(b.new_balance_out);
            (post_in, post_out)
        }
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
        Protocol::OneInchV6 => "one_inch_v6",
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
            raw_tx: vec![],
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

    // ----- unified_to_post_reserves Curve arm -----

    #[test]
    fn unified_to_post_reserves_curve_uses_predictor_balances_directly() {
        use aether_common::types::PoolId;
        use aether_pools::curve::CurvePostState;
        use aether_pools::UnifiedPostState;
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let meta = PoolMetadata {
            token0_idx: 0,
            token1_idx: 1,
            token0: usdc,
            token1: usdt,
            pool_id: PoolId {
                address: Address::ZERO,
                protocol: ProtocolType::Curve,
            },
            protocol: ProtocolType::Curve,
            fee_bps: 4,
            tick_spacing: None,
        };
        let post = UnifiedPostState::Curve(CurvePostState {
            i: 0,
            j: 1,
            new_balance_in: U256::from(11_000_000u64),
            new_balance_out: U256::from(9_900_000u64),
            amount_out: U256::from(99_500u64),
            analytical: true,
        });
        // Curve predictor already reports "in" and "out" relative to the
        // swap direction — the helper must trust them directly, regardless
        // of swap_token_in vs meta.token0/token1 ordering.
        let (pin, pout) = unified_to_post_reserves(usdc, &meta, &post);
        assert!((pin - 11_000_000.0).abs() < 1e-6);
        assert!((pout - 9_900_000.0).abs() < 1e-6);

        // Reverse direction: predictor would have flipped `i`/`j` upstream;
        // helper still trusts `new_balance_in`/`new_balance_out` directly,
        // which is the contract documented on the function.
        let post_reverse = UnifiedPostState::Curve(CurvePostState {
            i: 1,
            j: 0,
            new_balance_in: U256::from(11_000_000u64),
            new_balance_out: U256::from(9_900_000u64),
            amount_out: U256::from(99_500u64),
            analytical: true,
        });
        let (pin_r, pout_r) = unified_to_post_reserves(usdt, &meta, &post_reverse);
        assert!((pin_r - 11_000_000.0).abs() < 1e-6);
        assert!((pout_r - 9_900_000.0).abs() < 1e-6);
    }

    // ----- unified_to_post_reserves Bancor arm -----

    #[test]
    fn unified_to_post_reserves_bancor_uses_predictor_balances_directly() {
        use aether_common::types::PoolId;
        use aether_pools::bancor::BancorPostState;
        use aether_pools::UnifiedPostState;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bnt = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");
        let meta = PoolMetadata {
            token0_idx: 0,
            token1_idx: 1,
            token0: weth,
            token1: bnt,
            pool_id: PoolId {
                address: Address::ZERO,
                protocol: ProtocolType::BancorV3,
            },
            protocol: ProtocolType::BancorV3,
            fee_bps: 30,
            tick_spacing: None,
        };
        // Predictor aligns new_balance_in/out to swap direction — helper
        // trusts them directly regardless of swap_token_in vs meta.token0/1.
        let post = UnifiedPostState::Bancor(BancorPostState {
            new_balance_in: U256::from(1_010_000_000_000_000_000_000u128),
            new_balance_out: U256::from(1_980_198_019_801_980_198_018u128),
            amount_out: U256::from(19_801_980_198_019_801_982u128),
            analytical: true,
        });
        let (pin, pout) = unified_to_post_reserves(weth, &meta, &post);
        assert!((pin - 1_010_000_000_000_000_000_000.0).abs() < 1e6);
        assert!((pout - 1_980_198_019_801_980_198_018.0).abs() < 1e6);
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
            pool_address: None,
            one_inch_zero_for_one: None,
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

    fn fake_pending_event(to: Address) -> PendingTxEvent {
        PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: Some(to),
            value: U256::ZERO,
            input: vec![],
            gas_price: 0,
            first_seen_unix_nanos: 0,
            raw_tx: vec![],
        }
    }

    /// Build a synthetic V3 `PoolState` for use as the
    /// `try_post_state_replay` `pool_state` parameter in tests. The V3
    /// variant carries no pool-state fields the V3 reader inspects, so
    /// `Default::default()`-shaped values are sufficient.
    fn synthetic_v3_pool_state() -> PoolState {
        use aether_pools::uniswap_v3::UniswapV3Pool;
        PoolState::UniswapV3(UniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            30,
            60,
        ))
    }

    /// Build a synthetic 2-coin Curve `PoolState`. Tokens populated so
    /// the dispatcher can resolve `(i, j)` from a swap direction.
    fn synthetic_curve_pool_state(tokens: [Address; 2]) -> PoolState {
        use aether_pools::curve::CurvePool;
        PoolState::Curve(CurvePool::new(
            address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            tokens.to_vec(),
            100,
            4,
        ))
    }

    /// Build a synthetic 80/20-weight Balancer `PoolState` so the
    /// dispatcher reads `bal.token0` / `bal.token1` cleanly.
    fn synthetic_balancer_pool_state(token0: Address, token1: Address) -> PoolState {
        use aether_pools::balancer::BalancerPool;
        PoolState::Balancer(BalancerPool::new(
            address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56"),
            token0,
            token1,
            200_000,
            800_000,
            10,
        ))
    }

    #[test]
    fn try_post_state_replay_dormant_when_flag_disabled() {
        let metrics = EngineMetrics::new();
        let ctx = empty_sim_ctx();
        assert!(!ctx.post_state_replay_enabled);
        let pool_state = synthetic_v3_pool_state();
        let result = try_post_state_replay(
            &metrics,
            &ctx,
            ReplayProtocol::UniswapV3,
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            &pool_state,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            &fake_pending_event(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")),
            18_000_000,
        );
        assert!(result.is_none());
        assert_eq!(
            metrics.mempool_post_state_replay_count("unimplemented_protocol"),
            1
        );
        assert_eq!(metrics.mempool_post_state_replay_count("success"), 0);
    }

    fn unwrap_empty_sim_ctx() -> SimContext {
        Arc::try_unwrap(empty_sim_ctx())
            .ok()
            .expect("single Arc owner")
    }

    fn dummy_backrun_cfg() -> BackrunValidatorConfig {
        BackrunValidatorConfig {
            executor_address: Address::ZERO,
            searcher_caller: Address::ZERO,
            profit_token: Address::ZERO,
            balance_slot: U256::ZERO,
            chain_id: 1,
            min_profit_wei: U256::ZERO,
            input_amount_wei: U256::ZERO,
            gas_price_gwei: 20.0,
            sim_semaphore: Arc::new(Semaphore::new(1)),
            provider: None,
            mempool_prewarm: Arc::new(ArcSwap::from_pointee(None)),
            executor_bytecode: None,
        }
    }

    #[test]
    fn try_post_state_replay_dormant_when_backrun_unconfigured() {
        let metrics = EngineMetrics::new();
        // Flag flipped on, but no BackrunValidatorConfig attached — the
        // function short-circuits at `cfg = ctx.backrun.as_ref()?`
        // without bumping success and without panicking.
        let ctx = Arc::new(unwrap_empty_sim_ctx().with_post_state_replay(true));
        let before = metrics.mempool_post_state_replay_count("success");
        let pool_state = synthetic_v3_pool_state();
        let result = try_post_state_replay(
            &metrics,
            &ctx,
            ReplayProtocol::UniswapV3,
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            &pool_state,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            &fake_pending_event(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")),
            18_000_000,
        );
        assert!(result.is_none());
        assert_eq!(metrics.mempool_post_state_replay_count("success"), before);
    }

    #[test]
    fn try_post_state_replay_curve_balancer_reach_provider_check() {
        let metrics = EngineMetrics::new();
        // Curve and Balancer are now wired into the replay path. With
        // `provider = None` on the backrun config, the dispatcher
        // short-circuits at `cfg.provider.as_ref()?` *before* it bumps
        // any metric — confirming the dispatch path reaches the
        // protocol-specific branch rather than being routed through the
        // older `unimplemented_protocol` exit. The unchanged
        // `unimplemented_protocol` counter is the explicit witness.
        let ctx = Arc::new(
            unwrap_empty_sim_ctx()
                .with_backrun_validator(dummy_backrun_cfg())
                .with_post_state_replay(true),
        );
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let curve_state = synthetic_curve_pool_state([usdc, usdt]);
        let bal_state = synthetic_balancer_pool_state(usdc, usdt);
        let cases: [(ReplayProtocol, &PoolState); 2] = [
            (ReplayProtocol::Curve, &curve_state),
            (ReplayProtocol::Balancer, &bal_state),
        ];
        for (proto, state) in cases {
            let before = metrics.mempool_post_state_replay_count("unimplemented_protocol");
            let before_sim = metrics.mempool_post_state_replay_count("sim_error");
            let result = try_post_state_replay(
                &metrics,
                &ctx,
                proto,
                address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
                state,
                usdc,
                usdt,
                &fake_pending_event(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")),
                18_000_000,
            );
            assert!(result.is_none());
            assert_eq!(
                metrics.mempool_post_state_replay_count("unimplemented_protocol"),
                before,
                "{proto:?} must not bump unimplemented_protocol"
            );
            assert_eq!(
                metrics.mempool_post_state_replay_count("sim_error"),
                before_sim,
                "{proto:?} provider-missing path must not bump sim_error either"
            );
        }
    }

    #[test]
    fn sim_context_mempool_prewarm_arcswap_round_trips() {
        use aether_simulator::fork::PrewarmedState;

        let ctx = empty_sim_ctx();
        // Starts empty until the background refresher lands its first
        // snapshot — validator path counts a `prewarm_miss` in this state.
        assert!(ctx.mempool_prewarm.load().is_none());

        let warm = Arc::new(PrewarmedState::default());
        ctx.mempool_prewarm
            .store(Arc::new(Some(Arc::clone(&warm))));
        let loaded = ctx.mempool_prewarm.load();
        assert!(loaded.is_some());
        // Round-trip pointer-equality: same Arc instance came back out.
        assert!(Arc::ptr_eq(loaded.as_ref().as_ref().unwrap(), &warm));
    }

    #[test]
    fn with_backrun_validator_shares_prewarm_handle() {
        use aether_simulator::fork::PrewarmedState;
        use std::str::FromStr;

        let ctx_inner = {
            use crate::mempool_writer::NoopMempoolSink;
            use aether_pools::new_pool_state_cache;
            use aether_state::price_graph::PriceGraph;
            SimContext::new(
                Arc::new(ArcSwap::from_pointee(HashMap::<Address, PoolMetadata>::new())),
                Arc::new(ArcSwap::from_pointee(TokenIndex::default())),
                Arc::new(SnapshotManager::new(PriceGraph::new(0))),
                BellmanFord::new(3, 1_000),
                new_pool_state_cache(),
                Arc::new(NoopMempoolSink::new()),
                None,
            )
        };

        // `cfg` arrives with a fresh independent ArcSwap (as if built by
        // `build_backrun_validator_config_from_env`); `with_backrun_validator`
        // must overwrite it with the SimContext's shared handle so the
        // refresher and the validator rotate the same snapshot.
        let cfg = BackrunValidatorConfig {
            executor_address: Address::from_str("0x00000000000000000000000000000000000000aa").unwrap(),
            searcher_caller: Address::ZERO,
            profit_token: Address::ZERO,
            balance_slot: U256::ZERO,
            chain_id: 1,
            min_profit_wei: U256::ZERO,
            input_amount_wei: U256::ZERO,
            gas_price_gwei: 20.0,
            sim_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            provider: None,
            mempool_prewarm: Arc::new(ArcSwap::from_pointee(None)),
            executor_bytecode: None,
        };

        let shared_handle = Arc::clone(&ctx_inner.mempool_prewarm);
        let ctx_inner = ctx_inner.with_backrun_validator(cfg);

        let cfg_handle = Arc::clone(
            &ctx_inner
                .backrun
                .as_ref()
                .expect("backrun cfg installed")
                .mempool_prewarm,
        );
        assert!(Arc::ptr_eq(&cfg_handle, &shared_handle));

        // Refresher stores → validator-side load sees it.
        ctx_inner
            .mempool_prewarm
            .store(Arc::new(Some(Arc::new(PrewarmedState::default()))));
        assert!(cfg_handle.load().is_some());
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

    // ----- Bancor multi-hop integration tests -----

    use aether_common::types::PoolId;
    use aether_pools::bancor::BancorPool;
    use aether_pools::{Pool, PoolState};
    use aether_state::price_graph::PriceGraph;
    use std::sync::Mutex as StdMutex;

    /// Test-only sink that records every prediction without touching
    /// Postgres. Mirrors the `CapturingSink` in `mempool_writer::tests`
    /// (which is module-private there) so the multi-hop tests can assert
    /// on the persisted rows directly.
    struct LocalCapturingSink {
        seen: StdMutex<Vec<crate::mempool_writer::NewMempoolPrediction>>,
    }
    impl LocalCapturingSink {
        fn new() -> Self {
            Self {
                seen: StdMutex::new(Vec::new()),
            }
        }
        fn snapshot(&self) -> Vec<crate::mempool_writer::NewMempoolPrediction> {
            self.seen.lock().expect("capturing sink poisoned").clone()
        }
    }
    impl crate::mempool_writer::MempoolPredictionSink for LocalCapturingSink {
        fn insert_prediction(&self, prediction: crate::mempool_writer::NewMempoolPrediction) {
            self.seen
                .lock()
                .expect("capturing sink poisoned")
                .push(prediction);
        }
    }

    fn bancor_pool_meta(
        addr: Address,
        token: Address,
        bnt: Address,
        token0_idx: usize,
        token1_idx: usize,
    ) -> (Address, PoolMetadata) {
        let pool_id = PoolId {
            address: addr,
            protocol: ProtocolType::BancorV3,
        };
        let meta = PoolMetadata {
            token0_idx,
            token1_idx,
            token0: token,
            token1: bnt,
            pool_id,
            protocol: ProtocolType::BancorV3,
            fee_bps: 30,
            tick_spacing: None,
        };
        (addr, meta)
    }

    /// Build a SimContext seeded with two Bancor pools (WETH/BNT and
    /// LINK/BNT) covering the multi-hop dispatch path. Token vertices:
    /// 0 = WETH, 1 = BNT, 2 = LINK.
    fn multihop_sim_ctx(
        prediction_sink: Arc<dyn crate::mempool_writer::MempoolPredictionSink>,
        leg_b_present: bool,
    ) -> Arc<SimContext> {
        use aether_pools::new_pool_state_cache;

        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let link = address!("514910771AF9Ca656af840dff83E8264EcF986CA");

        let mut token_index = TokenIndex::default();
        let weth_idx = token_index.get_or_insert(weth);
        let bnt_idx = token_index.get_or_insert(BNT_ADDRESS);
        let link_idx = token_index.get_or_insert(link);
        assert_eq!(weth_idx, 0);
        assert_eq!(bnt_idx, 1);
        assert_eq!(link_idx, 2);

        let leg_a_addr = address!("aaaa000000000000000000000000000000000001");
        let leg_b_addr = address!("aaaa000000000000000000000000000000000002");

        let (leg_a_pool_addr, leg_a_meta) =
            bancor_pool_meta(leg_a_addr, weth, BNT_ADDRESS, weth_idx, bnt_idx);
        let (leg_b_pool_addr, leg_b_meta) =
            bancor_pool_meta(leg_b_addr, link, BNT_ADDRESS, link_idx, bnt_idx);

        let mut registry: HashMap<Address, PoolMetadata> = HashMap::new();
        registry.insert(leg_a_pool_addr, leg_a_meta.clone());
        if leg_b_present {
            registry.insert(leg_b_pool_addr, leg_b_meta.clone());
        }

        // Price graph: 3 vertices, 4 directed edges (each pool contributes
        // forward + reverse). Reserves are chosen so the analytical
        // multi-hop predictor accepts the swap without rounding to zero.
        let mut graph = PriceGraph::new(3);
        let leg_a_id = leg_a_meta.pool_id;
        let leg_b_id = leg_b_meta.pool_id;
        graph.add_edge(
            weth_idx,
            bnt_idx,
            1.0,
            leg_a_id,
            leg_a_pool_addr,
            ProtocolType::BancorV3,
            U256::from(1u64),
        );
        graph.add_edge(
            bnt_idx,
            weth_idx,
            1.0,
            leg_a_id,
            leg_a_pool_addr,
            ProtocolType::BancorV3,
            U256::from(1u64),
        );
        graph.add_edge(
            link_idx,
            bnt_idx,
            1.0,
            leg_b_id,
            leg_b_pool_addr,
            ProtocolType::BancorV3,
            U256::from(1u64),
        );
        graph.add_edge(
            bnt_idx,
            link_idx,
            1.0,
            leg_b_id,
            leg_b_pool_addr,
            ProtocolType::BancorV3,
            U256::from(1u64),
        );
        // Seed real reserves so update_edge_from_reserves operates on a
        // live edge.
        graph.update_edge_from_reserves(
            weth_idx,
            bnt_idx,
            leg_a_id,
            1_000_000_000_000_000_000_000.0,
            2_000_000_000_000_000_000_000.0,
            0.997,
        );
        graph.update_edge_from_reserves(
            bnt_idx,
            weth_idx,
            leg_a_id,
            2_000_000_000_000_000_000_000.0,
            1_000_000_000_000_000_000_000.0,
            0.997,
        );
        graph.update_edge_from_reserves(
            link_idx,
            bnt_idx,
            leg_b_id,
            500_000_000_000_000_000_000.0,
            1_500_000_000_000_000_000_000.0,
            0.997,
        );
        graph.update_edge_from_reserves(
            bnt_idx,
            link_idx,
            leg_b_id,
            1_500_000_000_000_000_000_000.0,
            500_000_000_000_000_000_000.0,
            0.997,
        );

        let pool_states = new_pool_state_cache();
        let mut leg_a_pool =
            BancorPool::new(leg_a_pool_addr, weth, BNT_ADDRESS, 30);
        leg_a_pool.update_state(
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(2_000_000_000_000_000_000_000u128),
        );
        pool_states.insert(leg_a_pool_addr, Arc::new(PoolState::Bancor(leg_a_pool)));
        if leg_b_present {
            let mut leg_b_pool =
                BancorPool::new(leg_b_pool_addr, link, BNT_ADDRESS, 30);
            leg_b_pool.update_state(
                U256::from(500_000_000_000_000_000_000u128),
                U256::from(1_500_000_000_000_000_000_000u128),
            );
            pool_states.insert(leg_b_pool_addr, Arc::new(PoolState::Bancor(leg_b_pool)));
        }

        Arc::new(SimContext::new(
            Arc::new(ArcSwap::from_pointee(registry)),
            Arc::new(ArcSwap::from_pointee(token_index)),
            Arc::new(SnapshotManager::new(graph)),
            BellmanFord::new(3, 1_000),
            pool_states,
            prediction_sink,
            None,
        ))
    }

    fn bancor_multihop_swap() -> DecodedSwap {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let link = address!("514910771AF9Ca656af840dff83E8264EcF986CA");
        DecodedSwap {
            protocol: Protocol::BancorV3,
            router: Address::ZERO,
            token_in: weth,
            token_out: link,
            // 10 WETH — large enough that the after-fee amount survives the
            // 2000 BNT denominator without rounding to zero.
            amount_in: U256::from(10_000_000_000_000_000_000u128),
            amount_out_min: U256::ZERO,
            recipient: Address::ZERO,
            fee_bps: 0,
            path_extra: vec![],
            curve_indices: None,
            pool_address: None,
            one_inch_zero_for_one: None,
        }
    }

    fn bancor_pending_event() -> PendingTxEvent {
        PendingTxEvent {
            tx_hash: B256::ZERO,
            from: Address::ZERO,
            to: Some(address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D")),
            value: U256::ZERO,
            input: vec![],
            gas_price: 0,
            first_seen_unix_nanos: 0,
            raw_tx: vec![],
        }
    }

    #[test]
    fn bancor_multihop_emits_two_predictions_with_distinct_pool_addresses() {
        let metrics = EngineMetrics::new();
        let sink = Arc::new(LocalCapturingSink::new());
        let ctx = multihop_sim_ctx(sink.clone(), /*leg_b_present=*/ true);
        let swap = bancor_multihop_swap();
        let event = bancor_pending_event();
        try_post_state_scan_bancor_multihop(
            &metrics,
            &ctx,
            "router",
            &swap,
            event.tx_hash,
            event.to.unwrap(),
            &event,
        );
        let rows = sink.snapshot();
        // Two rows — one per affected pool.
        assert_eq!(rows.len(), 2, "expected 2 prediction rows, got {}", rows.len());
        // Both rows share the victim's tx hash.
        assert!(rows.iter().all(|r| r.pending_tx_hash == event.tx_hash));
        // Pool addresses are distinct (leg A vs leg B).
        let pools: std::collections::HashSet<_> =
            rows.iter().filter_map(|r| r.pool_address).collect();
        assert_eq!(
            pools.len(),
            2,
            "expected 2 distinct pool addresses, got {pools:?}"
        );
        // Both rows are tagged as Bancor predictions.
        for row in &rows {
            assert_eq!(row.protocol, PROTOCOL_BANCOR);
        }
    }

    #[test]
    fn bancor_multihop_skips_with_named_reason_when_second_pool_missing() {
        let metrics = EngineMetrics::new();
        let sink = Arc::new(LocalCapturingSink::new());
        // Drop the LINK/BNT pool from the registry — the leg B lookup
        // must fail under `bancor_second_pool_not_found` rather than
        // bumping the generic `pool_not_registered` skip.
        let ctx = multihop_sim_ctx(sink.clone(), /*leg_b_present=*/ false);
        let swap = bancor_multihop_swap();
        let event = bancor_pending_event();
        let before = metrics.pending_arb_sim_skipped_count("bancor_second_pool_not_found");
        try_post_state_scan_bancor_multihop(
            &metrics,
            &ctx,
            "router",
            &swap,
            event.tx_hash,
            event.to.unwrap(),
            &event,
        );
        assert_eq!(
            metrics.pending_arb_sim_skipped_count("bancor_second_pool_not_found"),
            before + 1,
            "bancor_second_pool_not_found must increment"
        );
        // No predictions written on the skip path.
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn bancor_multihop_skips_when_predictor_returns_none() {
        // Construct a SimContext whose leg A pool has zero reserves so the
        // analytical predictor short-circuits to `None` on the first leg.
        // The skip reason must be `bancor_multihop_low_confidence`, not a
        // single-leg label.
        let metrics = EngineMetrics::new();
        let sink = Arc::new(LocalCapturingSink::new());
        let ctx = multihop_sim_ctx(sink.clone(), /*leg_b_present=*/ true);
        // Replace leg A pool state with a zero-reserve pool to force the
        // predictor to bail. Reuse the registry's known leg A address.
        let leg_a_addr = address!("aaaa000000000000000000000000000000000001");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let empty_pool = BancorPool::new(leg_a_addr, weth, BNT_ADDRESS, 30);
        ctx.pool_states
            .insert(leg_a_addr, Arc::new(PoolState::Bancor(empty_pool)));

        let swap = bancor_multihop_swap();
        let event = bancor_pending_event();
        let before =
            metrics.pending_arb_sim_skipped_count("bancor_multihop_low_confidence");
        try_post_state_scan_bancor_multihop(
            &metrics,
            &ctx,
            "router",
            &swap,
            event.tx_hash,
            event.to.unwrap(),
            &event,
        );
        assert_eq!(
            metrics.pending_arb_sim_skipped_count("bancor_multihop_low_confidence"),
            before + 1,
        );
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn pre_sim_filter_passes_bancor_multihop_when_both_pools_registered() {
        // Multi-hop pre-filter: both BNT pairs in the registry → pass.
        let metrics = EngineMetrics::new();
        let sink: Arc<dyn crate::mempool_writer::MempoolPredictionSink> =
            Arc::new(LocalCapturingSink::new());
        let ctx = multihop_sim_ctx(sink, /*leg_b_present=*/ true);
        let swap = bancor_multihop_swap();
        assert!(pre_sim_filter(&metrics, &ctx, &swap));
        // No drops attributed to `not_in_registry` on the happy path.
        assert_eq!(filtered_count(&metrics, "not_in_registry"), 0);
    }

    #[test]
    fn pre_sim_filter_drops_bancor_multihop_when_second_pool_absent() {
        // Multi-hop pre-filter: second pool missing → drop under
        // `not_in_registry` (the spawn_blocking + graph clone must be
        // skipped before the sim task starts).
        let metrics = EngineMetrics::new();
        let sink: Arc<dyn crate::mempool_writer::MempoolPredictionSink> =
            Arc::new(LocalCapturingSink::new());
        let ctx = multihop_sim_ctx(sink, /*leg_b_present=*/ false);
        let swap = bancor_multihop_swap();
        assert!(!pre_sim_filter(&metrics, &ctx, &swap));
        assert_eq!(filtered_count(&metrics, "not_in_registry"), 1);
    }

    /// The optimizer sizes a genuinely profitable two-venue WETH/USDC cycle
    /// to a positive net-profit input (exercises pool resolution, the AMM
    /// profit_fn, ternary search, per-hop expected-out, and the gate).
    #[test]
    fn optimizer_sizes_profitable_two_venue_v2_arb() {
        use aether_pools::new_pool_state_cache;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let pool_a = address!("00000000000000000000000000000000000000A1");
        let pool_b = address!("00000000000000000000000000000000000000B2");
        let (t0, t1) = if weth < usdc { (weth, usdc) } else { (usdc, weth) };
        let mk = |addr, weth_res: u128, usdc_res: u128| {
            let mut p = UniswapV2Pool::new(addr, t0, t1, 30);
            let (r0, r1) = if t0 == weth {
                (weth_res, usdc_res)
            } else {
                (usdc_res, weth_res)
            };
            p.update_state(U256::from(r0), U256::from(r1));
            p
        };
        // Pool A quotes ~2100 USDC/WETH, Pool B ~2000 — a ~5% gap that clears
        // the 2x0.3% pool fees + 9bps premium for a WETH->USDC->WETH loop.
        let pa = mk(pool_a, 1_000_000_000_000_000_000_000u128, 2_100_000_000_000u128);
        let pb = mk(pool_b, 1_000_000_000_000_000_000_000u128, 2_000_000_000_000u128);
        let pool_states = new_pool_state_cache();
        pool_states.insert(pool_a, std::sync::Arc::new(PoolState::UniswapV2(pa)));
        pool_states.insert(pool_b, std::sync::Arc::new(PoolState::UniswapV2(pb)));
        let mk_step = |pool, ti, to| aether_common::types::SwapStep {
            protocol: ProtocolType::UniswapV2,
            pool_address: pool,
            token_in: ti,
            token_out: to,
            amount_in: U256::ZERO,
            min_amount_out: U256::ZERO,
            calldata: Vec::new(),
        };
        let steps = vec![mk_step(pool_a, weth, usdc), mk_step(pool_b, usdc, weth)];
        let outcome = optimize_cycle_input(
            &steps,
            &pool_states,
            Address::ZERO, // no victim overlay
            weth,
            0.0,
            0.0,
            1.0,                            // 1 gwei gas
            U256::from(1_000_000_000u64),   // trivial min-profit floor
        );
        let SizingOutcome::Sized(sizing) = outcome else {
            panic!("expected Sized for a profitable two-venue arb");
        };
        assert!(sizing.net_profit_wei > 0, "net profit must be positive");
        assert_eq!(sizing.per_hop_expected_out.len(), 2);
        assert!(sizing.input_amount > U256::ZERO);
        // Final-hop expected out must exceed the input (gross > input).
        assert!(sizing.per_hop_expected_out[1] > sizing.input_amount);
    }

    /// A non-V2/Sushi hop is now exact-sized via its own `get_amount_out`
    /// instead of a blind fixed-size fallback. A degenerate single-hop
    /// weth->usdc "cycle" is correctly found unprofitable (gross in 6-dec USDC
    /// is numerically far below the 18-dec WETH input), so the optimizer drops
    /// it as below-min-profit rather than mis-sizing or falling back.
    #[test]
    fn optimizer_sizes_non_v2_cycle_and_drops_unprofitable() {
        use aether_pools::new_pool_state_cache;
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let pool_states = new_pool_state_cache();
        pool_states.insert(pool, std::sync::Arc::new(synthetic_v3_pool_state()));
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let steps = vec![aether_common::types::SwapStep {
            protocol: ProtocolType::UniswapV3,
            pool_address: pool,
            token_in: weth,
            token_out: usdc,
            amount_in: U256::from(1u64),
            min_amount_out: U256::ZERO,
            calldata: Vec::new(),
        }];
        let outcome = optimize_cycle_input(
            &steps,
            &pool_states,
            Address::ZERO,
            weth,
            0.0,
            0.0,
            1.0,
            U256::ZERO,
        );
        assert!(matches!(outcome, SizingOutcome::BelowMinProfit));
    }
}
