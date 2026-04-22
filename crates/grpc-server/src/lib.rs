/// Library entry point for the aether-grpc-server crate.
///
/// Re-exports the `provider` module so that integration tests and other
/// crates can access `ProviderConfig`, `RpcProvider`, and related types
/// without depending on the binary entry point. The `metrics` module is
/// crate-private; only the two types the binary and integration tests
/// actually need are re-exported publicly.
pub(crate) mod metrics;
pub mod provider;

pub use metrics::{start_metrics_server, EngineMetrics};
