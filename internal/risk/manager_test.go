package risk

import (
	"math/big"
	"testing"
)

// oneETH returns 1 ETH in wei (10^18).
func oneETH() *big.Int {
	return new(big.Int).Exp(big.NewInt(10), big.NewInt(18), nil)
}

// ethWei returns the given number of ETH as wei.
func ethWei(t *testing.T, eth int64) *big.Int {
	t.Helper()
	return new(big.Int).Mul(big.NewInt(eth), oneETH())
}

// fracETHWei returns a fractional ETH amount as wei.
// e.g., fracETHWei(t, 1, 1000) returns 0.001 ETH in wei.
func fracETHWei(t *testing.T, numerator, denominator int64) *big.Int {
	t.Helper()
	one := oneETH()
	result := new(big.Int).Mul(big.NewInt(numerator), one)
	result.Div(result, big.NewInt(denominator))
	return result
}

func TestPreflightCheck_Approved(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100), // 0.01 ETH profit
		ethWei(t, 10),         // 10 ETH trade
		100.0,                 // 100 gwei gas
		90.0,                  // 90% tip
		1.0,                   // 1 ETH balance
	)

	if !result.Approved {
		t.Errorf("expected approved, got rejected: %s", result.Reason)
	}
	if result.Reason != "approved" {
		t.Errorf("expected reason 'approved', got '%s'", result.Reason)
	}
}

func TestPreflightCheck_GasTooHigh(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100), // 0.01 ETH profit
		ethWei(t, 10),         // 10 ETH trade
		350.0,                 // 350 gwei — above 300 threshold
		90.0,
		1.0,
	)

	if result.Approved {
		t.Error("expected rejected for high gas, got approved")
	}
}

func TestPreflightCheck_LowBalance(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100),
		ethWei(t, 10),
		100.0,
		90.0,
		0.05, // 0.05 ETH — below 0.1 threshold
	)

	if result.Approved {
		t.Error("expected rejected for low balance, got approved")
	}
}

func TestPreflightCheck_TradeTooLarge(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100),
		ethWei(t, 60), // 60 ETH — above 50 ETH max
		100.0,
		90.0,
		1.0,
	)

	if result.Approved {
		t.Error("expected rejected for trade too large, got approved")
	}
}

func TestPreflightCheck_DailyVolumeExceeded(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// Record a trade that fills most of the daily volume (490 ETH)
	rm.RecordTrade(ethWei(t, 490), fracETHWei(t, 1, 100))

	// Try another trade of 20 ETH — would push total to 510 > 500 limit
	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100),
		ethWei(t, 20),
		100.0,
		90.0,
		1.0,
	)

	if result.Approved {
		t.Error("expected rejected for daily volume exceeded, got approved")
	}
}

func TestPreflightCheck_ProfitTooLow(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// 0.0001 ETH profit — below 0.001 ETH minimum
	tinyProfit := fracETHWei(t, 1, 10000)

	result := rm.PreflightCheck(
		tinyProfit,
		ethWei(t, 10),
		100.0,
		90.0,
		1.0,
	)

	if result.Approved {
		t.Error("expected rejected for profit too low, got approved")
	}
}

func TestPreflightCheck_TipShareTooHigh(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	result := rm.PreflightCheck(
		fracETHWei(t, 1, 100),
		ethWei(t, 10),
		100.0,
		96.0, // 96% — above 95% max
		1.0,
	)

	if result.Approved {
		t.Error("expected rejected for tip share too high, got approved")
	}
}

func TestPreflightCheck_SystemNotRunning(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name  string
		state SystemState
	}{
		{"Paused", StatePaused},
		{"Halted", StateHalted},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			rm := NewRiskManager(DefaultRiskConfig())
			rm.state.ForceState(tc.state)

			result := rm.PreflightCheck(
				fracETHWei(t, 1, 100),
				ethWei(t, 10),
				100.0,
				90.0,
				1.0,
			)

			if result.Approved {
				t.Errorf("expected rejected when system is %s, got approved", tc.state)
			}
		})
	}

	// Degraded should still be allowed
	t.Run("Degraded_Allowed", func(t *testing.T) {
		t.Parallel()

		rm := NewRiskManager(DefaultRiskConfig())
		rm.state.ForceState(StateDegraded)

		result := rm.PreflightCheck(
			fracETHWei(t, 1, 100),
			ethWei(t, 10),
			100.0,
			90.0,
			1.0,
		)

		if !result.Approved {
			t.Errorf("expected approved when Degraded, got rejected: %s", result.Reason)
		}
	})
}

func TestRecordRevert_CircuitBreaker(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// Record 3 reverts (threshold for pause)
	rm.RecordRevert()
	rm.RecordRevert()

	// Should still be running after 2
	if rm.State() != StateRunning {
		t.Fatalf("expected Running after 2 reverts, got %s", rm.State())
	}

	// 3rd revert triggers pause
	rm.RecordRevert()

	if rm.State() != StatePaused {
		t.Errorf("expected Paused after 3 reverts, got %s", rm.State())
	}
}

func TestRecordRevert_WindowCleanup(t *testing.T) {
	t.Parallel()

	config := DefaultRiskConfig()
	config.RevertWindowMinutes = 10
	config.ConsecutiveRevertsPause = 3
	rm := NewRiskManager(config)

	// RecordRevert adds to recentReverts and cleans entries outside the window.
	// Since all reverts are recorded "now", they should all be within the window.
	rm.RecordRevert()
	rm.RecordRevert()

	// 2 reverts within window, system still running
	if rm.State() != StateRunning {
		t.Errorf("expected Running after 2 reverts, got %s", rm.State())
	}

	// The cleanup logic removes entries outside the window. Since we can't
	// easily inject old timestamps, we verify that recording a 3rd revert
	// (all within window) triggers the circuit breaker.
	rm.RecordRevert()
	if rm.State() != StatePaused {
		t.Errorf("expected Paused after 3 reverts in window, got %s", rm.State())
	}
}

func TestRecordTrade_DailyLossHalt(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// Record a trade with a large negative PnL (loss of 0.6 ETH > 0.5 threshold)
	negativePnL := new(big.Int).Neg(fracETHWei(t, 6, 10)) // -0.6 ETH
	rm.RecordTrade(ethWei(t, 10), negativePnL)

	if rm.State() != StateHalted {
		t.Errorf("expected Halted after 0.6 ETH loss, got %s", rm.State())
	}
}

func TestRecordTrade_DailyLossHalt_BelowThreshold(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// Record a trade with a loss of 0.4 ETH (below 0.5 threshold)
	negativePnL := new(big.Int).Neg(fracETHWei(t, 4, 10)) // -0.4 ETH
	rm.RecordTrade(ethWei(t, 10), negativePnL)

	if rm.State() != StateRunning {
		t.Errorf("expected Running after 0.4 ETH loss (below threshold), got %s", rm.State())
	}
}

func TestRecordBundleResult(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	rm.RecordBundleResult(true)
	rm.RecordBundleResult(true)
	rm.RecordBundleResult(false)

	rm.mu.RLock()
	submitted := rm.bundlesSubmitted
	included := rm.bundlesIncluded
	rm.mu.RUnlock()

	if submitted != 3 {
		t.Errorf("bundlesSubmitted: got %d, want 3", submitted)
	}
	if included != 2 {
		t.Errorf("bundlesIncluded: got %d, want 2", included)
	}
}

func TestBundleMissRate(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name      string
		included  int
		missed    int
		wantRate  float64
	}{
		{"0 submitted", 0, 0, 0.0},
		{"all included", 10, 0, 0.0},
		{"none included", 0, 10, 100.0},
		{"half included", 5, 5, 50.0},
		{"80% miss rate", 2, 8, 80.0},
		{"20% miss rate", 8, 2, 20.0},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			rm := NewRiskManager(DefaultRiskConfig())

			for i := 0; i < tc.included; i++ {
				rm.RecordBundleResult(true)
			}
			for i := 0; i < tc.missed; i++ {
				rm.RecordBundleResult(false)
			}

			got := rm.BundleMissRate()
			if got != tc.wantRate {
				t.Errorf("BundleMissRate: got %.1f%%, want %.1f%%", got, tc.wantRate)
			}
		})
	}
}

func TestCalculateTipShare_NoHistory(t *testing.T) {
	t.Parallel()

	rm := NewRiskManager(DefaultRiskConfig())

	// With no bundle history, should return the 90% base.
	got := rm.CalculateTipShare(fracETHWei(t, 1, 100), 100.0)
	if got != 90.0 {
		t.Errorf("CalculateTipShare with no history: got %.1f%%, want 90.0%%", got)
	}
}

func TestCalculateTipShare_Adaptive(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name     string
		included int
		missed   int
		wantMin  float64
		wantMax  float64
	}{
		{"all included — tip decreases", 10, 0, 70.0, 78.0},
		{"all missed — tip increases toward max", 0, 10, 93.0, 95.0},
		{"50% inclusion — base tip", 5, 5, 89.0, 91.0},
		{"75% inclusion — below base", 15, 5, 77.0, 85.0},
		{"25% inclusion — above base", 5, 15, 93.0, 95.0},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			rm := NewRiskManager(DefaultRiskConfig())
			for i := 0; i < tc.included; i++ {
				rm.RecordBundleResult(true)
			}
			for i := 0; i < tc.missed; i++ {
				rm.RecordBundleResult(false)
			}

			got := rm.CalculateTipShare(fracETHWei(t, 1, 100), 100.0)
			if got < tc.wantMin || got > tc.wantMax {
				t.Errorf("CalculateTipShare: got %.1f%%, want in [%.1f%%, %.1f%%]",
					got, tc.wantMin, tc.wantMax)
			}
		})
	}
}

func TestCalculateTipShare_ClampedToMax(t *testing.T) {
	t.Parallel()

	cfg := DefaultRiskConfig()
	cfg.MaxTipSharePct = 92.0
	rm := NewRiskManager(cfg)

	// All misses should push tip up, but it must not exceed MaxTipSharePct.
	for i := 0; i < 20; i++ {
		rm.RecordBundleResult(false)
	}

	got := rm.CalculateTipShare(fracETHWei(t, 1, 100), 100.0)
	if got > cfg.MaxTipSharePct {
		t.Errorf("CalculateTipShare exceeded max: got %.1f%%, max %.1f%%", got, cfg.MaxTipSharePct)
	}
}

func TestWeiToETH(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		wei  *big.Int
		want float64
	}{
		{"zero", big.NewInt(0), 0.0},
		{"1 ETH", oneETH(), 1.0},
		{"0.5 ETH", new(big.Int).Div(oneETH(), big.NewInt(2)), 0.5},
		{"small value (1000 wei)", big.NewInt(1000), 1e-15},
		{"2 ETH", new(big.Int).Mul(oneETH(), big.NewInt(2)), 2.0},
		{"0.001 ETH", new(big.Int).Div(oneETH(), big.NewInt(1000)), 0.001},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got := WeiToETH(tc.wei)
			// Use a relative tolerance for floating point comparison
			diff := got - tc.want
			if diff < 0 {
				diff = -diff
			}
			tolerance := tc.want * 1e-9
			if tolerance < 1e-20 {
				tolerance = 1e-20
			}
			if diff > tolerance {
				t.Errorf("WeiToETH(%s): got %e, want %e", tc.wei.String(), got, tc.want)
			}
		})
	}
}
