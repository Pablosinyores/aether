use aether_common::types::{gas, ProtocolType};

/// Estimate gas cost for a single swap on the given protocol.
///
/// For Uniswap V3, each additional tick crossing adds `UNIV3_PER_TICK_GAS`
/// on top of the base cost. All other protocols use a flat base gas value.
pub fn estimate_swap_gas(protocol: ProtocolType, extra_ticks: u32) -> u64 {
    let base = protocol.base_gas();
    match protocol {
        ProtocolType::UniswapV3 => base + (extra_ticks as u64) * gas::UNIV3_PER_TICK_GAS,
        _ => base,
    }
}

/// Estimate total gas for a complete arbitrage route.
///
/// Includes:
/// - `TX_BASE_GAS` (21,000) for the Ethereum transaction itself
/// - `FLASHLOAN_BASE_GAS` (80,000) for the Aave V3 flash loan
/// - `EXECUTOR_OVERHEAD_GAS` (30,000) for the AetherExecutor contract logic
/// - Per-swap gas for each protocol in the path
pub fn estimate_total_gas(protocols: &[ProtocolType], tick_counts: &[u32]) -> u64 {
    let mut total = gas::TX_BASE_GAS + gas::FLASHLOAN_BASE_GAS + gas::EXECUTOR_OVERHEAD_GAS;
    for (i, protocol) in protocols.iter().enumerate() {
        let ticks = tick_counts.get(i).copied().unwrap_or(0);
        total += estimate_swap_gas(*protocol, ticks);
    }
    total
}

/// Calculate the gas cost in wei given gas units and gas price in gwei.
///
/// `gas_cost_wei = gas_units * gas_price_gwei * 1e9`
pub fn gas_cost_wei(gas_units: u64, gas_price_gwei: f64) -> u128 {
    (gas_units as f64 * gas_price_gwei * 1e9) as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    // --------------- estimate_swap_gas ---------------

    #[test]
    fn test_univ2_swap_gas() {
        assert_eq!(estimate_swap_gas(ProtocolType::UniswapV2, 0), 60_000);
        // Extra ticks should be ignored for non-V3 protocols
        assert_eq!(estimate_swap_gas(ProtocolType::UniswapV2, 10), 60_000);
    }

    #[test]
    fn test_univ3_swap_gas_no_ticks() {
        assert_eq!(estimate_swap_gas(ProtocolType::UniswapV3, 0), 200_000);
    }

    #[test]
    fn test_univ3_swap_gas_with_ticks() {
        // 200_000 + 5 * 5_000 = 225_000
        assert_eq!(estimate_swap_gas(ProtocolType::UniswapV3, 5), 225_000);
    }

    #[test]
    fn test_sushiswap_swap_gas() {
        assert_eq!(estimate_swap_gas(ProtocolType::SushiSwap, 0), 60_000);
    }

    #[test]
    fn test_curve_swap_gas() {
        assert_eq!(estimate_swap_gas(ProtocolType::Curve, 0), 130_000);
    }

    #[test]
    fn test_balancer_swap_gas() {
        assert_eq!(estimate_swap_gas(ProtocolType::BalancerV2, 0), 120_000);
    }

    #[test]
    fn test_bancor_swap_gas() {
        assert_eq!(estimate_swap_gas(ProtocolType::BancorV3, 0), 150_000);
    }

    // --------------- estimate_total_gas ---------------

    #[test]
    fn test_total_gas_empty_route() {
        // Just overhead: 21_000 + 80_000 + 30_000 = 131_000
        let total = estimate_total_gas(&[], &[]);
        assert_eq!(total, 131_000);
    }

    #[test]
    fn test_total_gas_single_swap() {
        // 131_000 + 60_000 = 191_000
        let total = estimate_total_gas(&[ProtocolType::UniswapV2], &[0]);
        assert_eq!(total, 191_000);
    }

    #[test]
    fn test_total_gas_triangle_arb() {
        // UniV2 -> UniV3 (3 ticks) -> SushiSwap
        // 131_000 + 60_000 + (200_000 + 3*5_000) + 60_000
        // = 131_000 + 60_000 + 215_000 + 60_000 = 466_000
        let protocols = [
            ProtocolType::UniswapV2,
            ProtocolType::UniswapV3,
            ProtocolType::SushiSwap,
        ];
        let ticks = [0, 3, 0];
        let total = estimate_total_gas(&protocols, &ticks);
        assert_eq!(total, 466_000);
    }

    #[test]
    fn test_total_gas_missing_tick_counts() {
        // If tick_counts is shorter than protocols, default to 0 ticks
        let protocols = [ProtocolType::UniswapV3, ProtocolType::UniswapV3];
        let ticks = [5]; // Only one entry for two protocols
        let total = estimate_total_gas(&protocols, &ticks);
        // 131_000 + (200_000 + 5*5_000) + (200_000 + 0*5_000)
        // = 131_000 + 225_000 + 200_000 = 556_000
        assert_eq!(total, 556_000);
    }

    #[test]
    fn test_total_gas_all_protocols() {
        let protocols = [
            ProtocolType::UniswapV2,
            ProtocolType::UniswapV3,
            ProtocolType::SushiSwap,
            ProtocolType::Curve,
            ProtocolType::BalancerV2,
            ProtocolType::BancorV3,
        ];
        let ticks = [0, 0, 0, 0, 0, 0];
        let total = estimate_total_gas(&protocols, &ticks);
        // 131_000 + 60_000 + 200_000 + 60_000 + 130_000 + 120_000 + 150_000
        // = 131_000 + 720_000 = 851_000
        assert_eq!(total, 851_000);
    }

    // --------------- gas_cost_wei ---------------

    #[test]
    fn test_gas_cost_wei_basic() {
        // 200_000 gas * 30 gwei = 200_000 * 30 * 1e9 = 6_000_000_000_000_000 wei
        let cost = gas_cost_wei(200_000, 30.0);
        assert_eq!(cost, 6_000_000_000_000_000);
    }

    #[test]
    fn test_gas_cost_wei_zero_gas() {
        assert_eq!(gas_cost_wei(0, 50.0), 0);
    }

    #[test]
    fn test_gas_cost_wei_zero_price() {
        assert_eq!(gas_cost_wei(200_000, 0.0), 0);
    }

    #[test]
    fn test_gas_cost_wei_fractional_gwei() {
        // 100_000 gas * 25.5 gwei = 100_000 * 25.5 * 1e9 = 2_550_000_000_000_000
        let cost = gas_cost_wei(100_000, 25.5);
        assert_eq!(cost, 2_550_000_000_000_000);
    }

    #[test]
    fn test_gas_cost_wei_high_gas_price() {
        // 300_000 gas * 300 gwei (circuit breaker threshold)
        // = 300_000 * 300 * 1e9 = 90_000_000_000_000_000 (0.09 ETH)
        let cost = gas_cost_wei(300_000, 300.0);
        assert_eq!(cost, 90_000_000_000_000_000);
    }
}
