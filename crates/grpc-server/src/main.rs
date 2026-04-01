use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod engine;
mod pipeline;
mod service;

use aether_grpc_server::provider::{ProviderConfig, RpcProvider};
use engine::{AetherEngine, EngineConfig};
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

    // Construct gRPC service implementations, each holding an Arc to the
    // shared state.
    let arb_service = ArbServiceImpl::new(Arc::clone(&state));
    let health_service = HealthServiceImpl::new(Arc::clone(&state));

    // Create the AetherEngine with a broadcast sender connected to the
    // ArbService's stream.
    let arb_tx = arb_service.arb_sender();
    let engine_config = EngineConfig {
        rpc_url: std::env::var("ETH_RPC_URL").ok(),
        ..EngineConfig::default()
    };
    if engine_config.rpc_url.is_some() {
        info!("ETH_RPC_URL set — engine will use RPC-backed fork simulation");
    } else {
        info!("ETH_RPC_URL not set — engine will use empty-state simulation");
    }
    let engine = Arc::new(AetherEngine::new(engine_config, arb_tx));

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
    let addr_str =
        std::env::var("GRPC_ADDRESS").unwrap_or_else(|_| "[::1]:50051".to_string());
    let addr = tokio::net::lookup_host(&addr_str)
        .await?
        .next()
        .ok_or_else(|| format!("could not resolve GRPC_ADDRESS: {addr_str}"))?;
    info!(%addr, "gRPC server listening");

    // Run the gRPC server. When it exits (e.g. ctrl-c), signal engine shutdown.
    let server_result = Server::builder()
        .add_service(ArbServiceServer::new(arb_service))
        .add_service(HealthServiceServer::new(health_service))
        .add_service(ControlServiceServer::new(control_service))
        .serve(addr)
        .await
        .map_err(|e| {
            error!(error = %e, "gRPC server failed");
            e
        });

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
