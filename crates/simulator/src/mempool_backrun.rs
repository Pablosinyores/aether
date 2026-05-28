//! Mempool-backrun validation — apply a pending victim tx, then our arb tx,
//! against a forked EVM state. Returns an accept/reject decision plus the
//! gross ERC20 profit measured at our recipient address.
//!
//! Forks at the parent block of the slot we are targeting (the victim has
//! not yet mined). The first `transact_commit` applies the victim and
//! mutates the cache; the second runs our `executeArb` calldata and reads
//! the post-state ERC20 balance delta. Both txs must succeed for an accept.
//!
//! Caller is responsible for the AETHER_MEMPOOL_SIM_TIMEOUT_MS / concurrency
//! semaphore — those live in the gRPC server side because they require
//! tokio integration. This module is a synchronous pure function so it can
//! run on `spawn_blocking` workers without leaking async dependencies.

use alloy::primitives::{Address, Bytes, U256};
use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::database_interface::{Database, DatabaseRef};
use revm::handler::{ExecuteCommitEvm, ExecuteEvm, MainBuilder};
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode, EvmState};
use revm::Context;
use tracing::debug;

use crate::fork::{ForkedState, RpcForkedState};

/// Pending victim transaction reconstructed from an Alchemy
/// `alchemy_pendingTransactions` event. `nonce` is intentionally absent —
/// we always sim with `disable_nonce_check = true` because the subscription
/// stream does not carry it.
#[derive(Debug, Clone)]
pub struct VictimTx {
    pub from: Address,
    pub to: Address,
    pub value: U256,
    pub data: Vec<u8>,
    pub gas_price: u128,
    /// Per-tx gas limit. Override comes from `eth_getTransactionByHash` when
    /// the pipeline has time to fetch it; otherwise the pipeline passes the
    /// block gas limit so the victim has headroom to execute.
    pub gas_limit: u64,
}

/// Our backrun arbitrage transaction. `caller` is the searcher EOA the
/// pipeline reserves for sim runs (does not need ETH balance because we
/// disable the balance check); `to` is the `AetherExecutor` contract.
#[derive(Debug, Clone)]
pub struct ArbTx {
    pub caller: Address,
    pub to: Address,
    pub data: Vec<u8>,
    pub gas_limit: u64,
}

/// Reason a mempool-backrun validation attempt failed. Maps 1:1 onto the
/// `aether_mempool_backrun_rejected_total{reason}` label set in
/// `crates/grpc-server/src/metrics.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    VictimReverted,
    VictimHalted,
    ArbReverted,
    ArbHalted,
    NegativeAfterGas,
    SimError,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VictimReverted => "victim_reverted",
            Self::VictimHalted => "victim_halted",
            Self::ArbReverted => "arb_reverted",
            Self::ArbHalted => "arb_halted",
            Self::NegativeAfterGas => "negative_after_gas",
            Self::SimError => "sim_error",
        }
    }
}

/// Outcome of one mempool-backrun validation attempt.
#[derive(Debug, Clone)]
pub struct BackrunSimResult {
    /// `true` iff victim + arb both committed cleanly and net profit was
    /// positive after subtracting the EIP-1559 gas cost at `base_fee`.
    pub accepted: bool,
    /// Gross ERC20 balance delta of the recipient. Zero on reject.
    pub gross_profit_wei: U256,
    /// Gas used by the arb tx alone (victim gas is reported separately for
    /// observability but not counted against the searcher's bundle cost).
    pub arb_gas_used: u64,
    pub victim_gas_used: u64,
    /// Set when `accepted == false`.
    pub reject: Option<RejectReason>,
    /// First 4 bytes of the arb revert output (selector) when the arb leg
    /// reverted, so the pipeline can log it without holding the full output
    /// bytes. `None` on success.
    pub revert_selector: Option<[u8; 4]>,
}

impl BackrunSimResult {
    fn rejected(reason: RejectReason, victim_gas: u64, arb_gas: u64) -> Self {
        Self {
            accepted: false,
            gross_profit_wei: U256::ZERO,
            arb_gas_used: arb_gas,
            victim_gas_used: victim_gas,
            reject: Some(reason),
            revert_selector: None,
        }
    }
}

/// Inputs the validator needs that aren't part of the forked state itself.
#[derive(Debug, Clone)]
pub struct ValidatorParams {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee: u64,
    pub chain_id: u64,
    /// ERC20 token whose balance delta is treated as our profit. Almost
    /// always WETH for backruns that flashloan ETH.
    pub profit_token: Address,
    /// Address whose balance delta is measured. The `AetherExecutor`
    /// transfers profits to its `owner` cold wallet — pass that address.
    pub profit_recipient: Address,
    /// Storage slot of the ERC20 `_balances` mapping in `profit_token`
    /// (WETH = 3, USDC = 9, USDT/DAI = 2). The caller resolves this from
    /// the static `aether_common` token table.
    pub balance_slot: U256,
    /// Optional runtime bytecode to inject at `arb.to` before running the
    /// arb tx. Used by demo / shadow-mode runs against a forked mainnet
    /// where `AetherExecutor` has not been deployed: the pipeline loads
    /// the compiled runtime bytecode from `contracts/out/AetherExecutor.sol`
    /// and threads it through here so the revm sim's `executeArb` call
    /// hits real bytecode instead of empty-account revert. `None` for
    /// production runs where the contract is on-chain.
    pub executor_bytecode: Option<Bytes>,
}

/// Run the two-tx sim against an RPC-backed fork. Production entry point.
///
/// Consumes `RpcForkedState` because the underlying `AlloyDB` is `!Clone`;
/// the pipeline rebuilds a fresh `RpcForkedState` per attempt (cheap — the
/// provider is `Arc`-wrapped internally).
pub fn validate_backrun_rpc(
    state: RpcForkedState,
    victim: &VictimTx,
    arb: &ArbTx,
    params: &ValidatorParams,
) -> BackrunSimResult {
    let RpcForkedState {
        db,
        block_number: _,
        block_timestamp: _,
        base_fee: _,
        chain_id: _,
    } = state;
    validate_backrun_inner(db, victim, arb, params)
}

/// Run the two-tx sim against a synthetic `ForkedState`. Used by unit
/// tests that pre-populate balances + bytecode + storage without standing
/// up a full RPC fork.
pub fn validate_backrun_cache(
    state: ForkedState,
    victim: &VictimTx,
    arb: &ArbTx,
    params: &ValidatorParams,
) -> BackrunSimResult {
    validate_backrun_inner(state.db, victim, arb, params)
}

/// Core implementation generic over the backing `DatabaseRef`. Both
/// `RpcForkedState` (CacheDB<SyncAlloyDb>) and `ForkedState`
/// (CacheDB<EmptyDB>) flow through here.
fn validate_backrun_inner<DB>(
    mut db: CacheDB<DB>,
    victim: &VictimTx,
    arb: &ArbTx,
    params: &ValidatorParams,
) -> BackrunSimResult
where
    DB: DatabaseRef,
    CacheDB<DB>: Database<Error = <DB as DatabaseRef>::Error>,
    <DB as DatabaseRef>::Error: std::fmt::Debug,
{
    // Demo / shadow runs against a forked mainnet may target an
    // AetherExecutor address that is not yet deployed on-chain. The
    // pipeline threads the compiled runtime bytecode through
    // `params.executor_bytecode` so we can inject it into the CacheDB at
    // `arb.to`, making the subsequent `executeArb` call hit real bytecode
    // instead of empty-account revert. Production runs pass `None` and
    // the cache resolves the address via the forked DB.
    if let Some(code) = params.executor_bytecode.as_ref() {
        let bytecode = Bytecode::new_raw(code.clone());
        db.insert_account_info(
            arb.to,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                ..Default::default()
            },
        );
    }

    // Compute the storage key for balanceOf(profit_recipient):
    //   slot = keccak256(pad32(recipient) ++ pad32(balance_slot))
    let mut key_input = [0u8; 64];
    key_input[12..32].copy_from_slice(params.profit_recipient.as_slice());
    key_input[32..64].copy_from_slice(&params.balance_slot.to_be_bytes::<32>());
    let storage_key = U256::from_be_slice(
        alloy::primitives::keccak256(key_input).as_slice(),
    );

    // Read pre-sim recipient balance via DatabaseRef::storage_ref so it
    // serves from cache when warm, or triggers a single RPC fetch under
    // the RPC variant. Failures are treated as zero balance — same as the
    // existing `simulate_rpc_with_erc20_profit` behaviour.
    let pre_balance = db
        .storage_ref(params.profit_token, storage_key)
        .unwrap_or_default();

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

    // ── Apply victim tx ─────────────────────────────────────────────
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

    let (victim_gas_used, victim_ok) = match evm.transact_commit(victim_env) {
        Ok(ExecutionResult::Success { gas_used, .. }) => (gas_used, true),
        Ok(ExecutionResult::Revert { gas_used, output }) => {
            // Log the actual revert reason so dashboards / triage can tell
            // an allowance failure (TRANSFER_FROM_FAILED) apart from a
            // slippage failure (INSUFFICIENT_OUTPUT_AMOUNT) without having
            // to re-replay the tx by hand.
            let reason = decode_revert_reason(output.as_ref());
            let selector = revert_selector(output.as_ref());
            debug!(
                gas_used,
                reason = %reason,
                selector = ?selector,
                output_hex = %alloy::primitives::hex::encode(output.as_ref()),
                "mempool-backrun: victim reverted"
            );
            return BackrunSimResult::rejected(RejectReason::VictimReverted, gas_used, 0);
        }
        Ok(ExecutionResult::Halt { reason, gas_used }) => {
            debug!(?reason, gas_used, "mempool-backrun: victim halted");
            return BackrunSimResult::rejected(RejectReason::VictimHalted, gas_used, 0);
        }
        Err(e) => {
            debug!(error = ?e, "mempool-backrun: victim sim error");
            return BackrunSimResult::rejected(RejectReason::SimError, 0, 0);
        }
    };

    // ── Apply our arb tx (non-committing, we only need the result) ──
    let arb_env = TxEnv::builder()
        .caller(arb.caller)
        .kind(revm::primitives::TxKind::Call(arb.to))
        .data(revm::primitives::Bytes::copy_from_slice(&arb.data))
        .value(U256::ZERO)
        .gas_limit(arb.gas_limit)
        .gas_price(params.base_fee as u128)
        .nonce(0)
        .chain_id(Some(params.chain_id))
        .build_fill();

    match evm.transact(arb_env) {
        Ok(rs) => match rs.result {
            ExecutionResult::Success { gas_used, .. } => {
                let post_balance = read_post_balance(
                    &rs.state,
                    params.profit_token,
                    storage_key,
                    pre_balance,
                );
                let gross = post_balance.saturating_sub(pre_balance);

                // Net-after-gas check at the sim base fee. This is a coarse
                // floor (the executor will price the actual gas separately
                // with priority fee tuning), but rejecting obvious losers
                // here saves a publish + a downstream pipeline step.
                let gas_cost_wei = U256::from(gas_used).saturating_mul(U256::from(params.base_fee));
                if gross <= gas_cost_wei {
                    debug!(
                        %gross,
                        %gas_cost_wei,
                        "mempool-backrun: gross profit does not cover gas at sim base fee"
                    );
                    return BackrunSimResult::rejected(
                        RejectReason::NegativeAfterGas,
                        victim_gas_used,
                        gas_used,
                    );
                }

                debug!(gas_used, %gross, "mempool-backrun: arb leg accepted");
                if !victim_ok {
                    return BackrunSimResult::rejected(
                        RejectReason::SimError,
                        victim_gas_used,
                        gas_used,
                    );
                }
                BackrunSimResult {
                    accepted: true,
                    gross_profit_wei: gross,
                    arb_gas_used: gas_used,
                    victim_gas_used,
                    reject: None,
                    revert_selector: None,
                }
            }
            ExecutionResult::Revert { gas_used, output } => {
                let selector = revert_selector(&output);
                let reason = decode_revert_reason(&output);
                debug!(
                    gas_used,
                    reason = %reason,
                    ?selector,
                    "mempool-backrun: arb leg reverted"
                );
                BackrunSimResult {
                    accepted: false,
                    gross_profit_wei: U256::ZERO,
                    arb_gas_used: gas_used,
                    victim_gas_used,
                    reject: Some(RejectReason::ArbReverted),
                    revert_selector: Some(selector),
                }
            }
            ExecutionResult::Halt { reason, gas_used } => {
                debug!(?reason, gas_used, "mempool-backrun: arb leg halted");
                BackrunSimResult::rejected(RejectReason::ArbHalted, victim_gas_used, gas_used)
            }
        },
        Err(e) => {
            debug!(error = ?e, "mempool-backrun: arb sim error");
            BackrunSimResult::rejected(RejectReason::SimError, victim_gas_used, 0)
        }
    }
}

fn read_post_balance(
    state: &EvmState,
    token: Address,
    storage_key: U256,
    fallback: U256,
) -> U256 {
    state
        .get(&token)
        .and_then(|acc| acc.storage.get(&storage_key))
        .map(|slot| slot.present_value)
        .unwrap_or(fallback)
}

fn revert_selector(output: &[u8]) -> [u8; 4] {
    let mut sel = [0u8; 4];
    let n = output.len().min(4);
    sel[..n].copy_from_slice(&output[..n]);
    sel
}

/// Decode a revert payload into a human-readable reason. Recognises the two
/// Solidity-standard shapes (`Error(string)` and `Panic(uint256)`); anything
/// else surfaces as a hex selector so the log keeps a stable signal even
/// when third-party DEXes ship custom errors. Caller is responsible for
/// hex-encoding overlong unknown payloads — we cap to keep log lines bounded.
pub fn decode_revert_reason(output: &[u8]) -> String {
    if output.is_empty() {
        return "empty_revert".to_string();
    }
    // Solidity `Error(string)` — `0x08c379a0` + ABI-encoded string.
    // Layout: selector(4) || offset(32) || length(32) || data(N, padded).
    if output.len() >= 4 + 32 + 32 && output[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
        let len_word = &output[4 + 32..4 + 32 + 32];
        // Length lives in the low 4 bytes; anything wider is malformed.
        let len = u32::from_be_bytes([len_word[28], len_word[29], len_word[30], len_word[31]])
            as usize;
        let start = 4 + 32 + 32;
        if len > 0 && start + len <= output.len() {
            if let Ok(s) = std::str::from_utf8(&output[start..start + len]) {
                return s.to_string();
            }
        }
    }
    // Solidity `Panic(uint256)` — `0x4e487b71` + 32-byte panic code.
    if output.len() >= 4 + 32 && output[0..4] == [0x4e, 0x48, 0x7b, 0x71] {
        let code_word = &output[4..36];
        let code = u32::from_be_bytes([
            code_word[28],
            code_word[29],
            code_word[30],
            code_word[31],
        ]);
        return format!("Panic(0x{:02x})", code);
    }
    // Custom error — surface the 4-byte selector. Caller can grep against a
    // signature DB (4byte.directory etc) to recover the human form.
    if output.len() >= 4 {
        return format!(
            "custom_error_0x{}",
            alloy::primitives::hex::encode(&output[0..4])
        );
    }
    format!(
        "short_revert_0x{}",
        alloy::primitives::hex::encode(output)
    )
}

/// Convenience for unit tests that only need an `EmptyDB`-backed cache.
#[doc(hidden)]
pub fn empty_cache_db() -> CacheDB<EmptyDB> {
    CacheDB::new(EmptyDB::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const WETH: Address = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    const RECIPIENT: Address = address!("1111111111111111111111111111111111111111");
    const VICTIM_FROM: Address = address!("2222222222222222222222222222222222222222");
    const VICTIM_TO: Address = address!("3333333333333333333333333333333333333333");
    const ARB_TO: Address = address!("4444444444444444444444444444444444444444");
    const ARB_CALLER: Address = address!("5555555555555555555555555555555555555555");

    fn default_params() -> ValidatorParams {
        ValidatorParams {
            block_number: 18_000_000,
            block_timestamp: 1_700_000_000,
            base_fee: 1_000_000_000, // 1 gwei
            chain_id: 1,
            profit_token: WETH,
            profit_recipient: RECIPIENT,
            balance_slot: U256::from(3u64), // WETH balances slot
            executor_bytecode: None,
        }
    }

    fn default_victim() -> VictimTx {
        VictimTx {
            from: VICTIM_FROM,
            to: VICTIM_TO,
            value: U256::ZERO,
            data: vec![],
            gas_price: 2_000_000_000,
            gas_limit: 100_000,
        }
    }

    fn default_arb() -> ArbTx {
        ArbTx {
            caller: ARB_CALLER,
            to: ARB_TO,
            data: vec![],
            gas_limit: 200_000,
        }
    }

    #[test]
    fn victim_with_no_code_succeeds_and_arb_with_no_code_succeeds_zero_profit() {
        // Both legs call EOAs (no code) with empty data → both succeed,
        // gross_profit = 0 → NegativeAfterGas reject (correct: zero profit
        // never beats zero gas at any non-zero base fee).
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &default_params());
        assert!(!result.accepted, "zero-profit must reject");
        assert_eq!(result.reject, Some(RejectReason::NegativeAfterGas));
        assert_eq!(result.gross_profit_wei, U256::ZERO);
    }

    #[test]
    fn victim_revert_short_circuits_arb() {
        // INVALID opcode at the victim's `to` makes the victim leg revert
        // via Halt; arb leg must not execute and reject reason is the
        // victim's, not the arb's.
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account(VICTIM_TO, U256::ZERO, vec![0xfe].into()); // INVALID
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &default_params());
        assert!(!result.accepted);
        // INVALID is an unknown opcode → Halt in revm.
        assert_eq!(result.reject, Some(RejectReason::VictimHalted));
        assert_eq!(result.arb_gas_used, 0, "arb must not have executed");
    }

    #[test]
    fn arb_revert_after_clean_victim_rejects_with_arb_reverted_reason() {
        // Victim is an EOA (succeeds). Arb target contains REVERT opcode:
        //   PUSH1 0x00 PUSH1 0x00 REVERT  → reverts with empty output
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account(ARB_TO, U256::ZERO, vec![0x60, 0x00, 0x60, 0x00, 0xfd].into());
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &default_params());
        assert!(!result.accepted);
        assert_eq!(result.reject, Some(RejectReason::ArbReverted));
        assert_eq!(result.victim_gas_used, 21000, "victim consumed base tx gas");
        assert!(result.arb_gas_used > 0, "arb leg actually executed");
    }

    #[test]
    fn arb_halt_propagates_as_arb_halted() {
        // Arb target is INVALID opcode → Halt.
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        state.insert_account(ARB_TO, U256::ZERO, vec![0xfe].into());
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &default_params());
        assert!(!result.accepted);
        assert_eq!(result.reject, Some(RejectReason::ArbHalted));
    }

    #[test]
    fn reject_reason_label_matches_metric_label_set() {
        // Defensive: the metric helper takes &str and we want the
        // RejectReason::as_str() values to stay in sync with the
        // documented metric label set. If anyone changes one without the
        // other, this asserts the contract.
        assert_eq!(RejectReason::VictimReverted.as_str(), "victim_reverted");
        assert_eq!(RejectReason::VictimHalted.as_str(), "victim_halted");
        assert_eq!(RejectReason::ArbReverted.as_str(), "arb_reverted");
        assert_eq!(RejectReason::ArbHalted.as_str(), "arb_halted");
        assert_eq!(RejectReason::NegativeAfterGas.as_str(), "negative_after_gas");
        assert_eq!(RejectReason::SimError.as_str(), "sim_error");
    }

    #[test]
    fn empty_cache_db_is_usable_for_construction() {
        // Sanity: the doc-hidden helper actually returns something the
        // generic core accepts. Used by downstream pipeline tests.
        let _: CacheDB<EmptyDB> = empty_cache_db();
    }

    #[test]
    fn executor_bytecode_injection_makes_arb_to_execute_real_code() {
        // Demo-mode override: no contract at ARB_TO on the forked DB.
        // Without injection the arb call hits empty bytecode → succeeds
        // with zero profit → NegativeAfterGas (covered by the
        // `victim_with_no_code_succeeds...` test above).
        //
        // With `executor_bytecode = Some(REVERT)`, the same call now hits
        // the injected REVERT opcode and the validator must propagate
        // ArbReverted instead of NegativeAfterGas — proving the bytecode
        // was actually installed at ARB_TO and the EVM dispatched to it.
        let state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        let mut params = default_params();
        // PUSH1 0x00 PUSH1 0x00 REVERT — minimal explicit-revert program.
        params.executor_bytecode = Some(Bytes::from(vec![0x60, 0x00, 0x60, 0x00, 0xfd]));
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &params);
        assert!(!result.accepted);
        assert_eq!(
            result.reject,
            Some(RejectReason::ArbReverted),
            "injected REVERT must propagate as ArbReverted, not NegativeAfterGas"
        );
        assert!(
            result.arb_gas_used > 0,
            "arb leg must have actually executed the injected bytecode"
        );
    }

    /// `Error(string)` revert from a UniV2 router's TransferHelper.
    /// Real on-chain shape: selector 0x08c379a0, offset=0x20, len=32,
    /// then the ASCII string. Verifies the decoder unwraps to the original
    /// human-readable reason without losing any chars.
    #[test]
    fn decode_revert_reason_error_string_uniswap_v2_transfer_helper() {
        // Real on-chain string the UniV2 router emits. 36 chars — exercises
        // the ABI-padding branch since 36 % 32 != 0.
        let msg: &[u8] = b"TransferHelper::TRANSFER_FROM_FAILED";
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0x08, 0xc3, 0x79, 0xa0]); // selector
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        payload.extend_from_slice(&offset); // offset = 32
        let mut length = [0u8; 32];
        length[31] = msg.len() as u8;
        payload.extend_from_slice(&length);
        payload.extend_from_slice(msg);
        // ABI padding to 32-byte boundary
        let pad = 32 - (msg.len() % 32);
        if pad != 32 {
            payload.extend(std::iter::repeat_n(0u8, pad));
        }
        let got = decode_revert_reason(&payload);
        assert_eq!(got, "TransferHelper::TRANSFER_FROM_FAILED");
    }

    /// `Panic(uint256)` with code 0x11 (arithmetic over/underflow). Common
    /// in Curve invariant math when a malformed swap is replayed.
    #[test]
    fn decode_revert_reason_panic_uint256() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0x4e, 0x48, 0x7b, 0x71]); // selector
        let mut code = [0u8; 32];
        code[31] = 0x11;
        payload.extend_from_slice(&code);
        assert_eq!(decode_revert_reason(&payload), "Panic(0x11)");
    }

    /// Unknown 4-byte custom error selector must surface as hex so triage
    /// can grep against 4byte.directory. Use a real UniV3 selector to make
    /// the test more obviously meaningful: `0x7939f424` = `TLU(int24,int24)`.
    #[test]
    fn decode_revert_reason_custom_error_falls_back_to_selector_hex() {
        let payload = [0x79, 0x39, 0xf4, 0x24];
        assert_eq!(decode_revert_reason(&payload), "custom_error_0x7939f424");
    }

    /// Empty revert (no return data) is a real on-chain shape — `require()`
    /// with no message string produces this. Must round-trip cleanly to a
    /// non-panicking marker rather than indexing into a zero-length slice.
    #[test]
    fn decode_revert_reason_empty_payload() {
        assert_eq!(decode_revert_reason(&[]), "empty_revert");
    }

    #[test]
    fn executor_bytecode_none_preserves_pre_existing_arb_to_state() {
        // When `executor_bytecode = None`, the injection branch is skipped
        // and any bytecode already at ARB_TO on the forked DB is left
        // untouched. This is the production path: the cache's on-chain
        // bytecode is the source of truth.
        let mut state = ForkedState::new_empty(18_000_000, 1_700_000_000, 0);
        // Pre-populate ARB_TO with INVALID opcode → ArbHalted on call.
        state.insert_account(ARB_TO, U256::ZERO, vec![0xfe].into());
        let params = default_params(); // executor_bytecode = None
        let result = validate_backrun_cache(state, &default_victim(), &default_arb(), &params);
        assert!(!result.accepted);
        assert_eq!(
            result.reject,
            Some(RejectReason::ArbHalted),
            "pre-existing INVALID at ARB_TO must drive the reject reason"
        );
    }
}
