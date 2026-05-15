// Mempool reconciliation persistence layer.
//
// Separate from the trade-ledger PgLedger by design: distinct DSN
// (MEMPOOL_LEDGER_DSN), distinct pgx pool. The reconciler binary is opt-in
// (the executor and engine don't link this code path) so its DB
// configuration must not collide with the trade-ledger's DATABASE_URL.
//
// API surface:
//   - LookupPredictionByTxHash: synchronous single-row read on the
//     `pending_tx_hash` unique index. Called once per landed block tx; the
//     reconciler hot path stays simple by awaiting this result inline.
//   - InsertReconciliation: fire-and-forget through the existing PgLedger
//     pattern — bounded channel, drop-on-saturation, separate writer
//     goroutine.
//   - MarkStaleAsDropped: batch SQL that inserts `outcome='dropped'` rows
//     for every prediction past its 12-block grace window without a
//     reconciliation row.
//
// See migrations/0004_mempool_reconciliation.sql for the schema.

package db

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"time"

	"github.com/google/uuid"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

const (
	// reconChannelCapacity bounds the writer queue. Sized smaller than the
	// trade-ledger ledger's 1024 because reconciliation throughput equals
	// "predictions that hit the same block" — at peak ~10/block on busy
	// mainnet — and a 256-deep buffer gives ~25 blocks of headroom before
	// drops at peak.
	reconChannelCapacity = 256

	// reconPoolSize sizes the underlying pgxpool. Smaller than the trade
	// ledger (4 vs 8) because the reconciler's write rate is bounded by
	// the per-block batch instead of the per-arb stream.
	reconPoolSize = 4

	// reconConnectTimeout fails boot fast on misconfigured
	// MEMPOOL_LEDGER_DSN so the binary degrades cleanly to "metric-only,
	// no DB writes" instead of stalling startup.
	reconConnectTimeout = 2 * time.Second

	// reconCloseDrainTimeout caps how long Close() waits for in-flight
	// writes. Mirrors PgLedger's policy.
	reconCloseDrainTimeout = 5 * time.Second

	// StaleConfirmationWindow is the number of blocks the reconciler waits
	// past a prediction's predicted_target_block before declaring it
	// dropped. 12 ≈ Flashbots' empirical "tx never landed" heuristic
	// (one epoch on post-merge Ethereum); shorter windows surface false
	// drops on busy fee markets where pendings wait several blocks for
	// inclusion.
	StaleConfirmationWindow = 12
)

// PendingPrediction is the subset of `mempool_predictions` columns the
// reconciler needs to score an outcome. Returned by LookupPredictionByTxHash.
// Kept tight on purpose — adding columns means widening the read path's hot
// SELECT and is gated by an actual reconciler-side use.
type PendingPrediction struct {
	PredictionID         uuid.UUID
	Protocol             string
	PoolAddress          *[20]byte // nil when registry miss recorded NULL
	PredictedTargetBlock uint64
}

// NewReconciliation is the insert payload for `mempool_reconciliation`. The
// outcome enum is constrained at the SQL CHECK level, but mirrored here as
// public constants so callers can switch on a stable identifier instead of
// re-typing the literal each time.
type NewReconciliation struct {
	PredictionID       uuid.UUID
	ResolutionTs       time.Time
	Outcome            string
	ActualTargetBlock  *uint64
	ActualTxIndex      *int
	BlockDelta         *int
	OrderingCorrect    *bool
	PoolPathCorrect    *bool
	ReplacedByTxHash   *[32]byte
	FailureReason      *string
}

const (
	OutcomeConfirmed    = "confirmed"
	OutcomeDropped      = "dropped"
	OutcomeReplaced     = "replaced"
	OutcomeStillPending = "still_pending"
)

// PgMempoolReconciliation owns the pgxpool and writer goroutine pair.
// Functionally a sibling of PgLedger; intentionally not collapsed into the
// same type because (a) the two run in separate process address spaces
// (engine vs reconciler binary), and (b) collapsing would force the engine
// to pull a pgx reconciliation-table runtime even when the reconciler is
// not in use.
type PgMempoolReconciliation struct {
	pool             *pgxpool.Pool
	ch               chan NewReconciliation
	metrics          *MempoolReconciliationMetrics
	wg               sync.WaitGroup
	dispatcherCancel context.CancelFunc
}

// NewPgMempoolReconciliation connects to Postgres and spawns the dispatcher.
// Mirrors NewPgLedger's lifecycle so a future joint shutdown coordinator
// can call Close on both without special-casing either.
func NewPgMempoolReconciliation(
	ctx context.Context,
	databaseURL string,
	metrics *MempoolReconciliationMetrics,
) (*PgMempoolReconciliation, error) {
	cfg, err := pgxpool.ParseConfig(databaseURL)
	if err != nil {
		return nil, fmt.Errorf("parse MEMPOOL_LEDGER_DSN: %w", err)
	}
	cfg.MaxConns = reconPoolSize
	cfg.ConnConfig.ConnectTimeout = reconConnectTimeout

	connectCtx, cancel := context.WithTimeout(ctx, reconConnectTimeout)
	defer cancel()
	pool, err := pgxpool.NewWithConfig(connectCtx, cfg)
	if err != nil {
		return nil, fmt.Errorf("connect mempool pgxpool: %w", err)
	}
	if err := pool.Ping(connectCtx); err != nil {
		pool.Close()
		return nil, fmt.Errorf("ping mempool postgres: %w", err)
	}

	dispatcherCtx, dispatcherCancel := context.WithCancel(context.Background())
	r := &PgMempoolReconciliation{
		pool:             pool,
		ch:               make(chan NewReconciliation, reconChannelCapacity),
		metrics:          metrics,
		dispatcherCancel: dispatcherCancel,
	}
	r.wg.Add(1)
	go r.dispatch(dispatcherCtx)

	slog.Info("PgMempoolReconciliation connected — reconciliation writes enabled",
		"component", "reconciler",
		"channel_capacity", reconChannelCapacity,
		"pool_size", reconPoolSize)
	return r, nil
}

// Close drains in-flight writes and shuts the pool down. Same bounded
// drain policy as PgLedger.Close: a wedged Postgres cannot hang the
// reconciler shutdown forever.
func (r *PgMempoolReconciliation) Close() {
	close(r.ch)
	done := make(chan struct{})
	go func() {
		r.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
		// Clean drain.
	case <-time.After(reconCloseDrainTimeout):
		slog.Warn("PgMempoolReconciliation Close() drain timed out",
			"component", "reconciler",
			"timeout", reconCloseDrainTimeout)
		r.dispatcherCancel()
		select {
		case <-done:
		case <-time.After(time.Second):
		}
	}
	r.pool.Close()
}

// LookupPredictionByTxHash returns the prediction row keyed by
// `pending_tx_hash`. The second return is false (with nil error) when no
// row matches — i.e. the tx hash was never a prediction. Callers MUST
// distinguish "missing" from "error" because the per-block hot path treats
// the two cases differently: missing is the expected dominant case (most
// tx hashes are not predictions) and is silent.
func (r *PgMempoolReconciliation) LookupPredictionByTxHash(
	ctx context.Context,
	txHash [32]byte,
) (PendingPrediction, bool, error) {
	row := r.pool.QueryRow(ctx, `
		SELECT prediction_id, protocol, pool_address, predicted_target_block
		FROM mempool_predictions
		WHERE pending_tx_hash = $1
	`, txHash[:])

	var (
		pred       PendingPrediction
		poolBytes  []byte
		targetBlk  int64
	)
	if err := row.Scan(&pred.PredictionID, &pred.Protocol, &poolBytes, &targetBlk); err != nil {
		if errors.Is(err, pgx.ErrNoRows) {
			return PendingPrediction{}, false, nil
		}
		return PendingPrediction{}, false, fmt.Errorf("lookup prediction: %w", err)
	}
	pred.PredictedTargetBlock = uint64(targetBlk)
	if len(poolBytes) == 20 {
		var arr [20]byte
		copy(arr[:], poolBytes)
		pred.PoolAddress = &arr
	}
	return pred, true, nil
}

// InsertReconciliation enqueues a write. Fire-and-forget. Saturation drops
// the row and bumps `aether_mempool_reconciler_drops_total`.
func (r *PgMempoolReconciliation) InsertReconciliation(rec NewReconciliation) {
	r.metrics.QueueDepth.Inc()
	select {
	case r.ch <- rec:
	default:
		r.metrics.QueueDepth.Dec()
		r.metrics.DropsTotal.Inc()
		slog.Warn("reconciliation channel full — dropping row",
			"component", "reconciler",
			"capacity", reconChannelCapacity,
			"outcome", rec.Outcome)
	}
}

// MarkStaleAsDropped runs the periodic sweep that closes the loop for
// predictions where the 12-block window elapsed without a matching landed
// tx. Returns the number of rows inserted.
//
// The query is a single INSERT … SELECT with NOT EXISTS guard so multiple
// concurrent reconcilers (e.g. blue/green deploy overlap) never produce
// duplicate dropped rows — the prediction_id PK on
// mempool_reconciliation makes the second writer's row a no-op via
// ON CONFLICT DO NOTHING.
func (r *PgMempoolReconciliation) MarkStaleAsDropped(
	ctx context.Context,
	currentHead uint64,
) (int64, error) {
	cutoff := int64(currentHead) - int64(StaleConfirmationWindow)
	if cutoff < 0 {
		return 0, nil
	}
	// The failure_reason is built in Go (rather than via SQL concat) so
	// pgx encodes a single $1 as TEXT and a single $2 as BIGINT — the
	// concat form pgx<->driver-encoder did not recognise the int64
	// argument when the column context was TEXT.
	failureReason := fmt.Sprintf("12-block window elapsed (head=%d)", currentHead)
	tag, err := r.pool.Exec(ctx, `
		INSERT INTO mempool_reconciliation (
			prediction_id, resolution_ts, outcome, failure_reason
		)
		SELECT p.prediction_id, now(), 'dropped', $1
		FROM mempool_predictions p
		WHERE p.predicted_target_block <= $2
		  AND NOT EXISTS (
		      SELECT 1 FROM mempool_reconciliation r
		      WHERE r.prediction_id = p.prediction_id
		  )
		ON CONFLICT (prediction_id) DO NOTHING
	`, failureReason, cutoff)
	if err != nil {
		return 0, fmt.Errorf("mark stale dropped: %w", err)
	}
	rows := tag.RowsAffected()
	if rows > 0 {
		r.metrics.ReconciledTotal.WithLabelValues(OutcomeDropped).Add(float64(rows))
	}
	return rows, nil
}

func (r *PgMempoolReconciliation) dispatch(ctx context.Context) {
	defer r.wg.Done()
	for rec := range r.ch {
		r.metrics.QueueDepth.Dec()
		if ctx.Err() != nil {
			// Drain remaining ops to keep the channel reader live; the
			// dispatcherCancel path is reserved for the wedged-PG case.
			continue
		}
		timer := time.Now()
		err := r.insertReconciliationInner(ctx, &rec)
		elapsedMs := float64(time.Since(timer).Microseconds()) / 1000.0
		result := "ok"
		if err != nil {
			result = "err"
			slog.Warn("reconciliation insert failed; row dropped",
				"component", "reconciler",
				"outcome", rec.Outcome,
				"prediction_id", rec.PredictionID,
				"error", err.Error())
		} else {
			r.metrics.ReconciledTotal.WithLabelValues(rec.Outcome).Inc()
		}
		r.metrics.WriteLatencyMs.WithLabelValues(result).Observe(elapsedMs)
	}
	slog.Info("PgMempoolReconciliation dispatcher exiting", "component", "reconciler")
}

func (r *PgMempoolReconciliation) insertReconciliationInner(
	ctx context.Context,
	rec *NewReconciliation,
) error {
	var (
		actualBlock *int64
		blockDelta  *int
		replaced    []byte
	)
	if rec.ActualTargetBlock != nil {
		v := int64(*rec.ActualTargetBlock)
		actualBlock = &v
	}
	if rec.BlockDelta != nil {
		blockDelta = rec.BlockDelta
	}
	if rec.ReplacedByTxHash != nil {
		replaced = rec.ReplacedByTxHash[:]
	}
	_, err := r.pool.Exec(ctx, `
		INSERT INTO mempool_reconciliation (
			prediction_id, resolution_ts, outcome,
			actual_target_block, actual_tx_index, block_delta,
			ordering_correct, pool_path_correct,
			replaced_by_tx_hash, failure_reason
		) VALUES (
			$1, $2, $3,
			$4, $5, $6,
			$7, $8,
			$9, $10
		)
		ON CONFLICT (prediction_id) DO NOTHING
	`,
		rec.PredictionID, rec.ResolutionTs, rec.Outcome,
		actualBlock, rec.ActualTxIndex, blockDelta,
		rec.OrderingCorrect, rec.PoolPathCorrect,
		replaced, rec.FailureReason,
	)
	return err
}
