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

use aether_common::types::{PoolId, ProtocolType, SwapStep};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::gas as gas_model;
use aether_detector::opportunity::DetectedCycle;
use aether_simulator::calldata::{
    build_execute_arb_calldata, build_univ2_swap_calldata, build_univ3_swap_calldata,
};
use aether_simulator::fork::{RpcForkedState, SimConfig};
use aether_simulator::EvmSimulator;
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
    const AAVE: Address = address!("7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9");
    match *addr {
        WETH => "WETH".into(),
        USDC => "USDC".into(),
        USDT => "USDT".into(),
        DAI => "DAI".into(),
        WBTC => "WBTC".into(),
        AAVE => "AAVE".into(),
        _ => format!("{:#x}", addr).chars().take(10).collect::<String>() + "…",
    }
}

#[derive(Parser, Clone)]
#[command(
    name = "aether-replay",
    about = "Replay one historical block through Aether's detector. Prints detected cycles."
)]
struct Args {
    /// Target block number. Reserves are fetched at `block - 1` (state before
    /// the block landed). Ignored when `--blocks-file` is set.
    #[arg(long, default_value_t = 0)]
    block: u64,

    /// Path to a newline-delimited file of block numbers for batch replay.
    /// Each listed block is replayed independently through the full-block
    /// pipeline with its own Anvil fork; `--csv` (if set) is appended across
    /// all blocks. Empty lines and lines starting with `#` are ignored.
    /// Implies `--full-block`.
    #[arg(long)]
    blocks_file: Option<PathBuf>,

    /// Skip state seeding for impersonated senders (intra-block mode).
    /// By default, before each historical tx replays, the sender's real
    /// mainnet balances for known ERC20s (USDC / USDT / DAI / WETH) at
    /// `block - 1` are written into Anvil storage so the tx can actually
    /// execute — without this, impersonated senders hold zero tokens on the
    /// fork and most real-world txs revert. Set this flag to reproduce the
    /// old degenerate-fork behavior.
    #[arg(long, default_value_t = false)]
    no_seed_state: bool,

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

    /// Attach to an already-running Anvil at `--anvil-port` instead of
    /// spawning a new one. Used by `scripts/historical_replay_e2e.sh`, which
    /// spawns Anvil once and points the whole production pipeline at it.
    #[arg(long, default_value_t = false)]
    anvil_attach: bool,

    /// Write per-detection-event rows to this CSV path (intra-block mode only).
    /// Columns: block, tx_index, tx_hash, cycles, top_profit_factor, hops,
    /// path, est_gas, base_fee_gwei, gas_cost_eth, sim_net_profit_eth.
    #[arg(long)]
    csv: Option<PathBuf>,

    /// Input amount (in WETH, 18 decimals) used to compute simulated net
    /// profit. `sim_net_profit = profit_factor * input - gas_cost`.
    #[arg(long, default_value_t = 1.0)]
    sim_input_weth: f64,

    /// Run each detected cycle through a full on-chain simulation of
    /// `AetherExecutor.executeArb()` on the replay's Anvil fork (revm under
    /// the hood). Each detection triggers: `evm_snapshot` → deploy-if-needed
    /// → impersonate owner → send tx → measure WETH balance delta →
    /// `evm_revert`. Kills graph-math outliers from drained-liquidity pools
    /// and reports actual-executable P&L per cycle. Requires the AetherExecutor
    /// Solidity artifact at `--executor-artifact` (default `contracts/out/AetherExecutor.sol/AetherExecutor.json`).
    #[arg(long, default_value_t = false)]
    sim_on_chain: bool,

    /// Path to the compiled `AetherExecutor.json` (forge artifact). Only read
    /// when `--sim-on-chain` is set.
    #[arg(long, default_value = "contracts/out/AetherExecutor.sol/AetherExecutor.json")]
    executor_artifact: PathBuf,
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
    ranked.sort_by(|a, b| {
        a.total_weight
            .partial_cmp(&b.total_weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

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

    // Batch mode: --blocks-file overrides --block and implies --full-block.
    if let Some(path) = args.blocks_file.clone() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read --blocks-file {}", path.display()))?;
        let blocks: Vec<u64> = content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.parse::<u64>())
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("parse --blocks-file {}", path.display()))?;
        if blocks.is_empty() {
            anyhow::bail!("--blocks-file {} contained no block numbers", path.display());
        }
        return run_batch_replay(&args, &rpc_url, &blocks).await;
    }

    if args.block == 0 {
        anyhow::bail!("--block is required when --blocks-file is not supplied");
    }

    if args.full_block {
        return run_full_block_replay(&args, &rpc_url, None).await;
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

/// Batch replay across a list of blocks. Each block spawns its own Anvil
/// fork. `--csv` output (when set) is opened once at the start with a single
/// header row and appended across every block.
async fn run_batch_replay(args: &Args, rpc_url: &str, blocks: &[u64]) -> Result<()> {
    println!("== Aether Batch Replay ==");
    println!("  Blocks:          {}", blocks.len());
    println!("  First / last:    {} .. {}", blocks[0], blocks[blocks.len() - 1]);
    if let Some(path) = &args.csv {
        println!("  CSV:             {} (appended across all blocks)", path.display());
    }
    if args.no_seed_state {
        println!("  State seeding:   DISABLED (--no-seed-state)");
    } else {
        println!("  State seeding:   enabled (USDC/USDT/DAI/WETH balances + ETH)");
    }

    // Open the shared CSV once so all per-block runs append to the same file.
    let shared_csv: Option<std::cell::RefCell<std::io::BufWriter<std::fs::File>>> =
        match &args.csv {
            Some(path) => {
                let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
                use std::io::Write;
                writeln!(
                    f,
                    "block,tx_index,tx_hash,cycles,top_profit_factor,hops,path,est_gas,base_fee_gwei,gas_cost_eth,sim_gross_profit_eth,sim_net_profit_eth,sim_success,sim_profit_eth,sim_gas_used,sim_revert_reason"
                )?;
                Some(std::cell::RefCell::new(f))
            }
            None => None,
        };

    let t0 = Instant::now();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (i, block_num) in blocks.iter().enumerate() {
        println!(
            "\n── Block {}/{}  (#{}) ──",
            i + 1,
            blocks.len(),
            block_num
        );
        let mut per_block_args = args.clone();
        per_block_args.block = *block_num;
        // The shared CSV is written externally; suppress the per-block header.
        per_block_args.csv = None;
        match run_full_block_replay(&per_block_args, rpc_url, shared_csv.as_ref()).await {
            Ok(()) => ok += 1,
            Err(e) => {
                tracing::warn!(block = block_num, error = %e, "block replay failed");
                failed += 1;
            }
        }
    }

    if let Some(cell) = shared_csv {
        use std::io::Write;
        cell.borrow_mut().flush()?;
    }

    println!("\n== Batch Summary ==");
    println!("  Blocks succeeded:  {}/{}", ok, blocks.len());
    println!("  Blocks failed:     {}", failed);
    println!("  Total time:        {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

/// Write the sender's real mainnet balances (ETH + known ERC20s) into Anvil's
/// fork state so the impersonated historical tx has real assets to work with.
///
/// Without this, Anvil's lazy state returns zero for any address that wasn't
/// touched pre-block, so ERC20.transferFrom reverts and the downstream state
/// transitions the tx would have caused (pool reserves shift, AAVE drains,
/// etc.) never happen on the fork — the arbs that depended on them become
/// invisible to the replay.
///
/// This only seeds a small set of known-slot ERC20s (USDC, USDT, DAI, WETH).
/// Tokens outside this set fall through to the original degenerate-fork
/// behavior — the tx may revert. Best-effort enrichment; never fatal.
async fn seed_sender_state<P: Provider>(
    anvil: &P,
    archive: &P,
    sender: Address,
    fork_block: u64,
) -> Result<()> {
    use alloy::primitives::{keccak256, B256};
    use alloy::rpc::types::serde_helpers::WithOtherFields;

    // Known ERC20 balances mapping slots. Entry format:
    //   (token, balances_mapping_slot)
    // Layout: storage_key = keccak256(pad32(owner) ++ pad32(slot))
    const TOKEN_SLOTS: &[(Address, u64)] = &[
        // USDC (FiatTokenV2): balances mapping at slot 9.
        (address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), 9),
        // USDT (TetherToken): balances mapping at slot 2.
        (address!("dAC17F958D2ee523a2206206994597C13D831ec7"), 2),
        // DAI (MakerDAO): balances mapping at slot 2.
        (address!("6B175474E89094C44Da98b954EedeAC495271d0F"), 2),
        // WETH9: balanceOf mapping at slot 3.
        (address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), 3),
    ];

    // 1. Fund sender with 1,000,000 ETH via anvil_setBalance. Covers both
    //    gas and any tx.value without additional surgery. Costs nothing —
    //    the fork is ephemeral.
    let funding = U256::from(1_000_000u128) * U256::from(10u128).pow(U256::from(18));
    let eth_hex = format!("0x{:x}", funding);
    anvil
        .client()
        .request::<_, ()>("anvil_setBalance", (sender, eth_hex))
        .await
        .context("anvil_setBalance")?;

    // 2. For each known ERC20, read the sender's real mainnet balance at
    //    `fork_block` and write it into the fork's storage. Done in parallel.
    let futs = TOKEN_SLOTS.iter().map(|&(token, slot)| async move {
        // Compute the storage key: keccak256( pad32(sender) || pad32(slot) )
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(sender.as_slice());
        buf[63] = slot as u8;
        let key = keccak256(buf);

        // Read real mainnet balance at pre-fork block via archive RPC.
        let balance: Option<U256> = match archive
            .raw_request::<_, B256>(
                "eth_getStorageAt".into(),
                (
                    token,
                    U256::from_be_bytes(key.0),
                    BlockId::from(fork_block),
                ),
            )
            .await
        {
            Ok(b256) => {
                let v = U256::from_be_bytes(b256.0);
                if v.is_zero() { None } else { Some(v) }
            }
            Err(_) => None,
        };

        (token, key, balance)
    });

    let results = futures::future::join_all(futs).await;

    for (_token, key, balance) in results.into_iter() {
        if let Some(val) = balance {
            let key_hex = format!("0x{:x}", key);
            let val_hex = format!("0x{:064x}", val);
            let _ = anvil
                .client()
                .request::<_, bool>(
                    "anvil_setStorageAt",
                    (_token, key_hex, val_hex),
                )
                .await;
        }
    }

    // WithOtherFields import kept only if we extend this to non-standard RPC
    // shapes later — silence unused warning for now.
    let _ = std::marker::PhantomData::<WithOtherFields<()>>;

    Ok(())
}

/// Intra-block replay entry point.
async fn run_full_block_replay(
    args: &Args,
    rpc_url: &str,
    shared_csv: Option<&std::cell::RefCell<std::io::BufWriter<std::fs::File>>>,
) -> Result<()> {
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

    // Anvil fork: spawn a new instance, or attach to a pre-existing one
    // (used by the e2e orchestration script which runs the full pipeline
    // against a single long-lived Anvil).
    let anvil_url_str = format!("http://127.0.0.1:{}", args.anvil_port);
    let _anvil_handle: Option<AnvilHandle> = if args.anvil_attach {
        println!(
            "\n  Attaching to existing Anvil at port {} ...",
            args.anvil_port
        );
        wait_for_anvil(&anvil_url_str, Duration::from_secs(15)).await?;
        None
    } else {
        println!("\n  Spawning Anvil at port {} ...", args.anvil_port);
        let anvil = spawn_anvil(rpc_url, fork_block, args.anvil_port)?;
        wait_for_anvil(&anvil.url, Duration::from_secs(60)).await?;
        Some(anvil)
    };
    let anvil_url: url::Url = anvil_url_str.parse()?;
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
    let mut total_sim_profit_eth = 0.0f64;
    let mut sim_success_count = 0usize;
    let mut sim_revert_count = 0usize;
    let mut sim_skip_count = 0usize;
    let mut skipped = 0usize;
    let mut reverted = 0usize;
    let detector = BellmanFord::new(args.max_hops, args.max_time_us);

    // Deploy AetherExecutor once per Anvil instance when on-chain sim is on.
    // Address is deterministic (CREATE from SIM_OWNER nonce 0), so subsequent
    // detections reuse the same executor for every cycle in this block.
    let sim_executor_addr: Option<Address> = if args.sim_on_chain {
        match load_executor_init_bytecode(&args.executor_artifact) {
            Ok(init) => match deploy_executor_on_anvil(&anvil_provider, &init).await {
                Ok(addr) => {
                    println!("  Sim executor:    deployed at {:#x}", addr);
                    Some(addr)
                }
                Err(e) => {
                    println!("  Sim executor:    DEPLOY FAILED — on-chain sim disabled ({})", e);
                    None
                }
            },
            Err(e) => {
                println!(
                    "  Sim executor:    artifact load failed — on-chain sim disabled ({})",
                    e
                );
                None
            }
        }
    } else {
        println!("  Sim executor:    off (pass --sim-on-chain to enable)");
        None
    };

    // Local CSV writer (single-block mode). Batch mode passes `shared_csv`
    // instead and this stays None. New columns `sim_success / sim_profit_eth /
    // sim_gas_used / sim_revert_reason` surface the on-chain-sim verdict next
    // to the offline graph-math estimate.
    let mut local_csv_writer: Option<std::io::BufWriter<std::fs::File>> =
        if shared_csv.is_none() {
            match &args.csv {
                Some(path) => {
                    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
                    use std::io::Write;
                    writeln!(
                        f,
                        "block,tx_index,tx_hash,cycles,top_profit_factor,hops,path,est_gas,base_fee_gwei,gas_cost_eth,sim_gross_profit_eth,sim_net_profit_eth,sim_success,sim_profit_eth,sim_gas_used,sim_revert_reason"
                    )?;
                    Some(f)
                }
                None => None,
            }
        } else {
            None
        };

    // Archive RPC handle reused for balance reads during state seeding.
    let archive_for_seed: url::Url = rpc_url.parse().context("parse RPC URL")?;
    let seed_archive = ProviderBuilder::new().connect_http(archive_for_seed);

    let t_replay = Instant::now();
    let mut seeded_senders: std::collections::HashSet<Address> = std::collections::HashSet::new();
    for (i, tx) in txs.iter().enumerate() {
        let Some(req) = build_impersonation_request(tx) else {
            skipped += 1;
            continue;
        };

        // Seed the sender's real mainnet balances into Anvil once per unique
        // sender, before their first tx in this block replays. Skipped when
        // --no-seed-state is set (reproduces old degenerate-fork behavior).
        if !args.no_seed_state {
            if let Some(from) = req.from {
                if seeded_senders.insert(from) {
                    if let Err(e) =
                        seed_sender_state(&anvil_provider, &seed_archive, from, fork_block).await
                    {
                        tracing::debug!(%from, error = %e, "state seeding failed (continuing)");
                    }
                }
            }
        }

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

            // On-chain sim: run executeArb through revm (via Anvil) for this
            // cycle and measure actual WETH delivered to the owner. This is
            // what actually matters — graph-level profit_factor is a lower-
            // bound radar that needs the revm verdict before we call it P&L.
            let sim_outcome = if let Some(executor) = sim_executor_addr {
                let outcome = sim_arb_with_evm_simulator(
                    &anvil_provider,
                    executor,
                    top_cycle,
                    &graph,
                    &token_index,
                    &pools,
                    args.sim_input_weth,
                )
                .await;
                if outcome.success {
                    sim_success_count += 1;
                    total_sim_profit_eth += u256_to_f64(outcome.profit_wei) / 1e18;
                } else if outcome
                    .revert_reason
                    .as_deref()
                    .map(|r| r.starts_with("skipped"))
                    .unwrap_or(false)
                {
                    sim_skip_count += 1;
                } else {
                    sim_revert_count += 1;
                }
                Some(outcome)
            } else {
                None
            };

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
                "           graph @ {:.2} WETH input:  gross {:+.4} ETH  - gas {:.5} ETH  = net {:+.4} ETH  (gas {}, base fee {:.1} gwei)",
                args.sim_input_weth,
                est.gross_profit_eth,
                est.gas_cost_eth,
                est.net_profit_eth,
                est.gas_units,
                est.base_fee_gwei,
            );
            if let Some(ref sim) = sim_outcome {
                if sim.success {
                    println!(
                        "           revm sim: ✓ SUCCESS — actual profit {:+.6} ETH (gas used {})",
                        u256_to_f64(sim.profit_wei) / 1e18,
                        sim.gas_used,
                    );
                } else {
                    let reason = sim
                        .revert_reason
                        .as_deref()
                        .unwrap_or("unknown revert");
                    println!("           revm sim: ✗ REVERTED — {}", reason);
                }
            }

            // CSV row. `sim_*` columns are empty when on-chain sim is off.
            let (sim_success, sim_profit_eth, sim_gas_used, sim_reason) = match &sim_outcome {
                Some(s) => (
                    if s.success { "true" } else { "false" }.to_string(),
                    format!("{:.8}", u256_to_f64(s.profit_wei) / 1e18),
                    s.gas_used.to_string(),
                    s.revert_reason.clone().unwrap_or_default(),
                ),
                None => ("".into(), "".into(), "".into(), "".into()),
            };

            // Write the CSV row — either to this run's local file, or into
            // the batch-shared writer if we were called from `run_batch_replay`.
            if let Some(cell) = shared_csv {
                use std::io::Write;
                let mut w = cell.borrow_mut();
                writeln!(
                    w,
                    "{block},{tx},{hash:#x},{cycles},{pf:.6},{hops},{path},{gas},{bf:.4},{gc:.8},{gp:.6},{np:.6},{ss},{sp},{sg},{sr}",
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
                    ss = sim_success,
                    sp = sim_profit_eth,
                    sg = sim_gas_used,
                    sr = sim_reason,
                )?;
            } else if let Some(w) = local_csv_writer.as_mut() {
                use std::io::Write;
                writeln!(
                    w,
                    "{block},{tx},{hash:#x},{cycles},{pf:.6},{hops},{path},{gas},{bf:.4},{gc:.8},{gp:.6},{np:.6},{ss},{sp},{sg},{sr}",
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
                    ss = sim_success,
                    sp = sim_profit_eth,
                    sg = sim_gas_used,
                    sr = sim_reason,
                )?;
            }
        }
    }
    let replay_ms = t_replay.elapsed().as_millis();

    // Flush local CSV if we opened one (batch shared writer is flushed by
    // the caller).
    if let Some(mut w) = local_csv_writer.take() {
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
        "  Theoretical net captureable: {:+.4} ETH (graph-level, at {:.2} WETH input, sum over tx windows)",
        total_net_profit_eth, args.sim_input_weth
    );
    if args.sim_on_chain {
        println!(
            "  Revm-sim result:           {} succeeded / {} reverted / {} skipped",
            sim_success_count, sim_revert_count, sim_skip_count
        );
        println!(
            "  Actually-executable P&L:   {:+.6} ETH (sum of revm-confirmed per-cycle profits at {:.2} WETH input)",
            total_sim_profit_eth, args.sim_input_weth
        );
    }
    println!("  Replay time:               {} ms", replay_ms);
    if let Some(path) = &args.csv {
        println!("  CSV:                       {}", path.display());
    }

    drop(_anvil_handle);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// On-chain simulation via Anvil's snapshot/revert (revm under the hood)
// ────────────────────────────────────────────────────────────────────────────
//
// This closes the gap between Bellman-Ford graph-level profit factors and
// actually-executable MEV. Per detection we deploy AetherExecutor once per
// block, then for every cycle: evm_snapshot → build executeArb calldata →
// impersonate owner → send tx → measure WETH balance delta → evm_revert.
//
// Catches what graph math misses:
//   * INSUFFICIENT_LIQUIDITY on drained pools (the 1M ETH outlier in
//     post-CoW-exploit blocks)
//   * UniswapV2 K-invariant violations on partial reserves
//   * Flashloan repayment shortfalls
//   * V3 tick-crossing math failures
//   * Aave premium not covered by round-trip profit
//   * Cross-hop state consistency bugs in our own executor

// Mainnet infra addresses — constructor args for AetherExecutor. Anvil forks
// mainnet, so these contracts exist at their mainnet addresses on the fork.
const AAVE_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
const BALANCER_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
const BANCOR_NETWORK: Address = address!("eEF417e1D5CC832e619ae18D2F140De2999dD4fB");
const WETH_ADDR: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// Deterministic owner/deployer address the replay uses to deploy and then
/// own the simulated AetherExecutor instance. Value is arbitrary — any
/// non-mainnet-contract address works — but fixing it keeps logs readable.
const SIM_OWNER: Address = address!("1111111111111111111111111111111111111111");

// WETH ERC20 `balanceOf(address) -> uint256` — used to measure arb profit
// post-sim (the executor sweeps profit to `owner()` as WETH via safeTransfer
// at the end of executeOperation when `asset == WETH`).
sol! {
    function balanceOf(address account) external view returns (uint256);
}

/// Result of a single on-chain sim of `executeArb` for one detected cycle.
struct SimOutcome {
    /// True iff the executeArb tx landed successfully AND `minProfitOut`
    /// would have been met (we pass 0 so any non-negative profit succeeds).
    success: bool,
    /// Actual WETH profit delivered to the owner, measured as
    /// `WETH.balanceOf(owner) after - before`. Zero on revert.
    profit_wei: U256,
    /// Gas consumed by the executeArb tx (flashloan callback included since
    /// Aave V3 charges within the same tx).
    gas_used: u64,
    /// Either the revert reason (truncated) or `"skipped: <why>"` for cycles
    /// we couldn't build valid calldata for.
    revert_reason: Option<String>,
}

/// Load AetherExecutor init-bytecode from the forge-compiled JSON artifact.
/// We use `bytecode.object` (deploy bytecode, runs constructor + installs
/// runtime) not `deployedBytecode` — the constructor fills `aavePool`
/// immutable and sets `owner = msg.sender`.
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
        anyhow::bail!("executor bytecode is empty — artifact may be abstract / interface-only");
    }
    Ok(bytes)
}

/// Deploy `AetherExecutor` on the Anvil fork from `SIM_OWNER` with constructor
/// args `(AAVE_POOL, BALANCER_VAULT, BANCOR_NETWORK)`. Returns the deployed
/// contract address. Impersonation + balance seeding are done here so the
/// caller only needs to invoke once per Anvil instance.
async fn deploy_executor_on_anvil<P: Provider + Clone>(
    provider: &P,
    init_bytecode: &[u8],
) -> Result<Address> {
    // 1. Fund the deployer so the deployment tx can pay gas.
    let hundred_eth = U256::from(100u64) * U256::from(10u64).pow(U256::from(18u64));
    provider
        .raw_request::<_, ()>(
            "anvil_setBalance".into(),
            (SIM_OWNER, format!("0x{:x}", hundred_eth)),
        )
        .await
        .context("anvil_setBalance for SIM_OWNER")?;

    // 2. Impersonate (auto-impersonate may or may not already cover SIM_OWNER
    //    — call explicitly to be safe; idempotent on Anvil).
    provider
        .raw_request::<_, ()>("anvil_impersonateAccount".into(), (SIM_OWNER,))
        .await
        .context("anvil_impersonateAccount SIM_OWNER")?;

    // 3. Encode constructor args after the init-bytecode.
    use alloy::sol_types::SolValue;
    let ctor_args = (AAVE_POOL, BALANCER_VAULT, BANCOR_NETWORK).abi_encode_params();
    let mut deploy_data = init_bytecode.to_vec();
    deploy_data.extend_from_slice(&ctor_args);

    // 4. Send the deployment tx. `to: None` == CREATE.
    let tx = TransactionRequest::default()
        .from(SIM_OWNER)
        .input(deploy_data.into())
        .gas_limit(8_000_000);

    let pending = provider
        .send_transaction(tx)
        .await
        .context("send AetherExecutor deploy tx")?;
    let receipt = pending
        .get_receipt()
        .await
        .context("get AetherExecutor deploy receipt")?;

    if !receipt.status() {
        anyhow::bail!("AetherExecutor deployment reverted");
    }
    let addr = receipt
        .contract_address
        .ok_or_else(|| anyhow::anyhow!("deploy receipt missing contract_address"))?;

    Ok(addr)
}

/// Fetch live pool state (reserves for V2/Sushi, sqrtPriceX96 for V3) at the
/// current Anvil head. Used by `build_steps_from_cycle` to compute expected
/// output amounts and thus each hop's chained `amount_in`.
async fn fetch_state_at_head<P: Provider>(
    provider: &P,
    pool: &LoadedPool,
) -> Option<PoolState> {
    fetch_one_state_latest(provider, pool).await
}

/// Build `Vec<SwapStep>` matching a detected cycle, ready for
/// `build_execute_arb_calldata`. Computes each hop's expected output using
/// the same on-chain math the production engine uses, then fills the
/// per-protocol inner calldata via the PR #90 builders.
///
/// Returns `None` if:
/// - cycle touches a protocol we don't encode here yet (Curve/Balancer/Bancor)
/// - expected output would be zero (e.g. drained pool → downstream hop impossible)
/// - a pool's live state can't be fetched
async fn build_steps_from_cycle<P: Provider>(
    provider: &P,
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
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

        // Pick the best edge (lowest weight = highest rate) between these
        // vertices — matches how `estimate_opp` ranks.
        let edge = graph
            .edges_from(from_v)
            .iter()
            .filter(|e| e.to == to_v)
            .min_by(|a, b| a.weight.partial_cmp(&b.weight).unwrap_or(std::cmp::Ordering::Equal))?;

        let token_in = *token_index.get_address(from_v)?;
        let token_out = *token_index.get_address(to_v)?;

        // Find the registry entry (fee_bps + token ordering) for this pool.
        let pool_entry = pools.iter().find(|p| p.address == edge.pool_address)?;

        // Compute expected output using the same AMM math the on-chain
        // contract will use. This is a best-effort forecast — the real
        // executable amount will be whatever `swap()` produces.
        let state = fetch_state_at_head(provider, pool_entry).await?;
        let (amount_out, inner_calldata) = match (pool_entry.protocol, state) {
            (ProtocolType::UniswapV2 | ProtocolType::SushiSwap, PoolState::V2 { r0, r1 }) => {
                // token0 is always the lower address on-chain (V2 invariant).
                let (reserve_in, reserve_out, zero_for_one) = if token_in == pool_entry.token0 {
                    (r0, r1, true)
                } else {
                    (r1, r0, false)
                };
                let out = uniswap_v2_get_amount_out(current_amount, reserve_in, reserve_out, pool_entry.fee_bps)?;
                if out.is_zero() {
                    return None;
                }
                // For V2, AetherExecutor receives tokens before the swap and
                // calls the pool with zero-amount on the input side. Recipient
                // is the next pool or the executor itself (final hop).
                let (amount0_out, amount1_out) = if zero_for_one {
                    (U256::ZERO, out)
                } else {
                    (out, U256::ZERO)
                };
                // Recipient for the last hop is the executor; intermediate
                // hops can also use the executor (contract routes internally).
                let cd = build_univ2_swap_calldata(amount0_out, amount1_out, executor_addr);
                (out, cd)
            }
            (ProtocolType::UniswapV3, PoolState::V3 { .. }) => {
                // V3 math (tick traversal) is too heavy to replicate here;
                // approximate output using the graph edge's rate. The on-chain
                // revm sim will tell us the real executable amount.
                let rate = (-edge.weight).exp(); // weight = -ln(rate)
                let approx_out = U256::from((u256_to_f64(current_amount) * rate).max(0.0) as u128);
                if approx_out.is_zero() {
                    return None;
                }
                let zero_for_one = token_in == pool_entry.token0;
                // `amount_specified > 0` means exact-input. Price limit set to
                // (near-)edge values to effectively disable slippage in the sim
                // — we enforce profit via minProfitOut at the outer level.
                let sqrt_limit = if zero_for_one {
                    U256::from(4_295_128_740u64) // MIN_SQRT_RATIO + 1
                } else {
                    // MAX_SQRT_RATIO - 1 ≈ 2^160 - 1 - 1.
                    (U256::from(1u8) << 160) - U256::from(2u8)
                };
                let amt_i128 = i128::try_from(current_amount.saturating_to::<u128>()).ok()?;
                let cd = build_univ3_swap_calldata(executor_addr, zero_for_one, amt_i128, sqrt_limit);
                (approx_out, cd)
            }
            _ => return None, // Curve / Balancer / Bancor not encoded here (tracked in #97).
        };

        steps.push(SwapStep {
            protocol: pool_entry.protocol,
            pool_address: pool_entry.address,
            token_in,
            token_out,
            amount_in: current_amount,
            min_amount_out: U256::ZERO, // profit enforced at the outer minProfitOut
            calldata: inner_calldata,
        });

        current_amount = amount_out;
    }

    Some(steps)
}

/// UniswapV2 `getAmountOut` — exact math, no rounding. Returns `None` on
/// zero-liquidity input (prevents the "drained pool" outlier from producing
/// a non-zero forecast).
fn uniswap_v2_get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> Option<U256> {
    if reserve_in.is_zero() || reserve_out.is_zero() || amount_in.is_zero() {
        return None;
    }
    // Default UniV2 fee: 30 bps → multiplier 997/1000.
    let fee_multiplier = U256::from(10_000u64 - fee_bps as u64);
    let amount_in_with_fee = amount_in.checked_mul(fee_multiplier)?;
    let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
    let denom = reserve_in.checked_mul(U256::from(10_000u64))?.checked_add(amount_in_with_fee)?;
    if denom.is_zero() {
        return None;
    }
    Some(numerator / denom)
}

/// WETH's `balanceOf` mapping is at storage slot 3 (verified against mainnet
/// WETH9 source). Used by `simulate_rpc_with_erc20_profit` to locate the
/// `balanceOf(SIM_OWNER)` slot for pre/post-sim balance diff.
const WETH_BALANCE_SLOT: u64 = 3;

/// Run one cycle through the production `EvmSimulator` (revm) against the
/// replay's Anvil as an RPC backend. Unlike `evm_snapshot`/`evm_revert` on
/// the same Anvil (which invalidates Anvil's fork cache and starves
/// subsequent tx-replay state fetches), `simulate_rpc_with_erc20_profit`
/// runs in an isolated `RpcForkedState::CacheDB` that's discarded after the
/// call — Anvil's own cache is left alone, so the next tx replay's pool
/// state fetches still hit warm storage.
///
/// The sim uses Anvil's CURRENT head as the fork block (so it sees post-tx
/// state, including any executor we've deployed there and any pool writes
/// from earlier txs in the block being replayed). Profit is measured as the
/// WETH balance delta on `SIM_OWNER` directly out of revm's returned state
/// diff — no extra RPC round-trips.
async fn sim_arb_with_evm_simulator<P: Provider + Clone + 'static>(
    anvil_provider: &P,
    executor_addr: Address,
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[LoadedPool],
    sim_input_weth: f64,
) -> SimOutcome {
    let flashloan_amount = U256::from((sim_input_weth * 1e18) as u128);

    // Build SwapStep[] for this cycle using live on-chain state. Returns
    // None for unsupported cycles (Curve/Balancer/Bancor hops) or zero-
    // liquidity intermediate pools.
    let steps = match build_steps_from_cycle(
        anvil_provider,
        cycle,
        graph,
        token_index,
        pools,
        executor_addr,
        flashloan_amount,
    )
    .await
    {
        Some(s) if !s.is_empty() => s,
        _ => {
            return SimOutcome {
                success: false,
                profit_wei: U256::ZERO,
                gas_used: 0,
                revert_reason: Some("skipped: unsupported protocol or zero-liquidity hop".into()),
            };
        }
    };

    // Build executeArb calldata. `minProfitOut=0` + `tipBps=0` so revm lets
    // any non-negative profit succeed and 100% of it lands with `SIM_OWNER`
    // as WETH — our balance-diff observable.
    let deadline = U256::from(u64::MAX);
    let calldata = build_execute_arb_calldata(
        &steps,
        WETH_ADDR,
        flashloan_amount,
        deadline,
        U256::ZERO,
        U256::ZERO,
    );

    // Fetch Anvil's current head block to pin RpcForkedState and extract
    // the real timestamp + base fee. Aave V3 uses block.timestamp for
    // reserve-index interest math — passing 0 underflows its
    // `block.timestamp - lastUpdateTimestamp` subtraction and triggers a
    // silent revert inside flashLoanSimple, which we'd surface as
    // `FlashLoanFailed`. Same applies to base_fee for EIP-1559 validation.
    let head = match anvil_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            return SimOutcome {
                success: false,
                profit_wei: U256::ZERO,
                gas_used: 0,
                revert_reason: Some("anvil latest block missing".into()),
            };
        }
        Err(e) => {
            return SimOutcome {
                success: false,
                profit_wei: U256::ZERO,
                gas_used: 0,
                revert_reason: Some(format!("anvil head fetch failed: {}", truncate_err(&e.to_string()))),
            };
        }
    };
    let head_block = head.header.number;
    let head_timestamp = head.header.timestamp;
    let head_base_fee = head.header.base_fee_per_gas.unwrap_or(1_000_000_000);

    // Construct the fork state. `RpcForkedState::new` returns `None` only if
    // called outside a multi-threaded tokio runtime — which we're not, so
    // this branch is effectively dead.
    let dyn_provider = alloy::providers::DynProvider::new(anvil_provider.clone());
    // Use `latest` block tag rather than a specific Anvil local block number.
    // Anvil's local-mined blocks may not resolve cleanly for every kind of
    // state query (code / storage / balance at an exact block N+K), but the
    // `latest` tag always serves from Anvil's current state — including any
    // contract we've deployed and any pool writes from replayed txs.
    let state = match RpcForkedState::new_at_latest(
        dyn_provider,
        head_block,
        head_timestamp,
        head_base_fee,
    ) {
        Some(s) => s,
        None => {
            return SimOutcome {
                success: false,
                profit_wei: U256::ZERO,
                gas_used: 0,
                revert_reason: Some("RpcForkedState construction failed (not in multi-thread runtime)".into()),
            };
        }
    };

    // Configure the simulator: caller = SIM_OWNER (matches executeArb's
    // onlyOwner gate), gas headroom generous enough to fit the full
    // flashloan callback (Aave + swaps + repay).
    let sim = EvmSimulator::new(SimConfig {
        gas_limit: 8_000_000,
        chain_id: 1,
        caller: SIM_OWNER,
        value: U256::ZERO,
    });

    // Run the sim on a blocking thread — revm's `transact` blocks on
    // `DatabaseRef::storage` / `basic` callbacks that fetch state via
    // AlloyDB, which internally calls `block_in_place`. Running this
    // outside a blocking context would panic.
    let result = tokio::task::spawn_blocking(move || {
        sim.simulate_rpc_with_erc20_profit(
            state,
            executor_addr,
            calldata,
            WETH_ADDR,
            SIM_OWNER,
            U256::from(WETH_BALANCE_SLOT),
        )
    })
    .await;

    match result {
        Ok(sim_result) => SimOutcome {
            success: sim_result.success,
            profit_wei: sim_result.profit_wei,
            gas_used: sim_result.gas_used,
            revert_reason: sim_result.revert_reason.map(|r| truncate_err(&r)),
        },
        Err(e) => SimOutcome {
            success: false,
            profit_wei: U256::ZERO,
            gas_used: 0,
            revert_reason: Some(format!("spawn_blocking failed: {}", e)),
        },
    }
}

/// Truncate alloy/anvil error strings for CSV output. Keep first 240 chars
/// to preserve the revert selector + any decoded error data while dropping
/// noisy trailing context (JSON-RPC envelope, source-location chains).
fn truncate_err(s: &str) -> String {
    if s.len() <= 240 {
        s.replace(['\n', ','], " ")
    } else {
        format!("{}…", &s.chars().take(240).collect::<String>().replace(['\n', ','], " "))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests — pure helpers
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_label_known_symbols() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
        let wbtc = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
        let aave = address!("7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9");
        assert_eq!(token_label(&weth), "WETH");
        assert_eq!(token_label(&usdc), "USDC");
        assert_eq!(token_label(&usdt), "USDT");
        assert_eq!(token_label(&dai), "DAI");
        assert_eq!(token_label(&wbtc), "WBTC");
        assert_eq!(token_label(&aave), "AAVE");
    }

    #[test]
    fn token_label_unknown_falls_back_to_truncated_hex() {
        let unknown = address!("1234567890123456789012345678901234567890");
        let label = token_label(&unknown);
        assert!(label.ends_with('\u{2026}'));
        assert!(label.starts_with("0x"));
    }

    #[test]
    fn parse_protocol_all_variants() {
        assert_eq!(parse_protocol("uniswap_v2"), Some(ProtocolType::UniswapV2));
        assert_eq!(parse_protocol("sushiswap"), Some(ProtocolType::SushiSwap));
        assert_eq!(parse_protocol("uniswap_v3"), Some(ProtocolType::UniswapV3));
        assert_eq!(parse_protocol("curve"), Some(ProtocolType::Curve));
        assert_eq!(parse_protocol("balancer_v2"), Some(ProtocolType::BalancerV2));
        assert_eq!(parse_protocol("bancor_v3"), Some(ProtocolType::BancorV3));
    }

    #[test]
    fn parse_protocol_unknown_returns_none() {
        assert!(parse_protocol("pancakeswap_v3").is_none());
        assert!(parse_protocol("").is_none());
        assert!(parse_protocol("UNISWAP_V2").is_none()); // case-sensitive
    }

    #[test]
    fn uniswap_v2_get_amount_out_matches_onchain_math() {
        // Hand-verified: 1 ETH in, 10 ETH / 20_000 USDC reserves, 30 bps fee.
        //   amount_in_with_fee = 1e18 * 9970 = 9.97e21
        //   numerator          = 9.97e21 * 2e10 = 1.994e32
        //   denom              = 10e18 * 10000 + 9.97e21 = 1.0997e23
        //   amount_out         = 1.994e32 / 1.0997e23 ≈ 1.8136e9 micro-USDC
        let amt = uniswap_v2_get_amount_out(
            U256::from(1_000_000_000_000_000_000u128),  // 1 WETH
            U256::from(10_000_000_000_000_000_000u128), // 10 WETH reserves
            U256::from(20_000_000_000u64),              // 20_000 USDC (6 dec)
            30,
        )
        .unwrap();
        // Strictly below the no-slippage ceiling (2e9 micro-USDC = 2000 USDC).
        assert!(amt > U256::ZERO);
        assert!(amt < U256::from(2_000_000_000u64));
        // Within ~1 USDC of the analytical answer (1813.6 USDC).
        assert!(amt >= U256::from(1_813_000_000u64));
        assert!(amt <= U256::from(1_814_000_000u64));
    }

    #[test]
    fn uniswap_v2_get_amount_out_rejects_degenerate_inputs() {
        // Zero reserves on either side = no liquidity
        assert!(uniswap_v2_get_amount_out(U256::from(1u64), U256::ZERO, U256::from(100u64), 30).is_none());
        assert!(uniswap_v2_get_amount_out(U256::from(1u64), U256::from(100u64), U256::ZERO, 30).is_none());
        // Zero input — nothing to swap
        assert!(uniswap_v2_get_amount_out(U256::ZERO, U256::from(100u64), U256::from(100u64), 30).is_none());
    }

    #[test]
    fn truncate_err_short_passthrough() {
        let s = "simple error no commas or newlines";
        assert_eq!(truncate_err(s), s);
    }

    #[test]
    fn truncate_err_strips_commas_and_newlines() {
        let s = "error line1\nline2, with comma";
        let out = truncate_err(s);
        assert!(!out.contains('\n'));
        assert!(!out.contains(','));
    }

    #[test]
    fn truncate_err_caps_at_240_chars() {
        let s = "x".repeat(500);
        let out = truncate_err(&s);
        // 240 chars + a single ellipsis sentinel
        assert!(out.chars().count() <= 241);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn u256_to_f64_roundtrip_small_values() {
        // Exact for integers below 2^52 (f64 mantissa limit).
        for &v in &[0u64, 1, 42, 1_000_000, (1u64 << 52) - 1] {
            assert_eq!(u256_to_f64(U256::from(v)), v as f64);
        }
    }

    #[test]
    fn u256_to_f64_handles_one_eth() {
        // 1e18 wei in U256 should f64-round to exactly 1e18 (representable).
        let one_eth = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(u256_to_f64(one_eth), 1e18);
    }
}
