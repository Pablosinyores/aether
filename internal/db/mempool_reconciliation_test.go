package db

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/prometheus/client_golang/prometheus"
)

// TestOutcomeConstantsMatchSchema pins the outcome string constants to the
// CHECK constraint in `migrations/0004_mempool_reconciliation.sql`. A drift
// here (e.g. renaming "confirmed" to "landed") would make every write fail
// with a CHECK violation; this test catches it without touching the DB.
func TestOutcomeConstantsMatchSchema(t *testing.T) {
	wantConfirmed := "confirmed"
	wantDropped := "dropped"
	wantReplaced := "replaced"
	wantStillPending := "still_pending"
	if OutcomeConfirmed != wantConfirmed {
		t.Fatalf("OutcomeConfirmed = %q, want %q (migration 0004 CHECK constraint)",
			OutcomeConfirmed, wantConfirmed)
	}
	if OutcomeDropped != wantDropped {
		t.Fatalf("OutcomeDropped = %q, want %q", OutcomeDropped, wantDropped)
	}
	if OutcomeReplaced != wantReplaced {
		t.Fatalf("OutcomeReplaced = %q, want %q", OutcomeReplaced, wantReplaced)
	}
	if OutcomeStillPending != wantStillPending {
		t.Fatalf("OutcomeStillPending = %q, want %q", OutcomeStillPending, wantStillPending)
	}
}

// TestStaleConfirmationWindow pins the dropped-grace constant. Lowering it
// would surface false drops on busy fee markets; raising it would delay
// the "dropped" outcome past usefulness. A drift bug is far more likely
// than an intentional change, so the test fails noisily when the constant
// moves.
func TestStaleConfirmationWindow(t *testing.T) {
	if StaleConfirmationWindow != 12 {
		t.Fatalf("StaleConfirmationWindow = %d, want 12 (Flashbots-aligned heuristic)",
			StaleConfirmationWindow)
	}
}

// TestMempoolReconciliationMetricsRegister exercises every path on the
// metrics struct so a typo in a Name / Help / label triggers in CI.
func TestMempoolReconciliationMetricsRegister(t *testing.T) {
	reg := prometheus.NewRegistry()
	m := NewMempoolReconciliationMetrics(reg)

	m.ReconciledTotal.WithLabelValues(OutcomeConfirmed).Inc()
	m.ReconciledTotal.WithLabelValues(OutcomeDropped).Add(3)
	m.DropsTotal.Inc()
	m.QueueDepth.Set(7)
	m.WriteLatencyMs.WithLabelValues("ok").Observe(1.5)

	families, err := reg.Gather()
	if err != nil {
		t.Fatalf("registry.Gather: %v", err)
	}
	names := map[string]bool{}
	for _, f := range families {
		names[f.GetName()] = true
	}
	for _, required := range []string{
		"aether_mempool_reconciled_total",
		"aether_mempool_reconciler_drops_total",
		"aether_mempool_reconciler_queue_depth",
		"aether_mempool_reconciler_write_latency_ms",
	} {
		if !names[required] {
			t.Fatalf("missing metric family %s", required)
		}
	}
}

// TestNewReconciliationDefaults documents the zero-value behaviour for the
// optional pointer fields. Without these defaults, a caller who forgets to
// populate ActualTargetBlock for a `dropped` outcome would still produce
// a row whose NULLs match the schema's expectations — this test pins that.
func TestNewReconciliationDefaults(t *testing.T) {
	rec := NewReconciliation{
		PredictionID: uuid.New(),
		ResolutionTs: time.Now().UTC(),
		Outcome:      OutcomeDropped,
	}
	if rec.ActualTargetBlock != nil {
		t.Errorf("ActualTargetBlock should default to nil; got %v", rec.ActualTargetBlock)
	}
	if rec.ActualTxIndex != nil {
		t.Errorf("ActualTxIndex should default to nil; got %v", rec.ActualTxIndex)
	}
	if rec.PoolPathCorrect != nil {
		t.Errorf("PoolPathCorrect should default to nil; got %v", rec.PoolPathCorrect)
	}
}

// ------- Integration test, gated by MEMPOOL_LEDGER_TEST_DSN -------

// TestPgMempoolReconciliationRoundTrip exercises the writer against a live
// Postgres reachable via MEMPOOL_LEDGER_TEST_DSN. Skipped when the env var
// is unset so `go test ./...` works on machines without Postgres.
//
// Pre-condition: migrations 0001 → 0004 applied. The test inserts one
// prediction via raw SQL (mirroring what the Rust writer would emit),
// invokes LookupPredictionByTxHash + InsertReconciliation, then verifies
// the join. Cleanup truncates the rows it added; it does not touch
// pre-existing data.
func TestPgMempoolReconciliationRoundTrip(t *testing.T) {
	dsn := os.Getenv("MEMPOOL_LEDGER_TEST_DSN")
	if dsn == "" {
		t.Skip("MEMPOOL_LEDGER_TEST_DSN unset — skipping live PG integration test")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	reg := prometheus.NewRegistry()
	metrics := NewMempoolReconciliationMetrics(reg)
	recon, err := NewPgMempoolReconciliation(ctx, dsn, metrics)
	if err != nil {
		t.Fatalf("NewPgMempoolReconciliation: %v", err)
	}
	defer recon.Close()

	// Insert a prediction we own. The tx hash is derived from two fresh
	// UUIDs so re-running the test against the same DB does not collide
	// with an orphan row from a previous failed run (the `pending_tx_hash`
	// UNIQUE index would otherwise short-circuit our INSERT and we'd
	// read back the stale prediction_id).
	predID := uuid.New()
	var txHash [32]byte
	low := uuid.New()
	high := uuid.New()
	copy(txHash[0:16], low[:])
	copy(txHash[16:32], high[:])
	router := [20]byte{0x7a, 0x25, 0x0d, 0x56, 0x30, 0xb4, 0xcf, 0x53, 0x97, 0x39,
		0xdf, 0x2c, 0x5d, 0xac, 0xb4, 0xc6, 0x59, 0xf2, 0x48, 0x8d}
	tokenIn := [20]byte{0xc0, 0x2a, 0xaa, 0x39, 0xb2, 0x23, 0xfe, 0x8d, 0x0a, 0x0e,
		0x5c, 0x4f, 0x27, 0xea, 0xd9, 0x08, 0x3c, 0x75, 0x6c, 0xc2}
	tokenOut := [20]byte{0xa0, 0xb8, 0x69, 0x91, 0xc6, 0x21, 0x8b, 0x36, 0xc1, 0xd1,
		0x9d, 0x4a, 0x2e, 0x9e, 0xb0, 0xce, 0x36, 0x06, 0xeb, 0x48}
	poolAddr := [20]byte{0xB4, 0xe1, 0x6d, 0x01, 0x68, 0xe5, 0x2d, 0x35, 0xCa, 0xCD,
		0x2c, 0x61, 0x85, 0xb4, 0x42, 0x81, 0xEc, 0x28, 0xC9, 0xDc}

	_, err = recon.pool.Exec(ctx, `
		INSERT INTO mempool_predictions (
			prediction_id, decoded_at, pending_tx_hash, router_address, protocol,
			token_in, token_out, amount_in, pool_address,
			predicted_target_block, predicted_post_state
		) VALUES (
			$1, now(), $2, $3, 'uni_v2', $4, $5, 1000000, $6,
			100, '{"kind":"v2","reserve_in":1000,"reserve_out":2000}'::jsonb
		)
		ON CONFLICT (pending_tx_hash) DO NOTHING
	`, predID, txHash[:], router[:], tokenIn[:], tokenOut[:], poolAddr[:])
	if err != nil {
		t.Fatalf("seed prediction: %v", err)
	}
	t.Cleanup(func() {
		// Cascade clears both rows when we delete the prediction.
		_, _ = recon.pool.Exec(context.Background(),
			`DELETE FROM mempool_predictions WHERE prediction_id = $1`, predID)
	})

	// Read it back through the public API.
	pred, found, err := recon.LookupPredictionByTxHash(ctx, txHash)
	if err != nil {
		t.Fatalf("LookupPredictionByTxHash: %v", err)
	}
	if !found {
		t.Fatalf("expected prediction to be found by tx hash")
	}
	if pred.PredictionID != predID {
		t.Fatalf("PredictionID = %v, want %v", pred.PredictionID, predID)
	}
	if pred.PoolAddress == nil || *pred.PoolAddress != poolAddr {
		t.Fatalf("PoolAddress mismatch: %v vs %v", pred.PoolAddress, poolAddr)
	}
	if pred.PredictedTargetBlock != 100 {
		t.Fatalf("PredictedTargetBlock = %d, want 100", pred.PredictedTargetBlock)
	}

	// Write a confirmed reconciliation row. Use Close() at end to drain.
	actualBlock := uint64(101)
	actualIdx := 5
	blockDelta := 1
	poolPathCorrect := true
	recon.InsertReconciliation(NewReconciliation{
		PredictionID:      predID,
		ResolutionTs:      time.Now().UTC(),
		Outcome:           OutcomeConfirmed,
		ActualTargetBlock: &actualBlock,
		ActualTxIndex:     &actualIdx,
		BlockDelta:        &blockDelta,
		PoolPathCorrect:   &poolPathCorrect,
	})

	// Allow the dispatcher to drain. Poll the row up to 2 s so we don't
	// race on a slow CI Postgres.
	deadline := time.Now().Add(2 * time.Second)
	var landed bool
	for time.Now().Before(deadline) {
		var outcome string
		err := recon.pool.QueryRow(ctx,
			`SELECT outcome FROM mempool_reconciliation WHERE prediction_id = $1`,
			predID,
		).Scan(&outcome)
		if err == nil && outcome == OutcomeConfirmed {
			landed = true
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if !landed {
		t.Fatalf("reconciliation row did not land within 2s")
	}
}

// TestPgMempoolReconciliationLookupMiss verifies the (false, nil err) case
// for a hash that is not in `mempool_predictions`. Important because the
// per-block loop treats `(false, nil)` as "tx hash is not a prediction"
// (the dominant case) without logging.
func TestPgMempoolReconciliationLookupMiss(t *testing.T) {
	dsn := os.Getenv("MEMPOOL_LEDGER_TEST_DSN")
	if dsn == "" {
		t.Skip("MEMPOOL_LEDGER_TEST_DSN unset — skipping live PG integration test")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	reg := prometheus.NewRegistry()
	metrics := NewMempoolReconciliationMetrics(reg)
	recon, err := NewPgMempoolReconciliation(ctx, dsn, metrics)
	if err != nil {
		t.Fatalf("NewPgMempoolReconciliation: %v", err)
	}
	defer recon.Close()

	// A hash that no prediction ever uses (high bytes set).
	missing := [32]byte{0xff, 0xff, 0xff, 0xff}
	_, found, err := recon.LookupPredictionByTxHash(ctx, missing)
	if err != nil {
		t.Fatalf("LookupPredictionByTxHash error on miss: %v", err)
	}
	if found {
		t.Fatalf("found=true for non-existent hash")
	}
}
