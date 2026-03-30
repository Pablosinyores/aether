package risk

import "testing"

func TestClassifyRevert_Competitive(t *testing.T) {
	t.Parallel()

	cases := []string{
		// Uniswap V2/V3
		"INSUFFICIENT_OUTPUT_AMOUNT",
		"UniswapV2: INSUFFICIENT_OUTPUT_AMOUNT",
		"UniswapV2: K",
		"K", "IIA", "LOK", "SPL", "TLM", "TML", "AS", "M0", "M1",
		// Generic slippage
		"Too little received",
		"Slippage too high",
		"Price slippage check",
		"EXCESSIVE_INPUT_AMOUNT",
		// Balancer
		"BAL#507", "BAL#208",
		// Curve
		"Exchange resulted in fewer coins than expected",
		// 1inch
		"MinReturn",
		"Return amount is not enough",
		// Empty = front-run, competitive
		"",
	}

	for _, reason := range cases {
		reason := reason
		t.Run("competitive/"+reason, func(t *testing.T) {
			t.Parallel()
			if got := ClassifyRevert(reason); got != RevertCompetitive {
				t.Errorf("ClassifyRevert(%q) = %q, want %q", reason, got, RevertCompetitive)
			}
		})
	}
}

func TestClassifyRevert_Bug(t *testing.T) {
	t.Parallel()

	cases := []string{
		"insufficient allowance",
		"ERC20: transfer amount exceeds balance",
		"arithmetic overflow",
		"out of gas",
		"invalid opcode",
		"execution reverted: custom error 0xdeadbeef",
		"stack overflow",
		"unknown error XYZ",
	}

	for _, reason := range cases {
		reason := reason
		t.Run("bug/"+reason, func(t *testing.T) {
			t.Parallel()
			if got := ClassifyRevert(reason); got != RevertBug {
				t.Errorf("ClassifyRevert(%q) = %q, want %q", reason, got, RevertBug)
			}
		})
	}
}

// ClassifyRevert must be conservative — unknown reasons default to bug.
func TestClassifyRevert_UnknownDefaultsToBug(t *testing.T) {
	t.Parallel()

	if got := ClassifyRevert("some future unknown error"); got != RevertBug {
		t.Errorf("ClassifyRevert(unknown) = %q, want %q", got, RevertBug)
	}
}