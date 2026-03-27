package main

import (
	"context"
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
func newTestComponents() (*risk.RiskManager, *BundleConstructor, *Submitter) {
	rm := risk.NewRiskManager(risk.DefaultRiskConfig())
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 90.0, 1)
	submitter := NewSubmitter(defaultBuilderConfigs())
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
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

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
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

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
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

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
		"0x0000000000000000000000000000000000000000", 90.0, 0.05)

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
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

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

	// Trigger circuit breaker: 3 consecutive reverts within the window pauses the system
	rm.RecordRevert()
	rm.RecordRevert()
	rm.RecordRevert()

	// Verify the system is no longer in Running state
	if rm.State() == risk.StateRunning {
		t.Fatal("expected system to be paused after 3 reverts, still running")
	}

	arb := newValidArb("arb-paused-001", 0.01, 5.0)

	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

	if err != nil {
		t.Fatalf("expected no error, got: %v", err)
	}
	if submitted {
		t.Fatal("expected arb to be rejected (system paused), got submitted=true")
	}
}
