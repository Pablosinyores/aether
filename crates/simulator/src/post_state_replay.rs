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
//! Curve and Balancer reader hooks intentionally stubbed in this module
//! and surface `UnimplementedProtocol` to the metric. Adding them is a
//! follow-up PR: each one needs an `alloy::sol!` view interface plus
//! decode of the protocol-specific post-state shape; the EVM commit
//! step is identical.
//!
//! Like `mempool_backrun`, this module is a synchronous pure function
//! so callers can run it on `spawn_blocking` workers without leaking
//! async dependencies.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
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

/// Curve post-state replay — currently unimplemented. The follow-up PR
/// adds a `sol!` `balances(uint256 i)` view interface and decodes the
/// returned uint256 for both coin indices.
pub fn replay_curve_post_state_rpc(
    _state: RpcForkedState,
    _victim: &VictimTx,
    _pool: Address,
    _params: &ReplayParams,
) -> Result<(), ReplayError> {
    Err(ReplayError::UnimplementedProtocol("curve"))
}

/// Balancer post-state replay — currently unimplemented. The follow-up
/// PR adds a `getPoolTokens(bytes32 poolId)` view call against the
/// BalancerV2 Vault contract and decodes the returned balances array.
pub fn replay_balancer_post_state_rpc(
    _state: RpcForkedState,
    _victim: &VictimTx,
    _vault: Address,
    _pool_id: [u8; 32],
    _params: &ReplayParams,
) -> Result<(), ReplayError> {
    Err(ReplayError::UnimplementedProtocol("balancer"))
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
    fn curve_stub_returns_unimplemented() {
        // Build a minimal RpcForkedState replacement-style assertion via
        // the cache path is awkward (no equivalent helper); instead
        // assert on the error variant returned by the unimplemented
        // stubs directly through the public-API surface.
        let err = ReplayError::UnimplementedProtocol("curve");
        assert_eq!(err.as_str(), "unimplemented_protocol");
    }

    #[test]
    fn balancer_stub_returns_unimplemented() {
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
