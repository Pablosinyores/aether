use aether_common::types::SwapStep;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use tracing::debug;

// ABI for AetherExecutor.executeArb
sol! {
    struct SolSwapStep {
        uint8 protocol;
        address pool;
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint256 minAmountOut;
        bytes data;
    }

    function executeArb(
        SolSwapStep[] steps,
        address flashloanToken,
        uint256 flashloanAmount,
        uint256 deadline,
        uint256 minProfitOut,
        uint256 tipBps
    );
}

/// Build the calldata for AetherExecutor.executeArb()
pub fn build_execute_arb_calldata(
    steps: &[SwapStep],
    flashloan_token: Address,
    flashloan_amount: U256,
    deadline: U256,
    min_profit_out: U256,
    tip_bps: U256,
) -> Vec<u8> {
    let sol_steps: Vec<SolSwapStep> = steps
        .iter()
        .map(|s| SolSwapStep {
            protocol: s.protocol as u8,
            pool: s.pool_address,
            tokenIn: s.token_in,
            tokenOut: s.token_out,
            amountIn: s.amount_in,
            minAmountOut: s.min_amount_out,
            data: s.calldata.clone().into(),
        })
        .collect();

    let call = executeArbCall {
        steps: sol_steps,
        flashloanToken: flashloan_token,
        flashloanAmount: flashloan_amount,
        deadline,
        minProfitOut: min_profit_out,
        tipBps: tip_bps,
    };

    debug!(
        num_steps = steps.len(),
        %flashloan_token,
        %flashloan_amount,
        %deadline,
        %tip_bps,
        "Built executeArb calldata"
    );

    call.abi_encode()
}

/// Build calldata for a Uniswap V2-style swap.
/// swap(uint amount0Out, uint amount1Out, address to, bytes data)
pub fn build_univ2_swap_calldata(
    amount0_out: U256,
    amount1_out: U256,
    to: Address,
) -> Vec<u8> {
    sol! {
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes data);
    }
    let call = swapCall {
        amount0Out: amount0_out,
        amount1Out: amount1_out,
        to,
        data: Bytes::new(),
    };
    call.abi_encode()
}

/// Build calldata for a Uniswap V3-style swap.
/// swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes data)
pub fn build_univ3_swap_calldata(
    recipient: Address,
    zero_for_one: bool,
    amount_specified: i128,
    sqrt_price_limit_x96: U256,
) -> Vec<u8> {
    sol! {
        function swap(
            address recipient,
            bool zeroForOne,
            int256 amountSpecified,
            uint160 sqrtPriceLimitX96,
            bytes data
        );
    }
    // Convert i128 to I256
    let amount_i256 = alloy::primitives::I256::try_from(amount_specified).unwrap_or_default();
    // Convert U256 to uint160 - clamp to max uint160
    let max_uint160 = U256::from(1u8).wrapping_shl(160) - U256::from(1u8);
    let clamped = sqrt_price_limit_x96.min(max_uint160);
    let limit_u160 = alloy::primitives::Uint::<160, 3>::from_limbs_slice(
        &clamped.into_limbs()[..3],
    );
    let call = swapCall {
        recipient,
        zeroForOne: zero_for_one,
        amountSpecified: amount_i256,
        sqrtPriceLimitX96: limit_u160,
        data: Bytes::new(),
    };
    call.abi_encode()
}

/// Build calldata for a Curve StableSwap `exchange`.
///
/// ABI: `function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) returns (uint256)`.
/// Selector: `0x3df02124`. `i`/`j` are the pool's token indices (small signed ints).
pub fn build_curve_swap_calldata(i: i128, j: i128, dx: U256, min_dy: U256) -> Vec<u8> {
    sol! {
        function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) returns (uint256);
    }
    // alloy's sol! lowers Solidity int128 to Rust's primitive i128 in the generated call
    // struct — Curve indices are always small so i128 is both exact and appropriate.
    let call = exchangeCall { i, j, dx, min_dy };
    call.abi_encode()
}

/// Build calldata for a Balancer V2 Vault `swap` (single-pool, GIVEN_IN kind).
///
/// ABI:
/// ```solidity
/// function swap(SingleSwap singleSwap, FundManagement funds, uint256 limit, uint256 deadline);
/// ```
/// Populated with `kind = GIVEN_IN (0)`, empty `userData`, and `sender == recipient == executor`
/// with both internal-balance flags false (direct ERC20 transfer from/to the executor).
pub fn build_balancer_swap_calldata(
    pool_id: [u8; 32],
    asset_in: Address,
    asset_out: Address,
    amount_in: U256,
    min_amount_out: U256,
    executor: Address,
    deadline: U256,
) -> Vec<u8> {
    sol! {
        enum SwapKind { GIVEN_IN, GIVEN_OUT }

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

        function swap(
            SingleSwap singleSwap,
            FundManagement funds,
            uint256 limit,
            uint256 deadline
        ) returns (uint256);
    }

    let call = swapCall {
        singleSwap: SingleSwap {
            poolId: pool_id.into(),
            kind: 0, // GIVEN_IN — arbs always route exact-input
            assetIn: asset_in,
            assetOut: asset_out,
            amount: amount_in,
            userData: Bytes::new(),
        },
        funds: FundManagement {
            sender: executor,
            fromInternalBalance: false,
            recipient: executor,
            toInternalBalance: false,
        },
        limit: min_amount_out,
        deadline,
    };
    call.abi_encode()
}

/// Build calldata for a Bancor V3 `tradeBySourceAmount`.
///
/// ABI: `function tradeBySourceAmount(address, address, uint256, uint256, uint256, address) returns (uint256)`.
/// `beneficiary` is the recipient of the output tokens (the executor at runtime).
pub fn build_bancor_swap_calldata(
    source_token: Address,
    target_token: Address,
    source_amount: U256,
    min_return: U256,
    deadline: U256,
    beneficiary: Address,
) -> Vec<u8> {
    sol! {
        function tradeBySourceAmount(
            address sourceToken,
            address targetToken,
            uint256 sourceAmount,
            uint256 minReturnAmount,
            uint256 deadline,
            address beneficiary
        ) returns (uint256);
    }
    let call = tradeBySourceAmountCall {
        sourceToken: source_token,
        targetToken: target_token,
        sourceAmount: source_amount,
        minReturnAmount: min_return,
        deadline,
        beneficiary,
    };
    call.abi_encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_common::types::ProtocolType;
    use alloy::primitives::address;

    #[test]
    fn test_build_execute_arb_calldata_single_step() {
        let steps = vec![SwapStep {
            protocol: ProtocolType::UniswapV2,
            pool_address: address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
            token_in: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            token_out: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            amount_in: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
            min_amount_out: U256::from(1_980_000_000u64),         // ~1980 USDC
            calldata: vec![],
        }];

        let flashloan_token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let flashloan_amount = U256::from(1_000_000_000_000_000_000u128);

        let deadline = U256::from(1_700_000_000u64 + 120);
        let calldata = build_execute_arb_calldata(
            &steps,
            flashloan_token,
            flashloan_amount,
            deadline,
            U256::ZERO,
            U256::from(9000u64),
        );

        // Should have the function selector (4 bytes) + encoded data
        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4); // At minimum: 4-byte selector

        // Verify function selector matches executeArb(...)
        let selector = &calldata[0..4];
        let expected_selector = &executeArbCall::SELECTOR;
        assert_eq!(selector, expected_selector);
    }

    #[test]
    fn test_build_execute_arb_calldata_multi_step() {
        let steps = vec![
            SwapStep {
                protocol: ProtocolType::UniswapV2,
                pool_address: address!("B4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"),
                token_in: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                token_out: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                amount_in: U256::from(1_000_000_000_000_000_000u128),
                min_amount_out: U256::from(1_980_000_000u64),
                calldata: vec![],
            },
            SwapStep {
                protocol: ProtocolType::SushiSwap,
                pool_address: address!("397FF1542f962076d0BFE58eA045FfA2d347ACa0"),
                token_in: address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
                token_out: address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                amount_in: U256::from(1_980_000_000u64),
                min_amount_out: U256::from(1_005_000_000_000_000_000u128),
                calldata: vec![],
            },
        ];

        let flashloan_token = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let flashloan_amount = U256::from(1_000_000_000_000_000_000u128);

        let deadline = U256::from(1_700_000_000u64 + 120);
        let tip_bps = U256::from(9000u64);
        let calldata = build_execute_arb_calldata(
            &steps,
            flashloan_token,
            flashloan_amount,
            deadline,
            U256::ZERO,
            tip_bps,
        );

        assert!(!calldata.is_empty());
        // Multi-step calldata should be larger than single-step
        let single_step_calldata = build_execute_arb_calldata(
            &steps[..1],
            flashloan_token,
            flashloan_amount,
            deadline,
            U256::ZERO,
            tip_bps,
        );
        assert!(calldata.len() > single_step_calldata.len());
    }

    #[test]
    fn test_build_execute_arb_calldata_empty_steps() {
        let steps: Vec<SwapStep> = vec![];
        let flashloan_token = Address::ZERO;
        let flashloan_amount = U256::ZERO;

        let deadline = U256::from(1_700_000_000u64 + 120);
        let calldata = build_execute_arb_calldata(
            &steps,
            flashloan_token,
            flashloan_amount,
            deadline,
            U256::ZERO,
            U256::ZERO,
        );

        // Should still produce valid ABI-encoded calldata even with empty steps
        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4);
    }

    #[test]
    fn test_build_univ2_swap_calldata() {
        let amount0_out = U256::from(1_000_000_000u64); // 1000 USDC
        let amount1_out = U256::ZERO;
        let to = address!("1111111111111111111111111111111111111111");

        let calldata = build_univ2_swap_calldata(amount0_out, amount1_out, to);

        // Should have function selector + parameters
        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4);

        // Verify the selector matches swap(uint256,uint256,address,bytes)
        // keccak256("swap(uint256,uint256,address,bytes)") first 4 bytes
        let expected_selector: [u8; 4] = [0x02, 0x2c, 0x0d, 0x9f];
        assert_eq!(&calldata[0..4], &expected_selector);
    }

    #[test]
    fn test_build_univ2_swap_calldata_reverse_direction() {
        let amount0_out = U256::ZERO;
        let amount1_out = U256::from(500_000_000_000_000_000u128); // 0.5 ETH
        let to = address!("2222222222222222222222222222222222222222");

        let calldata = build_univ2_swap_calldata(amount0_out, amount1_out, to);

        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4);
    }

    #[test]
    fn test_build_univ3_swap_calldata() {
        let recipient = address!("3333333333333333333333333333333333333333");
        let zero_for_one = true;
        let amount_specified: i128 = 1_000_000_000_000_000_000; // 1 ETH (exact input)
        let sqrt_price_limit_x96 = U256::from(4_295_128_740u64); // MIN_SQRT_RATIO + 1

        let calldata = build_univ3_swap_calldata(
            recipient,
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96,
        );

        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4);
    }

    #[test]
    fn test_build_univ3_swap_calldata_negative_amount() {
        let recipient = address!("4444444444444444444444444444444444444444");
        let zero_for_one = false;
        let amount_specified: i128 = -500_000_000; // exact output: 500 USDC
        let sqrt_price_limit_x96 = U256::MAX; // large value to test clamping

        let calldata = build_univ3_swap_calldata(
            recipient,
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96,
        );

        assert!(!calldata.is_empty());
        assert!(calldata.len() >= 4);
    }

    #[test]
    fn test_protocol_type_encoding() {
        // Verify protocol types map to correct uint8 values in calldata
        let protocols = [
            (ProtocolType::UniswapV2, 1u8),
            (ProtocolType::UniswapV3, 2u8),
            (ProtocolType::SushiSwap, 3u8),
            (ProtocolType::Curve, 4u8),
            (ProtocolType::BalancerV2, 5u8),
            (ProtocolType::BancorV3, 6u8),
        ];

        for (protocol, expected_value) in protocols {
            assert_eq!(protocol as u8, expected_value);
        }
    }

    #[test]
    fn test_calldata_deterministic() {
        let steps = vec![SwapStep {
            protocol: ProtocolType::UniswapV2,
            pool_address: Address::ZERO,
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::from(1000),
            min_amount_out: U256::from(900),
            calldata: vec![0x01, 0x02],
        }];

        let deadline = U256::from(1_700_000_000u64 + 120);
        let tip_bps = U256::from(9000u64);
        let calldata1 =
            build_execute_arb_calldata(&steps, Address::ZERO, U256::from(1000), deadline, U256::ZERO, tip_bps);
        let calldata2 =
            build_execute_arb_calldata(&steps, Address::ZERO, U256::from(1000), deadline, U256::ZERO, tip_bps);

        // Same inputs must produce identical calldata (deterministic)
        assert_eq!(calldata1, calldata2);
    }

    #[test]
    fn test_build_curve_swap_calldata_selector_and_roundtrip() {
        // Exact selector is whatever alloy computes from the ABI string — we re-use the
        // generated SELECTOR const rather than hardcoding it, so a future ABI tweak fails
        // loudly in the builder rather than silently here.
        sol! {
            function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) returns (uint256);
        }
        let calldata = build_curve_swap_calldata(
            0,
            1,
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(995_000_000_000_000_000u128),
        );
        assert_eq!(&calldata[0..4], exchangeCall::SELECTOR.as_slice());
        assert_eq!(calldata.len(), 4 + 4 * 32); // static-word payload
    }

    #[test]
    fn test_build_curve_swap_calldata_varies_with_inputs() {
        let a = build_curve_swap_calldata(0, 1, U256::from(1_000u64), U256::from(990u64));
        let b = build_curve_swap_calldata(1, 0, U256::from(1_000u64), U256::from(990u64));
        assert_ne!(a, b);
        assert_eq!(&a[0..4], &b[0..4]); // same selector
    }

    #[test]
    fn test_build_balancer_swap_calldata_selector() {
        // Declare the canonical Balancer V2 Vault `swap` signature in a local sol! block.
        // alloy derives `swapCall::SELECTOR` from
        //   swap((bytes32,uint8,address,address,uint256,bytes),(address,bool,address,bool),uint256,uint256)
        // which is the ground-truth 4-byte selector. If the builder's sol! block ever has
        // wrong field order or wrong types, the two SELECTOR constants will diverge and
        // this assertion fails — catching the bug that the previous self-referential
        // calldata2 comparison would have missed.
        sol! {
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

            function swap(
                SingleSwap singleSwap,
                FundManagement funds,
                uint256 limit,
                uint256 deadline
            ) returns (uint256);
        }

        let pool_id = [0x42u8; 32];
        let asset_in = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH
        let asset_out = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC
        let executor = address!("1111111111111111111111111111111111111111");
        let calldata = build_balancer_swap_calldata(
            pool_id,
            asset_in,
            asset_out,
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_980_000_000u64),
            executor,
            U256::from(1_700_000_000u64 + 120),
        );
        assert_eq!(&calldata[0..4], swapCall::SELECTOR.as_slice());
        assert!(calldata.len() > 4);
    }

    #[test]
    fn test_build_balancer_swap_calldata_varies_with_direction() {
        let pool_id = [0x42u8; 32];
        let executor = address!("1111111111111111111111111111111111111111");
        let deadline = U256::from(1_700_000_000u64 + 120);
        let a = build_balancer_swap_calldata(
            pool_id,
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            U256::from(1u64),
            U256::ZERO,
            executor,
            deadline,
        );
        let b = build_balancer_swap_calldata(
            pool_id,
            address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            U256::from(1u64),
            U256::ZERO,
            executor,
            deadline,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn test_build_bancor_swap_calldata_selector() {
        let source = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let target = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let beneficiary = address!("1111111111111111111111111111111111111111");
        let calldata = build_bancor_swap_calldata(
            source,
            target,
            U256::from(1_000_000_000_000_000_000u128),
            U256::from(1_980_000_000u64),
            U256::from(1_700_000_000u64 + 120),
            beneficiary,
        );
        // Compare against a second encoding rather than hardcoding the selector —
        // the generated SELECTOR is authoritative.
        sol! {
            function tradeBySourceAmount(
                address sourceToken,
                address targetToken,
                uint256 sourceAmount,
                uint256 minReturnAmount,
                uint256 deadline,
                address beneficiary
            ) returns (uint256);
        }
        assert_eq!(&calldata[0..4], tradeBySourceAmountCall::SELECTOR.as_slice());
        // Selector + 6 * 32-byte words (all static types)
        assert_eq!(calldata.len(), 4 + 6 * 32);
    }

    #[test]
    fn test_build_bancor_swap_calldata_beneficiary_in_payload() {
        let source = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let target = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let beneficiary = address!("2222222222222222222222222222222222222222");
        let calldata = build_bancor_swap_calldata(
            source,
            target,
            U256::from(1u64),
            U256::ZERO,
            U256::from(u64::MAX),
            beneficiary,
        );
        // Last 32-byte word is the beneficiary address (right-aligned in 32 bytes)
        let last_word = &calldata[calldata.len() - 32..];
        assert_eq!(&last_word[12..], beneficiary.as_slice());
    }

    #[test]
    fn test_build_execute_arb_calldata_tip_bps_encoding() {
        let steps: Vec<SwapStep> = vec![];
        let flashloan_token = Address::ZERO;
        let flashloan_amount = U256::ZERO;
        let deadline = U256::from(u64::MAX);

        // Different tip_bps values should produce different calldata
        let calldata_9000 = build_execute_arb_calldata(
            &steps, flashloan_token, flashloan_amount, deadline, U256::ZERO, U256::from(9000u64),
        );
        let calldata_5000 = build_execute_arb_calldata(
            &steps, flashloan_token, flashloan_amount, deadline, U256::ZERO, U256::from(5000u64),
        );
        let calldata_0 = build_execute_arb_calldata(
            &steps, flashloan_token, flashloan_amount, deadline, U256::ZERO, U256::ZERO,
        );

        assert_ne!(calldata_9000, calldata_5000);
        assert_ne!(calldata_9000, calldata_0);
        assert_ne!(calldata_5000, calldata_0);

        // All should have the same function selector
        assert_eq!(&calldata_9000[0..4], &calldata_5000[0..4]);
        assert_eq!(&calldata_9000[0..4], &calldata_0[0..4]);
    }
}
