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
        .or_else(|| std::env::var("ETH_RPC_URL").ok())
        .context("--rpc-url not set and ETH_RPC_URL env var missing")?;

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
