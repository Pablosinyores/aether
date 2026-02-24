use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info};

use alloy::primitives::{Address, U256};

use aether_common::types::{ArbHop, ArbOpportunity, PoolId, ProtocolType, SwapStep};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas::{estimate_total_gas, gas_cost_wei};
use aether_ingestion::event_decoder::PoolEvent;
use aether_ingestion::subscription::{EventChannels, NewBlockEvent};
use aether_simulator::calldata::build_execute_arb_calldata;
use aether_simulator::fork::ForkedState;
use aether_simulator::EvmSimulator;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

// Import the proto ValidatedArb type from service module
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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_hops: 4,
            detection_time_budget_us: 3_000, // 3ms
            min_profit_threshold_wei: 1_000_000_000_000_000, // 0.001 ETH
            gas_price_gwei: 30.0,
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
    /// Shared price graph (wrapped in RwLock for concurrent access).
    price_graph: Arc<RwLock<PriceGraph>>,
    /// Bellman-Ford detector.
    detector: BellmanFord,
    /// EVM simulator for validating arb profitability.
    simulator: EvmSimulator,
    /// Broadcast sender for validated arbs (connected to gRPC stream).
    arb_tx: broadcast::Sender<ProtoValidatedArb>,
    /// Current block info.
    current_block: Arc<RwLock<BlockInfo>>,
    /// Bidirectional token address ↔ graph vertex index mapping.
    token_index: Arc<RwLock<TokenIndex>>,
    /// Pool address → metadata mapping for event handling.
    pool_registry: Arc<RwLock<HashMap<Address, PoolMetadata>>>,
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
    profit_factor: f64,
    flashloan_token: Address,
    path_id: String,
}

impl AetherEngine {
    pub fn new(config: EngineConfig, arb_tx: broadcast::Sender<ProtoValidatedArb>) -> Self {
        let event_channels = Arc::new(EventChannels::new());
        let detector = BellmanFord::new(config.max_hops, config.detection_time_budget_us);
        let simulator = EvmSimulator::with_defaults();

        // Start with a reasonable initial graph size (can grow dynamically).
        let price_graph = Arc::new(RwLock::new(PriceGraph::new(100)));

        Self {
            config,
            event_channels,
            price_graph,
            detector,
            simulator,
            arb_tx,
            current_block: Arc::new(RwLock::new(BlockInfo::default())),
            token_index: Arc::new(RwLock::new(TokenIndex::new())),
            pool_registry: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get a reference to the event channels for external use (e.g., the
    /// provider pushing events into the engine).
    pub fn event_channels(&self) -> &Arc<EventChannels> {
        &self.event_channels
    }

    /// Get a reference to the current block info.
    #[allow(dead_code)]
    pub fn current_block(&self) -> &Arc<RwLock<BlockInfo>> {
        &self.current_block
    }

    /// Register a pool in the engine's pool registry and create placeholder
    /// edges in the price graph.
    pub async fn register_pool(
        &self,
        pool_addr: Address,
        token0: Address,
        token1: Address,
        protocol: ProtocolType,
        fee_bps: u32,
    ) {
        let (t0_idx, t1_idx, num_tokens) = {
            let mut token_index = self.token_index.write().await;
            let t0 = token_index.get_or_insert(token0);
            let t1 = token_index.get_or_insert(token1);
            (t0, t1, token_index.len())
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
            let mut registry = self.pool_registry.write().await;
            registry.insert(pool_addr, metadata);
        }

        // Ensure graph can hold the new vertices and add placeholder edges.
        {
            let mut graph = self.price_graph.write().await;
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
        }

        debug!(
            %pool_addr, %token0, %token1, ?protocol, fee_bps,
            "Pool registered (t0={}, t1={})", t0_idx, t1_idx
        );
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

        // Update current block info.
        {
            let mut block = self.current_block.write().await;
            block.number = event.block_number;
            block.timestamp = event.timestamp;
            block.base_fee = event.base_fee;
        }

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
                let meta = {
                    let registry = self.pool_registry.read().await;
                    registry.get(&pool).cloned()
                };

                if let Some(meta) = meta {
                    let r0 = u256_to_f64(reserve0);
                    let r1 = u256_to_f64(reserve1);

                    if r0 > 0.0 && r1 > 0.0 {
                        let fee = meta.fee_factor();
                        let mut graph = self.price_graph.write().await;
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
                    }
                }
            }
            PoolEvent::V3Update {
                pool,
                sqrt_price_x96,
                liquidity,
                tick: _,
            } => {
                debug!(%pool, %sqrt_price_x96, liquidity, "V3 pool update");

                let meta = {
                    let registry = self.pool_registry.read().await;
                    registry.get(&pool).cloned()
                };

                if let Some(meta) = meta {
                    // price = (sqrt_price_x96 / 2^96)^2
                    const TWO_POW_96: f64 = 79_228_162_514_264_337_593_543_950_336.0;
                    let sqrt_f64 = u256_to_f64(sqrt_price_x96);
                    let price = (sqrt_f64 / TWO_POW_96).powi(2);

                    if price > 0.0 {
                        let fee = meta.fee_factor();
                        let liq = U256::from(liquidity);
                        let mut graph = self.price_graph.write().await;
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
            }
        }
    }

    /// Run a detection cycle: scan for negative cycles, simulate, publish.
    async fn run_detection_cycle(&self) {
        // Phase 1: Detect cycles and extract candidate data under read locks.
        let candidates = {
            let graph = self.price_graph.read().await;

            if !graph.has_dirty_edges() && graph.num_edges() == 0 {
                return;
            }

            // Get affected vertices for partial scan.
            let affected = graph.affected_vertices();

            let cycles = if affected.is_empty() {
                // Full scan (e.g., on first run).
                self.detector.detect_negative_cycles(&graph)
            } else {
                self.detector.detect_from_affected(&graph, &affected)
            };

            if cycles.is_empty() {
                drop(graph);
                let mut graph = self.price_graph.write().await;
                graph.clear_dirty();
                return;
            }

            debug!(count = cycles.len(), "Detected negative cycles");

            let token_index = self.token_index.read().await;
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

                    let estimated_gas =
                        aether_detector::gas::estimate_swap_gas(best_edge.protocol, 0);

                    hops.push(ArbHop {
                        protocol: best_edge.protocol,
                        pool_address: best_edge.pool_address,
                        token_in: from_addr,
                        token_out: to_addr,
                        amount_in: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
                        expected_out: U256::ZERO, // Actual output depends on reserves
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
                    profit_factor,
                    flashloan_token,
                    path_id,
                });
            }

            candidates
            // graph and token_index read locks released here.
        };

        // Phase 2: Simulate and publish (no graph lock needed).
        let block_info = self.current_block.read().await.clone();

        for candidate in &candidates {
            // Estimate total gas.
            let total_gas =
                estimate_total_gas(&candidate.protocols, &candidate.tick_counts);
            let gas_cost = gas_cost_wei(total_gas, self.config.gas_price_gwei);

            // Estimate gross profit from profit_factor.
            let gross_profit_wei = (candidate.profit_factor * 1e18) as u128;

            if gross_profit_wei <= gas_cost {
                debug!("Cycle unprofitable after gas costs");
                continue;
            }

            let net_profit = gross_profit_wei - gas_cost;
            if net_profit < self.config.min_profit_threshold_wei {
                debug!(
                    net_profit,
                    threshold = self.config.min_profit_threshold_wei,
                    "Below min profit threshold"
                );
                continue;
            }

            let input_amount = U256::from(1_000_000_000_000_000_000u128); // 1 ETH

            // Build SwapSteps with 1% slippage.
            let steps: Vec<SwapStep> = candidate
                .hops
                .iter()
                .map(|hop| {
                    let min_out = hop.expected_out * U256::from(99) / U256::from(100);
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

            // Build calldata.
            let calldata = build_execute_arb_calldata(
                &steps,
                candidate.flashloan_token,
                input_amount,
            );

            // Create ArbOpportunity.
            let opp = ArbOpportunity {
                id: format!("arb-{}-{}", block_info.number, candidate.path_id),
                hops: candidate.hops.clone(),
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

            // Simulate on forked state.
            let forked_state = ForkedState::new_empty(
                block_info.number,
                block_info.timestamp,
                block_info.base_fee as u64,
            );

            // Executor contract address (placeholder — would be loaded from config).
            let executor_addr = Address::ZERO;
            let sim_result =
                self.simulator
                    .simulate(&forked_state, executor_addr, calldata.clone());

            if !sim_result.success {
                debug!(reason = ?sim_result.revert_reason, "Simulation failed, skipping");
                continue;
            }

            // Build proto ValidatedArb and publish.
            let proto_arb = pipeline::build_validated_arb(
                &opp,
                &sim_result,
                candidate.flashloan_token,
                input_amount,
                &steps,
                calldata,
            );

            if let Err(e) = self.arb_tx.send(proto_arb) {
                debug!(error = %e, "No arb subscribers connected");
            } else {
                info!(
                    id = %opp.id,
                    net_profit_wei = net_profit,
                    "Published validated arb"
                );
            }
        }

        // Phase 3: Clear dirty flags.
        let mut graph = self.price_graph.write().await;
        graph.clear_dirty();
    }

    /// Get the minimum profit threshold in wei.
    #[allow(dead_code)]
    pub fn min_profit_threshold_wei(&self) -> u128 {
        self.config.min_profit_threshold_wei
    }

    /// Get a reference to the token index.
    #[allow(dead_code)]
    pub fn token_index(&self) -> &Arc<RwLock<TokenIndex>> {
        &self.token_index
    }

    /// Get a reference to the pool registry.
    #[allow(dead_code)]
    pub fn pool_registry(&self) -> &Arc<RwLock<HashMap<Address, PoolMetadata>>> {
        &self.pool_registry
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

        let block = engine.current_block().read().await;
        assert_eq!(block.number, 18_500_000);
        assert_eq!(block.timestamp, 1_700_500_000);
        assert_eq!(block.base_fee, 25_000_000_000);
    }

    #[tokio::test]
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
        let block = engine.current_block().read().await;
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
        let ti = engine.token_index.read().await;
        assert_eq!(ti.len(), 2);
        assert!(ti.contains(&token0));
        assert!(ti.contains(&token1));

        // Verify pool registry has the pool.
        let reg = engine.pool_registry.read().await;
        assert!(reg.contains_key(&pool));
        let meta = reg.get(&pool).unwrap();
        assert_eq!(meta.protocol, ProtocolType::UniswapV2);
        assert_eq!(meta.fee_bps, 30);

        // Verify graph has 2 edges (bidirectional).
        let graph = engine.price_graph.read().await;
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

        // Clear dirty from registration.
        {
            let mut graph = engine.price_graph.write().await;
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

        // Graph should be dirty after the update.
        let graph = engine.price_graph.read().await;
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
            let mut graph = engine.price_graph.write().await;
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

        let graph = engine.price_graph.read().await;
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
        let reg = engine.pool_registry.read().await;
        assert!(reg.contains_key(&pool));
        let meta = reg.get(&pool).unwrap();
        assert_eq!(meta.protocol, ProtocolType::UniswapV2);
        assert_eq!(meta.fee_bps, 30);
    }

    #[tokio::test]
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
            let reg = engine.pool_registry.read().await;
            let meta_ab = reg.get(&pool_ab).unwrap().clone();
            let meta_bc = reg.get(&pool_bc).unwrap().clone();
            let meta_ca = reg.get(&pool_ca).unwrap().clone();
            drop(reg);

            let mut graph = engine.price_graph.write().await;
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
        {
            let mut block = engine.current_block.write().await;
            block.number = 18_000_000;
            block.timestamp = 1_700_000_000;
            block.base_fee = 0;
        }

        // Run detection cycle.
        engine.run_detection_cycle().await;

        // The EVM treats calls to Address::ZERO (no code) as a success,
        // so the simulation passes and the arb gets published.
        // Check that dirty flags were cleared.
        let graph = engine.price_graph.read().await;
        assert!(!graph.has_dirty_edges());

        // With zero gas cost and zero profit threshold, the profitable cycle
        // should be detected, simulated (success on empty account), and published.
        let arb = rx.try_recv().expect("should receive a published arb");
        assert!(!arb.id.is_empty());
        assert!(!arb.hops.is_empty());
    }
}
