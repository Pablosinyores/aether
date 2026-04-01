use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use futures::StreamExt;
use tokio::sync::RwLock;

use aether_common::types::NodeState;
use aether_ingestion::config::load_nodes_config;
use aether_ingestion::event_decoder;
use aether_ingestion::event_decoder::EventSignatures;
use aether_ingestion::node_pool::{NodeConfig, NodeConnection, NodePool, NodeType};
use aether_ingestion::subscription::{EventChannels, NewBlockEvent};

/// Configuration for the RPC provider connection
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// RPC endpoint URL (WS preferred, HTTP fallback)
    pub rpc_url: String,
    /// Optional path to nodes.yaml for multi-node pool configuration
    pub nodes_config_path: Option<String>,
    /// Pool addresses to monitor for events (empty = all)
    pub monitored_pools: Vec<Address>,
    /// Reconnect delay base (exponential backoff)
    pub reconnect_delay: Duration,
    /// Maximum reconnect attempts before giving up
    pub max_reconnect_attempts: u32,
    /// Health check interval
    pub health_check_interval: Duration,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            rpc_url: std::env::var("ETH_RPC_URL")
                .unwrap_or_else(|_| "http://localhost:8545".to_string()),
            nodes_config_path: std::env::var("AETHER_NODES_CONFIG").ok(),
            monitored_pools: vec![],
            reconnect_delay: Duration::from_secs(1),
            max_reconnect_attempts: 10,
            health_check_interval: Duration::from_secs(30),
        }
    }
}

/// Infer the node transport type from the URL scheme.
fn infer_node_type(url: &str) -> NodeType {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        NodeType::WebSocket
    } else if url.starts_with('/') || url.ends_with(".ipc") {
        NodeType::Ipc
    } else {
        NodeType::Http
    }
}

/// RPC provider that bridges Ethereum events to the ingestion EventChannels.
///
/// Supports WebSocket (native subscriptions), IPC (native subscriptions),
/// and HTTP (polling fallback) transports. When configured with a
/// `nodes_config_path`, manages a pool of nodes with automatic failover.
pub struct RpcProvider {
    config: ProviderConfig,
    event_channels: Arc<EventChannels>,
    node_pool: NodePool,
}

impl RpcProvider {
    /// Create a new `RpcProvider`.
    ///
    /// If `config.nodes_config_path` is set, loads the multi-node pool from
    /// the YAML config file. Otherwise, creates a single-node pool from
    /// `config.rpc_url` with the transport type inferred from the URL scheme.
    pub fn new(config: ProviderConfig, event_channels: Arc<EventChannels>) -> Self {
        let node_pool = match &config.nodes_config_path {
            Some(path) => match load_nodes_config(path) {
                Ok((configs, min_healthy)) => {
                    info!(
                        path = %path,
                        nodes = configs.len(),
                        min_healthy,
                        "Loaded node pool from config"
                    );
                    NodePool::new(configs, min_healthy)
                }
                Err(e) => {
                    warn!(
                        path = %path,
                        error = %e,
                        "Failed to load nodes config, falling back to rpc_url"
                    );
                    Self::single_node_pool(&config.rpc_url)
                }
            },
            None => Self::single_node_pool(&config.rpc_url),
        };

        Self {
            config,
            event_channels,
            node_pool,
        }
    }

    /// Build a single-node `NodePool` from a URL, inferring the transport type.
    fn single_node_pool(url: &str) -> NodePool {
        let node_type = infer_node_type(url);
        let node_config = NodeConfig {
            name: "default".to_string(),
            url: url.to_string(),
            node_type,
            priority: 0,
            max_retries: 5,
            health_check_interval: Duration::from_secs(30),
        };
        NodePool::new(vec![node_config], 1)
    }

    /// Main provider loop with automatic failover across the node pool.
    ///
    /// Selects the best available node, connects using the appropriate
    /// transport, and runs the event loop. On failure, marks the node as
    /// degraded/failed and retries with the next best node.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!("RpcProvider starting");

        let mut attempt = 0u32;

        loop {
            if *shutdown.borrow() {
                info!("RpcProvider shutting down before connection attempt");
                break;
            }

            match self.node_pool.best_node().await {
                Some(node) => {
                    let (node_type, node_url) = {
                        let n = node.read().await;
                        (n.config.node_type.clone(), n.config.url.clone())
                    };

                    info!(url = %node_url, transport = ?node_type, "Connecting to node");

                    let result = match node_type {
                        NodeType::WebSocket => {
                            self.connect_ws(&node_url, &node, &mut shutdown).await
                        }
                        NodeType::Ipc => {
                            self.connect_ipc(&node_url, &node, &mut shutdown).await
                        }
                        NodeType::Http => {
                            self.connect_http(&node_url, &node, &mut shutdown).await
                        }
                    };

                    match result {
                        Ok(()) => break, // Graceful shutdown
                        Err(e) => {
                            node.write().await.record_failure();
                            attempt += 1;
                            let delay = self.node_pool.backoff_delay(attempt);
                            warn!(
                                attempt,
                                delay_ms = delay.as_millis() as u64,
                                error = %e,
                                "Connection failed, reconnecting"
                            );
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                Ok(()) = shutdown.changed() => {
                                    if *shutdown.borrow() { break; }
                                }
                            }
                        }
                    }
                }
                None => {
                    // All nodes unhealthy
                    attempt += 1;
                    if attempt >= self.config.max_reconnect_attempts {
                        error!("All nodes failed, max reconnect attempts reached");
                        break;
                    }

                    let delay = self.node_pool.backoff_delay(attempt);
                    warn!(attempt, "All nodes unhealthy, waiting before retry");

                    // Reset all nodes to Connected so they can be retried
                    for node in self.node_pool.all_nodes() {
                        let mut n = node.write().await;
                        n.consecutive_failures = 0;
                        n.transition(NodeState::Connected);
                    }

                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        Ok(()) = shutdown.changed() => {
                            if *shutdown.borrow() { break; }
                        }
                    }
                }
            }
        }

        info!("RpcProvider exited");
    }

    /// Connect via WebSocket and run native subscriptions.
    async fn connect_ws(
        &self,
        url: &str,
        node: &Arc<RwLock<NodeConnection>>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let ws_connect = alloy::providers::WsConnect::new(url);
        let provider = ProviderBuilder::new().connect_ws(ws_connect).await?;

        let initial_block = provider.get_block_number().await?;
        info!(block = initial_block, "WebSocket provider connected");
        node.write().await.record_success(0, initial_block);

        self.run_subscription_loop(provider, node, shutdown).await
    }

    /// Connect via IPC (Unix domain socket) and run native subscriptions.
    async fn connect_ipc(
        &self,
        path: &str,
        node: &Arc<RwLock<NodeConnection>>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let ipc_connect = alloy::providers::IpcConnect::new(path.to_string());
        let provider = ProviderBuilder::new().connect_ipc(ipc_connect).await?;

        let initial_block = provider.get_block_number().await?;
        info!(block = initial_block, "IPC provider connected");
        node.write().await.record_success(0, initial_block);

        self.run_subscription_loop(provider, node, shutdown).await
    }

    /// Shared subscription loop for push-based transports (WS and IPC).
    ///
    /// Subscribes to `newHeads` and DEX event logs, dispatching events
    /// through `EventChannels` as they arrive.
    async fn run_subscription_loop<P>(
        &self,
        provider: P,
        node: &Arc<RwLock<NodeConnection>>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        P: Provider + Clone + 'static,
    {
        let block_sub = provider.subscribe_blocks().await?;
        let mut block_stream = block_sub.into_stream();

        // When monitored_pools is non-empty, scope the filter to those addresses only.
        // When empty, receive events from all contracts (pool discovery mode).
        let mut log_filter = Filter::new().event_signature(self.event_topics());
        if !self.config.monitored_pools.is_empty() {
            log_filter = log_filter.address(self.config.monitored_pools.clone());
        }
        let log_sub = provider.subscribe_logs(&log_filter).await?;
        let mut log_stream = log_sub.into_stream();

        info!("Subscriptions active (newHeads + logs)");

        loop {
            tokio::select! {
                block_opt = block_stream.next() => {
                    match block_opt {
                        Some(block) => {
                            let number = block.inner.number;
                            let timestamp = block.inner.timestamp;
                            let base_fee = block.inner.base_fee_per_gas.unwrap_or(0) as u128;
                            let gas_limit = block.inner.gas_limit;
                            debug!(block = number, "Block received via subscription");
                            self.dispatch_block(number, timestamp, base_fee, gas_limit);
                            node.write().await.record_success(0, number);
                        }
                        None => {
                            return Err("Block subscription stream ended".into());
                        }
                    }
                }
                log_opt = log_stream.next() => {
                    match log_opt {
                        Some(log) => {
                            self.process_single_log(&log);
                        }
                        None => {
                            return Err("Log subscription stream ended".into());
                        }
                    }
                }
                Ok(()) = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Connect via HTTP and run the polling-based event loop.
    ///
    /// HTTP does not support native subscriptions, so this falls back to
    /// polling `eth_getBlockByNumber` and `eth_getLogs` every second.
    async fn connect_http(
        &self,
        url: &str,
        node: &Arc<RwLock<NodeConnection>>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!("HTTP transport detected -- falling back to polling mode (latency ~1s)");

        let parsed_url: url::Url = url.parse()?;
        let provider = ProviderBuilder::new().connect_http(parsed_url);

        let initial_block = provider.get_block_number().await?;
        info!(block = initial_block, "HTTP provider connected (polling mode)");
        node.write().await.record_success(0, initial_block);

        let poll_interval = Duration::from_secs(1);
        let mut last_block = initial_block;
        let event_topics = self.event_topics();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {
                    let current_block = match provider.get_block_number().await {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(error = %e, "Failed to get block number");
                            continue;
                        }
                    };

                    if current_block <= last_block {
                        continue;
                    }

                    debug!(block = current_block, prev = last_block, "New block detected");

                    let block_opt = match provider.get_block_by_number(
                        alloy::eips::BlockNumberOrTag::Number(current_block),
                    ).await {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(error = %e, block = current_block, "Failed to get block");
                            continue;
                        }
                    };

                    if let Some(block) = block_opt {
                        let timestamp = block.header.timestamp;
                        let base_fee = block.header.base_fee_per_gas.unwrap_or(0) as u128;
                        let gas_limit = block.header.gas_limit;

                        self.dispatch_block(current_block, timestamp, base_fee, gas_limit);
                        node.write().await.record_success(0, current_block);

                        // Fetch logs for all blocks since last_block to avoid
                        // dropping events when multiple blocks arrive between polls.
                        // When monitored_pools is non-empty, scope to those addresses only.
                        let mut filter = Filter::new()
                            .from_block(last_block + 1)
                            .to_block(current_block)
                            .event_signature(event_topics.clone());
                        if !self.config.monitored_pools.is_empty() {
                            filter = filter.address(self.config.monitored_pools.clone());
                        }

                        match provider.get_logs(&filter).await {
                            Ok(logs) => {
                                if !logs.is_empty() {
                                    debug!(
                                        count = logs.len(),
                                        block = current_block,
                                        "Processing DEX event logs"
                                    );
                                    let decoded_logs: Vec<(Address, Vec<B256>, Vec<u8>)> = logs
                                        .iter()
                                        .map(|log| {
                                            (
                                                log.address(),
                                                log.topics().to_vec(),
                                                log.data().data.to_vec(),
                                            )
                                        })
                                        .collect();
                                    self.process_logs(&decoded_logs);
                                }
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    block = current_block,
                                    "Failed to get logs"
                                );
                            }
                        }
                    }

                    last_block = current_block;
                }
                Ok(()) = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Known DEX event topic signatures for log filtering.
    fn event_topics(&self) -> Vec<B256> {
        vec![
            EventSignatures::sync_topic(),
            EventSignatures::swap_v2_topic(),
            EventSignatures::swap_v3_topic(),
            EventSignatures::token_exchange_topic(),
            EventSignatures::pair_created_topic(),
        ]
    }

    /// Decode and dispatch a single log received from a subscription stream.
    /// Borrows directly from the log to avoid heap allocations on the hot path.
    fn process_single_log(&self, log: &alloy::rpc::types::Log) {
        let address = log.address();
        let topics = log.topics();
        let data = &log.data().data;
        if let Some(event) = event_decoder::decode_log(topics, data, address, None) {
            self.event_channels.dispatch_pool_update(event);
        }
    }

    /// Dispatch a new block event to the event channels.
    pub fn dispatch_block(&self, number: u64, timestamp: u64, base_fee: u128, gas_limit: u64) {
        self.event_channels.dispatch_new_block(NewBlockEvent {
            block_number: number,
            timestamp,
            base_fee,
            gas_limit,
        });
    }

    /// Process raw logs from a block and dispatch decoded pool events.
    pub fn process_logs(&self, logs: &[(Address, Vec<B256>, Vec<u8>)]) {
        for (address, topics, data) in logs {
            if let Some(event) = event_decoder::decode_log(topics, data, *address, None) {
                self.event_channels.dispatch_pool_update(event);
            }
        }
    }

    /// Get the configured RPC URL.
    #[allow(dead_code)]
    pub fn rpc_url(&self) -> &str {
        &self.config.rpc_url
    }

    /// Check if the provider is configured (has a non-empty URL).
    #[allow(dead_code)]
    pub fn is_configured(&self) -> bool {
        !self.config.rpc_url.is_empty()
    }

    /// Get a reference to the underlying node pool.
    #[allow(dead_code)]
    pub fn node_pool(&self) -> &NodePool {
        &self.node_pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    #[test]
    fn test_provider_config_default() {
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        assert_eq!(config.rpc_url, "http://localhost:8545");
        assert!(config.monitored_pools.is_empty());
        assert_eq!(config.reconnect_delay, Duration::from_secs(1));
        assert_eq!(config.max_reconnect_attempts, 10);
    }

    #[test]
    fn test_provider_config_nodes_config_path_defaults_to_env() {
        std::env::remove_var("AETHER_NODES_CONFIG");
        let config = ProviderConfig::default();
        assert!(config.nodes_config_path.is_none());
    }

    #[test]
    fn test_provider_creation() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "ws://localhost:8546".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, channels);
        assert_eq!(provider.rpc_url(), "ws://localhost:8546");
        assert!(provider.is_configured());
    }

    #[test]
    fn test_provider_not_configured_with_empty_url() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: String::new(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, channels);
        assert!(!provider.is_configured());
    }

    #[test]
    fn test_dispatch_block() {
        let channels = Arc::new(EventChannels::new());
        let mut rx = channels.subscribe_new_blocks();

        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, Arc::clone(&channels));

        provider.dispatch_block(18_000_000, 1_700_000_000, 30_000_000_000, 30_000_000);

        let event = rx.try_recv().expect("should receive block event");
        assert_eq!(event.block_number, 18_000_000);
        assert_eq!(event.timestamp, 1_700_000_000);
        assert_eq!(event.base_fee, 30_000_000_000);
        assert_eq!(event.gas_limit, 30_000_000);
    }

    #[test]
    fn test_process_logs_sync_event() {
        let channels = Arc::new(EventChannels::new());
        let mut rx = channels.subscribe_pool_updates();

        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, Arc::clone(&channels));

        let pool_addr = Address::repeat_byte(0xAA);
        let topics = vec![EventSignatures::sync_topic()];

        let reserve0 = U256::from(1_000_000_000_000_000_000u64);
        let reserve1 = U256::from(2_000_000_000u64);
        let mut data = Vec::new();
        data.extend_from_slice(&reserve0.to_be_bytes::<32>());
        data.extend_from_slice(&reserve1.to_be_bytes::<32>());

        provider.process_logs(&[(pool_addr, topics, data)]);

        let event = rx.try_recv().expect("should receive pool event");
        match event {
            aether_ingestion::event_decoder::PoolEvent::ReserveUpdate { pool, .. } => {
                assert_eq!(pool, pool_addr);
            }
            other => panic!("Expected ReserveUpdate, got {:?}", other),
        }
    }

    #[test]
    fn test_process_logs_unknown_event_ignored() {
        let channels = Arc::new(EventChannels::new());
        let mut rx = channels.subscribe_pool_updates();

        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, Arc::clone(&channels));

        let unknown_topic = B256::repeat_byte(0xFF);
        provider.process_logs(&[(Address::ZERO, vec![unknown_topic], vec![0u8; 64])]);

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_provider_run_with_shutdown() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            max_reconnect_attempts: 5,
            reconnect_delay: Duration::from_millis(200),
            ..ProviderConfig::default()
        };
        let provider = Arc::new(RpcProvider::new(config, channels));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let provider_clone = Arc::clone(&provider);
        let handle = tokio::spawn(async move {
            provider_clone.run(shutdown_rx).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(true);

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("provider should shut down within timeout")
            .expect("provider task should not panic");
    }

    #[test]
    fn test_process_logs_multiple() {
        let channels = Arc::new(EventChannels::new());
        let mut rx = channels.subscribe_pool_updates();

        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, Arc::clone(&channels));

        let pool1 = Address::repeat_byte(0x01);
        let pool2 = Address::repeat_byte(0x02);
        let sync_topic = EventSignatures::sync_topic();

        let mut data = Vec::new();
        let r = U256::from(1000u64);
        data.extend_from_slice(&r.to_be_bytes::<32>());
        data.extend_from_slice(&r.to_be_bytes::<32>());

        provider.process_logs(&[
            (pool1, vec![sync_topic], data.clone()),
            (pool2, vec![sync_topic], data),
        ]);

        let e1 = rx.try_recv().expect("should receive first event");
        let e2 = rx.try_recv().expect("should receive second event");

        match (e1, e2) {
            (
                aether_ingestion::event_decoder::PoolEvent::ReserveUpdate { pool: p1, .. },
                aether_ingestion::event_decoder::PoolEvent::ReserveUpdate { pool: p2, .. },
            ) => {
                assert_eq!(p1, pool1);
                assert_eq!(p2, pool2);
            }
            _ => panic!("Expected two ReserveUpdate events"),
        }
    }

    // ── Transport inference tests ──

    #[test]
    fn test_infer_node_type_websocket() {
        assert_eq!(infer_node_type("ws://localhost:8546"), NodeType::WebSocket);
        assert_eq!(infer_node_type("wss://eth-mainnet.g.alchemy.com/v2/key"), NodeType::WebSocket);
    }

    #[test]
    fn test_infer_node_type_ipc() {
        assert_eq!(infer_node_type("/tmp/reth.ipc"), NodeType::Ipc);
        assert_eq!(infer_node_type("/var/run/geth.ipc"), NodeType::Ipc);
        assert_eq!(infer_node_type("path/to/node.ipc"), NodeType::Ipc);
    }

    #[test]
    fn test_infer_node_type_http() {
        assert_eq!(infer_node_type("http://localhost:8545"), NodeType::Http);
        assert_eq!(infer_node_type("https://mainnet.infura.io/v3/key"), NodeType::Http);
    }

    #[test]
    fn test_infer_node_type_unknown_defaults_to_http() {
        assert_eq!(infer_node_type("some-random-string"), NodeType::Http);
    }

    // ── Node pool construction tests ──

    #[test]
    fn test_single_node_pool_from_ws_url() {
        let pool = RpcProvider::single_node_pool("ws://localhost:8546");
        assert_eq!(pool.all_nodes().len(), 1);
    }

    #[test]
    fn test_single_node_pool_from_http_url() {
        let pool = RpcProvider::single_node_pool("http://localhost:8545");
        assert_eq!(pool.all_nodes().len(), 1);
    }

    #[test]
    fn test_single_node_pool_from_ipc_path() {
        let pool = RpcProvider::single_node_pool("/tmp/reth.ipc");
        assert_eq!(pool.all_nodes().len(), 1);
    }

    #[tokio::test]
    async fn test_provider_with_nodes_config_file() {
        use std::io::Write;

        let dir = std::env::temp_dir().join("aether_provider_test");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("nodes.yaml");

        let yaml = r#"
nodes:
  - name: "ws-primary"
    url: "wss://example.com"
    type: "websocket"
    priority: 1
  - name: "ipc-local"
    url: "/tmp/reth.ipc"
    type: "ipc"
    priority: 0
  - name: "http-fallback"
    url: "http://localhost:8545"
    type: "http"
    priority: 2
min_healthy_nodes: 1
"#;
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(yaml.as_bytes()).expect("write temp file");

        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            nodes_config_path: Some(path.to_str().expect("valid path").to_string()),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, channels);

        assert_eq!(provider.node_pool().all_nodes().len(), 3);

        let best = provider.node_pool().best_node().await.expect("should have best node");
        let best_read = best.read().await;
        assert_eq!(best_read.config.name, "ipc-local");

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn test_provider_falls_back_on_invalid_config_path() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            nodes_config_path: Some("/nonexistent/path/nodes.yaml".to_string()),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, channels);
        assert_eq!(provider.node_pool().all_nodes().len(), 1);
    }

    #[test]
    fn test_event_topics_returns_known_signatures() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            ..ProviderConfig::default()
        };
        let provider = RpcProvider::new(config, channels);

        let topics = provider.event_topics();
        assert_eq!(topics.len(), 5);
        assert_eq!(topics[0], EventSignatures::sync_topic());
        assert_eq!(topics[1], EventSignatures::swap_v2_topic());
        assert_eq!(topics[2], EventSignatures::swap_v3_topic());
        assert_eq!(topics[3], EventSignatures::token_exchange_topic());
        assert_eq!(topics[4], EventSignatures::pair_created_topic());
    }
}
