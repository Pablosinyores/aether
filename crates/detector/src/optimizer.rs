use alloy::primitives::U256;

/// Ternary search to find the optimal input amount that maximizes profit.
///
/// The profit function for an arbitrage path through constant-product AMMs is
/// typically **unimodal** (rises to a single peak then falls), making ternary
/// search an efficient O(log n) approach.
///
/// # Arguments
///
/// * `min_input` - Lower bound of the search range.
/// * `max_input` - Upper bound of the search range.
/// * `iterations` - Maximum number of ternary search iterations (typically 60-100).
/// * `profit_fn` - Closure that returns net profit (signed) for a given input amount.
///
/// # Returns
///
/// `(optimal_amount, max_profit)` — the input amount and its corresponding profit.
pub fn ternary_search_optimal_input<F>(
    min_input: U256,
    max_input: U256,
    iterations: u32,
    profit_fn: F,
) -> (U256, i128)
where
    F: Fn(U256) -> i128,
{
    let mut lo = min_input;
    let mut hi = max_input;

    for _ in 0..iterations {
        if hi <= lo + U256::from(2) {
            break;
        }

        let range = hi - lo;
        let third = range / U256::from(3);

        let m1 = lo + third;
        let m2 = hi - third;

        let p1 = profit_fn(m1);
        let p2 = profit_fn(m2);

        if p1 < p2 {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    // Evaluate the midpoint as the final answer
    let optimal = lo + (hi - lo) / U256::from(2);
    let profit = profit_fn(optimal);

    (optimal, profit)
}

/// Simple grid search as fallback (less precise but more robust).
///
/// Evaluates `profit_fn` at `steps + 1` evenly spaced points in
/// `[min_input, max_input]` and returns the best. Useful when the profit
/// function may not be strictly unimodal (e.g., multi-pool paths with
/// discontinuities at tick boundaries).
///
/// # Returns
///
/// `(best_amount, best_profit)` — the input amount and its corresponding profit.
pub fn grid_search_optimal_input<F>(
    min_input: U256,
    max_input: U256,
    steps: u32,
    profit_fn: F,
) -> (U256, i128)
where
    F: Fn(U256) -> i128,
{
    if min_input >= max_input || steps == 0 {
        return (min_input, profit_fn(min_input));
    }

    let range = max_input - min_input;
    let step_size = range / U256::from(steps);
    if step_size.is_zero() {
        return (min_input, profit_fn(min_input));
    }

    let mut best_amount = min_input;
    let mut best_profit = i128::MIN;

    let mut current = min_input;
    for _ in 0..=steps {
        let profit = profit_fn(current);
        if profit > best_profit {
            best_profit = profit;
            best_amount = current;
        }
        current = current.saturating_add(step_size);
        if current > max_input {
            break;
        }
    }

    (best_amount, best_profit)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --------------- Ternary search ---------------

    #[test]
    fn test_ternary_search_unimodal_peak() {
        // Simulate a unimodal profit function: profit = -(x - 500)^2 + 10000
        // Peak at x = 500 with profit = 10000
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val - 500) * (x_val - 500) + 10_000
        };

        let (optimal, profit) = ternary_search_optimal_input(
            U256::from(0u64),
            U256::from(1000u64),
            100,
            profit_fn,
        );

        let opt_val = optimal.to::<u128>();
        // Should be very close to 500
        assert!(
            (opt_val as i128 - 500).unsigned_abs() <= 2,
            "optimal {} should be close to 500",
            opt_val
        );
        // Profit should be close to 10000
        assert!(
            profit >= 9_990,
            "profit {} should be close to 10000",
            profit
        );
    }

    #[test]
    fn test_ternary_search_peak_at_boundary_low() {
        // Monotonically decreasing: profit = 1000 - x
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            1000 - x_val
        };

        let (optimal, _profit) = ternary_search_optimal_input(
            U256::from(0u64),
            U256::from(1000u64),
            100,
            profit_fn,
        );

        let opt_val = optimal.to::<u128>();
        // Optimal should be near the lower bound
        assert!(
            opt_val <= 5,
            "optimal {} should be near lower bound for decreasing function",
            opt_val
        );
    }

    #[test]
    fn test_ternary_search_peak_at_boundary_high() {
        // Monotonically increasing: profit = x
        let profit_fn = |x: U256| -> i128 {
            x.to::<u128>() as i128
        };

        let (optimal, _profit) = ternary_search_optimal_input(
            U256::from(0u64),
            U256::from(1000u64),
            100,
            profit_fn,
        );

        let opt_val = optimal.to::<u128>();
        // Optimal should be near the upper bound
        assert!(
            opt_val >= 995,
            "optimal {} should be near upper bound for increasing function",
            opt_val
        );
    }

    #[test]
    fn test_ternary_search_narrow_range() {
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val - 5) * (x_val - 5) + 100
        };

        let (optimal, profit) = ternary_search_optimal_input(
            U256::from(3u64),
            U256::from(7u64),
            100,
            profit_fn,
        );

        let opt_val = optimal.to::<u128>();
        assert!(
            (opt_val as i128 - 5).unsigned_abs() <= 1,
            "optimal {} should be close to 5",
            opt_val
        );
        assert!(profit >= 96, "profit {} should be close to 100", profit);
    }

    #[test]
    fn test_ternary_search_same_min_max() {
        let profit_fn = |x: U256| -> i128 {
            x.to::<u128>() as i128
        };

        let (optimal, profit) = ternary_search_optimal_input(
            U256::from(42u64),
            U256::from(42u64),
            100,
            profit_fn,
        );

        assert_eq!(optimal, U256::from(42u64));
        assert_eq!(profit, 42);
    }

    #[test]
    fn test_ternary_search_zero_iterations() {
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val - 500) * (x_val - 500) + 10000
        };

        // With 0 iterations, should return midpoint
        let (optimal, _profit) = ternary_search_optimal_input(
            U256::from(0u64),
            U256::from(1000u64),
            0,
            profit_fn,
        );

        assert_eq!(optimal, U256::from(500u64));
    }

    #[test]
    fn test_ternary_search_large_values() {
        // Use ETH-scale values: search in [0.01 ETH, 50 ETH]
        let min = U256::from(10_000_000_000_000_000u128); // 0.01 ETH
        let max = U256::from(50_000_000_000_000_000_000u128); // 50 ETH

        // Peak at ~5 ETH
        let peak = 5_000_000_000_000_000_000i128;
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            let diff = (x_val - peak) / 1_000_000_000; // Scale down to avoid overflow
            -(diff * diff) + 1_000_000_000
        };

        let (optimal, _profit) = ternary_search_optimal_input(min, max, 100, profit_fn);

        let opt_val = optimal.to::<u128>() as i128;
        let error_pct = ((opt_val - peak).unsigned_abs() as f64 / peak as f64) * 100.0;
        assert!(
            error_pct < 1.0,
            "optimal should be within 1% of peak, error = {:.4}%",
            error_pct
        );
    }

    // --------------- Grid search ---------------

    #[test]
    fn test_grid_search_unimodal() {
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val - 500) * (x_val - 500) + 10_000
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(0u64),
            U256::from(1000u64),
            100,
            profit_fn,
        );

        let opt_val = optimal.to::<u128>();
        assert!(
            (opt_val as i128 - 500).unsigned_abs() <= 10,
            "grid search optimal {} should be close to 500",
            opt_val
        );
        assert!(profit >= 9_900, "profit {} should be close to 10000", profit);
    }

    #[test]
    fn test_grid_search_same_min_max() {
        let profit_fn = |x: U256| -> i128 {
            x.to::<u128>() as i128
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(42u64),
            U256::from(42u64),
            10,
            profit_fn,
        );

        assert_eq!(optimal, U256::from(42u64));
        assert_eq!(profit, 42);
    }

    #[test]
    fn test_grid_search_zero_steps() {
        let profit_fn = |x: U256| -> i128 {
            x.to::<u128>() as i128
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(10u64),
            U256::from(100u64),
            0,
            profit_fn,
        );

        assert_eq!(optimal, U256::from(10u64));
        assert_eq!(profit, 10);
    }

    #[test]
    fn test_grid_search_min_greater_than_max() {
        let profit_fn = |x: U256| -> i128 {
            x.to::<u128>() as i128
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(100u64),
            U256::from(10u64),
            10,
            profit_fn,
        );

        // Should return min_input when min > max
        assert_eq!(optimal, U256::from(100u64));
        assert_eq!(profit, 100);
    }

    #[test]
    fn test_grid_search_all_negative_profits() {
        // All profits are negative — should still find the least-bad option
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val * x_val) - 100
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(1u64),
            U256::from(100u64),
            50,
            profit_fn,
        );

        // Least-bad is at x=1: -(1) - 100 = -101
        let opt_val = optimal.to::<u128>();
        assert!(opt_val <= 3, "optimal {} should be near lower bound", opt_val);
        assert!(profit < 0, "all profits should be negative");
    }

    #[test]
    fn test_grid_search_step_size_larger_than_range() {
        // range = 5, steps = 1 => step_size = 5
        let profit_fn = |x: U256| -> i128 {
            let x_val = x.to::<u128>() as i128;
            -(x_val - 3) * (x_val - 3) + 9
        };

        let (optimal, profit) = grid_search_optimal_input(
            U256::from(0u64),
            U256::from(5u64),
            1,
            profit_fn,
        );

        // Only evaluates at 0 and 5
        let opt_val = optimal.to::<u128>();
        assert!(
            opt_val == 0 || opt_val == 5,
            "should only evaluate at boundaries, got {}",
            opt_val
        );
        // f(0) = -9 + 9 = 0, f(5) = -4 + 9 = 5
        assert_eq!(profit, 5);
    }
}
