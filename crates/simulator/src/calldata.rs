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
        uint256 flashloanAmount
    );
}

/// Build the calldata for AetherExecutor.executeArb()
pub fn build_execute_arb_calldata(
    steps: &[SwapStep],
    flashloan_token: Address,
    flashloan_amount: U256,
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
    };

    debug!(
        num_steps = steps.len(),
        %flashloan_token,
        %flashloan_amount,
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

        let calldata = build_execute_arb_calldata(&steps, flashloan_token, flashloan_amount);

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

        let calldata = build_execute_arb_calldata(&steps, flashloan_token, flashloan_amount);

        assert!(!calldata.is_empty());
        // Multi-step calldata should be larger than single-step
        let single_step_calldata = build_execute_arb_calldata(
            &steps[..1],
            flashloan_token,
            flashloan_amount,
        );
        assert!(calldata.len() > single_step_calldata.len());
    }

    #[test]
    fn test_build_execute_arb_calldata_empty_steps() {
        let steps: Vec<SwapStep> = vec![];
        let flashloan_token = Address::ZERO;
        let flashloan_amount = U256::ZERO;

        let calldata = build_execute_arb_calldata(&steps, flashloan_token, flashloan_amount);

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

        let calldata1 =
            build_execute_arb_calldata(&steps, Address::ZERO, U256::from(1000));
        let calldata2 =
            build_execute_arb_calldata(&steps, Address::ZERO, U256::from(1000));

        // Same inputs must produce identical calldata (deterministic)
        assert_eq!(calldata1, calldata2);
    }
}
