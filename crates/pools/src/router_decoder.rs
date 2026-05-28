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
//! - **1inch v6 AggregationRouter**: `swap` (opaque executor — surfaces
//!   the `(srcToken, dstToken, amount)` triple with `pool_address = None`
//!   so the upstream pipeline can tag it `unresolved_executor`),
//!   `unoswap{,2,3}{,To}` / `ethUnoswap{,2,3}{,To}` (V2-style chain across
//!   1–3 pools, one [`DecodedSwap`] emitted per pool with the pool address
//!   peeled from the low 160 bits of each `uint256` and the zeroForOne
//!   flag from bit 247), and `uniswapV3Swap{,To}` (same encoding, V3-only).
//!
//! Out of scope (returns `UnknownSelector`):
//!
//! - Curve router — its `exchange` / `exchange_multiple` shape varies per
//!   pool registry version and would inflate the decoder without yielding
//!   reliable hits in the testing scaffold.
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
    /// 1inch v6 AggregationRouter. The router multiplexes across every
    /// DEX family, so a single calldata can peel into multiple
    /// `DecodedSwap` records (one per pool in the `unoswap*` /
    /// `uniswapV3Swap*` chain). `DecodedSwap.pool_address` carries the
    /// per-hop pool when the encoding is pool-keyed; for the opaque
    /// `swap(executor, …)` selector the executor is custom bytecode and
    /// `pool_address` is `None`, leaving the upstream pipeline to
    /// classify the record as unresolved.
    OneInchV6,
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
    /// Concrete pool address when the decoder can resolve the swap to
    /// a single on-chain pool. Populated by aggregator decoders that
    /// embed the pool address in their calldata (e.g. 1inch v6
    /// `unoswap*` chains, where each `uint256` in the pool array packs
    /// the pool address in its low 160 bits). `None` for protocols whose
    /// pool is implicitly resolved from `(token_in, token_out,
    /// protocol)` via the pool registry (UniV2/V3, Sushi, Balancer,
    /// Curve, Bancor — those all leave this `None` and the pipeline
    /// looks up the pool by pair). Also `None` for opaque-executor
    /// aggregator paths (1inch v6 `swap(executor, …)`), which the
    /// pipeline classifies as `unresolved_executor`.
    pub pool_address: Option<Address>,
    /// 1inch v6 only: zeroForOne flag peeled from the high bits of a
    /// pool-encoded `uint256`. `Some(true)` means swap token0 → token1
    /// on the pool, `Some(false)` means token1 → token0. `None` for
    /// every non-1inch protocol. Used downstream to map the peeled
    /// pool's `token0`/`token1` to the swap's `(token_in, token_out)`
    /// without needing to know either token in calldata.
    pub one_inch_zero_for_one: Option<bool>,
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

        /// Liquidity-management entry points. They share the router address
        /// with the swap surface, so live mempool traffic mixes them in
        /// freely. We never backrun pool-LP edits — the AMM invariant after
        /// add/remove liquidity is identical to before for a swapper's
        /// purposes (no marginal price change). Declaring them here gives
        /// us their selectors so the top-level dispatcher can skip them
        /// silently instead of polluting the `unknown_selector` metric.
        function addLiquidity(address tokenA, address tokenB, uint256 amountADesired, uint256 amountBDesired, uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline) external;
        function addLiquidityETH(address token, uint256 amountTokenDesired, uint256 amountTokenMin, uint256 amountETHMin, address to, uint256 deadline) external payable;
        function removeLiquidity(address tokenA, address tokenB, uint256 liquidity, uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline) external;
        function removeLiquidityETH(address token, uint256 liquidity, uint256 amountTokenMin, uint256 amountETHMin, address to, uint256 deadline) external;
        function removeLiquidityWithPermit(address tokenA, address tokenB, uint256 liquidity, uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline, bool approveMax, uint8 v, bytes32 r, bytes32 s) external;
        function removeLiquidityETHWithPermit(address token, uint256 liquidity, uint256 amountTokenMin, uint256 amountETHMin, address to, uint256 deadline, bool approveMax, uint8 v, bytes32 r, bytes32 s) external;
        function removeLiquidityETHSupportingFeeOnTransferTokens(address token, uint256 liquidity, uint256 amountTokenMin, uint256 amountETHMin, address to, uint256 deadline) external;
        function removeLiquidityETHWithPermitSupportingFeeOnTransferTokens(address token, uint256 liquidity, uint256 amountTokenMin, uint256 amountETHMin, address to, uint256 deadline, bool approveMax, uint8 v, bytes32 r, bytes32 s) external;
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
        // SwapRouter (V1, with deadline) and SwapRouter02 (V2, no deadline)
        // share the canonical Solidity function names on chain. The selector
        // is keccak256 of `<name>(<tuple>)`, so the Rust function name
        // we pass to `sol!` MUST be the on-chain name — using disambiguating
        // suffixes like `exactInputSingle02` produces selectors that no
        // real router ever emits. Declaring both signatures with the same
        // name relies on `sol!`'s overload-renaming (`<name>_0` / `<name>_1`
        // suffix in generated Rust types) while keeping the ABI signature
        // — and therefore the selector — faithful to mainnet.
        function exactInputSingle(ExactInputSingleParams params) external payable returns (uint256);
        function exactInputSingle(ExactInputSingleParams02 params) external payable returns (uint256);
        function exactInput(ExactInputParams params) external payable returns (uint256);
        function exactInput(ExactInputParams02 params) external payable returns (uint256);
        function exactOutputSingle(ExactOutputSingleParams params) external payable returns (uint256);
        function exactOutputSingle(ExactOutputSingleParams02 params) external payable returns (uint256);
        function exactOutput(ExactOutputParams params) external payable returns (uint256);
        function exactOutput(ExactOutputParams02 params) external payable returns (uint256);

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

    /// 1inch v6 `AggregationRouterV6`.
    ///
    /// The v6 router multiplexes across every DEX family. Three selector
    /// families matter for mempool decoding:
    ///
    /// 1. `swap(executor, SwapDescription, data)` — opaque executor path.
    ///    The `executor` is a custom-deployed contract whose bytecode we
    ///    cannot statically analyse, so the decoder surfaces only the
    ///    user-visible `(srcToken, dstToken, amount)` triple and leaves
    ///    `pool_address = None` (pipeline tags it `unresolved_executor`).
    ///
    /// 2. `unoswap*` / `ethUnoswap*` — V2-style chain across one to three
    ///    pools encoded as `uint256` words. Each pool word packs:
    ///      - bits   0..159: pool address (low 160 bits)
    ///      - bit    247   : zeroForOne flag for V3 pools (and reverse
    ///                       direction for V2 pools); see [`pool_zero_for_one`]
    ///      - other bits   : protocol-family flags the decoder does not
    ///                       need to resolve (the pipeline disambiguates
    ///                       V2 vs V3 via the live pool registry's
    ///                       `protocol` column for that address).
    ///    `ethUnoswap*` mirrors `unoswap*` minus the `token` arg — `srcToken`
    ///    is native ETH which we treat as WETH downstream.
    ///
    /// 3. `uniswapV3Swap*` — same pool-encoding as `unoswap*` but every
    ///    pool is implicitly UniswapV3, so the pipeline can skip the
    ///    V2-vs-V3 disambiguation.
    ///
    /// `To` suffixed variants forward to an explicit `recipient`; the
    /// decoder treats them identically to the base variant for the
    /// purposes of `(srcToken, dstToken, amount)` extraction. `dex2` /
    /// `dex3` for the multi-hop variants are the chain continuation pools.
    #[allow(missing_docs)]
    interface IOneInchV6Router {
        struct SwapDescription {
            address srcToken;
            address dstToken;
            address payable srcReceiver;
            address payable dstReceiver;
            uint256 amount;
            uint256 minReturnAmount;
            uint256 flags;
        }
        function swap(address executor, SwapDescription desc, bytes data) external payable returns (uint256, uint256);

        function unoswap(uint256 token, uint256 amount, uint256 minReturn, uint256 dex) external returns (uint256);
        function unoswapTo(uint256 to, uint256 token, uint256 amount, uint256 minReturn, uint256 dex) external returns (uint256);
        function unoswap2(uint256 token, uint256 amount, uint256 minReturn, uint256 dex, uint256 dex2) external returns (uint256);
        function unoswap2To(uint256 to, uint256 token, uint256 amount, uint256 minReturn, uint256 dex, uint256 dex2) external returns (uint256);
        function unoswap3(uint256 token, uint256 amount, uint256 minReturn, uint256 dex, uint256 dex2, uint256 dex3) external returns (uint256);
        function unoswap3To(uint256 to, uint256 token, uint256 amount, uint256 minReturn, uint256 dex, uint256 dex2, uint256 dex3) external returns (uint256);

        function ethUnoswap(uint256 minReturn, uint256 dex) external payable returns (uint256);
        function ethUnoswapTo(uint256 to, uint256 minReturn, uint256 dex) external payable returns (uint256);
        function ethUnoswap2(uint256 minReturn, uint256 dex, uint256 dex2) external payable returns (uint256);
        function ethUnoswap2To(uint256 to, uint256 minReturn, uint256 dex, uint256 dex2) external payable returns (uint256);
        function ethUnoswap3(uint256 minReturn, uint256 dex, uint256 dex2, uint256 dex3) external payable returns (uint256);
        function ethUnoswap3To(uint256 to, uint256 minReturn, uint256 dex, uint256 dex2, uint256 dex3) external payable returns (uint256);

        function uniswapV3Swap(uint256 amount, uint256 minReturn, uint256[] pools) external payable returns (uint256);
        function uniswapV3SwapTo(address payable recipient, uint256 amount, uint256 minReturn, uint256[] pools) external payable returns (uint256);

        /// Limit-order admin entry points. They share the router address
        /// with the swap surface but never move AMM pool state in a way a
        /// backrunner can exploit — `cancelOrder` is a maker-side state
        /// flip, and `fillContractOrder` settles an off-chain order against
        /// a maker's pre-signed allowance rather than triggering an AMM
        /// swap. The decoder declares them only to recover their selectors
        /// so the dispatcher can silently skip them instead of bumping
        /// `unknown_selector`.
        function cancelOrder(uint256 makerTraits, bytes32 orderHash) external;
        function fillContractOrder(
            (uint256, uint256, uint256, uint256, uint256, uint256, uint256, uint256) order,
            bytes signature,
            uint256 amount,
            uint256 takerTraits
        ) external returns (uint256, uint256, bytes32);
    }

    /// Uniswap Universal Router — `execute(bytes commands, bytes[] inputs)`
    /// and its deadline-bearing sibling. The router is a small VM: each
    /// byte in `commands` is an opcode that consumes the corresponding
    /// entry from the parallel `inputs[]` array. The two `execute`
    /// signatures differ only in the trailing `deadline` parameter, so
    /// `sol!`'s overload renaming (`_0Call` / `_1Call`) keeps both
    /// selectors faithful to the on-chain values. Tuple ABIs of each
    /// individual opcode's input are NOT declared here — the opcode
    /// parser in [`try_universal_router`] walks `inputs[]` element by
    /// element using the per-opcode ABI shape documented in
    /// `Commands.sol`.
    #[allow(missing_docs)]
    interface IUniversalRouter {
        function execute(bytes commands, bytes[] inputs) external payable;
        function execute(bytes commands, bytes[] inputs, uint256 deadline) external payable;
    }
}

/// 1inch v6 AggregationRouter address on Ethereum Mainnet. Pinned here so
/// the decoder and the default filter list agree on a single source of
/// truth — adding a chain or moving routers means editing one constant.
pub const ONE_INCH_V6_ROUTER: Address =
    address!("111111125421cA6dc452d289314280a0f8842A65");

/// Uniswap Universal Router (V2, deployed April 2024) on Ethereum Mainnet.
pub const UNIVERSAL_ROUTER_V2: Address =
    address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");

/// Uniswap Universal Router (V1.2, predecessor) on Ethereum Mainnet. Still
/// receives traffic from integrators that haven't migrated to V2 — keep
/// it in the recognised router set so we can decode both side-by-side.
pub const UNIVERSAL_ROUTER_V12: Address =
    address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD");

/// Universal Router command bytes (a subset — only those the decoder
/// actually inspects). The full opcode table lives in Uniswap's
/// `Commands.sol`; everything not listed here falls through the
/// dispatcher as a no-op for our purposes (the opcode runs on-chain
/// but doesn't move AMM pool state in a way a backrunner can exploit).
mod ur_commands {
    /// Mask isolating the opcode bits (low 6) from the optional flag
    /// bits (high 2 — `FLAG_ALLOW_REVERT` and unused). Universal Router
    /// dispatch ignores the flag bits when looking up the handler.
    pub const COMMAND_TYPE_MASK: u8 = 0x3f;

    /// `V3_SWAP_EXACT_IN` — swap an exact input amount along a packed
    /// UniV3 path. Inputs ABI:
    /// `(address recipient, uint256 amountIn, uint256 amountOutMinimum, bytes path, bool payerIsUser)`.
    pub const V3_SWAP_EXACT_IN: u8 = 0x00;

    /// `V3_SWAP_EXACT_OUT` — swap to receive an exact output amount along
    /// a packed UniV3 path. The path is encoded in **reverse** order
    /// (`token_out | fee | ... | token_in`) so the on-chain quoter can
    /// walk it from the destination back to the source. Inputs ABI:
    /// `(address recipient, uint256 amountOut, uint256 amountInMaximum, bytes path, bool payerIsUser)`.
    pub const V3_SWAP_EXACT_OUT: u8 = 0x01;

    /// `V2_SWAP_EXACT_IN` — swap an exact input amount along a UniV2
    /// `address[]` path. Inputs ABI:
    /// `(address recipient, uint256 amountIn, uint256 amountOutMinimum, address[] path, bool payerIsUser)`.
    pub const V2_SWAP_EXACT_IN: u8 = 0x08;

    /// `V2_SWAP_EXACT_OUT` — swap to receive an exact output amount along
    /// a UniV2 `address[]` path. Inputs ABI:
    /// `(address recipient, uint256 amountOut, uint256 amountInMaximum, address[] path, bool payerIsUser)`.
    pub const V2_SWAP_EXACT_OUT: u8 = 0x09;

    /// `V4_SWAP` — wraps a Uniswap V4 PoolManager action stream
    /// (`(bytes actions, bytes[] params)`) inside a single UR input
    /// slot. Dominates current Universal Router V2 traffic: the
    /// production captures collected during the E5 fixture sweep at
    /// block ~25187100 showed `0x10` at the top level of roughly
    /// 80 percent of UR V2 calls, with the actual swap math living
    /// one layer deeper.
    ///
    /// Decoding V4 actions requires modelling the V4 PoolManager
    /// (PoolKey, hooks address, per-action ABI shapes) and is not
    /// part of the current V2/V3 backrun target surface. Until that
    /// lands the dispatcher treats this opcode as a *named* skip —
    /// distinct from the catch-all unknown-opcode skip — so the
    /// pipeline can size the missed-volume from a dedicated metric
    /// label and we can decide whether full V4 support clears the
    /// effort bar.
    pub const V4_SWAP: u8 = 0x10;
}

/// 1inch v6 pool-encoding bit constants. The router packs each pool word
/// as `low 160 bits = pool address`, with flag bits in the high end. The
/// only flag the decoder reads is the per-hop direction bit; other flags
/// (source DEX type, fee-on-transfer markers, etc.) the upstream pipeline
/// resolves via the live pool registry's `protocol` column for that
/// address.
mod one_inch_bits {
    /// Bit position of the `zeroForOne` direction flag inside a pool word.
    /// Matches the constant the v6 source uses for both V3 swaps (`true`
    /// = token0 → token1) and the V2 reverse-direction flag.
    pub const ZERO_FOR_ONE_BIT: u32 = 247;
}

/// Peel an encoded `uint256` pool word into `(pool_address, zero_for_one)`.
///
/// Pool address is the low 160 bits; `zero_for_one` is the bit at
/// [`one_inch_bits::ZERO_FOR_ONE_BIT`]. Both extractions are cheap
/// bit-ops with no allocation.
#[inline]
fn pool_zero_for_one(encoded: U256) -> (Address, bool) {
    // Low 160 bits = the pool address. U256::to_be_bytes() yields a
    // big-endian 32-byte view; the address is the last 20 bytes.
    let be: [u8; 32] = encoded.to_be_bytes();
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&be[12..32]);
    let pool = Address::from(addr);
    let bit = U256::from(1u64) << one_inch_bits::ZERO_FOR_ONE_BIT;
    let zero_for_one = (encoded & bit) != U256::ZERO;
    (pool, zero_for_one)
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
/// For routers that can emit more than one swap per calldata (UniV3
/// `multicall(bytes[])` bundles, 1inch v6 `unoswap*` chains) use
/// [`decode_pending_many`] instead — this function collapses to the first
/// hop only.
pub fn decode_pending(to: Address, calldata: &[u8]) -> Result<DecodedSwap, DecodeError> {
    let mut all = decode_pending_many(to, calldata)?;
    // `decode_pending_many` never returns an empty Ok vec — every Ok arm
    // pushes at least one record — so this `remove(0)` is infallible.
    Ok(all.remove(0))
}

/// Decode a pending tx's `(to, calldata)` into one or more [`DecodedSwap`]
/// records. Most router shapes emit exactly one record; 1inch v6
/// `unoswap2` / `unoswap3` / `uniswapV3Swap` chains emit one record per
/// pool in the chain (first hop carries the user-committed `amount_in`,
/// subsequent hops carry `U256::ZERO` because the intermediate amounts
/// are only resolvable mid-execution — the upstream pipeline rebuilds
/// them from each pool's post-state).
///
/// Returns a non-empty `Vec` on `Ok`. Empty vec is never produced; callers
/// can safely index `[0]` without bounds checks.
pub fn decode_pending_many(
    to: Address,
    calldata: &[u8],
) -> Result<Vec<DecodedSwap>, DecodeError> {
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

    // 1inch v6 AggregationRouter — multi-record producer; peeled before
    // try_decode_single so the per-hop pool addresses propagate.
    if let Some(swaps) = try_one_inch_v6(selector, calldata, to)? {
        return Ok(swaps);
    }
    // Uniswap Universal Router — also a multi-record producer (one
    // record per swap-typed opcode in the `commands` byte stream).
    if let Some(swaps) = try_universal_router(selector, calldata, to)? {
        return Ok(swaps);
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

/// Recognise a selector that targets a known router but doesn't move pool
/// state in a way that's interesting to a backrunner. Two families qualify:
///
/// 1. UniV3 multicall helpers (`selfPermit*`, `unwrapWETH9*`, `refundETH`,
///    `sweepToken*`) — legal inside `multicall(bytes[])` payloads.
/// 2. UniV2 liquidity-management entry points (`addLiquidity*`,
///    `removeLiquidity*` and their permit / fee-on-transfer variants) —
///    they share the router address with the swap surface and appear
///    frequently in live mempool traffic.
/// 3. 1inch v6 limit-order admin entry points (`cancelOrder`,
///    `fillContractOrder`) — they share the router address with the
///    aggregator swap surface and account for ~34% of `unknown_selector`
///    noise from the 1inch router in the most recent shadow run, but
///    neither moves AMM pool state directly.
///
/// Returning `true` here lets the dispatcher silently skip the helper
/// instead of bumping `unknown_selector` for a well-formed payload.
fn try_is_known_non_swap_helper(selector: [u8; 4]) -> bool {
    use IOneInchV6Router as v6;
    use IUniswapV2Router02 as v2;
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
        || selector == v2::addLiquidityCall::SELECTOR
        || selector == v2::addLiquidityETHCall::SELECTOR
        || selector == v2::removeLiquidityCall::SELECTOR
        || selector == v2::removeLiquidityETHCall::SELECTOR
        || selector == v2::removeLiquidityWithPermitCall::SELECTOR
        || selector == v2::removeLiquidityETHWithPermitCall::SELECTOR
        || selector == v2::removeLiquidityETHSupportingFeeOnTransferTokensCall::SELECTOR
        || selector == v2::removeLiquidityETHWithPermitSupportingFeeOnTransferTokensCall::SELECTOR
        || selector == v6::cancelOrderCall::SELECTOR
        || selector == v6::fillContractOrderCall::SELECTOR
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
        pool_address: None,
        one_inch_zero_for_one: None,
    })
}

fn try_uni_v3_family(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use IUniswapV3Router::*;
    if selector == exactInputSingle_0Call::SELECTOR {
        let c = exactInputSingle_0Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactInputSingle_1Call::SELECTOR {
        let c = exactInputSingle_1Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactInput_0Call::SELECTOR {
        let c = exactInput_0Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactInput_1Call::SELECTOR {
        let c = exactInput_1Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
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
    if selector == exactOutputSingle_0Call::SELECTOR {
        let c = exactOutputSingle_0Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactOutputSingle_1Call::SELECTOR {
        let c = exactOutputSingle_1Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactOutput_0Call::SELECTOR {
        let c = exactOutput_0Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
        }));
    }
    if selector == exactOutput_1Call::SELECTOR {
        let c = exactOutput_1Call::abi_decode(calldata)
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
            pool_address: None,
            one_inch_zero_for_one: None,
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
            pool_address: None,
            one_inch_zero_for_one: None,
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
            pool_address: None,
            one_inch_zero_for_one: None,
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
            pool_address: None,
            one_inch_zero_for_one: None,
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
        pool_address: None,
        one_inch_zero_for_one: None,
    }
}

/// 1inch v6 AggregationRouter decoder. Dispatches by selector and emits
/// one or more [`DecodedSwap`] records depending on the shape:
///
/// - `swap(executor, desc, data)` → one record with `pool_address = None`
///   (pipeline tags it `unresolved_executor`). `token_in` / `token_out`
///   come from `desc.srcToken` / `desc.dstToken`; `amount_in` from
///   `desc.amount`. Useful even without a concrete pool — at minimum the
///   token pair lets the pipeline gauge where 1inch volume is flowing.
///
/// - `unoswap{,2,3}{,To}` and `ethUnoswap{,2,3}{,To}` → one record per
///   pool in the chain. `token_in` on the first hop is the user's `token`
///   arg (or WETH for the `ethUnoswap*` family — ETH is always swapped
///   to WETH inside the router). `token_in` on hops 2 and 3 is `ZERO`
///   because the intermediate token is only known after looking the pool
///   up in the registry — the upstream pipeline does that. `amount_in`
///   on the first hop is the user's `amount` (or `0` for `ethUnoswap*`
///   because the amount is `msg.value` and not in calldata); subsequent
///   hops carry `ZERO`.
///
/// - `uniswapV3Swap{,To}` → one record per pool, every pool is V3.
///   `token_in` on the first hop is the pool's token0/token1 picked by
///   the `zeroForOne` bit; the pipeline resolves that lookup against
///   the live registry — we only know the pool address and direction
///   from calldata alone.
fn try_one_inch_v6(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<Vec<DecodedSwap>>, DecodeError> {
    use IOneInchV6Router::*;

    // ── swap(executor, SwapDescription, data) — opaque executor ──
    if selector == swapCall::SELECTOR {
        let c = swapCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(vec![one_inch_unresolved_swap(
            router,
            c.desc.srcToken,
            c.desc.dstToken,
            c.desc.amount,
            c.desc.minReturnAmount,
            c.desc.dstReceiver,
        )]));
    }

    // ── unoswap family (token committed in calldata) ──
    if selector == unoswapCall::SELECTOR {
        let c = unoswapCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex], Address::ZERO,
        )));
    }
    if selector == unoswapToCall::SELECTOR {
        let c = unoswapToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex], recipient,
        )));
    }
    if selector == unoswap2Call::SELECTOR {
        let c = unoswap2Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex, c.dex2], Address::ZERO,
        )));
    }
    if selector == unoswap2ToCall::SELECTOR {
        let c = unoswap2ToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex, c.dex2], recipient,
        )));
    }
    if selector == unoswap3Call::SELECTOR {
        let c = unoswap3Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex, c.dex2, c.dex3], Address::ZERO,
        )));
    }
    if selector == unoswap3ToCall::SELECTOR {
        let c = unoswap3ToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let token_in = u256_low_address(c.token);
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, token_in, c.amount, c.minReturn, &[c.dex, c.dex2, c.dex3], recipient,
        )));
    }

    // ── ethUnoswap family — src is native ETH; treat as WETH ──
    if selector == ethUnoswapCall::SELECTOR {
        let c = ethUnoswapCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex], Address::ZERO,
        )));
    }
    if selector == ethUnoswapToCall::SELECTOR {
        let c = ethUnoswapToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex], recipient,
        )));
    }
    if selector == ethUnoswap2Call::SELECTOR {
        let c = ethUnoswap2Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex, c.dex2], Address::ZERO,
        )));
    }
    if selector == ethUnoswap2ToCall::SELECTOR {
        let c = ethUnoswap2ToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex, c.dex2], recipient,
        )));
    }
    if selector == ethUnoswap3Call::SELECTOR {
        let c = ethUnoswap3Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex, c.dex2, c.dex3], Address::ZERO,
        )));
    }
    if selector == ethUnoswap3ToCall::SELECTOR {
        let c = ethUnoswap3ToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        let recipient = u256_low_address(c.to);
        return Ok(Some(one_inch_chain(
            router, WETH_ADDRESS, U256::ZERO, c.minReturn, &[c.dex, c.dex2, c.dex3], recipient,
        )));
    }

    // ── uniswapV3Swap family — every pool is V3 ──
    if selector == uniswapV3SwapCall::SELECTOR {
        let c = uniswapV3SwapCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        if c.pools.is_empty() {
            return Err(DecodeError::EmptyPath);
        }
        return Ok(Some(one_inch_chain(
            router, Address::ZERO, c.amount, c.minReturn, &c.pools, Address::ZERO,
        )));
    }
    if selector == uniswapV3SwapToCall::SELECTOR {
        let c = uniswapV3SwapToCall::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        if c.pools.is_empty() {
            return Err(DecodeError::EmptyPath);
        }
        return Ok(Some(one_inch_chain(
            router, Address::ZERO, c.amount, c.minReturn, &c.pools, c.recipient,
        )));
    }

    Ok(None)
}

/// Universal Router dispatcher.
///
/// `execute(bytes commands, bytes[] inputs[, uint256 deadline])` is a
/// command-byte VM, not an ABI-encoded swap. Each byte in `commands`
/// is an opcode (after masking with [`ur_commands::COMMAND_TYPE_MASK`])
/// that consumes one entry from the parallel `inputs[]` array. Opcodes
/// fall into three buckets:
///
/// 1. **Swap opcodes** (`V3_SWAP_EXACT_IN`, `V3_SWAP_EXACT_OUT`,
///    `V2_SWAP_EXACT_IN`, `V2_SWAP_EXACT_OUT`) — decode the input as a
///    typed tuple and emit one [`DecodedSwap`].
/// 2. **Permit / wrap / sweep helpers** (`PERMIT2_*`, `WRAP_ETH`,
///    `UNWRAP_WETH`, `SWEEP`, `TRANSFER`, `PAY_PORTION`,
///    `BALANCE_CHECK_ERC20`) — no AMM state change; skipped silently.
/// 3. **Unknown opcodes** — also skipped silently (the on-chain VM may
///    add new opcodes ahead of our decoder; bumping `unknown_selector`
///    for every unfamiliar command would drown the metric).
///
/// This first cut implements only `V3_SWAP_EXACT_IN` — the highest
/// traffic opcode by an order of magnitude. Additional swap opcodes
/// land as follow-up PRs (see decoder backlog E4).
fn try_universal_router(
    selector: [u8; 4],
    calldata: &[u8],
    router: Address,
) -> Result<Option<Vec<DecodedSwap>>, DecodeError> {
    use IUniversalRouter::*;

    let (commands, inputs) = if selector == execute_0Call::SELECTOR {
        let c = execute_0Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        (c.commands, c.inputs)
    } else if selector == execute_1Call::SELECTOR {
        let c = execute_1Call::abi_decode(calldata)
            .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
        (c.commands, c.inputs)
    } else {
        return Ok(None);
    };

    if commands.len() != inputs.len() {
        // Malformed payload — commands stream and inputs array must be
        // parallel. Treat as decode failure rather than partial decode
        // so the pipeline doesn't act on half-parsed traffic.
        return Err(DecodeError::AbiDecode(format!(
            "Universal Router: commands.len()={} but inputs.len()={}",
            commands.len(),
            inputs.len()
        )));
    }

    let mut out: Vec<DecodedSwap> = Vec::new();
    for (cmd_byte, input) in commands.iter().zip(inputs.iter()) {
        let opcode = cmd_byte & ur_commands::COMMAND_TYPE_MASK;
        if let Some(swap) = decode_ur_opcode(opcode, input.as_ref(), router)? {
            out.push(swap);
        }
        // Non-swap or unknown opcodes are intentionally skipped (see
        // function-level doc). The opcode index is preserved by virtue
        // of iterating in parallel — no record means no contribution to
        // the swap stream, which is exactly what a permit / wrap /
        // unknown step would produce.
    }
    Ok(Some(out))
}

/// Decode a single Universal Router opcode into an optional [`DecodedSwap`].
///
/// Returns `Ok(None)` for non-swap opcodes (permit, wrap, sweep, ...).
/// Returns `Err(...)` only if a recognised swap opcode has malformed
/// inputs — unrecognised opcodes are dropped silently to keep the
/// decoder robust against future opcode additions on the on-chain VM.
fn decode_ur_opcode(
    opcode: u8,
    input: &[u8],
    router: Address,
) -> Result<Option<DecodedSwap>, DecodeError> {
    use alloy::sol_types::{SolType, sol_data};
    type V3Tuple = (
        sol_data::Address,
        sol_data::Uint<256>,
        sol_data::Uint<256>,
        sol_data::Bytes,
        sol_data::Bool,
    );
    type V2Tuple = (
        sol_data::Address,
        sol_data::Uint<256>,
        sol_data::Uint<256>,
        sol_data::Array<sol_data::Address>,
        sol_data::Bool,
    );
    match opcode {
        ur_commands::V3_SWAP_EXACT_IN => {
            // Tuple shape, per Commands.sol:
            //   (address recipient, uint256 amountIn,
            //    uint256 amountOutMinimum, bytes path, bool payerIsUser)
            // `path` is the canonical UniV3 packed
            // token | fee | token | fee | … | token byte stream.
            let (recipient, amount_in, amount_out_min, path, _payer_is_user) =
                V3Tuple::abi_decode_params(input)
                    .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            let (token_in, token_out, fee, path_extra) = parse_v3_path(&path)?;
            Ok(Some(DecodedSwap {
                protocol: Protocol::UniswapV3,
                router,
                token_in,
                token_out,
                amount_in,
                amount_out_min,
                recipient,
                fee_bps: fee,
                path_extra,
                curve_indices: None,
                pool_address: None,
                one_inch_zero_for_one: None,
            }))
        }
        ur_commands::V3_SWAP_EXACT_OUT => {
            // Tuple shape: `(recipient, amountOut, amountInMaximum, path, payerIsUser)`.
            // Path is reversed (token_out first, token_in last). Follow
            // the same surfacing convention as the V3 exactOutput single /
            // multi-hop family elsewhere in this module: `amount_in` =
            // user-committed amountOut, `amount_out_min` = per-hop cap on
            // input spend.
            let (recipient, amount_out, amount_in_max, path, _payer_is_user) =
                V3Tuple::abi_decode_params(input)
                    .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            let (path_first, path_second, fee, extras) = parse_v3_path(&path)?;
            let (token_in, token_out) =
                swap_path_first_hop_for_exact_output(path_first, path_second, &extras);
            Ok(Some(DecodedSwap {
                protocol: Protocol::UniswapV3,
                router,
                token_in,
                token_out,
                amount_in: amount_out,
                amount_out_min: amount_in_max,
                recipient,
                fee_bps: fee,
                path_extra: extras,
                curve_indices: None,
                pool_address: None,
                one_inch_zero_for_one: None,
            }))
        }
        ur_commands::V2_SWAP_EXACT_IN => {
            // Tuple shape: `(recipient, amountIn, amountOutMin, address[] path, payerIsUser)`.
            // The path is an explicit array; the first hop is `path[0] →
            // path[1]`. Reject empty / single-element paths as malformed.
            let (recipient, amount_in, amount_out_min, path, _payer_is_user) =
                V2Tuple::abi_decode_params(input)
                    .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            if path.len() < 2 {
                return Err(DecodeError::EmptyPath);
            }
            let (token_in, token_out) = (path[0], path[1]);
            let path_extra: Vec<Address> = path.into_iter().skip(2).collect();
            Ok(Some(DecodedSwap {
                protocol: Protocol::UniswapV2,
                router,
                token_in,
                token_out,
                amount_in,
                amount_out_min,
                recipient,
                fee_bps: 0,
                path_extra,
                curve_indices: None,
                pool_address: None,
                one_inch_zero_for_one: None,
            }))
        }
        ur_commands::V2_SWAP_EXACT_OUT => {
            // Tuple shape: `(recipient, amountOut, amountInMaximum, address[] path, payerIsUser)`.
            // Unlike V3 exact-out the V2 path is *not* reversed — `path[0]`
            // is still the input token. Same exact-out surfacing
            // convention as the V3 branch above.
            let (recipient, amount_out, amount_in_max, path, _payer_is_user) =
                V2Tuple::abi_decode_params(input)
                    .map_err(|e| DecodeError::AbiDecode(e.to_string()))?;
            if path.len() < 2 {
                return Err(DecodeError::EmptyPath);
            }
            let (token_in, token_out) = (path[0], path[1]);
            let path_extra: Vec<Address> = path.into_iter().skip(2).collect();
            Ok(Some(DecodedSwap {
                protocol: Protocol::UniswapV2,
                router,
                token_in,
                token_out,
                amount_in: amount_out,
                amount_out_min: amount_in_max,
                recipient,
                fee_bps: 0,
                path_extra,
                curve_indices: None,
                pool_address: None,
                one_inch_zero_for_one: None,
            }))
        }
        ur_commands::V4_SWAP => {
            // V4 PoolManager action stream — see the docstring on
            // `ur_commands::V4_SWAP` for the rationale on why this is
            // skipped rather than decoded. The `debug!` is intentional
            // so an operator tailing the log can size the missed
            // volume against the existing `unknown_selector`
            // numerators without a fresh metric migration. Lifting
            // this branch to a real handler is gated on V4 PoolManager
            // pricing + reserve modelling, which lives outside the
            // current backrun target surface.
            tracing::debug!(
                target: "aether_pools::router_decoder",
                router = %router,
                input_len = input.len(),
                "ur_v4_swap_unsupported"
            );
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// WETH on Ethereum mainnet. The `ethUnoswap*` family swaps native ETH
/// into a token; the router wraps to WETH on the way in, so downstream
/// pool lookups should key off WETH rather than `Address::ZERO`.
const WETH_ADDRESS: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// Build a [`DecodedSwap`] for the opaque 1inch v6 `swap(executor, …)`
/// path. `pool_address` is `None` because the executor's pool choice is
/// in custom bytecode — the upstream pipeline tags this record
/// `unresolved_executor` and skips post-state prediction.
fn one_inch_unresolved_swap(
    router: Address,
    src: Address,
    dst: Address,
    amount: U256,
    min_return: U256,
    recipient: Address,
) -> DecodedSwap {
    DecodedSwap {
        protocol: Protocol::OneInchV6,
        router,
        token_in: src,
        token_out: dst,
        amount_in: amount,
        amount_out_min: min_return,
        recipient,
        fee_bps: 0,
        path_extra: vec![],
        curve_indices: None,
        pool_address: None,
        one_inch_zero_for_one: None,
    }
}

/// Build a Vec of [`DecodedSwap`] records for a 1inch `unoswap*` /
/// `uniswapV3Swap*` chain. `pools` lists the encoded pool words in
/// execution order. First hop carries the user-committed `amount_in`;
/// subsequent hops carry `U256::ZERO` (the intermediate amounts are
/// resolved by the executor at runtime and the upstream pipeline
/// reconstructs them from each pool's analytical post-state).
///
/// `token_in_first` is the user's source token (or WETH for the
/// `ethUnoswap*` family, or `Address::ZERO` for `uniswapV3Swap*` where
/// the source token is implied by the first pool's `zeroForOne` bit and
/// must be resolved upstream against the live registry). For hops past
/// the first, `token_in` / `token_out` are both `Address::ZERO` — the
/// pipeline resolves them via `pool_address` lookup.
fn one_inch_chain(
    router: Address,
    token_in_first: Address,
    amount_in: U256,
    min_return: U256,
    pools: &[U256],
    recipient: Address,
) -> Vec<DecodedSwap> {
    pools
        .iter()
        .enumerate()
        .map(|(idx, encoded)| {
            let (pool, zero_for_one) = pool_zero_for_one(*encoded);
            let is_first = idx == 0;
            let is_last = idx + 1 == pools.len();
            DecodedSwap {
                protocol: Protocol::OneInchV6,
                router,
                // Source token is known only for the first hop (and only
                // when the calldata names it — uniswapV3Swap leaves
                // `token_in_first` ZERO and the pipeline resolves from
                // the pool's tokens + zeroForOne).
                token_in: if is_first { token_in_first } else { Address::ZERO },
                token_out: Address::ZERO,
                // Only the first hop carries the user-committed amount;
                // intermediate amounts depend on each pool's runtime
                // post-state and are not statically derivable.
                amount_in: if is_first { amount_in } else { U256::ZERO },
                // Only the final hop carries the user's slippage bound;
                // intermediate hops have no min-return semantics in the
                // 1inch chain encoding.
                amount_out_min: if is_last { min_return } else { U256::ZERO },
                recipient,
                fee_bps: 0,
                path_extra: vec![],
                curve_indices: None,
                pool_address: Some(pool),
                one_inch_zero_for_one: Some(zero_for_one),
            }
        })
        .collect()
}

/// Coerce a `uint256` argument carrying an address-shaped value into a
/// 20-byte [`Address`]. 1inch v6 uses `uint256` instead of `address` for
/// most token / recipient parameters because the high bits encode flags
/// (e.g. `unwrap WETH` for the `to` arg). We discard the flag bits and
/// keep the low 160 bits, matching the router's own treatment of the arg
/// inside `assembly { let token := and(arg, _ADDRESS_MASK) }`.
#[inline]
fn u256_low_address(v: U256) -> Address {
    let be: [u8; 32] = v.to_be_bytes();
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&be[12..32]);
    Address::from(addr)
}

/// Pack a `(pool_address, zero_for_one)` pair into a `uint256` matching
/// 1inch v6's pool-word encoding. Used by downstream crates to build
/// hand-rolled `unoswap*` calldata against the same encoding the decoder
/// consumes.
#[doc(hidden)]
pub fn pool_word_for_test(pool: Address, zero_for_one: bool) -> U256 {
    let mut be = [0u8; 32];
    be[12..32].copy_from_slice(pool.as_slice());
    let mut v = U256::from_be_bytes(be);
    if zero_for_one {
        v |= U256::from(1u64) << one_inch_bits::ZERO_FOR_ONE_BIT;
    }
    v
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

    /// SwapRouter02 (V2) selectors observed in mainnet traffic. Locks in
    /// that the `sol!` overload-renaming produces selectors matching the
    /// canonical Solidity names, not the prior `*02` shim names that
    /// produced macro-fabricated selectors no router ever emits.
    #[test]
    fn univ3_swap_router_v2_selectors_match_mainnet() {
        use IUniswapV3Router::*;
        assert_eq!(
            exactInputSingle_1Call::SELECTOR,
            [0x04, 0xe4, 0x5a, 0xaf],
            "exactInputSingle (SwapRouter02, no deadline) must be 0x04e45aaf"
        );
        assert_eq!(
            exactOutputSingle_1Call::SELECTOR,
            [0x50, 0x23, 0xb4, 0xdf],
            "exactOutputSingle (SwapRouter02, no deadline) must be 0x5023b4df"
        );
        assert_eq!(
            exactInput_1Call::SELECTOR,
            [0xb8, 0x58, 0x18, 0x3f],
            "exactInput (SwapRouter02, no deadline) must be 0xb858183f"
        );
        assert_eq!(
            exactOutput_1Call::SELECTOR,
            [0x09, 0xb8, 0x13, 0x46],
            "exactOutput (SwapRouter02, no deadline) must be 0x09b81346"
        );
        // V1 (with-deadline) selectors are a regression sentinel — they
        // were already correct and must remain so after the overload
        // rename.
        assert_eq!(exactInputSingle_0Call::SELECTOR, [0x41, 0x4b, 0xf3, 0x89]);
        assert_eq!(exactOutputSingle_0Call::SELECTOR, [0xdb, 0x3e, 0x21, 0x98]);
        assert_eq!(exactInput_0Call::SELECTOR, [0xc0, 0x4b, 0x8d, 0x59]);
        assert_eq!(exactOutput_0Call::SELECTOR, [0xf2, 0x8c, 0x04, 0x98]);
    }

    #[test]
    fn decode_uniswap_v3_swap_router_v2_exact_input_single() {
        // V2 = no deadline field. End-to-end decode must succeed for the
        // canonical 0x04e45aaf calldata observed in production traffic.
        use IUniswapV3Router::{ExactInputSingleParams02, exactInputSingle_1Call};
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let params = ExactInputSingleParams02 {
            tokenIn: weth,
            tokenOut: usdc,
            fee: alloy::primitives::Uint::<24, 1>::from(3000u32),
            recipient: Address::ZERO,
            amountIn: U256::from(1_000_000_000_000_000_000u128),
            amountOutMinimum: U256::from(2_500_000_000u128),
            sqrtPriceLimitX96: alloy::primitives::U160::ZERO,
        };
        let calldata = exactInputSingle_1Call { params }.abi_encode();
        let univ3_swap_router02 = address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45");
        let decoded = decode_pending(univ3_swap_router02, &calldata).expect("decode");
        assert_eq!(decoded.protocol, Protocol::UniswapV3);
        assert_eq!(
            decoded.amount_in,
            U256::from(1_000_000_000_000_000_000u128)
        );
    }

    /// 1inch v6 limit-order admin selectors observed in mainnet traffic.
    /// Locks the canonical on-chain selectors in so a future ABI tweak
    /// (e.g. renaming the function in the sol! block without overload
    /// awareness) is caught here instead of by a silent regression in the
    /// `unknown_selector` metric.
    #[test]
    fn one_inch_v6_admin_selectors_match_mainnet() {
        use IOneInchV6Router::*;
        assert_eq!(
            cancelOrderCall::SELECTOR,
            [0xb6, 0x8f, 0xb0, 0x20],
            "cancelOrder(uint256,bytes32) must be 0xb68fb020"
        );
        assert_eq!(
            fillContractOrderCall::SELECTOR,
            [0xcc, 0x71, 0x3a, 0x04],
            "fillContractOrder(...) must be 0xcc713a04"
        );
    }

    #[test]
    fn decode_1inch_cancel_order_silently_skipped() {
        // `cancelOrder` accounted for ~14 of 44 unknown_selector hits in
        // the last shadow run. It's a maker-side state flip on the limit
        // order surface, not a swap — the dispatcher must drop it instead
        // of bubbling UnknownSelector.
        use IOneInchV6Router::cancelOrderCall;
        use alloy::primitives::FixedBytes;
        let calldata = cancelOrderCall {
            makerTraits: U256::ZERO,
            orderHash: FixedBytes::<32>::ZERO,
        }
        .abi_encode();
        let one_inch_router = ONE_INCH_V6_ROUTER;
        let swaps = decode_pending_many(one_inch_router, &calldata).expect("decode");
        assert!(
            swaps.is_empty(),
            "limit-order admin entry points must not produce swap records"
        );
    }

    /// Universal Router `execute` selectors must match the canonical
    /// on-chain values. Locking these here prevents a regression where
    /// either signature gets renamed in the `sol!` block and silently
    /// drifts to a fabricated selector.
    #[test]
    fn universal_router_execute_selectors_match_mainnet() {
        use IUniversalRouter::*;
        assert_eq!(
            execute_0Call::SELECTOR,
            [0x24, 0x85, 0x6b, 0xc3],
            "execute(bytes,bytes[]) must be 0x24856bc3"
        );
        assert_eq!(
            execute_1Call::SELECTOR,
            [0x35, 0x93, 0x56, 0x4c],
            "execute(bytes,bytes[],uint256) must be 0x3593564c"
        );
    }

    /// Build a packed UniV3 path: `token_in | fee | token_out` (43 bytes).
    fn pack_v3_path(token_in: Address, fee: u32, token_out: Address) -> Vec<u8> {
        let mut out = Vec::with_capacity(43);
        out.extend_from_slice(token_in.as_slice());
        out.push(((fee >> 16) & 0xff) as u8);
        out.push(((fee >> 8) & 0xff) as u8);
        out.push((fee & 0xff) as u8);
        out.extend_from_slice(token_out.as_slice());
        out
    }

    /// Encode the input bytes for a `V3_SWAP_EXACT_IN` opcode using the
    /// `sol_types` low-level encoder so the test stays decoupled from
    /// whatever bytes layout the production handler expects.
    fn encode_v3_exact_in_input(
        recipient: Address,
        amount_in: U256,
        amount_out_min: U256,
        path: &[u8],
        payer_is_user: bool,
    ) -> Vec<u8> {
        use alloy::sol_types::{SolType, sol_data};
        type Tup = (
            sol_data::Address,
            sol_data::Uint<256>,
            sol_data::Uint<256>,
            sol_data::Bytes,
            sol_data::Bool,
        );
        Tup::abi_encode_params(&(
            recipient,
            amount_in,
            amount_out_min,
            path.to_vec(),
            payer_is_user,
        ))
    }

    #[test]
    fn decode_universal_router_single_v3_swap_exact_in() {
        // A single-opcode UR call mirroring the simplest WETH→USDC swap
        // a Universal-Router-backed front-end would emit. Decoder must
        // surface one `Protocol::UniswapV3` swap with the path's
        // (token_in, token_out, fee) intact.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let path = pack_v3_path(weth, 3000, usdc);
        let recipient = address!("000000000000000000000000000000000000dEaD");
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        let amount_out_min = U256::from(2_500_000_000u128);
        let inputs: Vec<Bytes> =
            vec![encode_v3_exact_in_input(recipient, amount_in, amount_out_min, &path, true).into()];
        let commands = Bytes::from(vec![0x00]); // V3_SWAP_EXACT_IN
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
        assert_eq!(swaps[0].token_in, weth);
        assert_eq!(swaps[0].token_out, usdc);
        assert_eq!(swaps[0].fee_bps, 3000);
        assert_eq!(swaps[0].amount_in, amount_in);
        assert_eq!(swaps[0].amount_out_min, amount_out_min);
        assert_eq!(swaps[0].recipient, recipient);
        assert_eq!(swaps[0].router, ur);
    }

    #[test]
    fn decode_universal_router_v4_swap_silently_skipped() {
        // V4_SWAP wraps a Uniswap V4 PoolManager action stream — out
        // of scope for the current V2/V3 backrun target surface. The
        // dispatcher must drop it without erroring so a UR call that
        // mixes V4_SWAP with a recognised V2/V3 opcode still surfaces
        // the recognised swap.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let path = pack_v3_path(weth, 3000, usdc);
        let inputs: Vec<Bytes> = vec![
            // Opaque V4 action stream placeholder — decoder reads len
            // but never parses the body.
            vec![0u8; 64].into(),
            encode_v3_exact_in_input(Address::ZERO, U256::from(1u64), U256::ZERO, &path, true)
                .into(),
        ];
        let commands = Bytes::from(vec![ur_commands::V4_SWAP, ur_commands::V3_SWAP_EXACT_IN]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(
            swaps.len(),
            1,
            "V4_SWAP must be dropped, the V3 swap alongside must survive"
        );
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
        assert_eq!(swaps[0].fee_bps, 3000);
    }

    #[test]
    fn decode_universal_router_skips_non_swap_opcodes() {
        // Real UR payloads almost always wrap a swap with `WRAP_ETH`
        // before and `UNWRAP_WETH` after. Those must be dropped silently
        // — the decoder is only interested in opcodes that move pool
        // state.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let path = pack_v3_path(weth, 500, usdc);
        let inputs: Vec<Bytes> = vec![
            vec![0u8; 32].into(), // WRAP_ETH placeholder input (opaque to us)
            encode_v3_exact_in_input(
                Address::ZERO,
                U256::from(1u64),
                U256::ZERO,
                &path,
                true,
            )
            .into(),
            vec![0u8; 32].into(), // UNWRAP_WETH placeholder input
        ];
        let commands = Bytes::from(vec![0x0b, 0x00, 0x0c]); // WRAP_ETH, V3_SWAP_EXACT_IN, UNWRAP_WETH
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(
            swaps.len(),
            1,
            "wrap / unwrap opcodes must be skipped, only the swap remains"
        );
        assert_eq!(swaps[0].fee_bps, 500);
    }

    #[test]
    fn decode_universal_router_mismatched_commands_inputs_rejected() {
        // Commands length must equal inputs length. A mismatch indicates
        // a malformed payload that we shouldn't half-process.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let calldata = execute_0Call {
            commands: Bytes::from(vec![0x00, 0x00]),
            inputs: vec![vec![0u8; 32].into()], // 1 input but 2 commands
        }
        .abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let err = decode_pending_many(ur, &calldata).unwrap_err();
        assert!(
            matches!(err, DecodeError::AbiDecode(_)),
            "expected AbiDecode for length mismatch, got {:?}",
            err
        );
    }

    #[test]
    fn decode_universal_router_with_deadline_overload() {
        // The deadline-bearing `execute_1` is the most common entry point
        // for wallet-issued UR calls; cover it alongside `execute_0`.
        use IUniversalRouter::execute_1Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let path = pack_v3_path(weth, 10000, usdc);
        let inputs: Vec<Bytes> = vec![
            encode_v3_exact_in_input(Address::ZERO, U256::from(42u64), U256::ZERO, &path, true)
                .into(),
        ];
        let commands = Bytes::from(vec![0x00]);
        let calldata = execute_1Call {
            commands,
            inputs,
            deadline: U256::from(99u64),
        }
        .abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].fee_bps, 10000);
        assert_eq!(swaps[0].amount_in, U256::from(42u64));
    }

    /// Encode V3 exact-out tuple. Same ABI shape as exact-in — the
    /// difference is the path orientation (token_out first) and the
    /// semantic re-mapping the decoder applies.
    fn encode_v3_exact_out_input(
        recipient: Address,
        amount_out: U256,
        amount_in_max: U256,
        path: &[u8],
        payer_is_user: bool,
    ) -> Vec<u8> {
        encode_v3_exact_in_input(recipient, amount_out, amount_in_max, path, payer_is_user)
    }

    /// Encode V2 swap tuple `(recipient, amount, amount_limit, address[] path, bool)`.
    fn encode_v2_input(
        recipient: Address,
        amount: U256,
        amount_limit: U256,
        path: Vec<Address>,
        payer_is_user: bool,
    ) -> Vec<u8> {
        use alloy::sol_types::{SolType, sol_data};
        type V2Tuple = (
            sol_data::Address,
            sol_data::Uint<256>,
            sol_data::Uint<256>,
            sol_data::Array<sol_data::Address>,
            sol_data::Bool,
        );
        V2Tuple::abi_encode_params(&(recipient, amount, amount_limit, path, payer_is_user))
    }

    #[test]
    fn decode_universal_router_v3_swap_exact_out_reverses_path() {
        // V3 exact-out paths are encoded token_out → token_in. The
        // decoder must surface `(token_in, token_out)` in canonical
        // pool-pair order so registry lookups land on the same edge as
        // the equivalent exact-in swap would.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        // Encoded order: token_out (USDC) | fee | token_in (WETH).
        let path = pack_v3_path(usdc, 3000, weth);
        let amount_out = U256::from(2_500_000_000u128);
        let amount_in_max = U256::from(2_000_000_000_000_000_000u128);
        let inputs: Vec<Bytes> = vec![encode_v3_exact_out_input(
            Address::ZERO,
            amount_out,
            amount_in_max,
            &path,
            true,
        )
        .into()];
        let commands = Bytes::from(vec![ur_commands::V3_SWAP_EXACT_OUT]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
        assert_eq!(swaps[0].token_in, weth, "exact-out path must be flipped");
        assert_eq!(swaps[0].token_out, usdc);
        assert_eq!(swaps[0].fee_bps, 3000);
        assert_eq!(
            swaps[0].amount_in, amount_out,
            "exact-out surfaces user-committed amountOut as amount_in"
        );
        assert_eq!(swaps[0].amount_out_min, amount_in_max);
    }

    #[test]
    fn decode_universal_router_v2_swap_exact_in() {
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        let amount_out_min = U256::from(2_500_000_000u128);
        let inputs: Vec<Bytes> = vec![encode_v2_input(
            Address::ZERO,
            amount_in,
            amount_out_min,
            vec![weth, dai, usdc],
            true,
        )
        .into()];
        let commands = Bytes::from(vec![ur_commands::V2_SWAP_EXACT_IN]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].protocol, Protocol::UniswapV2);
        assert_eq!(swaps[0].token_in, weth);
        assert_eq!(swaps[0].token_out, dai);
        assert_eq!(swaps[0].path_extra, vec![usdc]);
        assert_eq!(swaps[0].amount_in, amount_in);
        assert_eq!(swaps[0].amount_out_min, amount_out_min);
        assert_eq!(swaps[0].fee_bps, 0, "V2 has no per-pool fee tier");
    }

    #[test]
    fn decode_universal_router_v2_swap_exact_out() {
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let amount_out = U256::from(2_500_000_000u128);
        let amount_in_max = U256::from(2_000_000_000_000_000_000u128);
        let inputs: Vec<Bytes> = vec![encode_v2_input(
            Address::ZERO,
            amount_out,
            amount_in_max,
            vec![weth, usdc], // V2 path is NOT reversed for exact-out
            true,
        )
        .into()];
        let commands = Bytes::from(vec![ur_commands::V2_SWAP_EXACT_OUT]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].protocol, Protocol::UniswapV2);
        assert_eq!(swaps[0].token_in, weth);
        assert_eq!(swaps[0].token_out, usdc);
        assert_eq!(swaps[0].amount_in, amount_out);
        assert_eq!(swaps[0].amount_out_min, amount_in_max);
    }

    #[test]
    fn decode_universal_router_v2_swap_rejects_empty_path() {
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let inputs: Vec<Bytes> = vec![encode_v2_input(
            Address::ZERO,
            U256::from(1u64),
            U256::ZERO,
            vec![], // empty path
            true,
        )
        .into()];
        let commands = Bytes::from(vec![ur_commands::V2_SWAP_EXACT_IN]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let err = decode_pending_many(ur, &calldata).unwrap_err();
        assert!(matches!(err, DecodeError::EmptyPath));
    }

    #[test]
    fn decode_universal_router_mixed_v2_and_v3_swap_chain() {
        // Realistic UR payload — V2 hop into WETH, V3 hop out of WETH —
        // must produce two swap records, one per protocol, in order.
        use IUniversalRouter::execute_0Call;
        use alloy::primitives::Bytes;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let dai = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
        let v2_path = vec![dai, weth];
        let v3_path = pack_v3_path(weth, 500, usdc);
        let inputs: Vec<Bytes> = vec![
            encode_v2_input(Address::ZERO, U256::from(1u64), U256::ZERO, v2_path, true).into(),
            encode_v3_exact_in_input(Address::ZERO, U256::from(1u64), U256::ZERO, &v3_path, false)
                .into(),
        ];
        let commands = Bytes::from(vec![
            ur_commands::V2_SWAP_EXACT_IN,
            ur_commands::V3_SWAP_EXACT_IN,
        ]);
        let calldata = execute_0Call { commands, inputs }.abi_encode();
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &calldata).expect("decode");
        assert_eq!(swaps.len(), 2, "one record per swap-typed opcode");
        assert_eq!(swaps[0].protocol, Protocol::UniswapV2);
        assert_eq!(swaps[0].token_in, dai);
        assert_eq!(swaps[0].token_out, weth);
        assert_eq!(swaps[1].protocol, Protocol::UniswapV3);
        assert_eq!(swaps[1].token_in, weth);
        assert_eq!(swaps[1].token_out, usdc);
        assert_eq!(swaps[1].fee_bps, 500);
    }

    // ── Real-calldata fixtures captured from mainnet ──
    //
    // Each `INPUT_*` constant is the raw `eth_getTransactionByHash` →
    // `input` payload of a real Universal Router pending tx. The tests
    // below decode the bytes through the production dispatcher and
    // assert against the protocol / token / fee values that match the
    // on-chain receipt for that transaction. Synthetic `sol!`-encoded
    // tests can mask encoder-vs-decoder drift because both sides use
    // the same Rust macro; these fixtures pin the decoder against
    // bytes that actually existed on chain.

    fn hex_to_bytes(s: &str) -> Vec<u8> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// Mainnet tx 0xfbb854...e8996 — Universal Router V2 at block
    /// 25187135. Commands stream `[V2_SWAP_EXACT_IN, SWEEP]`. Path:
    /// `WETH → 0xf280B16ef293D8e534e370794ef26bf3126941`.
    const FIXTURE_UR_V2_SWAP_EXACT_IN: &str = "0x3593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000006a16fb9a00000000000000000000000000000000000000000000000000000000000000020804000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000160000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000002924df0c1eec1024000000000000000000000000000000000000000000000000008b687025a73bfa00000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000f280b16ef293d8e534e370794ef26bf3126941260000000000000000000000000000000000000000000000000000000000000060000000000000000000000000f280b16ef293d8e534e370794ef26bf3126941260000000000000000000000009fb87dc0bd17f9ac26f3ca5d7b49edac54cd1021000000000000000000000000000000000000000000000000008b687025a73bfa0c";

    #[test]
    fn fixture_ur_v2_swap_exact_in_decodes() {
        let bytes = hex_to_bytes(FIXTURE_UR_V2_SWAP_EXACT_IN);
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &bytes).expect("decode fixture");
        assert_eq!(
            swaps.len(),
            1,
            "two-opcode stream [V2_SWAP_EXACT_IN, SWEEP] must produce exactly one swap"
        );
        assert_eq!(swaps[0].protocol, Protocol::UniswapV2);
        assert_eq!(
            swaps[0].token_in,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            "token_in must be canonical WETH"
        );
        assert_eq!(
            swaps[0].token_out,
            address!("f280B16ef293D8e534e370794Ef26bf312694126"),
        );
        assert_eq!(
            swaps[0].fee_bps, 0,
            "V2 swap must surface fee_bps=0 (registry-resolved)"
        );
        // Two-token path → no extras carry forward.
        assert!(swaps[0].path_extra.is_empty());
        // amount_in is the user-committed WETH amount; non-zero is
        // sufficient to prove the field was parsed off the live bytes
        // rather than left as the U256::ZERO default.
        assert!(swaps[0].amount_in > U256::ZERO);
    }

    /// Mainnet tx 0x87a841...4d4e33 — Universal Router V1.2.
    /// Commands stream `[WRAP_ETH, V3_SWAP_EXACT_OUT, UNWRAP_WETH]`.
    /// V3 path orientation is reversed (token_out first); the decoder
    /// must surface `(token_in, token_out)` in canonical pair order.
    const FIXTURE_UR_V3_SWAP_EXACT_OUT_WITH_WRAP: &str = "0x3593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000006a16fb2600000000000000000000000000000000000000000000000000000000000000030b010c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000003b6c589dc64cba0000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000040309c4d2fb46000000000000000000000000000000000000000000000000000000003b6c589dc64cba00000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002b974733a3f37208647577bb925d8ee854c7337e29002710c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn fixture_ur_v3_swap_exact_out_decodes() {
        let bytes = hex_to_bytes(FIXTURE_UR_V3_SWAP_EXACT_OUT_WITH_WRAP);
        let ur_v12 = address!("3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD");
        let swaps = decode_pending_many(ur_v12, &bytes).expect("decode fixture");
        assert_eq!(
            swaps.len(),
            1,
            "WRAP_ETH + V3_SWAP_EXACT_OUT + UNWRAP_WETH yields exactly one swap"
        );
        assert_eq!(swaps[0].protocol, Protocol::UniswapV3);
        // On-chain path (encoded reversed): 0x974733...e29 | fee(0x002710 = 10000) | WETH.
        // Decoder flips to canonical (token_in, token_out).
        assert_eq!(
            swaps[0].token_in,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            "token_in must be canonical WETH after path flip"
        );
        assert_eq!(
            swaps[0].token_out,
            address!("974733a3F37208647577bB925D8eE854c7337E29"),
        );
        assert_eq!(
            swaps[0].fee_bps, 10000,
            "1% fee tier expected from the on-chain path"
        );
        assert!(swaps[0].amount_in > U256::ZERO);
        assert!(swaps[0].amount_out_min > U256::ZERO);
    }

    /// Mainnet tx 0x637397...0b1f — Universal Router V2.
    /// Commands stream `[V2_SWAP_EXACT_IN, UNWRAP_WETH]`. Path:
    /// `0x759f8b...e987 → WETH`.
    const FIXTURE_UR_V2_SWAP_TO_WETH: &str = "0x3593564c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000006a16faab0000000000000000000000000000000000000000000000000000000000000002080c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000160000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000005731794b6c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002000000000000000000000000759f8b83db57033aed95545694f6ada480b6e987000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000000000000000000000000000000000000000004000000000000000000000000073900ee0e71ca58382e1ce9f0d176faca2216812000000000000000000000000000000000000000000000000027b66f0a4ac9385";

    #[test]
    fn fixture_ur_v2_swap_to_weth_decodes() {
        let bytes = hex_to_bytes(FIXTURE_UR_V2_SWAP_TO_WETH);
        let ur = address!("66a9893cC07D91D95644AEDD05D03f95e1dBA8Af");
        let swaps = decode_pending_many(ur, &bytes).expect("decode fixture");
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].protocol, Protocol::UniswapV2);
        assert_eq!(
            swaps[0].token_in,
            address!("759f8B83db57033aeD95545694f6Ada480B6e987"),
        );
        assert_eq!(
            swaps[0].token_out,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            "token_out must be canonical WETH"
        );
        assert_eq!(swaps[0].fee_bps, 0);
        assert!(swaps[0].amount_in > U256::ZERO);
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
        use IUniswapV3Router::{exactInputSingle_0Call, ExactInputSingleParams};
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
        let calldata = exactInputSingle_0Call { params }.abi_encode();
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

    #[test]
    fn decode_v2_remove_liquidity_eth_silently_skipped() {
        // `removeLiquidityETH` is selector 0x02751cec and accounted for
        // ~22% of the `unknown_selector` noise in the most recent shadow
        // run. It is an LP edit, not a swap — the dispatcher must return
        // an empty record set rather than bubble an UnknownSelector error.
        use IUniswapV2Router02::removeLiquidityETHCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let calldata = removeLiquidityETHCall {
            token: weth,
            liquidity: U256::from(1u64),
            amountTokenMin: U256::ZERO,
            amountETHMin: U256::ZERO,
            to: Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let uni_v2_router = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let swaps = decode_pending_many(uni_v2_router, &calldata).expect("decode");
        assert!(
            swaps.is_empty(),
            "liquidity-management entry points must not produce swap records"
        );
    }

    #[test]
    fn decode_v2_add_liquidity_eth_silently_skipped() {
        // Mirrors the remove-side test for the additive selector
        // (0xf305d719) so a regression that re-introduces it as
        // UnknownSelector is caught directly.
        use IUniswapV2Router02::addLiquidityETHCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let calldata = addLiquidityETHCall {
            token: weth,
            amountTokenDesired: U256::from(1u64),
            amountTokenMin: U256::ZERO,
            amountETHMin: U256::ZERO,
            to: Address::ZERO,
            deadline: U256::ZERO,
        }
        .abi_encode();
        let uni_v2_router = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let swaps = decode_pending_many(uni_v2_router, &calldata).expect("decode");
        assert!(swaps.is_empty());
    }

    #[test]
    fn decode_v2_remove_liquidity_with_permit_silently_skipped() {
        // The permit / FOT siblings sit on the same code path; assert one
        // representative permit-style entry to lock the cohort in.
        use IUniswapV2Router02::removeLiquidityETHWithPermitCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let calldata = removeLiquidityETHWithPermitCall {
            token: weth,
            liquidity: U256::from(1u64),
            amountTokenMin: U256::ZERO,
            amountETHMin: U256::ZERO,
            to: Address::ZERO,
            deadline: U256::ZERO,
            approveMax: false,
            v: 0,
            r: Default::default(),
            s: Default::default(),
        }
        .abi_encode();
        let uni_v2_router = address!("7a250d5630B4cF539739dF2C5dAcb4c659F2488D");
        let swaps = decode_pending_many(uni_v2_router, &calldata).expect("decode");
        assert!(swaps.is_empty());
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
            minReturnAmount: U256::from(2_500u64) * U256::from(1_000_000_000_000_000_000u128),    // 2500 BNT
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
        assert_eq!(decoded.amount_out_min, U256::from(2_500u64) * U256::from(1_000_000_000_000_000_000u128));
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
        use IUniswapV3Router::{exactInputSingle_0Call, ExactInputSingleParams};
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
        exactInputSingle_0Call { params }.abi_encode()
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
        use IUniswapV3Router::{exactOutputSingle_0Call, ExactOutputSingleParams};
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
        let calldata = exactOutputSingle_0Call { params }.abi_encode();
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

    // ── 1inch v6 AggregationRouter decoder ──

    /// Pack a `(pool_address, zero_for_one)` pair into a `uint256` matching
    /// 1inch v6's expected pool-word layout. Used to build hand-rolled
    /// calldata for the `unoswap*` / `uniswapV3Swap*` tests without
    /// pulling in a full Solidity helper.
    fn encode_pool(pool: Address, zero_for_one: bool) -> U256 {
        let mut be = [0u8; 32];
        be[12..32].copy_from_slice(pool.as_slice());
        let mut v = U256::from_be_bytes(be);
        if zero_for_one {
            v |= U256::from(1u64) << one_inch_bits::ZERO_FOR_ONE_BIT;
        }
        v
    }

    fn one_inch_router() -> Address {
        ONE_INCH_V6_ROUTER
    }

    #[test]
    fn pool_zero_for_one_round_trips_pool_address_and_flag() {
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let encoded = encode_pool(pool, true);
        let (got_pool, got_flag) = pool_zero_for_one(encoded);
        assert_eq!(got_pool, pool);
        assert!(got_flag);

        let encoded_off = encode_pool(pool, false);
        let (got_pool2, got_flag2) = pool_zero_for_one(encoded_off);
        assert_eq!(got_pool2, pool);
        assert!(!got_flag2);
    }

    #[test]
    fn decode_one_inch_v6_swap_emits_unresolved_executor_record() {
        // The opaque-executor path can't be mapped to a concrete pool.
        // Decoder must still emit a record (token pair + amount) and
        // leave `pool_address = None` so the pipeline tags it
        // `unresolved_executor` rather than dropping it silently.
        use IOneInchV6Router::{swapCall, SwapDescription};
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let executor = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let receiver = address!("000000000000000000000000000000000000dEaD");
        let desc = SwapDescription {
            srcToken: weth,
            dstToken: usdc,
            srcReceiver: executor,
            dstReceiver: receiver,
            amount: U256::from(1_000_000_000_000_000_000u128),
            minReturnAmount: U256::from(2_500_000_000u128),
            flags: U256::ZERO,
        };
        let calldata = swapCall {
            executor,
            desc,
            data: alloy::primitives::Bytes::new(),
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1, "swap() emits exactly one record");
        let r = &many[0];
        assert_eq!(r.protocol, Protocol::OneInchV6);
        assert_eq!(r.router, one_inch_router());
        assert_eq!(r.token_in, weth);
        assert_eq!(r.token_out, usdc);
        assert_eq!(r.amount_in, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(r.amount_out_min, U256::from(2_500_000_000u128));
        assert_eq!(r.recipient, receiver);
        assert!(
            r.pool_address.is_none(),
            "opaque executor must leave pool_address None for unresolved_executor tagging"
        );
        assert!(r.one_inch_zero_for_one.is_none());
    }

    #[test]
    fn decode_one_inch_v6_unoswap_single_pool() {
        use IOneInchV6Router::unoswapCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let amount = U256::from(1_000_000_000_000_000_000u128);
        let min_return = U256::from(2_500_000_000u128);
        let mut token_be = [0u8; 32];
        token_be[12..32].copy_from_slice(weth.as_slice());
        let calldata = unoswapCall {
            token: U256::from_be_bytes(token_be),
            amount,
            minReturn: min_return,
            dex: encode_pool(pool, true),
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1);
        let r = &many[0];
        assert_eq!(r.protocol, Protocol::OneInchV6);
        assert_eq!(r.token_in, weth, "first hop carries the user's src token");
        assert_eq!(r.pool_address, Some(pool));
        assert_eq!(r.one_inch_zero_for_one, Some(true));
        assert_eq!(r.amount_in, amount);
        assert_eq!(r.amount_out_min, min_return);
    }

    #[test]
    fn decode_one_inch_v6_unoswap3_three_pools_amount_only_on_first() {
        use IOneInchV6Router::unoswap3Call;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let p1 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1");
        let p2 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb2");
        let p3 = address!("ccccccccccccccccccccccccccccccccccccccc3");
        let amount = U256::from(123_456_789u64);
        let min_return = U256::from(99_999u64);
        let mut token_be = [0u8; 32];
        token_be[12..32].copy_from_slice(weth.as_slice());
        let calldata = unoswap3Call {
            token: U256::from_be_bytes(token_be),
            amount,
            minReturn: min_return,
            dex: encode_pool(p1, false),
            dex2: encode_pool(p2, true),
            dex3: encode_pool(p3, false),
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 3, "one record per pool in the chain");

        assert_eq!(many[0].pool_address, Some(p1));
        assert_eq!(many[0].one_inch_zero_for_one, Some(false));
        assert_eq!(many[0].token_in, weth, "src token only on first hop");
        assert_eq!(many[0].amount_in, amount, "amount only on first hop");
        assert_eq!(many[0].amount_out_min, U256::ZERO);

        assert_eq!(many[1].pool_address, Some(p2));
        assert_eq!(many[1].one_inch_zero_for_one, Some(true));
        assert_eq!(many[1].token_in, Address::ZERO);
        assert_eq!(many[1].amount_in, U256::ZERO);
        assert_eq!(many[1].amount_out_min, U256::ZERO);

        assert_eq!(many[2].pool_address, Some(p3));
        assert_eq!(many[2].one_inch_zero_for_one, Some(false));
        assert_eq!(many[2].amount_in, U256::ZERO);
        assert_eq!(
            many[2].amount_out_min, min_return,
            "final hop carries the user's slippage bound"
        );
    }

    #[test]
    fn decode_one_inch_v6_uniswap_v3_swap_with_direction_flag() {
        use IOneInchV6Router::uniswapV3SwapCall;
        let p = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let amount = U256::from(7_000u64);
        let min_return = U256::from(6_900u64);
        let calldata = uniswapV3SwapCall {
            amount,
            minReturn: min_return,
            pools: vec![encode_pool(p, true)],
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1);
        let r = &many[0];
        assert_eq!(r.protocol, Protocol::OneInchV6);
        assert_eq!(r.pool_address, Some(p));
        assert_eq!(r.one_inch_zero_for_one, Some(true));
        assert_eq!(
            r.token_in, Address::ZERO,
            "uniswapV3Swap has no calldata src token — pipeline resolves from pool + zeroForOne"
        );
        assert_eq!(r.amount_in, amount);
        assert_eq!(r.amount_out_min, min_return);
    }

    #[test]
    fn decode_one_inch_v6_uniswap_v3_swap_empty_pools_rejected() {
        use IOneInchV6Router::uniswapV3SwapCall;
        let calldata = uniswapV3SwapCall {
            amount: U256::from(1u64),
            minReturn: U256::ZERO,
            pools: vec![],
        }
        .abi_encode();
        let err = decode_pending_many(one_inch_router(), &calldata).unwrap_err();
        assert!(
            matches!(err, DecodeError::EmptyPath),
            "empty pool list should reject as EmptyPath, got {err:?}"
        );
    }

    #[test]
    fn decode_one_inch_v6_eth_unoswap_treats_src_as_weth() {
        // ethUnoswap omits the token arg — src is native ETH which the
        // router wraps to WETH. Decoder surfaces WETH as token_in so
        // downstream pool lookups key off the wrapped token.
        use IOneInchV6Router::ethUnoswapCall;
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let calldata = ethUnoswapCall {
            minReturn: U256::from(42u64),
            dex: encode_pool(pool, false),
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1);
        let r = &many[0];
        assert_eq!(r.token_in, WETH_ADDRESS);
        assert_eq!(r.pool_address, Some(pool));
        // amount comes from msg.value, not calldata — surfaced as zero
        // because the pending-tx layer carries value separately.
        assert_eq!(r.amount_in, U256::ZERO);
        assert_eq!(r.amount_out_min, U256::from(42u64));
    }

    #[test]
    fn decode_one_inch_v6_unoswap_to_carries_recipient() {
        use IOneInchV6Router::unoswapToCall;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let recipient = address!("000000000000000000000000000000000000dEaD");
        let mut token_be = [0u8; 32];
        token_be[12..32].copy_from_slice(weth.as_slice());
        let mut to_be = [0u8; 32];
        to_be[12..32].copy_from_slice(recipient.as_slice());
        let calldata = unoswapToCall {
            to: U256::from_be_bytes(to_be),
            token: U256::from_be_bytes(token_be),
            amount: U256::from(1_000u64),
            minReturn: U256::from(900u64),
            dex: encode_pool(pool, true),
        }
        .abi_encode();
        let many = decode_pending_many(one_inch_router(), &calldata).expect("decode");
        assert_eq!(many.len(), 1);
        assert_eq!(many[0].recipient, recipient);
        assert_eq!(many[0].pool_address, Some(pool));
    }

    #[test]
    fn decode_pending_single_helper_returns_first_record_for_chain() {
        // The single-record `decode_pending` collapses a multi-pool 1inch
        // chain to the first hop. Helpful for call sites that haven't yet
        // migrated to `decode_pending_many` but still want decode_failure
        // numbers to drop on real traffic.
        use IOneInchV6Router::unoswap2Call;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let p1 = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        let p2 = address!("11b815efB8f581194ae79006d24E0d814B7697F6");
        let mut token_be = [0u8; 32];
        token_be[12..32].copy_from_slice(weth.as_slice());
        let calldata = unoswap2Call {
            token: U256::from_be_bytes(token_be),
            amount: U256::from(1_000u64),
            minReturn: U256::from(900u64),
            dex: encode_pool(p1, true),
            dex2: encode_pool(p2, false),
        }
        .abi_encode();
        let single = decode_pending(one_inch_router(), &calldata).expect("decode");
        assert_eq!(single.pool_address, Some(p1));
        assert_eq!(single.token_in, weth);
    }
}
