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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use prometheus::{Encoder, Registry, TextEncoder};
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use aether_common::types::{PoolId, ProtocolType};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas as gas_model;
use aether_detector::opportunity::DetectedCycle;
use aether_detector::optimizer::ternary_search_optimal_input;
use aether_grpc_server::profitability_writer::{
    profit_writer_from_env, NewProfitabilityScore, PgProfitabilityWriter, ProfitabilitySink,
    ProfitabilityWriterMetrics, UnscoredConfirmedPrediction, DECISION_NO_PATH,
    DECISION_PROFITABLE, DECISION_UNPROFITABLE,
};
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

    info!("Loading pool config from {}", args.pools_config.display());
    let pools = load_pools(&args.pools_config)?;
    info!(pool_count = pools.len(), "Pools loaded");

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

    // Convert WS RPC URL to HTTPS for the alloy HTTP provider. The fork
    // path in aether-replay does the same rewrite; replicated here so
    // the scorer accepts the same env var as the engine.
    let http_url = rewrite_ws_to_http(&rpc_url);
    let provider = ProviderBuilder::new()
        .connect_http(http_url.parse().context("parse RPC URL")?);

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
                ).await {
                    warn!(error = %e, "score batch failed");
                }
            }
            _ = refresh_ticker.tick() => {
                match bootstrap_state(&pools, &provider).await {
                    Ok(fresh) => {
                        info!(base_fee_gwei = fresh.base_fee_wei as f64 / 1e9, "reference graph refreshed");
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
async fn score_batch(
    read_pool: &sqlx::PgPool,
    provider: &impl Provider,
    pools: &[LoadedPool],
    state: &ScorerState,
    sink: &dyn ProfitabilitySink,
    git_sha: Option<&str>,
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
        match score_one(provider, pools, state, &pred).await {
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

    let net = optimisation.net_profit_wei;
    let gas_wei = optimisation.gas_cost_wei;
    // Realised gross profit = net + gas (we subtracted gas inside the
    // optimiser to score the cycle, so add it back to expose the gross
    // signal separately).
    let realized_wei_i128 = net.saturating_add(gas_wei as i128).max(0);
    let realized_wei = U256::from(realized_wei_i128 as u128);
    let gas_estimate_wei = U256::from(gas_wei);
    let decision = if net > 0 {
        DECISION_PROFITABLE
    } else {
        DECISION_UNPROFITABLE
    };

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

    let (_optimal_input_wei, net_profit_wei) = if min_input < max_input {
        ternary_search_optimal_input(min_input, max_input, 80, profit_fn)
    } else {
        let p = profit_fn(min_input);
        (min_input, p)
    };

    Some(OptimiserSuccess {
        net_profit_wei,
        gas_cost_wei,
    })
}

fn gas_estimate_for_protocols(protocols: &[ProtocolType], base_fee_wei: u128) -> u128 {
    let ticks = vec![0u32; protocols.len()];
    let units = gas_model::estimate_total_gas(protocols, &ticks);
    gas_model::gas_cost_wei(units, base_fee_wei as f64 / 1e9)
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
}

async fn bootstrap_state(
    pools: &[LoadedPool],
    provider: &impl Provider,
) -> Result<ScorerState> {
    let head = provider.get_block_number().await.context("get_block_number")?;
    // Pull latest base fee for the gas model; default if it's missing
    // (e.g. archive-only provider that doesn't fill base_fee_per_gas).
    let base_fee_wei = provider
        .get_block(BlockId::Number(BlockNumberOrTag::Number(head)))
        .await
        .ok()
        .flatten()
        .and_then(|b| b.header.base_fee_per_gas)
        .map(u128::from)
        .unwrap_or(DEFAULT_BASE_FEE_WEI);

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
}
