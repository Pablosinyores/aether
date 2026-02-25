//! Anvil fork integration tests for the Aether RPC + detection pipeline.
//!
//! These tests require:
//! - `ETH_RPC_URL` environment variable pointing to an Ethereum mainnet RPC
//! - `anvil` binary in PATH (from Foundry)
//!
//! Run with: `cargo test -p aether-integration-tests --test anvil_fork_test`
//!
//! Tests are gated behind both env var and anvil availability checks.

use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{address, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;

use aether_common::types::{PoolId, ProtocolType};
use aether_detector::bellman_ford::BellmanFord;
use aether_ingestion::event_decoder::{self, EventSignatures};
use aether_ingestion::subscription::EventChannels;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

// Well-known mainnet addresses for reference.
#[allow(dead_code)]
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
#[allow(dead_code)]
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// Check if we have the prerequisites: ETH_RPC_URL env and anvil binary.
fn prerequisites_available() -> bool {
    if std::env::var("ETH_RPC_URL").is_err() {
        eprintln!("Skipping anvil fork test: ETH_RPC_URL not set");
        return false;
    }
    match std::process::Command::new("anvil").arg("--version").output() {
        Ok(out) => out.status.success(),
        Err(_) => {
            eprintln!("Skipping anvil fork test: anvil not found in PATH");
            false
        }
    }
}

/// Spawn an Anvil process forking from ETH_RPC_URL.
/// Returns the Anvil process handle and the local RPC URL.
fn spawn_anvil() -> (std::process::Child, String) {
    let rpc_url = std::env::var("ETH_RPC_URL").expect("ETH_RPC_URL must be set");
    let child = std::process::Command::new("anvil")
        .args(["--fork-url", &rpc_url, "--port", "0", "--silent"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn anvil");

    // Anvil with --port 0 picks a random port. We need to read it from output.
    // Anvil defaults to port 8545 when --port is not 0, so let's use a fixed port.
    // Use a random high port to avoid conflicts.
    let port = 18545 + (std::process::id() % 1000) as u16;
    drop(child);

    let child = std::process::Command::new("anvil")
        .args([
            "--fork-url",
            &rpc_url,
            "--port",
            &port.to_string(),
            "--silent",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn anvil");

    let url = format!("http://127.0.0.1:{}", port);
    (child, url)
}

/// Wait for Anvil to be ready by polling eth_blockNumber.
async fn wait_for_anvil(url: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let parsed: url::Url = url.parse().expect("valid URL");
    while start.elapsed() < timeout {
        let provider = ProviderBuilder::new().connect_http(parsed.clone());
        if provider.get_block_number().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Test 1: Connect to Anvil fork and fetch block headers.
/// Verifies that alloy HTTP provider can connect, fetch block number,
/// and retrieve block header with expected fields.
#[tokio::test]
async fn test_anvil_fork_block_fetch() {
    if !prerequisites_available() {
        return;
    }

    let (mut anvil, url) = spawn_anvil();

    let result = async {
        assert!(
            wait_for_anvil(&url, Duration::from_secs(30)).await,
            "Anvil did not start in time"
        );

        let parsed: url::Url = url.parse().unwrap();
        let provider = ProviderBuilder::new().connect_http(parsed);

        // Fetch current block number.
        let block_number = provider
            .get_block_number()
            .await
            .expect("should get block number");
        assert!(block_number > 0, "block number should be positive");

        // Fetch block header.
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await
            .expect("should get block")
            .expect("block should exist");

        assert_eq!(block.header.number, block_number);
        assert!(block.header.timestamp > 0);
        assert!(block.header.gas_limit > 0);

        // base_fee should exist on post-London blocks.
        assert!(
            block.header.base_fee_per_gas.is_some(),
            "base_fee should exist on mainnet fork"
        );
    }
    .await;

    let _ = anvil.kill();
    let _ = anvil.wait();
    result
}

/// Test 2: Fetch logs from a known block range and decode DEX events.
/// Fetches Sync/Swap events from recent blocks on the Anvil fork and
/// verifies they decode correctly via aether-ingestion.
#[tokio::test]
async fn test_anvil_fork_log_fetch_and_decode() {
    if !prerequisites_available() {
        return;
    }

    let (mut anvil, url) = spawn_anvil();

    let result = async {
        assert!(
            wait_for_anvil(&url, Duration::from_secs(30)).await,
            "Anvil did not start in time"
        );

        let parsed: url::Url = url.parse().unwrap();
        let provider = ProviderBuilder::new().connect_http(parsed);

        let block_number = provider
            .get_block_number()
            .await
            .expect("should get block number");

        // Search the last 10 blocks for DEX events.
        let from_block = block_number.saturating_sub(10);
        let event_topics: Vec<B256> = vec![
            EventSignatures::sync_topic(),
            EventSignatures::swap_v3_topic(),
            EventSignatures::token_exchange_topic(),
            EventSignatures::pair_created_topic(),
        ];

        let filter = Filter::new()
            .from_block(from_block)
            .to_block(block_number)
            .event_signature(event_topics);

        let logs = provider
            .get_logs(&filter)
            .await
            .expect("should get logs");

        // On mainnet there should be DEX events in recent blocks.
        // Even if there are none, the query should succeed.
        eprintln!(
            "Found {} DEX event logs in blocks {}-{}",
            logs.len(),
            from_block,
            block_number
        );

        // Decode whatever logs we got.
        let mut decoded_count = 0;
        for log in &logs {
            let topics = log.topics().to_vec();
            let data = log.data().data.to_vec();
            if event_decoder::decode_log(&topics, &data, log.address(), None).is_some() {
                decoded_count += 1;
            }
        }

        eprintln!("Successfully decoded {} out of {} logs", decoded_count, logs.len());

        // All recognized logs should decode successfully.
        // (Some logs may have topics we filter for but invalid data.)
        if !logs.is_empty() {
            assert!(
                decoded_count > 0,
                "Should decode at least one DEX event from mainnet"
            );
        }
    }
    .await;

    let _ = anvil.kill();
    let _ = anvil.wait();
    result
}

/// Test 3: End-to-end pipeline — fetch events from Anvil fork,
/// build price graph, and run Bellman-Ford detection.
/// This exercises the full flow: RPC → decode → graph → detect.
#[tokio::test]
async fn test_anvil_fork_full_pipeline() {
    if !prerequisites_available() {
        return;
    }

    let (mut anvil, url) = spawn_anvil();

    let result = async {
        assert!(
            wait_for_anvil(&url, Duration::from_secs(30)).await,
            "Anvil did not start in time"
        );

        let parsed: url::Url = url.parse().unwrap();
        let provider = ProviderBuilder::new().connect_http(parsed);

        let block_number = provider
            .get_block_number()
            .await
            .expect("should get block number");

        // Fetch events from the last 5 blocks.
        let from_block = block_number.saturating_sub(5);
        let sync_topic = EventSignatures::sync_topic();

        let filter = Filter::new()
            .from_block(from_block)
            .to_block(block_number)
            .event_signature(vec![sync_topic]);

        let logs = provider
            .get_logs(&filter)
            .await
            .expect("should get sync logs");

        // Dispatch events through EventChannels (same path as RpcProvider).
        let channels = Arc::new(EventChannels::new());
        let mut rx = channels.subscribe_pool_updates();

        let raw_logs: Vec<(Address, Vec<B256>, Vec<u8>)> = logs
            .iter()
            .map(|log| {
                (
                    log.address(),
                    log.topics().to_vec(),
                    log.data().data.to_vec(),
                )
            })
            .collect();

        for (address, topics, data) in &raw_logs {
            if let Some(event) = event_decoder::decode_log(topics, data, *address, None) {
                channels.dispatch_pool_update(event);
            }
        }

        // Count dispatched events.
        let mut event_count = 0;
        while rx.try_recv().is_ok() {
            event_count += 1;
        }
        eprintln!("Dispatched {} pool update events", event_count);

        // Build a price graph from the Sync events.
        let mut token_index = TokenIndex::new();
        let mut graph = PriceGraph::new(100); // Pre-allocate for many tokens.

        // Track unique pools we've seen.
        let mut pools_seen = std::collections::HashSet::new();

        for (address, topics, data) in &raw_logs {
            if let Some(event) = event_decoder::decode_log(topics, data, *address, None) {
                match event {
                    event_decoder::PoolEvent::ReserveUpdate {
                        pool,
                        reserve0,
                        reserve1,
                        ..
                    } => {
                        if reserve0 == U256::ZERO || reserve1 == U256::ZERO {
                            continue;
                        }

                        // For Sync events, we don't know token addresses from the event.
                        // Use pool address as proxy for token0/token1 indices.
                        // In production, pool metadata provides the token mapping.
                        if pools_seen.insert(pool) {
                            let idx0 = token_index.get_or_insert(pool);
                            // Use a deterministic "token1" derived from pool.
                            let token1_proxy = Address::from_word(B256::from(pool.into_word()));
                            let idx1 = token_index.get_or_insert(token1_proxy);

                            graph.resize(token_index.len());

                            let r0_f64 = reserve0.to::<u128>() as f64;
                            let r1_f64 = reserve1.to::<u128>() as f64;
                            let rate = r1_f64 / r0_f64 * 0.997;

                            let pool_id = PoolId {
                                address: pool,
                                protocol: ProtocolType::UniswapV2,
                            };
                            graph.add_edge(
                                idx0,
                                idx1,
                                rate,
                                pool_id,
                                pool,
                                ProtocolType::UniswapV2,
                                U256::ZERO,
                            );
                            graph.add_edge(
                                idx1,
                                idx0,
                                r0_f64 / r1_f64 * 0.997,
                                pool_id,
                                pool,
                                ProtocolType::UniswapV2,
                                U256::ZERO,
                            );
                        }
                    }
                    _ => {} // Ignore non-Sync events for this test.
                }
            }
        }

        eprintln!(
            "Built graph with {} edges from {} pools, {} tokens",
            graph.num_edges(),
            pools_seen.len(),
            token_index.len()
        );

        // Run Bellman-Ford detection.
        let bf = BellmanFord::new(5, 3_000_000);
        let cycles = bf.detect_negative_cycles(&graph);
        eprintln!("Detected {} negative cycles", cycles.len());

        // We don't assert specific cycles exist because real mainnet data
        // may or may not have arbitrage. The key assertion is that the
        // full pipeline runs without panicking or errors.
        assert!(
            graph.num_edges() > 0 || logs.is_empty(),
            "Should have edges if there were sync events"
        );
    }
    .await;

    let _ = anvil.kill();
    let _ = anvil.wait();
    result
}
