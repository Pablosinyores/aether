package main

import (
	"context"
	"math/big"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

func testBundle() *Bundle {
	chainID := big.NewInt(1)
	return &Bundle{
		Transactions: []*types.Transaction{
			types.NewTx(&types.DynamicFeeTx{ChainID: chainID, Nonce: 0}),
			types.NewTx(&types.DynamicFeeTx{ChainID: chainID, Nonce: 1}),
		},
		BlockNumber: 18000000,
	}
}

func TestSubmitToAll_AllEnabled(t *testing.T) {
	t.Parallel()

	builders := defaultBuilderConfigs()
	sub := NewSubmitter(builders)
	bundle := testBundle()

	results := sub.SubmitToAll(context.Background(), bundle)

	if len(results) != 4 {
		t.Fatalf("expected 4 results (all builders enabled), got %d", len(results))
	}

	for _, r := range results {
		if !r.Success {
			t.Errorf("builder %s: expected success, got failure: %v", r.Builder, r.Error)
		}
		if r.BundleHash == "" {
			t.Errorf("builder %s: expected non-empty bundle hash", r.Builder)
		}
		if r.Latency <= 0 {
			t.Errorf("builder %s: expected positive latency, got %v", r.Builder, r.Latency)
		}
	}
}

func TestSubmitToAll_SomeDisabled(t *testing.T) {
	t.Parallel()

	builders := defaultBuilderConfigs()
	builders[1].Enabled = false // titan
	builders[3].Enabled = false // rsync

	sub := NewSubmitter(builders)
	bundle := testBundle()

	results := sub.SubmitToAll(context.Background(), bundle)

	if len(results) != 2 {
		t.Fatalf("expected 2 results (2 builders disabled), got %d", len(results))
	}

	builderNames := make(map[string]bool)
	for _, r := range results {
		builderNames[r.Builder] = true
	}
	if builderNames["titan"] {
		t.Error("disabled builder 'titan' should not have submitted")
	}
	if builderNames["rsync"] {
		t.Error("disabled builder 'rsync' should not have submitted")
	}
	if !builderNames["flashbots"] {
		t.Error("enabled builder 'flashbots' should have submitted")
	}
	if !builderNames["beaver"] {
		t.Error("enabled builder 'beaver' should have submitted")
	}
}

func TestSubmitToAll_CancelledContext(t *testing.T) {
	t.Parallel()

	builders := defaultBuilderConfigs()
	sub := NewSubmitter(builders)
	bundle := testBundle()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	results := sub.SubmitToAll(ctx, bundle)

	for _, r := range results {
		if r.Builder == "" {
			t.Error("result has empty builder name")
		}
	}
	_ = results // No panic = pass
}

func TestSuccessCount(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name    string
		results []SubmissionResult
		want    int
	}{
		{"all success", []SubmissionResult{{Success: true}, {Success: true}, {Success: true}}, 3},
		{"all failure", []SubmissionResult{{Success: false}, {Success: false}}, 0},
		{"mixed", []SubmissionResult{{Success: true}, {Success: false}, {Success: true}, {Success: false}}, 2},
		{"empty", []SubmissionResult{}, 0},
		{"single success", []SubmissionResult{{Success: true}}, 1},
		{"single failure", []SubmissionResult{{Success: false}}, 0},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got := SuccessCount(tc.results)
			if got != tc.want {
				t.Errorf("SuccessCount: got %d, want %d", got, tc.want)
			}
		})
	}
}

func TestNewSubmitter(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "builder-a", URL: "https://a.example.com", Enabled: true, TimeoutMs: 1000},
		{Name: "builder-b", URL: "https://b.example.com", Enabled: false, TimeoutMs: 2000},
		{Name: "builder-c", URL: "https://c.example.com", Enabled: true, TimeoutMs: 3000},
	}

	sub := NewSubmitter(builders)

	if len(sub.builders) != 3 {
		t.Fatalf("expected 3 builders stored, got %d", len(sub.builders))
	}

	for i, b := range sub.builders {
		if b.Name != builders[i].Name {
			t.Errorf("builder[%d] name: got %s, want %s", i, b.Name, builders[i].Name)
		}
	}
}

// Unused import guard — common is used by testBundle indirectly through types.
var _ = common.Address{}
