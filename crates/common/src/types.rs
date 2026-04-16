use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Protocol type enum matching on-chain constants in AetherExecutor.sol.
///
/// Discriminants are load-bearing: they are cast to `uint8` and encoded into
/// `executeArb` calldata, where the on-chain contract branches on them. The
/// const assertions below fail compilation if any discriminant drifts away
/// from the Solidity constant it mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProtocolType {
    UniswapV2 = 1,
    UniswapV3 = 2,
    SushiSwap = 3,
    Curve = 4,
    BalancerV2 = 5,
    BancorV3 = 6,
}

// Compile-time guard: if someone reorders the enum or changes a discriminant, the
// build fails before a single test runs. Mirrors the Solidity constants in
// `contracts/src/AetherExecutor.sol` (UNISWAP_V2..BANCOR_V3).
const _: () = {
    assert!(ProtocolType::UniswapV2 as u8 == 1);
    assert!(ProtocolType::UniswapV3 as u8 == 2);
    assert!(ProtocolType::SushiSwap as u8 == 3);
    assert!(ProtocolType::Curve as u8 == 4);
    assert!(ProtocolType::BalancerV2 as u8 == 5);
    assert!(ProtocolType::BancorV3 as u8 == 6);
};

impl ProtocolType {
    /// Base gas cost for each protocol's swap
    pub fn base_gas(&self) -> u64 {
        match self {
            ProtocolType::UniswapV2 => 60_000,
            ProtocolType::UniswapV3 => 180_000,
            ProtocolType::SushiSwap => 60_000,
            ProtocolType::Curve => 130_000,
            ProtocolType::BalancerV2 => 120_000,
            ProtocolType::BancorV3 => 150_000,
        }
    }
}

/// Unique pool identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId {
    pub address: Address,
    pub protocol: ProtocolType,
}

/// Token with metadata
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
}

/// Pool tier for monitoring frequency
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolTier {
    Hot,  // every block
    Warm, // every 5 blocks
    Cold, // every 20 blocks
}

/// A single hop in an arbitrage path
#[derive(Debug, Clone)]
pub struct ArbHop {
    pub protocol: ProtocolType,
    pub pool_address: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub expected_out: U256,
    pub estimated_gas: u64,
}

/// Swap step for on-chain execution (passed to AetherExecutor.sol)
#[derive(Debug, Clone)]
pub struct SwapStep {
    pub protocol: ProtocolType,
    pub pool_address: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub min_amount_out: U256,
    pub calldata: Vec<u8>,
}

/// Complete arbitrage opportunity detected by the engine
#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub id: String,
    pub hops: Vec<ArbHop>,
    pub total_profit_wei: U256,
    pub total_gas: u64,
    pub gas_cost_wei: U256,
    pub net_profit_wei: U256,
    pub block_number: u64,
    pub timestamp_ns: i64,
}

/// Validated arb ready for execution (after EVM simulation)
#[derive(Debug, Clone)]
pub struct ValidatedArb {
    pub id: String,
    pub hops: Vec<ArbHop>,
    pub steps: Vec<SwapStep>,
    pub total_profit_wei: U256,
    pub total_gas: u64,
    pub gas_cost_wei: U256,
    pub net_profit_wei: U256,
    pub block_number: u64,
    pub timestamp_ns: i64,
    pub flashloan_token: Address,
    pub flashloan_amount: U256,
    pub calldata: Vec<u8>,
}

/// Result from EVM simulation
#[derive(Debug, Clone)]
pub struct SimulationResult {
    pub success: bool,
    pub profit_wei: U256,
    pub gas_used: u64,
    pub revert_reason: Option<String>,
}

/// System state for risk management
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemState {
    Running,
    Degraded,
    Paused,
    Halted,
}

/// Node connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Connected,
    Healthy,
    Degraded,
    Reconnecting,
    Failed,
}

/// Well-known Ethereum mainnet addresses
pub mod addresses {
    use alloy::primitives::{address, Address};

    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    pub const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    pub const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    pub const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
    pub const WBTC: Address = address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

    /// Aave V3 Pool on Ethereum Mainnet
    pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");

    /// Uniswap V2 Factory
    pub const UNISWAP_V2_FACTORY: Address =
        address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");

    /// Uniswap V3 Factory
    pub const UNISWAP_V3_FACTORY: Address =
        address!("1F98431c8aD98523631AE4a59f267346ea31F984");

    /// SushiSwap Factory
    pub const SUSHISWAP_FACTORY: Address =
        address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");
}

/// Gas constants
pub mod gas {
    pub const FLASHLOAN_BASE_GAS: u64 = 80_000;
    pub const TX_BASE_GAS: u64 = 21_000;
    /// Catch-all overhead for `executeArb` framing: reentrancy guard, nonReentrant
    /// modifier, calldata decoding, event emission, profit-split math, and per-step
    /// dispatch. The 30k budget also absorbs the registry `SLOAD`s added in E4/WS-3
    /// (2,100 gas cold on the first Balancer/Bancor hop of a block, ~100 gas warm on
    /// subsequent reads) — worst case ~4,200 gas leaving >85% of the buffer unused.
    pub const EXECUTOR_OVERHEAD_GAS: u64 = 30_000;
    pub const UNIV3_PER_TICK_GAS: u64 = 5_000;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn test_protocol_type_base_gas() {
        assert_eq!(ProtocolType::UniswapV2.base_gas(), 60_000);
        assert_eq!(ProtocolType::UniswapV3.base_gas(), 180_000);
        assert_eq!(ProtocolType::SushiSwap.base_gas(), 60_000);
        assert_eq!(ProtocolType::Curve.base_gas(), 130_000);
        assert_eq!(ProtocolType::BalancerV2.base_gas(), 120_000);
        assert_eq!(ProtocolType::BancorV3.base_gas(), 150_000);
    }

    #[test]
    fn test_protocol_type_repr() {
        assert_eq!(ProtocolType::UniswapV2 as u8, 1);
        assert_eq!(ProtocolType::UniswapV3 as u8, 2);
        assert_eq!(ProtocolType::SushiSwap as u8, 3);
        assert_eq!(ProtocolType::Curve as u8, 4);
        assert_eq!(ProtocolType::BalancerV2 as u8, 5);
        assert_eq!(ProtocolType::BancorV3 as u8, 6);
    }

    #[test]
    fn test_pool_id_equality() {
        let id1 = PoolId {
            address: Address::ZERO,
            protocol: ProtocolType::UniswapV2,
        };
        let id2 = PoolId {
            address: Address::ZERO,
            protocol: ProtocolType::UniswapV2,
        };
        let id3 = PoolId {
            address: Address::ZERO,
            protocol: ProtocolType::UniswapV3,
        };
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_pool_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let id = PoolId {
            address: Address::ZERO,
            protocol: ProtocolType::Curve,
        };
        set.insert(id);
        assert!(set.contains(&id));
    }

    #[test]
    fn test_token_creation() {
        let token = Token {
            address: addresses::WETH,
            decimals: 18,
            symbol: "WETH".to_string(),
        };
        assert_eq!(token.decimals, 18);
        assert_eq!(token.symbol, "WETH");
        assert_eq!(token.address, addresses::WETH);
    }

    #[test]
    fn test_pool_tier_variants() {
        let hot = PoolTier::Hot;
        let warm = PoolTier::Warm;
        let cold = PoolTier::Cold;
        assert_ne!(hot, warm);
        assert_ne!(warm, cold);
        assert_eq!(hot, PoolTier::Hot);
    }

    #[test]
    fn test_system_state_variants() {
        assert_ne!(SystemState::Running, SystemState::Degraded);
        assert_ne!(SystemState::Paused, SystemState::Halted);
        assert_eq!(SystemState::Running, SystemState::Running);
    }

    #[test]
    fn test_node_state_variants() {
        assert_ne!(NodeState::Connected, NodeState::Healthy);
        assert_ne!(NodeState::Degraded, NodeState::Reconnecting);
        assert_ne!(NodeState::Reconnecting, NodeState::Failed);
        assert_eq!(NodeState::Healthy, NodeState::Healthy);
    }

    #[test]
    fn test_arb_hop_construction() {
        let hop = ArbHop {
            protocol: ProtocolType::UniswapV2,
            pool_address: Address::ZERO,
            token_in: addresses::WETH,
            token_out: addresses::USDC,
            amount_in: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
            expected_out: U256::from(2000_000_000u64),            // 2000 USDC
            estimated_gas: 60_000,
        };
        assert_eq!(hop.protocol, ProtocolType::UniswapV2);
        assert_eq!(hop.estimated_gas, 60_000);
    }

    #[test]
    fn test_swap_step_construction() {
        let step = SwapStep {
            protocol: ProtocolType::UniswapV3,
            pool_address: Address::ZERO,
            token_in: addresses::WETH,
            token_out: addresses::USDC,
            amount_in: U256::from(1_000_000_000_000_000_000u64),
            min_amount_out: U256::from(1980_000_000u64), // 1% slippage
            calldata: vec![0x01, 0x02, 0x03],
        };
        assert_eq!(step.protocol, ProtocolType::UniswapV3);
        assert_eq!(step.calldata.len(), 3);
    }

    #[test]
    fn test_arb_opportunity_construction() {
        let opp = ArbOpportunity {
            id: "arb-001".to_string(),
            hops: vec![],
            total_profit_wei: U256::from(1_000_000_000_000_000u64), // 0.001 ETH
            total_gas: 200_000,
            gas_cost_wei: U256::from(500_000_000_000_000u64),
            net_profit_wei: U256::from(500_000_000_000_000u64),
            block_number: 18_000_000,
            timestamp_ns: 1_700_000_000_000_000_000,
        };
        assert_eq!(opp.id, "arb-001");
        assert_eq!(opp.block_number, 18_000_000);
        assert!(opp.hops.is_empty());
    }

    #[test]
    fn test_validated_arb_construction() {
        let varb = ValidatedArb {
            id: "varb-001".to_string(),
            hops: vec![],
            steps: vec![],
            total_profit_wei: U256::from(2_000_000_000_000_000u64),
            total_gas: 250_000,
            gas_cost_wei: U256::from(600_000_000_000_000u64),
            net_profit_wei: U256::from(1_400_000_000_000_000u64),
            block_number: 18_000_001,
            timestamp_ns: 1_700_000_000_000_000_000,
            flashloan_token: addresses::WETH,
            flashloan_amount: U256::from(10_000_000_000_000_000_000u128), // 10 ETH
            calldata: vec![0xaa, 0xbb],
        };
        assert_eq!(varb.flashloan_token, addresses::WETH);
        assert_eq!(varb.calldata.len(), 2);
    }

    #[test]
    fn test_simulation_result_success() {
        let result = SimulationResult {
            success: true,
            profit_wei: U256::from(1_000_000_000_000_000u64),
            gas_used: 180_000,
            revert_reason: None,
        };
        assert!(result.success);
        assert!(result.revert_reason.is_none());
    }

    #[test]
    fn test_simulation_result_failure() {
        let result = SimulationResult {
            success: false,
            profit_wei: U256::ZERO,
            gas_used: 50_000,
            revert_reason: Some("Insufficient output amount".to_string()),
        };
        assert!(!result.success);
        assert_eq!(
            result.revert_reason.unwrap(),
            "Insufficient output amount"
        );
    }

    #[test]
    fn test_well_known_addresses() {
        // Verify addresses are not zero
        assert_ne!(addresses::WETH, Address::ZERO);
        assert_ne!(addresses::USDC, Address::ZERO);
        assert_ne!(addresses::USDT, Address::ZERO);
        assert_ne!(addresses::DAI, Address::ZERO);
        assert_ne!(addresses::WBTC, Address::ZERO);
        assert_ne!(addresses::AAVE_V3_POOL, Address::ZERO);
        assert_ne!(addresses::UNISWAP_V2_FACTORY, Address::ZERO);
        assert_ne!(addresses::UNISWAP_V3_FACTORY, Address::ZERO);
        assert_ne!(addresses::SUSHISWAP_FACTORY, Address::ZERO);

        // Verify all addresses are distinct
        let addrs = [
            addresses::WETH,
            addresses::USDC,
            addresses::USDT,
            addresses::DAI,
            addresses::WBTC,
        ];
        for i in 0..addrs.len() {
            for j in (i + 1)..addrs.len() {
                assert_ne!(addrs[i], addrs[j], "Address collision at index {i} and {j}");
            }
        }
    }

    #[test]
    fn test_gas_constants() {
        assert_eq!(gas::FLASHLOAN_BASE_GAS, 80_000);
        assert_eq!(gas::TX_BASE_GAS, 21_000);
        assert_eq!(gas::EXECUTOR_OVERHEAD_GAS, 30_000);
        assert_eq!(gas::UNIV3_PER_TICK_GAS, 5_000);
    }

    #[test]
    fn test_protocol_type_serde_roundtrip() {
        let original = ProtocolType::BalancerV2;
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: ProtocolType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_system_state_serde_roundtrip() {
        for state in [
            SystemState::Running,
            SystemState::Degraded,
            SystemState::Paused,
            SystemState::Halted,
        ] {
            let serialized = serde_json::to_string(&state).unwrap();
            let deserialized: SystemState = serde_json::from_str(&serialized).unwrap();
            assert_eq!(state, deserialized);
        }
    }

    #[test]
    fn test_pool_tier_serde_roundtrip() {
        for tier in [PoolTier::Hot, PoolTier::Warm, PoolTier::Cold] {
            let serialized = serde_json::to_string(&tier).unwrap();
            let deserialized: PoolTier = serde_json::from_str(&serialized).unwrap();
            assert_eq!(tier, deserialized);
        }
    }

    #[test]
    fn test_token_serde_roundtrip() {
        let token = Token {
            address: addresses::USDC,
            decimals: 6,
            symbol: "USDC".to_string(),
        };
        let serialized = serde_json::to_string(&token).unwrap();
        let deserialized: Token = serde_json::from_str(&serialized).unwrap();
        assert_eq!(token, deserialized);
    }
}
