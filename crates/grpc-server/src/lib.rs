/// Library entry point for the aether-grpc-server crate.
///
/// Re-exports the `provider` module so that integration tests and other
/// crates can access `ProviderConfig`, `RpcProvider`, and related types
/// without depending on the binary entry point.
pub mod provider;
