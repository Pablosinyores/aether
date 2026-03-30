package risk

import "strings"

// competitiveExactReasons are exact (normalised) revert reasons that indicate
// another searcher beat us to the same MEV opportunity.
//
// All entries must be lowercase; ClassifyRevert normalises the input before
// matching.
var competitiveExactReasons = map[string]struct{}{
	"":          {}, // many builders omit reason on expected race losses
	"k":         {},
	"iia":       {},
	"lok":       {},
	"spl":       {},
	"tlm":       {},
	"tml":       {},
	"as":        {},
	"m0":        {},
	"m1":        {},
	"bal#507":   {},
	"bal#208":   {},
	"minreturn": {},
}

// competitivePatterns are revert-reason substrings that indicate expected MEV
// competition and therefore should NOT count toward the circuit breaker.
var competitivePatterns = []string{
	// Builder / mempool rejections when a same-nonce tx already landed
	"nonce too low",
	"already known",
	"replacement transaction underpriced",
	"transaction underpriced",

	// Bundle simulation: someone else captured the arb first
	"bundle collision",
	"already included",
	"already executed",
	"arbitrage already executed",
	"arb already taken",

	// Flash-loan simulation: the profit route dried up (another bot was faster)
	"insufficient output amount",
	"insufficient_output_amount",
	"insufficient liquidity",
	"price impact too high",
	"k invariant",
	"uniswapv2: k",
	"invariant violation",
	"slippage",
	"too little received",
	"excessive_input_amount",
	"exchange resulted in fewer coins than expected",
	"return amount is not enough",

	// Generic "another tx beat us" signal from some builders
	"frontrun",
	"sandwich",
	"mev already captured",
}

// ClassifyRevert maps a raw revert-reason string to a RevertType.
//
// It performs a case-insensitive substring search against a list of known
// competitive patterns. If the reason matches any of them the revert is
// classified as RevertCompetitive (expected MEV race). Everything else,
// including unknown reasons, is classified conservatively as RevertBug, so
// the circuit breaker is not silently bypassed for novel
// failure modes.
func ClassifyRevert(revertReason string) RevertType {
	lower := strings.ToLower(strings.TrimSpace(revertReason))

	if _, ok := competitiveExactReasons[lower]; ok {
		return RevertCompetitive
	}

	for _, pattern := range competitivePatterns {
		if strings.Contains(lower, pattern) {
			return RevertCompetitive
		}
	}
	return RevertBug
}
