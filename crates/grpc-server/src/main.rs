use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::transport::Server;
use tracing::{error, info};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;

mod engine;
mod mempool_pipeline;
mod mempool_writer;
mod pipeline;
mod service;
mod tracing_init;

use aether_grpc_server::provider::{ProviderConfig, RpcProvider};
use aether_grpc_server::{start_metrics_server, EngineMetrics};
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

    // Initialise logging + optional OTLP span export to Tempo.
    // RUST_LOG controls level (default info), LOG_FORMAT=json picks the JSON
    // fmt layer, OTEL_EXPORTER_OTLP_ENDPOINT (when set) wires the span exporter.
    let _tracing_guard = tracing_init::init();

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
    // Cloned twice: once moved into the engine for the block-driven path
    // and once attached to `SimContext::arb_publisher` so the mempool
    // validator can publish on the same channel.
    let arb_tx = arb_service.arb_sender();
    let arb_tx_for_mempool = arb_tx.clone();
    let engine_config = EngineConfig {
        rpc_url: std::env::var("ETH_RPC_URL").ok(),
        ..EngineConfig::default()
    };
    if engine_config.rpc_url.is_some() {
        info!("ETH_RPC_URL set — engine will use RPC-backed fork simulation");
    } else {
        info!("ETH_RPC_URL not set — engine will use empty-state simulation");
    }
    let ledger_metrics = aether_common::db::LedgerMetrics::register(metrics.registry());
    let ledger = aether_common::db::ledger_from_env(ledger_metrics).await;
    let engine = Arc::new(AetherEngine::new_with_metrics_and_ledger(
        engine_config,
        arb_tx,
        Arc::clone(&metrics),
        ledger,
    ));

    // ControlService needs a handle to the engine for hot-reload support.
    let control_service = ControlServiceImpl::new(Arc::clone(&state), Arc::clone(&engine));

    // Bootstrap pools from config file at startup.
    // Supports AETHER_POOLS_CONFIG env var to override the default path,
    // so the binary works regardless of the working directory.
    let pools_config =
        std::env::var("AETHER_POOLS_CONFIG").unwrap_or_else(|_| "config/pools.toml".to_string());
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
        Arc::clone(&metrics),
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

    // Mempool tracking is opt-in via MEMPOOL_TRACKING=1. When unset the
    // engine behaves identically to today; when set we spawn the Alchemy
    // pending-tx subscription and the decode pipeline that consumes it.
    let mempool_handles = if aether_ingestion::mempool::is_enabled() {
        info!("MEMPOOL_TRACKING enabled — spawning pending-tx subscription + decode pipeline");
        let ws_url = std::env::var("MEMPOOL_WS_URL")
            .or_else(|_| std::env::var("ETH_RPC_URL"))
            .unwrap_or_default();
        if ws_url.is_empty() {
            tracing::warn!(
                "MEMPOOL_TRACKING set but neither MEMPOOL_WS_URL nor ETH_RPC_URL provided; skipping"
            );
            None
        } else {
            let cfg = aether_ingestion::mempool::AlchemyMempoolConfig {
                ws_url,
                router_filter: aether_ingestion::mempool::default_router_addresses(),
            };
            let source = Arc::new(aether_ingestion::mempool::AlchemyMempool::new(cfg));
            let channels = Arc::clone(engine.event_channels());
            let source_shutdown = shutdown_rx.clone();
            let source_handle = tokio::spawn(async move {
                use aether_ingestion::mempool::MempoolSource;
                source.run(channels, source_shutdown).await;
            });
            // Build the post-state simulation context from the engine's
            // live registry / token index / snapshot. The detector mirrors
            // the engine's BellmanFord config so the analytical scan
            // honours the same hop / latency budget as the main path.
            let engine_cfg = EngineConfig::default();
            // Mempool prediction writer: optional persistence to a separate
            // Postgres DSN. MEMPOOL_LEDGER_DSN unset → NoopMempoolSink, no
            // DB writes, behaviour identical to today. Distinct from the
            // trade ledger's DATABASE_URL so an operator can enable mempool
            // observability without provisioning the executor schema.
            let writer_metrics =
                mempool_writer::MempoolWriterMetrics::register(metrics.registry());
            let prediction_sink = mempool_writer::mempool_writer_from_env(writer_metrics).await;
            let engine_git_sha = std::env::var("AETHER_GIT_SHA").ok();
            let mut sim_ctx_inner = mempool_pipeline::SimContext::new(
                Arc::clone(engine.pool_registry()),
                Arc::clone(engine.token_index()),
                Arc::clone(engine.snapshot_manager()),
                aether_detector::bellman_ford::BellmanFord::new(
                    engine_cfg.max_hops,
                    engine_cfg.detection_time_budget_us,
                ),
                Arc::clone(engine.pool_states()),
                prediction_sink,
                engine_git_sha,
            );
            // Wire the validated-arb publisher so the revm validator can
            // hand accepted backruns to the existing gRPC stream. The
            // executor consumes both block-driven and mempool-backrun
            // arbs from this single channel.
            sim_ctx_inner = sim_ctx_inner.with_arb_publisher(arb_tx_for_mempool);
            // Validator config — env-tunable. All fields default to safe
            // values when the operator hasn't opted into mempool-backrun
            // execution. `AETHER_EXECUTOR_ADDRESS` is the only hard
            // requirement; when unset the validator stays dormant and
            // the analytical-only candidate path runs as before.
            if let Some(cfg) = build_backrun_validator_config(engine.rpc_provider()) {
                let with_provider = cfg.provider.is_some();
                sim_ctx_inner = sim_ctx_inner.with_backrun_validator(cfg);
                if with_provider {
                    info!("Mempool-backrun revm validator enabled (live RPC fork)");
                } else {
                    info!(
                        "Mempool-backrun revm validator enabled (provider unavailable — rejects with provider_unavailable)"
                    );
                }
            } else {
                info!(
                    "AETHER_EXECUTOR_ADDRESS not set — mempool-backrun revm validator disabled"
                );
            }
            // Post-state replay fallback for V3 swaps the analytical
            // predictor cannot settle. Opt-in via env so the dormant
            // behaviour from develop is preserved until an operator
            // enables it deliberately.
            let replay_enabled = std::env::var("MEMPOOL_POST_STATE_REPLAY")
                .map(|v| v == "1")
                .unwrap_or(false);
            sim_ctx_inner = sim_ctx_inner.with_post_state_replay(replay_enabled);
            if replay_enabled {
                info!(
                    "MEMPOOL_POST_STATE_REPLAY enabled — V3 tick-crossing swaps will escalate to revm fork-replay"
                );
            }
            // Plumb the engine's bytecode cache (when configured via
            // `AETHER_BYTECODE_CACHE_PATH`) into the mempool prewarm path so
            // address-keyed `eth_getCode` hits skip the RPC round-trip.
            sim_ctx_inner =
                sim_ctx_inner.with_bytecode_cache(engine.bytecode_cache().cloned());
            // Plumb the engine's WS-fed V2 reserves cache so the mempool
            // prewarm path can synthesise slot 8 for warm pools instead of
            // round-tripping `eth_getStorageAt`.
            sim_ctx_inner =
                sim_ctx_inner.with_v2_reserves_cache(Some(engine.v2_reserves_cache()));
            let sim_ctx = Arc::new(sim_ctx_inner);
            let pipeline_handle = mempool_pipeline::spawn_mempool_pipeline(
                Arc::clone(engine.event_channels()),
                Arc::clone(&metrics),
                Some(Arc::clone(&sim_ctx)),
                shutdown_rx.clone(),
            );
            // Pre-warm refresher: rebuilds the long-lived
            // PrewarmedState (tracked-pool bytecode + V2 reserve slot
            // 8) every Nth new block. Without this each per-pending-tx
            // shadow-sim builds an empty CacheDB and re-fetches the
            // same bytecode from Alchemy. The refresher only spawns
            // when an RPC provider is available — without one
            // `prewarm_state` has nowhere to fetch from.
            if let Some(provider) = engine.rpc_provider() {
                let prewarm_interval = std::env::var("AETHER_MEMPOOL_PREWARM_INTERVAL_BLOCKS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(8);
                let _prewarm_handle = mempool_pipeline::spawn_mempool_prewarm_refresher(
                    Arc::clone(&sim_ctx),
                    provider,
                    Arc::clone(engine.event_channels()),
                    Arc::clone(&metrics),
                    prewarm_interval,
                    shutdown_rx.clone(),
                );
            } else {
                info!(
                    "Mempool prewarm refresher disabled — no RPC provider configured (set ETH_RPC_URL)"
                );
            }
            // First-seen → inclusion latency tracker. Pure observability —
            // listens on the same broadcast channels, never publishes
            // anything outward. Spawned alongside the mempool pipeline so
            // its lifecycle matches `MEMPOOL_TRACKING=1`.
            let first_seen_handle = aether_grpc_server::first_seen_tracker::spawn_first_seen_tracker(
                Arc::clone(engine.event_channels()),
                Arc::clone(&metrics),
                shutdown_rx.clone(),
            );
            Some((source_handle, pipeline_handle, first_seen_handle))
        }
    } else {
        None
    };

    // Read the listen address from the environment so the systemd unit and
    // the binary always agree.  Default to localhost TCP for development.
    //
    // Production (UDS): GRPC_ADDRESS=unix:///var/run/aether/engine.sock
    // Development (TCP): GRPC_ADDRESS=[::1]:50051  (default)
    let addr_str = std::env::var("GRPC_ADDRESS").unwrap_or_else(|_| "[::1]:50051".to_string());

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
                Err(e) => {
                    tracing::warn!(path = %uds_path, error = %e, "Failed to remove stale UDS socket")
                }
            }

            // Ensure parent directory exists.
            if let Some(parent) = std::path::Path::new(uds_path).parent() {
                std::fs::create_dir_all(parent)?;
            }

            let uds = UnixListener::bind(uds_path)?;
            // Restrict socket access to the process owner — UDS bypasses
            // network-layer controls (iptables, mTLS), so file permissions
            // are the only access control for ControlService endpoints.
            std::fs::set_permissions(uds_path, PermissionsExt::from_mode(0o600))?;
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

    if let Some((source_handle, pipeline_handle, first_seen_handle)) = mempool_handles {
        if let Err(e) = source_handle.await {
            error!(error = %e, "Mempool source task panicked");
        }
        if let Err(e) = pipeline_handle.await {
            error!(error = %e, "Mempool pipeline task panicked");
        }
        if let Err(e) = first_seen_handle.await {
            error!(error = %e, "First-seen tracker task panicked");
        }
    }

    server_result?;

    Ok(())
}

/// Build a [`mempool_pipeline::BackrunValidatorConfig`] from environment
/// variables. Returns `None` when `AETHER_EXECUTOR_ADDRESS` is unset — the
/// pipeline then runs analytical-only without attempting the revm
/// validator path. All other env vars have safe defaults documented inline.
fn build_backrun_validator_config(
    provider: Option<alloy::providers::DynProvider<alloy::network::Ethereum>>,
) -> Option<mempool_pipeline::BackrunValidatorConfig> {
    use alloy::primitives::{Address, U256};
    use std::str::FromStr;

    let executor_address = std::env::var("AETHER_EXECUTOR_ADDRESS")
        .ok()
        .and_then(|s| Address::from_str(s.trim()).ok())?;
    let searcher_caller = std::env::var("AETHER_SEARCHER_CALLER")
        .ok()
        .and_then(|s| Address::from_str(s.trim()).ok())
        .unwrap_or(executor_address);
    // WETH-9 mainnet. Override via env when running on a fork / testnet.
    let profit_token = std::env::var("AETHER_PROFIT_TOKEN")
        .ok()
        .and_then(|s| Address::from_str(s.trim()).ok())
        .unwrap_or_else(|| {
            Address::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
        });
    // WETH `_balances` mapping at slot 3. USDC = 9, USDT = 2, DAI = 2.
    let balance_slot = std::env::var("AETHER_PROFIT_TOKEN_BALANCE_SLOT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(U256::from)
        .unwrap_or_else(|| U256::from(3u64));
    let chain_id = std::env::var("AETHER_CHAIN_ID")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1);
    let min_profit_wei = std::env::var("AETHER_MEMPOOL_MIN_PROFIT_WEI")
        .ok()
        .and_then(|s| U256::from_str(s.trim()).ok())
        .unwrap_or_else(|| U256::from(1_000_000_000_000_000u64)); // 0.001 ETH
    let input_amount_wei = std::env::var("AETHER_MEMPOOL_INPUT_AMOUNT_WEI")
        .ok()
        .and_then(|s| U256::from_str(s.trim()).ok())
        .unwrap_or_else(|| U256::from(10_000_000_000_000_000u64)); // 0.01 ETH
    let sim_concurrency = std::env::var("AETHER_MEMPOOL_SIM_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(8);
    // Optional AetherExecutor runtime bytecode for demo / shadow runs
    // against a forked chain where the contract is not deployed. Path
    // points at forge's `out/AetherExecutor.sol/AetherExecutor.json` —
    // the hex string under `.deployedBytecode.object` is loaded and
    // injected into the revm CacheDB on every sim. Unset in production.
    let executor_bytecode = std::env::var("AETHER_EXECUTOR_BYTECODE_PATH")
        .ok()
        .and_then(|p| std::fs::read_to_string(p.trim()).ok())
        .and_then(|s| load_executor_runtime_bytecode(&s).ok());
    Some(mempool_pipeline::BackrunValidatorConfig {
        executor_address,
        searcher_caller,
        profit_token,
        balance_slot,
        chain_id,
        min_profit_wei,
        input_amount_wei,
        sim_semaphore: Arc::new(tokio::sync::Semaphore::new(sim_concurrency)),
        provider,
        // Placeholder — `SimContext::with_backrun_validator` overwrites this
        // with the SimContext's shared handle so the validator and the
        // background refresher rotate the same `ArcSwap`.
        mempool_prewarm: Arc::new(arc_swap::ArcSwap::from_pointee(None)),
        executor_bytecode,
    })
}

/// Pull the `deployedBytecode.object` hex string out of a forge artifact
/// JSON and decode it. Returns `Err` for missing field, malformed JSON,
/// or non-hex bytes — callers treat the error as "no bytecode override"
/// and the sim falls back to the on-chain fetch path.
fn load_executor_runtime_bytecode(
    artifact_json: &str,
) -> Result<alloy::primitives::Bytes, Box<dyn std::error::Error>> {
    let v: serde_json::Value = serde_json::from_str(artifact_json)?;
    let hex_str = v
        .get("deployedBytecode")
        .and_then(|d| d.get("object"))
        .and_then(|o| o.as_str())
        .ok_or("artifact missing deployedBytecode.object")?;
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = alloy::hex::decode(hex_str)?;
    Ok(alloy::primitives::Bytes::from(bytes))
}
