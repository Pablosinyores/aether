/// Library entry point for the aether-grpc-server crate.
///
/// Re-exports the `provider` module so that integration tests and other
/// crates can access `ProviderConfig`, `RpcProvider`, and related types
/// without depending on the binary entry point. The `metrics` module is
/// crate-private; only the two types the binary and integration tests
/// actually need are re-exported publicly.
pub mod cycle_gating;
pub mod first_seen_tracker;
pub mod historical;
pub mod hot_token;
pub(crate) mod metrics;
pub mod pool_admission;
pub mod profitability_writer;
pub mod provider;

pub use metrics::{start_metrics_server, EngineMetrics};
