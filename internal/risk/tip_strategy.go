package risk

import (
	"math/big"
	"sync"
)

// TipStrategy computes the tip share percentage to use for bundle submission.
type TipStrategy interface {
	CalculateTip(profitWei *big.Int, inclusionRate float64, gasGwei float64) float64
}

// AdaptiveTipStrategy adjusts tip share in +/- step increments based on
// observed inclusion rate while respecting configured floor/ceiling bounds.
type AdaptiveTipStrategy struct {
	mu                     sync.Mutex
	currentTipPct          float64
	minTipPct              float64
	maxTipPct              float64
	stepPct                float64
	lowInclusionThreshold  float64
	highInclusionThreshold float64
}

// NewAdaptiveTipStrategy creates an adaptive tip strategy with sane defaults.
func NewAdaptiveTipStrategy(startTipPct, minTipPct, maxTipPct, stepPct float64) *AdaptiveTipStrategy {
	if minTipPct <= 0 {
		minTipPct = 50.0
	}
	if maxTipPct <= 0 || maxTipPct > 100 {
		maxTipPct = 95.0
	}
	if minTipPct >= maxTipPct {
		minTipPct = 50.0
		maxTipPct = 95.0
	}
	if stepPct <= 0 {
		stepPct = 5.0
	}

	startTipPct = clampTip(startTipPct, minTipPct, maxTipPct)

	return &AdaptiveTipStrategy{
		currentTipPct:          startTipPct,
		minTipPct:              minTipPct,
		maxTipPct:              maxTipPct,
		stepPct:                stepPct,
		lowInclusionThreshold:  50.0,
		highInclusionThreshold: 80.0,
	}
}

// CalculateTip applies one adjustment step based on inclusion rate.
//
// - inclusion < 50%  => increase tip by step
// - inclusion > 80%  => decrease tip by step
// - otherwise        => keep current tip
func (s *AdaptiveTipStrategy) CalculateTip(profitWei *big.Int, inclusionRate float64, gasGwei float64) float64 {
	// This baseline strategy adapts only on recent inclusion performance.
	// profitWei and gasGwei are part of the interface so future strategies
	// can incorporate margin/gas sensitivity without API changes. Logging of
	// effective tip changes is emitted by RiskManager.CalculateTipShare.
	_ = profitWei
	_ = gasGwei

	s.mu.Lock()
	defer s.mu.Unlock()

	if inclusionRate < 0 || inclusionRate > 100 {
		return s.currentTipPct
	}

	if inclusionRate < s.lowInclusionThreshold {
		s.currentTipPct += s.stepPct
	} else if inclusionRate > s.highInclusionThreshold {
		s.currentTipPct -= s.stepPct
	}

	s.currentTipPct = clampTip(s.currentTipPct, s.minTipPct, s.maxTipPct)
	return s.currentTipPct
}

func clampTip(v, minV, maxV float64) float64 {
	if v < minV {
		return minV
	}
	if v > maxV {
		return maxV
	}
	return v
}
