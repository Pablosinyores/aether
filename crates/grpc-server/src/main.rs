use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;

mod engine;
mod metrics;
mod pipeline;
mod service;

use aether_grpc_server::provider::{ProviderConfig, RpcProvider};
use engine::{AetherEngine, EngineConfig};
use metrics::{start_metrics_server, EngineMetrics};
use service::aether_proto::arb_service_server::ArbServiceServer;
use service::aether_proto::control_service_server::ControlServiceServer;
use service::aether_proto::health_service_server::HealthServiceServer;
use service::{ArbServiceImpl, ControlServiceImpl, EngineState, HealthServiceImpl};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env file into the process environment so std::env::var() picks
    // up ETH_RPC_URL, ALCHEMY_API_KEY, etc. Silently ignored if .env is missing.
    let _ = dotenvy::dotenv();

    // Initialize structured logging with tracing.
    // Respects RUST_LOG env var; defaults to `info` level.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("Starting Aether gRPC server");

    // Shared engine state — single source of truth for system health,
    // current block number, and active pool count.
    let state = Arc::new(RwLock::new(EngineState::default()));

    let metrics = Arc::new(EngineMetrics::new());
    start_metrics_server(Arc::clone(&metrics));

    // Construct gRPC service implementations, each holding an Arc to the
    // shared state.
    let arb_service = ArbServiceImpl::new(Arc::clone(&state));
    let health_service = HealthServiceImpl::new(Arc::clone(&state));

    // Create the AetherEngine with a broadcast sender connected to the
    // ArbService's stream.
    let arb_tx = arb_service.arb_sender();

    // EXECUTOR_ADDRESS is mandatory — without it the engine builds calldata targeting
    // the zero address, which is a silent no-op on-chain. Fail-fast at startup.
    let executor_address = parse_executor_address()?;

    let engine_config = EngineConfig {
        rpc_url: std::env::var("ETH_RPC_URL").ok(),
        executor_address,
        ..EngineConfig::default()
    };
    if engine_config.rpc_url.is_some() {
        info!("ETH_RPC_URL set — engine will use RPC-backed fork simulation");
    } else {
        info!("ETH_RPC_URL not set — engine will use empty-state simulation");
    }
    info!(executor_address = %engine_config.executor_address, "Executor contract target");
    let engine = Arc::new(AetherEngine::new_with_metrics(
        engine_config,
        arb_tx,
        Arc::clone(&metrics),
    ));

    // ControlService needs a handle to the engine for hot-reload support.
    let control_service = ControlServiceImpl::new(Arc::clone(&state), Arc::clone(&engine));

    // Bootstrap pools from config file at startup.
    // Supports AETHER_POOLS_CONFIG env var to override the default path,
    // so the binary works regardless of the working directory.
    let pools_config = std::env::var("AETHER_POOLS_CONFIG")
        .unwrap_or_else(|_| "config/pools.toml".to_string());
    let pool_count = engine.bootstrap_pools(&pools_config).await;
    info!(pool_count, path = %pools_config, "Pools loaded at startup");

    // Fetch initial on-chain reserves so the price graph has real edges.
    engine.fetch_initial_reserves().await;

    // Create the RpcProvider, sharing the engine's event channels so events
    // flow from the provider into the engine's event loop.
    // Reads AETHER_NODES_CONFIG for multi-node pool config, falls back to ETH_RPC_URL.
    let provider_config = ProviderConfig::default();
    if provider_config.nodes_config_path.is_some() {
        info!("AETHER_NODES_CONFIG set — provider will use multi-node pool");
    }
    let provider = Arc::new(RpcProvider::new(
        provider_config,
        Arc::clone(engine.event_channels()),
    ));

    // Shutdown coordination via watch channel.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn the engine in a background task.
    let engine_clone = Arc::clone(&engine);
    let engine_shutdown_rx = shutdown_rx.clone();
    let engine_handle = tokio::spawn(async move {
        engine_clone.run(engine_shutdown_rx).await;
    });

    // Spawn the RPC provider in a background task.
    let provider_clone = Arc::clone(&provider);
    let provider_shutdown_rx = shutdown_rx.clone();
    let provider_handle = tokio::spawn(async move {
        provider_clone.run(provider_shutdown_rx).await;
    });

    // Read the listen address from the environment so the systemd unit and
    // the binary always agree.  Default to localhost TCP for development.
    //
    // Production (UDS): GRPC_ADDRESS=unix:///var/run/aether/engine.sock
    // Development (TCP): GRPC_ADDRESS=[::1]:50051  (default)
    let addr_str =
        std::env::var("GRPC_ADDRESS").unwrap_or_else(|_| "[::1]:50051".to_string());

    let server = Server::builder()
        .add_service(ArbServiceServer::new(arb_service))
        .add_service(HealthServiceServer::new(health_service))
        .add_service(ControlServiceServer::new(control_service));

    let server_result = if let Some(uds_path) = addr_str.strip_prefix("unix://") {
        // Unix Domain Socket transport for production.
        #[cfg(unix)]
        {
            // Remove stale socket file if it exists from a previous run.
            match std::fs::remove_file(uds_path) {
                Ok(()) => info!(path = %uds_path, "Removed stale UDS socket"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %uds_path, error = %e, "Failed to remove stale UDS socket"),
            }

            // Ensure parent directory exists.
            if let Some(parent) = std::path::Path::new(uds_path).parent() {
                std::fs::create_dir_all(parent)?;
            }

            let uds = UnixListener::bind(uds_path)?;
            // Restrict socket access to the process owner — UDS bypasses
            // network-layer controls (iptables, mTLS), so file permissions
            // are the only access control for ControlService endpoints.
            std::fs::set_permissions(
                uds_path,
                PermissionsExt::from_mode(0o600),
            )?;
            info!(path = %uds_path, "gRPC server listening on UDS");
            let stream = UnixListenerStream::new(uds);
            server.serve_with_incoming(stream).await.map_err(|e| {
                error!(error = %e, "gRPC server failed");
                e
            })
        }
        #[cfg(not(unix))]
        {
            return Err(format!(
                "UDS transport (unix://) is not supported on this platform: {addr_str}"
            )
            .into());
        }
    } else {
        // TCP transport for development / non-UDS configs.
        let addr = tokio::net::lookup_host(&addr_str)
            .await?
            .next()
            .ok_or_else(|| format!("could not resolve GRPC_ADDRESS: {addr_str}"))?;
        info!(%addr, "gRPC server listening on TCP");
        server.serve(addr).await.map_err(|e| {
            error!(error = %e, "gRPC server failed");
            e
        })
    };

    // Clean up UDS socket file on shutdown.
    #[cfg(unix)]
    if let Some(uds_path) = addr_str.strip_prefix("unix://") {
        let _ = std::fs::remove_file(uds_path);
    }

    // Signal the engine and provider to shut down.
    let _ = shutdown_tx.send(true);

    // Wait for the engine and provider to finish.
    if let Err(e) = engine_handle.await {
        error!(error = %e, "Engine task panicked");
    }
    if let Err(e) = provider_handle.await {
        error!(error = %e, "Provider task panicked");
    }

    server_result?;

    Ok(())
}

/// Parse and validate the `EXECUTOR_ADDRESS` env var at startup.
///
/// Fails when the variable is unset, cannot be parsed as an Ethereum address, or is the
/// zero address — all three are deployment misconfigurations, not recoverable runtime states.
fn parse_executor_address() -> Result<Address, Box<dyn std::error::Error>> {
    let raw = std::env::var("EXECUTOR_ADDRESS")
        .map_err(|_| "EXECUTOR_ADDRESS env var is required (on-chain AetherExecutor contract)")?;
    let addr = Address::from_str(raw.trim())
        .map_err(|e| format!("EXECUTOR_ADDRESS='{raw}' is not a valid Ethereum address: {e}"))?;
    if addr == Address::ZERO {
        return Err("EXECUTOR_ADDRESS is the zero address — set it to the deployed AetherExecutor proxy or contract".into());
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env mutations must be serialized across tests to avoid cross-thread races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_executor_address_missing_env_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        // SAFETY: set_var/remove_var are marked unsafe in the 2024 edition; the lock
        // above serializes access across this module's tests.
        unsafe { std::env::remove_var("EXECUTOR_ADDRESS"); }
        assert!(parse_executor_address().is_err());
    }

    #[test]
    fn parse_executor_address_invalid_hex_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("EXECUTOR_ADDRESS", "not-an-address"); }
        assert!(parse_executor_address().is_err());
        unsafe { std::env::remove_var("EXECUTOR_ADDRESS"); }
    }

    #[test]
    fn parse_executor_address_zero_is_error() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(
                "EXECUTOR_ADDRESS",
                "0x0000000000000000000000000000000000000000",
            );
        }
        assert!(parse_executor_address().is_err());
        unsafe { std::env::remove_var("EXECUTOR_ADDRESS"); }
    }

    #[test]
    fn parse_executor_address_valid() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(
                "EXECUTOR_ADDRESS",
                "0x1111111111111111111111111111111111111111",
            );
        }
        let got = parse_executor_address().expect("valid address parses");
        assert_eq!(got.to_string().to_lowercase(), "0x1111111111111111111111111111111111111111");
        unsafe { std::env::remove_var("EXECUTOR_ADDRESS"); }
    }
}
