//! Shared helpers for historical-block tooling — replay (`aether-replay`) and
//! mempool profit scoring (`aether-profit-scorer`). Both binaries fetch pool
//! state at a specific block, load pools from `config/pools.toml`, decode
//! `getReserves`/`slot0` calldata, and run identical math to convert U256
//! reserves into f64 graph weights. Before this module, each binary kept its
//! own inline copy of every helper; this file is the single source of truth.

use std::path::PathBuf;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};

use aether_common::types::ProtocolType;

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
}

/// 2^96 as f64, used to convert UniswapV3 `sqrtPriceX96` into a floating-point
/// price.
pub const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

/// Per-pool state fetched from the chain. V3 carries `sqrtPriceX96`; V2/Sushi
/// carry `(reserve0, reserve1)`.
#[derive(Clone, Copy, Debug)]
pub enum PoolState {
    V2 { r0: U256, r1: U256 },
    V3 { sqrt_price_x96: U256 },
}

#[derive(Clone, Debug)]
pub struct LoadedPool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub protocol: ProtocolType,
    pub fee_bps: u32,
}

#[derive(serde::Deserialize)]
pub struct PoolEntry {
    pub protocol: String,
    pub address: String,
    pub token0: String,
    pub token1: String,
    pub fee_bps: u32,
}

#[derive(serde::Deserialize)]
pub struct PoolsConfig {
    #[serde(default)]
    pub pools: Vec<PoolEntry>,
}

/// Parse the long-form protocol string used in `config/pools.toml` (e.g.
/// `"uniswap_v2"`, `"uniswap_v3"`). Distinct from the scorer-local
/// `parse_db_protocol`, which reads the short-form strings the engine writes
/// into `mempool_predictions.protocol`.
pub fn parse_protocol(s: &str) -> Option<ProtocolType> {
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

/// Load + filter pools from a `pools.toml` config. Only V2 / Sushi / V3 pools
/// are returned; Curve / Balancer / Bancor entries parse but are dropped
/// because the historical tooling can't compute reserves for them yet.
pub fn load_pools(path: &PathBuf) -> Result<Vec<LoadedPool>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read pool config {}", path.display()))?;
    let cfg: PoolsConfig = toml::from_str(&raw).context("parse pool config")?;

    let mut out = Vec::new();
    for entry in cfg.pools {
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

/// Fetch `(getReserves)` or `(slot0().sqrtPriceX96)` for a pool at a specific
/// block. Returns `Ok(None)` if the call returns fewer bytes than expected
/// (truncated response, non-pool address). RPC errors propagate via `Err` —
/// callers that prefer the swallow-as-None behaviour wrap with
/// `.ok().flatten()`.
pub async fn fetch_pool_state_at(
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

/// Truncate a U256 to f64 by summing each 64-bit limb scaled by its power of
/// two. Loss of precision is acceptable: callers use the result for graph
/// edge weights (`-ln(price)`), where only the ratio matters.
pub fn u256_to_f64(v: U256) -> f64 {
    let limbs = v.as_limbs();
    let mut acc = 0.0f64;
    for (i, &limb) in limbs.iter().enumerate() {
        acc += (limb as f64) * (2f64).powi((64 * i) as i32);
    }
    acc
}

/// UniswapV2 / SushiSwap constant-product output formula:
/// `dy = (dx * (10_000 - fee_bps) * y) / (x * 10_000 + dx * (10_000 - fee_bps))`.
///
/// Returns `None` on zero reserves, zero input, or U256 overflow at any step.
/// `saturating_sub` on `(10_000 - fee_bps)` guards against pathological
/// configs where `fee_bps > 10_000` would otherwise underflow — for real
/// configs (max 30 bps V2, 100 bps V3) the saturating form is identical to
/// straight subtraction.
pub fn uniswap_v2_get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> Option<U256> {
    if reserve_in.is_zero() || reserve_out.is_zero() || amount_in.is_zero() {
        return None;
    }
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

/// Load the deployed-bytecode field from a forge-compiled AetherExecutor
/// artifact JSON. The pointer `/bytecode/object` matches forge's standard
/// output shape (`out/AetherExecutor.sol/AetherExecutor.json`).
pub fn load_executor_init_bytecode(artifact_path: &PathBuf) -> Result<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_protocol_known_strings() {
        assert!(matches!(parse_protocol("uniswap_v2"), Some(ProtocolType::UniswapV2)));
        assert!(matches!(parse_protocol("sushiswap"), Some(ProtocolType::SushiSwap)));
        assert!(matches!(parse_protocol("uniswap_v3"), Some(ProtocolType::UniswapV3)));
        assert!(matches!(parse_protocol("curve"), Some(ProtocolType::Curve)));
        assert!(matches!(parse_protocol("balancer_v2"), Some(ProtocolType::BalancerV2)));
        assert!(matches!(parse_protocol("bancor_v3"), Some(ProtocolType::BancorV3)));
        assert!(parse_protocol("unknown").is_none());
        assert!(parse_protocol("").is_none());
    }

    #[test]
    fn u256_to_f64_small_values_exact() {
        assert_eq!(u256_to_f64(U256::ZERO), 0.0);
        assert_eq!(u256_to_f64(U256::from(1u64)), 1.0);
        assert_eq!(u256_to_f64(U256::from(1_000_000_000_000_000_000u64)), 1e18);
    }

    #[test]
    fn uniswap_v2_get_amount_out_canonical() {
        // 1 token in, 1000:1000 reserves, 30 bps fee.
        // Expected: (1 * 9970 * 1000) / (1000 * 10000 + 1 * 9970) = 9970000 / 10009970 ≈ 0
        let out = uniswap_v2_get_amount_out(U256::from(1u64), U256::from(1000u64), U256::from(1000u64), 30);
        assert_eq!(out, Some(U256::from(0u64)));
        // Larger trade.
        let out = uniswap_v2_get_amount_out(
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(1_000_000_000_000_000_000_000u128),
            30,
        );
        assert!(out.is_some());
        assert!(out.unwrap() > U256::ZERO);
    }

    #[test]
    fn uniswap_v2_get_amount_out_zero_inputs() {
        assert_eq!(
            uniswap_v2_get_amount_out(U256::ZERO, U256::from(1u64), U256::from(1u64), 30),
            None
        );
        assert_eq!(
            uniswap_v2_get_amount_out(U256::from(1u64), U256::ZERO, U256::from(1u64), 30),
            None
        );
        assert_eq!(
            uniswap_v2_get_amount_out(U256::from(1u64), U256::from(1u64), U256::ZERO, 30),
            None
        );
    }

    #[test]
    fn load_pools_filters_unsupported_protocols() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"
[[pools]]
protocol = "uniswap_v2"
address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 30

[[pools]]
protocol = "curve"
address = "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"
token0 = "0x6B175474E89094C44Da98b954EedeAC495271d0F"
token1 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
fee_bps = 4

[[pools]]
protocol = "uniswap_v3"
address = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
token0 = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
token1 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
fee_bps = 5

[[pools]]
protocol = "unknown_dex"
address = "0x0000000000000000000000000000000000000001"
token0 = "0x0000000000000000000000000000000000000002"
token1 = "0x0000000000000000000000000000000000000003"
fee_bps = 30
"#,
        )
        .unwrap();
        let pools = load_pools(&tmp.path().to_path_buf()).unwrap();
        // Curve + unknown filtered out; V2 + V3 retained.
        assert_eq!(pools.len(), 2);
        assert!(pools.iter().any(|p| matches!(p.protocol, ProtocolType::UniswapV2)));
        assert!(pools.iter().any(|p| matches!(p.protocol, ProtocolType::UniswapV3)));
    }

    #[test]
    fn load_pools_handles_missing_pools_key() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# empty config\n").unwrap();
        let pools = load_pools(&tmp.path().to_path_buf()).unwrap();
        assert!(pools.is_empty());
    }
}
