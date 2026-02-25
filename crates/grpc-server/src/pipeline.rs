//! Detection → Simulation pipeline conversion layer.
//!
//! Converts between native Rust types (`aether_common::types::*`) and
//! proto-generated types (`crate::service::aether_proto::*`) for the gRPC
//! boundary. Also provides `build_validated_arb` to assemble a proto
//! `ValidatedArb` from an `ArbOpportunity` and `SimulationResult`.

use alloy::primitives::{Address, U256};

use aether_common::types::{
    ArbHop, ArbOpportunity, ProtocolType, SimulationResult, SwapStep, ValidatedArb,
};
use crate::service::aether_proto;

// ---------------------------------------------------------------------------
// Address / U256 serialization helpers
// ---------------------------------------------------------------------------

/// Serialize an `Address` (20 bytes) to proto `bytes`.
pub fn address_to_bytes(addr: &Address) -> Vec<u8> {
    addr.as_slice().to_vec()
}

/// Serialize a `U256` to big-endian proto `bytes` (32 bytes).
pub fn u256_to_bytes(val: &U256) -> Vec<u8> {
    val.to_be_bytes::<32>().to_vec()
}

// ---------------------------------------------------------------------------
// ProtocolType conversion
// ---------------------------------------------------------------------------

/// Map a native `ProtocolType` to the proto enum `i32` value.
pub fn protocol_to_proto(p: ProtocolType) -> i32 {
    match p {
        ProtocolType::UniswapV2 => aether_proto::ProtocolType::UniswapV2 as i32,
        ProtocolType::UniswapV3 => aether_proto::ProtocolType::UniswapV3 as i32,
        ProtocolType::SushiSwap => aether_proto::ProtocolType::Sushiswap as i32,
        ProtocolType::Curve => aether_proto::ProtocolType::Curve as i32,
        ProtocolType::BalancerV2 => aether_proto::ProtocolType::BalancerV2 as i32,
        ProtocolType::BancorV3 => aether_proto::ProtocolType::BancorV3 as i32,
    }
}

// ---------------------------------------------------------------------------
// Struct conversions: native → proto
// ---------------------------------------------------------------------------

/// Convert a native `ArbHop` to a proto `ArbHop`.
pub fn arb_hop_to_proto(hop: &ArbHop) -> aether_proto::ArbHop {
    aether_proto::ArbHop {
        protocol: protocol_to_proto(hop.protocol),
        pool_address: address_to_bytes(&hop.pool_address),
        token_in: address_to_bytes(&hop.token_in),
        token_out: address_to_bytes(&hop.token_out),
        amount_in: u256_to_bytes(&hop.amount_in),
        expected_out: u256_to_bytes(&hop.expected_out),
        estimated_gas: hop.estimated_gas,
    }
}

/// Convert a native `SwapStep` to a proto `SwapStep`.
pub fn swap_step_to_proto(step: &SwapStep) -> aether_proto::SwapStep {
    aether_proto::SwapStep {
        protocol: protocol_to_proto(step.protocol),
        pool_address: address_to_bytes(&step.pool_address),
        token_in: address_to_bytes(&step.token_in),
        token_out: address_to_bytes(&step.token_out),
        amount_in: u256_to_bytes(&step.amount_in),
        min_amount_out: u256_to_bytes(&step.min_amount_out),
        calldata: step.calldata.clone(),
    }
}

/// Convert a native `ValidatedArb` to a proto `ValidatedArb`.
#[allow(dead_code)]
pub fn validated_arb_to_proto(arb: &ValidatedArb) -> aether_proto::ValidatedArb {
    aether_proto::ValidatedArb {
        id: arb.id.clone(),
        hops: arb.hops.iter().map(arb_hop_to_proto).collect(),
        total_profit_wei: u256_to_bytes(&arb.total_profit_wei),
        total_gas: arb.total_gas,
        gas_cost_wei: u256_to_bytes(&arb.gas_cost_wei),
        net_profit_wei: u256_to_bytes(&arb.net_profit_wei),
        block_number: arb.block_number,
        timestamp_ns: arb.timestamp_ns,
        flashloan_token: address_to_bytes(&arb.flashloan_token),
        flashloan_amount: u256_to_bytes(&arb.flashloan_amount),
        steps: arb.steps.iter().map(swap_step_to_proto).collect(),
        calldata: arb.calldata.clone(),
    }
}

// ---------------------------------------------------------------------------
// High-level pipeline function
// ---------------------------------------------------------------------------

/// Build a proto `ValidatedArb` from an `ArbOpportunity` and a successful
/// `SimulationResult`.
///
/// This is the key function called after detection + simulation to produce the
/// message that gets published to the gRPC stream for the Go executor.
///
/// `flashloan_token` and `flashloan_amount` come from the optimizer / calldata
/// builder.  `calldata` is the ABI-encoded `executeArb()` payload.
pub fn build_validated_arb(
    opportunity: &ArbOpportunity,
    sim_result: &SimulationResult,
    flashloan_token: Address,
    flashloan_amount: U256,
    steps: &[SwapStep],
    calldata: Vec<u8>,
) -> aether_proto::ValidatedArb {
    // Use the simulation gas_used for total_gas when available.
    let total_gas = if sim_result.gas_used > 0 {
        sim_result.gas_used
    } else {
        opportunity.total_gas
    };

    // Recalculate net profit from simulation if available.
    let net_profit_wei = if sim_result.profit_wei > U256::ZERO {
        sim_result.profit_wei
    } else {
        opportunity.net_profit_wei
    };

    aether_proto::ValidatedArb {
        id: opportunity.id.clone(),
        hops: opportunity.hops.iter().map(arb_hop_to_proto).collect(),
        total_profit_wei: u256_to_bytes(&opportunity.total_profit_wei),
        total_gas,
        gas_cost_wei: u256_to_bytes(&opportunity.gas_cost_wei),
        net_profit_wei: u256_to_bytes(&net_profit_wei),
        block_number: opportunity.block_number,
        timestamp_ns: opportunity.timestamp_ns,
        flashloan_token: address_to_bytes(&flashloan_token),
        flashloan_amount: u256_to_bytes(&flashloan_amount),
        steps: steps.iter().map(swap_step_to_proto).collect(),
        calldata,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aether_common::types::addresses;

    // ---- Serialization helpers ----

    #[test]
    fn test_address_to_bytes() {
        let addr = Address::repeat_byte(0xAB);
        let bytes = address_to_bytes(&addr);
        assert_eq!(bytes.len(), 20);
        assert!(bytes.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn test_address_to_bytes_zero() {
        let bytes = address_to_bytes(&Address::ZERO);
        assert_eq!(bytes.len(), 20);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_u256_to_bytes() {
        let val = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
        let bytes = u256_to_bytes(&val);
        assert_eq!(bytes.len(), 32);
        // Verify big-endian round-trip.
        let reconstructed = U256::from_be_slice(&bytes);
        assert_eq!(reconstructed, val);
    }

    #[test]
    fn test_u256_to_bytes_zero() {
        let bytes = u256_to_bytes(&U256::ZERO);
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_u256_to_bytes_max() {
        let val = U256::MAX;
        let bytes = u256_to_bytes(&val);
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|&b| b == 0xFF));
    }

    // ---- Protocol conversion ----

    #[test]
    fn test_protocol_to_proto_all_variants() {
        assert_eq!(
            protocol_to_proto(ProtocolType::UniswapV2),
            aether_proto::ProtocolType::UniswapV2 as i32
        );
        assert_eq!(
            protocol_to_proto(ProtocolType::UniswapV3),
            aether_proto::ProtocolType::UniswapV3 as i32
        );
        assert_eq!(
            protocol_to_proto(ProtocolType::SushiSwap),
            aether_proto::ProtocolType::Sushiswap as i32
        );
        assert_eq!(
            protocol_to_proto(ProtocolType::Curve),
            aether_proto::ProtocolType::Curve as i32
        );
        assert_eq!(
            protocol_to_proto(ProtocolType::BalancerV2),
            aether_proto::ProtocolType::BalancerV2 as i32
        );
        assert_eq!(
            protocol_to_proto(ProtocolType::BancorV3),
            aether_proto::ProtocolType::BancorV3 as i32
        );
    }

    // ---- ArbHop conversion ----

    #[test]
    fn test_arb_hop_to_proto() {
        let hop = ArbHop {
            protocol: ProtocolType::UniswapV2,
            pool_address: Address::repeat_byte(0x11),
            token_in: addresses::WETH,
            token_out: addresses::USDC,
            amount_in: U256::from(1_000_000_000_000_000_000u128),
            expected_out: U256::from(2_000_000_000u64),
            estimated_gas: 60_000,
        };

        let proto = arb_hop_to_proto(&hop);

        assert_eq!(proto.protocol, aether_proto::ProtocolType::UniswapV2 as i32);
        assert_eq!(proto.pool_address.len(), 20);
        assert_eq!(proto.token_in.len(), 20);
        assert_eq!(proto.token_out.len(), 20);
        assert_eq!(proto.amount_in.len(), 32);
        assert_eq!(proto.expected_out.len(), 32);
        assert_eq!(proto.estimated_gas, 60_000);

        // Verify address bytes match.
        assert_eq!(
            Address::from_slice(&proto.pool_address),
            Address::repeat_byte(0x11)
        );
    }

    // ---- SwapStep conversion ----

    #[test]
    fn test_swap_step_to_proto() {
        let step = SwapStep {
            protocol: ProtocolType::UniswapV3,
            pool_address: Address::repeat_byte(0x22),
            token_in: addresses::WETH,
            token_out: addresses::USDT,
            amount_in: U256::from(5_000_000_000_000_000_000u128),
            min_amount_out: U256::from(9_900_000_000u64),
            calldata: vec![0xAA, 0xBB, 0xCC],
        };

        let proto = swap_step_to_proto(&step);

        assert_eq!(proto.protocol, aether_proto::ProtocolType::UniswapV3 as i32);
        assert_eq!(proto.pool_address.len(), 20);
        assert_eq!(proto.calldata, vec![0xAA, 0xBB, 0xCC]);

        // Verify amount round-trip.
        let reconstructed = U256::from_be_slice(&proto.amount_in);
        assert_eq!(reconstructed, U256::from(5_000_000_000_000_000_000u128));
    }

    // ---- ValidatedArb conversion ----

    #[test]
    fn test_validated_arb_to_proto() {
        let arb = ValidatedArb {
            id: "test-arb-001".to_string(),
            hops: vec![ArbHop {
                protocol: ProtocolType::UniswapV2,
                pool_address: Address::repeat_byte(0x33),
                token_in: addresses::WETH,
                token_out: addresses::USDC,
                amount_in: U256::from(1_000_000_000_000_000_000u128),
                expected_out: U256::from(2_000_000_000u64),
                estimated_gas: 60_000,
            }],
            steps: vec![SwapStep {
                protocol: ProtocolType::UniswapV2,
                pool_address: Address::repeat_byte(0x33),
                token_in: addresses::WETH,
                token_out: addresses::USDC,
                amount_in: U256::from(1_000_000_000_000_000_000u128),
                min_amount_out: U256::from(1_980_000_000u64),
                calldata: vec![0x01],
            }],
            total_profit_wei: U256::from(2_000_000_000_000_000u128),
            total_gas: 200_000,
            gas_cost_wei: U256::from(600_000_000_000_000u128),
            net_profit_wei: U256::from(1_400_000_000_000_000u128),
            block_number: 18_500_000,
            timestamp_ns: 1_700_000_000_000_000_000,
            flashloan_token: addresses::WETH,
            flashloan_amount: U256::from(10_000_000_000_000_000_000u128),
            calldata: vec![0xDE, 0xAD],
        };

        let proto = validated_arb_to_proto(&arb);

        assert_eq!(proto.id, "test-arb-001");
        assert_eq!(proto.hops.len(), 1);
        assert_eq!(proto.steps.len(), 1);
        assert_eq!(proto.total_gas, 200_000);
        assert_eq!(proto.block_number, 18_500_000);
        assert_eq!(proto.timestamp_ns, 1_700_000_000_000_000_000);
        assert_eq!(proto.calldata, vec![0xDE, 0xAD]);

        // Verify flashloan token address.
        assert_eq!(
            Address::from_slice(&proto.flashloan_token),
            addresses::WETH
        );
    }

    // ---- build_validated_arb ----

    #[test]
    fn test_build_validated_arb_basic() {
        let opportunity = ArbOpportunity {
            id: "opp-001".to_string(),
            hops: vec![
                ArbHop {
                    protocol: ProtocolType::UniswapV2,
                    pool_address: Address::repeat_byte(0x44),
                    token_in: addresses::WETH,
                    token_out: addresses::USDC,
                    amount_in: U256::from(1_000_000_000_000_000_000u128),
                    expected_out: U256::from(2_000_000_000u64),
                    estimated_gas: 60_000,
                },
                ArbHop {
                    protocol: ProtocolType::SushiSwap,
                    pool_address: Address::repeat_byte(0x55),
                    token_in: addresses::USDC,
                    token_out: addresses::WETH,
                    amount_in: U256::from(2_000_000_000u64),
                    expected_out: U256::from(1_010_000_000_000_000_000u128),
                    estimated_gas: 60_000,
                },
            ],
            total_profit_wei: U256::from(10_000_000_000_000_000u128),
            total_gas: 250_000,
            gas_cost_wei: U256::from(5_000_000_000_000_000u128),
            net_profit_wei: U256::from(5_000_000_000_000_000u128),
            block_number: 19_000_000,
            timestamp_ns: 1_710_000_000_000_000_000,
        };

        let sim_result = SimulationResult {
            success: true,
            profit_wei: U256::from(4_800_000_000_000_000u128),
            gas_used: 230_000,
            revert_reason: None,
        };

        let steps = vec![
            SwapStep {
                protocol: ProtocolType::UniswapV2,
                pool_address: Address::repeat_byte(0x44),
                token_in: addresses::WETH,
                token_out: addresses::USDC,
                amount_in: U256::from(1_000_000_000_000_000_000u128),
                min_amount_out: U256::from(1_980_000_000u64),
                calldata: vec![],
            },
            SwapStep {
                protocol: ProtocolType::SushiSwap,
                pool_address: Address::repeat_byte(0x55),
                token_in: addresses::USDC,
                token_out: addresses::WETH,
                amount_in: U256::from(2_000_000_000u64),
                min_amount_out: U256::from(1_000_000_000_000_000_000u128),
                calldata: vec![],
            },
        ];

        let calldata = vec![0xFF; 128];

        let proto = build_validated_arb(
            &opportunity,
            &sim_result,
            addresses::WETH,
            U256::from(1_000_000_000_000_000_000u128),
            &steps,
            calldata.clone(),
        );

        assert_eq!(proto.id, "opp-001");
        assert_eq!(proto.hops.len(), 2);
        assert_eq!(proto.steps.len(), 2);
        // Should use sim gas_used (230_000) instead of opportunity total_gas (250_000).
        assert_eq!(proto.total_gas, 230_000);
        assert_eq!(proto.block_number, 19_000_000);
        assert_eq!(proto.calldata.len(), 128);

        // Verify sim profit was used for net_profit_wei.
        let net_profit = U256::from_be_slice(&proto.net_profit_wei);
        assert_eq!(net_profit, U256::from(4_800_000_000_000_000u128));
    }

    #[test]
    fn test_build_validated_arb_fallback_when_sim_zero() {
        let opportunity = ArbOpportunity {
            id: "opp-002".to_string(),
            hops: vec![],
            total_profit_wei: U256::from(1_000_000_000_000_000u128),
            total_gas: 180_000,
            gas_cost_wei: U256::from(500_000_000_000_000u128),
            net_profit_wei: U256::from(500_000_000_000_000u128),
            block_number: 19_000_001,
            timestamp_ns: 1_710_000_000_000_000_001,
        };

        // Simulation returned zero gas and zero profit (e.g., basic simulate()).
        let sim_result = SimulationResult {
            success: true,
            profit_wei: U256::ZERO,
            gas_used: 0,
            revert_reason: None,
        };

        let proto = build_validated_arb(
            &opportunity,
            &sim_result,
            addresses::WETH,
            U256::from(1_000_000_000_000_000_000u128),
            &[],
            vec![],
        );

        // Should fall back to opportunity values.
        assert_eq!(proto.total_gas, 180_000);
        let net_profit = U256::from_be_slice(&proto.net_profit_wei);
        assert_eq!(net_profit, U256::from(500_000_000_000_000u128));
    }

    #[test]
    fn test_build_validated_arb_empty_opportunity() {
        let opportunity = ArbOpportunity {
            id: "opp-empty".to_string(),
            hops: vec![],
            total_profit_wei: U256::ZERO,
            total_gas: 0,
            gas_cost_wei: U256::ZERO,
            net_profit_wei: U256::ZERO,
            block_number: 0,
            timestamp_ns: 0,
        };

        let sim_result = SimulationResult {
            success: true,
            profit_wei: U256::ZERO,
            gas_used: 0,
            revert_reason: None,
        };

        let proto = build_validated_arb(
            &opportunity,
            &sim_result,
            Address::ZERO,
            U256::ZERO,
            &[],
            vec![],
        );

        assert_eq!(proto.id, "opp-empty");
        assert!(proto.hops.is_empty());
        assert!(proto.steps.is_empty());
        assert_eq!(proto.total_gas, 0);
    }
}
