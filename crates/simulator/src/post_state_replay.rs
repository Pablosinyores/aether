//! Post-state replayer — revm fork-replay of a pending victim tx, then
//! read the affected pool's storage post-execution to recover its new
//! analytical state.
//!
//! Fallback path for cases the closed-form predictor in `aether-pools`
//! can't handle: V3 swaps that cross at least one tick boundary, Curve
//! StableSwap iterations that don't converge, Balancer pools with
//! unequal weights. In all three cases the analytical predictor returns
//! a low-confidence flag — replaying the victim against a real forked
//! EVM and reading the pool's new storage gives an exact answer.
//!
//! The Uniswap V3 reader pulls `slot0()` + `liquidity()` after the
//! victim commits. The Curve reader pulls `balances(uint256 i)` for the
//! two coin indices involved in the swap. The Balancer V2 reader resolves
//! the pool's `bytes32 poolId` via `getPoolId()` on the pool contract
//! then calls `IVault.getPoolTokens(poolId)` on the canonical mainnet
//! Vault, picking out the two balances aligned with `(token_in, token_out)`.
//!
//! Like `mempool_backrun`, this module is a synchronous pure function
//! so callers can run it on `spawn_blocking` workers without leaking
//! async dependencies.

use alloy::primitives::{address, Address, FixedBytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use aether_pools::balancer::BalancerPostState;
use aether_pools::curve::CurvePostState;
use aether_pools::uniswap_v3::V3PostState;
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, TxEnv};
use revm::database::CacheDB;
use revm::database_interface::{Database, DatabaseRef};
use revm::handler::{ExecuteCommitEvm, ExecuteEvm, MainBuilder};
use revm::primitives::hardfork::SpecId;
use revm::Context;
use tracing::debug;

use crate::fork::{ForkedState, RpcForkedState};
use crate::mempool_backrun::VictimTx;

/// Canonical Balancer V2 Vault on Ethereum mainnet. Every weighted /
/// stable / linear pool routes its balance bookkeeping through this
/// singleton; the replay reader calls `getPoolTokens(poolId)` on it
/// to recover post-swap balances.
pub const BALANCER_V2_VAULT: Address = address!("BA12222222228d8Ba445958a75a0704d566BF2C8");

sol! {
    interface IUniswapV3PoolReader {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }

    interface ICurvePoolReader {
        function balances(uint256 i) external view returns (uint256);
    }

    interface IBalancerPoolReader {
        function getPoolId() external view returns (bytes32);
    }

    interface IBalancerVaultReader {
        function getPoolTokens(bytes32 poolId)
            external
            view
            returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
    }
}

/// Why a post-state replay attempt did not produce a usable result.
/// Maps 1:1 onto the `aether_mempool_post_state_replay_total{outcome}`
/// label set in `crates/grpc-server/src/metrics.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// Victim tx executed but reverted on-chain. The replay path returns
    /// no useful post-state — analytical predictor was already going to
    /// skip this candidate anyway.
    VictimReverted,
    /// Victim tx halted (out-of-gas / stack overflow / etc.). Same
    /// downstream impact as `VictimReverted`.
    VictimHalted,
    /// View call against the pool's reader interface failed after the
    /// victim was committed (e.g. pool address has no bytecode, or
    /// returned an unexpected revert). String is the function name.
    ReadCallFailed(&'static str),
    /// ABI decode of a successful view call's output failed. Indicates
    /// either a corrupt pool or an unexpected ABI shape. String is the
    /// function name.
    DecodeFailed(&'static str),
    /// EVM dispatch itself errored (e.g. AlloyDB RPC failure). The
    /// candidate is dropped without further consequence.
    SimError,
    /// Replayer was invoked for a protocol family it does not yet
    /// implement (Curve / Balancer). String is the protocol label.
    UnimplementedProtocol(&'static str),
}

impl ReplayError {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VictimReverted => "victim_reverted",
            Self::VictimHalted => "victim_halted",
            Self::ReadCallFailed(_) => "read_call_failed",
            Self::DecodeFailed(_) => "decode_failed",
            Self::SimError => "sim_error",
            Self::UnimplementedProtocol(_) => "unimplemented_protocol",
        }
    }
}

/// Block-context parameters needed to build the revm BlockEnv for the
/// victim's intended slot. The caller resolves these once per pending tx
/// (block number from the snapshot manager + timestamp / basefee from
/// the latest block header).
#[derive(Debug, Clone)]
pub struct ReplayParams {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee: u64,
    pub chain_id: u64,
}

/// Replay the victim tx against an RPC-backed fork and read the V3
/// pool's `slot0() + liquidity()` post-execution. Production entry point.
pub fn replay_v3_post_state_rpc(
    state: RpcForkedState,
    victim: &VictimTx,
    pool: Address,
    params: &ReplayParams,
) -> Result<V3PostState, ReplayError> {
    let RpcForkedState { db, .. } = state;
    replay_v3_inner(db, victim, pool, params)
}

/// Replay the victim tx against a synthetic `ForkedState` and read the
/// V3 pool's post-state. Used by unit tests that pre-populate storage
/// without standing up a full RPC fork.
pub fn replay_v3_post_state_cache(
    state: ForkedState,
    victim: &VictimTx,
    pool: Address,
    params: &ReplayParams,
) -> Result<V3PostState, ReplayError> {
    replay_v3_inner(state.db, victim, pool, params)
}

/// Replay the victim tx against an RPC-backed fork and read a 2-coin
/// Curve pool's `balances(i)` and `balances(j)` post-execution. The
/// caller supplies the coin indices because Curve V1 pools store
/// `tokens` and `balances` as parallel arrays and the on-chain
/// `balances(uint256)` view is keyed on that index, not on a token
/// address — the `CurvePool` cached state in `aether-pools` already
/// knows the index for both ends of the swap.
///
/// `amount_out` is the difference between the pre-swap `balances(j)`
/// (read off the cached `CurvePool` by the caller) and the post-swap
/// value returned here. The reader does not re-derive it because the
/// caller has the pre-state already and feeding the pre-balance through
/// here would only duplicate that information.
pub fn replay_curve_post_state_rpc(
    state: RpcForkedState,
    victim: &VictimTx,
    pool: Address,
    coin_idx_in: u8,
    coin_idx_out: u8,
    params: &ReplayParams,
) -> Result<CurvePostState, ReplayError> {
    let RpcForkedState { db, .. } = state;
    replay_curve_inner(db, victim, pool, coin_idx_in, coin_idx_out, params)
}

/// Cache-backed sibling of [`replay_curve_post_state_rpc`] for tests that
/// pre-populate a synthetic `ForkedState` instead of standing up a real
/// RPC fork.
pub fn replay_curve_post_state_cache(
    state: ForkedState,
    victim: &VictimTx,
    pool: Address,
    coin_idx_in: u8,
    coin_idx_out: u8,
    params: &ReplayParams,
) -> Result<CurvePostState, ReplayError> {
    replay_curve_inner(state.db, victim, pool, coin_idx_in, coin_idx_out, params)
}

/// Replay the victim tx against an RPC-backed fork and read a Balancer
/// V2 pool's post-swap balances via the canonical Vault. The pool
/// contract is queried for its `bytes32 poolId`, then
/// `IVault.getPoolTokens(poolId)` is called on the Vault — both reads
/// run against the same post-commit EVM context so they always reflect
/// the victim's effect.
///
/// `token0` / `token1` are the pool's canonical token ordering (as
/// stored on the cached `BalancerPool`). The returned `BalancerPostState`
/// keeps `new_balance0` aligned with `token0` and `new_balance1` with
/// `token1`, matching the analytical-path convention so the consumer's
/// `unified_to_post_reserves` helper can re-derive swap direction
/// uniformly across analytical and replay branches.
pub fn replay_balancer_post_state_rpc(
    state: RpcForkedState,
    victim: &VictimTx,
    pool: Address,
    token0: Address,
    token1: Address,
    params: &ReplayParams,
) -> Result<BalancerPostState, ReplayError> {
    let RpcForkedState { db, .. } = state;
    replay_balancer_inner(db, victim, pool, BALANCER_V2_VAULT, token0, token1, params)
}

/// Cache-backed sibling of [`replay_balancer_post_state_rpc`]. Tests
/// supply both the pool address and the Vault address explicitly so a
/// synthetic mock can stand in for the canonical mainnet Vault.
pub fn replay_balancer_post_state_cache(
    state: ForkedState,
    victim: &VictimTx,
    pool: Address,
    vault: Address,
    token0: Address,
    token1: Address,
    params: &ReplayParams,
) -> Result<BalancerPostState, ReplayError> {
    replay_balancer_inner(state.db, victim, pool, vault, token0, token1, params)
}

fn replay_v3_inner<DB>(
    db: CacheDB<DB>,
    victim: &VictimTx,
    pool: Address,
    params: &ReplayParams,
) -> Result<V3PostState, ReplayError>
where
    DB: DatabaseRef,
    CacheDB<DB>: Database<Error = <DB as DatabaseRef>::Error>,
    <DB as DatabaseRef>::Error: std::fmt::Debug,
{
    let block = BlockEnv {
        number: U256::from(params.block_number),
        timestamp: U256::from(params.block_timestamp),
        basefee: params.base_fee,
        ..Default::default()
    };

    let ctx = Context::<BlockEnv, TxEnv, _, CacheDB<DB>, revm::context::Journal<CacheDB<DB>>, ()>::new(
        db, SpecId::CANCUN,
    )
    .with_block(block)
    .modify_cfg_chained(|cfg| {
        cfg.chain_id = params.chain_id;
        cfg.disable_nonce_check = true;
        cfg.disable_balance_check = true;
        cfg.disable_base_fee = true;
    });

    let mut evm = ctx.build_mainnet();

    let victim_env = TxEnv::builder()
        .caller(victim.from)
        .kind(revm::primitives::TxKind::Call(victim.to))
        .data(revm::primitives::Bytes::copy_from_slice(&victim.data))
        .value(victim.value)
        .gas_limit(victim.gas_limit)
        .gas_price(victim.gas_price)
        .nonce(0)
        .chain_id(Some(params.chain_id))
        .build_fill();

    match evm.transact_commit(victim_env) {
        Ok(ExecutionResult::Success { .. }) => {}
        Ok(ExecutionResult::Revert { .. }) => return Err(ReplayError::VictimReverted),
        Ok(ExecutionResult::Halt { .. }) => return Err(ReplayError::VictimHalted),
        Err(e) => {
            debug!(error = ?e, "post-state replay: victim sim error");
            return Err(ReplayError::SimError);
        }
    }

    // Read slot0() against post-victim state.
    let slot0_data = IUniswapV3PoolReader::slot0Call {}.abi_encode();
    let slot0_env = build_view_env(pool, slot0_data, params.chain_id);
    let slot0_output = match evm.transact(slot0_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("slot0")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("slot0")),
    };
    let decoded_slot0 = IUniswapV3PoolReader::slot0Call::abi_decode_returns(&slot0_output)
        .map_err(|_| ReplayError::DecodeFailed("slot0"))?;

    // Read liquidity() against the same post-victim state.
    let liq_data = IUniswapV3PoolReader::liquidityCall {}.abi_encode();
    let liq_env = build_view_env(pool, liq_data, params.chain_id);
    let liq_output = match evm.transact(liq_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("liquidity")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("liquidity")),
    };
    let decoded_liq = IUniswapV3PoolReader::liquidityCall::abi_decode_returns(&liq_output)
        .map_err(|_| ReplayError::DecodeFailed("liquidity"))?;

    // `amount_out` is intentionally left as `U256::ZERO` — the graph-edge
    // update in `unified_to_post_reserves` derives reserves from
    // `new_sqrt_price_x96` alone and never reads this field for V3. Setting
    // `single_tick = true` because the post-state was read directly from
    // post-execution storage and the multi-tick precision concern that
    // motivates the flag does not apply to revm-derived values.
    Ok(V3PostState {
        new_sqrt_price_x96: U256::from(decoded_slot0.sqrtPriceX96),
        new_liquidity: decoded_liq,
        amount_out: U256::ZERO,
        single_tick: true,
    })
}

fn build_view_env(pool: Address, data: Vec<u8>, chain_id: u64) -> TxEnv {
    TxEnv::builder()
        .caller(Address::ZERO)
        .kind(revm::primitives::TxKind::Call(pool))
        .data(revm::primitives::Bytes::from(data))
        .value(U256::ZERO)
        .gas_limit(1_000_000)
        .gas_price(0)
        .nonce(0)
        .chain_id(Some(chain_id))
        .build_fill()
}

fn replay_curve_inner<DB>(
    db: CacheDB<DB>,
    victim: &VictimTx,
    pool: Address,
    coin_idx_in: u8,
    coin_idx_out: u8,
    params: &ReplayParams,
) -> Result<CurvePostState, ReplayError>
where
    DB: DatabaseRef,
    CacheDB<DB>: Database<Error = <DB as DatabaseRef>::Error>,
    <DB as DatabaseRef>::Error: std::fmt::Debug,
{
    let block = BlockEnv {
        number: U256::from(params.block_number),
        timestamp: U256::from(params.block_timestamp),
        basefee: params.base_fee,
        ..Default::default()
    };
    let ctx = Context::<BlockEnv, TxEnv, _, CacheDB<DB>, revm::context::Journal<CacheDB<DB>>, ()>::new(
        db, SpecId::CANCUN,
    )
    .with_block(block)
    .modify_cfg_chained(|cfg| {
        cfg.chain_id = params.chain_id;
        cfg.disable_nonce_check = true;
        cfg.disable_balance_check = true;
        cfg.disable_base_fee = true;
    });
    let mut evm = ctx.build_mainnet();

    let victim_env = TxEnv::builder()
        .caller(victim.from)
        .kind(revm::primitives::TxKind::Call(victim.to))
        .data(revm::primitives::Bytes::copy_from_slice(&victim.data))
        .value(victim.value)
        .gas_limit(victim.gas_limit)
        .gas_price(victim.gas_price)
        .nonce(0)
        .chain_id(Some(params.chain_id))
        .build_fill();

    match evm.transact_commit(victim_env) {
        Ok(ExecutionResult::Success { .. }) => {}
        Ok(ExecutionResult::Revert { .. }) => return Err(ReplayError::VictimReverted),
        Ok(ExecutionResult::Halt { .. }) => return Err(ReplayError::VictimHalted),
        Err(e) => {
            debug!(error = ?e, "post-state replay: victim sim error (curve)");
            return Err(ReplayError::SimError);
        }
    }

    // Read `balances(i)` against post-victim state for both coin indices.
    let in_data = ICurvePoolReader::balancesCall { i: U256::from(coin_idx_in) }.abi_encode();
    let in_env = build_view_env(pool, in_data, params.chain_id);
    let in_output = match evm.transact(in_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("balances")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("balances")),
    };
    let new_balance_in = ICurvePoolReader::balancesCall::abi_decode_returns(&in_output)
        .map_err(|_| ReplayError::DecodeFailed("balances"))?;

    let out_data = ICurvePoolReader::balancesCall { i: U256::from(coin_idx_out) }.abi_encode();
    let out_env = build_view_env(pool, out_data, params.chain_id);
    let out_output = match evm.transact(out_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("balances")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("balances")),
    };
    let new_balance_out = ICurvePoolReader::balancesCall::abi_decode_returns(&out_output)
        .map_err(|_| ReplayError::DecodeFailed("balances"))?;

    // `i`/`j` here are usize-widened from the caller-supplied coin indices
    // (Curve V1 pools index coins as a uint256 on-chain but the index
    // always fits in a u8 in practice — 2- and 3-coin pools dominate).
    // `amount_out` is intentionally `U256::ZERO`: `unified_to_post_reserves`
    // in the mempool pipeline only consumes `new_balance_in`/`new_balance_out`
    // for Curve and never reads `amount_out`. Setting `analytical = false`
    // preserves the existing label space — the replay path is the
    // non-analytical branch by definition.
    Ok(CurvePostState {
        i: coin_idx_in as usize,
        j: coin_idx_out as usize,
        new_balance_in,
        new_balance_out,
        amount_out: U256::ZERO,
        analytical: false,
    })
}

#[allow(clippy::too_many_arguments)]
fn replay_balancer_inner<DB>(
    db: CacheDB<DB>,
    victim: &VictimTx,
    pool: Address,
    vault: Address,
    token0: Address,
    token1: Address,
    params: &ReplayParams,
) -> Result<BalancerPostState, ReplayError>
where
    DB: DatabaseRef,
    CacheDB<DB>: Database<Error = <DB as DatabaseRef>::Error>,
    <DB as DatabaseRef>::Error: std::fmt::Debug,
{
    let block = BlockEnv {
        number: U256::from(params.block_number),
        timestamp: U256::from(params.block_timestamp),
        basefee: params.base_fee,
        ..Default::default()
    };
    let ctx = Context::<BlockEnv, TxEnv, _, CacheDB<DB>, revm::context::Journal<CacheDB<DB>>, ()>::new(
        db, SpecId::CANCUN,
    )
    .with_block(block)
    .modify_cfg_chained(|cfg| {
        cfg.chain_id = params.chain_id;
        cfg.disable_nonce_check = true;
        cfg.disable_balance_check = true;
        cfg.disable_base_fee = true;
    });
    let mut evm = ctx.build_mainnet();

    let victim_env = TxEnv::builder()
        .caller(victim.from)
        .kind(revm::primitives::TxKind::Call(victim.to))
        .data(revm::primitives::Bytes::copy_from_slice(&victim.data))
        .value(victim.value)
        .gas_limit(victim.gas_limit)
        .gas_price(victim.gas_price)
        .nonce(0)
        .chain_id(Some(params.chain_id))
        .build_fill();

    match evm.transact_commit(victim_env) {
        Ok(ExecutionResult::Success { .. }) => {}
        Ok(ExecutionResult::Revert { .. }) => return Err(ReplayError::VictimReverted),
        Ok(ExecutionResult::Halt { .. }) => return Err(ReplayError::VictimHalted),
        Err(e) => {
            debug!(error = ?e, "post-state replay: victim sim error (balancer)");
            return Err(ReplayError::SimError);
        }
    }

    // Step 1: resolve poolId by calling `getPoolId()` on the pool contract.
    let pool_id_data = IBalancerPoolReader::getPoolIdCall {}.abi_encode();
    let pool_id_env = build_view_env(pool, pool_id_data, params.chain_id);
    let pool_id_output = match evm.transact(pool_id_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("getPoolId")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("getPoolId")),
    };
    let pool_id: FixedBytes<32> = IBalancerPoolReader::getPoolIdCall::abi_decode_returns(&pool_id_output)
        .map_err(|_| ReplayError::DecodeFailed("getPoolId"))?;

    // Step 2: call `getPoolTokens(poolId)` on the Vault.
    let tokens_data = IBalancerVaultReader::getPoolTokensCall { poolId: pool_id }.abi_encode();
    let tokens_env = build_view_env(vault, tokens_data, params.chain_id);
    let tokens_output = match evm.transact(tokens_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            _ => return Err(ReplayError::ReadCallFailed("getPoolTokens")),
        },
        Err(_) => return Err(ReplayError::ReadCallFailed("getPoolTokens")),
    };
    let decoded = IBalancerVaultReader::getPoolTokensCall::abi_decode_returns(&tokens_output)
        .map_err(|_| ReplayError::DecodeFailed("getPoolTokens"))?;

    // Walk the Vault's `(tokens, balances)` parallel arrays once and pick
    // out the entries matching the pool's canonical `(token0, token1)`
    // ordering. Matching by address (not by index) is robust to Balancer's
    // pool-registration ordering, which is not guaranteed to mirror the
    // engine's cached `(token0, token1)` convention.
    let mut bal0: Option<U256> = None;
    let mut bal1: Option<U256> = None;
    for (tok, bal) in decoded.tokens.iter().zip(decoded.balances.iter()) {
        if *tok == token0 {
            bal0 = Some(*bal);
        } else if *tok == token1 {
            bal1 = Some(*bal);
        }
    }
    let (Some(b0), Some(b1)) = (bal0, bal1) else {
        return Err(ReplayError::DecodeFailed("getPoolTokens"));
    };

    // `amount_out = U256::ZERO` because the consumer
    // (`unified_to_post_reserves`) derives output amounts from the
    // graph-edge update, not from this field. The analytical flag stays
    // `false` since revm-derived state is the non-analytical branch by
    // construction.
    Ok(BalancerPostState {
        new_balance0: b0,
        new_balance1: b1,
        amount_out: U256::ZERO,
        analytical: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn default_params() -> ReplayParams {
        ReplayParams {
            block_number: 18_000_000,
            block_timestamp: 1_700_000_000,
            base_fee: 1_000_000_000,
            chain_id: 1,
        }
    }

    fn default_victim() -> VictimTx {
        VictimTx {
            from: address!("2222222222222222222222222222222222222222"),
            to: address!("3333333333333333333333333333333333333333"),
            value: U256::ZERO,
            data: vec![],
            gas_price: 1_000_000_000,
            gas_limit: 500_000,
        }
    }

    #[test]
    fn reject_when_pool_address_has_no_bytecode() {
        // Empty target accepts the victim call (revm treats no-code call
        // as a value transfer success). The slot0() read against a
        // codeless pool address returns empty output; ABI decode then
        // fails because the slot0() return tuple needs ~224 bytes. The
        // decode-failed branch is the right rejection surface — it tells
        // the caller "this isn't a V3 pool" rather than implying the EVM
        // dispatch itself broke.
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let result = replay_v3_post_state_cache(state, &default_victim(), pool, &default_params());
        assert!(
            matches!(result, Err(ReplayError::DecodeFailed("slot0"))),
            "expected DecodeFailed(slot0), got {result:?}"
        );
    }

    #[test]
    fn reject_when_pool_address_is_an_eoa() {
        // Same shape as the no-bytecode case, but with the victim target
        // funded so its execution path stays clean. The decode failure
        // again surfaces from the codeless pool read.
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        state.insert_account_balance(default_victim().to, U256::from(10u128.pow(18)));
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let result = replay_v3_post_state_cache(state, &default_victim(), pool, &default_params());
        assert!(
            matches!(result, Err(ReplayError::DecodeFailed("slot0"))),
            "expected DecodeFailed(slot0), got {result:?}"
        );
    }

    #[test]
    fn curve_replay_decode_failed_when_pool_has_no_bytecode() {
        // Codeless pool address — the `balances(i)` view returns empty
        // output and ABI decode then fails, surfacing as
        // `DecodeFailed("balances")`. This is the expected rejection
        // path for a Curve replay against an address that isn't a real
        // StableSwap pool.
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        let pool = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let result = replay_curve_post_state_cache(
            state,
            &default_victim(),
            pool,
            0,
            1,
            &default_params(),
        );
        assert!(
            matches!(result, Err(ReplayError::DecodeFailed("balances"))),
            "expected DecodeFailed(balances), got {result:?}"
        );
    }

    #[test]
    fn balancer_replay_decode_failed_when_pool_has_no_bytecode() {
        // Codeless pool address — the `getPoolId()` read fails to
        // ABI-decode. Surfaces as `DecodeFailed("getPoolId")`, which is
        // the right reason for a non-Balancer address routed into the
        // Balancer replay path.
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        let pool = address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56");
        let vault = BALANCER_V2_VAULT;
        let t0 = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let t1 = address!("ba100000625a3754423978a60c9317c58a424e3D");
        let result = replay_balancer_post_state_cache(
            state,
            &default_victim(),
            pool,
            vault,
            t0,
            t1,
            &default_params(),
        );
        assert!(
            matches!(result, Err(ReplayError::DecodeFailed("getPoolId"))),
            "expected DecodeFailed(getPoolId), got {result:?}"
        );
    }

    /// Build runtime bytecode that returns the same 32-byte word for
    /// every call. Used to stand in for a Curve pool's `balances(i)`
    /// view so the replay reader's success branch can be exercised
    /// without standing up a real StableSwap deployment.
    fn const_uint256_returner(value: U256) -> Vec<u8> {
        let mut code = Vec::with_capacity(38);
        code.push(0x7f); // PUSH32
        code.extend_from_slice(&value.to_be_bytes::<32>());
        code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0
        code.push(0x52); // MSTORE
        code.extend_from_slice(&[0x60, 0x20]); // PUSH1 32
        code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0
        code.push(0xf3); // RETURN
        code
    }

    #[test]
    fn curve_replay_success_returns_decoded_balances() {
        // Stand-in pool returns the same constant for every `balances(i)`
        // call. Both reads succeed and decode cleanly, so the reader
        // populates a `CurvePostState` with `new_balance_in` /
        // `new_balance_out` both equal to the constant and
        // `analytical = false` (revm-derived state is the
        // non-analytical branch by definition).
        let constant = U256::from(123_456_789u64);
        let pool = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        state.insert_account(pool, U256::ZERO, const_uint256_returner(constant).into());
        // Fund victim so the value-transfer succeeds against codeless
        // recipient; the replay path commits the victim before the
        // balances() reads.
        state.insert_account_balance(default_victim().from, U256::from(10u128.pow(18)));
        let result = replay_curve_post_state_cache(
            state,
            &default_victim(),
            pool,
            0,
            1,
            &default_params(),
        );
        let post = result.expect("replay should succeed against const-returner bytecode");
        assert_eq!(post.new_balance_in, constant);
        assert_eq!(post.new_balance_out, constant);
        assert_eq!(post.i, 0);
        assert_eq!(post.j, 1);
        assert!(!post.analytical, "revm-derived post-state must mark analytical=false");
    }

    #[test]
    fn curve_replay_victim_reverted_path() {
        // Victim target deployed with an explicit REVERT opcode. The
        // reader must surface `VictimReverted` before any balances()
        // call is attempted.
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        let victim_target = address!("3333333333333333333333333333333333333333");
        state.insert_account(
            victim_target,
            U256::ZERO,
            vec![0x60, 0x00, 0x60, 0x00, 0xfd].into(),
        );
        state.insert_account_balance(default_victim().from, U256::from(10u128.pow(18)));
        let pool = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let result = replay_curve_post_state_cache(
            state,
            &default_victim(),
            pool,
            0,
            1,
            &default_params(),
        );
        assert!(matches!(result, Err(ReplayError::VictimReverted)));
    }

    #[test]
    fn balancer_replay_victim_reverted_path() {
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 1_000_000_000);
        let victim_target = address!("3333333333333333333333333333333333333333");
        state.insert_account(
            victim_target,
            U256::ZERO,
            vec![0x60, 0x00, 0x60, 0x00, 0xfd].into(),
        );
        state.insert_account_balance(default_victim().from, U256::from(10u128.pow(18)));
        let pool = address!("5c6Ee304399DBdB9C8Ef030aB642B10820DB8F56");
        let t0 = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let t1 = address!("ba100000625a3754423978a60c9317c58a424e3D");
        let result = replay_balancer_post_state_cache(
            state,
            &default_victim(),
            pool,
            BALANCER_V2_VAULT,
            t0,
            t1,
            &default_params(),
        );
        assert!(matches!(result, Err(ReplayError::VictimReverted)));
    }

    #[test]
    fn unimplemented_protocol_label_still_available() {
        // Kept for label-space stability — the metric vocabulary in
        // `metrics.rs` still includes `unimplemented_protocol` for
        // legitimately disabled replay paths (no provider wired,
        // semaphore saturation, etc.).
        let err = ReplayError::UnimplementedProtocol("curve");
        assert_eq!(err.as_str(), "unimplemented_protocol");
        let err = ReplayError::UnimplementedProtocol("balancer");
        assert_eq!(err.as_str(), "unimplemented_protocol");
    }

    #[test]
    fn replay_error_label_stability() {
        // Lock in the label values — the metric label space in
        // `metrics.rs` pre-touches these exact strings.
        assert_eq!(ReplayError::VictimReverted.as_str(), "victim_reverted");
        assert_eq!(ReplayError::VictimHalted.as_str(), "victim_halted");
        assert_eq!(ReplayError::ReadCallFailed("slot0").as_str(), "read_call_failed");
        assert_eq!(ReplayError::DecodeFailed("slot0").as_str(), "decode_failed");
        assert_eq!(ReplayError::SimError.as_str(), "sim_error");
    }
}
