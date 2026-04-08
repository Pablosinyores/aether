//! Shared test utilities for integration tests.
//!
//! Extracts duplicated constants, helpers, and graph-building logic used
//! across mainnet fork tests.

#![allow(dead_code)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use alloy::primitives::{address, keccak256, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolCall;

use aether_common::types::{PoolId, ProtocolType, SwapStep};
use aether_detector::bellman_ford::BellmanFord;
use aether_detector::opportunity::DetectedCycle;
use aether_simulator::calldata::build_univ2_swap_calldata;
use aether_state::price_graph::PriceGraph;
use aether_state::token_index::TokenIndex;

// ── Well-known mainnet token addresses ──────────────────────────────

pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
pub const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
pub const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

// ── Well-known UniswapV2/Sushi pair addresses ───────────────────────

pub const UNIV2_WETH_USDC: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
pub const UNIV2_WETH_USDT: Address = address!("0d4a11d5EEaaC28EC3F61d100daF4d40471f1852");
pub const SUSHI_WETH_USDC: Address = address!("397FF1542f962076d0BFE58eA045FfA2d347ACa0");
pub const SUSHI_WETH_USDT: Address = address!("06da0fd433C1A5d7a4faa01111c044910A184553");
pub const UNIV2_WETH_DAI: Address = address!("A478c2975Ab1Ea89e8196811F51A7B7Ade33eB11");
pub const SUSHI_WETH_DAI: Address = address!("C3D03e4F041Fd4cD388c549Ee2A29a9E5075882f");
pub const UNIV2_USDC_USDT: Address = address!("3041CbD36888bECc7bbCBc0045E3B1f144466f5f");

// ── Anvil defaults ──────────────────────────────────────────────────

pub const ANVIL_ACCOUNT0: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
pub const ANVIL_ACCOUNT0_KEY: &str =
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
pub const BALANCER_V2_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");
pub const WETH_BALANCE_SLOT: u64 = 3;

// Aave-flashloanable tokens
pub const FLASHLOAN_TOKENS: [Address; 4] = [WETH, USDC, USDT, DAI];

// ── ABI fragments ───────────────────────────────────────────────────

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data);
}

// ── Pool definition ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PoolDef {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub protocol: ProtocolType,
    pub fee_bps: u32,
}

/// Returns the default set of 7 monitored pools.
pub fn default_pool_set() -> Vec<PoolDef> {
    vec![
        PoolDef {
            address: UNIV2_WETH_USDC,
            token0: USDC,
            token1: WETH,
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        },
        PoolDef {
            address: UNIV2_WETH_USDT,
            token0: WETH,
            token1: USDT,
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        },
        PoolDef {
            address: SUSHI_WETH_USDC,
            token0: USDC,
            token1: WETH,
            protocol: ProtocolType::SushiSwap,
            fee_bps: 30,
        },
        PoolDef {
            address: SUSHI_WETH_USDT,
            token0: WETH,
            token1: USDT,
            protocol: ProtocolType::SushiSwap,
            fee_bps: 30,
        },
        PoolDef {
            address: UNIV2_WETH_DAI,
            token0: WETH,
            token1: DAI,
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        },
        PoolDef {
            address: SUSHI_WETH_DAI,
            token0: WETH,
            token1: DAI,
            protocol: ProtocolType::SushiSwap,
            fee_bps: 30,
        },
        PoolDef {
            address: UNIV2_USDC_USDT,
            token0: USDC,
            token1: USDT,
            protocol: ProtocolType::UniswapV2,
            fee_bps: 30,
        },
    ]
}

// ── Prerequisite checks ─────────────────────────────────────────────

/// Check if ETH_RPC_URL is set (lightweight, no binary checks).
pub fn check_rpc_available() -> bool {
    if std::env::var("ETH_RPC_URL").is_err() {
        eprintln!("Skipping test: ETH_RPC_URL not set");
        return false;
    }
    true
}

/// Check prerequisites: ETH_RPC_URL + optional binaries (anvil, forge, cast).
pub fn check_prerequisites(need_forge: bool, need_cast: bool) -> bool {
    if !check_rpc_available() {
        return false;
    }

    let mut bins = vec!["anvil"];
    if need_forge {
        bins.push("forge");
    }
    if need_cast {
        bins.push("cast");
    }

    for bin in bins {
        match Command::new(bin).arg("--version").output() {
            Ok(out) if out.status.success() => {}
            _ => {
                eprintln!("Skipping test: {bin} not found in PATH");
                return false;
            }
        }
    }
    true
}

// ── Anvil management ────────────────────────────────────────────────

/// Spawn an Anvil process forking from ETH_RPC_URL.
///
/// - `fork_block`: if `Some(n)`, fork at block `n` via `--fork-block-number`.
/// - `port_offset`: base port offset (added to PID-derived port for uniqueness).
pub fn spawn_anvil_at_block(fork_block: Option<u64>, port_offset: u16) -> (Child, String) {
    let rpc_url = std::env::var("ETH_RPC_URL").expect("ETH_RPC_URL must be set");
    let port = port_offset + (std::process::id() % 1000) as u16;

    let mut args = vec![
        "--fork-url".to_string(),
        rpc_url,
        "--port".to_string(),
        port.to_string(),
        "--silent".to_string(),
    ];

    if let Some(bn) = fork_block {
        args.push("--fork-block-number".to_string());
        args.push(bn.to_string());
    }

    let child = Command::new("anvil")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn anvil");

    let url = format!("http://127.0.0.1:{}", port);
    (child, url)
}

/// Wait for Anvil to be ready by polling `eth_blockNumber`.
pub async fn wait_for_anvil(url: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    let parsed: url::Url = url.parse().expect("valid URL");
    while start.elapsed() < timeout {
        let provider = ProviderBuilder::new().connect_http(parsed.clone());
        if provider.get_block_number().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

// ── Reserve fetching ────────────────────────────────────────────────

/// Fetch getReserves() from a UniV2/Sushi pair via eth_call (latest block).
pub async fn fetch_reserves(provider: &impl Provider, pair: Address) -> Option<(U256, U256)> {
    let calldata = getReservesCall {}.abi_encode();
    let tx = alloy::rpc::types::TransactionRequest::default()
        .to(pair)
        .input(calldata.into());

    match provider.call(tx).await {
        Ok(output) => {
            if output.len() < 96 {
                eprintln!("  {pair}: output too short ({} bytes)", output.len());
                return None;
            }
            let r0 = U256::from_be_slice(&output[0..32]);
            let r1 = U256::from_be_slice(&output[32..64]);
            Some((r0, r1))
        }
        Err(e) => {
            eprintln!("  {pair}: eth_call failed: {e}");
            None
        }
    }
}

/// Fetch getReserves() at a specific historical block number.
pub async fn fetch_reserves_at_block(
    provider: &impl Provider,
    pair: Address,
    block_number: u64,
) -> Option<(U256, U256)> {
    let calldata = getReservesCall {}.abi_encode();
    let tx = alloy::rpc::types::TransactionRequest::default()
        .to(pair)
        .input(calldata.into());

    let block_id = alloy::eips::BlockId::Number(
        alloy::eips::BlockNumberOrTag::Number(block_number),
    );

    match provider.call(tx).block(block_id).await {
        Ok(output) => {
            if output.len() < 96 {
                return None;
            }
            let r0 = U256::from_be_slice(&output[0..32]);
            let r1 = U256::from_be_slice(&output[32..64]);
            Some((r0, r1))
        }
        Err(_) => None,
    }
}

/// Fetch reserves for all pools, optionally at a specific block.
pub async fn fetch_all_reserves(
    provider: &impl Provider,
    pools: &[PoolDef],
    block: Option<u64>,
) -> Vec<(usize, U256, U256)> {
    let mut results = Vec::new();
    for (i, pool) in pools.iter().enumerate() {
        let res = if let Some(bn) = block {
            fetch_reserves_at_block(provider, pool.address, bn).await
        } else {
            fetch_reserves(provider, pool.address).await
        };
        if let Some((r0, r1)) = res {
            results.push((i, r0, r1));
        }
    }
    results
}

// ── Graph building ──────────────────────────────────────────────────

/// Build a PriceGraph + TokenIndex from pool definitions and reserves.
pub fn build_price_graph(
    pools: &[PoolDef],
    reserves: &[(usize, U256, U256)],
) -> (PriceGraph, TokenIndex) {
    let mut token_index = TokenIndex::new();
    let mut graph = PriceGraph::new(10);

    for &(pool_idx, r0, r1) in reserves {
        let pool = &pools[pool_idx];
        let t0_idx = token_index.get_or_insert(pool.token0);
        let t1_idx = token_index.get_or_insert(pool.token1);
        graph.resize(token_index.len());

        let r0_f64 = u256_to_f64(r0);
        let r1_f64 = u256_to_f64(r1);

        if r0_f64 == 0.0 || r1_f64 == 0.0 {
            continue;
        }

        let fee = (10000 - pool.fee_bps) as f64 / 10000.0;
        let pool_id = PoolId {
            address: pool.address,
            protocol: pool.protocol,
        };

        graph.add_edge(
            t0_idx,
            t1_idx,
            (r1_f64 / r0_f64) * fee,
            pool_id,
            pool.address,
            pool.protocol,
            U256::ZERO,
        );
        graph.add_edge(
            t1_idx,
            t0_idx,
            (r0_f64 / r1_f64) * fee,
            pool_id,
            pool.address,
            pool.protocol,
            U256::ZERO,
        );
    }

    (graph, token_index)
}

// ── Cycle-to-route conversion ───────────────────────────────────────

/// Convert a detected cycle into swap steps for on-chain execution.
///
/// Walks the cycle path, resolves each hop's pool and reserves,
/// computes `get_amount_out()` along the chain, and builds UniV2 swap calldata.
///
/// Returns `None` if any hop produces zero output or cannot be resolved.
pub fn cycle_to_swap_route(
    cycle: &DetectedCycle,
    graph: &PriceGraph,
    token_index: &TokenIndex,
    pools: &[PoolDef],
    reserves: &[(usize, U256, U256)],
    input_amount: U256,
    executor_addr: Address,
) -> Option<(Vec<SwapStep>, U256)> {
    if cycle.path.len() < 3 {
        return None;
    }

    let mut steps = Vec::new();
    let mut current_amount = input_amount;

    // Walk each hop in the cycle (path[0] -> path[1] -> ... -> path[last])
    for i in 0..cycle.path.len() - 1 {
        let from_vertex = cycle.path[i];
        let to_vertex = cycle.path[i + 1];

        // Find the best edge for this hop
        let edge = graph
            .edges_from(from_vertex)
            .iter()
            .filter(|e| e.to == to_vertex)
            .min_by(|a, b| {
                a.weight
                    .partial_cmp(&b.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;

        let from_token = *token_index.get_address(from_vertex)?;
        let to_token = *token_index.get_address(to_vertex)?;

        // Find the pool definition and reserves for this edge
        let (pool_def, pool_r0, pool_r1) = find_pool_reserves(
            edge.pool_address,
            pools,
            reserves,
        )?;

        // Determine which reserve is "in" and which is "out"
        let (reserve_in, reserve_out, is_token0_out) = if from_token == pool_def.token0 {
            (pool_r0, pool_r1, false) // selling token0, getting token1
        } else {
            (pool_r1, pool_r0, true) // selling token1, getting token0
        };

        let amount_out = get_amount_out(current_amount, reserve_in, reserve_out);
        if amount_out.is_zero() {
            return None;
        }

        // Build swap calldata: swap(amount0Out, amount1Out, to, data)
        let (amount0_out, amount1_out) = if is_token0_out {
            (amount_out, U256::ZERO) // getting token0 out
        } else {
            (U256::ZERO, amount_out) // getting token1 out
        };

        let swap_data = build_univ2_swap_calldata(amount0_out, amount1_out, executor_addr);

        // 1% slippage tolerance
        let min_out = amount_out * U256::from(99) / U256::from(100);

        steps.push(SwapStep {
            protocol: pool_def.protocol,
            pool_address: pool_def.address,
            token_in: from_token,
            token_out: to_token,
            amount_in: current_amount,
            min_amount_out: min_out,
            calldata: swap_data,
        });

        current_amount = amount_out;
    }

    Some((steps, current_amount))
}

/// Find pool definition and reserves for a given pool address.
fn find_pool_reserves<'a>(
    pool_address: Address,
    pools: &'a [PoolDef],
    reserves: &[(usize, U256, U256)],
) -> Option<(&'a PoolDef, U256, U256)> {
    for &(pool_idx, r0, r1) in reserves {
        if pools[pool_idx].address == pool_address {
            return Some((&pools[pool_idx], r0, r1));
        }
    }
    None
}

// ── Math helpers ────────────────────────────────────────────────────

/// Convert a U256 to f64 approximation.
pub fn u256_to_f64(val: U256) -> f64 {
    let limbs = val.as_limbs();
    limbs[0] as f64
        + limbs[1] as f64 * 18_446_744_073_709_551_616.0
        + limbs[2] as f64 * 3.402_823_669_209_385e38
        + limbs[3] as f64 * 1.157_920_892_373_162e77
}

/// UniV2 constant-product getAmountOut: (997 * dx * y) / (1000 * x + 997 * dx)
pub fn get_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    if reserve_in.is_zero() || reserve_out.is_zero() || amount_in.is_zero() {
        return U256::ZERO;
    }
    let amount_in_with_fee = amount_in * U256::from(997);
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(1000) + amount_in_with_fee;
    numerator / denominator
}

/// Compute keccak256(abi.encode(addr, slot)) for ERC20 balanceOf mapping.
pub fn erc20_balance_slot(addr: Address, mapping_slot: u64) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(addr.as_slice());
    buf[56..64].copy_from_slice(&mapping_slot.to_be_bytes());
    let hash = keccak256(buf);
    U256::from_be_bytes(hash.0)
}

/// Query ERC20 balanceOf via eth_call.
pub async fn erc20_balance(provider: &impl Provider, token: Address, account: Address) -> U256 {
    let calldata = balanceOfCall { account }.abi_encode();
    let tx = alloy::rpc::types::TransactionRequest::default()
        .to(token)
        .input(calldata.into());

    match provider.call(tx).await {
        Ok(output) => {
            if output.len() >= 32 {
                U256::from_be_slice(&output[0..32])
            } else {
                U256::ZERO
            }
        }
        Err(_) => U256::ZERO,
    }
}

/// Get the contracts/ directory path relative to the integration-tests crate.
pub fn contracts_root() -> std::path::PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("contracts")
}

// ── Deploy helper ───────────────────────────────────────────────────

/// Deploy AetherExecutor to an Anvil fork. Returns the deployed address.
pub fn deploy_executor(anvil_url: &str) -> Result<Address, String> {
    let contracts_dir = contracts_root();

    let deploy_output = Command::new("forge")
        .args([
            "create",
            "--broadcast",
            "--root",
            contracts_dir.to_str().unwrap(),
            "--rpc-url",
            anvil_url,
            "--private-key",
            ANVIL_ACCOUNT0_KEY,
            "src/AetherExecutor.sol:AetherExecutor",
            "--constructor-args",
            &format!("{}", AAVE_V3_POOL),
            &format!("{}", BALANCER_V2_VAULT),
        ])
        .output()
        .map_err(|e| format!("forge create failed to run: {e}"))?;

    if !deploy_output.status.success() {
        let stderr = String::from_utf8_lossy(&deploy_output.stderr);
        let stdout = String::from_utf8_lossy(&deploy_output.stdout);
        return Err(format!(
            "forge create failed:\nstdout: {stdout}\nstderr: {stderr}"
        ));
    }

    let stdout_str = String::from_utf8_lossy(&deploy_output.stdout);
    let addr_str = stdout_str
        .lines()
        .find_map(|line| line.strip_prefix("Deployed to: "))
        .ok_or_else(|| format!("No 'Deployed to' in forge output:\n{stdout_str}"))?
        .trim();
    addr_str
        .parse()
        .map_err(|e| format!("parse executor address '{addr_str}': {e}"))
}

// ── Detection helpers ───────────────────────────────────────────────

/// Run Bellman-Ford detection on a price graph.
pub fn run_detection(graph: &PriceGraph, max_hops: usize, timeout_us: u64) -> Vec<DetectedCycle> {
    let bf = BellmanFord::new(max_hops, timeout_us);
    bf.detect_negative_cycles(graph)
}

/// Check if a token is Aave-flashloanable (WETH, USDC, USDT, DAI).
pub fn is_flashloanable(token: &Address) -> bool {
    FLASHLOAN_TOKENS.contains(token)
}
