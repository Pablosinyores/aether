package risk

import (
	"fmt"
	"log"
	"math/big"
	"sync"
	"time"

	"github.com/aether-arb/aether/internal/config"
)

// RevertType classifies a transaction revert.
type RevertType string

const (
	// RevertCompetitive is an expected revert: another searcher/bot won the same MEV
	// opportunity. Flash loan reverts are always atomic and cost no gas — these are
	// normal in competitive environments and should NOT count toward the circuit breaker.
	RevertCompetitive RevertType = "competitive"

	// RevertBug is an unexpected revert caused by a logic error, bad simulation,
	// stale data, or misconfiguration. These DO count toward the circuit breaker.
	RevertBug RevertType = "bug"
)

// RiskConfig holds all risk management parameters.
type RiskConfig struct {
	MaxGasGwei                float64
	ConsecutiveRevertsPause   int
	RevertWindowMinutes       int
	CompetitiveRevertAlertPct float64
	DailyLossHaltETH          float64
	MinETHBalance             float64
	MaxNodeLatencyMs          int64
	BundleMissRateAlertPct    float64
	BundleMissRateWindowMin   int
	MaxSingleTradeETH         float64
	MaxDailyVolumeETH         float64
	MinProfitETH              float64
	MaxTipSharePct            float64
}

// DefaultRiskConfig returns the default risk parameters.
func DefaultRiskConfig() RiskConfig {
	return RiskConfig{
		MaxGasGwei:                300,
		ConsecutiveRevertsPause:   10,
		RevertWindowMinutes:       10,
		CompetitiveRevertAlertPct: 90,
		DailyLossHaltETH:          0.5,
		MinETHBalance:             0.1,
		MaxNodeLatencyMs:          500,
		BundleMissRateAlertPct:    80,
		BundleMissRateWindowMin:   60,
		MaxSingleTradeETH:         50.0,
		MaxDailyVolumeETH:         500.0,
		MinProfitETH:              0.001,
		MaxTipSharePct:            95.0,
	}
}

// LoadRiskConfig reads a risk YAML config file and returns a RiskConfig.
// It delegates parsing and validation to config.LoadRiskConfig, then maps
// the file-level struct fields to the RiskConfig used by the RiskManager.
func LoadRiskConfig(path string) (RiskConfig, error) {
	fc, err := config.LoadRiskConfig(path)
	if err != nil {
		return RiskConfig{}, fmt.Errorf("load risk config: %w", err)
	}

	return RiskConfig{
		MaxGasGwei:                fc.CircuitBreakers.MaxGasGwei,
		ConsecutiveRevertsPause:   fc.CircuitBreakers.ConsecutiveRevertsPause,
		RevertWindowMinutes:       fc.CircuitBreakers.RevertWindowMinutes,
		CompetitiveRevertAlertPct: fc.CircuitBreakers.CompetitiveRevertAlertPct,
		DailyLossHaltETH:          fc.CircuitBreakers.DailyLossHaltETH,
		MinETHBalance:             fc.CircuitBreakers.MinETHBalance,
		MaxNodeLatencyMs:          fc.CircuitBreakers.MaxNodeLatencyMs,
		BundleMissRateAlertPct:    fc.CircuitBreakers.BundleMissRateAlertPct,
		BundleMissRateWindowMin:   fc.CircuitBreakers.BundleMissRateWindowMinutes,
		MaxSingleTradeETH:         fc.PositionLimits.MaxSingleTradeETH,
		MaxDailyVolumeETH:         fc.PositionLimits.MaxDailyVolumeETH,
		MinProfitETH:              fc.PositionLimits.MinProfitETH,
		MaxTipSharePct:            fc.PositionLimits.MaxTipSharePct,
	}, nil
}

// PreflightResult holds the result of a preflight check.
type PreflightResult struct {
	Approved bool
	Reason   string
}

// RiskManager implements circuit breakers and position limits.
type RiskManager struct {
	mu                  sync.RWMutex
	config              RiskConfig
	state               *SystemStateMachine
	recentBugReverts    []time.Time // Only these count toward circuit breaker
	recentCompReverts   []time.Time // Tracked separately for metrics / stale-data alert
	dailyVolume         *big.Int    // Wei
	dailyPnL            *big.Int    // Wei (can be negative)
	dailyResetTime      time.Time
	bundlesSubmitted    int
	bundlesIncluded     int
}

// NewRiskManager creates a new risk manager.
func NewRiskManager(config RiskConfig) *RiskManager {
	return &RiskManager{
		config:            config,
		state:             NewSystemStateMachine(),
		recentBugReverts:  make([]time.Time, 0),
		recentCompReverts: make([]time.Time, 0),
		dailyVolume:       big.NewInt(0),
		dailyPnL:          big.NewInt(0),
		dailyResetTime:    time.Now().Truncate(24 * time.Hour).Add(24 * time.Hour),
	}
}

// PreflightCheck validates an arb opportunity against all risk limits.
func (rm *RiskManager) PreflightCheck(
	profitWei *big.Int,
	tradeValueWei *big.Int,
	gasGwei float64,
	tipSharePct float64,
	ethBalance float64,
) PreflightResult {
	rm.mu.RLock()
	defer rm.mu.RUnlock()

	// Check system state
	if rm.state.Current() != StateRunning && rm.state.Current() != StateDegraded {
		return PreflightResult{false, fmt.Sprintf("system state: %s", rm.state.Current())}
	}

	// Circuit breaker: gas price
	if gasGwei > rm.config.MaxGasGwei {
		return PreflightResult{false, fmt.Sprintf("gas too high: %.1f > %.1f gwei", gasGwei, rm.config.MaxGasGwei)}
	}

	// Circuit breaker: ETH balance
	if ethBalance < rm.config.MinETHBalance {
		return PreflightResult{false, fmt.Sprintf("ETH balance too low: %.4f < %.4f", ethBalance, rm.config.MinETHBalance)}
	}

	// Position limit: single trade size
	tradeETH := WeiToETH(tradeValueWei)
	if tradeETH > rm.config.MaxSingleTradeETH {
		return PreflightResult{false, fmt.Sprintf("trade too large: %.2f > %.2f ETH", tradeETH, rm.config.MaxSingleTradeETH)}
	}

	// Position limit: daily volume
	newVolume := new(big.Int).Add(rm.dailyVolume, tradeValueWei)
	if WeiToETH(newVolume) > rm.config.MaxDailyVolumeETH {
		return PreflightResult{false, fmt.Sprintf("daily volume exceeded: %.2f ETH", rm.config.MaxDailyVolumeETH)}
	}

	// Position limit: minimum profit
	profitETH := WeiToETH(profitWei)
	if profitETH < rm.config.MinProfitETH {
		return PreflightResult{false, fmt.Sprintf("profit too low: %.6f < %.6f ETH", profitETH, rm.config.MinProfitETH)}
	}

	// Position limit: max tip share
	if tipSharePct > rm.config.MaxTipSharePct {
		return PreflightResult{false, fmt.Sprintf("tip share too high: %.1f%% > %.1f%%", tipSharePct, rm.config.MaxTipSharePct)}
	}

	return PreflightResult{true, "approved"}
}

// RecordRevert records a transaction revert for circuit breaker tracking.
//
// Only RevertBug reverts count toward the consecutive reverts circuit breaker.
// RevertCompetitive reverts are tracked separately; a high competitive-revert
// rate emits an alert indicating the bot may be acting on stale data.
func (rm *RiskManager) RecordRevert(revertType RevertType) {
	rm.mu.Lock()
	defer rm.mu.Unlock()

	now := time.Now()
	cutoff := now.Add(-time.Duration(rm.config.RevertWindowMinutes) * time.Minute)

	// Helper to prune entries outside the rolling window.
	prune := func(ts []time.Time) []time.Time {
		out := ts[:0]
		for _, t := range ts {
			if t.After(cutoff) {
				out = append(out, t)
			}
		}
		return out
	}

	switch revertType {
	case RevertCompetitive:
		rm.recentCompReverts = append(prune(rm.recentCompReverts), now)
	default: // RevertBug (and any unknown type) — conservative: count it
		rm.recentBugReverts = append(prune(rm.recentBugReverts), now)
	}

	// --- Bug-revert circuit breaker ---
	if len(rm.recentBugReverts) >= rm.config.ConsecutiveRevertsPause {
		log.Printf("CIRCUIT BREAKER: %d bug reverts in %d minutes, pausing",
			len(rm.recentBugReverts), rm.config.RevertWindowMinutes)
		rm.state.Transition(StatePaused)
	}

	// --- Competitive-revert stale-data alert ---
	totalReverts := len(rm.recentBugReverts) + len(rm.recentCompReverts)
	if totalReverts > 0 {
		compPct := float64(len(rm.recentCompReverts)) / float64(totalReverts) * 100
		if compPct >= rm.config.CompetitiveRevertAlertPct {
			log.Printf("ALERT: competitive revert rate %.0f%% (>= %.0f%%) in last %d min — possible stale data",
				compPct, rm.config.CompetitiveRevertAlertPct, rm.config.RevertWindowMinutes)
		}
	}
}

// RecordTrade records a completed trade for daily tracking.
func (rm *RiskManager) RecordTrade(volumeWei *big.Int, pnlWei *big.Int) {
	rm.mu.Lock()
	defer rm.mu.Unlock()

	rm.maybeResetDaily()
	rm.dailyVolume.Add(rm.dailyVolume, volumeWei)
	rm.dailyPnL.Add(rm.dailyPnL, pnlWei)

	// Check daily loss halt
	lossETH := WeiToETH(new(big.Int).Neg(rm.dailyPnL))
	if lossETH > rm.config.DailyLossHaltETH {
		log.Printf("CIRCUIT BREAKER: daily loss %.4f ETH exceeds threshold %.4f ETH, halting",
			lossETH, rm.config.DailyLossHaltETH)
		rm.state.Transition(StateHalted)
	}
}

// RecordBundleResult records bundle inclusion/miss for rate tracking.
func (rm *RiskManager) RecordBundleResult(included bool) {
	rm.mu.Lock()
	defer rm.mu.Unlock()

	rm.bundlesSubmitted++
	if included {
		rm.bundlesIncluded++
	}
}

// BundleMissRate returns the current bundle miss rate as a percentage.
func (rm *RiskManager) BundleMissRate() float64 {
	rm.mu.RLock()
	defer rm.mu.RUnlock()

	if rm.bundlesSubmitted == 0 {
		return 0.0
	}
	missRate := float64(rm.bundlesSubmitted-rm.bundlesIncluded) / float64(rm.bundlesSubmitted) * 100
	return missRate
}

// State returns the current system state.
func (rm *RiskManager) State() SystemState {
	return rm.state.Current()
}

func (rm *RiskManager) maybeResetDaily() {
	if time.Now().After(rm.dailyResetTime) {
		rm.dailyVolume = big.NewInt(0)
		rm.dailyPnL = big.NewInt(0)
		rm.dailyResetTime = time.Now().Truncate(24 * time.Hour).Add(24 * time.Hour)
		log.Println("Daily counters reset")
	}
}

// WeiToETH converts wei to ETH as float64.
func WeiToETH(wei *big.Int) float64 {
	eth := new(big.Float).SetInt(wei)
	divisor := new(big.Float).SetFloat64(1e18)
	eth.Quo(eth, divisor)
	f, _ := eth.Float64()
	return f
}
