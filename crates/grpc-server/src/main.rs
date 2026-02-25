use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod engine;
mod pipeline;
mod provider;
mod service;

use engine::{AetherEngine, EngineConfig};
use provider::{ProviderConfig, RpcProvider};
use service::aether_proto::arb_service_server::ArbServiceServer;
use service::aether_proto::control_service_server::ControlServiceServer;
use service::aether_proto::health_service_server::HealthServiceServer;
use service::{ArbServiceImpl, ControlServiceImpl, EngineState, HealthServiceImpl};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let control_service = ControlServiceImpl::new(Arc::clone(&state));

    // Create the AetherEngine with a broadcast sender connected to the
    // ArbService's stream.
    let arb_tx = arb_service.arb_sender();
    let engine = Arc::new(AetherEngine::new(EngineConfig::default(), arb_tx));

    // Create the RpcProvider, sharing the engine's event channels so events
    // flow from the provider into the engine's event loop.
    let provider = Arc::new(RpcProvider::new(
        ProviderConfig::default(),
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

    // In production this would be a Unix Domain Socket for sub-microsecond
    // transport to the Go executor on the same machine. For development and
    // testing we bind to a TCP address.
    let addr = "[::1]:50051".parse()?;
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
