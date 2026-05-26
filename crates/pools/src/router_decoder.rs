//! Pending-tx calldata decoder for known DEX routers.
//!
//! Maps a raw `(to, calldata)` pair from a pending transaction to a
//! protocol-tagged [`DecodedSwap`] when the call selector matches one of the
//! supported router shapes. Anything we don't recognise returns
//! [`DecodeError::UnknownSelector`] so the caller can bump a decode-failure
//! metric and move on without taking the engine down.
//!
//! ## Coverage in this scaffold
//!
//! - **UniswapV2 Router02** and **SushiSwap Router02** share an ABI: we decode
//!   the family of `swapExact*` / `swap*ForExact*` calls, extracting the
//!   first hop only (the rest of the path is recoverable downstream).
//! - **UniswapV3 SwapRouter** and **SwapRouter02**: `exactInputSingle` and
//!   `exactInput` (multi-hop bytes-encoded path).
//! - **Balancer V2 Vault**: `swap(SingleSwap, FundManagement, limit, deadline)`
//!   single-pool variant.
//!
//! Out of scope (returns `UnknownSelector`):
//!
//! - Curve router — its `exchange` / `exchange_multiple` shape varies per
//!   pool registry version and would inflate the decoder without yielding
//!   reliable hits in the testing scaffold.
//! - 1inch v6 AggregationRouter — multi-encoded calldata; deferred so the
//!   `decode_failure` counter can quantify the gap before we invest.
//!
//! ## Multicall (UniV3 SwapRouter / SwapRouter02)
//!
//! Both the original `multicall(bytes[])` (selector `0xac9650d8`) and the
//! deadline-prefixed `multicall(uint256,bytes[])` (selector `0x5ae401dc`)
//! peel each inner call through [`decode_pending_many`] and emit one
//! [`DecodedSwap`] per recognised inner selector. Inner non-swap helpers
//! (`selfPermit*`, `unwrapWETH9`, `refundETH`, `sweepToken*`) are intentionally
//! ignored — they're legal payload but don't move pool state. Recursion
//! depth is capped at [`MAX_MULTICALL_DEPTH`] (2) so a hostile nested
//! `multicall(multicall(...))` cannot blow the stack or quadratically
//! expand decode work on the hot path.
//!
//! Every decoded swap is paired with a [`Protocol`] tag so downstream
//! simulators can route to the right post-state computation.

use alloy::primitives::{address, Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

/// Curve Router addresses we knowingly cannot decode yet.
///
/// Curve's `exchange` / `exchange_multiple` selectors vary per pool registry
/// version and the calldata shape is too divergent to handle in the scaffold.
/// We still want pending txs to these routers in the address filter (so the
/// firehose stays representative of real router traffic), but unknown-selector
/// errors against them otherwise drown out genuine decoder gaps in
/// other protocols. Marking them up-front lets the caller bump a dedicated
/// `curve_unsupported` reason instead of `unknown_selector`.
const CURVE_ROUTERS: &[Address] =
    &[address!("99a58482BD75cbab83b27EC03CA68fF489b5788f")];

/// Returns `true` when `router` is a known Curve router that the decoder
/// cannot parse. Caller should short-circuit with a dedicated metric.
pub fn is_unsupported_curve_router(router: Address) -> bool {
    CURVE_ROUTERS.contains(&router)
}

/// Routers that share the UniswapV2 ABI but should be tagged as SushiSwap.
///
/// SushiSwap forked Router02 verbatim, so its calldata is byte-for-byte
/// indistinguishable from UniswapV2's at the selector layer; the only signal
/// the decoder has is the `to` address. Without this dispatch every Sushi
/// pending tx falls into the UniswapV2 metric label and the registry lookup
/// hunts in the wrong protocol's pool set.
///
/// Update this list when adding a new Sushi-flavoured router (e.g. SushiX,
/// Sushi RouteProcessor) — adding the address here is the only change
/// required for correct protocol attribution downstream.
const SUSHISWAP_ROUTERS: &[Address] =
    &[address!("d9e1cE17f2641f24aE83637ab66a2cca9C378B9F")];

fn router_to_v2_protocol(router: Address) -> Protocol {
    if SUSHISWAP_ROUTERS.contains(&router) {
        Protocol::SushiSwap
    } else {
        Protocol::UniswapV2
    }
}

/// Protocol tag attached to every successful decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    UniswapV2,
    UniswapV3,
    SushiSwap,
    BalancerV2,
    /// Curve StableSwap pool. Unlike the AMM families above, Curve swaps
    /// call `exchange()` **directly on the pool address** (no router
    /// indirection); `DecodedSwap.router` therefore carries the *pool*
    /// address and the upstream pipeline must resolve token_in / token_out
    /// from the pool registry using the indices in
    /// [`DecodedSwap.curve_indices`].
    Curve,
    /// Bancor V3 `BancorNetwork.tradeBySourceAmount`. Unlike Curve, this
    /// is router-mediated (not pool-direct), so `DecodedSwap.router` is
    /// the BancorNetwork address and `token_in`/`token_out` are resolved
    /// from calldata directly.
    BancorV3,
}

/// Minimal swap shape produced by the decoder.
///
/// Multi-hop paths (V2 chained, V3 `exactInput`) collapse to first-hop
/// fields here; the full path is preserved in `path_extra` for callers that
/// need it (post-state simulation reapplies the full hop list).
#[derive(Debug, Clone)]
pub struct DecodedSwap {
    pub protocol: Protocol,
    /// Router address the tx is calling — useful for metric labelling.
    pub router: Address,
    /// First-hop input token.
    pub token_in: Address,
    /// First-hop output token (or final token for V3 `exactInputSingle`).
    pub token_out: Address,
    /// Amount of `token_in` the user is committing.
    pub amount_in: U256,
    /// Minimum `token_out` required by user for slippage protection.
    pub amount_out_min: U256,
    /// Recipient (`to`) the swap will pay out to.
    pub recipient: Address,
    /// Pool fee in hundredths-of-a-bp (V3) or `0` for non-V3 protocols.
    pub fee_bps: u32,
    /// Remaining path tokens past the first hop, in order. Empty for
    /// single-hop swaps.
    pub path_extra: Vec<Address>,
    /// Curve-only: the `(i, j)` token indices from `exchange()` calldata.
    /// `None` for non-Curve protocols. `token_in` / `token_out` are
    /// emitted as `Address::ZERO` for Curve because the decoder cannot
    /// resolve indices to addresses without the pool's coin list — the
    /// pipeline does that via the pool registry. Index width is `u8`
    /// because mainnet Curve pools are ≤ 8 coins, so `i128` source
    /// values that overflow `u8` indicate a malformed or non-standard
    /// pool and the decoder rejects them with `AbiDecode`.
    pub curve_indices: Option<(u8, u8)>,
}

/// Reasons a pending tx might fail to decode. Caller maps these to a
/// `decode_failure` counter; the variants are intentionally fine-grained so
/// dashboards can show *why* coverage is low.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecodeError {
    #[error("calldata too short for any selector")]
    TooShort,
    #[error("unknown selector {selector:?}")]
    UnknownSelector { selector: [u8; 4] },
    #[error("known selector but ABI decode failed: {0}")]
    AbiDecode(String),
    #[error("path is empty or malformed")]
    EmptyPath,
    /// Recipient is a Curve router but the decoder does not yet support
    /// Curve's `exchange` / `exchange_multiple` shapes. Distinct from
    /// `UnknownSelector` so the `curve_unsupported` metric reason isolates
    /// the known gap from genuine unmapped selectors elsewhere.
    #[error("curve router {0} not yet supported by decoder")]
    CurveUnsupported(Address),
}

// ── Router ABIs ──
//
// Selectors are computed at compile time via the `sol!` macro. Only the
// methods we actually decode are listed; the rest are intentionally absent
// so an unsupported variant fails the selector lookup loudly.

sol! {
    /// UniswapV2 / SushiSwap Router02 surface — they share the ABI.
    /// Includes the fee-on-transfer variants because meme-token routing
    /// dominates the live mempool and the non-FOT shapes alone produce a
    /// near-zero decode hit rate against real Alchemy traffic.
    #[allow(missing_docs)]
    interface IUniswapV2Router02 {
        function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
        function swapTokensForExactTokens(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) external;
        function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable;
        function swapTokensForExactETH(uint256 amountOut, uint256 amountInMax, address[] path, address to, uint256 deadline) external;
        function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
        function swapETHForExactTokens(uint256 amountOut, address[] path, address to, uint256 deadline) external payable;
        function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
        function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline) external payable;
        function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline) external;
    }

    /// UniswapV3 SwapRouter (deadline) and SwapRouter02 (no deadline) flavours.
    /// The structs carry distinct selectors because of the deadline field
    /// shift, so we declare both and try each.
    #[allow(missing_docs)]
    interface IUniswapV3Router {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactInputSingleParams02 {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactInputParams {
            bytes path;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
        }
        struct ExactInputParams02 {
            bytes path;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
        }
        struct ExactOutputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactOutputSingleParams02 {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountOut;
            uint256 amountInMaximum;
            uint160 sqrtPriceLimitX96;
        }
        struct ExactOutputParams {
            bytes path;
            address recipient;
            uint256 deadline;
            uint256 amountOut;
            uint256 amountInMaximum;
        }
        struct ExactOutputParams02 {
            bytes path;
            address recipient;
            uint256 amountOut;
            uint256 amountInMaximum;
        }
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256);
        function exactInputSingle02(ExactInputSingleParams02 params) external payable returns (uint256);
        function exactInput(ExactInputParams params) external payable returns (uint256);
        function exactInput02(ExactInputParams02 params) external payable returns (uint256);
        function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256);
        function exactOutputSingle02(ExactOutputSingleParams02 params) external payable returns (uint256);
        function exactOutput(ExactOutputParams params) external payable returns (uint256);
        function exactOutput02(ExactOutputParams02 params) external payable returns (uint256);

        /// Non-swap helpers that legally appear inside `multicall(bytes[])`
        /// payloads. We only need their selectors so the inner-call
        /// dispatcher can skip them without bumping `unknown_selector` for
        /// otherwise well-formed multicalls. Bodies are intentionally
        /// minimal — we never decode the args.
        function selfPermit(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external payable;
        function selfPermitAllowed(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) external payable;
        function selfPermitIfNecessary(address token, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external payable;
        function selfPermitAllowedIfNecessary(address token, uint256 nonce, uint256 expiry, uint8 v, bytes32 r, bytes32 s) external payable;
        function unwrapWETH9(uint256 amountMinimum, address recipient) external payable;
        function unwrapWETH9WithFee(uint256 amountMinimum, address recipient, uint256 feeBips, address feeRecipient) external payable;
        function refundETH() external payable;
        function sweepToken(address token, uint256 amountMinimum, address recipient) external payable;
        function sweepTokenWithFee(address token, uint256 amountMinimum, address recipient, uint256 feeBips, address feeRecipient) external payable;
    }

    /// UniV3 SwapRouter / SwapRouter02 multicall flavours. Split into two
    /// single-method interfaces because both Solidity functions are named
    /// `multicall` (the selector differs only in the prepended `deadline`
    /// arg); splitting keeps the generated Rust types unambiguous and lets
    /// each selector's dispatch line stay one-liner.
    #[allow(missing_docs)]
    interface IUniswapV3Multicall {
        function multicall(bytes[] data) external payable returns (bytes[]);
    }
    #[allow(missing_docs)]
    interface IUniswapV3MulticallDeadline {
        function multicall(uint256 deadline, bytes[] data) external payable returns (bytes[]);
    }

    /// Curve StableSwap pool — direct `exchange()` on the pool address.
    /// The original Curve interface uses `int128` indices; newer
    /// crypto-pool variants use `uint256`. Two interfaces because the
    /// `sol!` macro can't generate two `exchange` functions with the same
    /// Rust name from one interface; splitting also keeps each selector's
    /// selector dispatch obvious. `exchange_underlying` (lending-pool
    /// wrappers like Compound cTokens) shares the int128 signature with
    /// a distinct selector and is decoded identically — we only care
    /// about the indices + amount.
    #[allow(missing_docs)]
    interface ICurvePoolInt128 {
        function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) external returns (uint256);
        function exchange_underlying(int128 i, int128 j, uint256 dx, uint256 min_dy) external returns (uint256);
    }
    #[allow(missing_docs)]
    interface ICurvePoolUint256 {
        function exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy) external returns (uint256);
    }

    /// Bancor V3 `BancorNetwork`. The dominant pending-tx shape is
    /// `tradeBySourceAmount(source, target, sourceAmount, minReturn,
    /// deadline, beneficiary)` — the user commits a known source amount
    /// and accepts ≥ minReturn. The complementary `tradeByTargetAmount`
    /// shape (commit a target amount, pay ≤ maxSource) is decoded with a
    /// parallel arm so both flavours land in the same `DecodedSwap` shape.
    #[allow(missing_docs)]
    interface IBancorNetwork {
        function tradeBySourceAmount(address sourceToken, address targetToken, uint256 sourceAmount, uint256 minReturnAmount, uint256 deadline, address beneficiary) external payable returns (uint256);
        function tradeByTargetAmount(address sourceToken, address targetToken, uint256 targetAmount, uint256 maxSourceAmount, uint256 deadline, address beneficiary) external payable returns (uint256);
    }

    /// Balancer V2 Vault `swap` for the SingleSwap shape.
    #[allow(missing_docs)]
    interface IBalancerVault {
        struct SingleSwap {
            bytes32 poolId;
            uint8 kind;
            address assetIn;
            address assetOut;
            uint256 amount;
            bytes userData;
        }
        struct FundManagement {
            address sender;
            bool fromInternalBalance;
            address recipient;
            bool toInternalBalance;
        }
        function swap(SingleSwap singleSwap, FundManagement funds, uint256 limit, uint256 deadline) external payable returns (uint256);
    }
}

/// Maximum recursion depth for `multicall(bytes[])` peeling.
///
/// Two is enough to cover every shape seen on mainnet (router-level
/// multicall around per-hop `exactInput*` calls; occasional `multicall`
/// nested one level under another). A hostile sender could otherwise
/// chain `multicall(multicall(multicall(...)))` to amplify decode work
/// quadratically per pending tx; capping the depth keeps the per-event
/// budget bounded.
pub const MAX_MULTICALL_DEPTH: usize = 2;

/// Decode a pending tx's `(to, calldata)` into a [`DecodedSwap`].
///
/// `to` is required: anonymous calls (contract creation) always return
/// [`DecodeError::TooShort`]. The caller is expected to filter by router
/// address before calling — this function does not validate that the `to`
/// matches a known router; it only consumes the selector + payload.
///
/// **Single-swap entry point.** Callers that need to peel UniV3
/// `multicall(bytes[])` bundles should use [`decode_pending_many`]; this
/// function returns [`DecodeError::UnknownSelector`] for the two multicall
/// selectors so existing call sites keep their previous semantics. The
/// multicall dispatch is documented in the [module docs](self).
pub fn decode_pending(to: Address, calldata: &[u8]) -> Result<DecodedSwap, DecodeError> {
    if calldata.len() < 4 {
        return Err(DecodeError::TooShort);
    }
    // Short-circuit known Curve routers before selector dispatch so they map
    // to a dedicated reason instead of inflating `unknown_selector`. They
    // remain in the address filter because dropping them would skew the
    // firehose's protocol mix away from real router traffic.
    if is_unsupported_curve_router(to) {
        return Err(DecodeError::CurveUnsupported(to));
    }
    let selector: [u8; 4] = calldata[0..4].try_into().expect("4 bytes by check above");

    if let Some(swap) = try_decode_single(selector, calldata, to)? {
        return Ok(swap);
    }

    Err(DecodeError::UnknownSelector { selector })
}

/// Decode a pending tx's `(to, calldata)` into one or more [`DecodedSwap`]
/// records.
///
/// Behaves identically to [`decode_pending`] for single-swap selectors
/// (`exactInputSingle`, `swapExactTokensForTokens`, etc.) — returns a
/// one-element vector wrapping the same `DecodedSwap`. For UniV3
/// `multicall(bytes[])` (selector `0xac9650d8`) and
/// `multicall(uint256, bytes[])` (selector `0x5ae401dc`), peels each
/// inner call and emits one record per recognised swap selector. Inner
/// non-swap helpers (`selfPermit*`, `unwrapWETH9*`, `refundETH`,
/// `sweepToken*`) are intentionally ignored — they're legal inside
/// multicall but don't change pool state.
///
/// Recursion depth is capped at [`MAX_MULTICALL_DEPTH`]; nested multicalls
/// past the cap surface as `UnknownSelector` so the metric is bumped and
/// the engine moves on.
///
/// **Error semantics:** an unknown inner selector inside a multicall is
/// *ignored*, not propagated — the wrapping multicall is a successful
/// decode even if one inner call was unrecognised. Only outer-level
/// unknown selectors (no swap at all) produce [`DecodeError::UnknownSelector`].
/// If the outer call is a multicall but contains zero recognised inner
/// swaps, returns `Ok(vec![])` so the caller can decide whether to count
/// it as a decode hit or skip silently.
pub fn decode_pending_many(
    to: Address,
    calldata: &[u8],
) -> Result<Vec<DecodedSwap>, DecodeError> {
    if calldata.len() < 4 {
        return Err(DecodeError::TooShort);
    }
    if is_unsupported_curve_router(to) {
        return Err(DecodeError::CurveUnsupported(to));
    }
    decode_at_depth(to, calldata, 0)
}

/// Recursive worker for [`decode_pending_many`]. The `depth` argument is
/// incremented every time we step inside a `multicall(bytes[])` payload;
/// once it reaches [`MAX_MULTICALL_DEPTH`] further nested multicalls are
/// reported as `UnknownSelector` so they're visible in the metric and the
/// recursion can't blow the stack.
fn decode_at_depth(
    to: Address,
    calldata: &[u8],
    depth: usize,
) -> Result<Vec<DecodedSwap>, DecodeError> {
    if calldata.len() < 4 {
        return Err(DecodeError::TooShort);
    }
    let selector: [u8; 4] = calldata[0..4].try_into().expect("4 bytes by check above");

    // ── UniV3 multicall(bytes[]) and multicall(uint256, bytes[]) ──
    if depth < MAX_MULTICALL_DEPTH {
        if let Some(inner_calls) = try_extract_multicall(selector, calldata)? {
            let mut out: Vec<DecodedSwap> = Vec::with_capacity(inner_calls.len());
            for inner in inner_calls {
                // Inner calls inherit the outer multicall's `to` (the
                // SwapRouter), which is exactly what the per-hop swap
                // semantics expect — the recipient/router address stays
                // the same as if the user had called the inner method
                // directly.
                if inner.len() < 4 {
                    // Empty / truncated inner call — skip without erroring.
                    // Multicall payloads occasionally include zero-byte
                    // padding entries; treating those as decode failures
                    // would distort the unknown_selector metric.
                    continue;
                }
                match decode_at_depth(to, &inner, depth + 1) {
                    Ok(mut swaps) => out.append(&mut swaps),
                    // Inner-call decode failures are intentionally swallowed:
                    // legal multicall payloads include non-swap helpers
                    // (selfPermit, unwrapWETH9, refundETH, sweepToken) plus
                    // anything we haven't taught the decoder yet. Surfacing
                    // those as outer errors would mislabel real-world UniV3
                    // multicall traffic as broken. The cap depth check
                    // above prevents an attacker from amplifying decode
                    // work via nested multicalls.
                    Err(_) => continue,
                }
            }
            return Ok(out);
        }
    }

    if let Some(swap) = try_decode_single(selector, calldata, to)? {
        return Ok(vec![swap]);
    }
    if try_is_known_non_swap_helper(selector) {
        // Legal inside multicall, no swap to emit.
        return Ok(vec![]);
    }
    Err(DecodeError::UnknownSelector { selector })
}

/// Try every single-call decoder family. Returns `None` when no family
/// claims the selector; returns `Ok(Some(_))` on success and propagates
/// ABI-level errors via `Err(...)`.
fn try_decode_single(
    selector: [u8; 4],
    calldata: &[u8],
    to: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    if let Some(swap) = try_uni_v2_family(selector, calldata, to)? {
        return Ok(Some(swap));
    }
    if let Some(swap) = try_uni_v3_family(selector, calldata, to)? {
        return Ok(Some(swap));
    }
    if let Some(swap) = try_balancer(selector, calldata, to)? {
        return Ok(Some(swap));
    }
    if let Some(swap) = try_curve(selector, calldata, to)? {
        return Ok(Some(swap));
    }
    if let Some(swap) = try_bancor(selector, calldata, to)? {
        return Ok(Some(swap));
    }
    Ok(None)
}

/// Detect a UniV3 multicall selector and return the inner `bytes[]` array.
/// Returns `Ok(None)` when the selector isn't a multicall — caller falls
/// through to single-swap decoding.
fn try_extract_multicall(
    selector: [u8; 4],
    calldata: &[u8],
) -> Result<Option<Vec<Vec<u8>>>, DecodeError> {
    if selector == IUniswapV3Multicall::multicallCall::SELECTOR {
        let c = IUniswapV3Multicall::multicallCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(c.data.into_iter().map(|b| b.to_vec()).collect()));
    }
    if selector == IUniswapV3MulticallDeadline::multicallCall::SELECTOR {
        let c = IUniswapV3MulticallDeadline::multicallCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(c.data.into_iter().map(|b| b.to_vec()).collect()));
    }
    Ok(None)
}

/// Recognise an inner-call selector that's legal inside a UniV3 multicall
/// but doesn't move pool state (`selfPermit*`, `unwrapWETH9*`, `refundETH`,
/// `sweepToken*`). Returning `true` here lets the multicall dispatcher
/// silently skip the helper instead of bumping `unknown_selector` for a
/// well-formed payload.
fn try_is_known_non_swap_helper(selector: [u8; 4]) -> bool {
    use IUniswapV3Router::*;
    selector == selfPermitCall::SELECTOR
        || selector == selfPermitAllowedCall::SELECTOR
        || selector == selfPermitIfNecessaryCall::SELECTOR
        || selector == selfPermitAllowedIfNecessaryCall::SELECTOR
        || selector == unwrapWETH9Call::SELECTOR
        || selector == unwrapWETH9WithFeeCall::SELECTOR
        || selector == refundETHCall::SELECTOR
        || selector == sweepTokenCall::SELECTOR
        || selector == sweepTokenWithFeeCall::SELECTOR
}

fn try_uni_v2_family(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use IUniswapV2Router02::*;
    if selector == swapExactTokensForTokensCall::SELECTOR {
        let c = swapExactTokensForTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountIn,
            c.amountOutMin,
            c.to,
        )?));
    }
    if selector == swapTokensForExactTokensCall::SELECTOR {
        let c = swapTokensForExactTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountInMax,
            c.amountOut,
            c.to,
        )?));
    }
    if selector == swapExactETHForTokensCall::SELECTOR {
        let c = swapExactETHForTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            U256::ZERO, // amount_in carried as msg.value, unknown from calldata alone
            c.amountOutMin,
            c.to,
        )?));
    }
    if selector == swapExactTokensForETHCall::SELECTOR {
        let c = swapExactTokensForETHCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountIn,
            c.amountOutMin,
            c.to,
        )?));
    }
    if selector == swapTokensForExactETHCall::SELECTOR {
        let c = swapTokensForExactETHCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountInMax,
            c.amountOut,
            c.to,
        )?));
    }
    if selector == swapETHForExactTokensCall::SELECTOR {
        let c = swapETHForExactTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            U256::ZERO,
            c.amountOut,
            c.to,
        )?));
    }
    if selector == swapExactTokensForTokensSupportingFeeOnTransferTokensCall::SELECTOR {
        let c = swapExactTokensForTokensSupportingFeeOnTransferTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountIn,
            c.amountOutMin,
            c.to,
        )?));
    }
    if selector == swapExactETHForTokensSupportingFeeOnTransferTokensCall::SELECTOR {
        let c = swapExactETHForTokensSupportingFeeOnTransferTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            U256::ZERO,
            c.amountOutMin,
            c.to,
        )?));
    }
    if selector == swapExactTokensForETHSupportingFeeOnTransferTokensCall::SELECTOR {
        let c = swapExactTokensForETHSupportingFeeOnTransferTokensCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(decode_v2_call(
            router,
            c.path,
            c.amountIn,
            c.amountOutMin,
            c.to,
        )?));
    }
    Ok(None)
}

fn decode_v2_call(
    router: Address,
    path: Vec<Address>,
    amount_in: U256,
    amount_out_min: U256,
    to: Address,
) -> Result<DecodedSwap, DecodeError> {
    if path.len() < 2 {
        return Err(DecodeError::EmptyPath);
    }
    let token_in = path[0];
    let token_out = path[1];
    let path_extra = path.iter().skip(2).copied().collect();
    Ok(DecodedSwap {
        protocol: router_to_v2_protocol(router),
        router,
        token_in,
        token_out,
        amount_in,
        amount_out_min,
        recipient: to,
        fee_bps: 0,
        path_extra,
        curve_indices: None,
    })
}

fn try_uni_v3_family(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use IUniswapV3Router::*;
    if selector == exactInputSingleCall::SELECTOR {
        let c = exactInputSingleCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in: c.params.tokenIn,
            token_out: c.params.tokenOut,
            amount_in: c.params.amountIn,
            amount_out_min: c.params.amountOutMinimum,
            recipient: c.params.recipient,
            fee_bps: c.params.fee.to::<u32>(),
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    if selector == exactInputSingle02Call::SELECTOR {
        let c = exactInputSingle02Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in: c.params.tokenIn,
            token_out: c.params.tokenOut,
            amount_in: c.params.amountIn,
            amount_out_min: c.params.amountOutMinimum,
            recipient: c.params.recipient,
            fee_bps: c.params.fee.to::<u32>(),
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    if selector == exactInputCall::SELECTOR {
        let c = exactInputCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let (token_in, token_out, fee, extras) = parse_v3_path(&c.params.path)?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in,
            token_out,
            amount_in: c.params.amountIn,
            amount_out_min: c.params.amountOutMinimum,
            recipient: c.params.recipient,
            fee_bps: fee,
            path_extra: extras,
            curve_indices: None,
        }));
    }
    if selector == exactInput02Call::SELECTOR {
        let c = exactInput02Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let (token_in, token_out, fee, extras) = parse_v3_path(&c.params.path)?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in,
            token_out,
            amount_in: c.params.amountIn,
            amount_out_min: c.params.amountOutMinimum,
            recipient: c.params.recipient,
            fee_bps: fee,
            path_extra: extras,
            curve_indices: None,
        }));
    }
    // ── exact-output flavours ──
    //
    // For predictor purposes the amount the user *commits* is the output
    // amount (`amountOut`), and the per-hop max spend (`amountInMaximum`)
    // is surfaced as `amount_out_min`. The pre-state simulator can still
    // reason about the affected pool because `(token_in, token_out)` are
    // unambiguous; downstream maths that needs `amount_in` directly should
    // recompute via the V3 quoter (single-call exactOutput is rare on
    // mainnet — most flow goes through exactInput).
    if selector == exactOutputSingleCall::SELECTOR {
        let c = exactOutputSingleCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in: c.params.tokenIn,
            token_out: c.params.tokenOut,
            amount_in: c.params.amountOut,
            amount_out_min: c.params.amountInMaximum,
            recipient: c.params.recipient,
            fee_bps: c.params.fee.to::<u32>(),
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    if selector == exactOutputSingle02Call::SELECTOR {
        let c = exactOutputSingle02Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in: c.params.tokenIn,
            token_out: c.params.tokenOut,
            amount_in: c.params.amountOut,
            amount_out_min: c.params.amountInMaximum,
            recipient: c.params.recipient,
            fee_bps: c.params.fee.to::<u32>(),
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    if selector == exactOutputCall::SELECTOR {
        let c = exactOutputCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        // exactOutput paths are encoded in reverse: token_out first, then
        // intermediate tokens, with token_in last. `parse_v3_path` returns
        // them in encoding order, so for the predictor's `(token_in,
        // token_out)` semantics we flip the first two.
        let (path_first, path_second, fee, extras) = parse_v3_path(&c.params.path)?;
        let (token_in, token_out) = swap_path_first_hop_for_exact_output(path_first, path_second, &extras);
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in,
            token_out,
            amount_in: c.params.amountOut,
            amount_out_min: c.params.amountInMaximum,
            recipient: c.params.recipient,
            fee_bps: fee,
            path_extra: extras,
            curve_indices: None,
        }));
    }
    if selector == exactOutput02Call::SELECTOR {
        let c = exactOutput02Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let (path_first, path_second, fee, extras) = parse_v3_path(&c.params.path)?;
        let (token_in, token_out) = swap_path_first_hop_for_exact_output(path_first, path_second, &extras);
        return Ok(Some(DecodedSwap {
            protocol: Protocol::UniswapV3,
            router,
            token_in,
            token_out,
            amount_in: c.params.amountOut,
            amount_out_min: c.params.amountInMaximum,
            recipient: c.params.recipient,
            fee_bps: fee,
            path_extra: extras,
            curve_indices: None,
        }));
    }
    Ok(None)
}

/// Flip the first-hop `(token_in, token_out)` reported by
/// [`parse_v3_path`] for `exactOutput*` calldata. The V3 path encoding is
/// the same byte layout (`addr | fee | addr | ...`) but semantically
/// reversed for exact-output: the first 20 bytes are `tokenOut`. We let
/// `parse_v3_path` do the byte-walk (so the fee + extras logic stays in
/// one place) and rebind the labels here.
///
/// For single-hop exact-output (no `extras`), the first two parsed
/// addresses are `(tokenOut, tokenIn)`. For multi-hop, the parsed `extras`
/// still reads in path order — callers that need the full path can
/// reconstruct it; the predictor only consumes the affected first hop
/// from the user's perspective so the swap-token logic is sufficient.
fn swap_path_first_hop_for_exact_output(
    path_first: Address,
    path_second: Address,
    _extras: &[Address],
) -> (Address, Address) {
    // path_first = tokenOut (in encoding order), path_second = next token.
    // For the affected-pool semantics we want (token_in, token_out) so the
    // pool registry lookup hits the same canonical pair.
    (path_second, path_first)
}

/// Decode a UniV3 packed path: `address(20) | fee(3) | address(20) | fee(3) | ... | address(20)`.
///
/// Returns `(token_in, token_out_first, fee_first_hop, [remaining tokens])`.
fn parse_v3_path(path: &[u8]) -> Result<(Address, Address, u32, Vec<Address>), DecodeError> {
    const ADDR_LEN: usize = 20;
    const FEE_LEN: usize = 3;
    const HOP_LEN: usize = ADDR_LEN + FEE_LEN;

    // Minimum well-formed path: token_in | fee | token_out = 43 bytes.
    if path.len() < HOP_LEN + ADDR_LEN {
        return Err(DecodeError::EmptyPath);
    }

    let token_in = Address::from_slice(&path[0..ADDR_LEN]);
    let fee_bytes = &path[ADDR_LEN..ADDR_LEN + FEE_LEN];
    let fee = (u32::from(fee_bytes[0]) << 16)
        | (u32::from(fee_bytes[1]) << 8)
        | u32::from(fee_bytes[2]);

    let mut tokens: Vec<Address> = Vec::new();
    let mut cursor = ADDR_LEN + FEE_LEN;
    while cursor + ADDR_LEN <= path.len() {
        tokens.push(Address::from_slice(&path[cursor..cursor + ADDR_LEN]));
        cursor += ADDR_LEN;
        // Skip the next fee chunk if there are more tokens to follow.
        if cursor + FEE_LEN < path.len() {
            cursor += FEE_LEN;
        }
    }
    if tokens.is_empty() {
        return Err(DecodeError::EmptyPath);
    }
    let token_out = tokens.remove(0);
    Ok((token_in, token_out, fee, tokens))
}

fn try_balancer(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use IBalancerVault::*;
    if selector == swapCall::SELECTOR {
        let c = swapCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::BalancerV2,
            router,
            token_in: c.singleSwap.assetIn,
            token_out: c.singleSwap.assetOut,
            amount_in: c.singleSwap.amount,
            amount_out_min: c.limit,
            recipient: c.funds.recipient,
            fee_bps: 0,
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    Ok(None)
}

/// Curve pool-direct `exchange` decoder. Covers the int128 (original) and
/// uint256 (crypto-pool) shapes plus `exchange_underlying` (lending-pool
/// wrappers like Compound cToken / Aave aToken Curve pools). All three
/// share the `(i, j, dx, min_dy)` arg shape so they collapse to a single
/// `DecodedSwap` body.
///
/// `to` is the pool address, not a router — Curve's design is direct
/// pool calls. Token addresses can't be resolved here without the pool's
/// coin list; `token_in`/`token_out` are left as `Address::ZERO` and the
/// upstream pipeline resolves them via `pool_registry[to].token0/1` keyed
/// off the `curve_indices` we emit.
fn try_curve(
    selector: [u8; 4],
    calldata: &[u8],
    pool: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    // int128 variants first — much more common on mainnet (every
    // stablecoin StableSwap pool).
    if selector == ICurvePoolInt128::exchangeCall::SELECTOR
        || selector == ICurvePoolInt128::exchange_underlyingCall::SELECTOR
    {
        // Both calls have identical layouts, so decode either.
        let (i, j, dx) = if selector == ICurvePoolInt128::exchangeCall::SELECTOR {
            let c = ICurvePoolInt128::exchangeCall::abi_decode(calldata)
                .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            (c.i, c.j, c.dx)
        } else {
            let c = ICurvePoolInt128::exchange_underlyingCall::abi_decode(calldata)
                .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            (c.i, c.j, c.dx)
        };
        let indices = curve_indices_from_int128(i, j)?;
        return Ok(Some(curve_decoded_swap(pool, indices, dx)));
    }
    // uint256 variant (newer crypto pools, e.g. tricrypto).
    if selector == ICurvePoolUint256::exchangeCall::SELECTOR {
        let c = ICurvePoolUint256::exchangeCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let indices = curve_indices_from_uint256(c.i, c.j)?;
        return Ok(Some(curve_decoded_swap(pool, indices, c.dx)));
    }
    Ok(None)
}

/// Bancor V3 `BancorNetwork` decoder.
///
/// Routes both `tradeBySourceAmount` (commit source, accept min target)
/// and `tradeByTargetAmount` (commit target, pay max source). The two
/// shapes carry the same `(source, target)` token pair so collapse to a
/// single `DecodedSwap` body — the `amount_in` / `amount_out_min`
/// semantics differ slightly (`tradeByTargetAmount` puts the user-
/// committed *target* amount in `amount_in` and the *source ceiling* in
/// `amount_out_min`), but both numbers are still actionable signal for
/// the upstream post-state simulator.
fn try_bancor(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use IBancorNetwork::*;
    if selector == tradeBySourceAmountCall::SELECTOR {
        let c = tradeBySourceAmountCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::BancorV3,
            router,
            token_in: c.sourceToken,
            token_out: c.targetToken,
            amount_in: c.sourceAmount,
            amount_out_min: c.minReturnAmount,
            recipient: c.beneficiary,
            fee_bps: 0,
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    if selector == tradeByTargetAmountCall::SELECTOR {
        let c = tradeByTargetAmountCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(DecodedSwap {
            protocol: Protocol::BancorV3,
            router,
            token_in: c.sourceToken,
            token_out: c.targetToken,
            // tradeByTargetAmount commits the target amount; surface it
            // as amount_in so the predictor has a real magnitude rather
            // than zero. The user-paid source ceiling lives in
            // amount_out_min for symmetry.
            amount_in: c.targetAmount,
            amount_out_min: c.maxSourceAmount,
            recipient: c.beneficiary,
            fee_bps: 0,
            path_extra: vec![],
            curve_indices: None,
        }));
    }
    Ok(None)
}

/// Narrow Curve `int128` index args to `u8`. Mainnet Curve pools are
/// ≤ 8 coins so a value that doesn't fit `u8` (negative, > 255) is
/// either malformed calldata or a non-standard pool we shouldn't be
/// publishing; reject as `AbiDecode` so the decode-failure metric
/// surfaces it. The Solidity ABI decoder returns Rust `i128` for
/// `int128`, not alloy's `I256`.
fn curve_indices_from_int128(i: i128, j: i128) -> Result<(u8, u8), DecodeError> {
    let to_u8 = |v: i128| -> Result<u8, DecodeError> {
        if v < 0 || v > u8::MAX as i128 {
            return Err(DecodeError::AbiDecode(format!("curve index out of u8 range: {v}")));
        }
        Ok(v as u8)
    };
    Ok((to_u8(i)?, to_u8(j)?))
}

/// Narrow Curve `uint256` index args to `u8`. Same rationale as the
/// `int128` variant — sane mainnet values fit, anything else gets
/// rejected loudly.
fn curve_indices_from_uint256(i: U256, j: U256) -> Result<(u8, u8), DecodeError> {
    let to_u8 = |v: U256| -> Result<u8, DecodeError> {
        if v > U256::from(u8::MAX) {
            return Err(DecodeError::AbiDecode(format!("curve index out of u8 range: {v}")));
        }
        Ok(v.to::<u64>() as u8)
    };
    Ok((to_u8(i)?, to_u8(j)?))
}

/// Build the `DecodedSwap` shell for a Curve exchange. Token addresses
/// stay `Address::ZERO`; the pipeline must resolve them via the pool
/// registry using `curve_indices`. `recipient` is the tx sender (Curve
/// pays the caller directly), but we don't have `from` in this scope so
/// leave as ZERO — the field is unused on the Curve path.
fn curve_decoded_swap(pool: Address, indices: (u8, u8), amount_in: U256) -> DecodedSwap {
    DecodedSwap {
        protocol: Protocol::Curve,
        router: pool,
        token_in: Address::ZERO,
        token_out: Address::ZERO,
        amount_in,
        amount_out_min: U256::ZERO,
        recipient: Address::ZERO,
        fee_bps: 0,
        path_extra: vec![],
        curve_indices: Some(indices),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, U256};

    #[test]
    fn too_short_calldata_rejected() {
        let to = Address::ZERO;
        let err = decode_pending(to, &[0x12, 0x34]).unwrap_err();
        assert!(matches!(err, DecodeError::TooShort));
    }

    #[test]
    fn unknown_selector_returned_for_random_bytes() {
        let to = Address::ZERO;
        // 4-byte selector + 32 bytes of payload.
        let mut data = vec![0xde, 0xad, 0xbe, 0xef];
        data.extend(std::iter::repeat_n(0u8, 32));
        let err = decode_pending(to, &data).unwrap_err();
        match err {
            DecodeError::UnknownSelector { selector } => {
                assert_eq!(selector, [0xde, 0xad, 0xbe, 0xef]);
            }
            other => panic!("expected UnknownSelector, got {:?}", other),
        }
    }

    #[test]
    fn decode_uniswap_v2_swap_exact_tokens_for_tokens() {
        use IUniswapV2Router02::swapExactTokensForTokensCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let to_recipient = address!("000000000000000000000000000000000000dEaD");
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let amount_out_min = U256::from(2_500_000_000u128); // 2500 USDC (6dp)
        let path = vec![weth, usdc];
        let deadline = U256::from(99_999_999_999u64);

        let calldata = swapExactTokensForTokensCall {
            amountIn: amount_in,
            amountOutMin: amount_out_min,
            path: path.clone(),
            to: to_recipient,
            deadline,
        }
        .abi_encode();

        let router = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let decoded = decode_pending(router, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::UniswapV2);
        assert_eq!(decoded.router, router);
        assert_eq!(decoded.token_in, weth);
        assert_eq!(decoded.token_out, usdc);
        assert_eq!(decoded.amount_in, amount_in);
        assert_eq!(decoded.amount_out_min, amount_out_min);
        assert_eq!(decoded.recipient, to_recipient);
        assert_eq!(decoded.fee_bps, 0);
        assert!(decoded.path_extra.is_empty());
    }

    #[test]
    fn decode_uniswap_v3_exact_input_single_with_deadline() {
        use IUniswapV3Router::{exactInputSingleCall, ExactInputSingleParams};
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let recip = address!("000000000000000000000000000000000000dEaD");

        let params = ExactInputSingleParams {
            tokenIn: weth,
            tokenOut: usdc,
            fee: alloy::primitives::aliases::U24::from(3000), // 30 bps
            recipient: recip,
            deadline: U256::from(99u64),
            amountIn: U256::from(2_000u64),
            amountOutMinimum: U256::from(1_000u64),
            sqrtPriceLimitX96: alloy::primitives::U160::ZERO,
        };
        let calldata = exactInputSingleCall { params }.abi_encode();
        let router = address!("E592427A0AEce92De3Edee1F18E0157C05861564");
        let decoded = decode_pending(router, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::UniswapV3);
        assert_eq!(decoded.token_in, weth);
        assert_eq!(decoded.token_out, usdc);
        assert_eq!(decoded.fee_bps, 3000);
        assert_eq!(decoded.amount_in, U256::from(2_000u64));
    }

    #[test]
    fn parse_v3_path_extracts_first_hop_and_extras() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");

        // Build path: WETH | 3000 | USDC | 500 | DAI (43 + 3 + 20 = 66 bytes).
        let mut path = Vec::new();
        path.extend_from_slice(weth.as_slice());
        path.extend_from_slice(&[0x00, 0x0b, 0xb8]); // 3000
        path.extend_from_slice(usdc.as_slice());
        path.extend_from_slice(&[0x00, 0x01, 0xf4]); // 500
        path.extend_from_slice(dai.as_slice());

        let (token_in, token_out, fee, extras) = parse_v3_path(&path).expect("parse");
        assert_eq!(token_in, weth);
        assert_eq!(token_out, usdc);
        assert_eq!(fee, 3000);
        assert_eq!(extras, vec![dai]);
    }

    #[test]
    fn parse_v3_path_rejects_too_short() {
        let res = parse_v3_path(&[0u8; 10]);
        assert!(matches!(res, Err(DecodeError::EmptyPath)));
    }

    #[test]
    fn decode_sushiswap_router_tagged_as_sushi_not_uni_v2() {
        use IUniswapV2Router02::swapExactTokensForTokensCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let calldata = swapExactTokensForTokensCall {
            amountIn: U256::from(1_000u64),
            amountOutMin: U256::from(900u64),
            path: vec![weth, usdc],
            to: Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let sushi_router = address!("d9e1cE17f2641f24aE83637ab66a2cca9C378B9F");
        let decoded = decode_pending(sushi_router, &calldata).expect("decode");
        assert_eq!(
            decoded.protocol,
            Protocol::SushiSwap,
            "Sushi Router02 must dispatch to Protocol::SushiSwap so registry \
             lookups hit the Sushi pool set, not UniV2's"
        );
        assert_eq!(decoded.router, sushi_router);
    }

    #[test]
    fn decode_uni_v2_router_still_tagged_as_uni_v2() {
        use IUniswapV2Router02::swapExactTokensForTokensCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let calldata = swapExactTokensForTokensCall {
            amountIn: U256::from(1u64),
            amountOutMin: U256::from(0u64),
            path: vec![weth, usdc],
            to: Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let uni_v2_router = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let decoded = decode_pending(uni_v2_router, &calldata).expect("decode");
        assert_eq!(decoded.protocol, Protocol::UniswapV2);
    }

    #[test]
    fn empty_v2_path_rejected() {
        use IUniswapV2Router02::swapExactTokensForTokensCall;
        let calldata = swapExactTokensForTokensCall {
            amountIn: U256::from(1u64),
            amountOutMin: U256::from(0u64),
            path: vec![], // intentionally empty
            to: Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let err = decode_pending(Address::ZERO, &calldata).unwrap_err();
        assert!(matches!(err, DecodeError::EmptyPath));
    }

    // ── Curve pool-direct decoder ──

    #[test]
    fn decode_curve_exchange_int128_happy_path() {
        // 3pool address; the decoder doesn't validate this is a real
        // Curve pool, the upstream pool_registry filter does.
        let pool = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");
        let calldata = ICurvePoolInt128::exchangeCall {
            i: 0,
            j: 1,
            dx: U256::from(1_000_000u64),    // 1 USDC
            min_dy: U256::from(990_000u64),
        }
        .abi_encode();
        let decoded = decode_pending(pool, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::Curve);
        assert_eq!(decoded.router, pool);
        assert_eq!(decoded.token_in, Address::ZERO, "Curve must leave token addrs unresolved");
        assert_eq!(decoded.token_out, Address::ZERO);
        assert_eq!(decoded.amount_in, U256::from(1_000_000u64));
        assert_eq!(decoded.curve_indices, Some((0, 1)));
    }

    #[test]
    fn decode_curve_exchange_underlying_int128() {
        let pool = address!("0000000000000000000000000000000000000000");
        let calldata = ICurvePoolInt128::exchange_underlyingCall {
            i: 2,
            j: 0,
            dx: U256::from(50_000u64),
            min_dy: U256::from(49_500u64),
        }
        .abi_encode();
        let decoded = decode_pending(pool, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::Curve);
        assert_eq!(decoded.curve_indices, Some((2, 0)));
    }

    #[test]
    fn decode_curve_exchange_uint256() {
        // Newer crypto-pool variant uses uint256 indices.
        let pool = address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"); // tricrypto2
        let calldata = ICurvePoolUint256::exchangeCall {
            i: U256::from(1u64),
            j: U256::from(2u64),
            dx: U256::from(1_000_000_000_000_000_000u128),
            min_dy: U256::from(900u64),
        }
        .abi_encode();
        let decoded = decode_pending(pool, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::Curve);
        assert_eq!(decoded.curve_indices, Some((1, 2)));
    }

    #[test]
    fn decode_curve_negative_index_rejected() {
        // A negative `int128` index is either malformed calldata or a
        // shape the decoder shouldn't be guessing at; reject loudly.
        let pool = address!("0000000000000000000000000000000000000001");
        let calldata = ICurvePoolInt128::exchangeCall {
            i: -1,
            j: 0,
            dx: U256::from(1u64),
            min_dy: U256::from(0u64),
        }
        .abi_encode();
        let err = decode_pending(pool, &calldata).unwrap_err();
        assert!(matches!(err, DecodeError::AbiDecode(_)),
            "negative index should reject as AbiDecode, got {err:?}");
    }

    #[test]
    fn decode_curve_index_above_u8_rejected() {
        // uint256 index above 255 wouldn't fit our u8 narrowing.
        let pool = address!("0000000000000000000000000000000000000002");
        let calldata = ICurvePoolUint256::exchangeCall {
            i: U256::from(256u64),
            j: U256::from(0u64),
            dx: U256::from(1u64),
            min_dy: U256::from(0u64),
        }
        .abi_encode();
        let err = decode_pending(pool, &calldata).unwrap_err();
        assert!(matches!(err, DecodeError::AbiDecode(_)),
            "index above u8::MAX should reject as AbiDecode, got {err:?}");
    }

    // ── Bancor V3 BancorNetwork decoder ──

    #[test]
    fn decode_bancor_trade_by_source_amount() {
        let bancor = address!("eEF417e1D5CC832e619ae18D2F140De2999dD4fB");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bnt = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");
        let beneficiary = address!("000000000000000000000000000000000000dEaD");
        let calldata = IBancorNetwork::tradeBySourceAmountCall {
            sourceToken: weth,
            targetToken: bnt,
            sourceAmount: U256::from(1_000_000_000_000_000_000u128), // 1 WETH
            minReturnAmount: U256::from(2_500u64 * 10u64.pow(18)),    // 2500 BNT
            deadline: U256::from(99_999_999_999u64),
            beneficiary,
        }
        .abi_encode();
        let decoded = decode_pending(bancor, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::BancorV3);
        assert_eq!(decoded.router, bancor);
        assert_eq!(decoded.token_in, weth);
        assert_eq!(decoded.token_out, bnt);
        assert_eq!(decoded.amount_in, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(decoded.amount_out_min, U256::from(2_500u64 * 10u64.pow(18)));
        assert_eq!(decoded.recipient, beneficiary);
        assert_eq!(decoded.fee_bps, 0);
        assert!(decoded.path_extra.is_empty());
    }

    // ── UniV3 multicall(bytes[]) decoder ──

    /// Helper: build an `exactInputSingle` (deadline flavour) inner call so
    /// the multicall tests don't need to inline the `sol!` boilerplate at
    /// every call site.
    fn build_exact_input_single(
        token_in: Address,
        token_out: Address,
        amount_in: U256,
        recipient: Address,
    ) -> Vec<u8> {
        use IUniswapV3Router::{exactInputSingleCall, ExactInputSingleParams};
        let params = ExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            fee: alloy::primitives::aliases::U24::from(3000),
            recipient,
            deadline: U256::from(1u64),
            amountIn: amount_in,
            amountOutMinimum: U256::from(1u64),
            sqrtPriceLimitX96: alloy::primitives::U160::ZERO,
        };
        exactInputSingleCall { params }.abi_encode()
    }

    fn univ3_router() -> Address {
        address!("E592427A0AEce92De3Edee1F18E0157C05861564")
    }

    #[test]
    fn decode_multicall_two_exact_input_single_emits_two_swaps() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
        let recip = address!("000000000000000000000000000000000000dEaD");
        let inner_a = build_exact_input_single(weth, usdc, U256::from(1_000u64), recip);
        let inner_b = build_exact_input_single(usdc, dai, U256::from(2_000u64), recip);
        let calldata = IUniswapV3Multicall::multicallCall {
            data: vec![inner_a.into(), inner_b.into()],
        }
        .abi_encode();

        let swaps = decode_pending_many(univ3_router(), &calldata).expect("multicall decodes");
        assert_eq!(swaps.len(), 2, "two exactInputSingle calls = two DecodedSwap records");
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
        assert_eq!(swaps[0].token_in, weth);
        assert_eq!(swaps[0].token_out, usdc);
        assert_eq!(swaps[0].amount_in, U256::from(1_000u64));
        assert_eq!(swaps[1].token_in, usdc);
        assert_eq!(swaps[1].token_out, dai);
        assert_eq!(swaps[1].amount_in, U256::from(2_000u64));
    }

    #[test]
    fn decode_multicall_with_deadline_variant_also_peels() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let inner = build_exact_input_single(weth, usdc, U256::from(42u64), Address::ZERO);
        let calldata = IUniswapV3MulticallDeadline::multicallCall {
            deadline: U256::from(99u64),
            data: vec![inner.into()],
        }
        .abi_encode();
        let swaps = decode_pending_many(univ3_router(), &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].amount_in, U256::from(42u64));
    }

    #[test]
    fn decode_multicall_one_swap_one_non_swap_helper_emits_one_swap() {
        use IUniswapV3Router::unwrapWETH9Call;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let inner_swap = build_exact_input_single(weth, usdc, U256::from(1u64), Address::ZERO);
        let inner_unwrap = unwrapWETH9Call {
            amountMinimum: U256::ZERO,
            recipient: Address::ZERO,
        }
        .abi_encode();
        let calldata = IUniswapV3Multicall::multicallCall {
            data: vec![inner_swap.into(), inner_unwrap.into()],
        }
        .abi_encode();
        let swaps = decode_pending_many(univ3_router(), &calldata).expect("decode");
        assert_eq!(swaps.len(), 1, "unwrapWETH9 is ignored, only the swap remains");
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
    }

    #[test]
    fn decode_empty_multicall_returns_empty_vec() {
        let calldata = IUniswapV3Multicall::multicallCall { data: vec![] }.abi_encode();
        let swaps = decode_pending_many(univ3_router(), &calldata).expect("decode");
        assert!(swaps.is_empty());
    }

    #[test]
    fn decode_multicall_with_unknown_inner_selector_skips_silently() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let good = build_exact_input_single(weth, usdc, U256::from(1u64), Address::ZERO);
        // Garbage inner: 4-byte selector + 32 bytes payload.
        let mut bad = vec![0xde, 0xad, 0xbe, 0xef];
        bad.extend(std::iter::repeat_n(0u8, 32));
        let calldata = IUniswapV3Multicall::multicallCall {
            data: vec![good.into(), bad.into()],
        }
        .abi_encode();
        let swaps = decode_pending_many(univ3_router(), &calldata).expect("decode");
        // Outer decode succeeds, garbage inner is silently dropped — the
        // good swap still surfaces.
        assert_eq!(swaps.len(), 1);
    }

    #[test]
    fn decode_nested_multicall_depth_two_still_peels() {
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let inner = build_exact_input_single(weth, usdc, U256::from(7u64), Address::ZERO);
        // Outer is multicall, inner is *also* a multicall containing one swap.
        let mid = IUniswapV3Multicall::multicallCall {
            data: vec![inner.into()],
        }
        .abi_encode();
        let outer = IUniswapV3Multicall::multicallCall {
            data: vec![mid.into()],
        }
        .abi_encode();
        let swaps = decode_pending_many(univ3_router(), &outer).expect("decode");
        // Depth cap is 2 (outer = depth 0, mid = depth 1, inner-call selectors
        // decoded at depth 2 — still allowed, the cap blocks recursion *into*
        // a multicall at depth 2).
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].amount_in, U256::from(7u64));
    }

    #[test]
    fn decode_triple_nested_multicall_blocked_at_depth_cap() {
        // Build: multicall(multicall(multicall(exactInputSingle))) — three
        // wrapping layers. With MAX_MULTICALL_DEPTH = 2, the innermost
        // multicall is reached at depth 2 and refused to recurse, so the
        // exactInputSingle inside is *not* peeled.
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let leaf = build_exact_input_single(weth, usdc, U256::from(1u64), Address::ZERO);
        let l1 = IUniswapV3Multicall::multicallCall { data: vec![leaf.into()] }.abi_encode();
        let l2 = IUniswapV3Multicall::multicallCall { data: vec![l1.into()] }.abi_encode();
        let l3 = IUniswapV3Multicall::multicallCall { data: vec![l2.into()] }.abi_encode();
        let swaps = decode_pending_many(univ3_router(), &l3).expect("decode");
        assert!(
            swaps.is_empty(),
            "swap nested past MAX_MULTICALL_DEPTH must not be peeled"
        );
    }

    #[test]
    fn decode_pending_many_single_swap_returns_one_record() {
        // Behavioural parity: callers that pass a plain exactInputSingle
        // calldata get a one-element vec equivalent to `decode_pending`'s
        // success path.
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let calldata = build_exact_input_single(weth, usdc, U256::from(42u64), Address::ZERO);
        let many = decode_pending_many(univ3_router(), &calldata).expect("decode");
        let single = decode_pending(univ3_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1);
        assert_eq!(many[0].amount_in, single.amount_in);
        assert_eq!(many[0].token_in, single.token_in);
        assert_eq!(many[0].token_out, single.token_out);
    }

    #[test]
    fn decode_exact_output_single_surfaces_amount_out_as_amount_in() {
        use IUniswapV3Router::{exactOutputSingleCall, ExactOutputSingleParams};
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let params = ExactOutputSingleParams {
            tokenIn: weth,
            tokenOut: usdc,
            fee: alloy::primitives::aliases::U24::from(500),
            recipient: Address::ZERO,
            deadline: U256::from(99u64),
            amountOut: U256::from(2_500u64),
            amountInMaximum: U256::from(2u64) * U256::from(10u64).pow(U256::from(18u64)),
            sqrtPriceLimitX96: alloy::primitives::U160::ZERO,
        };
        let calldata = exactOutputSingleCall { params }.abi_encode();
        let swap = decode_pending(univ3_router(), &calldata).expect("decode");
        assert_eq!(swap.protocol, Protocol::UniswapV3);
        assert_eq!(swap.token_in, weth);
        assert_eq!(swap.token_out, usdc);
        assert_eq!(swap.amount_in, U256::from(2_500u64));
        assert_eq!(swap.fee_bps, 500);
    }

    #[test]
    fn decode_bancor_trade_by_target_amount() {
        // Target-amount flavour: user commits a target amount, accepts up
        // to maxSource. Decoder surfaces targetAmount as `amount_in` and
        // maxSourceAmount as `amount_out_min` for symmetry with the
        // source-amount path — both numbers are real magnitudes for the
        // predictor.
        let bancor = address!("eEF417e1D5CC832e619ae18D2F140De2999dD4fB");
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let bnt = address!("1F573D6Fb3F13d689FF844B4cE37794d79a7FF1C");
        let calldata = IBancorNetwork::tradeByTargetAmountCall {
            sourceToken: weth,
            targetToken: bnt,
            targetAmount: U256::from(5_000u64),
            maxSourceAmount: U256::from(2_000u64),
            deadline: U256::from(99u64),
            beneficiary: Address::ZERO,
        }
        .abi_encode();
        let decoded = decode_pending(bancor, &calldata).expect("should decode");
        assert_eq!(decoded.protocol, Protocol::BancorV3);
        assert_eq!(decoded.token_in, weth);
        assert_eq!(decoded.token_out, bnt);
        assert_eq!(decoded.amount_in, U256::from(5_000u64));
        assert_eq!(decoded.amount_out_min, U256::from(2_000u64));
    }
}
