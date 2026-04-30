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

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

/// Protocol tag attached to every successful decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    UniswapV2,
    UniswapV3,
    SushiSwap,
    BalancerV2,
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
        protocol: Protocol::UniswapV2, // SushiSwap callers rely on metric label, not this
        router,
        token_in,
        token_out,
        amount_in,
        amount_out_min,
        recipient: to,
        fee_bps: 0,
        path_extra,
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
        }));
    }
    Ok(None)
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
}
