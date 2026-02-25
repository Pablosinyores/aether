package main

import (
	"context"
	"testing"
)

func TestSubmitToAll_AllEnabled(t *testing.T) {
	t.Parallel()

	builders := defaultBuilderConfigs()
	sub := NewSubmitter(builders)

	// Create a minimal bundle for submission
	bundle := &Bundle{
		Transactions: []Transaction{{Nonce: 0}, {Nonce: 1}},
		BlockNumber:  18000000,
	}

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
	// Disable 2 of 4 builders
	builders[1].Enabled = false // titan
	builders[3].Enabled = false // rsync

	sub := NewSubmitter(builders)

	bundle := &Bundle{
		Transactions: []Transaction{{Nonce: 0}, {Nonce: 1}},
		BlockNumber:  18000000,
	}

	results := sub.SubmitToAll(context.Background(), bundle)

	if len(results) != 2 {
		t.Fatalf("expected 2 results (2 builders disabled), got %d", len(results))
	}

	// Verify only enabled builders ran
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

	bundle := &Bundle{
		Transactions: []Transaction{{Nonce: 0}},
		BlockNumber:  18000000,
	}

	// Cancel context immediately before submit
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	// Should not panic, results may be empty or contain errors
	results := sub.SubmitToAll(ctx, bundle)

	// With an already-cancelled context, builders may succeed (select default)
	// or fail (context done). Either way, should not panic.
	for _, r := range results {
		// Each result should have a builder name
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
		{
			name:    "all success",
			results: []SubmissionResult{{Success: true}, {Success: true}, {Success: true}},
			want:    3,
		},
		{
			name:    "all failure",
			results: []SubmissionResult{{Success: false}, {Success: false}},
			want:    0,
		},
		{
			name:    "mixed",
			results: []SubmissionResult{{Success: true}, {Success: false}, {Success: true}, {Success: false}},
			want:    2,
		},
		{
			name:    "empty",
			results: []SubmissionResult{},
			want:    0,
		},
		{
			name:    "single success",
			results: []SubmissionResult{{Success: true}},
			want:    1,
		},
		{
			name:    "single failure",
			results: []SubmissionResult{{Success: false}},
			want:    0,
		},
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
		if b.URL != builders[i].URL {
			t.Errorf("builder[%d] URL: got %s, want %s", i, b.URL, builders[i].URL)
		}
		if b.Enabled != builders[i].Enabled {
			t.Errorf("builder[%d] Enabled: got %v, want %v", i, b.Enabled, builders[i].Enabled)
		}
		if b.TimeoutMs != builders[i].TimeoutMs {
			t.Errorf("builder[%d] TimeoutMs: got %d, want %d", i, b.TimeoutMs, builders[i].TimeoutMs)
		}
	}
}
