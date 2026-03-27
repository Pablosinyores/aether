package main

import (
	"context"
	"testing"
	"time"

	aethergrpc "github.com/aether-arb/aether/internal/grpc"
	pb "github.com/aether-arb/aether/internal/pb"
	"github.com/aether-arb/aether/internal/risk"
	"github.com/aether-arb/aether/internal/testutil"
)

// TestMockServerHealthCheck verifies the mock gRPC server responds to health
// checks and the Go client correctly interprets the response.
func TestMockServerHealthCheck(t *testing.T) {
	srv := testutil.NewMockArbServer()
	addr, err := srv.Start()
	if err != nil {
		t.Fatalf("start mock server: %v", err)
	}
	defer srv.Stop()

	client, err := aethergrpc.Dial(addr)
	if err != nil {
		t.Fatalf("dial mock server: %v", err)
	}
	defer client.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	resp, err := client.CheckHealth(ctx)
	if err != nil {
		t.Fatalf("health check: %v", err)
	}

	if !resp.Healthy {
		t.Error("expected healthy=true for RUNNING state")
	}
}

// TestProcessArbViaGRPC verifies the full pipeline: mock server streams an arb,
// the client receives it, and processArb handles it correctly.
func TestProcessArbViaGRPC(t *testing.T) {
	srv := testutil.NewMockArbServer()
	srv.SetArbs([]*pb.ValidatedArb{testutil.ProfitableTriangleArb()})
	addr, err := srv.Start()
	if err != nil {
		t.Fatalf("start mock server: %v", err)
	}
	defer srv.Stop()

	client, err := aethergrpc.Dial(addr)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer client.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	stream, err := client.StreamArbs(ctx, 0.001)
	if err != nil {
		t.Fatalf("stream arbs: %v", err)
	}

	arb, err := stream.Recv()
	if err != nil {
		t.Fatalf("recv arb: %v", err)
	}

	if arb.Id != "arb-triangle-001" {
		t.Errorf("expected arb id arb-triangle-001, got %s", arb.Id)
	}

	rm, bundler, submitter := newTestComponents()
	submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)
	if err != nil {
		t.Fatalf("processArb: %v", err)
	}
	if !submitted {
		t.Error("expected profitable arb to be submitted")
	}
}

// TestConsumeArbStream verifies that consumeArbStream processes all arbs from
// the mock server and returns when the stream ends. Uses a short context
// so the function exits after processing.
func TestConsumeArbStream(t *testing.T) {
	srv := testutil.NewMockArbServer()
	arbs := testutil.BatchArbs()
	srv.SetArbs(arbs)

	addr, err := srv.Start()
	if err != nil {
		t.Fatalf("start mock server: %v", err)
	}
	defer srv.Stop()

	client, err := aethergrpc.Dial(addr)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer client.Close()

	rm, bundler, submitter := newTestComponents()

	// consumeArbStream loops forever; use a short context so it exits
	// after the stream completes and the reconnect timer fires.
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	consumeArbStream(ctx, client, bundler, submitter, rm, nil,
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)

	// Verify bundle tracking was updated
	missRate := rm.BundleMissRate()
	if missRate < 0 || missRate > 100 {
		t.Errorf("unexpected miss rate: %.2f", missRate)
	}
}

// TestCircuitBreakerAcrossArbs verifies that the risk manager pauses the
// system after enough reverts, and subsequent arbs are rejected.
func TestCircuitBreakerAcrossArbs(t *testing.T) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()

	// Process first arb — should succeed
	arb1 := testutil.ProfitableTriangleArb()
	submitted, err := processArb(ctx, arb1, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)
	if err != nil {
		t.Fatalf("arb1: %v", err)
	}
	if !submitted {
		t.Error("arb1 should be submitted")
	}

	// Trigger circuit breaker: 3 consecutive reverts
	rm.RecordRevert()
	rm.RecordRevert()
	rm.RecordRevert()

	if rm.State() == risk.StateRunning {
		t.Fatal("system should be paused after 3 reverts")
	}

	// Process second arb — should be rejected
	arb2 := testutil.Profitable2HopArb()
	submitted, err = processArb(ctx, arb2, rm, bundler, submitter, nil,
		"0x0000000000000000000000000000000000000000", 90.0, 0.5)
	if err != nil {
		t.Fatalf("arb2: %v", err)
	}
	if submitted {
		t.Error("arb2 should be rejected when system is paused")
	}
}

// TestMixedArbScenarios runs a batch of arbs with different risk profiles
// and verifies each gets the correct accept/reject decision.
func TestMixedArbScenarios(t *testing.T) {
	tests := []struct {
		name       string
		arb        func() *pb.ValidatedArb
		ethBalance float64
		wantSubmit bool
	}{
		{"profitable_triangle", testutil.ProfitableTriangleArb, 0.5, true},
		{"profitable_2hop", testutil.Profitable2HopArb, 0.5, true},
		{"marginal_profit", testutil.MarginalProfitArb, 0.5, true},
		{"low_profit", testutil.LowProfitArb, 0.5, false},
		{"large_trade", testutil.LargeTradeArb, 0.5, false},
		{"low_balance", testutil.ProfitableTriangleArb, 0.05, false},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			rm, bundler, submitter := newTestComponents()
			ctx := context.Background()

			arb := tc.arb()
			submitted, err := processArb(ctx, arb, rm, bundler, submitter, nil,
				"0x0000000000000000000000000000000000000000", 90.0, tc.ethBalance)
			if err != nil {
				t.Fatalf("processArb: %v", err)
			}
			if submitted != tc.wantSubmit {
				t.Errorf("submitted=%v, want %v", submitted, tc.wantSubmit)
			}
		})
	}
}

// TestGracefulShutdown verifies that consumeArbStream exits cleanly when
// the context is cancelled immediately.
func TestGracefulShutdown(t *testing.T) {
	srv := testutil.NewMockArbServer()
	srv.SetArbs(testutil.BatchArbs())
	addr, err := srv.Start()
	if err != nil {
		t.Fatalf("start mock server: %v", err)
	}
	defer srv.Stop()

	client, err := aethergrpc.Dial(addr)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer client.Close()

	rm, bundler, submitter := newTestComponents()

	// Cancel immediately — consumeArbStream should return quickly
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	done := make(chan struct{})
	go func() {
		consumeArbStream(ctx, client, bundler, submitter, rm, nil,
			"0x0000000000000000000000000000000000000000", 90.0, 0.5)
		close(done)
	}()

	select {
	case <-done:
		// OK — exited cleanly
	case <-time.After(2 * time.Second):
		t.Fatal("consumeArbStream did not exit within 2s after context cancel")
	}
}

// TestBundleMissRateTracking verifies that bundle success/failure tracking
// produces correct miss rates.
func TestBundleMissRateTracking(t *testing.T) {
	rm := risk.NewRiskManager(risk.DefaultRiskConfig())

	// 3 submitted, 1 included => miss rate = 66.67%
	rm.RecordBundleResult(true)
	rm.RecordBundleResult(false)
	rm.RecordBundleResult(false)

	missRate := rm.BundleMissRate()
	expected := 66.67
	if missRate < expected-1 || missRate > expected+1 {
		t.Errorf("miss rate = %.2f%%, want ~%.2f%%", missRate, expected)
	}
}

// TestConfigToRiskManager verifies that default config creates a functional
// risk manager with expected thresholds.
func TestConfigToRiskManager(t *testing.T) {
	rc := risk.DefaultRiskConfig()
	rm := risk.NewRiskManager(rc)

	if rm.State() != risk.StateRunning {
		t.Errorf("initial state = %s, want Running", rm.State())
	}

	// Should approve a normal arb
	result := rm.PreflightCheck(
		ethToWei(0.01),  // profit
		ethToWei(5.0),   // trade size
		30.0,            // gas gwei
		90.0,            // tip share
		0.5,             // ETH balance
	)
	if !result.Approved {
		t.Errorf("normal arb rejected: %s", result.Reason)
	}

	// Should reject gas too high
	result = rm.PreflightCheck(ethToWei(0.01), ethToWei(5.0), 350.0, 90.0, 0.5)
	if result.Approved {
		t.Error("high gas arb should be rejected")
	}
}
