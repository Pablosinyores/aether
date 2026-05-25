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
//! - `multicall` / `execute` wrappers (UniV3 SwapRouter `multicall`) —
//!   handled in a follow-up that recursively peels nested calldata.
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
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256);
        function exactInputSingle02(ExactInputSingleParams02 params) external payable returns (uint256);
        function exactInput(ExactInputParams params) external payable returns (uint256);
        function exactInput02(ExactInputParams02 params) external payable returns (uint256);
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

/// Decode a pending tx's `(to, calldata)` into a [`DecodedSwap`].
///
/// `to` is required: anonymous calls (contract creation) always return
/// [`DecodeError::TooShort`]. The caller is expected to filter by router
/// address before calling — this function does not validate that the `to`
/// matches a known router; it only consumes the selector + payload.
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

    // ── UniV2 / Sushi family ──
    if let Some(swap) = try_uni_v2_family(selector, calldata, to)? {
        return Ok(swap);
    }
    // ── UniV3 SwapRouter / SwapRouter02 ──
    if let Some(swap) = try_uni_v3_family(selector, calldata, to)? {
        return Ok(swap);
    }
    // ── Balancer V2 Vault ──
    if let Some(swap) = try_balancer(selector, calldata, to)? {
        return Ok(swap);
    }
    // ── Curve StableSwap pool-direct exchange() ──
    if let Some(swap) = try_curve(selector, calldata, to)? {
        return Ok(swap);
    }
    // ── Bancor V3 BancorNetwork ──
    if let Some(swap) = try_bancor(selector, calldata, to)? {
        return Ok(swap);
    }

    Err(DecodeError::UnknownSelector { selector })
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
    Ok(None)
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
