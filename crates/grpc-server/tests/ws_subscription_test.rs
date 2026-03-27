//! Integration tests for WebSocket subscription support in `RpcProvider`.
//!
//! These tests spawn local Anvil instances (no fork, empty chain) to verify
//! that `RpcProvider` correctly subscribes to `newHeads` over WebSocket,
//! reconnects after a node restart, and falls back to HTTP polling when
//! given an HTTP URL.
//!
//! Prerequisites: `anvil` must be installed and available in `$PATH`.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::{Provider, ProviderBuilder};
use tokio::sync::watch;
use tokio::time::timeout;

use aether_grpc_server::provider::{ProviderConfig, RpcProvider};
use aether_ingestion::subscription::EventChannels;

// ── Helpers ──────────────────────────────────────────────────────────

/// Spawn an Anvil instance on the given port with no fork (empty chain).
fn spawn_anvil(port: u16) -> Child {
    Command::new("anvil")
        .args(["--port", &port.to_string(), "--silent"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn anvil -- is it installed?")
}

/// Poll the HTTP endpoint until Anvil responds to `eth_blockNumber`.
async fn wait_for_anvil(http_url: &str, timeout_dur: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout_dur {
        if let Ok(parsed) = http_url.parse::<url::Url>() {
            let provider = ProviderBuilder::new().connect_http(parsed);
            if provider.get_block_number().await.is_ok() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Derive a unique port per test to avoid collisions across parallel runs.
///
/// `offset` should be different for each test function.
fn test_port(offset: u16) -> u16 {
    8600 + offset + (std::process::id() % 100) as u16
}

/// Mine a single block on Anvil by calling `evm_mine` via HTTP.
async fn mine_block(http_url: &str) {
    let parsed: url::Url = http_url.parse().expect("valid HTTP URL for mining");
    let provider = ProviderBuilder::new().connect_http(parsed);
    provider
        .raw_request::<_, ()>("evm_mine".into(), ())
        .await
        .expect("evm_mine should succeed");
}

/// Check that `anvil` is available in `$PATH`. Returns `false` (and
/// prints a skip message) when it is missing.
fn anvil_available() -> bool {
    match Command::new("anvil").arg("--version").output() {
        Ok(out) if out.status.success() => true,
        _ => {
            eprintln!("Skipping test: anvil not found in PATH");
            false
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

/// Verify that `RpcProvider` receives `NewBlockEvent` when Anvil mines a
/// block over a WebSocket connection.
#[tokio::test]
async fn test_ws_subscription_receives_blocks() {
    if !anvil_available() {
        return;
    }

    let port = test_port(0);
    let http_url = format!("http://127.0.0.1:{port}");
    let ws_url = format!("ws://127.0.0.1:{port}");

    // 1. Spawn Anvil and wait for readiness.
    let mut anvil = spawn_anvil(port);
    assert!(
        wait_for_anvil(&http_url, Duration::from_secs(10)).await,
        "Anvil did not become ready in time"
    );

    // 2. Create event channels and subscribe to new blocks.
    let channels = Arc::new(EventChannels::new());
    let mut block_rx = channels.subscribe_new_blocks();

    // 3. Create and start the RpcProvider.
    let config = ProviderConfig {
        rpc_url: ws_url,
        reconnect_delay: Duration::from_millis(200),
        max_reconnect_attempts: 5,
        ..ProviderConfig::default()
    };
    let provider = Arc::new(RpcProvider::new(config, Arc::clone(&channels)));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let provider_clone = Arc::clone(&provider);
    let provider_handle = tokio::spawn(async move {
        provider_clone.run(shutdown_rx).await;
    });

    // Give the provider time to connect and subscribe.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 4. Mine a block via the HTTP endpoint.
    mine_block(&http_url).await;

    // 5. Wait for the block event.
    let event = timeout(Duration::from_secs(5), block_rx.recv())
        .await
        .expect("timed out waiting for block event")
        .expect("block channel closed unexpectedly");

    assert!(
        event.block_number >= 1,
        "Expected block number >= 1, got {}",
        event.block_number
    );

    // 6. Cleanup.
    let _ = shutdown_tx.send(true);
    let _ = timeout(Duration::from_secs(5), provider_handle).await;
    anvil.kill().ok();
    anvil.wait().ok();
}

/// Verify that `RpcProvider` reconnects after Anvil is killed and
/// restarted on the same port, and that it resumes receiving block events.
#[tokio::test]
async fn test_ws_reconnection_on_disconnect() {
    if !anvil_available() {
        return;
    }

    let port = test_port(10);
    let http_url = format!("http://127.0.0.1:{port}");
    let ws_url = format!("ws://127.0.0.1:{port}");

    // 1. First Anvil instance.
    let mut anvil = spawn_anvil(port);
    assert!(
        wait_for_anvil(&http_url, Duration::from_secs(10)).await,
        "Anvil did not become ready in time"
    );

    let channels = Arc::new(EventChannels::new());
    let mut block_rx = channels.subscribe_new_blocks();

    let config = ProviderConfig {
        rpc_url: ws_url,
        reconnect_delay: Duration::from_millis(500),
        max_reconnect_attempts: 20,
        ..ProviderConfig::default()
    };
    let provider = Arc::new(RpcProvider::new(config, Arc::clone(&channels)));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let provider_clone = Arc::clone(&provider);
    let provider_handle = tokio::spawn(async move {
        provider_clone.run(shutdown_rx).await;
    });

    // Wait for provider to connect.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 2. Mine a block and verify initial reception.
    mine_block(&http_url).await;

    let first_event = timeout(Duration::from_secs(5), block_rx.recv())
        .await
        .expect("timed out waiting for first block event")
        .expect("block channel closed");

    assert!(
        first_event.block_number >= 1,
        "Expected first block >= 1, got {}",
        first_event.block_number
    );

    // 3. Kill Anvil to simulate a disconnect.
    anvil.kill().ok();
    anvil.wait().ok();

    // Give provider time to detect the disconnect and enter reconnect loop.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 4. Respawn Anvil on the same port.
    let mut anvil2 = spawn_anvil(port);
    assert!(
        wait_for_anvil(&http_url, Duration::from_secs(10)).await,
        "Second Anvil instance did not become ready"
    );

    // Wait for the provider to reconnect.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 5. Mine a block on the new instance.
    mine_block(&http_url).await;

    // 6. Verify the provider receives the new block.
    let second_event = timeout(Duration::from_secs(10), block_rx.recv())
        .await
        .expect("timed out waiting for reconnected block event")
        .expect("block channel closed after reconnect");

    assert!(
        second_event.block_number >= 1,
        "Expected reconnected block >= 1, got {}",
        second_event.block_number
    );

    // 7. Cleanup.
    let _ = shutdown_tx.send(true);
    let _ = timeout(Duration::from_secs(5), provider_handle).await;
    anvil2.kill().ok();
    anvil2.wait().ok();
}

/// Verify that the HTTP polling fallback works when given an `http://` URL.
#[tokio::test]
async fn test_http_fallback_still_works() {
    if !anvil_available() {
        return;
    }

    let port = test_port(20);
    let http_url = format!("http://127.0.0.1:{port}");

    // 1. Spawn Anvil and wait for readiness.
    let mut anvil = spawn_anvil(port);
    assert!(
        wait_for_anvil(&http_url, Duration::from_secs(10)).await,
        "Anvil did not become ready in time"
    );

    // 2. Create provider with HTTP URL (triggers polling mode).
    let channels = Arc::new(EventChannels::new());
    let mut block_rx = channels.subscribe_new_blocks();

    let config = ProviderConfig {
        rpc_url: http_url.clone(),
        reconnect_delay: Duration::from_millis(200),
        max_reconnect_attempts: 5,
        ..ProviderConfig::default()
    };
    let provider = Arc::new(RpcProvider::new(config, Arc::clone(&channels)));

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let provider_clone = Arc::clone(&provider);
    let provider_handle = tokio::spawn(async move {
        provider_clone.run(shutdown_rx).await;
    });

    // Give the provider time to connect and start polling.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 3. Mine a block.
    mine_block(&http_url).await;

    // 4. Wait for the block event. The HTTP poller runs on a 1s interval,
    //    so allow up to 3s for the event to arrive.
    let event = timeout(Duration::from_secs(3), block_rx.recv())
        .await
        .expect("timed out waiting for HTTP-polled block event")
        .expect("block channel closed");

    assert!(
        event.block_number >= 1,
        "Expected block number >= 1, got {}",
        event.block_number
    );

    // 5. Cleanup.
    let _ = shutdown_tx.send(true);
    let _ = timeout(Duration::from_secs(5), provider_handle).await;
    anvil.kill().ok();
    anvil.wait().ok();
}
