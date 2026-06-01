//! Fee-on-transfer / honeypot screen for candidate pool tokens.
//!
//! Why this exists: the constant-product (and other) AMM math the engine uses
//! assumes `amount_out` arrives in full. **Fee-on-transfer ("tax") tokens break
//! that assumption** — the recipient receives less than the swap math predicts,
//! so the simulator says "profit" and the on-chain execution reverts (or worse,
//! lands at a loss). A burst of those reverts also trips the consecutive-revert
//! circuit breaker. So before any auto-discovered memecoin pool is admitted to
//! the registry, its token must pass this screen.
//!
//! Approach (this component): in a forked EVM, override a fresh test holder's
//! balance of the token, then `transfer()` a realistic amount **into the pool
//! address** and check conservation. Transferring *to the pair* is exactly what
//! a swap's input leg does, so pair-gated transfer taxes trigger here. The
//! verdict is one of [`FotVerdict`]:
//! - `Clean`            — pool received the full amount (within tolerance).
//! - `FeeOnTransfer`    — pool received less; reports the tax in bps.
//! - `Honeypot`         — the transfer reverted / halted (transfer-restricted).
//! - `Inconclusive`     — could not locate the token's balance storage slot.
//!
//! Scope note: this screens the **buy/input leg**. A sell-side honeypot (where
//! `transfer` works but the *swap out* reverts) needs the full buy→sell pool
//! round-trip, which is the planned refinement — see the admission gate (C3).
//!
//! The pure helpers ([`erc20_balance_key`], [`expected_amount_out`],
//! [`discover_balance_slot`], [`classify_transfer`]) carry no revm/RPC
//! dependency and are unit-tested directly; the revm executor
//! ([`screen_token_transfer`]) wires them to a forked state and is exercised by
//! an RPC-gated integration test.

use crate::fork::{RpcDB, RpcForkedState};
use alloy::primitives::{address, keccak256, Address, U256};
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, TxEnv};
use revm::database::DatabaseRef;
use revm::handler::{ExecuteEvm, MainBuilder};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{Bytes, TxKind};
use revm::state::AccountInfo;
use revm::Context;
use tracing::debug;

/// `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// Fixed, otherwise-unused test holder EOA used as the transfer sender. Its
/// token balance is overridden in the fork before the probe runs.
const TEST_HOLDER: Address = address!("000000000000000000000000000000000000d39D");

/// Tuning for the screen.
#[derive(Clone, Debug)]
pub struct FotConfig {
    /// Tax (in bps) at or below which a token is still treated as `Clean` —
    /// absorbs integer-rounding noise. Any deviation above this is reported as
    /// `FeeOnTransfer`. The admission gate decides the admit/reject policy on
    /// the reported number.
    pub max_tax_bps: u32,
    /// Transfer size for the probe, as a fraction of the pool's token balance
    /// (in bps). Small enough to be realistic, large enough that a percentage
    /// tax is measurable above rounding. Default 10 bps = 0.1% of reserve.
    pub test_fraction_bps: u32,
    /// How many storage slots to brute-force when locating the token's
    /// `mapping(address => uint256)` balances slot.
    pub max_slot_probe: u64,
    /// Gas limit for the probe transfer.
    pub gas_limit: u64,
}

impl Default for FotConfig {
    fn default() -> Self {
        Self {
            max_tax_bps: 10,
            test_fraction_bps: 10,
            max_slot_probe: 40,
            gas_limit: 1_000_000,
        }
    }
}

/// Outcome of screening one token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FotVerdict {
    /// Conservation held (received >= sent, or tax within tolerance).
    Clean { observed_tax_bps: u32 },
    /// The pool received measurably less than was sent.
    FeeOnTransfer { tax_bps: u32 },
    /// The transfer reverted or halted — transfer-restricted / honeypot-like.
    Honeypot { reason: String },
    /// Could not run a conclusive probe (e.g. balance slot not found).
    Inconclusive { reason: String },
}

impl FotVerdict {
    /// Only `Clean` tokens are safe to feed the constant-product AMM math.
    pub fn is_admissible(&self) -> bool {
        matches!(self, FotVerdict::Clean { .. })
    }
}

/// Storage key for `balances[holder]` of a Solidity
/// `mapping(address => uint256)` declared at `mapping_slot`:
/// `keccak256(pad32(holder) ++ pad32(mapping_slot))`. Mirrors the derivation
/// used by [`crate::EvmSimulator`]'s ERC20-profit accounting.
pub fn erc20_balance_key(holder: Address, mapping_slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[32..64].copy_from_slice(&mapping_slot.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(buf).as_slice())
}

/// UniswapV2-style constant-product output for `amount_in`, where `fee_bps` is
/// the pool fee in basis points (e.g. 30 = 0.30%). Returns 0 on degenerate
/// input. Provided for the future buy→sell round-trip; not used by the
/// single-transfer screen.
pub fn expected_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() || fee_bps >= 10_000 {
        return U256::ZERO;
    }
    let fee_factor = U256::from(10_000u32 - fee_bps);
    let amount_in_with_fee = amount_in * fee_factor;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(10_000u64) + amount_in_with_fee;
    numerator / denominator
}

/// Tax in basis points implied by receiving `actual` when `expected` was sent.
/// 0 when there is no shortfall.
fn tax_bps(expected: U256, actual: U256) -> u32 {
    if expected.is_zero() || actual >= expected {
        return 0;
    }
    let diff = expected - actual;
    let bps = diff.saturating_mul(U256::from(10_000u64)) / expected;
    u32::try_from(bps).unwrap_or(10_000)
}

/// Classify a single transfer: `sent` left the holder, `received` arrived at the
/// recipient. A shortfall above `max_tax_bps` is a fee-on-transfer token.
pub fn classify_transfer(sent: U256, received: U256, max_tax_bps: u32) -> FotVerdict {
    let tax = tax_bps(sent, received);
    if tax <= max_tax_bps {
        FotVerdict::Clean {
            observed_tax_bps: tax,
        }
    } else {
        FotVerdict::FeeOnTransfer { tax_bps: tax }
    }
}

/// Brute-force the balances-mapping storage slot for a token by finding the
/// slot whose `balances[holder]` storage equals a *known* balance. `read_storage`
/// returns the token's storage value at a given key (i.e. a closure over
/// `db.storage_ref(token, key)`). Returns `None` if `known_balance` is zero or
/// no slot in `0..max_slots` matches.
pub fn discover_balance_slot<F>(
    read_storage: F,
    holder: Address,
    known_balance: U256,
    max_slots: u64,
) -> Option<U256>
where
    F: Fn(U256) -> U256,
{
    if known_balance.is_zero() {
        return None;
    }
    (0..max_slots).map(U256::from).find(|slot| {
        read_storage(erc20_balance_key(holder, *slot)) == known_balance
    })
}

/// Screen `token` (paired in `pool`, which currently holds `pool_token_balance`
/// of it) for fee-on-transfer / transfer-restriction behaviour against `state`.
///
/// `pool_token_balance` is the pool's on-chain balance of `token` (the V2
/// reserve, or the vault/pool balance) — supplied by the caller from the live
/// registry/state and used both to locate the balances slot and to size the
/// probe transfer. Consumes `state` (each probe needs a fresh fork).
pub fn screen_token_transfer(
    mut state: RpcForkedState,
    token: Address,
    pool: Address,
    pool_token_balance: U256,
    cfg: &FotConfig,
) -> FotVerdict {
    // 1. Locate the balances mapping slot using the pool as a known holder.
    let slot = {
        let read = |key: U256| state.db.storage_ref(token, key).unwrap_or_default();
        match discover_balance_slot(read, pool, pool_token_balance, cfg.max_slot_probe) {
            Some(s) => s,
            None => {
                return FotVerdict::Inconclusive {
                    reason: "balance storage slot not found".into(),
                }
            }
        }
    };

    // 2. Size the probe transfer and override the test holder's balance.
    let x = (pool_token_balance.saturating_mul(U256::from(cfg.test_fraction_bps))
        / U256::from(10_000u64))
    .max(U256::from(1u64));
    let holder_key = erc20_balance_key(TEST_HOLDER, slot);
    let _ = state.db.insert_account_storage(token, holder_key, x);

    // Pre-transfer pool balance (read before the fork state is consumed).
    let pool_key = erc20_balance_key(pool, slot);
    let pre_pool = state.db.storage_ref(token, pool_key).unwrap_or_default();

    // 3. Build `transfer(pool, x)` calldata.
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&TRANSFER_SELECTOR);
    let mut to_word = [0u8; 32];
    to_word[12..32].copy_from_slice(pool.as_slice());
    data.extend_from_slice(&to_word);
    data.extend_from_slice(&x.to_be_bytes::<32>());

    // 4. Execute the transfer in the fork.
    let RpcForkedState {
        db,
        block_number,
        block_timestamp,
        base_fee,
        chain_id,
    } = state;
    let block = BlockEnv {
        number: U256::from(block_number),
        timestamp: U256::from(block_timestamp),
        basefee: base_fee,
        ..Default::default()
    };
    let tx = TxEnv::builder()
        .caller(TEST_HOLDER)
        .kind(TxKind::Call(token))
        .data(Bytes::copy_from_slice(&data))
        .value(U256::ZERO)
        .gas_limit(cfg.gas_limit)
        .gas_price(base_fee as u128)
        .nonce(0)
        .chain_id(Some(chain_id))
        .build_fill();
    let ctx = Context::<BlockEnv, TxEnv, _, RpcDB, revm::context::Journal<RpcDB>, ()>::new(
        db,
        SpecId::CANCUN,
    )
    .with_block(block)
    .modify_cfg_chained(|c| {
        c.chain_id = chain_id;
        c.disable_nonce_check = true;
        c.disable_balance_check = true;
        c.disable_base_fee = true;
    });
    let mut evm = ctx.build_mainnet();

    match evm.transact(tx) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { .. } => {
                let post_pool = rs
                    .state
                    .get(&token)
                    .and_then(|acc| acc.storage.get(&pool_key))
                    .map(|s| s.present_value)
                    .unwrap_or(pre_pool);
                let received = post_pool.saturating_sub(pre_pool);
                let verdict = classify_transfer(x, received, cfg.max_tax_bps);
                debug!(%token, %pool, sent = %x, %received, ?verdict, "fot screen complete");
                verdict
            }
            ExecutionResult::Revert { output, .. } => FotVerdict::Honeypot {
                reason: format!("transfer reverted: 0x{}", alloy::hex::encode(&output)),
            },
            ExecutionResult::Halt { reason, .. } => FotVerdict::Honeypot {
                reason: format!("transfer halted: {reason:?}"),
            },
        },
        Err(e) => FotVerdict::Inconclusive {
            reason: format!("evm transact error: {e}"),
        },
    }
}

// ===========================================================================
// Buy→sell round-trip primitives (sell-side honeypot / tax detection)
// ===========================================================================

/// `swap(uint256,uint256,address,bytes)` selector on a UniswapV2 pair.
const V2_SWAP_SELECTOR: [u8; 4] = [0x02, 0x2c, 0x0d, 0x9f];

/// Storage slot holding a UniswapV2 pair's packed reserves
/// (`reserve0 | reserve1 << 112 | blockTimestampLast << 224`).
pub const V2_RESERVES_SLOT: u64 = 8;

/// Unpack a UniswapV2 `slot 8` value into `(reserve0, reserve1)` (each uint112).
pub fn unpack_v2_reserves(slot_value: U256) -> (U256, U256) {
    let mask = (U256::from(1u64) << 112) - U256::from(1u64);
    let reserve0 = slot_value & mask;
    let reserve1 = (slot_value >> 112) & mask;
    (reserve0, reserve1)
}

/// ABI-encode `pair.swap(amount0Out, amount1Out, to, "")` (empty `data`, so no
/// flash-swap callback). Layout: selector ++ amount0Out ++ amount1Out ++ to ++
/// data-offset(0x80) ++ data-len(0).
pub fn encode_v2_swap(amount0_out: U256, amount1_out: U256, to: Address) -> Vec<u8> {
    let mut data = Vec::with_capacity(4 + 5 * 32);
    data.extend_from_slice(&V2_SWAP_SELECTOR);
    data.extend_from_slice(&amount0_out.to_be_bytes::<32>());
    data.extend_from_slice(&amount1_out.to_be_bytes::<32>());
    let mut to_word = [0u8; 32];
    to_word[12..32].copy_from_slice(to.as_slice());
    data.extend_from_slice(&to_word);
    data.extend_from_slice(&U256::from(0x80u64).to_be_bytes::<32>()); // bytes offset
    data.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // bytes length 0
    data
}

/// Outcome of a buy→sell round-trip screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundTripVerdict {
    /// Round-trip recovered the base asset within `max_loss_bps` of the
    /// fee-only expectation — safe to trade.
    Clean { recovery_bps: u32 },
    /// The token is sellable but the round-trip lost materially more than the
    /// pool fee implies — a transfer/swap tax. `loss_bps` is beyond fee.
    FeeOnTransfer { loss_bps: u32 },
    /// The sell leg failed or returned nothing — sell-side honeypot.
    Honeypot { reason: String },
    /// Could not run a conclusive round-trip.
    Inconclusive { reason: String },
    /// The screen exceeded its wall-clock budget (the discovery loop runs the
    /// screen under a `tokio::time::timeout`). Distinct from `Inconclusive` so
    /// dashboards can alert on screen-budget exhaustion without false-positives
    /// from balance-slot misses.
    ScreenTimeout { budget_ms: u64 },
}

impl RoundTripVerdict {
    pub fn is_admissible(&self) -> bool {
        matches!(self, RoundTripVerdict::Clean { .. })
    }

    /// Stable, low-cardinality label for metrics. The label set is the enum
    /// discriminator only — never embed reason strings (which can include
    /// token addresses) so the Prometheus cardinality stays bounded.
    pub fn metric_label(&self) -> &'static str {
        match self {
            RoundTripVerdict::Clean { .. } => "clean",
            RoundTripVerdict::FeeOnTransfer { .. } => "fee_on_transfer",
            RoundTripVerdict::Honeypot { .. } => "honeypot",
            RoundTripVerdict::Inconclusive { .. } => "inconclusive",
            RoundTripVerdict::ScreenTimeout { .. } => "screen_timeout",
        }
    }

    /// Bridge to [`FotVerdict`] for the admission gate ([`crate::fee_on_transfer`]
    /// round-trip screen produces a `RoundTripVerdict`, but `pool_admission::evaluate`
    /// consumes a `FotVerdict`). Only `Clean` is admissible on both sides; timeouts
    /// map to `Inconclusive` so the gate rejects rather than admits on a budget miss.
    pub fn as_fot_verdict(&self) -> FotVerdict {
        match self {
            RoundTripVerdict::Clean { .. } => FotVerdict::Clean {
                observed_tax_bps: 0,
            },
            RoundTripVerdict::FeeOnTransfer { loss_bps } => {
                FotVerdict::FeeOnTransfer { tax_bps: *loss_bps }
            }
            RoundTripVerdict::Honeypot { reason } => FotVerdict::Honeypot {
                reason: reason.clone(),
            },
            RoundTripVerdict::Inconclusive { reason } => FotVerdict::Inconclusive {
                reason: reason.clone(),
            },
            RoundTripVerdict::ScreenTimeout { budget_ms } => FotVerdict::Inconclusive {
                reason: format!("screen timed out after {budget_ms}ms"),
            },
        }
    }
}

/// Classify a round-trip from its measured economics. `base_in` was spent
/// buying; `base_out` was recovered selling everything back; `fee_bps` is the
/// pool fee. A tax-free round-trip through one pool recovers ≈ `(1-fee)²` of
/// `base_in` (minus small, size-dependent price impact), so anything losing
/// more than that beyond `max_loss_bps` is taxed.
///
/// `max_loss_bps` must budget for the round-trip price impact at the probe
/// size (impact does not fully cancel), so it is deliberately loose; the gate
/// can tighten it.
pub fn classify_round_trip(
    base_in: U256,
    base_out: U256,
    fee_bps: u32,
    sell_succeeded: bool,
    max_loss_bps: u32,
) -> RoundTripVerdict {
    if !sell_succeeded {
        return RoundTripVerdict::Honeypot {
            reason: "sell leg reverted".into(),
        };
    }
    if base_out.is_zero() {
        return RoundTripVerdict::Honeypot {
            reason: "sell recovered zero base".into(),
        };
    }
    if base_in.is_zero() {
        return RoundTripVerdict::Inconclusive {
            reason: "zero base_in".into(),
        };
    }
    // Actual recovery in bps of base_in.
    let actual_bps = u32::try_from(
        base_out.saturating_mul(U256::from(10_000u64)) / base_in,
    )
    .unwrap_or(u32::MAX);
    // Fee-only expectation: (1-fee)² expressed in bps = fee_factor² / 10000.
    let ff = (10_000u64).saturating_sub(fee_bps as u64);
    let expected_bps = u32::try_from(ff * ff / 10_000).unwrap_or(10_000);
    let loss_bps = expected_bps.saturating_sub(actual_bps);
    if loss_bps <= max_loss_bps {
        RoundTripVerdict::Clean {
            recovery_bps: actual_bps,
        }
    } else {
        RoundTripVerdict::FeeOnTransfer { loss_bps }
    }
}

/// Run one `Call` against `db`, commit its state diffs back into `db` on
/// success, and return `(success, revert/halt reason, db)`. The committed `db`
/// is threaded into the next call so a multi-step sequence shares state (the
/// same technique [`crate::EvmSimulator::deploy_and_simulate_with_erc20_profit`]
/// uses).
#[allow(clippy::too_many_arguments)]
fn v2_transact(
    db: RpcDB,
    block: &BlockEnv,
    chain_id: u64,
    base_fee: u64,
    caller: Address,
    to: Address,
    data: Vec<u8>,
    nonce: u64,
    gas_limit: u64,
) -> (bool, Option<String>, RpcDB) {
    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(to))
        .data(Bytes::copy_from_slice(&data))
        .value(U256::ZERO)
        .gas_limit(gas_limit)
        .gas_price(base_fee as u128)
        .nonce(nonce)
        .chain_id(Some(chain_id))
        .build_fill();
    let ctx = Context::<BlockEnv, TxEnv, _, RpcDB, revm::context::Journal<RpcDB>, ()>::new(
        db,
        SpecId::CANCUN,
    )
    .with_block(block.clone())
    .modify_cfg_chained(|c| {
        c.chain_id = chain_id;
        c.disable_nonce_check = true;
        c.disable_balance_check = true;
        c.disable_base_fee = true;
    });
    let mut evm = ctx.build_mainnet();
    match evm.transact(tx) {
        Ok(rs) => {
            let success = matches!(rs.result, ExecutionResult::Success { .. });
            let reason = match &rs.result {
                ExecutionResult::Revert { output, .. } => {
                    Some(format!("revert 0x{}", alloy::hex::encode(output)))
                }
                ExecutionResult::Halt { reason, .. } => Some(format!("halt {reason:?}")),
                _ => None,
            };
            let mut db = evm.ctx.journaled_state.database;
            if success {
                for (a, account) in rs.state.iter() {
                    if account.is_selfdestructed() {
                        continue;
                    }
                    let info = &account.info;
                    db.insert_account_info(
                        *a,
                        AccountInfo {
                            balance: info.balance,
                            nonce: info.nonce,
                            code_hash: info.code_hash,
                            code: info.code.clone(),
                            ..Default::default()
                        },
                    );
                    for (slot, sv) in account.storage.iter() {
                        let _ = db.insert_account_storage(*a, *slot, sv.present_value);
                    }
                }
            }
            (success, reason, db)
        }
        Err(e) => {
            let db = evm.ctx.journaled_state.database;
            (false, Some(format!("evm error: {e}")), db)
        }
    }
}

#[inline]
fn read_balance(db: &RpcDB, token: Address, holder: Address, slot: U256) -> U256 {
    db.storage_ref(token, erc20_balance_key(holder, slot))
        .unwrap_or_default()
}

/// Full buy→sell round-trip screen for a token paired with `base` (e.g. WETH)
/// in a **UniswapV2-style** `pair`. Buys `token` with `base` (base funded by
/// balance override — base is assumed tax-free), then sells the received tokens
/// back via a *real* `transfer` into the pair followed by `swap` — so a
/// fee-on-transfer hook or a sell-only honeypot triggers on the sell leg and is
/// caught. Returns a [`RoundTripVerdict`] from the recovered-base ratio.
///
/// `fee_bps` must be the pair's real fee (30 for UniV2/SushiSwap). `base_slot`
/// is `base`'s ERC20 balances mapping slot (WETH = 3). Consumes `state`.
///
/// NOTE: end-to-end behaviour is verified by an RPC-backed integration run (a
/// forked pair), not by unit tests — the pure decision pieces it composes
/// (`expected_amount_out`, `unpack_v2_reserves`, `encode_v2_swap`,
/// `classify_round_trip`) are unit-tested.
#[allow(clippy::too_many_arguments)]
pub fn screen_token_v2_round_trip(
    mut state: RpcForkedState,
    pair: Address,
    token: Address,
    base: Address,
    base_slot: U256,
    fee_bps: u32,
    cfg: &FotConfig,
    max_loss_bps: u32,
) -> RoundTripVerdict {
    let eoa = TEST_HOLDER;
    let token_is_0 = token < base;

    // Reserves from slot 8.
    let slot8 = state
        .db
        .storage_ref(pair, U256::from(V2_RESERVES_SLOT))
        .unwrap_or_default();
    let (r0, r1) = unpack_v2_reserves(slot8);
    let (reserve_token, reserve_base) = if token_is_0 { (r0, r1) } else { (r1, r0) };
    if reserve_token.is_zero() || reserve_base.is_zero() {
        return RoundTripVerdict::Inconclusive {
            reason: "pair has zero reserves".into(),
        };
    }

    // Token balances slot (pair holds `reserve_token` of it).
    let token_slot = {
        let read = |key: U256| state.db.storage_ref(token, key).unwrap_or_default();
        match discover_balance_slot(read, pair, reserve_token, cfg.max_slot_probe) {
            Some(s) => s,
            None => {
                return RoundTripVerdict::Inconclusive {
                    reason: "token balance slot not found".into(),
                }
            }
        }
    };

    // Size the buy and compute the expected token output.
    let base_in = (reserve_base.saturating_mul(U256::from(cfg.test_fraction_bps))
        / U256::from(10_000u64))
    .max(U256::from(1u64));
    let amt_t_out = expected_amount_out(base_in, reserve_base, reserve_token, fee_bps);
    if amt_t_out.is_zero() {
        return RoundTripVerdict::Inconclusive {
            reason: "computed zero token-out for buy".into(),
        };
    }

    // Fund the pair with `base_in` of base (tax-free => override is faithful).
    let pair_base_key = erc20_balance_key(pair, base_slot);
    let pair_base_bal = state.db.storage_ref(base, pair_base_key).unwrap_or_default();
    let _ = state
        .db
        .insert_account_storage(base, pair_base_key, pair_base_bal.saturating_add(base_in));

    let RpcForkedState {
        db,
        block_number,
        block_timestamp,
        base_fee,
        chain_id,
    } = state;
    let block = BlockEnv {
        number: U256::from(block_number),
        timestamp: U256::from(block_timestamp),
        basefee: base_fee,
        ..Default::default()
    };

    // BUY: pair.swap(token out -> eoa).
    let (a0, a1) = if token_is_0 {
        (amt_t_out, U256::ZERO)
    } else {
        (U256::ZERO, amt_t_out)
    };
    let pre_t = read_balance(&db, token, eoa, token_slot);
    let (ok, reason, db) = v2_transact(
        db,
        &block,
        chain_id,
        base_fee,
        eoa,
        pair,
        encode_v2_swap(a0, a1, eoa),
        0,
        cfg.gas_limit,
    );
    if !ok {
        return RoundTripVerdict::Inconclusive {
            reason: format!("buy leg failed: {}", reason.unwrap_or_default()),
        };
    }
    let t_received = read_balance(&db, token, eoa, token_slot).saturating_sub(pre_t);
    if t_received.is_zero() {
        return RoundTripVerdict::Honeypot {
            reason: "buy returned zero tokens".into(),
        };
    }

    // SELL step 1: real transfer of received tokens into the pair (triggers any
    // fee-on-transfer / sell restriction).
    let mut transfer_data = Vec::with_capacity(68);
    transfer_data.extend_from_slice(&TRANSFER_SELECTOR);
    let mut to_word = [0u8; 32];
    to_word[12..32].copy_from_slice(pair.as_slice());
    transfer_data.extend_from_slice(&to_word);
    transfer_data.extend_from_slice(&t_received.to_be_bytes::<32>());
    let (ok_t, reason_t, db) = v2_transact(
        db,
        &block,
        chain_id,
        base_fee,
        eoa,
        token,
        transfer_data,
        1,
        cfg.gas_limit,
    );
    if !ok_t {
        return RoundTripVerdict::Honeypot {
            reason: format!("sell transfer to pair failed: {}", reason_t.unwrap_or_default()),
        };
    }

    // Fresh reserves (the buy swap called `_update`) + the pair's real token
    // balance after the transfer => the amount actually available to sell.
    let slot8b = db
        .storage_ref(pair, U256::from(V2_RESERVES_SLOT))
        .unwrap_or_default();
    let (r0b, r1b) = unpack_v2_reserves(slot8b);
    let (reserve_token_b, reserve_base_b) = if token_is_0 { (r0b, r1b) } else { (r1b, r0b) };
    let pair_token_bal = read_balance(&db, token, pair, token_slot);
    let amt_t_in = pair_token_bal.saturating_sub(reserve_token_b);
    let amt_base_out = expected_amount_out(amt_t_in, reserve_token_b, reserve_base_b, fee_bps);
    if amt_base_out.is_zero() {
        return RoundTripVerdict::Honeypot {
            reason: "pair received ~zero sellable tokens".into(),
        };
    }

    // SELL step 2: pair.swap(base out -> eoa).
    let base_is_0 = !token_is_0;
    let (sa0, sa1) = if base_is_0 {
        (amt_base_out, U256::ZERO)
    } else {
        (U256::ZERO, amt_base_out)
    };
    let pre_b = read_balance(&db, base, eoa, base_slot);
    let (ok_s, reason_s, db) = v2_transact(
        db,
        &block,
        chain_id,
        base_fee,
        eoa,
        pair,
        encode_v2_swap(sa0, sa1, eoa),
        2,
        cfg.gas_limit,
    );
    if !ok_s {
        return RoundTripVerdict::Honeypot {
            reason: format!("sell swap reverted: {}", reason_s.unwrap_or_default()),
        };
    }
    let base_out = read_balance(&db, base, eoa, base_slot).saturating_sub(pre_b);
    let verdict = classify_round_trip(base_in, base_out, fee_bps, true, max_loss_bps);
    debug!(%token, %pair, %base_in, %base_out, ?verdict, "v2 round-trip screen complete");
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn addr(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    #[test]
    fn balance_key_is_deterministic_and_distinct() {
        let h1 = addr(0x11);
        let h2 = addr(0x22);
        // Stable for the same (holder, slot).
        assert_eq!(
            erc20_balance_key(h1, U256::from(3u64)),
            erc20_balance_key(h1, U256::from(3u64))
        );
        // Different holder => different key.
        assert_ne!(
            erc20_balance_key(h1, U256::from(3u64)),
            erc20_balance_key(h2, U256::from(3u64))
        );
        // Different slot => different key.
        assert_ne!(
            erc20_balance_key(h1, U256::from(3u64)),
            erc20_balance_key(h1, U256::from(9u64))
        );
    }

    #[test]
    fn expected_amount_out_matches_canonical_v2() {
        // dy = dx*997*y / (x*1000 + dx*997), here via 10000-basis fee=30bps.
        // x=1000, y=1000, dx=100 -> 90.661... -> floor 90.
        let out = expected_amount_out(
            U256::from(100u64),
            U256::from(1000u64),
            U256::from(1000u64),
            30,
        );
        assert_eq!(out, U256::from(90u64));
        // Degenerate inputs return zero.
        assert_eq!(
            expected_amount_out(U256::ZERO, U256::from(1u64), U256::from(1u64), 30),
            U256::ZERO
        );
        assert_eq!(
            expected_amount_out(U256::from(1u64), U256::from(1u64), U256::from(1u64), 10_000),
            U256::ZERO
        );
    }

    #[test]
    fn classify_transfer_clean_when_full_received() {
        assert_eq!(
            classify_transfer(U256::from(1000u64), U256::from(1000u64), 10),
            FotVerdict::Clean {
                observed_tax_bps: 0
            }
        );
        // Received more than sent (reflection credit) is still clean.
        assert_eq!(
            classify_transfer(U256::from(1000u64), U256::from(1001u64), 10),
            FotVerdict::Clean {
                observed_tax_bps: 0
            }
        );
    }

    #[test]
    fn classify_transfer_tolerance_absorbs_rounding() {
        // 1000 -> 999 is 10 bps shortfall; with max_tax_bps=10 it's clean.
        assert_eq!(
            classify_transfer(U256::from(1000u64), U256::from(999u64), 10),
            FotVerdict::Clean {
                observed_tax_bps: 10
            }
        );
    }

    #[test]
    fn classify_transfer_flags_fee_on_transfer() {
        // 5% tax: 10000 -> 9500 = 500 bps > tolerance.
        assert_eq!(
            classify_transfer(U256::from(10_000u64), U256::from(9_500u64), 10),
            FotVerdict::FeeOnTransfer { tax_bps: 500 }
        );
    }

    #[test]
    fn admissible_only_for_clean() {
        assert!(FotVerdict::Clean {
            observed_tax_bps: 0
        }
        .is_admissible());
        assert!(!FotVerdict::FeeOnTransfer { tax_bps: 500 }.is_admissible());
        assert!(!FotVerdict::Honeypot {
            reason: "x".into()
        }
        .is_admissible());
        assert!(!FotVerdict::Inconclusive {
            reason: "x".into()
        }
        .is_admissible());
    }

    #[test]
    fn discover_balance_slot_finds_matching_slot() {
        let token_holder = addr(0xAB);
        let known = U256::from(123_456u64);
        // Fake storage: only slot 7's balances[holder] holds `known`.
        let mut store: HashMap<U256, U256> = HashMap::new();
        store.insert(erc20_balance_key(token_holder, U256::from(7u64)), known);
        let reader = |key: U256| *store.get(&key).unwrap_or(&U256::ZERO);

        assert_eq!(
            discover_balance_slot(reader, token_holder, known, 40),
            Some(U256::from(7u64))
        );
        // Wrong known-balance => no match.
        assert_eq!(
            discover_balance_slot(reader, token_holder, U256::from(999u64), 40),
            None
        );
        // Zero known-balance => None (cannot disambiguate).
        assert_eq!(discover_balance_slot(reader, token_holder, U256::ZERO, 40), None);
        // Slot beyond probe budget is not found.
        assert_eq!(discover_balance_slot(reader, token_holder, known, 7), None);
    }

    #[test]
    fn unpack_v2_reserves_splits_112_bit_halves() {
        let r0 = U256::from(1_000_000u64);
        let r1 = U256::from(2_500_000u64);
        let packed = r0 | (r1 << 112);
        assert_eq!(unpack_v2_reserves(packed), (r0, r1));
    }

    #[test]
    fn encode_v2_swap_layout() {
        let data = encode_v2_swap(U256::ZERO, U256::from(123u64), addr(0x42));
        assert_eq!(data.len(), 4 + 5 * 32);
        assert_eq!(&data[0..4], &[0x02, 0x2c, 0x0d, 0x9f]);
        // amount1Out occupies the second word; low byte == 123.
        assert_eq!(data[4 + 64 - 1], 123);
        // `to` lives in the low 20 bytes of the third word.
        assert_eq!(&data[4 + 64 + 12..4 + 96], addr(0x42).as_slice());
    }

    #[test]
    fn round_trip_honeypot_when_sell_fails() {
        let v = classify_round_trip(U256::from(1000u64), U256::ZERO, 30, false, 200);
        assert!(matches!(v, RoundTripVerdict::Honeypot { .. }));
        // Sell "succeeded" but recovered nothing is still a honeypot.
        let v2 = classify_round_trip(U256::from(1000u64), U256::ZERO, 30, true, 200);
        assert!(matches!(v2, RoundTripVerdict::Honeypot { .. }));
    }

    #[test]
    fn round_trip_clean_for_fee_only_loss() {
        // 0.3% pool: fee-only recovery = 0.997^2 = 0.994009 -> 9940 bps.
        // Recover 9930 bps (extra 10 bps slippage) with 200 bps tolerance -> Clean.
        let base_in = U256::from(1_000_000u64);
        let base_out = U256::from(993_000u64); // 9930 bps
        let v = classify_round_trip(base_in, base_out, 30, true, 200);
        assert!(matches!(v, RoundTripVerdict::Clean { .. }), "got {v:?}");
    }

    #[test]
    fn round_trip_flags_sell_tax() {
        // Recover only 85% — far below the ~99.4% fee-only expectation.
        let base_in = U256::from(1_000_000u64);
        let base_out = U256::from(850_000u64);
        let v = classify_round_trip(base_in, base_out, 30, true, 200);
        match v {
            RoundTripVerdict::FeeOnTransfer { loss_bps } => assert!(loss_bps > 200),
            other => panic!("expected FeeOnTransfer, got {other:?}"),
        }
    }

    // =======================================================================
    // RPC-backed integration tests (live mainnet fork). Gated on ETH_RPC_URL
    // and #[ignore]d, so default `cargo test` skips them. Run with:
    //   ETH_RPC_URL="https://eth-mainnet.g.alchemy.com/v2/<KEY>" \
    //     cargo test -p aether-simulator --lib fee_on_transfer -- --ignored
    // =======================================================================
    use crate::fork::RpcForkedState;
    use alloy::primitives::address;
    use alloy::providers::{Provider, ProviderBuilder};

    const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    const UNIV2_USDC_WETH: Address = address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc");
    const UNIV2_DAI_WETH: Address = address!("A478c2975Ab1Ea89e8196811F51A7B7Ade33eB11");

    /// Resolve the mainnet RPC URL from the process env, falling back to the
    /// repo `.env` (interpolating `${ALCHEMY_API_KEY}`). `None` => skip.
    fn resolve_rpc_url() -> Option<String> {
        if let Ok(url) = std::env::var("ETH_RPC_URL") {
            if !url.trim().is_empty() && !url.contains("${") {
                return Some(url);
            }
        }
        let env_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
        let contents = std::fs::read_to_string(env_path).ok()?;
        let mut alchemy_key: Option<String> = None;
        let mut raw_url: Option<String> = None;
        for line in contents.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("ALCHEMY_API_KEY=") {
                alchemy_key = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("ETH_RPC_URL=") {
                raw_url = Some(v.trim().to_string());
            }
        }
        let url = raw_url?;
        Some(match alchemy_key {
            Some(k) => url.replace("${ALCHEMY_API_KEY}", &k),
            None => url,
        })
    }

    /// Build a fresh forked state at `latest` and run the V2 round-trip screen.
    /// `base_slot` is `base`'s ERC20 balances mapping slot (WETH = 3).
    async fn screen(
        rpc_url: &str,
        pair: Address,
        token: Address,
        base: Address,
        base_slot: u64,
    ) -> RoundTripVerdict {
        let parsed: alloy::transports::http::reqwest::Url =
            rpc_url.parse().expect("valid RPC URL");
        let provider = ProviderBuilder::new().connect_http(parsed).erased();
        let latest = provider.get_block_number().await.expect("block number");
        // A fresh RpcForkedState is required per run (AlloyDB is !Clone).
        let state = RpcForkedState::new_at_latest(provider.clone(), latest, 4_000_000_000, 1_000_000_000)
            .expect("must run inside a multi-thread tokio runtime");
        // 0.1% probe, 3% round-trip loss tolerance to absorb fee + slippage.
        screen_token_v2_round_trip(
            state,
            pair,
            token,
            base,
            U256::from(base_slot),
            30,
            &FotConfig::default(),
            300,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ETH_RPC_URL (live mainnet fork)"]
    async fn round_trip_usdc_weth_is_clean() {
        let Some(rpc) = resolve_rpc_url() else {
            eprintln!("skipping: ETH_RPC_URL unset and repo .env unavailable");
            return;
        };
        // WETH is the base; its _balances slot is 3.
        let v = screen(&rpc, UNIV2_USDC_WETH, USDC, WETH, 3).await;
        assert!(v.is_admissible(), "USDC/WETH must screen Clean, got {v:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ETH_RPC_URL (live mainnet fork)"]
    async fn round_trip_dai_weth_is_clean() {
        let Some(rpc) = resolve_rpc_url() else {
            eprintln!("skipping: ETH_RPC_URL unset and repo .env unavailable");
            return;
        };
        let v = screen(&rpc, UNIV2_DAI_WETH, DAI, WETH, 3).await;
        assert!(v.is_admissible(), "DAI/WETH must screen Clean, got {v:?}");
    }

    /// Verifies the sell-side detection against a *known* fee-on-transfer or
    /// honeypot token. Driven by env so a possibly-stale address is never baked
    /// in as a hard assertion — set all three to run:
    ///   AETHER_FOT_TEST_PAIR=0x...   (UniswapV2 pair, token paired with WETH)
    ///   AETHER_FOT_TEST_TOKEN=0x...  (the suspect token)
    ///   AETHER_FOT_TEST_BASE_SLOT=3  (optional; WETH default 3)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ETH_RPC_URL + AETHER_FOT_TEST_{PAIR,TOKEN}"]
    async fn round_trip_flags_configured_fot_token() {
        let Some(rpc) = resolve_rpc_url() else {
            eprintln!("skipping: ETH_RPC_URL unavailable");
            return;
        };
        let (Ok(pair_s), Ok(token_s)) = (
            std::env::var("AETHER_FOT_TEST_PAIR"),
            std::env::var("AETHER_FOT_TEST_TOKEN"),
        ) else {
            eprintln!("skipping: AETHER_FOT_TEST_PAIR / AETHER_FOT_TEST_TOKEN unset");
            return;
        };
        let pair: Address = pair_s.parse().expect("valid pair address");
        let token: Address = token_s.parse().expect("valid token address");
        let base_slot: u64 = std::env::var("AETHER_FOT_TEST_BASE_SLOT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let v = screen(&rpc, pair, token, WETH, base_slot).await;
        assert!(
            !v.is_admissible(),
            "configured fee-on-transfer/honeypot token must NOT screen Clean, got {v:?}"
        );
    }
}
