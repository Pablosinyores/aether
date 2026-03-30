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

func TestRecordRevert_BugCircuitBreaker(t *testing.T) {
	t.Parallel()

	config := DefaultRiskConfig()
	config.ConsecutiveRevertsPause = 3 // lower threshold for test speed
	rm := NewRiskManager(config)

	// Record 2 bug reverts — should still be running.
	rm.RecordRevert(RevertBug)
	rm.RecordRevert(RevertBug)
	if rm.State() != StateRunning {
		t.Fatalf("expected Running after 2 bug reverts, got %s", rm.State())
	}

	// 3rd bug revert hits threshold → pause.
	rm.RecordRevert(RevertBug)
	if rm.State() != StatePaused {
		t.Errorf("expected Paused after 3 bug reverts, got %s", rm.State())
	}
}

func TestRecordRevert_CompetitiveDoesNotTriggerCircuitBreaker(t *testing.T) {
	t.Parallel()

	config := DefaultRiskConfig()
	config.ConsecutiveRevertsPause = 3
	config.CompetitiveRevertAlertPct = 100 // suppress alert so only CB fires
	rm := NewRiskManager(config)

	// Record many competitive reverts — circuit breaker must NOT fire.
	for i := 0; i < 20; i++ {
		rm.RecordRevert(RevertCompetitive)
	}
	if rm.State() != StateRunning {
		t.Errorf("expected Running after 20 competitive reverts, got %s", rm.State())
	}
}

func TestRecordRevert_MixedTypes_OnlyBugCountsTowardBreaker(t *testing.T) {
	t.Parallel()

	config := DefaultRiskConfig()
	config.ConsecutiveRevertsPause = 3
	config.CompetitiveRevertAlertPct = 100 // suppress alert
	rm := NewRiskManager(config)

	// 2 competitive + 2 bug = still under threshold (only 2 bug reverts).
	rm.RecordRevert(RevertCompetitive)
	rm.RecordRevert(RevertCompetitive)
	rm.RecordRevert(RevertBug)
	rm.RecordRevert(RevertBug)
	if rm.State() != StateRunning {
		t.Errorf("expected Running with 2 bug reverts (threshold=3), got %s", rm.State())
	}

	// One more bug revert → pause.
	rm.RecordRevert(RevertBug)
	if rm.State() != StatePaused {
		t.Errorf("expected Paused after 3rd bug revert, got %s", rm.State())
	}
}

func TestRecordRevert_CompetitiveRateAlert(t *testing.T) {
	t.Parallel()

	// Set a low alert threshold to make it easy to trigger.
	config := DefaultRiskConfig()
	config.ConsecutiveRevertsPause = 100 // effectively disable CB for this test
	config.CompetitiveRevertAlertPct = 80 // alert at 80%+
	rm := NewRiskManager(config)

	// 8 competitive + 2 bug = 80% competitive → should log alert but NOT pause.
	for i := 0; i < 8; i++ {
		rm.RecordRevert(RevertCompetitive)
	}
	for i := 0; i < 2; i++ {
		rm.RecordRevert(RevertBug)
	}

	// System must remain running (CB not triggered).
	if rm.State() != StateRunning {
		t.Errorf("expected Running (alert only, not CB), got %s", rm.State())
	}
}

func TestRecordRevert_WindowCleanup(t *testing.T) {
	t.Parallel()

	config := DefaultRiskConfig()
	config.RevertWindowMinutes = 10
	config.ConsecutiveRevertsPause = 3
	rm := NewRiskManager(config)

	// All bug reverts recorded "now" — all within the window.
	rm.RecordRevert(RevertBug)
	rm.RecordRevert(RevertBug)

	// 2 bug reverts: still running.
	if rm.State() != StateRunning {
		t.Errorf("expected Running after 2 bug reverts, got %s", rm.State())
	}

	// 3rd bug revert (all within window) → circuit breaker fires.
	rm.RecordRevert(RevertBug)
	if rm.State() != StatePaused {
		t.Errorf("expected Paused after 3 bug reverts in window, got %s", rm.State())
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
