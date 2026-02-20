use alloy::primitives::Address;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AetherError {
    // Pool errors
    #[error("Pool not found: {0}")]
    PoolNotFound(Address),

    #[error("Invalid token pair: {token_in} -> {token_out}")]
    InvalidTokenPair {
        token_in: Address,
        token_out: Address,
    },

    #[error("Insufficient liquidity in pool {0}")]
    InsufficientLiquidity(Address),

    // Detection errors
    #[error("No arbitrage opportunity found")]
    NoOpportunity,

    #[error("Profit below minimum threshold: {profit_wei} < {min_profit_wei}")]
    ProfitBelowThreshold {
        profit_wei: String,
        min_profit_wei: String,
    },

    // Simulation errors
    #[error("Simulation failed: {0}")]
    SimulationFailed(String),

    #[error("EVM revert: {0}")]
    EvmRevert(String),

    // Execution errors
    #[error("Bundle submission failed: {0}")]
    BundleSubmissionFailed(String),

    #[error("Nonce mismatch: expected {expected}, got {actual}")]
    NonceMismatch { expected: u64, actual: u64 },

    // Node errors
    #[error("Node connection failed: {0}")]
    NodeConnectionFailed(String),

    #[error("All nodes unhealthy")]
    AllNodesUnhealthy,

    // Risk errors
    #[error("Circuit breaker triggered: {0}")]
    CircuitBreakerTriggered(String),

    #[error("Position limit exceeded: {0}")]
    PositionLimitExceeded(String),

    // Config errors
    #[error("Configuration error: {0}")]
    ConfigError(String),

    // gRPC errors
    #[error("gRPC error: {0}")]
    GrpcError(String),

    // Generic
    #[error("Internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_pool_not_found_display() {
        let err = AetherError::PoolNotFound(Address::ZERO);
        let msg = err.to_string();
        assert!(msg.contains("Pool not found"));
        assert!(msg.contains(&Address::ZERO.to_string()));
    }

    #[test]
    fn test_invalid_token_pair_display() {
        let err = AetherError::InvalidTokenPair {
            token_in: Address::ZERO,
            token_out: Address::ZERO,
        };
        let msg = err.to_string();
        assert!(msg.contains("Invalid token pair"));
    }

    #[test]
    fn test_insufficient_liquidity_display() {
        let err = AetherError::InsufficientLiquidity(Address::ZERO);
        assert!(err.to_string().contains("Insufficient liquidity"));
    }

    #[test]
    fn test_no_opportunity_display() {
        let err = AetherError::NoOpportunity;
        assert_eq!(err.to_string(), "No arbitrage opportunity found");
    }

    #[test]
    fn test_profit_below_threshold_display() {
        let err = AetherError::ProfitBelowThreshold {
            profit_wei: "100".to_string(),
            min_profit_wei: "1000".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("100"));
        assert!(msg.contains("1000"));
    }

    #[test]
    fn test_simulation_failed_display() {
        let err = AetherError::SimulationFailed("out of gas".to_string());
        assert!(err.to_string().contains("out of gas"));
    }

    #[test]
    fn test_evm_revert_display() {
        let err = AetherError::EvmRevert("INSUFFICIENT_OUTPUT".to_string());
        assert!(err.to_string().contains("INSUFFICIENT_OUTPUT"));
    }

    #[test]
    fn test_bundle_submission_failed_display() {
        let err = AetherError::BundleSubmissionFailed("timeout".to_string());
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn test_nonce_mismatch_display() {
        let err = AetherError::NonceMismatch {
            expected: 42,
            actual: 41,
        };
        let msg = err.to_string();
        assert!(msg.contains("42"));
        assert!(msg.contains("41"));
    }

    #[test]
    fn test_node_connection_failed_display() {
        let err = AetherError::NodeConnectionFailed("ws://localhost:8546".to_string());
        assert!(err.to_string().contains("ws://localhost:8546"));
    }

    #[test]
    fn test_all_nodes_unhealthy_display() {
        let err = AetherError::AllNodesUnhealthy;
        assert_eq!(err.to_string(), "All nodes unhealthy");
    }

    #[test]
    fn test_circuit_breaker_triggered_display() {
        let err = AetherError::CircuitBreakerTriggered("gas > 300 gwei".to_string());
        assert!(err.to_string().contains("gas > 300 gwei"));
    }

    #[test]
    fn test_position_limit_exceeded_display() {
        let err = AetherError::PositionLimitExceeded("max single trade 50 ETH".to_string());
        assert!(err.to_string().contains("max single trade 50 ETH"));
    }

    #[test]
    fn test_config_error_display() {
        let err = AetherError::ConfigError("invalid pools.toml".to_string());
        assert!(err.to_string().contains("invalid pools.toml"));
    }

    #[test]
    fn test_grpc_error_display() {
        let err = AetherError::GrpcError("connection refused".to_string());
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn test_internal_error_display() {
        let err = AetherError::Internal("unexpected state".to_string());
        assert!(err.to_string().contains("unexpected state"));
    }

    #[test]
    fn test_io_error_from() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: AetherError = io_err.into();
        assert!(err.to_string().contains("file not found"));
        assert!(matches!(err, AetherError::Io(_)));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // std::io::Error is Send + Sync, and all other variants use String/u64/Address
        // which are Send + Sync, so AetherError should be Send + Sync
        assert_send_sync::<AetherError>();
    }
}
