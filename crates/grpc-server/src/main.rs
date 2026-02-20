use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod service;

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

    // In production this would be a Unix Domain Socket for sub-microsecond
    // transport to the Go executor on the same machine. For development and
    // testing we bind to a TCP address.
    let addr = "[::1]:50051".parse()?;
    info!(%addr, "gRPC server listening");

    Server::builder()
        .add_service(ArbServiceServer::new(arb_service))
        .add_service(HealthServiceServer::new(health_service))
        .add_service(ControlServiceServer::new(control_service))
        .serve(addr)
        .await
        .map_err(|e| {
            error!(error = %e, "gRPC server failed");
            e
        })?;

    Ok(())
}
