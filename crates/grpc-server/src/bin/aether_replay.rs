//! Historical block replay — Phase 1.
//!
//! One-shot binary: given a past block number, fetch the pre-block state from
//! an archive RPC (Alchemy), build the same price graph production uses, run
//! the same detector, and print the opportunities it would have found.
//!
//! Run with:
//!     ETH_RPC_URL=... cargo run --release -p aether-grpc-server \
//!         --bin aether-replay -- --block 19500123
//!
//! Uses the exact same `aether-detector` + `aether-state` + `aether-pools`
//! code paths as the live gRPC server.

use std::path::PathBuf;
use std::time::Instant;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use aether_common::types::{PoolId, ProtocolType};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas as gas_model;
use aether_detector::opportunity::DetectedCycle;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
}

/// 2^96 as f64, used to convert UniswapV3 `sqrtPriceX96` into a floating-point price.
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

/// Per-pool state fetched from the chain. V3 carries `sqrtPriceX96`; V2/Sushi
/// carry `(reserve0, reserve1)`.
#[derive(Clone, Copy)]
enum PoolState {
    V2 { r0: U256, r1: U256 },
    V3 { sqrt_price_x96: U256 },
}

/// Well-known mainnet token labels for readable output.
fn token_label(addr: &Address) -> String {
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
    match *addr {
        WETH => "WETH".into(),
        USDC => "USDC".into(),
        USDT => "USDT".into(),
        DAI => "DAI".into(),
        WBTC => "WBTC".into(),
        _ => format!("{:#x}", addr).chars().take(10).collect::<String>() + "…",
    }
}

#[derive(Parser)]
#[command(
    name = "aether-replay",
    about = "Replay one historical block through Aether's detector. Prints detected cycles."
)]
struct Args {
    /// Target block number. Reserves are fetched at `block - 1` (state before
    /// the block landed).
    #[arg(long)]
    block: u64,

    /// Path to the pool registry TOML. When omitted, uses a built-in 7-pool
    /// default set (WETH/USDC/USDT/DAI across UniV2 + Sushi).
    #[arg(long)]
    pools: Option<PathBuf>,

    /// Archive RPC URL. Falls back to the ETH_RPC_URL env var.
    #[arg(long)]
    rpc_url: Option<String>,

    /// Maximum hops per arbitrage cycle.
    #[arg(long, default_value_t = 4)]
    max_hops: usize,

    /// Detector time budget (microseconds).
    #[arg(long, default_value_t = 5_000_000)]
    max_time_us: u64,

    /// Print the top-N cycles.
    #[arg(long, default_value_t = 5)]
    top: usize,

    /// Intra-block replay mode. Spawns Anvil forked at `block - 1`,
    /// replays each tx of the target block via impersonation, and runs the
    /// detector between every tx to catch intra-block arbitrage windows.
    #[arg(long, default_value_t = false)]
    full_block: bool,

    /// Port for the Anvil fork (intra-block mode only).
    #[arg(long, default_value_t = 8546)]
    anvil_port: u16,

    /// Write per-detection-event rows to this CSV path (intra-block mode only).
    /// Columns: block, tx_index, tx_hash, cycles, top_profit_factor, hops,
    /// path, est_gas, base_fee_gwei, gas_cost_eth, sim_net_profit_eth.
    #[arg(long)]
    csv: Option<PathBuf>,

    /// Input amount (in WETH, 18 decimals) used to compute simulated net
    /// profit. `sim_net_profit = profit_factor * input - gas_cost`.
    #[arg(long, default_value_t = 1.0)]
    sim_input_weth: f64,
}

#[derive(serde::Deserialize)]
struct PoolEntry {
    protocol: String,
    address: String,
    token0: String,
    token1: String,
    fee_bps: u32,
}

#[derive(serde::Deserialize)]
struct PoolsConfig {
    #[serde(default)]
    pools: Vec<PoolEntry>,
}

struct LoadedPool {
    address: Address,
    token0: Address,
    token1: Address,
    protocol: ProtocolType,
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

/// Built-in 7-pool set matching the integration tests. Enough token diversity
/// (WETH/USDC/USDT/DAI) to produce real triangle-arb cycles when reserves are
/// fetched from any recent mainnet block.
fn default_pool_set() -> Vec<LoadedPool> {
    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
    const AAVE: Address = address!("7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9");

    let mk = |addr: Address, t0: Address, t1: Address, proto: ProtocolType, fee: u32| LoadedPool {
        address: addr,
        token0: t0,
        token1: t1,
        protocol: proto,
        fee_bps: fee,
    };
    vec![
        // ── V2 / Sushi ────────────────────────────────────────────────
        mk(address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"), USDC, WETH, ProtocolType::UniswapV2, 30),
        mk(address!("0d4a11d5EEaaC28EC3F61d100daF4d40471f1852"), WETH, USDT, ProtocolType::UniswapV2, 30),
        mk(address!("397FF1542f962076d0BFE58eA045FfA2d347ACa0"), USDC, WETH, ProtocolType::SushiSwap, 30),
        mk(address!("06da0fd433C1A5d7a4faa01111c044910A184553"), WETH, USDT, ProtocolType::SushiSwap, 30),
        // WETH-DAI: on-chain token0 = DAI (lower address), token1 = WETH.
        mk(address!("A478c2975Ab1Ea89e8196811F51A7B7Ade33eB11"), DAI, WETH, ProtocolType::UniswapV2, 30),
        mk(address!("C3D03e4F041Fd4cD388c549Ee2A29a9E5075882f"), DAI, WETH, ProtocolType::SushiSwap, 30),
        mk(address!("3041CbD36888bECc7bbCBc0045E3B1f144466f5f"), USDC, USDT, ProtocolType::UniswapV2, 30),
        // ── V3 majors (fee in bps; UniV3 fee tiers: 5, 30, 100 bps) ───
        mk(address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"), USDC, WETH, ProtocolType::UniswapV3, 5),
        mk(address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"), USDC, WETH, ProtocolType::UniswapV3, 30),
        mk(address!("11b815efB8f581194ae79006d24E0d814B7697F6"), WETH, USDT, ProtocolType::UniswapV3, 5),
        mk(address!("4e68Ccd3E89f51C3074ca5072bbAC773960dFa36"), WETH, USDT, ProtocolType::UniswapV3, 30),
        mk(address!("C2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8"), DAI, WETH, ProtocolType::UniswapV3, 30),
        mk(address!("60594a405d53811d3BC4766596EFD80fd545A270"), DAI, WETH, ProtocolType::UniswapV3, 5),
        mk(address!("3416cF6C708Da44DB2624D63ea0AAef7113527C6"), USDC, USDT, ProtocolType::UniswapV3, 1),
        // ── WBTC pairs (8 decimals — cross-decimal with WETH) ─────────
        mk(address!("Bb2b8038a1640196FbE3e38816F3e67Cba72D940"), WBTC, WETH, ProtocolType::UniswapV2, 30),
        mk(address!("CEfF51756c56CeFFCA006cD410B03FFC46dd3a58"), WBTC, WETH, ProtocolType::SushiSwap, 30),
        mk(address!("4585FE77225b41b697C938B018E2Ac67Ac5a20c0"), WBTC, WETH, ProtocolType::UniswapV3, 5),
        mk(address!("CBCdF9626bC03E24f779434178A73a0B4bad62eD"), WBTC, WETH, ProtocolType::UniswapV3, 30),
        // ── AAVE pairs (18 decimals, token0 = AAVE by address order) ──
        mk(address!("DFC14d2Af169B0D36C4EFF567Ada9b2E0CAE044f"), AAVE, WETH, ProtocolType::UniswapV2, 30),
        mk(address!("D75EA151a61d06868E31F8988D28DFE5E9df57B4"), AAVE, WETH, ProtocolType::SushiSwap, 30),
        mk(address!("5aB53EE1d50eeF2C1DD3d5402789cd27bB52c1bB"), AAVE, WETH, ProtocolType::UniswapV3, 30),
    ]
}

fn load_pools(path: &PathBuf) -> Result<Vec<LoadedPool>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read pool config {}", path.display()))?;
    let cfg: PoolsConfig = toml::from_str(&raw).context("parse pool config")?;

    let mut out = Vec::new();
    for entry in cfg.pools {
        // Phase 1 supports V2/Sushi (via `getReserves`) and UniswapV3 (via
        // `slot0().sqrtPriceX96`). Curve / Balancer / Bancor are deferred.
        let Some(protocol) = parse_protocol(&entry.protocol) else {
            continue;
        };
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
) -> Option<PoolState> {
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block));
    match pool.protocol {
        ProtocolType::UniswapV2 | ProtocolType::SushiSwap => {
            let calldata = getReservesCall {}.abi_encode();
            let tx = TransactionRequest::default()
                .to(pool.address)
                .input(calldata.into());
            match provider.call(tx).block(block_id).await {
                Ok(out) if out.len() >= 64 => Some(PoolState::V2 {
                    r0: U256::from_be_slice(&out[0..32]),
                    r1: U256::from_be_slice(&out[32..64]),
                }),
                _ => None,
            }
        }
        ProtocolType::UniswapV3 => {
            let calldata = slot0Call {}.abi_encode();
            let tx = TransactionRequest::default()
                .to(pool.address)
                .input(calldata.into());
            match provider.call(tx).block(block_id).await {
                // slot0 returns 7 values; only the first 32-byte word (sqrtPriceX96) is used.
                Ok(out) if out.len() >= 32 => Some(PoolState::V3 {
                    sqrt_price_x96: U256::from_be_slice(&out[0..32]),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Truncate a U256 reserve to f64 for graph weight computation.
/// Loss of precision is acceptable: we only care about the ratio.
fn u256_to_f64(v: U256) -> f64 {
    let limbs = v.as_limbs();
    let mut acc = 0.0f64;
    for (i, &limb) in limbs.iter().enumerate() {
        acc += (limb as f64) * (2f64).powi((64 * i) as i32);
    }
    acc
}

fn build_graph(
    pools: &[LoadedPool],
    states: &[(usize, PoolState)],
) -> (PriceGraph, TokenIndex) {
    let mut token_index = TokenIndex::new();
    let mut graph = PriceGraph::new(10);

    for &(pool_idx, state) in states {
        let p = &pools[pool_idx];
        let t0 = token_index.get_or_insert(p.token0);
        let t1 = token_index.get_or_insert(p.token1);
        graph.resize(token_index.len());

        // Raw atomic rate token0 -> token1 (before fee).
        // For V2: rate_0to1 = r1 / r0.
        // For V3: sqrtPriceX96 = sqrt(token1/token0) * 2^96, so rate_0to1 = (s/2^96)^2.
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

        let fee = (10_000 - p.fee_bps) as f64 / 10_000.0;
        let pool_id = PoolId {
            address: p.address,
            protocol: p.protocol,
        };

        // Both directions. Weight = -ln(rate * fee).
        graph.add_edge(
            t0,
            t1,
            rate_0to1 * fee,
            pool_id,
            p.address,
            p.protocol,
            U256::ZERO,
        );
        graph.add_edge(
            t1,
            t0,
            (1.0 / rate_0to1) * fee,
            pool_id,
            p.address,
            p.protocol,
            U256::ZERO,
        );
    }

    (graph, token_index)
}

fn print_cycles(cycles: &[DetectedCycle], token_index: &TokenIndex, top: usize) {
    let mut ranked: Vec<&DetectedCycle> = cycles.iter().filter(|c| c.is_profitable()).collect();
    ranked.sort_by(|a, b| a.total_weight.partial_cmp(&b.total_weight).unwrap());

    let shown = ranked.iter().take(top);
    for (i, cycle) in shown.enumerate() {
        let path_labels: Vec<String> = cycle
            .path
            .iter()
            .filter_map(|&v| token_index.get_address(v).map(token_label))
            .collect();
        println!(
            "  #{:<2} hops={}  profit_factor={:.4}%  path: {}",
            i + 1,
            cycle.num_hops(),
            cycle.profit_factor() * 100.0,
            path_labels.join(" -> ")
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    // Default to warn so alloy's DEBUG chatter stays out of the replay output.
    // Callers who actually want logs can pass RUST_LOG explicitly on the CLI.
    let default_filter = "warn";
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("AETHER_REPLAY_LOG")
                .ok()
                .and_then(|v| EnvFilter::try_new(v).ok())
                .unwrap_or_else(|| EnvFilter::new(default_filter)),
        )
        .init();

    let args = Args::parse();

    let rpc_url = args
        .rpc_url
        .clone()
        .or_else(|| std::env::var("ETH_RPC_URL").ok())
        .context("--rpc-url not set and ETH_RPC_URL env var missing")?;

    if args.full_block {
        return run_full_block_replay(&args, &rpc_url).await;
    }

    println!("== Aether Replay ==");
    println!("  Target block:    {}", args.block);
    println!("  State at block:  {} (pre-state)", args.block - 1);
    println!("  Max hops:        {}", args.max_hops);

    let t_total = Instant::now();

    // Load pools — TOML if --pools supplied, otherwise the built-in 7-pool set.
    let pools = match &args.pools {
        Some(path) => {
            println!("  Pool config:     {}", path.display());
            load_pools(path)?
        }
        None => {
            println!("  Pool config:     (built-in 7-pool default set)");
            default_pool_set()
        }
    };
    let v2_count = pools
        .iter()
        .filter(|p| matches!(p.protocol, ProtocolType::UniswapV2 | ProtocolType::SushiSwap))
        .count();
    let v3_count = pools
        .iter()
        .filter(|p| matches!(p.protocol, ProtocolType::UniswapV3))
        .count();
    println!(
        "  Pools loaded:    {} total ({} V2/Sushi, {} V3)",
        pools.len(),
        v2_count,
        v3_count
    );

    // Connect to Alchemy / archive RPC.
    let parsed: url::Url = rpc_url.parse().context("parse RPC URL")?;
    let provider = ProviderBuilder::new().connect_http(parsed);

    let tip = provider
        .get_block_number()
        .await
        .context("get_block_number")?;
    if args.block > tip {
        anyhow::bail!("requested block {} > chain tip {}", args.block, tip);
    }

    // Fetch pool state (V2 reserves or V3 sqrtPriceX96) at the pre-state block.
    let t_fetch = Instant::now();
    let pre_state_block = args.block - 1;
    let mut states = Vec::with_capacity(pools.len());
    for (i, pool) in pools.iter().enumerate() {
        if let Some(state) = fetch_pool_state_at(&provider, pool, pre_state_block).await {
            states.push((i, state));
        }
    }
    let fetch_ms = t_fetch.elapsed().as_millis();
    println!(
        "  State fetched:   {}/{} ({} ms)",
        states.len(),
        pools.len(),
        fetch_ms
    );

    if states.len() < 3 {
        anyhow::bail!(
            "too few pools with state at block {}: {}",
            pre_state_block,
            states.len()
        );
    }

    // Build graph.
    let (graph, token_index) = build_graph(&pools, &states);
    println!(
        "  Graph:           {} tokens, {} edges",
        token_index.len(),
        graph.num_edges()
    );

    // Run detector — same code path as production.
    let t_detect = Instant::now();
    let detector = BellmanFord::new(args.max_hops, args.max_time_us);
    let cycles = detector.detect_negative_cycles(&graph);
    let detect_ms = t_detect.elapsed().as_millis();

    let profitable = cycles.iter().filter(|c| c.is_profitable()).count();
    println!(
        "  Detection:       {} cycles ({} profitable) in {} ms",
        cycles.len(),
        profitable,
        detect_ms
    );

    if profitable > 0 {
        println!("\n  Top opportunities:");
        print_cycles(&cycles, &token_index, args.top);
    }

    println!("\n  Total time:      {} ms", t_total.elapsed().as_millis());

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Phase 1b — intra-block replay via Anvil
// ────────────────────────────────────────────────────────────────────────────
//
// Anvil is used here purely as a **block replayer** — we spawn it forked at
// `block - 1`, feed each historical tx of the target block through via
// impersonation, and query pool state between every tx to see what the
// detector would have found at every intermediate point.
//
// Aether's candidate arb tx (inserted at each detection point) is still
// simulated via `aether-simulator::EvmSimulator` (revm) in the production path
// — Anvil never simulates Aether's tx. This keeps the production simulator
// honest while giving us cheap mid-block state computation.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use alloy::rpc::types::BlockTransactionsKind;

struct AnvilHandle {
    child: Child,
    url: String,
}

impl Drop for AnvilHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_anvil(fork_url: &str, fork_block: u64, port: u16) -> Result<AnvilHandle> {
    let child = Command::new("anvil")
        .arg("--fork-url")
        .arg(fork_url)
        .arg("--fork-block-number")
        .arg(fork_block.to_string())
        .arg("--port")
        .arg(port.to_string())
        .arg("--auto-impersonate")
        .arg("--chain-id")
        .arg("1")
        .arg("--silent")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn anvil — is foundry installed?")?;

    let url = format!("http://127.0.0.1:{port}");
    Ok(AnvilHandle { child, url })
}

async fn wait_for_anvil(url: &str, timeout: Duration) -> Result<()> {
    let parsed: url::Url = url.parse()?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        let provider = ProviderBuilder::new().connect_http(parsed.clone());
        if provider.get_block_number().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("anvil did not become ready at {url} within {timeout:?}")
}

/// Query reserves / sqrtPriceX96 for a single pool at "latest" against an RPC.
async fn fetch_one_state_latest(
    provider: &impl Provider,
    pool: &LoadedPool,
) -> Option<PoolState> {
    let block_id = BlockId::Number(BlockNumberOrTag::Latest);
    match pool.protocol {
        ProtocolType::UniswapV2 | ProtocolType::SushiSwap => {
            let calldata = getReservesCall {}.abi_encode();
            let tx = TransactionRequest::default()
                .to(pool.address)
                .input(calldata.into());
            let output = provider.call(tx).block(block_id).await.ok()?;
            if output.len() >= 64 {
                Some(PoolState::V2 {
                    r0: U256::from_be_slice(&output[0..32]),
                    r1: U256::from_be_slice(&output[32..64]),
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
            let output = provider.call(tx).block(block_id).await.ok()?;
            if output.len() >= 32 {
                Some(PoolState::V3 {
                    sqrt_price_x96: U256::from_be_slice(&output[0..32]),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Initial state fetch for all pools (called once before the tx loop).
async fn fetch_all_states_latest(
    provider: &impl Provider,
    pools: &[LoadedPool],
) -> Vec<(usize, PoolState)> {
    let mut out = Vec::with_capacity(pools.len());
    for (i, pool) in pools.iter().enumerate() {
        if let Some(state) = fetch_one_state_latest(provider, pool).await {
            out.push((i, state));
        }
    }
    out
}

/// Build a `TransactionRequest` from a historical `Transaction` for replay via
/// impersonation on Anvil. Drops the signature — `--auto-impersonate` accepts
/// any `from` without a valid signature.
fn build_impersonation_request(
    tx: &alloy::rpc::types::Transaction,
) -> Option<TransactionRequest> {
    use alloy::consensus::{Transaction as TxTrait, Typed2718};

    let inner = tx.inner.inner();
    let ty = inner.ty();

    // Skip EIP-4844 blob txs — Anvil's impersonation path doesn't accept
    // blob-carrying txs cleanly and the execution-layer effect (non-blob
    // fields) still modifies state via the rollup blob gas accounting.
    // For mainnet-DEX replay these are rare; we log and skip.
    if ty == 3 {
        return None;
    }

    let mut req = TransactionRequest::default()
        .from(tx.inner.signer())
        .value(inner.value())
        .input(inner.input().clone().into())
        .nonce(inner.nonce())
        .gas_limit(inner.gas_limit());

    if let Some(to) = inner.to() {
        req = req.to(to);
    }

    // Gas pricing: EIP-1559 vs legacy.
    if let Some(max_fee) = inner.max_fee_per_gas().into() {
        req = req.max_fee_per_gas(max_fee);
    }
    if let Some(tip) = inner.max_priority_fee_per_gas() {
        req = req.max_priority_fee_per_gas(tip);
    }
    if ty == 0 {
        // Legacy: use gas_price.
        req = req.gas_price(inner.gas_price().unwrap_or(0));
    }

    // Access list (EIP-2930+).
    if let Some(access_list) = inner.access_list() {
        req = req.access_list(access_list.clone());
    }

    Some(req)
}

/// Walk a detected cycle and produce (protocols_per_hop, tick_counts_per_hop)
/// by picking the best edge (most negative weight) for each hop. Feeds directly
/// into `aether-detector::gas::estimate_total_gas` so gas estimates match
/// exactly what the production ranker computes.
fn protocols_along_cycle(cycle: &DetectedCycle, graph: &PriceGraph) -> Vec<ProtocolType> {
    let mut protocols = Vec::with_capacity(cycle.path.len().saturating_sub(1));
    for pair in cycle.path.windows(2) {
        let [from, to] = [pair[0], pair[1]];
        let best = graph
            .edges_from(from)
            .iter()
            .filter(|e| e.to == to)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(e) = best {
            protocols.push(e.protocol);
        }
    }
    protocols
}

/// Per-opportunity accounting. Same profit/gas math the production ranker runs.
struct OppEstimate {
    profit_factor: f64,
    gas_units: u64,
    base_fee_gwei: f64,
    gas_cost_eth: f64,
    gross_profit_eth: f64,
    net_profit_eth: f64,
}

fn estimate_opp(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    base_fee_wei: u128,
    input_eth: f64,
) -> OppEstimate {
    let protocols = protocols_along_cycle(cycle, graph);
    // Conservative: 0 tick crossings per V3 hop. Real crossings would add
    // UNIV3_PER_TICK_GAS each; pessimistic gas → net_profit_eth is a lower
    // bound on profitability.
    let ticks = vec![0u32; protocols.len()];
    let gas_units = gas_model::estimate_total_gas(&protocols, &ticks);
    let base_fee_gwei = base_fee_wei as f64 / 1e9;
    let gas_cost_wei = gas_model::gas_cost_wei(gas_units, base_fee_gwei);
    let gas_cost_eth = gas_cost_wei as f64 / 1e18;
    let profit_factor = cycle.profit_factor();
    let gross_profit_eth = profit_factor * input_eth;
    OppEstimate {
        profit_factor,
        gas_units,
        base_fee_gwei,
        gas_cost_eth,
        gross_profit_eth,
        net_profit_eth: gross_profit_eth - gas_cost_eth,
    }
}

/// Intra-block replay entry point.
async fn run_full_block_replay(args: &Args, rpc_url: &str) -> Result<()> {
    let fork_block = args.block.saturating_sub(1);

    println!("== Aether Full-Block Replay ==");
    println!("  Target block:    {}", args.block);
    println!("  Anvil fork at:   {}  (state pre-block)", fork_block);

    // Load pools.
    let pools = match &args.pools {
        Some(path) => load_pools(path)?,
        None => default_pool_set(),
    };
    let v2 = pools
        .iter()
        .filter(|p| matches!(p.protocol, ProtocolType::UniswapV2 | ProtocolType::SushiSwap))
        .count();
    let v3 = pools
        .iter()
        .filter(|p| matches!(p.protocol, ProtocolType::UniswapV3))
        .count();
    println!(
        "  Pools tracked:   {} ({} V2/Sushi, {} V3)",
        pools.len(),
        v2,
        v3
    );

    // Fetch block N with full txs from the archive RPC (Alchemy).
    let archive_url: url::Url = rpc_url.parse().context("parse RPC URL")?;
    let archive = ProviderBuilder::new().connect_http(archive_url);
    let block = archive
        .get_block_by_number(BlockNumberOrTag::Number(args.block))
        .kind(BlockTransactionsKind::Full)
        .await
        .context("eth_getBlockByNumber")?
        .ok_or_else(|| anyhow::anyhow!("block {} not found", args.block))?;

    let txs = match block.transactions {
        alloy::rpc::types::BlockTransactions::Full(ref v) => v.clone(),
        _ => anyhow::bail!("expected full-tx block, got hashes only"),
    };
    let base_fee_wei = block.header.base_fee_per_gas.unwrap_or(30_000_000_000) as u128;
    println!("  Block txs:       {}", txs.len());
    println!("  Block base fee:  {:.2} gwei", base_fee_wei as f64 / 1e9);
    println!("  Sim input:       {:.3} WETH", args.sim_input_weth);

    // Spawn Anvil fork.
    println!("\n  Spawning Anvil at port {} ...", args.anvil_port);
    let anvil = spawn_anvil(rpc_url, fork_block, args.anvil_port)?;
    wait_for_anvil(&anvil.url, Duration::from_secs(60)).await?;
    let anvil_url: url::Url = anvil.url.parse()?;
    let anvil_provider = ProviderBuilder::new().connect_http(anvil_url);
    println!("  Anvil ready.");

    // Establish initial pool state on Anvil (= state at end of block-1).
    let initial_states = fetch_all_states_latest(&anvil_provider, &pools).await;
    println!(
        "  Initial state:   {}/{} pools populated",
        initial_states.len(),
        pools.len()
    );

    // Maintain a running `pool_idx -> PoolState` map. Only refresh entries
    // for pools touched by each tx; avoids re-querying all 21 pools every tx.
    let mut running_states: std::collections::HashMap<usize, PoolState> =
        initial_states.into_iter().collect();

    // Per-tx replay + detection.
    let mut opp_events: Vec<(u64, usize)> = Vec::new(); // (tx_index, profitable_cycles)
    let mut total_net_profit_eth = 0.0f64;
    let mut skipped = 0usize;
    let mut reverted = 0usize;
    let detector = BellmanFord::new(args.max_hops, args.max_time_us);

    // Optional CSV writer.
    let mut csv_writer: Option<std::io::BufWriter<std::fs::File>> = match &args.csv {
        Some(path) => {
            let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
            use std::io::Write;
            writeln!(
                f,
                "block,tx_index,tx_hash,cycles,top_profit_factor,hops,path,est_gas,base_fee_gwei,gas_cost_eth,sim_gross_profit_eth,sim_net_profit_eth"
            )?;
            Some(f)
        }
        None => None,
    };

    let t_replay = Instant::now();
    for (i, tx) in txs.iter().enumerate() {
        let Some(req) = build_impersonation_request(tx) else {
            skipped += 1;
            continue;
        };

        // Impersonate-send. `--auto-impersonate` handles the sig bypass.
        let pending = match anvil_provider.send_transaction(req).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(tx_index = i, error = %e, "send_transaction failed");
                skipped += 1;
                continue;
            }
        };
        let receipt = match pending.get_receipt().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(tx_index = i, error = %e, "get_receipt failed");
                continue;
            }
        };
        if !receipt.status() {
            reverted += 1;
        }

        // Re-read state for pools touched by this tx (via receipt logs).
        let touched: std::collections::HashSet<Address> =
            receipt.logs().iter().map(|l| l.address()).collect();
        let tracked_touched: Vec<usize> = pools
            .iter()
            .enumerate()
            .filter(|(_, p)| touched.contains(&p.address))
            .map(|(i, _)| i)
            .collect();

        if tracked_touched.is_empty() {
            continue;
        }

        // Refresh only the touched pools; keep the rest of the running state.
        for &pool_idx in &tracked_touched {
            if let Some(state) = fetch_one_state_latest(&anvil_provider, &pools[pool_idx]).await {
                running_states.insert(pool_idx, state);
            }
        }

        let states: Vec<(usize, PoolState)> =
            running_states.iter().map(|(&i, &s)| (i, s)).collect();

        let (graph, token_index) = build_graph(&pools, &states);
        let cycles = detector.detect_negative_cycles(&graph);
        let profitable = cycles.iter().filter(|c| c.is_profitable()).count();

        if profitable > 0 {
            opp_events.push((i as u64, profitable));
            let tx_hash = receipt.transaction_hash;

            // Rank profitable cycles; compute the top one's production-path
            // gas + net profit estimate.
            let top_cycle = cycles
                .iter()
                .filter(|c| c.is_profitable())
                .min_by(|a, b| {
                    a.total_weight
                        .partial_cmp(&b.total_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .expect("profitable > 0 implies at least one");

            let path_labels: Vec<String> = top_cycle
                .path
                .iter()
                .filter_map(|&v| token_index.get_address(v).map(token_label))
                .collect();
            let path_str = path_labels.join(" -> ");

            let est = estimate_opp(top_cycle, &graph, base_fee_wei, args.sim_input_weth);
            if est.net_profit_eth > 0.0 {
                total_net_profit_eth += est.net_profit_eth;
            }

            println!(
                "  tx #{:3}  {}  → {} cycle(s), touched {} pool(s)",
                i,
                &format!("{:#x}", tx_hash)[..14],
                profitable,
                tracked_touched.len()
            );
            println!(
                "           top: {:.4}%  path: {}",
                est.profit_factor * 100.0,
                path_str
            );
            println!(
                "           sim @ {:.2} WETH input:  gross {:+.4} ETH  - gas {:.5} ETH  = net {:+.4} ETH  (gas {}, base fee {:.1} gwei)",
                args.sim_input_weth,
                est.gross_profit_eth,
                est.gas_cost_eth,
                est.net_profit_eth,
                est.gas_units,
                est.base_fee_gwei,
            );

            if let Some(w) = csv_writer.as_mut() {
                use std::io::Write;
                writeln!(
                    w,
                    "{block},{tx},{hash:#x},{cycles},{pf:.6},{hops},{path},{gas},{bf:.4},{gc:.8},{gp:.6},{np:.6}",
                    block = args.block,
                    tx = i,
                    hash = tx_hash,
                    cycles = profitable,
                    pf = est.profit_factor,
                    hops = top_cycle.num_hops(),
                    path = path_str,
                    gas = est.gas_units,
                    bf = est.base_fee_gwei,
                    gc = est.gas_cost_eth,
                    gp = est.gross_profit_eth,
                    np = est.net_profit_eth,
                )?;
            }
        }
    }
    let replay_ms = t_replay.elapsed().as_millis();

    // Flush CSV if open.
    if let Some(mut w) = csv_writer.take() {
        use std::io::Write;
        w.flush()?;
    }

    // Summary.
    println!("\n== Summary ==");
    println!("  Block:                     {}", args.block);
    println!("  Txs total:                 {}", txs.len());
    println!("  Txs skipped:               {} (e.g. EIP-4844 blobs)", skipped);
    println!("  Txs reverted:              {}", reverted);
    println!("  Detection events:          {}", opp_events.len());
    println!(
        "  Theoretical net captureable: {:+.4} ETH (at {:.2} WETH input, sum over tx windows)",
        total_net_profit_eth, args.sim_input_weth
    );
    println!("  Replay time:               {} ms", replay_ms);
    if let Some(path) = &args.csv {
        println!("  CSV:                       {}", path.display());
    }

    drop(anvil);
    Ok(())
}
