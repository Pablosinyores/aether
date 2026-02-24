use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;

use aether_ingestion::event_decoder;
use aether_ingestion::event_decoder::EventSignatures;
use aether_ingestion::subscription::{EventChannels, NewBlockEvent};

/// Configuration for the RPC provider connection
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProviderConfig {
    /// RPC endpoint URL (WS preferred, HTTP fallback)
    pub rpc_url: String,
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
            monitored_pools: vec![],
            reconnect_delay: Duration::from_secs(1),
            max_reconnect_attempts: 10,
            health_check_interval: Duration::from_secs(30),
        }
    }
}

/// RPC provider that bridges Ethereum events to the ingestion EventChannels
pub struct RpcProvider {
    config: ProviderConfig,
    event_channels: Arc<EventChannels>,
}

impl RpcProvider {
    pub fn new(config: ProviderConfig, event_channels: Arc<EventChannels>) -> Self {
        Self {
            config,
            event_channels,
        }
    }

    /// Main provider loop with auto-reconnect
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!(url = %self.config.rpc_url, "RpcProvider starting");

        let mut attempt = 0u32;

        loop {
            // Check shutdown before attempting connection
            if *shutdown.borrow() {
                info!("RpcProvider shutting down before connection attempt");
                break;
            }

            match self.connect_and_subscribe(&mut shutdown).await {
                Ok(()) => {
                    info!("RpcProvider connection closed gracefully");
                    break; // Graceful shutdown
                }
                Err(e) => {
                    attempt += 1;
                    if attempt >= self.config.max_reconnect_attempts {
                        error!(
                            attempt,
                            max = self.config.max_reconnect_attempts,
                            error = %e,
                            "RpcProvider max reconnect attempts reached, giving up"
                        );
                        break;
                    }

                    let delay = self.config.reconnect_delay * 2u32.saturating_pow(attempt.min(5));
                    warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "RpcProvider connection failed, reconnecting"
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        Ok(()) = shutdown.changed() => {
                            if *shutdown.borrow() {
                                info!("RpcProvider shutdown during reconnect backoff");
                                break;
                            }
                        }
                    }
                }
            }
        }

        info!("RpcProvider exited");
    }

    /// Connect to the RPC endpoint and subscribe to events.
    /// Returns Ok(()) on graceful shutdown, Err on connection failure.
    async fn connect_and_subscribe(
        &self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(url = %self.config.rpc_url, "Connecting to RPC endpoint");

        let url: url::Url = self.config.rpc_url.parse()?;
        let provider = ProviderBuilder::new().connect_http(url);

        // Verify connection by fetching the current block number.
        let initial_block = provider.get_block_number().await?;
        info!(block = initial_block, "RPC provider connected (polling mode)");

        let poll_interval = Duration::from_secs(1);
        let mut last_block = initial_block;

        // Known DEX event topics for log filtering.
        let event_topics = vec![
            EventSignatures::sync_topic(),
            EventSignatures::swap_v3_topic(),
            EventSignatures::token_exchange_topic(),
            EventSignatures::pair_created_topic(),
        ];

        loop {
            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {
                    // Get current block number.
                    let current_block = match provider.get_block_number().await {
                        Ok(n) => n,
                        Err(e) => {
                            warn!(error = %e, "Failed to get block number");
                            continue;
                        }
                    };

                    if current_block <= last_block {
                        continue; // No new block
                    }

                    debug!(block = current_block, prev = last_block, "New block detected");

                    // Get block header for timestamp, base_fee, gas_limit.
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

                        // Dispatch block event.
                        self.dispatch_block(
                            current_block,
                            timestamp,
                            base_fee,
                            gas_limit,
                        );

                        // Get logs for known DEX events in this block.
                        let filter = Filter::new()
                            .from_block(current_block)
                            .to_block(current_block)
                            .event_signature(event_topics.clone());

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

    /// Dispatch a new block event to the event channels
    pub fn dispatch_block(&self, number: u64, timestamp: u64, base_fee: u128, gas_limit: u64) {
        self.event_channels.dispatch_new_block(NewBlockEvent {
            block_number: number,
            timestamp,
            base_fee,
            gas_limit,
        });
    }

    /// Process raw logs from a block and dispatch decoded pool events
    pub fn process_logs(&self, logs: &[(Address, Vec<B256>, Vec<u8>)]) {
        for (address, topics, data) in logs {
            if let Some(event) = event_decoder::decode_log(topics, data, *address, None) {
                self.event_channels.dispatch_pool_update(event);
            }
        }
    }

    /// Get the configured RPC URL
    #[allow(dead_code)]
    pub fn rpc_url(&self) -> &str {
        &self.config.rpc_url
    }

    /// Check if the provider is configured (has a non-empty URL)
    #[allow(dead_code)]
    pub fn is_configured(&self) -> bool {
        !self.config.rpc_url.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    #[test]
    fn test_provider_config_default() {
        // Use explicit URL for deterministic test
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

        // Build a Sync event log
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

        // Unknown event topic
        let unknown_topic = B256::repeat_byte(0xFF);
        provider.process_logs(&[(Address::ZERO, vec![unknown_topic], vec![0u8; 64])]);

        // Should not receive anything
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_provider_run_with_shutdown() {
        let channels = Arc::new(EventChannels::new());
        let config = ProviderConfig {
            rpc_url: "http://localhost:8545".to_string(),
            max_reconnect_attempts: 1,
            reconnect_delay: Duration::from_millis(10),
            ..ProviderConfig::default()
        };
        let provider = Arc::new(RpcProvider::new(config, channels));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let provider_clone = Arc::clone(&provider);
        let handle = tokio::spawn(async move {
            provider_clone.run(shutdown_rx).await;
        });

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send shutdown
        shutdown_tx.send(true).unwrap();

        // Should complete within a reasonable time
        tokio::time::timeout(Duration::from_secs(2), handle)
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

        // Two Sync events for different pools
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
}
