package main

import (
	"context"
	"errors"
	"math/big"
	"testing"

	pb "github.com/aether-arb/aether/internal/pb"
	"github.com/aether-arb/aether/internal/risk"
)

// bigIntToBytes converts a big.Int to its byte representation for proto fields.
func bigIntToBytes(n *big.Int) []byte {
	return n.Bytes()
}

// ethToWei converts an ETH float value to wei as big.Int.
func ethToWei(eth float64) *big.Int {
	// Multiply by 1e18 using big.Float for precision
	f := new(big.Float).SetFloat64(eth)
	f.Mul(f, new(big.Float).SetFloat64(1e18))
	wei, _ := f.Int(nil)
	return wei
}

// newTestComponents creates a standard set of executor components for testing.
// The submitter has no searcher key, so bundles without RawTxs will fail
// submission (expected until signer is wired into test setup).
func newTestComponents() (*risk.RiskManager, *BundleConstructor, *Submitter) {
	rm := risk.NewRiskManager(risk.DefaultRiskConfig())
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)
	submitter, _ := NewSubmitter(defaultBuilderConfigs(), "")
	submitter.submitFn = func(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
		return SubmissionResult{Builder: builder.Name, Success: true, BundleHash: "test-hash"}
	}
	return rm, bundler, submitter
}

// newValidArb creates a ValidatedArb proto message with the given profit and trade size.
func newValidArb(id string, profitETH float64, tradeETH float64) *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id:              id,
		Hops:            []*pb.ArbHop{{PoolAddress: []byte{0x01}, Protocol: pb.ProtocolType_UNISWAP_V2}},
		NetProfitWei:    bigIntToBytes(ethToWei(profitETH)),
		TotalGas:        200000,
		BlockNumber:     18000000,
		FlashloanAmount: bigIntToBytes(ethToWei(tradeETH)),
		Calldata:        []byte{0xab, 0xcd, 0xef},
	}
}

func TestProcessArb_Approved(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	arb := newValidArb("arb-approved-001", 0.01, 5.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if !submitted {
		t.Fatal("expected arb to be submitted, got submitted=false")
	}
}

func TestProcessArb_RejectedLowProfit(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	// Profit of 0.0001 ETH is below the 0.001 ETH minimum threshold
	arb := newValidArb("arb-lowprofit-001", 0.0001, 5.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (low profit), got submitted=true")
	}
}

func TestProcessArb_RejectedHighGas(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	// Set gas oracle to 350 gwei (above 300 gwei threshold)
	bundler.gasOracle.Update(big.NewInt(350e9), big.NewInt(2e9))

	arb := newValidArb("arb-highgas-001", 0.01, 5.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (high gas), got submitted=true")
	}
}

func TestProcessArb_RejectedLowBalance(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	arb := newValidArb("arb-lowbal-001", 0.01, 5.0)

	// Pass ethBalance of 0.05, below the 0.1 ETH minimum
	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.05)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (low ETH balance), got submitted=true")
	}
}

func TestProcessArb_RejectedTradeTooLarge(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	// Trade of 60 ETH exceeds the 50 ETH single trade limit
	arb := newValidArb("arb-bigtrade-001", 0.5, 60.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (trade too large), got submitted=true")
	}
}

func TestProcessArb_SystemPaused(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	// Trigger circuit breaker: 10 consecutive bug reverts pauses the system
	// (competitive/MEV reverts are excluded from this count — see issue #27)
	for i := 0; i < 10; i++ {
		rm.RecordRevert(risk.RevertBug)
	}

	// Verify the system is no longer in Running state
	if rm.State() == risk.StateRunning {
		t.Fatal("expected system to be paused after 10 bug reverts, still running")
	}

	arb := newValidArb("arb-paused-001", 0.01, 5.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (system paused), got submitted=true")
	}
}

func TestProcessArb_CompetitiveReverts_DoNotPause(t *testing.T) {
	t.Parallel()

	rc := risk.DefaultRiskConfig()
	rc.ConsecutiveRevertsPause = 3
	rm := risk.NewRiskManager(rc)

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)

	builders := []BuilderConfig{
		{Name: "b1", Enabled: true, TimeoutMs: 1000},
		{Name: "b2", Enabled: true, TimeoutMs: 1000},
		{Name: "b3", Enabled: true, TimeoutMs: 1000},
	}
	submitter, _ := NewSubmitter(builders, "")
	submitter.submitFn = func(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   errors.New("execution reverted: UniswapV2: INSUFFICIENT_OUTPUT_AMOUNT"),
		}
	}

	arb := newValidArb("arb-competitive-001", 0.01, 5.0)

	for i := 0; i < 2; i++ {
		submitted, err := processArb(context.Background(), arb, rm, bundler, submitter, nil,
			"0x0000000000000000000000000000000000000000", 0.5)
		if err != nil {
			t.Fatalf("processArb: %v", err)
		}
		if submitted {
			t.Fatal("expected submitted=false when all builders reject")
		}
	}

	if rm.State() != risk.StateRunning {
		t.Fatalf("expected Running after competitive reverts, got %s", rm.State())
	}
}

func TestProcessArb_BugReverts_PauseSystem(t *testing.T) {
	t.Parallel()

	rc := risk.DefaultRiskConfig()
	rc.ConsecutiveRevertsPause = 2
	rm := risk.NewRiskManager(rc)

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)

	builders := []BuilderConfig{
		{Name: "b1", Enabled: true, TimeoutMs: 1000},
		{Name: "b2", Enabled: true, TimeoutMs: 1000},
	}
	submitter, _ := NewSubmitter(builders, "")
	submitter.submitFn = func(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   errors.New("execution reverted: arithmetic overflow"),
		}
	}

	arb := newValidArb("arb-bug-001", 0.01, 5.0)

	// With dedup, each arb attempt counts as 1 revert regardless of builder
	// count, so we need 2 arb attempts to reach the threshold of 2.
	for i := 0; i < 2; i++ {
		submitted, err := processArb(context.Background(), arb, rm, bundler, submitter, nil,
			"0x0000000000000000000000000000000000000000", 0.5)
		if err != nil {
			t.Fatalf("processArb[%d]: %v", i, err)
		}
		if submitted {
			t.Fatal("expected submitted=false when all builders reject")
		}
	}

	if rm.State() != risk.StatePaused {
		t.Fatalf("expected Paused after 2 bug revert arbs, got %s", rm.State())
	}
}

func TestProcessArb_NonRevertErrors_NotCounted(t *testing.T) {
	t.Parallel()

	rc := risk.DefaultRiskConfig()
	rc.ConsecutiveRevertsPause = 1
	rm := risk.NewRiskManager(rc)

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)

	builders := []BuilderConfig{{Name: "b1", Enabled: true, TimeoutMs: 1000}}
	submitter, _ := NewSubmitter(builders, "")
	submitter.submitFn = func(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   errors.New("context deadline exceeded"),
		}
	}

	arb := newValidArb("arb-timeout-001", 0.01, 5.0)

	submitted, err := processArb(context.Background(), arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 0.5)
	if err != nil {
		t.Fatalf("processArb: %v", err)
	}
	if submitted {
		t.Fatal("expected submitted=false when all builders reject")
	}

	if rm.State() != risk.StateRunning {
		t.Fatalf("expected Running for non-revert errors, got %s", rm.State())
	}
}

func TestLooksLikeRevert(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		msg  string
		want bool
	}{
		{name: "explicit revert", msg: "execution reverted: reason", want: true},
		{name: "competitive nonce", msg: "nonce too low", want: true},
		{name: "competitive slippage", msg: "INSUFFICIENT_OUTPUT_AMOUNT", want: true},
		{name: "empty reason", msg: "", want: true},
		{name: "infra timeout", msg: "context deadline exceeded", want: false},
		{name: "transport", msg: "tls handshake timeout", want: false},
	}

	for _, tc := range tests {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			if got := looksLikeRevert(tc.msg); got != tc.want {
				t.Fatalf("looksLikeRevert(%q)=%v, want %v", tc.msg, got, tc.want)
			}
		})
	}
}

func TestSubmitter_CustomSubmitFn_IsUsed(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{{Name: "mock", Enabled: true, TimeoutMs: 1000}}
	s, _ := NewSubmitter(builders, "")
	s.submitFn = func(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
		return SubmissionResult{
			Builder:    builder.Name,
			Success:    true,
			BundleHash: "custom-hash",
		}
	}

	results := s.SubmitToAll(context.Background(), &Bundle{BlockNumber: 1})
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if !results[0].Success {
		t.Fatal("expected custom submit function result to be successful")
	}
	if results[0].BundleHash != "custom-hash" {
		t.Fatalf("expected custom-hash, got %q", results[0].BundleHash)
	}
	if results[0].Builder != "mock" {
		t.Fatalf("expected builder mock, got %q", results[0].Builder)
	}
}
