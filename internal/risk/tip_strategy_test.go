package risk

import (
	"math/big"
	"testing"
)

func TestAdaptiveTipStrategy_IncreaseOnLowInclusion(t *testing.T) {
	t.Parallel()

	s := NewAdaptiveTipStrategy(90, 50, 95, 5)
	got := s.CalculateTip(big.NewInt(1), 40, 30)

	if got != 95 {
		t.Fatalf("tip share = %.1f, want 95.0", got)
	}
}

func TestAdaptiveTipStrategy_DecreaseOnHighInclusion(t *testing.T) {
	t.Parallel()

	s := NewAdaptiveTipStrategy(90, 50, 95, 5)
	got := s.CalculateTip(big.NewInt(1), 90, 30)

	if got != 85 {
		t.Fatalf("tip share = %.1f, want 85.0", got)
	}
}

func TestAdaptiveTipStrategy_NoChangeInMidBand(t *testing.T) {
	t.Parallel()

	s := NewAdaptiveTipStrategy(90, 50, 95, 5)
	got := s.CalculateTip(big.NewInt(1), 70, 30)

	if got != 90 {
		t.Fatalf("tip share = %.1f, want 90.0", got)
	}
}

func TestAdaptiveTipStrategy_RespectsBounds(t *testing.T) {
	t.Parallel()

	high := NewAdaptiveTipStrategy(95, 50, 95, 5)
	if got := high.CalculateTip(big.NewInt(1), 0, 30); got != 95 {
		t.Fatalf("upper bound tip share = %.1f, want 95.0", got)
	}

	low := NewAdaptiveTipStrategy(50, 50, 95, 5)
	if got := low.CalculateTip(big.NewInt(1), 100, 30); got != 50 {
		t.Fatalf("lower bound tip share = %.1f, want 50.0", got)
	}
}

func TestAdaptiveTipStrategy_InvalidInclusionKeepsCurrentTip(t *testing.T) {
	t.Parallel()

	s := NewAdaptiveTipStrategy(90, 50, 95, 5)
	if got := s.CalculateTip(big.NewInt(1), -1, 30); got != 90 {
		t.Fatalf("tip share = %.1f, want 90.0 for invalid inclusion", got)
	}
}
