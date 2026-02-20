use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, warn, error};

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

        let (tx, rx) = mpsc::channel(100);
        let mut arb_rx = self.arb_tx.subscribe();

        tokio::spawn(async move {
            loop {
                match arb_rx.recv().await {
                    Ok(arb) => {
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
}

impl ControlServiceImpl {
    pub fn new(state: Arc<RwLock<EngineState>>) -> Self {
        Self { state }
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

        // In production this would re-read pools.toml and update the pool
        // registry via the detection engine. For now, acknowledge the request.
        Ok(Response::new(ReloadConfigResponse {
            success: true,
            pools_loaded: 0,
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

    fn shared_state() -> Arc<RwLock<EngineState>> {
        Arc::new(RwLock::new(EngineState::default()))
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
        let svc = ControlServiceImpl::new(Arc::clone(&state));

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
        let svc = ControlServiceImpl::new(Arc::clone(&state));

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
        let svc = ControlServiceImpl::new(Arc::clone(&state));

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
    async fn test_reload_config() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: "config/pools.toml".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(resp.success);
        assert!(resp.error.is_empty());
    }

    #[tokio::test]
    async fn test_reload_config_empty_path() {
        let state = shared_state();
        let svc = ControlServiceImpl::new(Arc::clone(&state));

        let resp = svc
            .reload_config(Request::new(ReloadConfigRequest {
                config_path: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();

        // Should still succeed (stub implementation).
        assert!(resp.success);
    }
}
