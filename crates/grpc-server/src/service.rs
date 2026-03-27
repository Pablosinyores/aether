use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn, error};

/// Include generated proto code from tonic-build.
pub mod aether_proto {
    tonic::include_proto!("aether");
}

use aether_proto::arb_service_server::ArbService;
use aether_proto::control_service_server::ControlService;
use aether_proto::health_service_server::HealthService;
use aether_proto::*;

/// Shared engine state accessible by all gRPC handlers.
///
/// Protected by an `RwLock` so multiple readers (health checks, streaming)
/// can proceed concurrently while writes (state transitions) are exclusive.
pub struct EngineState {
    pub system_state: aether_common::types::SystemState,
    pub last_block: u64,
    pub active_pools: u32,
    pub start_time: std::time::Instant,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            system_state: aether_common::types::SystemState::Running,
            last_block: 0,
            active_pools: 0,
            start_time: std::time::Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: convert between aether_common::types::SystemState and proto i32
// ---------------------------------------------------------------------------

fn system_state_to_proto(state: aether_common::types::SystemState) -> i32 {
    match state {
        aether_common::types::SystemState::Running => 1,
        aether_common::types::SystemState::Degraded => 2,
        aether_common::types::SystemState::Paused => 3,
        aether_common::types::SystemState::Halted => 4,
    }
}

#[allow(clippy::result_large_err)]
fn proto_to_system_state(value: i32) -> Result<aether_common::types::SystemState, Status> {
    match value {
        1 => Ok(aether_common::types::SystemState::Running),
        2 => Ok(aether_common::types::SystemState::Degraded),
        3 => Ok(aether_common::types::SystemState::Paused),
        4 => Ok(aether_common::types::SystemState::Halted),
        _ => Err(Status::invalid_argument(format!(
            "Invalid system state value: {value}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Helper: parse big-endian profit bytes to u128 for threshold comparison
// ---------------------------------------------------------------------------

/// Convert `net_profit_wei` bytes (big-endian U256) to `u128`.
///
/// If the value exceeds `u128::MAX` (i.e. the high 16 bytes are nonzero), we
/// return `u128::MAX` — the arb is astronomically profitable and will always
/// pass any sane filter.
fn profit_wei_to_u128(bytes: &[u8]) -> u128 {
    if bytes.is_empty() {
        return 0;
    }
    // Strip leading zero bytes.
    let stripped = match bytes.iter().position(|&b| b != 0) {
        Some(pos) => &bytes[pos..],
        None => return 0, // all zeros
    };
    if stripped.len() > 16 {
        // Value exceeds u128::MAX — saturate.
        return u128::MAX;
    }
    let mut buf = [0u8; 16];
    buf[16 - stripped.len()..].copy_from_slice(stripped);
    u128::from_be_bytes(buf)
}

/// Convert an ETH-denominated `f64` threshold to wei as `u128`.
///
/// Negative or zero values return 0 (meaning: no filtering).
fn eth_to_wei_threshold(eth: f64) -> u128 {
    if eth <= 0.0 {
        return 0;
    }
    // 1 ETH = 1e18 wei.  f64 has 53-bit mantissa so this is exact enough
    // for any practical threshold (sub-wei precision is irrelevant).
    (eth * 1e18) as u128
}

// ===========================================================================
// ArbService
// ===========================================================================

/// gRPC handler for arbitrage submission and streaming.
///
/// `arb_tx` is a broadcast channel used to fan-out validated arbs to all
/// connected `StreamArbs` subscribers (typically the Go executor).
pub struct ArbServiceImpl {
    state: Arc<RwLock<EngineState>>,
    arb_tx: broadcast::Sender<ValidatedArb>,
}

impl ArbServiceImpl {
    pub fn new(state: Arc<RwLock<EngineState>>) -> Self {
        let (arb_tx, _) = broadcast::channel(1000);
        Self { state, arb_tx }
    }

    /// Obtain a clone of the broadcast sender so external code (e.g. the
    /// detection pipeline) can publish arbs into the stream.
    #[allow(dead_code)]
    pub fn arb_sender(&self) -> broadcast::Sender<ValidatedArb> {
        self.arb_tx.clone()
    }
}

#[tonic::async_trait]
impl ArbService for ArbServiceImpl {
    /// Accept a single validated arb for execution.
    ///
    /// The arb is broadcast to all `StreamArbs` subscribers. If the engine is
    /// in a non-accepting state (Paused / Halted), the submission is rejected
    /// with an explanatory error message rather than a gRPC error status, so
    /// the caller can distinguish between transport failures and policy
    /// rejections.
    async fn submit_arb(
        &self,
        request: Request<ValidatedArb>,
    ) -> Result<Response<SubmitArbResponse>, Status> {
        let arb = request.into_inner();
        let arb_id = arb.id.clone();
        info!(id = %arb_id, "Received arb submission");

        // Gate on system state -- only Running and Degraded accept arbs.
        {
            let engine_state = self.state.read().await;
            match engine_state.system_state {
                aether_common::types::SystemState::Running
                | aether_common::types::SystemState::Degraded => { /* ok */ }
                other => {
                    warn!(state = ?other, id = %arb_id, "Rejecting arb: system not accepting");
                    return Ok(Response::new(SubmitArbResponse {
                        accepted: false,
                        bundle_hash: String::new(),
                        error: format!("System is {other:?}, not accepting arbs"),
                    }));
                }
            }
        }

        // Broadcast to all connected StreamArbs subscribers.
        match self.arb_tx.send(arb) {
            Ok(receiver_count) => {
                info!(id = %arb_id, receivers = receiver_count, "Arb broadcast succeeded");
                Ok(Response::new(SubmitArbResponse {
                    accepted: true,
                    bundle_hash: format!("pending-{arb_id}"),
                    error: String::new(),
                }))
            }
            Err(e) => {
                warn!(error = %e, id = %arb_id, "No arb subscribers connected");
                Ok(Response::new(SubmitArbResponse {
                    accepted: false,
                    bundle_hash: String::new(),
                    error: "No subscribers connected".to_string(),
                }))
            }
        }
    }

    type StreamArbsStream = ReceiverStream<Result<ValidatedArb, Status>>;

    /// Server-side streaming RPC. Each connected client receives a copy of
    /// every validated arb published via the broadcast channel.
    ///
    /// The stream stays open until the client disconnects or the broadcast
    /// sender is dropped.
    async fn stream_arbs(
        &self,
        request: Request<StreamArbsRequest>,
    ) -> Result<Response<Self::StreamArbsStream>, Status> {
        let min_profit = request.into_inner().min_profit_eth;
        info!(min_profit_eth = min_profit, "Client subscribed to arb stream");

        let min_profit_wei = eth_to_wei_threshold(min_profit);
        let (tx, rx) = mpsc::channel(100);
        let mut arb_rx = self.arb_tx.subscribe();

        tokio::spawn(async move {
            loop {
                match arb_rx.recv().await {
                    Ok(arb) => {
                        // Apply min-profit filter when a positive threshold is set.
                        if min_profit_wei > 0 {
                            let profit = profit_wei_to_u128(&arb.net_profit_wei);
                            if profit < min_profit_wei {
                                debug!(
                                    id = %arb.id,
                                    profit_wei = profit,
                                    threshold_wei = min_profit_wei,
                                    "Skipping arb below min_profit_eth threshold"
                                );
                                continue;
                            }
                        }

                        if tx.send(Ok(arb)).await.is_err() {
                            // Client disconnected.
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "StreamArbs subscriber lagged, dropped messages");
                        // Continue — the subscriber missed some but can still
                        // receive future messages.
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Sender dropped — server shutting down.
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ===========================================================================
// HealthService
// ===========================================================================

/// Reports engine health for the Go executor and external monitors.
pub struct HealthServiceImpl {
    state: Arc<RwLock<EngineState>>,
}

impl HealthServiceImpl {
    pub fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self { state }
    }
}

#[tonic::async_trait]
impl HealthService for HealthServiceImpl {
    async fn check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let engine_state = self.state.read().await;

        let healthy = matches!(
            engine_state.system_state,
            aether_common::types::SystemState::Running
                | aether_common::types::SystemState::Degraded
        );

        Ok(Response::new(HealthCheckResponse {
            healthy,
            status: format!("{:?}", engine_state.system_state),
            uptime_seconds: engine_state.start_time.elapsed().as_secs() as i64,
            last_block: engine_state.last_block,
            active_pools: engine_state.active_pools,
        }))
    }
}

// ===========================================================================
// ControlService
// ===========================================================================

/// Allows the Go executor (or an admin tool) to change engine state and
/// hot-reload configuration.
pub struct ControlServiceImpl {
    state: Arc<RwLock<EngineState>>,
    engine: Arc<crate::engine::AetherEngine>,
}

impl ControlServiceImpl {
    pub fn new(state: Arc<RwLock<EngineState>>, engine: Arc<crate::engine::AetherEngine>) -> Self {
        Self { state, engine }
    }
}

#[tonic::async_trait]
impl ControlService for ControlServiceImpl {
    async fn set_state(
        &self,
        request: Request<SetStateRequest>,
    ) -> Result<Response<SetStateResponse>, Status> {
        let req = request.into_inner();
        let new_state = proto_to_system_state(req.state)?;

        let mut engine_state = self.state.write().await;
        let previous = engine_state.system_state;

        info!(
            from = ?previous,
            to = ?new_state,
            reason = %req.reason,
            "State transition"
        );

        engine_state.system_state = new_state;

        Ok(Response::new(SetStateResponse {
            success: true,
            previous_state: system_state_to_proto(previous),
        }))
    }

    async fn reload_config(
        &self,
        request: Request<ReloadConfigRequest>,
    ) -> Result<Response<ReloadConfigResponse>, Status> {
        let path = request.into_inner().config_path;
        info!(path = %path, "Config reload requested");

        if path.is_empty() {
            return Ok(Response::new(ReloadConfigResponse {
                success: false,
                pools_loaded: 0,
                error: "config_path is empty".to_string(),
            }));
        }

        // Read the TOML file from disk.
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => {
                error!(path = %path, error = %e, "Failed to read config file");
                return Ok(Response::new(ReloadConfigResponse {
                    success: false,
                    pools_loaded: 0,
                    error: format!("Failed to read {path}: {e}"),
                }));
            }
        };

        // Parse as TOML and extract the [[pools]] array.
        #[derive(serde::Deserialize)]
        struct PoolsConfig {
            #[serde(default)]
            pools: Vec<toml::Value>,
        }

        let config: PoolsConfig = match toml::from_str(&contents) {
            Ok(c) => c,
            Err(e) => {
                error!(path = %path, error = %e, "Failed to parse config TOML");
                return Ok(Response::new(ReloadConfigResponse {
                    success: false,
                    pools_loaded: 0,
                    error: format!("Failed to parse {path}: {e}"),
                }));
            }
        };

        let total_in_file = config.pools.len() as u32;

        // Actually register the pools in the engine (skips duplicates).
        let loaded = self.engine.bootstrap_pools(&path).await;

        // Fetch on-chain reserves for newly registered pools.
        self.engine.fetch_initial_reserves().await;

        // Update engine state with the new pool count.
        {
            let mut engine_state = self.state.write().await;
            engine_state.active_pools += loaded;
        }

        info!(path = %path, loaded, total_in_file, "Config reloaded successfully");

        Ok(Response::new(ReloadConfigResponse {
            success: true,
            pools_loaded: loaded,
            error: String::new(),
        }))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    fn shared_state() -> Arc<RwLock<EngineState>> {
        Arc::new(RwLock::new(EngineState::default()))
    }

    fn dummy_engine() -> Arc<crate::engine::AetherEngine> {
        let (arb_tx, _) = tokio::sync::broadcast::channel(16);
        Arc::new(crate::engine::AetherEngine::new(
            crate::engine::EngineConfig::default(),
            arb_tx,
        ))
    }

    // ---- EngineState tests ----

    #[test]
    fn test_engine_state_default() {
        let state = EngineState::default();
        assert_eq!(
            state.system_state,
            aether_common::types::SystemState::Running
        );
        assert_eq!(state.last_block, 0);
        assert_eq!(state.active_pools, 0);
    }

    // ---- Proto conversion helpers ----

    #[test]
    fn test_system_state_to_proto_roundtrip() {
        use aether_common::types::SystemState;
        for (state, expected) in [
            (SystemState::Running, 1),
            (SystemState::Degraded, 2),
            (SystemState::Paused, 3),
            (SystemState::Halted, 4),
        ] {
            let proto = system_state_to_proto(state);
            assert_eq!(proto, expected);
            let back = proto_to_system_state(proto).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn test_proto_to_system_state_invalid() {
        assert!(proto_to_system_state(0).is_err());
        assert!(proto_to_system_state(5).is_err());
        assert!(proto_to_system_state(-1).is_err());
    }

    // ---- ArbServiceImpl tests ----

    #[test]
    fn test_arb_service_creation() {
        let state = shared_state();
        let svc = ArbServiceImpl::new(state);
        // Should be able to obtain a sender clone.
        let _sender = svc.arb_sender();
    }

    #[tokio::test]
    async fn test_submit_arb_accepted() {
        let state = shared_state();
        let svc = ArbServiceImpl::new(Arc::clone(&state));

        // Need at least one subscriber for broadcast to succeed.
        let _rx = svc.arb_tx.subscribe();

        let arb = ValidatedArb {
            id: "test-arb-001".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 200_000,
            gas_cost_wei: vec![],
            net_profit_wei: vec![],
            block_number: 18_000_000,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        let resp = svc
            .submit_arb(Request::new(arb))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.accepted);
        assert!(resp.bundle_hash.contains("test-arb-001"));
        assert!(resp.error.is_empty());
    }

    #[tokio::test]
    async fn test_submit_arb_rejected_when_paused() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Paused;
        }

        let svc = ArbServiceImpl::new(Arc::clone(&state));

        let arb = ValidatedArb {
            id: "test-arb-paused".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: vec![],
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        let resp = svc
            .submit_arb(Request::new(arb))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.accepted);
        assert!(resp.error.contains("Paused"));
    }

    #[tokio::test]
    async fn test_submit_arb_rejected_when_halted() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Halted;
        }

        let svc = ArbServiceImpl::new(Arc::clone(&state));

        let arb = ValidatedArb {
            id: "test-arb-halted".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: vec![],
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        let resp = svc
            .submit_arb(Request::new(arb))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.accepted);
        assert!(resp.error.contains("Halted"));
    }

    #[tokio::test]
    async fn test_submit_arb_no_subscribers() {
        let state = shared_state();
        let svc = ArbServiceImpl::new(Arc::clone(&state));
        // No subscribers at all.

        let arb = ValidatedArb {
            id: "test-arb-nosub".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: vec![],
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        let resp = svc
            .submit_arb(Request::new(arb))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.accepted);
        assert!(resp.error.contains("No subscribers"));
    }

    #[tokio::test]
    async fn test_submit_arb_accepted_in_degraded() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Degraded;
        }

        let svc = ArbServiceImpl::new(Arc::clone(&state));
        let _rx = svc.arb_tx.subscribe();

        let arb = ValidatedArb {
            id: "test-arb-degraded".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: vec![],
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        let resp = svc
            .submit_arb(Request::new(arb))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.accepted);
    }

    // ---- HealthServiceImpl tests ----

    #[tokio::test]
    async fn test_health_check_running() {
        let state = shared_state();
        let svc = HealthServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .check(Request::new(HealthCheckRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.healthy);
        assert_eq!(resp.status, "Running");
        assert!(resp.uptime_seconds >= 0);
    }

    #[tokio::test]
    async fn test_health_check_degraded() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Degraded;
        }

        let svc = HealthServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .check(Request::new(HealthCheckRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.healthy); // Degraded is still considered healthy.
        assert_eq!(resp.status, "Degraded");
    }

    #[tokio::test]
    async fn test_health_check_paused() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Paused;
        }

        let svc = HealthServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .check(Request::new(HealthCheckRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.healthy);
        assert_eq!(resp.status, "Paused");
    }

    #[tokio::test]
    async fn test_health_check_halted() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.system_state = aether_common::types::SystemState::Halted;
        }

        let svc = HealthServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .check(Request::new(HealthCheckRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.healthy);
        assert_eq!(resp.status, "Halted");
    }

    #[tokio::test]
    async fn test_health_check_reports_block_and_pools() {
        let state = shared_state();
        {
            let mut s = state.write().await;
            s.last_block = 19_500_000;
            s.active_pools = 4200;
        }

        let svc = HealthServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .check(Request::new(HealthCheckRequest {}))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.last_block, 19_500_000);
        assert_eq!(resp.active_pools, 4200);
    }

    // ---- ControlServiceImpl tests ----

    #[tokio::test]
    async fn test_set_state_running_to_paused() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let resp = svc
            .set_state(Request::new(SetStateRequest {
                state: 3, // PAUSED
                reason: "maintenance".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.success);
        assert_eq!(resp.previous_state, 1); // was RUNNING

        let s = state.read().await;
        assert_eq!(s.system_state, aether_common::types::SystemState::Paused);
    }

    #[tokio::test]
    async fn test_set_state_invalid_value() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let result = svc
            .set_state(Request::new(SetStateRequest {
                state: 99,
                reason: "bad".into(),
            }))
            .await;

        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn test_set_state_full_cycle() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        // Running -> Degraded -> Paused -> Halted -> Running
        for (new, expected_prev) in [(2, 1), (3, 2), (4, 3), (1, 4)] {
            let resp = svc
                .set_state(Request::new(SetStateRequest {
                    state: new,
                    reason: "cycle test".into(),
                }))
                .await
                .unwrap()
                .into_inner();

            assert!(resp.success);
            assert_eq!(resp.previous_state, expected_prev);
        }

        let s = state.read().await;
        assert_eq!(s.system_state, aether_common::types::SystemState::Running);
    }

    #[tokio::test]
    async fn test_reload_config_valid_file() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        // Write a temp TOML file with 2 pool entries.
        let dir = std::env::temp_dir().join("aether_test_reload_config");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pools.toml");
        std::fs::write(
            &path,
            r#"
[[pools]]
protocol = "uniswap_v2"
address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 30
tier = "hot"

[[pools]]
protocol = "sushiswap"
address = "0x397FF1542f962076d0BFE58eA045FfA2d347ACa0"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 30
tier = "warm"
"#,
        )
        .unwrap();

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: path.to_str().unwrap().to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.success);
        assert!(resp.error.is_empty());
        assert_eq!(resp.pools_loaded, 2);

        // Verify engine state was updated.
        let s = state.read().await;
        assert_eq!(s.active_pools, 2);

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_reload_config_empty_path() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.success);
        assert!(resp.error.contains("empty"));
    }

    #[tokio::test]
    async fn test_reload_config_nonexistent_file() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: "/tmp/aether_nonexistent_file_12345.toml".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.success);
        assert_eq!(resp.pools_loaded, 0);
        assert!(!resp.error.is_empty());
    }

    #[tokio::test]
    async fn test_reload_config_invalid_toml() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let dir = std::env::temp_dir().join("aether_test_reload_bad_toml");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.toml");
        std::fs::write(&path, "this is [[[not valid toml").unwrap();

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: path.to_str().unwrap().to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.success);
        assert_eq!(resp.pools_loaded, 0);
        assert!(resp.error.contains("parse"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_reload_config_no_pools_key() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state), dummy_engine());

        let dir = std::env::temp_dir().join("aether_test_reload_no_pools");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.toml");
        std::fs::write(&path, "[settings]\nkey = \"value\"\n").unwrap();

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: path.to_str().unwrap().to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Valid TOML but no [[pools]] — should succeed with 0 pools loaded.
        assert!(resp.success);
        assert_eq!(resp.pools_loaded, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Profit filtering helper tests ----

    #[test]
    fn test_profit_wei_to_u128_empty() {
        assert_eq!(profit_wei_to_u128(&[]), 0);
    }

    #[test]
    fn test_profit_wei_to_u128_zeros() {
        assert_eq!(profit_wei_to_u128(&[0, 0, 0]), 0);
    }

    #[test]
    fn test_profit_wei_to_u128_small() {
        // 1 ETH = 1e18 wei = 0xDE0B6B3A7640000 (8 bytes)
        let one_eth_bytes = 1_000_000_000_000_000_000u128.to_be_bytes();
        assert_eq!(profit_wei_to_u128(&one_eth_bytes), 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_profit_wei_to_u128_with_leading_zeros() {
        // 0x00 0x00 0x01 => 1
        assert_eq!(profit_wei_to_u128(&[0, 0, 1]), 1);
    }

    #[test]
    fn test_profit_wei_to_u128_u256_overflow_saturates() {
        // 32 bytes with high bytes nonzero => saturates to u128::MAX
        let mut bytes = [0u8; 32];
        bytes[0] = 1; // high bit set in the u256 upper half
        assert_eq!(profit_wei_to_u128(&bytes), u128::MAX);
    }

    #[test]
    fn test_eth_to_wei_threshold_zero() {
        assert_eq!(eth_to_wei_threshold(0.0), 0);
    }

    #[test]
    fn test_eth_to_wei_threshold_negative() {
        assert_eq!(eth_to_wei_threshold(-1.0), 0);
    }

    #[test]
    fn test_eth_to_wei_threshold_one_eth() {
        assert_eq!(eth_to_wei_threshold(1.0), 1_000_000_000_000_000_000);
    }

    #[test]
    fn test_eth_to_wei_threshold_small() {
        // 0.001 ETH = 1e15 wei
        assert_eq!(eth_to_wei_threshold(0.001), 1_000_000_000_000_000);
    }

    // ---- StreamArbs filtering integration tests ----

    #[tokio::test]
    async fn test_stream_arbs_filters_below_threshold() {
        let state = shared_state();
        let svc = ArbServiceImpl::new(Arc::clone(&state));

        // Subscribe with min_profit_eth = 1.0 (1e18 wei).
        let resp = svc
            .stream_arbs(Request::new(StreamArbsRequest {
                min_profit_eth: 1.0,
            }))
            .await
            .unwrap();

        let mut stream = resp.into_inner();

        // Publish an arb with 0.5 ETH profit (below threshold).
        let low_profit_wei = 500_000_000_000_000_000u128.to_be_bytes().to_vec();
        let low_arb = ValidatedArb {
            id: "low-profit".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: low_profit_wei,
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        // Publish an arb with 2.0 ETH profit (above threshold).
        let high_profit_wei = 2_000_000_000_000_000_000u128.to_be_bytes().to_vec();
        let high_arb = ValidatedArb {
            id: "high-profit".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: high_profit_wei,
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        // Send both arbs.
        svc.arb_tx.send(low_arb).unwrap();
        svc.arb_tx.send(high_arb).unwrap();

        // We should only receive the high-profit arb (low-profit is filtered out).
        let received = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stream.next(),
        )
        .await
        .expect("should receive within timeout")
        .expect("stream should have a message")
        .expect("item should be Ok");

        assert_eq!(received.id, "high-profit");
    }

    #[tokio::test]
    async fn test_stream_arbs_no_filter_when_zero() {
        let state = shared_state();
        let svc = ArbServiceImpl::new(Arc::clone(&state));

        // Subscribe with min_profit_eth = 0.0 (no filtering).
        let resp = svc
            .stream_arbs(Request::new(StreamArbsRequest {
                min_profit_eth: 0.0,
            }))
            .await
            .unwrap();

        let mut stream = resp.into_inner();

        // Publish an arb with tiny profit.
        let arb = ValidatedArb {
            id: "tiny-profit".into(),
            hops: vec![],
            total_profit_wei: vec![],
            total_gas: 0,
            gas_cost_wei: vec![],
            net_profit_wei: vec![0, 0, 1], // 1 wei
            block_number: 0,
            timestamp_ns: 0,
            flashloan_token: vec![],
            flashloan_amount: vec![],
            steps: vec![],
            calldata: vec![],
        };

        svc.arb_tx.send(arb).unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stream.next(),
        )
        .await
        .expect("should receive within timeout")
        .expect("stream should have a message")
        .expect("item should be Ok");

        assert_eq!(received.id, "tiny-profit");
    }
}
