//! Mempool profit scorer — issue #132.
//!
//! Closes the value loop on PR #133 (predictions) + PR #134 (reconciliation):
//! for every prediction that confirmed, computes what our analytical arb
//! cycle would have realised against the **actual** post-state of the pool
//! at the block where the victim swap landed. The headline answer is
//! `SUM(net_profit_wei) WHERE decision='profitable'` over the soak window.
//!
//! Architecture:
//!
//! Bootstrap: load pools.toml → fetch all reserves at latest block →
//! build PriceGraph + TokenIndex. Held in `ScorerState` as the reference
//! graph; refreshed every `GRAPH_REFRESH_INTERVAL` so the "rest of the
//! graph" baseline stays close to current chain state.
//!
//! Poll loop: every `POLL_INTERVAL` SELECTs confirmed predictions that
//! have no profitability row yet. For each one we fetch the affected
//! pool's reserves at `actual_target_block` (one `eth_call` with a
//! historical BlockId), clone the reference graph and replace the
//! affected edge's reserves with the actual-block values, run
//! `BellmanFord::detect_from_affected` on the clone, and if a profitable
//! cycle is found we run the same ternary-search optimiser the engine
//! uses. The optimiser returns net_profit_wei (gross minus per-protocol
//! gas estimate); we INSERT the row with
//! `decision = profitable / unprofitable / no_path`.
//!
//! Approximation note: the "rest of the graph" reflects the latest fetched
//! reserves, not the actual_target_block. Properly fetching all 76 pools'
//! reserves at the prediction's block would cost 76 RPC calls per scoring
//! and is deferred. For most cycles (top pools shift slowly) the
//! approximation is acceptable; cases where it matters surface as
//! `decision=unprofitable` rows that PR-3 v2 (with full-block fetch) could
//! re-score upward.
//!
//! Inlined helpers (fetch_pool_state_at, build_graph, u256_to_f64, sol!
//! getReserves / slot0) are deliberate duplicates of the equivalents in
//! `bin/aether_replay.rs`. Extracting them into a shared module would
//! touch the merged replay file (2200+ lines) and inflate this PR's
//! review burden. Follow-up: deduplicate after the mempool phase lands.
//!
//! Run with:
//!
//!     MEMPOOL_LEDGER_DSN=postgres://aether:aether@localhost:5433/aether \
//!     ETH_RPC_URL=wss://eth-mainnet.g.alchemy.com/v2/<key> \
//!     AETHER_POOLS_CONFIG=$(pwd)/config/pools.toml \
//!     PROFIT_SCORER_METRICS_ADDR=:9095 \
//!     ./aether-profit-scorer

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::Ethereum;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use prometheus::{Encoder, Registry, TextEncoder};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use aether_common::types::{PoolId, ProtocolType, SwapStep};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas as gas_model;
use aether_detector::opportunity::DetectedCycle;
use aether_detector::optimizer::ternary_search_optimal_input;
use aether_grpc_server::profitability_writer::{
    profit_writer_from_env, NewProfitabilityScore, PgProfitabilityWriter, ProfitabilitySink,
    ProfitabilityWriterMetrics, UnscoredConfirmedPrediction, DECISION_NO_PATH,
    DECISION_PROFITABLE, DECISION_REVERTED, DECISION_UNPROFITABLE,
};
use aether_simulator::calldata::{
    build_execute_arb_calldata, build_univ2_swap_calldata, build_univ3_swap_calldata,
};
use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

/// Cadence of the unscored-prediction SQL poll. 30 s matches the
/// acceptance criterion in #132: "scorer processes every confirmed
/// prediction within 30 s of its reconciliation row".
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How often the reference graph is refreshed from latest-block reserves.
/// 5 min balances RPC budget against staleness; the per-scoring fetch
/// still hits the affected pool at actual_target_block so the affected
/// edge is always exact.
const GRAPH_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Maximum predictions scored per poll tick. Bounds memory + RPC fan-out
/// when the scorer starts with a backlog (e.g. it was offline for an hour
/// and 100+ unscored predictions are waiting).
const SCORE_BATCH_LIMIT: i64 = 25;

/// Maximum hops in a candidate cycle. Matches the engine's default so
/// the scorer reproduces the same paths the engine would have considered
/// at decode time.
const MAX_HOPS: usize = 4;

/// Bellman-Ford time budget per detection pass, in microseconds. Same
/// envelope as the engine's hot-path detection so the scorer's cycle
/// search is apples-to-apples with the production predictor.
const DETECT_BUDGET_US: u64 = 3_000;

/// 2^96 as f64. Used to convert UniswapV3 `sqrtPriceX96` into a
/// floating-point price.
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

/// Default base fee assumption (wei) when `eth_getBlock(latest)` is
/// unavailable. 30 gwei matches the engine's typical assumption in
/// quiet markets; replaced by the actual base fee on every refresh.
const DEFAULT_BASE_FEE_WEI: u128 = 30_000_000_000;

/// Upper bound on the number of pools augmented from `mempool_predictions`.
/// Bounds memory + bootstrap RPC fan-out (one `eth_call` per pool to fetch
/// reserves). The current production registry has ~55 pools; allow 5x
/// headroom while still containing pathological cases (e.g. a misbehaving
/// engine writing thousands of bogus pool addresses).
const MAX_DB_PREDICTED_POOLS: i64 = 256;

/// Default fee in basis points for DB-augmented pools whose protocol is
/// V2-style. Uniswap V2, SushiSwap, and almost every V2 fork charge 30 bps;
/// the 0.05% (5 bps) and 1% (100 bps) outliers exist but are rare enough on
/// V2 forks that the default is good enough for the f64 rate weight here.
/// The U256 verifier only uses fee_bps for V2/Sushi hops, where it's exact.
const DEFAULT_V2_FEE_BPS: u32 = 30;

/// Default fee for DB-augmented Uniswap V3 pools. V3's actual fee comes
/// from `pool.fee()` and lives in one of (1, 5, 30, 100) bps; we can't
/// know it without an extra RPC and the U256 verifier returns `None` for
/// V3 hops anyway, so this only affects the f64 rate path's graph weight
/// — a small error swamped by the rate magnitude itself.
const DEFAULT_V3_FEE_BPS: u32 = 5;

/// Safety floor for f64 fallback verdicts. The U256 verifier returns
/// `None` for any cycle it can't resolve exactly — V3 hops, drained
/// pools, edge-selection picking a pool whose state is missing, etc.
/// In those cases the score falls back to the f64 optimiser's number,
/// which is exactly the precision-biased path this PR set out to
/// contain. So: cap the trust. Any f64-only verdict claiming net
/// profit above this floor is downgraded to `DECISION_REVERTED` because
/// a 1+ ETH arb on mainnet would be captured intra-block by faster
/// searchers and never reach our scorer. The threshold is denominated
/// in the starting token's base units, which matches `net_profit_wei`.
const MAX_PLAUSIBLE_F64_NET_WEI: i128 = 1_000_000_000_000_000_000; // 1 ETH worth

// ── revm V3 verifier constants ─────────────────────────────────────

/// Mainnet infra addresses — constructor args for AetherExecutor.
const AAVE_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BANCOR_NETWORK: Address = address!("eEF417e1D5CC832e619ae18D2F140De2999dD4fB");
const WETH_ADDR: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC_ADDR: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const DAI_ADDR: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const USDT_ADDR: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

/// Deterministic deployer/owner for the scorer's in-revm executor.
const SIM_OWNER: Address = address!("1111111111111111111111111111111111111111");

/// Default executor artifact path (relative to CWD).
const DEFAULT_EXECUTOR_ARTIFACT: &str =
    "contracts/out/AetherExecutor.sol/AetherExecutor.json";

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
}

#[derive(Parser, Debug)]
#[command(name = "aether-profit-scorer", about = "Compute realised P&L per confirmed mempool prediction")]
struct Args {
    /// Path to the pool registry TOML. Defaults to ./config/pools.toml.
    #[arg(long, default_value = "config/pools.toml")]
    pools_config: PathBuf,

    /// Path to the forge-compiled AetherExecutor JSON artifact. Used by the
    /// revm verifier to deploy the executor inside pure-revm simulation for
    /// V3-touching cycles. If absent or unreadable, the revm path is
    /// disabled and V3 cycles fall back to the f64 absurdity floor.
    #[arg(long, default_value = DEFAULT_EXECUTOR_ARTIFACT)]
    executor_artifact: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();

    let dsn = std::env::var("MEMPOOL_LEDGER_DSN")
        .context("MEMPOOL_LEDGER_DSN required")?;
    let rpc_url = std::env::var("ETH_RPC_URL").context("ETH_RPC_URL required")?;
    let metrics_addr: SocketAddr = std::env::var("PROFIT_SCORER_METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9095".to_string())
        .parse()
        .context("PROFIT_SCORER_METRICS_ADDR must be host:port")?;
    let git_sha = std::env::var("AETHER_GIT_SHA").ok();

    // Load executor init bytecode for the revm V3 verifier. If the artifact
    // doesn't exist (e.g. forge not run, or scorer deployed without contracts/),
    // we log and continue — V3 cycles will fall back to the f64 absurdity floor.
    let executor_bytecode: Option<Arc<Vec<u8>>> = match load_executor_init_bytecode(&args.executor_artifact) {
        Ok(bc) => {
            info!(
                artifact = %args.executor_artifact.display(),
                bytecode_len = bc.len(),
                "Loaded executor init bytecode for revm V3 verifier"
            );
            Some(Arc::new(bc))
        }
        Err(e) => {
            warn!(
                artifact = %args.executor_artifact.display(),
                error = %e,
                "Could not load executor artifact; revm V3 verifier disabled (f64 fallback only)"
            );
            None
        }
    };

    info!("Loading pool config from {}", args.pools_config.display());
    let mut pools = load_pools(&args.pools_config)?;
    info!(pool_count = pools.len(), "Pools loaded from config");

    let registry = Registry::new();
    let writer_metrics = ProfitabilityWriterMetrics::register(&registry);
    let sink = profit_writer_from_env(Arc::clone(&writer_metrics)).await;

    // Separate PgPool for the read side: the writer's pool is for INSERTs
    // (small, bounded) and we keep reads off it so a write backlog can't
    // serialise the SELECT.
    let read_pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&dsn)
        .await
        .context("connect read pool")?;

    // Augment the registry with every distinct pool the engine has
    // already written a prediction for. The engine's runtime pair-index
    // extends past `pools.toml` whenever the mempool decoder spots a new
    // pool, but pre-#137 the scorer only loaded the static config — so
    // most predictions resolved as `no_path` even when the engine could
    // perfectly well graph them. This bootstrap pull closes that gap.
    let config_addresses: HashSet<Address> = pools.iter().map(|p| p.address).collect();
    match load_predicted_pools(&read_pool, &config_addresses).await {
        Ok(extra) => {
            info!(added_from_db = extra.len(), "DB-augmented pool registry");
            pools.extend(extra);
        }
        Err(e) => warn!(error = %e, "could not augment pools from DB; continuing with config only"),
    }

    // Convert WS RPC URL to HTTPS for the alloy HTTP provider. The fork
    // path in aether-replay does the same rewrite; replicated here so
    // the scorer accepts the same env var as the engine.
    let http_url = rewrite_ws_to_http(&rpc_url);
    let provider = ProviderBuilder::new()
        .connect_http(http_url.parse().context("parse RPC URL")?);
    // Type-erased provider for the revm verifier (requires DynProvider<Ethereum>).
    let dyn_provider: DynProvider<Ethereum> = DynProvider::new(provider.clone());

    info!("Bootstrapping reference graph (this fetches reserves for every pool at latest block)");
    let initial_state = bootstrap_state(&pools, &provider).await?;
    info!(
        graph_edges = initial_state.graph.num_edges(),
        base_fee_gwei = initial_state.base_fee_wei as f64 / 1e9,
        "Reference graph ready"
    );

    start_metrics_server(metrics_addr, registry.clone());

    let mut state = initial_state;
    let mut poll_ticker = interval(POLL_INTERVAL);
    poll_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut refresh_ticker = interval(GRAPH_REFRESH_INTERVAL);
    refresh_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the first tick since we just bootstrapped.
    refresh_ticker.tick().await;

    info!("Scorer started; polling every {:?}", POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = poll_ticker.tick() => {
                if let Err(e) = score_batch(
                    &read_pool, &provider, &pools, &state, sink.as_ref(),
                    git_sha.as_deref(),
                    executor_bytecode.as_ref(),
                    &dyn_provider,
                ).await {
                    warn!(error = %e, "score batch failed");
                }
            }
            _ = refresh_ticker.tick() => {
                // Pick up pools the engine has discovered since startup.
                // We re-run the same DB-augmentation as bootstrap, scoped
                // to addresses we don't already have. Failure here is
                // non-fatal — we keep the existing pool set if the SELECT
                // fails — because losing one refresh cycle is better than
                // killing the scorer over a transient DB blip.
                let known: HashSet<Address> = pools.iter().map(|p| p.address).collect();
                match load_predicted_pools(&read_pool, &known).await {
                    Ok(extra) if !extra.is_empty() => {
                        info!(added_from_db = extra.len(), "registry grew via mempool_predictions");
                        pools.extend(extra);
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "DB-augmented pool refresh failed"),
                }
                match bootstrap_state(&pools, &provider).await {
                    Ok(fresh) => {
                        info!(
                            base_fee_gwei = fresh.base_fee_wei as f64 / 1e9,
                            pool_count = pools.len(),
                            "reference graph refreshed"
                        );
                        state = fresh;
                    }
                    Err(e) => warn!(error = %e, "graph refresh failed; reusing previous reference"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received; exiting");
                break;
            }
        }
    }
    Ok(())
}

/// Single tick of the score loop: pull a batch of unscored confirmed
/// predictions and score each one.
#[allow(clippy::too_many_arguments)]
async fn score_batch(
    read_pool: &sqlx::PgPool,
    provider: &impl Provider,
    pools: &[LoadedPool],
    state: &ScorerState,
    sink: &dyn ProfitabilitySink,
    git_sha: Option<&str>,
    executor_bytecode: Option<&Arc<Vec<u8>>>,
    dyn_provider: &DynProvider<Ethereum>,
) -> Result<()> {
    let batch = PgProfitabilityWriter::fetch_unscored_confirmed(read_pool, SCORE_BATCH_LIMIT)
        .await
        .context("fetch unscored confirmed")?;
    if batch.is_empty() {
        debug!("no unscored confirmed predictions");
        return Ok(());
    }
    info!(count = batch.len(), "scoring batch");
    for pred in batch {
        match score_one(provider, pools, state, &pred, executor_bytecode, dyn_provider).await {
            Ok(score) => sink.insert_score(NewProfitabilityScore {
                prediction_id: pred.prediction_id,
                scored_at: Utc::now(),
                cycle_path: score.cycle_path,
                realized_profit_wei: score.realized_profit_wei,
                gas_estimate_wei: score.gas_estimate_wei,
                net_profit_wei: score.net_profit_wei,
                decision: score.decision,
                scoring_engine_git_sha: git_sha.map(str::to_string),
            }),
            Err(e) => warn!(
                prediction_id = %pred.prediction_id,
                error = %e,
                "score_one failed; skipping this prediction (will retry next tick)"
            ),
        }
    }
    Ok(())
}

/// Result of scoring a single prediction.
struct ScoreOutcome {
    cycle_path: serde_json::Value,
    realized_profit_wei: U256,
    gas_estimate_wei: U256,
    net_profit_wei: i128,
    decision: &'static str,
}

async fn score_one(
    provider: &impl Provider,
    pools: &[LoadedPool],
    state: &ScorerState,
    pred: &UnscoredConfirmedPrediction,
    executor_bytecode: Option<&Arc<Vec<u8>>>,
    dyn_provider: &DynProvider<Ethereum>,
) -> Result<ScoreOutcome> {
    // Locate the prediction's pool in the registry. A prediction whose
    // pool is no longer in the registry (rare; registry change between
    // prediction time and scoring time) lands as `no_path` so the row
    // still gets written and the dashboard sees the case.
    let Some(pool_idx) = pools.iter().position(|p| p.address == pred.pool_address) else {
        warn!(
            prediction_id = %pred.prediction_id,
            pool = %pred.pool_address,
            "pool absent from registry; emitting no_path"
        );
        return Ok(no_path_outcome(None));
    };
    let pool_entry = &pools[pool_idx];

    // Fetch actual reserves at the prediction's confirmed block.
    let actual_state = fetch_pool_state_at(provider, pool_entry, pred.actual_target_block)
        .await
        .context("fetch_pool_state_at")?;
    let Some(actual_state) = actual_state else {
        warn!(
            prediction_id = %pred.prediction_id,
            block = pred.actual_target_block,
            "eth_call returned no state; emitting no_path"
        );
        return Ok(no_path_outcome(None));
    };

    // Clone the reference graph, then overwrite the affected edge with
    // the actual-block reserves.
    let mut graph = state.graph.clone();
    let token_index = &state.token_index;
    let Some(t0) = token_index.get_index(&pool_entry.token0) else {
        return Ok(no_path_outcome(None));
    };
    let Some(t1) = token_index.get_index(&pool_entry.token1) else {
        return Ok(no_path_outcome(None));
    };
    let pool_id = PoolId {
        address: pool_entry.address,
        protocol: pool_entry.protocol,
    };
    let fee_factor = (10_000u32 - pool_entry.fee_bps) as f64 / 10_000.0;
    let (post0, post1) = state_to_graph_reserves(&actual_state);
    if post0 <= 0.0 || post1 <= 0.0 {
        return Ok(no_path_outcome(None));
    }
    graph.update_edge_from_reserves(t0, t1, pool_id, post0, post1, fee_factor);
    graph.update_edge_from_reserves(t1, t0, pool_id, post1, post0, fee_factor);

    // Run the same Bellman-Ford the engine uses at decode time. We restrict
    // to cycles through the affected tokens (detect_from_affected) so the
    // scorer doesn't burn time enumerating unrelated cycles.
    let detector = BellmanFord::new(MAX_HOPS, DETECT_BUDGET_US);
    let cycles = detector.detect_from_affected(&graph, &[t0, t1]);
    let profitable: Vec<DetectedCycle> = cycles.into_iter().filter(|c| c.is_profitable()).collect();
    if profitable.is_empty() {
        let gas = gas_estimate_for_protocols(&[pool_entry.protocol], state.base_fee_wei);
        return Ok(no_path_outcome(Some(gas)));
    }

    // Optimise the best cycle. The optimiser walks the cycle, applies the
    // post-state reserves to every V2 hop, and ternary-searches for the
    // input amount that maximises (output - input - gas).
    let best = &profitable[0];
    let running_states = collect_running_states(pools, &state.latest_states, pool_idx, actual_state);
    let Some(optimisation) = optimise_cycle(best, &graph, token_index, pools, &running_states, state.base_fee_wei) else {
        let gas = gas_estimate_for_protocols(&[pool_entry.protocol], state.base_fee_wei);
        return Ok(no_path_outcome(Some(gas)));
    };

    let gas_wei = optimisation.gas_cost_wei;
    let optimal_input_wei = optimisation.optimal_input_wei;

    // Decide: V2-only cycles get exact U256 math; V3-touching cycles route
    // through the revm verifier (deploy AetherExecutor + executeArb in pure
    // revm). Cycles that neither path can resolve fall back to the f64
    // absurdity floor.
    let v3_touching = is_v3_touching_cycle(best, &graph, token_index, pools, &running_states);

    let (net, realized_wei_i128, decision) = if !v3_touching {
        // V2-only: exact U256 getAmountOut walk (unchanged from pre-V3 scorer).
        let verified_gross = verify_cycle_u256(
            best,
            &graph,
            token_index,
            pools,
            &running_states,
            optimal_input_wei,
        );
        match verified_gross {
            Some(gross_out) => {
                let gross_i128 = u256_to_i128_saturating(gross_out)
                    .saturating_sub(u256_to_i128_saturating(optimal_input_wei));
                let exact_net = gross_i128.saturating_sub(gas_wei as i128);
                let realised = gross_i128.max(0);
                let decision = if gross_out < optimal_input_wei {
                    DECISION_REVERTED
                } else if exact_net > 0 {
                    DECISION_PROFITABLE
                } else {
                    DECISION_UNPROFITABLE
                };
                (exact_net, realised, decision)
            }
            None => f64_fallback_verdict(optimisation.net_profit_wei, gas_wei),
        }
    } else if let Some(executor_bc) = executor_bytecode {
        // V3-touching: deploy+simulate via pure revm.
        let verdict = verify_cycle_revm(
            best,
            &graph,
            token_index,
            pools,
            &running_states,
            optimal_input_wei,
            dyn_provider,
            executor_bc,
            state.block_number,
            state.block_timestamp,
            state.base_fee_wei as u64,
        );
        match verdict {
            Some(rv) => revm_verdict_to_decision(rv, gas_wei),
            // revm couldn't resolve (unsupported token, Curve hop, etc.)
            None => f64_fallback_verdict(optimisation.net_profit_wei, gas_wei),
        }
    } else {
        // No executor bytecode available — pure f64 fallback.
        f64_fallback_verdict(optimisation.net_profit_wei, gas_wei)
    };

    let realized_wei = U256::from(realized_wei_i128 as u128);
    let gas_estimate_wei = U256::from(gas_wei);

    let cycle_json = cycle_to_json(best, &graph, token_index, pools);

    Ok(ScoreOutcome {
        cycle_path: cycle_json,
        realized_profit_wei: realized_wei,
        gas_estimate_wei,
        net_profit_wei: net,
        decision,
    })
}

fn no_path_outcome(gas: Option<u128>) -> ScoreOutcome {
    let gas_wei = gas.unwrap_or(0);
    ScoreOutcome {
        cycle_path: serde_json::Value::Array(vec![]),
        realized_profit_wei: U256::ZERO,
        gas_estimate_wei: U256::from(gas_wei),
        net_profit_wei: -(gas_wei as i128),
        decision: DECISION_NO_PATH,
    }
}

/// Convert `PoolState` to graph-edge reserves matching how the engine
/// seeds them: V2 keeps `(r0, r1)`; V3 uses a synthetic `(1.0,
/// spot_price)` pair so Bellman-Ford treats the two families
/// identically (the engine's mempool pipeline does the same mapping).
fn state_to_graph_reserves(state: &PoolState) -> (f64, f64) {
    match state {
        PoolState::V2 { r0, r1 } => (u256_to_f64(*r0), u256_to_f64(*r1)),
        PoolState::V3 { sqrt_price_x96 } => {
            let sqrt_f = u256_to_f64(*sqrt_price_x96);
            if sqrt_f == 0.0 {
                return (0.0, 0.0);
            }
            let root = sqrt_f / Q96;
            (1.0, root * root)
        }
    }
}

/// Merge the latest per-pool states (refreshed by the bootstrap loop)
/// with the affected pool's actual-block state. This is the map the
/// optimiser consults when walking each cycle hop.
fn collect_running_states(
    pools: &[LoadedPool],
    latest_states: &HashMap<usize, PoolState>,
    affected_idx: usize,
    affected_state: PoolState,
) -> HashMap<usize, PoolState> {
    let mut out = HashMap::with_capacity(pools.len());
    for (idx, state) in latest_states.iter() {
        out.insert(*idx, *state);
    }
    out.insert(affected_idx, affected_state);
    out
}

struct OptimiserSuccess {
    net_profit_wei: i128,
    gas_cost_wei: u128,
    /// Input amount the ternary search converged on. Exposed so the
    /// post-optimiser U256 verifier (`verify_cycle_u256`) can re-walk
    /// the cycle with exact integer math at the same input the f64
    /// optimiser scored, and either confirm the profit or downgrade the
    /// row to `DECISION_REVERTED` when f64 precision overstated reserves.
    optimal_input_wei: U256,
}

fn optimise_cycle(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    running_states: &HashMap<usize, PoolState>,
    base_fee_wei: u128,
) -> Option<OptimiserSuccess> {
    if cycle.path.len() < 2 {
        return None;
    }
    let mut hop_reserves: Vec<(f64, f64)> = Vec::with_capacity(cycle.path.len() - 1);
    let mut hop_fee_factors: Vec<f64> = Vec::with_capacity(cycle.path.len() - 1);
    let mut hop_rates: Vec<f64> = Vec::with_capacity(cycle.path.len() - 1);
    let mut protocols: Vec<ProtocolType> = Vec::with_capacity(cycle.path.len() - 1);
    let mut min_liquidity_wei: Option<f64> = None;

    for pair in cycle.path.windows(2) {
        let [from_v, to_v] = [pair[0], pair[1]];
        let edge = graph
            .edges_from(from_v)
            .iter()
            .filter(|e| e.to == to_v)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))?;

        let token_in = *token_index.get_address(from_v)?;
        let (pool_idx, pool_entry) = pools
            .iter()
            .enumerate()
            .find(|(_, p)| p.address == edge.pool_address)?;
        let fee_multiplier = (10_000u32 - pool_entry.fee_bps) as f64 / 10_000.0;
        let rate = (-edge.weight).exp();

        let (rin, rout) = match running_states.get(&pool_idx).copied() {
            Some(PoolState::V2 { r0, r1 }) => {
                let (ri, ro) = if token_in == pool_entry.token0 {
                    (r0, r1)
                } else {
                    (r1, r0)
                };
                let ri_f = u256_to_f64(ri);
                if min_liquidity_wei.is_none_or(|prev| prev > ri_f) {
                    min_liquidity_wei = Some(ri_f);
                }
                (ri_f, u256_to_f64(ro))
            }
            // V3 / unknown: optimiser falls back to rate-only path.
            Some(PoolState::V3 { .. }) | None => (0.0, 0.0),
        };

        hop_reserves.push((rin, rout));
        hop_fee_factors.push(fee_multiplier);
        hop_rates.push(rate);
        protocols.push(pool_entry.protocol);
    }

    let min_input = U256::from(10_000_000_000_000_000u128); // 0.01 ETH
    let hard_max = U256::from(50_000_000_000_000_000_000u128); // 50 ETH
    let max_input = match min_liquidity_wei {
        Some(liq) if liq > 0.0 => {
            let liq_u256 = U256::from(liq as u128);
            if liq_u256 < hard_max {
                liq_u256
            } else {
                hard_max
            }
        }
        _ => hard_max,
    };

    let ticks = vec![0u32; protocols.len()];
    let gas_units = gas_model::estimate_total_gas(&protocols, &ticks);
    let base_fee_gwei = base_fee_wei as f64 / 1e9;
    let gas_cost_wei = gas_model::gas_cost_wei(gas_units, base_fee_gwei);

    let profit_fn = |input: U256| -> i128 {
        let mut current = u256_to_f64(input);
        for i in 0..hop_reserves.len() {
            let (x, y) = hop_reserves[i];
            let fee = hop_fee_factors[i];
            if x > 0.0 && y > 0.0 {
                current = (current * fee * y) / (x + current * fee);
            } else {
                current *= hop_rates[i];
            }
        }
        let output = current as i128;
        let input_i128 = u256_to_f64(input) as i128;
        output
            .saturating_sub(input_i128)
            .saturating_sub(gas_cost_wei as i128)
    };

    let (optimal_input_wei, net_profit_wei) = if min_input < max_input {
        ternary_search_optimal_input(min_input, max_input, 80, profit_fn)
    } else {
        let p = profit_fn(min_input);
        (min_input, p)
    };

    Some(OptimiserSuccess {
        net_profit_wei,
        gas_cost_wei,
        optimal_input_wei,
    })
}

/// Re-walk the optimiser's chosen cycle with exact U256 V2 math and return
/// the gross output amount in the cycle's starting token, or `None` when
/// the cycle isn't fully V2-decidable.
///
/// Returns `None` (inconclusive — caller falls back to the f64 optimiser's
/// verdict) when:
/// - any hop's running state is missing
/// - any hop is V3 (`PoolState::V3`) — V3 amount-out needs tick traversal;
///   replicating that here is out of scope for the precision fix
/// - any hop has zero-or-degenerate reserves
/// - the graph edge doesn't resolve cleanly back to a registry pool
///
/// Returns `Some(gross_wei)` when every hop resolves to a V2/Sushi pool
/// with positive reserves. The caller compares `gross_wei` against the
/// starting input: `gross < input` ⇒ `DECISION_REVERTED` (f64 bias),
/// otherwise the exact net = gross − input − gas drives the decision.
fn verify_cycle_u256(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    running_states: &HashMap<usize, PoolState>,
    optimal_input_wei: U256,
) -> Option<U256> {
    if cycle.path.len() < 2 || optimal_input_wei.is_zero() {
        return None;
    }
    // Per-pool reserve copy that we mutate as the cycle progresses. When
    // a multi-hop cycle revisits the same pool (e.g. A→B→A self-loops the
    // Bellman-Ford detector can emit whenever both edge directions exist
    // on a single pool), the second hop MUST see reserves shifted by hop
    // 1's swap; otherwise the verifier double-uses the pre-swap reserves
    // and lets the second hop "regenerate" the input out of thin air,
    // producing ETH-scale ghost profit identical in shape to the f64
    // precision bias this PR set out to remove.
    //
    // Keyed by `pool_idx` so address-collision is impossible. Entries are
    // only ever V2 `(r0, r1)` pairs — V3 hops short-circuit to `None` on
    // first encounter, so any present entry is guaranteed V2.
    let mut local_reserves: HashMap<usize, (U256, U256)> = HashMap::new();

    let mut current_amount = optimal_input_wei;
    for pair in cycle.path.windows(2) {
        let [from_v, to_v] = [pair[0], pair[1]];
        let edge = graph
            .edges_from(from_v)
            .iter()
            .filter(|e| e.to == to_v)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))?;
        let token_in = *token_index.get_address(from_v)?;
        let (pool_idx, pool_entry) = pools
            .iter()
            .enumerate()
            .find(|(_, p)| p.address == edge.pool_address)?;

        let (r0, r1) = match local_reserves.get(&pool_idx).copied() {
            Some(rs) => rs,
            None => match running_states.get(&pool_idx).copied()? {
                PoolState::V2 { r0, r1 } => (r0, r1),
                // V3 hop: out of scope for the U256 verifier; signal
                // caller to keep the f64 verdict.
                PoolState::V3 { .. } => return None,
            },
        };
        let zero_for_one = token_in == pool_entry.token0;
        let (r_in, r_out) = if zero_for_one { (r0, r1) } else { (r1, r0) };
        let amount_out =
            uniswap_v2_get_amount_out(current_amount, r_in, r_out, pool_entry.fee_bps)?;
        if amount_out.is_zero() {
            return None;
        }

        // Apply the swap to the local copy so subsequent hops on this
        // pool see the post-swap reserves. V2 invariant
        // (`r_in_new * r_out_new ≥ r_in * r_out`) is preserved exactly by
        // construction since `uniswap_v2_get_amount_out` returns the
        // largest `amount_out` consistent with the curve.
        let r_in_new = r_in.checked_add(current_amount)?;
        let r_out_new = r_out.checked_sub(amount_out)?;
        let new_state = if zero_for_one {
            (r_in_new, r_out_new)
        } else {
            (r_out_new, r_in_new)
        };
        local_reserves.insert(pool_idx, new_state);

        current_amount = amount_out;
    }
    Some(current_amount)
}

/// UniswapV2 `getAmountOut` — exact U256 math, no rounding. Same formula
/// the pool's `swap()` invariant check enforces on-chain, so the verifier
/// here is byte-identical to what would actually execute. Returns `None`
/// when any leg has zero reserves / zero input (drained-pool guard) or any
/// intermediate multiplication overflows U256.
fn uniswap_v2_get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> Option<U256> {
    if reserve_in.is_zero() || reserve_out.is_zero() || amount_in.is_zero() {
        return None;
    }
    // fee_bps = 30 (0.30%) → multiplier 9970/10000. The 10_000 - fee_bps
    // form matches the contract's hard-coded numerator for the default 30
    // bps pool and generalises to lower-fee Uni V2 forks.
    let fee_multiplier = U256::from(10_000u64.saturating_sub(fee_bps as u64));
    let amount_in_with_fee = amount_in.checked_mul(fee_multiplier)?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denominator = reserve_in
        .checked_mul(U256::from(10_000u64))?
        .checked_add(amount_in_with_fee)?;
    if denominator.is_zero() {
        return None;
    }
    Some(numerator / denominator)
}

/// U256 → i128 with saturating overflow. The scorer's `net_profit_wei`
/// column is i128; profits beyond i128::MAX wei (≈170 quadrillion ETH —
/// numerically unreachable on Ethereum) saturate rather than wrap. The
/// guard exists for the precision-bias path where an unbounded f64 may
/// have proposed an input larger than i128 can hold.
fn u256_to_i128_saturating(v: U256) -> i128 {
    let limbs = v.as_limbs();
    // i128 fits in limbs[0] + limbs[1] (each limb is u64). Anything beyond
    // limbs[1]'s sign bit overflows.
    if limbs[2] != 0 || limbs[3] != 0 || (limbs[1] >> 63) == 1 {
        return i128::MAX;
    }
    ((limbs[1] as i128) << 64) | (limbs[0] as i128)
}

fn gas_estimate_for_protocols(protocols: &[ProtocolType], base_fee_wei: u128) -> u128 {
    let ticks = vec![0u32; protocols.len()];
    let units = gas_model::estimate_total_gas(protocols, &ticks);
    gas_model::gas_cost_wei(units, base_fee_wei as f64 / 1e9)
}

// ── V3 revm verifier ──────────────────────────────────────────────

/// Result from the revm deploy+simulate verifier.
#[derive(Debug, Clone, Copy)]
struct RevmVerdict {
    /// Gross profit in the cycle's starting token (ERC20 balance delta on
    /// SIM_OWNER after executeArb). Zero on revert.
    gross_profit_wei: U256,
    /// Gas consumed by the executeArb CALL (excludes CREATE overhead).
    /// Currently informational only — the decision mapping uses the
    /// scorer's static `gas_estimate_for_protocols` rather than revm's
    /// measured cost, so this field is populated but not yet read by
    /// the decision path. Kept for forthcoming gas-model calibration.
    #[allow(dead_code)]
    gas_used: u64,
    /// True if the executeArb CALL reverted or halted.
    reverted: bool,
}

/// Map a `RevmVerdict` into `(net, realised_i128, decision)`.
fn revm_verdict_to_decision(rv: RevmVerdict, gas_cost_wei: u128) -> (i128, i128, &'static str) {
    if rv.reverted {
        let gas_i128 = gas_cost_wei as i128;
        (-(gas_i128), 0, DECISION_REVERTED)
    } else {
        let gross_i128 = u256_to_i128_saturating(rv.gross_profit_wei);
        let net = gross_i128.saturating_sub(gas_cost_wei as i128);
        let realised = gross_i128.max(0);
        let decision = if net > 0 {
            DECISION_PROFITABLE
        } else {
            DECISION_UNPROFITABLE
        };
        (net, realised, decision)
    }
}

/// Fallback for cycles that neither the U256 walker nor revm can resolve.
/// Applies the absurdity floor: f64 nets above 1 ETH are downgraded to
/// REVERTED (precision-bias artefact).
fn f64_fallback_verdict(f64_net: i128, gas_cost_wei: u128) -> (i128, i128, &'static str) {
    let realised = f64_net.saturating_add(gas_cost_wei as i128).max(0);
    let decision = if f64_net > MAX_PLAUSIBLE_F64_NET_WEI {
        DECISION_REVERTED
    } else if f64_net > 0 {
        DECISION_PROFITABLE
    } else {
        DECISION_UNPROFITABLE
    };
    (f64_net, realised, decision)
}

/// Walk the cycle's hops and return `true` if any hop's pool state is
/// `PoolState::V3`. O(hops) — typically 2-4 iterations.
fn is_v3_touching_cycle(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    running_states: &HashMap<usize, PoolState>,
) -> bool {
    for pair in cycle.path.windows(2) {
        let [from_v, to_v] = [pair[0], pair[1]];
        let edge = match graph
            .edges_from(from_v)
            .iter()
            .filter(|e| e.to == to_v)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))
        {
            Some(e) => e,
            None => continue,
        };
        // Resolve to a pool, check if it has a V3 state.
        if token_index.get_address(from_v).is_none() {
            continue;
        }
        let pool_idx = match pools.iter().position(|p| p.address == edge.pool_address) {
            Some(i) => i,
            None => continue,
        };
        if matches!(running_states.get(&pool_idx), Some(PoolState::V3 { .. })) {
            return true;
        }
    }
    false
}

/// Return the ERC20 `_balances` mapping storage slot for well-known mainnet
/// tokens. Returns `None` for tokens without a known slot — the revm verifier
/// returns `None` (f64 fallback) for those cycles.
fn balance_slot_for_token(token: Address) -> Option<U256> {
    if token == WETH_ADDR {
        Some(U256::from(3u64))
    } else if token == USDC_ADDR {
        Some(U256::from(9u64))
    } else if token == DAI_ADDR || token == USDT_ADDR {
        Some(U256::from(2u64))
    } else {
        None
    }
}

/// Load AetherExecutor init-bytecode from the forge-compiled JSON artifact.
fn load_executor_init_bytecode(artifact_path: &PathBuf) -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string(artifact_path)
        .with_context(|| format!("read executor artifact {}", artifact_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).context("parse executor artifact JSON")?;
    let hex_str = v
        .pointer("/bytecode/object")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing /bytecode/object in artifact"))?;
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = alloy::hex::decode(stripped).context("decode bytecode hex")?;
    if bytes.is_empty() {
        anyhow::bail!("executor bytecode is empty");
    }
    Ok(bytes)
}

/// Build `Vec<SwapStep>` from a detected cycle using pre-fetched running
/// states (synchronous — no RPC calls). Ported from aether_replay's
/// `build_steps_from_cycle` but sync and fed from `running_states`.
///
/// Returns `None` if any hop touches Curve/Balancer/Bancor, has missing
/// state, or produces zero output.
fn build_steps_from_cycle_sync(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    running_states: &HashMap<usize, PoolState>,
    executor_addr: Address,
    flashloan_amount: U256,
) -> Option<Vec<SwapStep>> {
    if cycle.path.len() < 2 {
        return None;
    }
    let mut current_amount = flashloan_amount;
    let mut steps: Vec<SwapStep> = Vec::with_capacity(cycle.path.len() - 1);

    for pair in cycle.path.windows(2) {
        let [from_v, to_v] = [pair[0], pair[1]];
        let edge = graph
            .edges_from(from_v)
            .iter()
            .filter(|e| e.to == to_v)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))?;

        let token_in = *token_index.get_address(from_v)?;
        let token_out = *token_index.get_address(to_v)?;
        let (pool_idx, pool_entry) = pools
            .iter()
            .enumerate()
            .find(|(_, p)| p.address == edge.pool_address)?;

        let state = running_states.get(&pool_idx).copied()?;
        let (amount_out, inner_calldata) = match (pool_entry.protocol, state) {
            (ProtocolType::UniswapV2 | ProtocolType::SushiSwap, PoolState::V2 { r0, r1 }) => {
                let (reserve_in, reserve_out, zero_for_one) = if token_in == pool_entry.token0 {
                    (r0, r1, true)
                } else {
                    (r1, r0, false)
                };
                let out = uniswap_v2_get_amount_out(current_amount, reserve_in, reserve_out, pool_entry.fee_bps)?;
                if out.is_zero() {
                    return None;
                }
                let (amount0_out, amount1_out) = if zero_for_one {
                    (U256::ZERO, out)
                } else {
                    (out, U256::ZERO)
                };
                let cd = build_univ2_swap_calldata(amount0_out, amount1_out, executor_addr);
                (out, cd)
            }
            (ProtocolType::UniswapV3, PoolState::V3 { .. }) => {
                // V3: approximate output from graph edge rate; the revm sim
                // produces the real executable amount via tick traversal.
                let rate = (-edge.weight).exp();
                let approx_out = U256::from((u256_to_f64(current_amount) * rate).max(0.0) as u128);
                if approx_out.is_zero() {
                    return None;
                }
                let zero_for_one = token_in == pool_entry.token0;
                let sqrt_limit = if zero_for_one {
                    U256::from(4_295_128_740u64) // MIN_SQRT_RATIO + 1
                } else {
                    (U256::from(1u8) << 160) - U256::from(2u8) // MAX_SQRT_RATIO - 1
                };
                let amt_i128 = i128::try_from(current_amount.saturating_to::<u128>()).ok()?;
                let cd = build_univ3_swap_calldata(executor_addr, zero_for_one, amt_i128, sqrt_limit);
                (approx_out, cd)
            }
            // Curve / Balancer / Bancor: out of scope for V3 verifier.
            _ => return None,
        };

        steps.push(SwapStep {
            protocol: pool_entry.protocol,
            pool_address: pool_entry.address,
            token_in,
            token_out,
            amount_in: current_amount,
            min_amount_out: U256::ZERO,
            calldata: inner_calldata,
        });

        current_amount = amount_out;
    }

    Some(steps)
}

/// Verify a V3-touching cycle by deploying AetherExecutor inside pure revm
/// and calling `executeArb`. Returns `None` when the cycle can't be resolved
/// (unsupported token for balance-slot, Curve/Balancer hop, build failure).
///
/// Runs synchronously — callers should wrap in `spawn_blocking` if on an
/// async context (the scorer's `score_one` is already async but the revm
/// transact calls `block_in_place` internally via AlloyDB).
#[allow(clippy::too_many_arguments)]
fn verify_cycle_revm(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    running_states: &HashMap<usize, PoolState>,
    optimal_input_wei: U256,
    provider: &DynProvider<Ethereum>,
    executor_init_bytecode: &[u8],
    block_number: u64,
    block_timestamp: u64,
    base_fee: u64,
) -> Option<RevmVerdict> {
    if cycle.path.len() < 2 || optimal_input_wei.is_zero() {
        return None;
    }
    // The cycle's starting token = flashloan asset = profit token.
    let start_token = *token_index.get_address(cycle.path[0])?;
    let balance_slot = balance_slot_for_token(start_token)?;

    // We need a temporary executor address for inner-calldata recipients.
    // Since we don't know the deployed address yet, we pre-compute it:
    // CREATE from SIM_OWNER at nonce 0 → deterministic address.
    let executor_addr = SIM_OWNER.create(0);

    let steps = build_steps_from_cycle_sync(
        cycle,
        graph,
        token_index,
        pools,
        running_states,
        executor_addr,
        optimal_input_wei,
    )?;

    if steps.is_empty() {
        return None;
    }

    let calldata = build_execute_arb_calldata(
        &steps,
        start_token,
        optimal_input_wei,
        U256::from(u64::MAX), // deadline
        U256::ZERO,           // minProfitOut
        U256::ZERO,           // tipBps
    );

    let ctor_args = (AAVE_POOL, BALANCER_VAULT, BANCOR_NETWORK).abi_encode_params();

    let fork_state = RpcForkedState::new(
        provider.clone(),
        block_number,
        block_timestamp,
        base_fee,
    )?;

    let sim = EvmSimulator::new(SimConfig {
        gas_limit: 8_000_000,
        chain_id: 1,
        caller: SIM_OWNER,
        value: U256::ZERO,
    });

    let result = sim.deploy_and_simulate_with_erc20_profit(
        fork_state,
        SIM_OWNER,
        executor_init_bytecode,
        &ctor_args,
        calldata,
        start_token,
        SIM_OWNER,
        balance_slot,
    );

    Some(RevmVerdict {
        gross_profit_wei: result.profit_wei,
        gas_used: result.gas_used,
        reverted: !result.success,
    })
}

/// Serialise a DetectedCycle into the JSONB shape the dashboard reads.
/// Each hop carries `pool`, `token_in`, `token_out`, `protocol`.
fn cycle_to_json(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
) -> serde_json::Value {
    let mut hops = Vec::with_capacity(cycle.path.len().saturating_sub(1));
    for pair in cycle.path.windows(2) {
        let [from_v, to_v] = [pair[0], pair[1]];
        let Some(edge) = graph.edges_from(from_v).iter().find(|e| e.to == to_v) else {
            continue;
        };
        let Some(token_in) = token_index.get_address(from_v) else {
            continue;
        };
        let Some(token_out) = token_index.get_address(to_v) else {
            continue;
        };
        let proto_label = pools
            .iter()
            .find(|p| p.address == edge.pool_address)
            .map(|p| protocol_label(p.protocol))
            .unwrap_or("unknown");
        hops.push(serde_json::json!({
            "pool": format!("{:#x}", edge.pool_address),
            "token_in": format!("{:#x}", token_in),
            "token_out": format!("{:#x}", token_out),
            "protocol": proto_label,
        }));
    }
    serde_json::Value::Array(hops)
}

fn protocol_label(p: ProtocolType) -> &'static str {
    match p {
        ProtocolType::UniswapV2 => "uni_v2",
        ProtocolType::UniswapV3 => "uni_v3",
        ProtocolType::SushiSwap => "sushi",
        ProtocolType::Curve => "curve",
        ProtocolType::BalancerV2 => "balancer",
        ProtocolType::BancorV3 => "bancor",
    }
}

// ----- inlined helpers (duplicate of aether_replay.rs; see module docstring) -----

#[derive(Clone, Copy, Debug)]
enum PoolState {
    V2 { r0: U256, r1: U256 },
    V3 { sqrt_price_x96: U256 },
}

#[derive(Clone, Debug)]
struct LoadedPool {
    address: Address,
    token0: Address,
    token1: Address,
    protocol: ProtocolType,
    fee_bps: u32,
}

#[derive(Deserialize)]
struct PoolsConfig {
    pools: Vec<PoolEntry>,
}

#[derive(Deserialize)]
struct PoolEntry {
    address: String,
    token0: String,
    token1: String,
    protocol: String,
    fee_bps: u32,
}

fn parse_protocol(s: &str) -> Option<ProtocolType> {
    match s {
        "uniswap_v2" => Some(ProtocolType::UniswapV2),
        "sushiswap" => Some(ProtocolType::SushiSwap),
        "uniswap_v3" => Some(ProtocolType::UniswapV3),
        "curve" => Some(ProtocolType::Curve),
        "balancer_v2" => Some(ProtocolType::BalancerV2),
        "bancor_v3" => Some(ProtocolType::BancorV3),
        _ => None,
    }
}

/// Map the short-form protocol strings the engine writes into
/// `mempool_predictions.protocol` (see `aether_grpc_server::mempool_writer`
/// `PROTOCOL_*` constants) to `ProtocolType`. Distinct from
/// [`parse_protocol`], which reads the long-form names used in
/// `config/pools.toml`. Kept narrow on purpose: only the protocols the
/// scorer can actually score are returned; Balancer / Curve / Bancor
/// fall through to `None` so we don't add edges for hops the engine
/// can't compute reserves for at present.
fn parse_db_protocol(s: &str) -> Option<ProtocolType> {
    match s {
        "uni_v2" => Some(ProtocolType::UniswapV2),
        "uni_v3" => Some(ProtocolType::UniswapV3),
        "sushi" => Some(ProtocolType::SushiSwap),
        _ => None,
    }
}

/// Augment the static `config/pools.toml` registry with every distinct
/// pool the engine has actually written a prediction for, but doesn't
/// appear in the config. The engine's runtime pair-index extends as
/// mempool decoding discovers new pools; the scorer's old behaviour of
/// loading only the TOML config meant ~88% of confirmed predictions
/// resolved as `decision='no_path'` even when their pool existed in the
/// engine's graph at decode time.
///
/// `known` is the set of addresses already present from the config; pools
/// in `known` are skipped so we don't double-register them.
///
/// Returns up to `MAX_DB_PREDICTED_POOLS` distinct LoadedPool entries.
/// The cap exists so a runaway engine writing thousands of pool
/// addresses can't blow the bootstrap's RPC fan-out (one eth_call per
/// pool) or memory. The query orders by pool_address so the truncation
/// is deterministic — same set across restarts unless the underlying
/// table changes.
async fn load_predicted_pools(
    pg_pool: &sqlx::PgPool,
    known: &HashSet<Address>,
) -> Result<Vec<LoadedPool>> {
    // Pull (pool, protocol, sample token_in, sample token_out) for every
    // distinct pool address. token_in/token_out come from one arbitrary
    // prediction row per pool; we use them only to derive the canonical
    // (token0, token1) ordering, which is direction-agnostic by V2/V3
    // invariant (token0 = min(addr), token1 = max(addr)).
    // `(pool_address, protocol, token_in, token_out)` — all bytea fields
    // come back as `Vec<u8>` from sqlx. Aliased so clippy doesn't flag
    // the nested generic.
    type DbPoolRow = (Vec<u8>, String, Vec<u8>, Vec<u8>);
    let rows: Vec<DbPoolRow> = sqlx::query_as(
        "SELECT DISTINCT ON (pool_address) pool_address, protocol, token_in, token_out \
         FROM mempool_predictions \
         WHERE pool_address IS NOT NULL \
         ORDER BY pool_address, decoded_at DESC \
         LIMIT $1",
    )
    .bind(MAX_DB_PREDICTED_POOLS)
    .fetch_all(pg_pool)
    .await
    .context("SELECT DISTINCT pool_address FROM mempool_predictions")?;

    let mut out = Vec::with_capacity(rows.len());
    for (pool_bytes, proto_str, tin_bytes, tout_bytes) in rows {
        if pool_bytes.len() != 20 || tin_bytes.len() != 20 || tout_bytes.len() != 20 {
            warn!(
                pool_len = pool_bytes.len(),
                tin_len = tin_bytes.len(),
                tout_len = tout_bytes.len(),
                "skipping db pool with non-20-byte address fields"
            );
            continue;
        }
        let addr = Address::from_slice(&pool_bytes);
        if known.contains(&addr) {
            continue;
        }
        let Some(protocol) = parse_db_protocol(&proto_str) else {
            // Balancer / Curve / Bancor / unknown — out of scope for the
            // current scoring path. Tracked as future work.
            debug!(protocol = %proto_str, pool = %addr, "skipping db pool with unsupported protocol");
            continue;
        };
        let tin = Address::from_slice(&tin_bytes);
        let tout = Address::from_slice(&tout_bytes);
        let (token0, token1) = if tin < tout { (tin, tout) } else { (tout, tin) };
        let fee_bps = match protocol {
            ProtocolType::UniswapV3 => DEFAULT_V3_FEE_BPS,
            _ => DEFAULT_V2_FEE_BPS,
        };
        out.push(LoadedPool {
            address: addr,
            token0,
            token1,
            protocol,
            fee_bps,
        });
    }
    Ok(out)
}

fn load_pools(path: &PathBuf) -> Result<Vec<LoadedPool>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read pool config {}", path.display()))?;
    let cfg: PoolsConfig = toml::from_str(&raw).context("parse pool config")?;
    let mut out = Vec::new();
    for entry in cfg.pools {
        let Some(protocol) = parse_protocol(&entry.protocol) else {
            continue;
        };
        // v1 scorer supports the same protocols aether-replay supports.
        if !matches!(
            protocol,
            ProtocolType::UniswapV2 | ProtocolType::SushiSwap | ProtocolType::UniswapV3
        ) {
            continue;
        }
        out.push(LoadedPool {
            address: entry.address.parse().context("pool address")?,
            token0: entry.token0.parse().context("token0")?,
            token1: entry.token1.parse().context("token1")?,
            protocol,
            fee_bps: entry.fee_bps,
        });
    }
    Ok(out)
}

async fn fetch_pool_state_at(
    provider: &impl Provider,
    pool: &LoadedPool,
    block: u64,
) -> Result<Option<PoolState>> {
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block));
    let state = match pool.protocol {
        ProtocolType::UniswapV2 | ProtocolType::SushiSwap => {
            let calldata = getReservesCall {}.abi_encode();
            let tx = TransactionRequest::default()
                .to(pool.address)
                .input(calldata.into());
            let out = provider.call(tx).block(block_id).await?;
            if out.len() >= 64 {
                Some(PoolState::V2 {
                    r0: U256::from_be_slice(&out[0..32]),
                    r1: U256::from_be_slice(&out[32..64]),
                })
            } else {
                None
            }
        }
        ProtocolType::UniswapV3 => {
            let calldata = slot0Call {}.abi_encode();
            let tx = TransactionRequest::default()
                .to(pool.address)
                .input(calldata.into());
            let out = provider.call(tx).block(block_id).await?;
            if out.len() >= 32 {
                Some(PoolState::V3 {
                    sqrt_price_x96: U256::from_be_slice(&out[0..32]),
                })
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(state)
}

fn u256_to_f64(v: U256) -> f64 {
    let limbs = v.as_limbs();
    let mut acc = 0.0f64;
    for (i, &limb) in limbs.iter().enumerate() {
        acc += (limb as f64) * (2f64).powi((64 * i) as i32);
    }
    acc
}

struct ScorerState {
    graph: PriceGraph,
    token_index: TokenIndex,
    /// Per-pool reserves at the latest fetched block. Keyed by index into
    /// the `pools` slice so the optimiser can look up by pool-registry
    /// position rather than by address.
    latest_states: HashMap<usize, PoolState>,
    base_fee_wei: u128,
    /// Block number the reference graph was bootstrapped at. Used by the
    /// revm verifier to pin `RpcForkedState` to a specific block.
    block_number: u64,
    /// Block timestamp at the reference-graph block.
    block_timestamp: u64,
}

async fn bootstrap_state(
    pools: &[LoadedPool],
    provider: &impl Provider,
) -> Result<ScorerState> {
    let head = provider.get_block_number().await.context("get_block_number")?;
    // Pull the full block header for base fee + timestamp (revm verifier
    // needs both for accurate simulation).
    let head_block = provider
        .get_block(BlockId::Number(BlockNumberOrTag::Number(head)))
        .await
        .ok()
        .flatten();
    let base_fee_wei = head_block
        .as_ref()
        .and_then(|b| b.header.base_fee_per_gas)
        .map(u128::from)
        .unwrap_or(DEFAULT_BASE_FEE_WEI);
    let block_timestamp = head_block
        .as_ref()
        .map(|b| b.header.timestamp)
        .unwrap_or(0);

    let mut latest_states: HashMap<usize, PoolState> = HashMap::new();
    for (idx, pool) in pools.iter().enumerate() {
        match fetch_pool_state_at(provider, pool, head).await? {
            Some(state) => {
                latest_states.insert(idx, state);
            }
            None => {
                debug!(
                    pool = %pool.address,
                    "no state returned at head; skipping"
                );
            }
        }
    }

    let mut token_index = TokenIndex::new();
    let mut graph = PriceGraph::new(10);
    for (idx, pool) in pools.iter().enumerate() {
        let Some(state) = latest_states.get(&idx).copied() else {
            continue;
        };
        let t0 = token_index.get_or_insert(pool.token0);
        let t1 = token_index.get_or_insert(pool.token1);
        graph.resize(token_index.len());

        let rate_0to1 = match state {
            PoolState::V2 { r0, r1 } => {
                let r0f = u256_to_f64(r0);
                let r1f = u256_to_f64(r1);
                if r0f == 0.0 || r1f == 0.0 {
                    continue;
                }
                r1f / r0f
            }
            PoolState::V3 { sqrt_price_x96 } => {
                let s = u256_to_f64(sqrt_price_x96);
                if s == 0.0 {
                    continue;
                }
                let root = s / Q96;
                root * root
            }
        };
        if !rate_0to1.is_finite() || rate_0to1 <= 0.0 {
            continue;
        }
        let fee = (10_000 - pool.fee_bps) as f64 / 10_000.0;
        let pool_id = PoolId {
            address: pool.address,
            protocol: pool.protocol,
        };
        graph.add_edge(t0, t1, rate_0to1 * fee, pool_id, pool.address, pool.protocol, U256::ZERO);
        graph.add_edge(t1, t0, (1.0 / rate_0to1) * fee, pool_id, pool.address, pool.protocol, U256::ZERO);
    }

    Ok(ScorerState {
        graph,
        token_index,
        latest_states,
        base_fee_wei,
        block_number: head,
        block_timestamp,
    })
}

/// Rewrite a `wss://...` URL to `https://...` so the alloy HTTP provider
/// can use it. No-op for already-HTTP URLs.
fn rewrite_ws_to_http(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    }
}

fn start_metrics_server(addr: SocketAddr, registry: Registry) {
    tokio::spawn(async move {
        let make_svc = move || {
            let registry = registry.clone();
            async move {
                let encoder = TextEncoder::new();
                let mut buf = Vec::new();
                let _ = encoder.encode(&registry.gather(), &mut buf);
                buf
            }
        };
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, "metrics listener bind failed");
                return;
            }
        };
        info!("metrics server listening at {addr}");
        loop {
            match listener.accept().await {
                Ok((mut socket, _)) => {
                    let body = make_svc().await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    use tokio::io::AsyncWriteExt;
                    if let Err(e) = socket.write_all(response.as_bytes()).await {
                        debug!(error = %e, "metrics write header failed");
                        continue;
                    }
                    if let Err(e) = socket.write_all(&body).await {
                        debug!(error = %e, "metrics write body failed");
                        continue;
                    }
                }
                Err(e) => {
                    debug!(error = %e, "metrics accept failed");
                }
            }
        }
    });
}

// Silence the unused-but-imported warning for default-but-not-needed
// addresses pulled in via alloy::primitives::address. Removing the import
// would break the inlined helpers if they're ever expanded to include
// well-known mainnet token labels.
#[allow(dead_code)]
const _DUMMY_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_ws_to_http_handles_both_schemes() {
        assert_eq!(
            rewrite_ws_to_http("wss://eth.example/v2/key"),
            "https://eth.example/v2/key"
        );
        assert_eq!(
            rewrite_ws_to_http("ws://eth.example/v2/key"),
            "http://eth.example/v2/key"
        );
        assert_eq!(
            rewrite_ws_to_http("https://eth.example/v2/key"),
            "https://eth.example/v2/key"
        );
    }

    #[test]
    fn state_to_graph_reserves_v2_passes_through() {
        let s = PoolState::V2 {
            r0: U256::from(1_000_000u64),
            r1: U256::from(2_000_000u64),
        };
        let (r0, r1) = state_to_graph_reserves(&s);
        assert!((r0 - 1_000_000.0).abs() < 1.0);
        assert!((r1 - 2_000_000.0).abs() < 1.0);
    }

    #[test]
    fn state_to_graph_reserves_v3_uses_synthetic_pair() {
        // sqrtPriceX96 = 2^96 → rate_0to1 = 1.0; synthetic (1.0, 1.0).
        let s = PoolState::V3 {
            sqrt_price_x96: U256::from_be_slice(&{
                let mut buf = [0u8; 32];
                buf[31 - 12] = 1;
                buf
            }),
        };
        let (r0, r1) = state_to_graph_reserves(&s);
        assert_eq!(r0, 1.0);
        assert!(r1 > 0.0 && r1 < 2.0);
    }

    #[test]
    fn protocol_label_covers_supported_variants() {
        for (p, expected) in [
            (ProtocolType::UniswapV2, "uni_v2"),
            (ProtocolType::UniswapV3, "uni_v3"),
            (ProtocolType::SushiSwap, "sushi"),
            (ProtocolType::Curve, "curve"),
            (ProtocolType::BalancerV2, "balancer"),
            (ProtocolType::BancorV3, "bancor"),
        ] {
            assert_eq!(protocol_label(p), expected);
        }
    }

    #[test]
    fn no_path_outcome_carries_negative_net_when_gas_given() {
        let out = no_path_outcome(Some(50_000));
        assert_eq!(out.decision, DECISION_NO_PATH);
        assert_eq!(out.net_profit_wei, -50_000);
        assert_eq!(out.gas_estimate_wei, U256::from(50_000u64));
    }

    #[test]
    fn uniswap_v2_get_amount_out_matches_constant_product() {
        // 1 WETH in, 100 WETH / 200_000 USDC pool, 30 bps fee.
        // Exact math: amount_in_with_fee = 1e18 * 9970 = 9.97e21
        // numerator = 9.97e21 * 200e9 = 1.994e33
        // denominator = 100e18 * 10_000 + 9.97e21 ≈ 1.00997e24
        // out = 1.994e33 / 1.00997e24 ≈ 1.974e9 USDC (input is ~1% of pool
        // depth so slippage compounds with the fee). Range below brackets
        // the exact value while keeping wiggle room for unrelated changes
        // to the formula.
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 WETH (18 dec)
        let reserve_in = U256::from(100_000_000_000_000_000_000u128); // 100 WETH
        let reserve_out = U256::from(200_000_000_000u128); // 200_000 USDC (6 dec)
        let out = uniswap_v2_get_amount_out(amount_in, reserve_in, reserve_out, 30).unwrap();
        let out_u64 = out.try_into().unwrap_or(u64::MAX);
        assert!(
            (1_970_000_000..=1_980_000_000).contains(&out_u64),
            "expected ~1974 USDC, got {out_u64}"
        );
    }

    #[test]
    fn uniswap_v2_get_amount_out_rejects_zero_inputs() {
        let r = U256::from(1_000_000u64);
        assert!(uniswap_v2_get_amount_out(U256::ZERO, r, r, 30).is_none());
        assert!(uniswap_v2_get_amount_out(r, U256::ZERO, r, 30).is_none());
        assert!(uniswap_v2_get_amount_out(r, r, U256::ZERO, 30).is_none());
    }

    #[test]
    fn u256_to_i128_saturating_handles_full_range() {
        assert_eq!(u256_to_i128_saturating(U256::ZERO), 0);
        assert_eq!(u256_to_i128_saturating(U256::from(42u64)), 42);
        // i128::MAX fits exactly: high limb = i64::MAX, low limb = u64::MAX
        let max_i128_as_u256 = U256::from(i128::MAX as u128);
        assert_eq!(u256_to_i128_saturating(max_i128_as_u256), i128::MAX);
        // Anything beyond i128::MAX saturates rather than wrapping.
        let too_big = U256::from(1u128) << 127; // 2^127 — first value over i128::MAX
        assert_eq!(u256_to_i128_saturating(too_big), i128::MAX);
        // 2^192 lives in limb 3 — must saturate, not panic.
        let huge = U256::from(1u128) << 192;
        assert_eq!(u256_to_i128_saturating(huge), i128::MAX);
    }

    fn make_token_index() -> (TokenIndex, [usize; 3]) {
        let a = address!("AAaaAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAaaaa");
        let b = address!("BBbbBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBbbbb");
        let c = address!("CCccCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCcccc");
        let mut idx = TokenIndex::new();
        let ia = idx.get_or_insert(a);
        let ib = idx.get_or_insert(b);
        let ic = idx.get_or_insert(c);
        (idx, [ia, ib, ic])
    }

    fn loaded(addr_byte: u8, token0: Address, token1: Address) -> LoadedPool {
        // Construct a unique address by repeating addr_byte across all 20 bytes.
        let mut raw = [0u8; 20];
        raw.fill(addr_byte);
        LoadedPool {
            address: Address::from(raw),
            token0,
            token1,
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        }
    }

    #[test]
    fn verify_cycle_u256_returns_none_when_v3_hop_present() {
        // A two-hop cycle with a V3 hop in the middle must return None so
        // the caller falls back to the f64 optimiser verdict.
        let (token_index, [ta, tb, _tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let mut pool_v3 = loaded(0x33, a, b);
        pool_v3.protocol = ProtocolType::UniswapV3;
        let pools = vec![pool_v3];

        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        graph.add_edge(
            ta,
            tb,
            1.0,
            PoolId { address: pools[0].address, protocol: pools[0].protocol },
            pools[0].address,
            pools[0].protocol,
            U256::ZERO,
        );

        let mut states = HashMap::new();
        states.insert(0, PoolState::V3 { sqrt_price_x96: U256::from(1u64) });

        let cycle = DetectedCycle {
            path: vec![ta, tb],
            total_weight: 0.0,
        };
        assert!(
            verify_cycle_u256(&cycle, &graph, &token_index, &pools, &states, U256::from(1u64))
                .is_none()
        );
    }

    #[test]
    fn verify_cycle_u256_walks_balanced_triangle() {
        // Three V2 pools forming a balanced triangle. With balanced
        // reserves and 30bps fee on each hop, an input of 1e18 should
        // round-trip back to ~(1 - 3*0.003) * 1e18 ≈ 9.91e17 (lossy: the
        // arb is unprofitable, which is the correct expected behaviour
        // for a flat, no-edge triangle).
        let (token_index, [ta, tb, tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let c = *token_index.get_address(tc).unwrap();

        let pools = vec![
            loaded(0x11, a, b),
            loaded(0x22, b, c),
            loaded(0x33, a, c),
        ];

        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        // Three balanced edges at rate=1.0; only the U256 walk matters
        // for the verifier's behaviour, so we don't bother making the
        // weights realistic.
        for (i, (from, to)) in [(ta, tb), (tb, tc), (tc, ta)].iter().enumerate() {
            graph.add_edge(
                *from,
                *to,
                0.0,
                PoolId { address: pools[i].address, protocol: pools[i].protocol },
                pools[i].address,
                pools[i].protocol,
                U256::ZERO,
            );
        }

        let mut states = HashMap::new();
        // Balanced reserves: every pool 1e21 / 1e21 (no inter-pool edge).
        let r = U256::from(1_000_000_000_000_000_000_000u128);
        for i in 0..3 {
            states.insert(i, PoolState::V2 { r0: r, r1: r });
        }

        let cycle = DetectedCycle {
            path: vec![ta, tb, tc, ta],
            total_weight: 0.0,
        };
        let input = U256::from(1_000_000_000_000_000_000u128); // 1.0
        let out = verify_cycle_u256(&cycle, &graph, &token_index, &pools, &states, input).unwrap();
        assert!(out < input);
    }

    #[test]
    fn verify_cycle_u256_rejects_self_loop_with_shifted_reserves() {
        // A→B→A on a single V2 pool. Without per-hop reserve evolution,
        // the verifier returns gross_out >> input for large inputs
        // because hop 2 sees the pre-swap reserves and "regenerates" the
        // input. With evolution, gross_out < input for *every* input
        // (double 30 bps fee is always lossy on a self-loop, regardless
        // of input size). This is the exact bug that fabricated 80B ETH
        // ghost profit on the soak's DAI/USDC self-loop row.
        let (token_index, [ta, tb, _tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let pools = vec![loaded(0x55, a, b)];

        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        let pid = PoolId { address: pools[0].address, protocol: pools[0].protocol };
        graph.add_edge(ta, tb, 1.0, pid, pools[0].address, pools[0].protocol, U256::ZERO);
        graph.add_edge(tb, ta, 1.0, pid, pools[0].address, pools[0].protocol, U256::ZERO);

        // DAI/USDC-shaped reserves: 5M DAI (1e25 base units) / 5M USDC
        // (5e12 base units). Mainnet-scale where the f64 precision bias
        // would otherwise bite.
        let r_a = U256::from(5_000_000u128) * U256::from(10u128).pow(U256::from(18u64));
        let r_b = U256::from(5_000_000u128) * U256::from(10u128).pow(U256::from(6u64));
        let mut states = HashMap::new();
        states.insert(0, PoolState::V2 { r0: r_a, r1: r_b });

        let cycle = DetectedCycle { path: vec![ta, tb, ta], total_weight: 0.0 };

        // Sweep inputs across four orders of magnitude — small inputs,
        // pool-fraction inputs, and oversized inputs all must come back
        // strictly less than input (double fee + slippage compound).
        for &exp in &[16u32, 18, 21, 24] {
            let input = U256::from(10u128).pow(U256::from(exp));
            let out = verify_cycle_u256(&cycle, &graph, &token_index, &pools, &states, input)
                .expect("self-loop should resolve");
            assert!(
                out < input,
                "self-loop at input 10^{exp} returned {out} >= {input} — reserve evolution missing",
            );
        }
    }

    #[test]
    fn parse_db_protocol_maps_short_form() {
        assert_eq!(parse_db_protocol("uni_v2"), Some(ProtocolType::UniswapV2));
        assert_eq!(parse_db_protocol("uni_v3"), Some(ProtocolType::UniswapV3));
        assert_eq!(parse_db_protocol("sushi"), Some(ProtocolType::SushiSwap));
        // Long forms are config-only; load_predicted_pools should
        // reject them so we never accidentally route a config row
        // through the DB path.
        assert_eq!(parse_db_protocol("uniswap_v2"), None);
        assert_eq!(parse_db_protocol("sushiswap"), None);
        // Balancer / Curve / Bancor are valid engine protocols but the
        // scorer can't compute reserves for them yet — they MUST be
        // refused here so an unsupported pool doesn't sneak in with
        // wrong fee_bps + nonexistent state.
        assert_eq!(parse_db_protocol("balancer"), None);
        assert_eq!(parse_db_protocol("curve"), None);
        assert_eq!(parse_db_protocol("bancor"), None);
        assert_eq!(parse_db_protocol(""), None);
    }

    #[test]
    fn default_fee_bps_constants_match_spec() {
        // Treat as a behavioural contract: changing either default
        // changes graph weight for every DB-augmented pool, which
        // shifts cycle rankings. Force the change to come through code
        // review by surfacing here.
        assert_eq!(DEFAULT_V2_FEE_BPS, 30);
        assert_eq!(DEFAULT_V3_FEE_BPS, 5);
    }

    #[test]
    fn max_db_predicted_pools_is_bounded() {
        // Sanity floor: needs to be both positive and below the RPC
        // fan-out budget (one eth_call per pool at bootstrap; ~256 is
        // the production-tested ceiling). Surfaced as a behavioural
        // contract so retunes go through review.
        const _: () = {
            assert!(MAX_DB_PREDICTED_POOLS > 0);
            assert!(MAX_DB_PREDICTED_POOLS <= 1024);
        };
        assert_eq!(MAX_DB_PREDICTED_POOLS, 256);
    }

    #[test]
    fn absurdity_floor_is_set_at_one_eth() {
        // The constant gates "verifier inconclusive but f64 says huge"
        // → REVERTED. If anyone retunes it, this test reminds them to
        // re-read the comment block and re-run the soak.
        assert_eq!(MAX_PLAUSIBLE_F64_NET_WEI, 1_000_000_000_000_000_000i128);
    }

    // ── V3 verifier tests ─────────────────────────────────────────

    fn loaded_v3(addr_byte: u8, token0: Address, token1: Address) -> LoadedPool {
        let mut raw = [0u8; 20];
        raw.fill(addr_byte);
        LoadedPool {
            address: Address::from(raw),
            token0,
            token1,
            protocol: ProtocolType::UniswapV3,
            fee_bps: 5,
        }
    }

    fn loaded_curve(addr_byte: u8, token0: Address, token1: Address) -> LoadedPool {
        let mut raw = [0u8; 20];
        raw.fill(addr_byte);
        LoadedPool {
            address: Address::from(raw),
            token0,
            token1,
            protocol: ProtocolType::Curve,
            fee_bps: 4,
        }
    }

    #[test]
    fn is_v3_touching_cycle_v2_only_returns_false() {
        let (token_index, [ta, tb, tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let c = *token_index.get_address(tc).unwrap();
        let pools = vec![loaded(0x11, a, b), loaded(0x22, b, c), loaded(0x33, a, c)];
        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        for (i, &(from, to)) in [(ta, tb), (tb, tc), (tc, ta)].iter().enumerate() {
            graph.add_edge(
                from, to, 0.0,
                PoolId { address: pools[i].address, protocol: pools[i].protocol },
                pools[i].address, pools[i].protocol, U256::ZERO,
            );
        }
        let r = U256::from(1_000_000u64);
        let mut states = HashMap::new();
        for i in 0..3 { states.insert(i, PoolState::V2 { r0: r, r1: r }); }
        let cycle = DetectedCycle { path: vec![ta, tb, tc, ta], total_weight: 0.0 };
        assert!(!is_v3_touching_cycle(&cycle, &graph, &token_index, &pools, &states));
    }

    #[test]
    fn is_v3_touching_cycle_mixed_returns_true() {
        let (token_index, [ta, tb, _tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        // Pool 0 is V2, pool 1 is V3 — mixed cycle.
        let pools = vec![loaded(0x11, a, b), loaded_v3(0x22, a, b)];
        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        graph.add_edge(
            ta, tb, 0.0,
            PoolId { address: pools[0].address, protocol: pools[0].protocol },
            pools[0].address, pools[0].protocol, U256::ZERO,
        );
        graph.add_edge(
            tb, ta, 0.0,
            PoolId { address: pools[1].address, protocol: pools[1].protocol },
            pools[1].address, pools[1].protocol, U256::ZERO,
        );
        let mut states = HashMap::new();
        states.insert(0, PoolState::V2 { r0: U256::from(1u64), r1: U256::from(1u64) });
        states.insert(1, PoolState::V3 { sqrt_price_x96: U256::from(1u64) });
        let cycle = DetectedCycle { path: vec![ta, tb, ta], total_weight: 0.0 };
        assert!(is_v3_touching_cycle(&cycle, &graph, &token_index, &pools, &states));
    }

    #[test]
    fn is_v3_touching_cycle_v3_only_returns_true() {
        let (token_index, [ta, tb, _tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let pools = vec![loaded_v3(0x44, a, b)];
        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        let pid = PoolId { address: pools[0].address, protocol: pools[0].protocol };
        graph.add_edge(ta, tb, 0.0, pid, pools[0].address, pools[0].protocol, U256::ZERO);
        graph.add_edge(tb, ta, 0.0, pid, pools[0].address, pools[0].protocol, U256::ZERO);
        let mut states = HashMap::new();
        states.insert(0, PoolState::V3 { sqrt_price_x96: U256::from(1u64) });
        let cycle = DetectedCycle { path: vec![ta, tb, ta], total_weight: 0.0 };
        assert!(is_v3_touching_cycle(&cycle, &graph, &token_index, &pools, &states));
    }

    #[test]
    fn build_steps_returns_none_for_curve_hop() {
        let (token_index, [ta, tb, _tc]) = make_token_index();
        let a = *token_index.get_address(ta).unwrap();
        let b = *token_index.get_address(tb).unwrap();
        let pools = vec![loaded_curve(0x77, a, b)];
        let mut graph = PriceGraph::new(token_index.len());
        graph.resize(token_index.len());
        let pid = PoolId { address: pools[0].address, protocol: pools[0].protocol };
        graph.add_edge(ta, tb, 0.0, pid, pools[0].address, pools[0].protocol, U256::ZERO);
        let mut states = HashMap::new();
        states.insert(0, PoolState::V2 { r0: U256::from(1_000_000u64), r1: U256::from(1_000_000u64) });
        let cycle = DetectedCycle { path: vec![ta, tb], total_weight: 0.0 };
        let executor_addr = address!("1111111111111111111111111111111111111111");
        assert!(build_steps_from_cycle_sync(
            &cycle, &graph, &token_index, &pools, &states, executor_addr, U256::from(1_000u64),
        ).is_none());
    }

    #[test]
    fn revm_verdict_decision_mapping_reverted() {
        let rv = RevmVerdict { gross_profit_wei: U256::ZERO, gas_used: 100_000, reverted: true };
        let (net, realised, dec) = revm_verdict_to_decision(rv, 50_000);
        assert_eq!(dec, DECISION_REVERTED);
        assert!(net < 0);
        assert_eq!(realised, 0);
    }

    #[test]
    fn revm_verdict_decision_mapping_profitable() {
        let rv = RevmVerdict {
            gross_profit_wei: U256::from(200_000u64),
            gas_used: 100_000,
            reverted: false,
        };
        let (net, _realised, dec) = revm_verdict_to_decision(rv, 50_000);
        assert_eq!(dec, DECISION_PROFITABLE);
        assert!(net > 0);
    }

    #[test]
    fn revm_verdict_decision_mapping_unprofitable() {
        let rv = RevmVerdict {
            gross_profit_wei: U256::from(10_000u64),
            gas_used: 100_000,
            reverted: false,
        };
        let (net, _realised, dec) = revm_verdict_to_decision(rv, 50_000);
        assert_eq!(dec, DECISION_UNPROFITABLE);
        assert!(net <= 0);
    }

    #[test]
    fn f64_fallback_verdict_above_floor_reverted() {
        let big_net = MAX_PLAUSIBLE_F64_NET_WEI + 1;
        let (_net, _realised, dec) = f64_fallback_verdict(big_net, 50_000);
        assert_eq!(dec, DECISION_REVERTED);
    }

    #[test]
    fn f64_fallback_verdict_below_floor_profitable() {
        let small_net = 1_000_000i128;
        let (_net, _realised, dec) = f64_fallback_verdict(small_net, 50_000);
        assert_eq!(dec, DECISION_PROFITABLE);
    }

    #[test]
    fn f64_fallback_verdict_negative_unprofitable() {
        let neg = -500_000i128;
        let (net, _realised, dec) = f64_fallback_verdict(neg, 50_000);
        assert_eq!(dec, DECISION_UNPROFITABLE);
        assert!(net < 0);
    }

    #[test]
    fn balance_slot_for_known_tokens() {
        assert_eq!(balance_slot_for_token(WETH_ADDR), Some(U256::from(3u64)));
        assert_eq!(balance_slot_for_token(USDC_ADDR), Some(U256::from(9u64)));
        assert_eq!(balance_slot_for_token(DAI_ADDR), Some(U256::from(2u64)));
        assert_eq!(balance_slot_for_token(USDT_ADDR), Some(U256::from(2u64)));
        // Unknown token → None.
        assert_eq!(
            balance_slot_for_token(address!("0000000000000000000000000000000000000042")),
            None,
        );
    }
}
