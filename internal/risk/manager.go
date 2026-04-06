package risk

import (
	"fmt"
	"log"
	"math/big"
	"sync"
	"time"

	"github.com/aether-arb/aether/internal/config"
)

// RiskConfig holds all risk management parameters.
type RiskConfig struct {
	MaxGasGwei              float64
	ConsecutiveRevertsPause int
	RevertWindowMinutes     int
	DailyLossHaltETH        float64
	MinETHBalance           float64
	MaxNodeLatencyMs        int64
	BundleMissRateAlertPct  float64
	BundleMissRateWindowMin int
	MaxSingleTradeETH       float64
	MaxDailyVolumeETH       float64
	MinProfitETH            float64
	MaxTipSharePct          float64
}

// DefaultRiskConfig returns the default risk parameters.
func DefaultRiskConfig() RiskConfig {
	return RiskConfig{
		MaxGasGwei:              300,
		ConsecutiveRevertsPause: 3,
		RevertWindowMinutes:     10,
		DailyLossHaltETH:        0.5,
		MinETHBalance:           0.1,
		MaxNodeLatencyMs:        500,
		BundleMissRateAlertPct:  80,
		BundleMissRateWindowMin: 60,
		MaxSingleTradeETH:       50.0,
		MaxDailyVolumeETH:       500.0,
		MinProfitETH:            0.001,
		MaxTipSharePct:          95.0,
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
		MaxGasGwei:              fc.CircuitBreakers.MaxGasGwei,
		ConsecutiveRevertsPause: fc.CircuitBreakers.ConsecutiveRevertsPause,
		RevertWindowMinutes:     fc.CircuitBreakers.RevertWindowMinutes,
		DailyLossHaltETH:        fc.CircuitBreakers.DailyLossHaltETH,
		MinETHBalance:           fc.CircuitBreakers.MinETHBalance,
		MaxNodeLatencyMs:        fc.CircuitBreakers.MaxNodeLatencyMs,
		BundleMissRateAlertPct:  fc.CircuitBreakers.BundleMissRateAlertPct,
		BundleMissRateWindowMin: fc.CircuitBreakers.BundleMissRateWindowMinutes,
		MaxSingleTradeETH:       fc.PositionLimits.MaxSingleTradeETH,
		MaxDailyVolumeETH:       fc.PositionLimits.MaxDailyVolumeETH,
		MinProfitETH:            fc.PositionLimits.MinProfitETH,
		MaxTipSharePct:          fc.PositionLimits.MaxTipSharePct,
	}, nil
}

// PreflightResult holds the result of a preflight check.
type PreflightResult struct {
	Approved bool
	Reason   string
}

// RiskManager implements circuit breakers and position limits.
type RiskManager struct {
	mu               sync.RWMutex
	config           RiskConfig
	state            *SystemStateMachine
	recentReverts    []time.Time
	dailyVolume      *big.Int // Wei
	dailyPnL         *big.Int // Wei (can be negative)
	dailyResetTime   time.Time
	bundlesSubmitted int
	bundlesIncluded  int
}

// NewRiskManager creates a new risk manager.
func NewRiskManager(config RiskConfig) *RiskManager {
	return &RiskManager{
		config:         config,
		state:          NewSystemStateMachine(),
		recentReverts:  make([]time.Time, 0),
		dailyVolume:    big.NewInt(0),
		dailyPnL:       big.NewInt(0),
		dailyResetTime: time.Now().Truncate(24 * time.Hour).Add(24 * time.Hour),
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
func (rm *RiskManager) RecordRevert() {
	rm.mu.Lock()
	defer rm.mu.Unlock()

	now := time.Now()
	rm.recentReverts = append(rm.recentReverts, now)

	// Clean old reverts outside window
	cutoff := now.Add(-time.Duration(rm.config.RevertWindowMinutes) * time.Minute)
	cleaned := make([]time.Time, 0)
	for _, t := range rm.recentReverts {
		if t.After(cutoff) {
			cleaned = append(cleaned, t)
		}
	}
	rm.recentReverts = cleaned

	// Check consecutive reverts threshold
	if len(rm.recentReverts) >= rm.config.ConsecutiveRevertsPause {
		log.Printf("CIRCUIT BREAKER: %d reverts in %d minutes, pausing",
			len(rm.recentReverts), rm.config.RevertWindowMinutes)
		rm.state.Transition(StatePaused)
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

// CalculateTipShare returns an adaptive tip share percentage based on recent
// bundle inclusion rate. The base tip is 90%. When inclusion drops below 50%,
// the tip increases toward MaxTipSharePct to incentivize builders. When
// inclusion is above 50%, the tip decreases toward a 70% floor to retain more
// profit. The result is always clamped to [70, MaxTipSharePct].
func (rm *RiskManager) CalculateTipShare() float64 {
	rm.mu.RLock()
	defer rm.mu.RUnlock()

	const (
		baseTipPct = 90.0
		minTipPct  = 70.0
	)
	maxTipPct := rm.config.MaxTipSharePct

	// No history yet — use the base tip.
	if rm.bundlesSubmitted == 0 {
		return baseTipPct
	}

	inclusionRate := float64(rm.bundlesIncluded) / float64(rm.bundlesSubmitted)

	// Linear adjustment: below 50% inclusion we increase tip, above we decrease.
	// At 50% inclusion the adjustment is zero (returns baseTipPct).
	// At 0% inclusion: +maxAdjust  (tip goes up toward maxTipPct)
	// At 100% inclusion: -maxAdjust (tip goes down toward minTipPct)
	const midpoint = 0.5
	maxAdjust := (maxTipPct - minTipPct) / 2.0
	adjustment := (midpoint - inclusionRate) * 2.0 * maxAdjust

	tip := baseTipPct + adjustment

	// Clamp to [minTipPct, maxTipPct]
	if tip < minTipPct {
		tip = minTipPct
	}
	if tip > maxTipPct {
		tip = maxTipPct
	}

	return tip
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
