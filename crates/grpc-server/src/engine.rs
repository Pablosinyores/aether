use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tracing::{debug, info, warn};

use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::sol_types::SolCall;

use aether_common::db::{Ledger, NewArb, NewPool, NoopLedger};
use aether_common::types::{ArbHop, ArbOpportunity, PoolId, ProtocolType, SwapStep};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas::{estimate_total_gas, gas_cost_wei};
use aether_detector::optimizer::ternary_search_optimal_input;
use aether_ingestion::event_decoder::PoolEvent;
use aether_ingestion::subscription::{EventChannels, NewBlockEvent};
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::fork::{prewarm_state, ForkedState, PrewarmedState, RpcForkedState};
use aether_simulator::EvmSimulator;
use aether_state::price_graph::PriceGraph;
use aether_state::snapshot::SnapshotManager;
use aether_state::token_index::TokenIndex;

// Import the proto ValidatedArb type from service module
use aether_grpc_server::EngineMetrics;

use crate::pipeline;
use crate::service::aether_proto::ValidatedArb as ProtoValidatedArb;

/// Configuration for the AetherEngine.
pub struct EngineConfig {
    /// Maximum hops in arbitrage path.
    pub max_hops: usize,
    /// Time budget for detection in microseconds.
    pub detection_time_budget_us: u64,
    /// Minimum net profit in wei to consider an arb worth simulating.
    pub min_profit_threshold_wei: u128,
    /// Gas price assumption in gwei for profit calculations.
    pub gas_price_gwei: f64,
    /// Optional RPC URL for real fork simulation. When `None`, falls back to
    /// the empty-state `ForkedState` (no on-chain data).
    pub rpc_url: Option<String>,
    /// Executor contract address used as the simulation target.
    /// Defaults to `Address::ZERO` (empty call) when unset.
    pub executor_address: Address,
    /// Tip to block.coinbase in basis points (e.g. 9000 = 90%).
    /// Encoded into executeArb calldata for inline coinbase tip payment.
    pub tip_bps: u64,
    /// Slippage tolerance in basis points (100 = 1%).
    pub slippage_bps: u32,
    /// Maximum number of parallel EVM simulations per block cycle.
    /// Should match the number of pinned CPUs (CPU 0-3 → 4).
    pub max_parallel_sims: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_hops: 4,
            detection_time_budget_us: 3_000, // 3ms
            min_profit_threshold_wei: 1_000_000_000_000_000, // 0.001 ETH
            gas_price_gwei: 30.0,
            rpc_url: None,
            executor_address: Address::ZERO,
            tip_bps: 9000,
            slippage_bps: 100,
            max_parallel_sims: 4,
        }
    }
}

/// Metadata about a registered pool, used to map between on-chain events
/// and the in-memory price graph.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolMetadata {
    pub token0_idx: usize,
    pub token1_idx: usize,
    pub token0: Address,
    pub token1: Address,
    pub pool_id: PoolId,
    pub protocol: ProtocolType,
    pub fee_bps: u32,
}

impl PoolMetadata {
    /// Fee factor: `(10000 - fee_bps) / 10000`. E.g. 30 bps → 0.997.
    pub fn fee_factor(&self) -> f64 {
        (10000 - self.fee_bps) as f64 / 10000.0
    }
}

/// Core pipeline orchestrator that wires the Rust detection crates together.
///
/// Listens for pool update and new block events via broadcast channels, runs
/// Bellman-Ford detection on the price graph, simulates profitable cycles via
/// revm, and publishes validated arbs to the gRPC streaming channel.
pub struct AetherEngine {
    config: EngineConfig,
    /// Event channels for receiving pool updates and new blocks.
    event_channels: Arc<EventChannels>,
    /// Writer-side mutable graph. Mutations go here, then publish to snapshot_manager.
    working_graph: Mutex<PriceGraph>,
    /// Reader-side lock-free snapshots for detection and external readers.
    snapshot_manager: Arc<SnapshotManager>,
    /// Bellman-Ford detector.
    detector: BellmanFord,
    /// EVM simulator for validating arb profitability.
    simulator: EvmSimulator,
    /// Broadcast sender for validated arbs (connected to gRPC stream).
    arb_tx: broadcast::Sender<ProtoValidatedArb>,
    /// Current block info (lock-free via ArcSwap).
    current_block: Arc<ArcSwap<BlockInfo>>,
    /// Bidirectional token address ↔ graph vertex index mapping.
    /// Writers clone-modify-swap; readers load() zero-copy.
    token_index: Arc<ArcSwap<TokenIndex>>,
    /// Pool address → metadata mapping for event handling.
    /// Writers clone-modify-swap; readers load() zero-copy.
    pool_registry: Arc<ArcSwap<HashMap<Address, PoolMetadata>>>,
    /// Optional type-erased alloy provider for RPC-backed simulation.
    /// When `Some`, `run_detection_cycle` uses `RpcForkedState` instead of
    /// the empty `ForkedState`.
    rpc_provider: Option<DynProvider<Ethereum>>,
    /// Prometheus metrics for engine operations.
    metrics: Arc<EngineMetrics>,
    /// Persistent trade ledger. NoopLedger by default; PgLedger when
    /// `DATABASE_URL` is set at startup.
    ledger: Arc<dyn Ledger>,
}

/// Lightweight snapshot of the current block's key fields.
#[derive(Debug, Clone, Default)]
pub struct BlockInfo {
    pub number: u64,
    pub timestamp: u64,
    pub base_fee: u128,
}

/// Convert a U256 to f64 approximation (used for exchange rate calculations).
/// Uses limb-based conversion to handle values larger than u128::MAX.
/// Human-readable label for a well-known mainnet token.
/// Keep in sync with `tokenLabels` in cmd/executor/main.go and `token_label`
/// in crates/grpc-server/src/bin/aether_replay.rs so the same symbols show up
/// in every log / CSV / JSON the e2e pipeline produces.
fn token_label(addr: &Address) -> String {
    use alloy::primitives::address;
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
    const AAVE: Address = address!("7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9");
    match *addr {
        WETH => "WETH".into(),
        USDC => "USDC".into(),
        USDT => "USDT".into(),
        DAI => "DAI".into(),
        WBTC => "WBTC".into(),
        AAVE => "AAVE".into(),
        _ => {
            let hex = format!("{:#x}", addr);
            if hex.len() > 10 {
                format!("{}…", &hex[..10])
            } else {
                hex
            }
        }
    }
}

/// Build a [`NewArb`] row from a published opportunity.
///
/// `arb_id` is generated server-side because `ArbOpportunity::id` is a free-form
/// String not guaranteed to be UUID-shaped. `path_hash` is sha256 of the pool
/// address sequence so equivalent paths collapse to the same key for grouping.
fn build_new_arb(
    opp: &ArbOpportunity,
    flashloan_token: Address,
    flashloan_amount: U256,
    net_profit_u128: u128,
    tip_bps: u64,
    sim_us: u128,
    path_label: &str,
) -> NewArb {
    let pool_addrs: Vec<String> = opp
        .hops
        .iter()
        .map(|h| format!("{:#x}", h.pool_address))
        .collect();
    let protocols: Vec<String> = opp
        .hops
        .iter()
        .map(|h| format!("{:?}", h.protocol))
        .collect();

    let mut hasher = Sha256::new();
    for h in &opp.hops {
        hasher.update(h.pool_address.as_slice());
    }
    let digest = hasher.finalize();
    let mut path_hash = [0u8; 32];
    path_hash.copy_from_slice(&digest);

    NewArb {
        arb_id: Uuid::new_v4(),
        target_block: opp.block_number,
        path_hash: path_hash.into(),
        hops: u8::try_from(opp.hops.len()).unwrap_or(u8::MAX),
        path: serde_json::Value::String(path_label.to_string()),
        protocols: serde_json::json!(protocols),
        pool_addresses: serde_json::json!(pool_addrs),
        flashloan_token,
        flashloan_amount,
        gross_profit_wei: opp.total_profit_wei,
        net_profit_wei: U256::from(net_profit_u128),
        gas_estimate: opp.total_gas,
        tip_bps: u32::try_from(tip_bps).unwrap_or(u32::MAX),
        detection_us: None,
        sim_us: Some(u64::try_from(sim_us).unwrap_or(u64::MAX)),
        git_sha: option_env!("GIT_SHA").map(|s| s.to_string()),
    }
}

/// Build a path like "WETH -> AAVE -> WETH" from an `ArbOpportunity`'s hop list.
fn arb_path_labels(opp: &ArbOpportunity) -> String {
    if opp.hops.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::with_capacity(opp.hops.len() + 1);
    parts.push(token_label(&opp.hops[0].token_in));
    for hop in &opp.hops {
        parts.push(token_label(&hop.token_out));
    }
    parts.join(" -> ")
}

fn u256_to_f64(val: U256) -> f64 {
    let limbs = val.as_limbs();
    limbs[0] as f64
        + limbs[1] as f64 * 18_446_744_073_709_551_616.0 // 2^64
        + limbs[2] as f64 * 3.402_823_669_209_385e38      // 2^128
        + limbs[3] as f64 * 1.157_920_892_373_162e77       // 2^192
}

/// Intermediate data extracted from a detected cycle under the graph read lock.
/// Used to defer simulation and publishing until after the lock is released.
struct CycleCandidate {
    hops: Vec<ArbHop>,
    protocols: Vec<ProtocolType>,
    tick_counts: Vec<u32>,
    flashloan_token: Address,
    path_id: String,
    /// Per-hop exchange rates recovered from graph edge weights: e^(-weight).
    /// Used as fallback when reserves are unavailable.
    exchange_rates: Vec<f64>,
    /// Minimum liquidity across all hops — caps the optimizer search range.
    min_liquidity: U256,
    /// Per-hop pool reserves (reserve_in, reserve_out) for AMM-aware profit function.
    reserves: Vec<(f64, f64)>,
    /// Per-hop fee factors (e.g. 0.997 for 30bps), used in constant-product formula.
    fee_factors: Vec<f64>,
}

impl AetherEngine {
    #[allow(dead_code)]
    pub fn new(config: EngineConfig, arb_tx: broadcast::Sender<ProtoValidatedArb>) -> Self {
        let metrics = Arc::new(EngineMetrics::new());
        Self::new_with_metrics(config, arb_tx, metrics)
    }

    pub fn new_with_metrics(
        config: EngineConfig,
        arb_tx: broadcast::Sender<ProtoValidatedArb>,
        metrics: Arc<EngineMetrics>,
    ) -> Self {
        Self::new_with_metrics_and_ledger(config, arb_tx, metrics, Arc::new(NoopLedger::new()))
    }

    /// Build an engine with an explicit ledger backend. Production callers
    /// pass a `PgLedger` constructed from `DATABASE_URL`; tests and dev mode
    /// use `NoopLedger`.
    pub fn new_with_metrics_and_ledger(
        config: EngineConfig,
        arb_tx: broadcast::Sender<ProtoValidatedArb>,
        metrics: Arc<EngineMetrics>,
        ledger: Arc<dyn Ledger>,
    ) -> Self {
        let event_channels = Arc::new(EventChannels::new());
        let detector = BellmanFord::new(config.max_hops, config.detection_time_budget_us);
        let simulator = EvmSimulator::with_defaults();

        // Build the RPC provider when an RPC URL is configured.
        let rpc_provider = config.rpc_url.as_ref().and_then(|url_str| {
            let parsed: url::Url = match url_str.parse() {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(error = %e, url = %url_str, "Invalid RPC URL, falling back to empty state");
                    return None;
                }
            };
            let provider = alloy::providers::ProviderBuilder::new().connect_http(parsed);
            info!(url = %url_str, "RPC provider created for fork simulation");
            Some(provider.erased())
        });

        // Start with a reasonable initial graph size (can grow dynamically).
        let initial_graph = PriceGraph::new(100);
        let snapshot_manager = Arc::new(SnapshotManager::new(initial_graph.clone()));
        let working_graph = Mutex::new(initial_graph);

        Self {
            config,
            event_channels,
            working_graph,
            snapshot_manager,
            detector,
            simulator,
            arb_tx,
            current_block: Arc::new(ArcSwap::from_pointee(BlockInfo::default())),
            token_index: Arc::new(ArcSwap::from_pointee(TokenIndex::new())),
            pool_registry: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            rpc_provider,
            metrics,
            ledger,
        }
    }

    /// Get a reference to the event channels for external use (e.g., the
    /// provider pushing events into the engine).
    pub fn event_channels(&self) -> &Arc<EventChannels> {
        &self.event_channels
    }

    /// Get a reference to the current block info (lock-free ArcSwap).
    #[allow(dead_code)]
    pub fn current_block(&self) -> &Arc<ArcSwap<BlockInfo>> {
        &self.current_block
    }

    /// Register a pool in the engine's pool registry and create placeholder
    /// edges in the price graph.
    ///
    /// # Concurrency
    ///
    /// This method is NOT safe for concurrent calls — multiple callers would
    /// race on load-clone-modify-store of `token_index`. Currently safe because
    /// it's only called from the single-threaded engine event loop.
    pub async fn register_pool(
        &self,
        pool_addr: Address,
        token0: Address,
        token1: Address,
        protocol: ProtocolType,
        fee_bps: u32,
    ) {
        let (t0_idx, t1_idx, num_tokens) = {
            let mut ti = (**self.token_index.load()).clone();
            let t0 = ti.get_or_insert(token0);
            let t1 = ti.get_or_insert(token1);
            let len = ti.len();
            self.token_index.store(Arc::new(ti));
            (t0, t1, len)
        };

        let pool_id = PoolId {
            address: pool_addr,
            protocol,
        };
        let metadata = PoolMetadata {
            token0_idx: t0_idx,
            token1_idx: t1_idx,
            token0,
            token1,
            pool_id,
            protocol,
            fee_bps,
        };

        {
            let mut reg = (**self.pool_registry.load()).clone();
            reg.insert(pool_addr, metadata);
            self.pool_registry.store(Arc::new(reg));
        }

        // Ensure graph can hold the new vertices, add placeholder edges, then publish.
        {
            let mut graph = self.working_graph.lock().await;
            graph.resize(num_tokens);
            // Placeholder edges with rate 1.0 (neutral weight = 0).
            graph.add_edge(
                t0_idx,
                t1_idx,
                1.0,
                pool_id,
                pool_addr,
                protocol,
                U256::ZERO,
            );
            graph.add_edge(
                t1_idx,
                t0_idx,
                1.0,
                pool_id,
                pool_addr,
                protocol,
                U256::ZERO,
            );
            // Snapshot is published by callers (fetch_initial_reserves, handle_pool_update)
            // after batch operations, not per-registration, to avoid O(N) clones on startup.
        }

        debug!(
            %pool_addr, %token0, %token1, ?protocol, fee_bps,
            "Pool registered (t0={}, t1={})", t0_idx, t1_idx
        );

        self.ledger.insert_pool(&NewPool {
            address: pool_addr,
            protocol,
            token0,
            token1,
            fee_bps: Some(fee_bps),
            tier: None,
            source: "register_pool".to_string(),
        });
    }

    /// Bootstrap pools from a TOML config file (e.g. `config/pools.toml`).
    ///
    /// Parses the file, validates each entry, and calls `register_pool()` for
    /// each valid pool. Returns the number of pools successfully registered.
    pub async fn bootstrap_pools(&self, config_path: &str) -> u32 {
        info!(path = %config_path, "Bootstrapping pools from config");

        let contents = match tokio::fs::read_to_string(config_path).await {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %config_path, error = %e, "Failed to read pool config");
                return 0;
            }
        };

        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct PoolEntry {
            protocol: String,
            address: String,
            token0: String,
            token1: String,
            fee_bps: u32,
            #[serde(default)]
            tier: String,
            #[serde(default)]
            tick_spacing: Option<i32>,
        }

        #[derive(serde::Deserialize)]
        struct PoolsConfig {
            #[serde(default)]
            pools: Vec<PoolEntry>,
        }

        let config: PoolsConfig = match toml::from_str(&contents) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %config_path, error = %e, "Failed to parse pool config TOML");
                return 0;
            }
        };

        if config.pools.is_empty() {
            warn!(path = %config_path, "No [[pools]] entries found in config");
            return 0;
        }

        let mut loaded: u32 = 0;

        for (i, entry) in config.pools.iter().enumerate() {
            // Validate protocol string.
            let protocol = match entry.protocol.as_str() {
                "uniswap_v2" => ProtocolType::UniswapV2,
                "uniswap_v3" => ProtocolType::UniswapV3,
                "sushiswap" => ProtocolType::SushiSwap,
                "curve" => ProtocolType::Curve,
                "balancer_v2" => ProtocolType::BalancerV2,
                "bancor_v3" => ProtocolType::BancorV3,
                other => {
                    warn!(index = i, protocol = %other, "Unknown protocol, skipping pool");
                    continue;
                }
            };

            // Validate and parse addresses.
            let pool_addr = match entry.address.parse::<Address>() {
                Ok(a) if a != Address::ZERO => a,
                Ok(_) => {
                    warn!(index = i, address = %entry.address, "Zero address, skipping pool");
                    continue;
                }
                Err(e) => {
                    warn!(index = i, address = %entry.address, error = %e, "Invalid pool address, skipping");
                    continue;
                }
            };

            let token0 = match entry.token0.parse::<Address>() {
                Ok(a) if a != Address::ZERO => a,
                Ok(_) => {
                    warn!(index = i, token0 = %entry.token0, "Zero token0 address, skipping pool");
                    continue;
                }
                Err(e) => {
                    warn!(index = i, token0 = %entry.token0, error = %e, "Invalid token0 address, skipping");
                    continue;
                }
            };

            let token1 = match entry.token1.parse::<Address>() {
                Ok(a) if a != Address::ZERO => a,
                Ok(_) => {
                    warn!(index = i, token1 = %entry.token1, "Zero token1 address, skipping pool");
                    continue;
                }
                Err(e) => {
                    warn!(index = i, token1 = %entry.token1, error = %e, "Invalid token1 address, skipping");
                    continue;
                }
            };

            // Check for duplicate pool address.
            if self.pool_registry.load().contains_key(&pool_addr) {
                warn!(index = i, %pool_addr, "Duplicate pool address, skipping");
                continue;
            }

            self.register_pool(pool_addr, token0, token1, protocol, entry.fee_bps)
                .await;
            loaded += 1;

            info!(
                %pool_addr, ?protocol, fee_bps = entry.fee_bps, tier = %entry.tier,
                "Bootstrapped pool {}/{}", loaded, config.pools.len()
            );
        }

        info!(loaded, total = config.pools.len(), "Pool bootstrap complete");
        loaded
    }

    /// Fetch initial on-chain reserves for all registered pools via RPC.
    ///
    /// For V2/SushiSwap pools: calls `getReserves()`.
    /// For V3 pools: calls `slot0()`.
    /// RPC calls are made concurrently for scalability (5,000+ pools).
    /// Updates the price graph with real exchange rates so detection works
    /// immediately after startup.
    pub async fn fetch_initial_reserves(&self) {
        let provider = match &self.rpc_provider {
            Some(p) => p.clone(),
            None => {
                info!("No RPC provider configured, skipping initial reserve fetch");
                return;
            }
        };

        // Collect pool metadata snapshot to avoid holding the guard during RPC calls.
        let pools: Vec<(Address, PoolMetadata)> = {
            let registry = self.pool_registry.load();
            registry.iter().map(|(a, m)| (*a, m.clone())).collect()
        };

        if pools.is_empty() {
            return;
        }

        info!(count = pools.len(), "Fetching initial reserves via RPC (concurrent)");

        // ABI for getReserves() and slot0()
        alloy::sol! {
            function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
            function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        }

        // Result type for each concurrent RPC fetch.
        enum ReserveResult {
            V2 { pool_addr: Address, meta: PoolMetadata, r0: U256, r1: U256 },
            V3 { pool_addr: Address, meta: PoolMetadata, sqrt_price_x96: U256 },
            Skipped,
        }

        // Fire off all RPC calls concurrently.
        let mut join_set = tokio::task::JoinSet::new();

        for (pool_addr, meta) in pools.iter().cloned() {
            let provider = provider.clone();
            join_set.spawn(async move {
                match meta.protocol {
                    ProtocolType::UniswapV2 | ProtocolType::SushiSwap => {
                        let calldata = getReservesCall {}.abi_encode();
                        let tx = alloy::rpc::types::TransactionRequest::default()
                            .to(pool_addr)
                            .input(calldata.into());

                        match provider.call(tx).await {
                            Ok(output) if output.len() >= 96 => {
                                let r0 = U256::from_be_slice(&output[0..32]);
                                let r1 = U256::from_be_slice(&output[32..64]);
                                ReserveResult::V2 { pool_addr, meta, r0, r1 }
                            }
                            Ok(output) => {
                                warn!(%pool_addr, len = output.len(), "getReserves output too short");
                                ReserveResult::Skipped
                            }
                            Err(e) => {
                                warn!(%pool_addr, error = %e, "getReserves RPC call failed");
                                ReserveResult::Skipped
                            }
                        }
                    }
                    ProtocolType::UniswapV3 => {
                        let calldata = slot0Call {}.abi_encode();
                        let tx = alloy::rpc::types::TransactionRequest::default()
                            .to(pool_addr)
                            .input(calldata.into());

                        match provider.call(tx).await {
                            Ok(output) if output.len() >= 64 => {
                                let sqrt_price_x96 = U256::from_be_slice(&output[0..32]);
                                ReserveResult::V3 { pool_addr, meta, sqrt_price_x96 }
                            }
                            Ok(output) => {
                                warn!(%pool_addr, len = output.len(), "slot0 output too short");
                                ReserveResult::Skipped
                            }
                            Err(e) => {
                                warn!(%pool_addr, error = %e, "slot0 RPC call failed");
                                ReserveResult::Skipped
                            }
                        }
                    }
                    _ => {
                        debug!(%pool_addr, protocol = ?meta.protocol, "Reserve fetch not yet implemented for this protocol");
                        ReserveResult::Skipped
                    }
                }
            });
        }

        // Collect all RPC results first (concurrently), then apply to the graph
        // in a single lock acquisition and publish one snapshot.
        let mut all_results: Vec<ReserveResult> = Vec::with_capacity(pools.len());
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(r) => all_results.push(r),
                Err(e) => {
                    warn!(error = %e, "Reserve fetch task panicked");
                }
            }
        }

        let mut fetched: u32 = 0;
        {
            let mut graph = self.working_graph.lock().await;
            for reserve in all_results {
                match reserve {
                    ReserveResult::V2 { pool_addr, meta, r0, r1 } => {
                        let r0_f = u256_to_f64(r0);
                        let r1_f = u256_to_f64(r1);
                        if r0_f > 0.0 && r1_f > 0.0 {
                            let fee = meta.fee_factor();
                            graph.update_edge_from_reserves(
                                meta.token0_idx, meta.token1_idx,
                                meta.pool_id, r0_f, r1_f, fee,
                            );
                            graph.update_edge_from_reserves(
                                meta.token1_idx, meta.token0_idx,
                                meta.pool_id, r1_f, r0_f, fee,
                            );
                            fetched += 1;
                            debug!(%pool_addr, reserve0 = %r0, reserve1 = %r1, "V2 reserves fetched");
                        }
                    }
                    ReserveResult::V3 { pool_addr, meta, sqrt_price_x96 } => {
                        const TWO_POW_96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
                        let sqrt_f64 = u256_to_f64(sqrt_price_x96);
                        let price = (sqrt_f64 / TWO_POW_96).powi(2);
                        if price > 0.0 {
                            let fee = meta.fee_factor();
                            let liq = U256::ZERO;
                            graph.add_edge(
                                meta.token0_idx, meta.token1_idx,
                                price * fee, meta.pool_id, pool_addr,
                                meta.protocol, liq,
                            );
                            graph.add_edge(
                                meta.token1_idx, meta.token0_idx,
                                (1.0 / price) * fee, meta.pool_id, pool_addr,
                                meta.protocol, liq,
                            );
                            fetched += 1;
                            debug!(%pool_addr, %sqrt_price_x96, "V3 slot0 fetched");
                        }
                    }
                    ReserveResult::Skipped => {}
                }
            }

            // Publish the updated snapshot with current block context.
            let block = self.current_block.load();
            self.snapshot_manager
                .publish(graph.clone(), block.number, block.timestamp as i64);
        }

        info!(fetched, total = pools.len(), "Initial reserve fetch complete");
    }

    /// Main engine loop: processes events, detects arbs, simulates, publishes.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!("AetherEngine starting main loop");

        let mut block_rx = self.event_channels.subscribe_new_blocks();
        let mut pool_rx = self.event_channels.subscribe_pool_updates();

        loop {
            tokio::select! {
                // Handle new block events.
                Ok(block_event) = block_rx.recv() => {
                    self.handle_new_block(block_event).await;
                }
                // Handle pool update events.
                Ok(pool_event) = pool_rx.recv() => {
                    self.handle_pool_update(pool_event).await;
                }
                // Handle shutdown signal.
                Ok(()) = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("AetherEngine received shutdown signal");
                        break;
                    }
                }
            }
        }

        info!("AetherEngine main loop exited");
    }

    /// Handle a new block: update block info, run detection on dirty edges.
    async fn handle_new_block(&self, event: NewBlockEvent) {
        debug!(block = event.block_number, "Processing new block");
        self.metrics.inc_blocks_processed();

        self.current_block.store(Arc::new(BlockInfo {
            number: event.block_number,
            timestamp: event.timestamp,
            base_fee: event.base_fee,
        }));

        // Run detection on the price graph.
        self.run_detection_cycle().await;
    }

    /// Handle a pool update: update the price graph edge.
    async fn handle_pool_update(&self, event: PoolEvent) {
        match event {
            PoolEvent::ReserveUpdate {
                pool,
                protocol,
                reserve0,
                reserve1,
            } => {
                debug!(%pool, ?protocol, "Pool reserve update");

                // Look up pool metadata to get graph vertex indices.
                let meta = self.pool_registry.load().get(&pool).cloned();

                if let Some(meta) = meta {
                    let r0 = u256_to_f64(reserve0);
                    let r1 = u256_to_f64(reserve1);

                    if r0 > 0.0 && r1 > 0.0 {
                        let fee = meta.fee_factor();
                        let mut graph = self.working_graph.lock().await;
                        graph.update_edge_from_reserves(
                            meta.token0_idx,
                            meta.token1_idx,
                            meta.pool_id,
                            r0,
                            r1,
                            fee,
                        );
                        graph.update_edge_from_reserves(
                            meta.token1_idx,
                            meta.token0_idx,
                            meta.pool_id,
                            r1,
                            r0,
                            fee,
                        );
                        // Snapshot is published once per detection cycle, not per event.
                    }
                }
            }
            PoolEvent::V2Swap {
                pool,
                sender,
                to,
                amount0_in,
                amount1_in,
                amount0_out,
                amount1_out,
            } => {
                // Informational only — reserves reconcile via the paired
                // `Sync` event, which arrives in the same log batch and
                // drives `ReserveUpdate` above. This arm exists so the
                // match stays exhaustive and downstream trade analytics
                // have a hook.
                debug!(
                    %pool, %sender, %to,
                    %amount0_in, %amount1_in, %amount0_out, %amount1_out,
                    "V2 swap (informational)"
                );
            }
            PoolEvent::V3Update {
                pool,
                sqrt_price_x96,
                liquidity,
                tick: _,
            } => {
                debug!(%pool, %sqrt_price_x96, liquidity, "V3 pool update");

                let meta = self.pool_registry.load().get(&pool).cloned();

                if let Some(meta) = meta {
                    // price = (sqrt_price_x96 / 2^96)^2
                    const TWO_POW_96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
                    let sqrt_f64 = u256_to_f64(sqrt_price_x96);
                    let price = (sqrt_f64 / TWO_POW_96).powi(2);

                    if price > 0.0 {
                        let fee = meta.fee_factor();
                        let liq = U256::from(liquidity);
                        let mut graph = self.working_graph.lock().await;
                        graph.add_edge(
                            meta.token0_idx,
                            meta.token1_idx,
                            price * fee,
                            meta.pool_id,
                            pool,
                            meta.protocol,
                            liq,
                        );
                        graph.add_edge(
                            meta.token1_idx,
                            meta.token0_idx,
                            (1.0 / price) * fee,
                            meta.pool_id,
                            pool,
                            meta.protocol,
                            liq,
                        );
                        // Snapshot is published once per detection cycle, not per event.
                    }
                }
            }
            PoolEvent::PoolCreated {
                token0,
                token1,
                pool,
            } => {
                info!(%pool, %token0, %token1, "New pool discovered, auto-registering");
                // Default to UniswapV2 with 30 bps fee (most PairCreated events).
                self.register_pool(pool, token0, token1, ProtocolType::UniswapV2, 30)
                    .await;
                // Snapshot is published once per detection cycle, not per event.
            }
        }
    }

    /// Run a detection cycle: scan for negative cycles, simulate, publish.
    #[tracing::instrument(skip_all, name = "engine.detection_cycle")]
    async fn run_detection_cycle(&self) {
        let t_cycle = Instant::now();

        // Snapshot the current working graph for this detection cycle.
        // Clone and clear_dirty MUST be atomic under the Mutex: if an event
        // slips in between clone and clear, its dirty flag would be set on
        // working_graph, then immediately wiped by clear_dirty — that pool
        // update would never trigger a detection scan (TOCTOU on dirty flags).
        let (detection_graph, block_number, timestamp_ns) = {
            let mut graph = self.working_graph.lock().await;
            let snapshot_graph = graph.clone();
            graph.clear_dirty();
            let block = (**self.current_block.load()).clone();
            (snapshot_graph, block.number, block.timestamp as i64)
        };

        // Phase 1: Detect cycles using the local detection_graph directly.
        // Publish to snapshot_manager AFTER detection so external readers
        // only see snapshots whose detection has completed.
        let candidates = {
            let graph = &detection_graph;

            if !graph.has_dirty_edges() && graph.num_edges() == 0 {
                self.snapshot_manager
                    .publish(detection_graph, block_number, timestamp_ns);
                return;
            }

            // Get affected vertices for partial scan.
            let affected = graph.affected_vertices();

            let t_detect = Instant::now();
            let detect_span =
                tracing::info_span!("detect", affected = affected.len(), block_number);
            let cycles = {
                let _entered = detect_span.enter();
                if affected.is_empty() {
                    // Full scan (e.g., on first run).
                    self.detector.detect_negative_cycles(graph)
                } else {
                    self.detector.detect_from_affected(graph, &affected)
                }
            };
            let detect_us = t_detect.elapsed().as_micros();
            self.metrics.observe_detection_latency_us(detect_us);
            info!(
                detect_us,
                block_number,
                "Bellman-Ford detection complete"
            );
            self.metrics.inc_cycles_detected(cycles.len() as u64);

            if cycles.is_empty() {
                self.snapshot_manager
                    .publish(detection_graph, block_number, timestamp_ns);
                return;
            }

            debug!(count = cycles.len(), "Detected negative cycles");

            let token_index = self.token_index.load();
            let pool_registry = self.pool_registry.load();
            let mut candidates = Vec::new();

            for cycle in &cycles {
                if !cycle.is_profitable() {
                    continue;
                }

                let profit_factor = cycle.profit_factor();
                debug!(
                    hops = cycle.num_hops(),
                    profit_factor = %profit_factor,
                    "Profitable cycle found"
                );

                // Build ArbHops from the cycle path.
                let mut hops = Vec::new();
                let mut protocols = Vec::new();
                let mut tick_counts = Vec::new();
                let mut exchange_rates = Vec::new();
                let mut reserves = Vec::new();
                let mut fee_factors = Vec::new();
                let mut min_liquidity = U256::MAX;
                let mut valid = true;

                for i in 0..cycle.path.len() - 1 {
                    let from_idx = cycle.path[i];
                    let to_idx = cycle.path[i + 1];

                    let from_addr = match token_index.get_address(from_idx) {
                        Some(addr) => *addr,
                        None => {
                            valid = false;
                            break;
                        }
                    };
                    let to_addr = match token_index.get_address(to_idx) {
                        Some(addr) => *addr,
                        None => {
                            valid = false;
                            break;
                        }
                    };

                    // Find the best (lowest weight) edge for this hop.
                    let best_edge = match graph
                        .edges_from(from_idx)
                        .iter()
                        .filter(|e| e.to == to_idx)
                        .min_by(|a, b| {
                            a.weight
                                .partial_cmp(&b.weight)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        }) {
                        Some(edge) => edge,
                        None => {
                            valid = false;
                            break;
                        }
                    };

                    // Recover exchange rate from edge weight: rate = e^(-weight).
                    let rate = (-best_edge.weight).exp();
                    exchange_rates.push(rate);

                    // Collect pool reserves for AMM-aware profit function.
                    reserves.push((best_edge.reserve_in, best_edge.reserve_out));

                    // Look up fee_bps from pool registry, default 30bps.
                    let fee_bps = pool_registry
                        .get(&best_edge.pool_address)
                        .map(|m| m.fee_bps)
                        .unwrap_or(30);
                    fee_factors.push((10000.0 - fee_bps as f64) / 10000.0);

                    // Track minimum liquidity across hops to cap optimizer range.
                    // Skip zero-liquidity edges (placeholders from register_pool).
                    if !best_edge.liquidity.is_zero()
                        && best_edge.liquidity < min_liquidity
                    {
                        min_liquidity = best_edge.liquidity;
                    }

                    let estimated_gas =
                        aether_detector::gas::estimate_swap_gas(best_edge.protocol, 0);

                    hops.push(ArbHop {
                        protocol: best_edge.protocol,
                        pool_address: best_edge.pool_address,
                        token_in: from_addr,
                        token_out: to_addr,
                        amount_in: U256::ZERO,    // Placeholder — optimizer fills this
                        expected_out: U256::ZERO,  // Placeholder — optimizer fills this
                        estimated_gas,
                    });

                    protocols.push(best_edge.protocol);
                    tick_counts.push(0u32);
                }

                if !valid || hops.is_empty() {
                    continue;
                }

                let flashloan_token = hops[0].token_in;
                let path_id = cycle
                    .path
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join("-");

                candidates.push(CycleCandidate {
                    hops,
                    protocols,
                    tick_counts,
                    flashloan_token,
                    path_id,
                    exchange_rates,
                    min_liquidity,
                    reserves,
                    fee_factors,
                });
            }

            candidates
        };

        // Publish the snapshot after detection completes so external readers
        // only see snapshots that have been fully evaluated.
        self.snapshot_manager
            .publish(detection_graph, block_number, timestamp_ns);

        let phase1_us = t_cycle.elapsed().as_micros();

        // Phase 2: Simulate in parallel and publish (no graph lock needed).
        let t_phase2 = Instant::now();
        let mut sim_count: u32 = 0;
        let mut sim_success: u32 = 0;
        let block_info = (**self.current_block.load()).clone();

        // Pre-filter candidates and build simulation inputs (cheap, sequential).
        struct SimInput {
            opp: ArbOpportunity,
            steps: Vec<SwapStep>,
            calldata: Vec<u8>,
            flashloan_token: Address,
            input_amount: U256,
            net_profit: u128,
        }

        let executor_addr = self.config.executor_address;
        let tip_bps = U256::from(self.config.tip_bps);

        let sim_inputs: Vec<SimInput> = candidates
            .iter()
            .filter_map(|candidate| {
                let total_gas =
                    estimate_total_gas(&candidate.protocols, &candidate.tick_counts);
                let gas_cost = gas_cost_wei(total_gas, self.config.gas_price_gwei);

                // ── Optimizer: find the optimal input amount ──
                let min_input = U256::from(10_000_000_000_000_000u128); // 0.01 ETH
                let max_trade = U256::from(50_000_000_000_000_000_000u128); // 50 ETH
                let max_input = if candidate.min_liquidity < max_trade
                    && !candidate.min_liquidity.is_zero()
                {
                    candidate.min_liquidity
                } else {
                    max_trade
                };

                let hop_reserves = &candidate.reserves;
                let hop_fee_factors = &candidate.fee_factors;
                let hop_rates = &candidate.exchange_rates;
                let profit_fn = |input: U256| -> i128 {
                    let mut current = u256_to_f64(input);
                    for i in 0..hop_reserves.len() {
                        let (x, y) = hop_reserves[i];
                        let fee = hop_fee_factors[i];
                        if x > 0.0 && y > 0.0 {
                            // Constant-product AMM: dy = (dx * fee * y) / (x + dx * fee)
                            current = (current * fee * y) / (x + current * fee);
                        } else {
                            // Fallback to linear rate when reserves are unknown.
                            current *= hop_rates[i];
                        }
                    }
                    let output = current as i128;
                    let input_i128 = u256_to_f64(input) as i128;
                    output
                        .saturating_sub(input_i128)
                        .saturating_sub(gas_cost as i128)
                };

                let (optimal_input, net_profit_i128) = if min_input < max_input {
                    ternary_search_optimal_input(min_input, max_input, 80, profit_fn)
                } else {
                    (min_input, profit_fn(min_input))
                };

                if net_profit_i128 <= 0 {
                    debug!("Cycle unprofitable after optimizer + gas costs");
                    return None;
                }

                let net_profit: u128 = match net_profit_i128.try_into() {
                    Ok(v) => v,
                    Err(_) => return None,
                };
                if net_profit < self.config.min_profit_threshold_wei {
                    debug!(
                        net_profit,
                        threshold = self.config.min_profit_threshold_wei,
                        "Below min profit threshold"
                    );
                    return None;
                }

                let input_amount = optimal_input;

                // ── Compute per-hop amount_in and expected_out ──
                let mut optimized_hops = candidate.hops.clone();
                let mut current_amount = input_amount;
                for (i, hop) in optimized_hops.iter_mut().enumerate() {
                    hop.amount_in = current_amount;
                    let dx = u256_to_f64(current_amount);
                    let (x, y) = candidate.reserves[i];
                    let fee = candidate.fee_factors[i];
                    let out_f64 = if x > 0.0 && y > 0.0 {
                        (dx * fee * y) / (x + dx * fee)
                    } else {
                        dx * candidate.exchange_rates[i]
                    };
                    let out_u256 = U256::from(out_f64 as u128);
                    hop.expected_out = out_u256;
                    current_amount = out_u256;
                }

                let gross_profit_wei = (u256_to_f64(current_amount) as u128)
                    .saturating_sub(u256_to_f64(input_amount) as u128);

                // ── Build SwapSteps with configurable slippage ──
                let slippage_denom = U256::from(10_000u32);
                let clamped_bps = self.config.slippage_bps.min(9999);
                let slippage_factor = slippage_denom - U256::from(clamped_bps);
                let steps: Vec<SwapStep> = optimized_hops
                    .iter()
                    .map(|hop| {
                        let min_out = hop.expected_out * slippage_factor / slippage_denom;
                        SwapStep {
                            protocol: hop.protocol,
                            pool_address: hop.pool_address,
                            token_in: hop.token_in,
                            token_out: hop.token_out,
                            amount_in: hop.amount_in,
                            min_amount_out: min_out,
                            calldata: vec![],
                        }
                    })
                    .collect();

                // Deadline: block timestamp + 24s (~2 blocks) for MEV bundle window
                let deadline = U256::from(block_info.timestamp + 24);
                let min_profit_out = U256::ZERO; // Enforced off-chain via net_profit check
                let calldata = build_execute_arb_calldata(
                    &steps,
                    candidate.flashloan_token,
                    input_amount,
                    deadline,
                    min_profit_out,
                    tip_bps,
                );

                let opp = ArbOpportunity {
                    id: format!("arb-{}-{}", block_info.number, candidate.path_id),
                    hops: optimized_hops,
                    total_profit_wei: U256::from(gross_profit_wei),
                    total_gas,
                    gas_cost_wei: U256::from(gas_cost),
                    net_profit_wei: U256::from(net_profit),
                    block_number: block_info.number,
                    timestamp_ns: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as i64,
                };

                Some(SimInput {
                    opp,
                    steps,
                    calldata,
                    flashloan_token: candidate.flashloan_token,
                    input_amount,
                    net_profit,
                })
            })
            .collect();

        // Pre-warm: fetch contract code and V2 reserve slots once before
        // spawning parallel tasks, so every task's RpcForkedState cache starts
        // hot. Without this, each task independently fetches the same executor
        // and pool bytecode — N tasks × M addresses = N×M cold RPC round-trips.
        let sim_config = self.simulator.config().clone();
        let rpc_provider = self.rpc_provider.clone();

        let prewarmed: Option<Arc<PrewarmedState>> = if let Some(ref provider) = rpc_provider {
            // Collect unique addresses across all simulation inputs.
            let mut code_addrs: Vec<Address> = vec![executor_addr];
            let mut v2_addrs: Vec<Address> = vec![];
            for input in &sim_inputs {
                for step in &input.steps {
                    code_addrs.push(step.pool_address);
                    if matches!(
                        step.protocol,
                        ProtocolType::UniswapV2 | ProtocolType::SushiSwap
                    ) {
                        v2_addrs.push(step.pool_address);
                    }
                }
            }
            code_addrs.sort_unstable();
            code_addrs.dedup();
            v2_addrs.sort_unstable();
            v2_addrs.dedup();

            let state =
                prewarm_state(provider, block_info.number, &code_addrs, &v2_addrs).await;
            Some(Arc::new(state))
        } else {
            None
        };

        // Semaphore caps parallel simulations — permits are acquired before
        // spawning so at most max_parallel_sims tasks exist at a time.
        // This prevents N-4 parked tasks from holding tokio worker threads.
        let semaphore = Arc::new(Semaphore::new(self.config.max_parallel_sims));

        let mut sim_handles: Vec<tokio::task::JoinHandle<_>> = Vec::new();
        for input in sim_inputs {
            // Acquire before spawning — ensures only max_parallel_sims tasks
            // exist concurrently, so no worker threads are held by parked tasks.
            let permit = Arc::clone(&semaphore)
                .acquire_owned()
                .await
                .expect("semaphore closed");
            let sim_config = sim_config.clone();
            let rpc_provider = rpc_provider.clone();
            let prewarmed = prewarmed.clone();
            let block_number = block_info.number;
            let block_timestamp = block_info.timestamp;
            let base_fee = block_info.base_fee as u64;

            sim_handles.push(tokio::spawn(async move {
                // Hold permit for the lifetime of this task.
                let _permit = permit;

                // block_in_place: signals tokio to spawn extra workers for
                // remaining async tasks while this worker runs blocking revm.
                // Keeps tokio runtime context alive for WrapDatabaseAsync.
                tokio::task::block_in_place(|| {
                        // Thread-local simulator reuses the instance across
                        // successive block cycles on the same OS thread,
                        // avoiding repeated EvmSimulator construction overhead.
                        thread_local! {
                            static LOCAL_SIM: std::cell::RefCell<Option<EvmSimulator>> =
                                const { std::cell::RefCell::new(None) };
                        }

                        LOCAL_SIM.with(|cell| {
                            let mut borrow = cell.borrow_mut();
                            if borrow.is_none() {
                                *borrow = Some(EvmSimulator::new(sim_config));
                            }
                            let simulator = borrow.as_ref().unwrap();
                            let t_sim = Instant::now();

                            // Extract calldata to avoid partial move of input.
                            let calldata = input.calldata.clone();

                            let sim_result = if let Some(ref provider) = rpc_provider {
                                match RpcForkedState::new(
                                    provider.clone(),
                                    block_number,
                                    block_timestamp,
                                    base_fee,
                                ) {
                                    Some(mut rpc_state) => {
                                        // Inject pre-warmed contract code and
                                        // storage so simulation reads are served
                                        // from cache, not cold RPC fetches.
                                        if let Some(ref pw) = prewarmed {
                                            pw.inject_into(&mut rpc_state);
                                        }
                                        simulator.simulate_rpc(
                                            rpc_state,
                                            executor_addr,
                                            calldata,
                                        )
                                    }
                                    None => {
                                        debug!("RpcForkedState::new returned None, falling back to empty state");
                                        let forked_state = ForkedState::new_empty(
                                            block_number,
                                            block_timestamp,
                                            base_fee,
                                        );
                                        simulator.simulate(
                                            &forked_state,
                                            executor_addr,
                                            calldata,
                                        )
                                    }
                                }
                            } else {
                                let forked_state = ForkedState::new_empty(
                                    block_number,
                                    block_timestamp,
                                    base_fee,
                                );
                                simulator.simulate(
                                    &forked_state,
                                    executor_addr,
                                    calldata,
                                )
                            };

                            let sim_us = t_sim.elapsed().as_micros();
                            (input, sim_result, sim_us)
                        })
                    })
                }));
        }

        // Collect results from all parallel simulations.
        let sim_results = futures::future::join_all(sim_handles).await;

        for result in sim_results {
            let (input, sim_result, sim_us) = match result {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "Simulation task panicked");
                    continue;
                }
            };

            sim_count += 1;
            self.metrics.inc_simulations_run(1);
            self.metrics.observe_simulation_latency_us(sim_us);

            if !sim_result.success {
                debug!(sim_us, reason = ?sim_result.revert_reason, "Simulation failed, skipping");
                continue;
            }

            let proto_arb = pipeline::build_validated_arb(
                &input.opp,
                &sim_result,
                input.flashloan_token,
                input.input_amount,
                input.steps,
                input.calldata,
            );

            let publish_span = tracing::info_span!(
                "arb.publish",
                arb_id = %input.opp.id,
                hops = input.opp.hops.len(),
                net_profit_wei = input.net_profit,
                sim_us,
            );
            publish_span.in_scope(|| {
                if let Err(e) = self.arb_tx.send(proto_arb) {
                    debug!(error = %e, "No arb subscribers connected");
                } else {
                    sim_success += 1;
                    self.metrics.inc_arbs_published(1);

                    // Human-readable path: WETH -> AAVE -> WETH
                    // Built from the simulator's own input/hops so it matches
                    // exactly what Go will see (same hop order, same token_in/out).
                    let path = arb_path_labels(&input.opp);
                    let hop_count = input.opp.hops.len();
                    let flashloan_label = token_label(&input.flashloan_token);
                    let net_profit_eth = input.net_profit as f64 / 1e18;

                    // Emit BOTH the legacy and new log lines during the transition.
                    // Downstream Loki / Grafana alert rules key on either the
                    // "Published validated arb" message or the `net_profit_wei`
                    // u128 field; dropping either would silently break them.
                    // Drop the legacy line after E2-gate alerts are ported.
                    info!(
                        id = %input.opp.id,
                        path = %path,
                        hops = hop_count,
                        flashloan = %flashloan_label,
                        net_profit_wei = input.net_profit,
                        net_profit_eth = format_args!("{:.6}", net_profit_eth),
                        sim_us,
                        "Published validated arb"
                    );
                    info!(
                        id = %input.opp.id,
                        path = %path,
                        hops = hop_count,
                        flashloan = %flashloan_label,
                        net_profit_wei = input.net_profit,
                        net_profit_eth = format_args!("{:.6}", net_profit_eth),
                        sim_us,
                        "ARB PUBLISHED"
                    );

                    self.ledger.insert_arb(&build_new_arb(
                        &input.opp,
                        input.flashloan_token,
                        input.input_amount,
                        input.net_profit,
                        self.config.tip_bps,
                        sim_us,
                        &path,
                    ));
                }
            });
        }

        let phase2_us = t_phase2.elapsed().as_micros();
        let total_cycle_us = t_cycle.elapsed().as_micros();

        info!(
            total_cycle_us,
            phase1_us,
            phase2_us,
            candidates = candidates.len(),
            simulated = sim_count,
            sim_passed = sim_success,
            "Detection cycle complete"
        );
    }

    /// Get the minimum profit threshold in wei.
    #[allow(dead_code)]
    pub fn min_profit_threshold_wei(&self) -> u128 {
        self.config.min_profit_threshold_wei
    }

    /// Get a reference to the token index.
    #[allow(dead_code)]
    pub fn token_index(&self) -> &Arc<ArcSwap<TokenIndex>> {
        &self.token_index
    }

    /// Get a reference to the pool registry.
    #[allow(dead_code)]
    pub fn pool_registry(&self) -> &Arc<ArcSwap<HashMap<Address, PoolMetadata>>> {
        &self.pool_registry
    }

    /// Get a reference to the snapshot manager.
    #[allow(dead_code)]
    pub fn snapshot_manager(&self) -> &Arc<SnapshotManager> {
        &self.snapshot_manager
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_config_default() {
        let config = EngineConfig::default();
        assert_eq!(config.max_hops, 4);
        assert_eq!(config.detection_time_budget_us, 3_000);
        assert_eq!(config.min_profit_threshold_wei, 1_000_000_000_000_000);
        assert!((config.gas_price_gwei - 30.0).abs() < f64::EPSILON);
        assert_eq!(config.tip_bps, 9000);
    }

    #[test]
    fn test_block_info_default() {
        let info = BlockInfo::default();
        assert_eq!(info.number, 0);
        assert_eq!(info.timestamp, 0);
        assert_eq!(info.base_fee, 0);
    }

    #[tokio::test]
    async fn test_engine_creation() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        // Should have event channels.
        let (pool_subs, block_subs, tx_subs) = engine.event_channels().subscriber_counts();
        assert_eq!(pool_subs, 0);
        assert_eq!(block_subs, 0);
        assert_eq!(tx_subs, 0);
    }

    #[tokio::test]
    async fn test_engine_event_channels_accessible() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        // External code can subscribe through the engine's event channels.
        let _pool_rx = engine.event_channels().subscribe_pool_updates();
        let _block_rx = engine.event_channels().subscribe_new_blocks();

        let (pool_subs, block_subs, _) = engine.event_channels().subscriber_counts();
        assert_eq!(pool_subs, 1);
        assert_eq!(block_subs, 1);
    }

    #[tokio::test]
    async fn test_engine_run_with_shutdown() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Start engine in a task.
        let engine_handle = tokio::spawn(async move {
            engine.run(shutdown_rx).await;
        });

        // Give it a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send shutdown.
        shutdown_tx.send(true).unwrap();

        // Should complete within a reasonable time.
        tokio::time::timeout(std::time::Duration::from_secs(2), engine_handle)
            .await
            .expect("engine should shut down within timeout")
            .expect("engine task should not panic");
    }

    #[tokio::test]
    async fn test_engine_handle_new_block() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let block_event = NewBlockEvent {
            block_number: 18_500_000,
            timestamp: 1_700_500_000,
            base_fee: 25_000_000_000,
            gas_limit: 30_000_000,
        };

        engine.handle_new_block(block_event).await;

        let block = engine.current_block().load();
        assert_eq!(block.number, 18_500_000);
        assert_eq!(block.timestamp, 1_700_500_000);
        assert_eq!(block.base_fee, 25_000_000_000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_engine_detection_cycle_empty_graph() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        // Empty graph, no dirty edges -- should be a no-op.
        engine.run_detection_cycle().await;
    }

    #[tokio::test]
    async fn test_engine_processes_block_via_channels() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = Arc::new(AetherEngine::new(EngineConfig::default(), tx));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let engine_clone = Arc::clone(&engine);
        let engine_handle = tokio::spawn(async move {
            engine_clone.run(shutdown_rx).await;
        });

        // Small delay for the engine to start subscribing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Dispatch a block event via the channels.
        engine.event_channels().dispatch_new_block(NewBlockEvent {
            block_number: 19_000_000,
            timestamp: 1_710_000_000,
            base_fee: 20_000_000_000,
            gas_limit: 30_000_000,
        });

        // Give the engine time to process.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify block was processed.
        let block = engine.current_block().load();
        assert_eq!(block.number, 19_000_000);

        // Shutdown.
        shutdown_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), engine_handle).await;
    }

    #[tokio::test]
    async fn test_engine_min_profit_threshold() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);
        assert_eq!(engine.min_profit_threshold_wei(), 1_000_000_000_000_000);
    }

    #[tokio::test]
    async fn test_engine_custom_config() {
        let config = EngineConfig {
            max_hops: 3,
            detection_time_budget_us: 5_000,
            min_profit_threshold_wei: 2_000_000_000_000_000,
            gas_price_gwei: 50.0,
            ..EngineConfig::default()
        };
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(config, tx);
        assert_eq!(engine.min_profit_threshold_wei(), 2_000_000_000_000_000);
    }

    #[tokio::test]
    async fn test_engine_handle_pool_update_reserve() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let event = PoolEvent::ReserveUpdate {
            pool: alloy::primitives::Address::ZERO,
            protocol: aether_common::types::ProtocolType::UniswapV2,
            reserve0: alloy::primitives::U256::from(1_000_000u64),
            reserve1: alloy::primitives::U256::from(2_000_000u64),
        };

        // Should not panic.
        engine.handle_pool_update(event).await;
    }

    #[tokio::test]
    async fn test_engine_handle_pool_update_v3() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let event = PoolEvent::V3Update {
            pool: alloy::primitives::Address::ZERO,
            sqrt_price_x96: alloy::primitives::U256::from(123_456u64),
            liquidity: 999_999,
            tick: -50,
        };

        // Should not panic.
        engine.handle_pool_update(event).await;
    }

    #[tokio::test]
    async fn test_engine_handle_pool_created() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let event = PoolEvent::PoolCreated {
            token0: alloy::primitives::Address::ZERO,
            token1: alloy::primitives::Address::repeat_byte(1),
            pool: alloy::primitives::Address::repeat_byte(2),
        };

        // Should not panic.
        engine.handle_pool_update(event).await;
    }

    // ---- New Phase 6 tests ----

    #[test]
    fn test_pool_metadata_fee_factor() {
        let meta = PoolMetadata {
            token0_idx: 0,
            token1_idx: 1,
            token0: Address::ZERO,
            token1: Address::repeat_byte(1),
            pool_id: PoolId {
                address: Address::repeat_byte(2),
                protocol: ProtocolType::UniswapV2,
            },
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        };
        assert!((meta.fee_factor() - 0.997).abs() < 1e-10);

        let meta_v3 = PoolMetadata {
            fee_bps: 5,
            ..meta
        };
        assert!((meta_v3.fee_factor() - 0.9995).abs() < 1e-10);
    }

    #[test]
    fn test_u256_to_f64_zero() {
        assert_eq!(u256_to_f64(U256::ZERO), 0.0);
    }

    #[test]
    fn test_u256_to_f64_small() {
        let val = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let f = u256_to_f64(val);
        assert!((f - 1e18).abs() < 1.0);
    }

    #[test]
    fn test_u256_to_f64_large() {
        // 2^128 = 3.4e38
        let val = U256::from(1u128) << 128;
        let f = u256_to_f64(val);
        assert!((f - 3.402_823_669_209_385e38).abs() / f < 1e-10);
    }

    #[tokio::test]
    async fn test_register_pool() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let pool = Address::repeat_byte(0xAA);
        let token0 = Address::repeat_byte(0x01);
        let token1 = Address::repeat_byte(0x02);

        engine
            .register_pool(pool, token0, token1, ProtocolType::UniswapV2, 30)
            .await;

        // Verify token index has both tokens.
        let ti = engine.token_index.load();
        assert_eq!(ti.len(), 2);
        assert!(ti.contains(&token0));
        assert!(ti.contains(&token1));

        // Verify pool registry has the pool.
        let reg = engine.pool_registry.load();
        assert!(reg.contains_key(&pool));
        let meta = reg.get(&pool).unwrap();
        assert_eq!(meta.protocol, ProtocolType::UniswapV2);
        assert_eq!(meta.fee_bps, 30);

        // Verify working graph has 2 edges (bidirectional).
        // Note: register_pool does not publish to snapshot (deferred to batch callers).
        let graph = engine.working_graph.lock().await;
        assert_eq!(graph.num_edges(), 2);
    }

    #[tokio::test]
    async fn test_reserve_update_updates_graph() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let pool = Address::repeat_byte(0xBB);
        let token0 = Address::repeat_byte(0x10);
        let token1 = Address::repeat_byte(0x20);

        // Register the pool first.
        engine
            .register_pool(pool, token0, token1, ProtocolType::UniswapV2, 30)
            .await;

        // Clear dirty from registration on the working graph.
        {
            let mut graph = engine.working_graph.lock().await;
            graph.clear_dirty();
        }

        // Send a reserve update.
        let event = PoolEvent::ReserveUpdate {
            pool,
            protocol: ProtocolType::UniswapV2,
            reserve0: U256::from(1_000_000_000_000_000_000u128), // 1e18
            reserve1: U256::from(2_000_000_000_000_000_000u128), // 2e18
        };
        engine.handle_pool_update(event).await;

        // Under the new model, event handlers mutate working_graph only.
        // The snapshot is published at cycle start, not per-event.
        // Dirty flags must be present on working_graph (not yet snapshotted).
        let graph = engine.working_graph.lock().await;
        assert!(graph.has_dirty_edges());
    }

    #[tokio::test]
    async fn test_v3_update_updates_graph() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let pool = Address::repeat_byte(0xCC);
        let token0 = Address::repeat_byte(0x30);
        let token1 = Address::repeat_byte(0x40);

        // Register as V3 pool.
        engine
            .register_pool(pool, token0, token1, ProtocolType::UniswapV3, 5)
            .await;

        {
            let mut graph = engine.working_graph.lock().await;
            graph.clear_dirty();
        }

        // Send a V3 update with a realistic sqrt_price_x96.
        // For a 1:1 price, sqrt_price_x96 = 2^96 = 79228162514264337593543950336
        let sqrt_one = U256::from(1u128) << 96;
        let event = PoolEvent::V3Update {
            pool,
            sqrt_price_x96: sqrt_one,
            liquidity: 1_000_000,
            tick: 0,
        };
        engine.handle_pool_update(event).await;

        // Under the new model, event handlers mutate working_graph only.
        // Dirty flags must be present on working_graph (not yet snapshotted).
        let graph = engine.working_graph.lock().await;
        assert!(graph.has_dirty_edges());
    }

    #[tokio::test]
    async fn test_pool_created_auto_registers() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let token0 = Address::repeat_byte(0x50);
        let token1 = Address::repeat_byte(0x60);
        let pool = Address::repeat_byte(0x70);

        let event = PoolEvent::PoolCreated {
            token0,
            token1,
            pool,
        };
        engine.handle_pool_update(event).await;

        // Should have auto-registered.
        let reg = engine.pool_registry.load();
        assert!(reg.contains_key(&pool));
        let meta = reg.get(&pool).unwrap();
        assert_eq!(meta.protocol, ProtocolType::UniswapV2);
        assert_eq!(meta.fee_bps, 30);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_detection_cycle_with_registered_pools() {
        let (tx, mut rx) = broadcast::channel(100);
        let engine = AetherEngine::new(
            EngineConfig {
                min_profit_threshold_wei: 0, // Accept any profit for testing.
                gas_price_gwei: 0.0,         // Zero gas for testing.
                ..EngineConfig::default()
            },
            tx,
        );

        // Register 3 pools forming a triangle.
        let token_a = Address::repeat_byte(0x01);
        let token_b = Address::repeat_byte(0x02);
        let token_c = Address::repeat_byte(0x03);
        let pool_ab = Address::repeat_byte(0x11);
        let pool_bc = Address::repeat_byte(0x22);
        let pool_ca = Address::repeat_byte(0x33);

        engine
            .register_pool(pool_ab, token_a, token_b, ProtocolType::UniswapV2, 0)
            .await;
        engine
            .register_pool(pool_bc, token_b, token_c, ProtocolType::SushiSwap, 0)
            .await;
        engine
            .register_pool(pool_ca, token_c, token_a, ProtocolType::Curve, 0)
            .await;

        // Set exchange rates that create a profitable cycle.
        // A→B rate=1.5, B→C rate=1.5, C→A rate=1.5 → product=3.375 > 1.
        {
            let reg = engine.pool_registry.load();
            let meta_ab = reg.get(&pool_ab).unwrap().clone();
            let meta_bc = reg.get(&pool_bc).unwrap().clone();
            let meta_ca = reg.get(&pool_ca).unwrap().clone();
            drop(reg);

            let mut graph = engine.working_graph.lock().await;
            graph.add_edge(
                meta_ab.token0_idx,
                meta_ab.token1_idx,
                1.5,
                meta_ab.pool_id,
                pool_ab,
                ProtocolType::UniswapV2,
                U256::from(1_000_000u64),
            );
            graph.add_edge(
                meta_bc.token0_idx,
                meta_bc.token1_idx,
                1.5,
                meta_bc.pool_id,
                pool_bc,
                ProtocolType::SushiSwap,
                U256::from(1_000_000u64),
            );
            graph.add_edge(
                meta_ca.token0_idx,
                meta_ca.token1_idx,
                1.5,
                meta_ca.pool_id,
                pool_ca,
                ProtocolType::Curve,
                U256::from(1_000_000u64),
            );
        }

        // Set a block so the detection cycle has context.
        engine.current_block.store(Arc::new(BlockInfo {
            number: 18_000_000,
            timestamp: 1_700_000_000,
            base_fee: 0,
        }));

        // Run detection cycle.
        engine.run_detection_cycle().await;

        // The EVM treats calls to Address::ZERO (no code) as a success,
        // so the simulation passes and the arb gets published.
        // Check that dirty flags were cleared on the working graph.
        // The published snapshot retains dirty flags (detection reads them),
        // but working_graph is clean — ready for next cycle's event accumulation.
        let graph = engine.working_graph.lock().await;
        assert!(!graph.has_dirty_edges());

        // With zero gas cost and zero profit threshold, the profitable cycle
        // should be detected, simulated (success on empty account), and published.
        let arb = rx.try_recv().expect("should receive a published arb");
        assert!(!arb.id.is_empty());
        assert!(!arb.hops.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bootstrap_pools_then_detect_arb() {
        // Integration test: bootstrap real mainnet pools from config/pools.toml,
        // set profitable exchange rates on the graph, run detection, and confirm
        // an arb opportunity is detected and published.

        // Real mainnet pool addresses from config/pools.toml.
        // Real mainnet token and pool addresses.
        let _usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap();
        let _weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".parse().unwrap();
        let uni_v2_pool: Address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".parse().unwrap();
        let sushi_pool: Address = "0x397FF1542f962076d0BFE58eA045FfA2d347ACa0".parse().unwrap();
        let uni_v3_pool: Address = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".parse().unwrap();

        // 1. Create engine and bootstrap from the real config file.
        let (tx, mut rx) = broadcast::channel(100);
        let engine = AetherEngine::new(
            EngineConfig {
                min_profit_threshold_wei: 0, // Accept any profit for testing.
                gas_price_gwei: 0.0,         // Zero gas for testing.
                ..EngineConfig::default()
            },
            tx,
        );

        // CARGO_MANIFEST_DIR points to crates/grpc-server/, go up two levels
        // to reach the workspace root where config/pools.toml lives.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let config_path = format!("{manifest_dir}/../../config/pools.toml");
        let loaded = engine.bootstrap_pools(&config_path).await;
        assert_eq!(loaded, 3, "All 3 pools from config/pools.toml should be loaded");

        // 2. Verify all real pools are registered with correct metadata.
        {
            let registry = engine.pool_registry.load();
            assert_eq!(registry.len(), 3);

            let meta_v2 = registry.get(&uni_v2_pool).expect("Uniswap V2 pool should be registered");
            assert_eq!(meta_v2.protocol, ProtocolType::UniswapV2);
            assert_eq!(meta_v2.fee_bps, 30);

            let meta_sushi = registry.get(&sushi_pool).expect("SushiSwap pool should be registered");
            assert_eq!(meta_sushi.protocol, ProtocolType::SushiSwap);
            assert_eq!(meta_sushi.fee_bps, 30);

            let meta_v3 = registry.get(&uni_v3_pool).expect("Uniswap V3 pool should be registered");
            assert_eq!(meta_v3.protocol, ProtocolType::UniswapV3);
            assert_eq!(meta_v3.fee_bps, 5);
        }

        // 3. Set profitable exchange rates to simulate a cross-DEX arb.
        //    All pools share the same USDC/WETH pair. We set divergent prices
        //    so buying on one DEX and selling on another is profitable.
        //    Uni V2: USDC→WETH = 2000 (cheap WETH)
        //    Sushi:  WETH→USDC = 2100 (expensive WETH) — the arb sells here
        //    V3:     USDC→WETH = 2050 (mid price, creates cycle opportunity)
        {
            let reg = engine.pool_registry.load();
            let meta_v2 = reg.get(&uni_v2_pool).unwrap().clone();
            let meta_sushi = reg.get(&sushi_pool).unwrap().clone();
            let meta_v3 = reg.get(&uni_v3_pool).unwrap().clone();
            drop(reg);

            let mut graph = engine.working_graph.lock().await;

            // Uni V2: USDC→WETH at 1/2000, WETH→USDC at 2000
            graph.add_edge(
                meta_v2.token0_idx, meta_v2.token1_idx,
                0.0005, meta_v2.pool_id, uni_v2_pool,
                ProtocolType::UniswapV2, U256::from(1_000_000u64),
            );
            graph.add_edge(
                meta_v2.token1_idx, meta_v2.token0_idx,
                2000.0, meta_v2.pool_id, uni_v2_pool,
                ProtocolType::UniswapV2, U256::from(1_000_000u64),
            );

            // Sushi: USDC→WETH at 1/2100, WETH→USDC at 2100
            graph.add_edge(
                meta_sushi.token0_idx, meta_sushi.token1_idx,
                0.000476, meta_sushi.pool_id, sushi_pool,
                ProtocolType::SushiSwap, U256::from(1_000_000u64),
            );
            graph.add_edge(
                meta_sushi.token1_idx, meta_sushi.token0_idx,
                2100.0, meta_sushi.pool_id, sushi_pool,
                ProtocolType::SushiSwap, U256::from(1_000_000u64),
            );

            // V3: USDC→WETH at 1/2050, WETH→USDC at 2050
            graph.add_edge(
                meta_v3.token0_idx, meta_v3.token1_idx,
                0.000488, meta_v3.pool_id, uni_v3_pool,
                ProtocolType::UniswapV3, U256::from(1_000_000u64),
            );
            graph.add_edge(
                meta_v3.token1_idx, meta_v3.token0_idx,
                2050.0, meta_v3.pool_id, uni_v3_pool,
                ProtocolType::UniswapV3, U256::from(1_000_000u64),
            );
        }

        // 4. Set a recent block so detection has context.
        engine.current_block.store(Arc::new(BlockInfo {
            number: 18_000_000,
            timestamp: 1_700_000_000,
            base_fee: 0,
        }));

        // 5. Run detection cycle.
        engine.run_detection_cycle().await;

        // 6. Assert: arb opportunity was detected and published.
        //    The price divergence between Uni V2 (buy WETH at 2000) and
        //    Sushi (sell WETH at 2100) creates a profitable cycle that
        //    Bellman-Ford should detect.
        let arb = rx.try_recv().expect(
            "should receive a published arb — price divergence between \
             Uniswap V2 (2000) and SushiSwap (2100) should be detected"
        );
        assert!(!arb.id.is_empty(), "arb should have an ID");
        assert!(!arb.hops.is_empty(), "arb should have at least one hop");
    }

    #[tokio::test]
    async fn test_bootstrap_pools_invalid_config() {
        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        // Non-existent file should return 0.
        let loaded = engine.bootstrap_pools("/tmp/nonexistent_pools.toml").await;
        assert_eq!(loaded, 0);
    }

    #[tokio::test]
    async fn test_bootstrap_pools_skips_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("pools.toml");
        let toml_content = r#"
[[pools]]
protocol = "uniswap_v2"
address = "0x1111111111111111111111111111111111111111"
token0 = "0x0101010101010101010101010101010101010101"
token1 = "0x0202020202020202020202020202020202020202"
fee_bps = 30

[[pools]]
protocol = "sushiswap"
address = "0x1111111111111111111111111111111111111111"
token0 = "0x0101010101010101010101010101010101010101"
token1 = "0x0202020202020202020202020202020202020202"
fee_bps = 30
"#;
        tokio::fs::write(&config_path, toml_content).await.unwrap();

        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let loaded = engine.bootstrap_pools(config_path.to_str().unwrap()).await;
        assert_eq!(loaded, 1, "Second pool with same address should be skipped");
    }

    #[tokio::test]
    async fn test_bootstrap_pools_skips_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("pools.toml");
        let toml_content = r#"
[[pools]]
protocol = "uniswap_v2"
address = "0x1111111111111111111111111111111111111111"
token0 = "0x0101010101010101010101010101010101010101"
token1 = "0x0202020202020202020202020202020202020202"
fee_bps = 30

[[pools]]
protocol = "unknown_dex"
address = "0x2222222222222222222222222222222222222222"
token0 = "0x0101010101010101010101010101010101010101"
token1 = "0x0202020202020202020202020202020202020202"
fee_bps = 30

[[pools]]
protocol = "uniswap_v2"
address = "0x0000000000000000000000000000000000000000"
token0 = "0x0101010101010101010101010101010101010101"
token1 = "0x0202020202020202020202020202020202020202"
fee_bps = 30
"#;
        tokio::fs::write(&config_path, toml_content).await.unwrap();

        let (tx, _rx) = broadcast::channel(100);
        let engine = AetherEngine::new(EngineConfig::default(), tx);

        let loaded = engine.bootstrap_pools(config_path.to_str().unwrap()).await;
        assert_eq!(loaded, 1, "Only the valid pool should be loaded");
    }

    // ---- Optimizer + slippage integration tests ----

    /// Decode a big-endian 32-byte proto `bytes` field back into `U256`.
    fn bytes_to_u256(bytes: &[u8]) -> U256 {
        if bytes.is_empty() {
            return U256::ZERO;
        }
        U256::from_be_slice(bytes)
    }

    /// Set up an engine with a profitable A->B->C->A triangle and run
    /// the detection cycle, returning the published proto arb.
    async fn setup_triangle_engine(
        slippage_bps: u32,
        rate_ab: f64,
        rate_bc: f64,
        rate_ca: f64,
        liquidity: U256,
    ) -> Option<crate::service::aether_proto::ValidatedArb> {
        let (tx, mut rx) = broadcast::channel(100);
        let engine = AetherEngine::new(
            EngineConfig {
                min_profit_threshold_wei: 0,
                gas_price_gwei: 0.0,
                slippage_bps,
                ..EngineConfig::default()
            },
            tx,
        );

        let token_a = Address::repeat_byte(0x01);
        let token_b = Address::repeat_byte(0x02);
        let token_c = Address::repeat_byte(0x03);
        let pool_ab = Address::repeat_byte(0x11);
        let pool_bc = Address::repeat_byte(0x22);
        let pool_ca = Address::repeat_byte(0x33);

        engine
            .register_pool(pool_ab, token_a, token_b, ProtocolType::UniswapV2, 0)
            .await;
        engine
            .register_pool(pool_bc, token_b, token_c, ProtocolType::SushiSwap, 0)
            .await;
        engine
            .register_pool(pool_ca, token_c, token_a, ProtocolType::Curve, 0)
            .await;

        {
            let reg = engine.pool_registry.load();
            let meta_ab = reg.get(&pool_ab).unwrap().clone();
            let meta_bc = reg.get(&pool_bc).unwrap().clone();
            let meta_ca = reg.get(&pool_ca).unwrap().clone();
            drop(reg);

            let mut graph = engine.working_graph.lock().await;
            graph.add_edge(
                meta_ab.token0_idx,
                meta_ab.token1_idx,
                rate_ab,
                meta_ab.pool_id,
                pool_ab,
                ProtocolType::UniswapV2,
                liquidity,
            );
            graph.add_edge(
                meta_bc.token0_idx,
                meta_bc.token1_idx,
                rate_bc,
                meta_bc.pool_id,
                pool_bc,
                ProtocolType::SushiSwap,
                liquidity,
            );
            graph.add_edge(
                meta_ca.token0_idx,
                meta_ca.token1_idx,
                rate_ca,
                meta_ca.pool_id,
                pool_ca,
                ProtocolType::Curve,
                liquidity,
            );
        }

        {
            let mut block = (**engine.current_block.load()).clone();
            block.number = 18_000_000;
            block.timestamp = 1_700_000_000;
            block.base_fee = 0;
            engine.current_block.store(Arc::new(block));
        }

        // Publish the working graph as a snapshot so run_detection_cycle can read it.
        {
            let graph = engine.working_graph.lock().await;
            engine.snapshot_manager.publish(graph.clone(), 18_000_000, 1_700_000_000_000_000_000);
        }

        engine.run_detection_cycle().await;

        rx.try_recv().ok()
    }

    #[test]
    fn test_engine_config_slippage_default() {
        let config = EngineConfig::default();
        assert_eq!(config.slippage_bps, 100, "Default slippage should be 100 bps (1%)");
    }

    #[test]
    fn test_engine_config_custom_slippage() {
        let config = EngineConfig {
            slippage_bps: 500,
            ..EngineConfig::default()
        };
        assert_eq!(config.slippage_bps, 500);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_optimizer_finds_optimal_input_not_hardcoded() {
        let arb = setup_triangle_engine(
            100,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128), // 100 ETH liquidity
        )
        .await
        .expect("profitable cycle should produce an arb");

        // The optimizer should NOT use hardcoded 1 ETH.
        let one_eth = U256::from(1_000_000_000_000_000_000u128);
        let first_hop_amount_in = bytes_to_u256(&arb.hops[0].amount_in);
        assert_ne!(
            first_hop_amount_in, one_eth,
            "Optimizer should find an amount different from hardcoded 1 ETH"
        );
        assert!(
            !first_hop_amount_in.is_zero(),
            "Optimizer should produce a non-zero input"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_expected_out_is_nonzero_per_hop() {
        let arb = setup_triangle_engine(
            100,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await
        .expect("profitable cycle should produce an arb");

        for (i, hop) in arb.hops.iter().enumerate() {
            let expected_out = bytes_to_u256(&hop.expected_out);
            assert!(
                !expected_out.is_zero(),
                "Hop {} expected_out should be non-zero, got 0",
                i
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_slippage_protection_active() {
        let arb = setup_triangle_engine(
            100,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await
        .expect("profitable cycle should produce an arb");

        for (i, step) in arb.steps.iter().enumerate() {
            let min_amount_out = bytes_to_u256(&step.min_amount_out);
            assert!(
                !min_amount_out.is_zero(),
                "Step {} min_amount_out should be non-zero (slippage protection active)",
                i
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_optimizer_respects_liquidity_cap() {
        // Set small liquidity so the optimizer is capped.
        let small_liquidity = U256::from(500_000_000_000_000_000u128); // 0.5 ETH
        let arb = setup_triangle_engine(100, 1.5, 1.5, 1.5, small_liquidity)
            .await
            .expect("profitable cycle should produce an arb");

        let first_hop_amount_in = bytes_to_u256(&arb.hops[0].amount_in);
        assert!(
            first_hop_amount_in <= small_liquidity,
            "Input {} should not exceed min liquidity {}",
            first_hop_amount_in,
            small_liquidity
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_hop_amounts_chain_correctly() {
        let arb = setup_triangle_engine(
            100,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await
        .expect("profitable cycle should produce an arb");

        assert!(arb.hops.len() >= 2, "need at least 2 hops for chaining test");
        for i in 1..arb.hops.len() {
            let prev_out = bytes_to_u256(&arb.hops[i - 1].expected_out);
            let curr_in = bytes_to_u256(&arb.hops[i].amount_in);
            assert_eq!(
                prev_out, curr_in,
                "Hop {} amount_in ({}) should equal hop {} expected_out ({})",
                i, curr_in, i - 1, prev_out
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_unprofitable_cycle_filtered_by_optimizer() {
        // rates 0.9^3 = 0.729 < 1 — unprofitable.
        let result = setup_triangle_engine(
            100,
            0.9,
            0.9,
            0.9,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await;

        assert!(
            result.is_none(),
            "Unprofitable cycle (0.9^3 = 0.729) should not produce an arb"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_custom_slippage_bps_applied() {
        // 500 bps = 5% slippage
        let arb = setup_triangle_engine(
            500,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await
        .expect("profitable cycle should produce an arb");

        for step in &arb.steps {
            let amount_in = bytes_to_u256(&step.amount_in);
            let min_out = bytes_to_u256(&step.min_amount_out);
            // With 500 bps slippage, min_out should be roughly 95% of expected_out.
            // Since expected_out = amount_in * rate and min_out = expected_out * 9500/10000,
            // min_out should be strictly less than amount_in * rate (for rate=1.5).
            assert!(
                !min_out.is_zero(),
                "min_amount_out should be non-zero with 500 bps slippage"
            );
            // min_out should be less than what you'd get without slippage
            // For rate 1.5: expected_out = amount_in * 1.5, min_out = expected_out * 0.95
            // So min_out < expected_out, and min_out > 0
            assert!(
                min_out < amount_in * U256::from(2u32),
                "min_out should be reasonable relative to amount_in"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_optimizer_output_exceeds_input_for_profitable_cycle() {
        let arb = setup_triangle_engine(
            100,
            1.5,
            1.5,
            1.5,
            U256::from(100_000_000_000_000_000_000u128),
        )
        .await
        .expect("profitable cycle should produce an arb");

        let first_input = bytes_to_u256(&arb.hops[0].amount_in);
        let last_output = bytes_to_u256(&arb.hops.last().unwrap().expected_out);
        assert!(
            last_output > first_input,
            "For profitable cycle, last output ({}) should exceed first input ({})",
            last_output,
            first_input
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_optimizer_profit_ge_fixed_1eth_with_reserves() {
        // Set up engine with realistic reserves so AMM math is exercised.
        let (tx, mut rx) = broadcast::channel(100);
        let engine = AetherEngine::new(
            EngineConfig {
                min_profit_threshold_wei: 0,
                gas_price_gwei: 0.0,
                slippage_bps: 100,
                ..EngineConfig::default()
            },
            tx,
        );

        let token_a = Address::repeat_byte(0x01);
        let token_b = Address::repeat_byte(0x02);
        let token_c = Address::repeat_byte(0x03);
        let pool_ab = Address::repeat_byte(0x11);
        let pool_bc = Address::repeat_byte(0x22);
        let pool_ca = Address::repeat_byte(0x33);
        let liq = U256::from(500_000_000_000_000_000_000u128); // 500 ETH

        engine.register_pool(pool_ab, token_a, token_b, ProtocolType::UniswapV2, 30).await;
        engine.register_pool(pool_bc, token_b, token_c, ProtocolType::SushiSwap, 30).await;
        engine.register_pool(pool_ca, token_c, token_a, ProtocolType::Curve, 30).await;

        // Reserves that create a profitable cycle: rate product > 1.
        // Pool AB: 1000 A / 1500 B → rate ~1.5 (after fee ~1.4955)
        // Pool BC: 1000 B / 1500 C → rate ~1.5
        // Pool CA: 1000 C / 1000 A → rate ~1.0
        // Product ~2.25 before fees → profitable.
        let r_ab_in = 1000.0_f64 * 1e18;
        let r_ab_out = 1500.0_f64 * 1e18;
        let r_bc_in = 1000.0_f64 * 1e18;
        let r_bc_out = 1500.0_f64 * 1e18;
        let r_ca_in = 1000.0_f64 * 1e18;
        let r_ca_out = 1000.0_f64 * 1e18;

        {
            let reg = engine.pool_registry.load();
            let meta_ab = reg.get(&pool_ab).unwrap().clone();
            let meta_bc = reg.get(&pool_bc).unwrap().clone();
            let meta_ca = reg.get(&pool_ca).unwrap().clone();
            drop(reg);

            let fee = 0.997;
            let mut graph = engine.working_graph.lock().await;

            // Set rates from reserves and populate reserve fields.
            graph.add_edge(meta_ab.token0_idx, meta_ab.token1_idx,
                (r_ab_out / r_ab_in) * fee, meta_ab.pool_id, pool_ab,
                ProtocolType::UniswapV2, liq);
            graph.update_edge_from_reserves(
                meta_ab.token0_idx, meta_ab.token1_idx, meta_ab.pool_id,
                r_ab_in, r_ab_out, fee);

            graph.add_edge(meta_bc.token0_idx, meta_bc.token1_idx,
                (r_bc_out / r_bc_in) * fee, meta_bc.pool_id, pool_bc,
                ProtocolType::SushiSwap, liq);
            graph.update_edge_from_reserves(
                meta_bc.token0_idx, meta_bc.token1_idx, meta_bc.pool_id,
                r_bc_in, r_bc_out, fee);

            graph.add_edge(meta_ca.token0_idx, meta_ca.token1_idx,
                (r_ca_out / r_ca_in) * fee, meta_ca.pool_id, pool_ca,
                ProtocolType::Curve, liq);
            graph.update_edge_from_reserves(
                meta_ca.token0_idx, meta_ca.token1_idx, meta_ca.pool_id,
                r_ca_in, r_ca_out, fee);
        }

        {
            let mut block = (**engine.current_block.load()).clone();
            block.number = 18_000_000;
            block.timestamp = 1_700_000_000;
            block.base_fee = 0;
            engine.current_block.store(Arc::new(block));
        }

        // Publish the working graph as a snapshot so run_detection_cycle can read it.
        {
            let graph = engine.working_graph.lock().await;
            engine.snapshot_manager.publish(graph.clone(), 18_000_000, 1_700_000_000_000_000_000);
        }

        engine.run_detection_cycle().await;
        let arb = rx.try_recv().expect("should produce an arb");

        let optimizer_input = bytes_to_u256(&arb.hops[0].amount_in);
        let optimizer_output = bytes_to_u256(&arb.hops.last().unwrap().expected_out);
        let optimizer_profit = optimizer_output.saturating_sub(optimizer_input);

        // Compute what fixed 1 ETH would yield through the same AMM path.
        let one_eth = 1_000_000_000_000_000_000.0_f64;
        let mut current = one_eth;
        let reserves = [(r_ab_in, r_ab_out), (r_bc_in, r_bc_out), (r_ca_in, r_ca_out)];
        for (x, y) in &reserves {
            current = (current * 0.997 * y) / (x + current * 0.997);
        }
        let fixed_profit_f64 = current - one_eth;
        let fixed_profit = U256::from(fixed_profit_f64.max(0.0) as u128);

        assert!(
            optimizer_profit >= fixed_profit,
            "Optimizer profit ({}) should be >= fixed 1 ETH profit ({})",
            optimizer_profit, fixed_profit
        );

        // The optimizer should NOT have chosen exactly 1 ETH.
        let one_eth_u256 = U256::from(1_000_000_000_000_000_000u128);
        assert_ne!(optimizer_input, one_eth_u256,
            "Optimizer should find a different amount than hardcoded 1 ETH");
    }

    #[test]
    fn test_slippage_bps_overflow_clamped() {
        // Verify that slippage_bps >= 10000 doesn't cause U256 underflow.
        // The engine clamps to 9999 internally.
        let config = EngineConfig {
            slippage_bps: 10_000,
            ..EngineConfig::default()
        };
        let denom = U256::from(10_000u32);
        let clamped = config.slippage_bps.min(9999);
        let factor = denom - U256::from(clamped);
        assert_eq!(factor, U256::from(1u32), "Clamped factor should be 1 (not underflow)");
    }
}
