package db

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/big"
	"sync"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
)

const (
	// ledgerChannelCapacity bounds the queue between the executor hot path and
	// the writer goroutine. Sized for ~5 s of bursty submissions at the
	// executor's peak rate before saturating; the drops counter is the alert
	// signal that Postgres is the bottleneck. Mirrors the Rust side's
	// LEDGER_CHANNEL_CAPACITY for behavioural symmetry.
	ledgerChannelCapacity = 1024

	// ledgerMaxInflight bounds simultaneous in-flight INSERTs on the writer
	// goroutine pool. Matches ledgerPoolSize so the pgx pool runs at capacity
	// without queueing on connection acquire.
	ledgerMaxInflight = 8

	// ledgerPoolSize sizes the underlying pgxpool. Kept identical to
	// ledgerMaxInflight so the two are tuned in lockstep.
	ledgerPoolSize = 8

	// ledgerConnectTimeout fails boot fast on misconfigured DATABASE_URL so
	// the executor degrades to NoopLedger via LedgerFromEnv instead of
	// stalling startup.
	ledgerConnectTimeout = 2 * time.Second

	// ledgerCloseDrainTimeout caps how long Close() will wait for in-flight
	// writes to complete before tearing down the pool. A wedged Postgres
	// must not be able to hang executor shutdown forever; rows still in the
	// channel at deadline are dropped (counted via existing drops metric
	// when Inc was already done; otherwise simply unrecorded).
	ledgerCloseDrainTimeout = 5 * time.Second
)

// PgLedger writes trade-ledger rows to Postgres via pgxpool. The hot path is
// non-blocking: every Ledger interface method enqueues a ledgerOp onto a
// bounded channel and returns immediately. A pool of writer goroutines, gated
// by a counting semaphore matching ledgerPoolSize, drains the channel and
// runs queries concurrently across the connection pool.
//
// Saturation drops the write and bumps `aether_ledger_drops_total{op}`
// instead of fanning out unbounded background goroutines while Postgres is
// slow.
type PgLedger struct {
	pool             *pgxpool.Pool
	ch               chan ledgerOp
	metrics          *LedgerMetrics
	wg               sync.WaitGroup
	dispatcherCancel context.CancelFunc
}

type ledgerOp struct {
	kind   string // "insert_bundle" | "insert_inclusion" | "upsert_pnl_daily"
	bundle *NewBundle
	incl   *NewInclusion
	pnl    *PnLDailyDelta
}

// NewPgLedger connects to Postgres, spawns the dispatcher goroutine, and
// returns a ready ledger. The dispatcher runs until ctx is cancelled or the
// channel is closed (typically at process shutdown).
func NewPgLedger(ctx context.Context, databaseURL string, metrics *LedgerMetrics) (*PgLedger, error) {
	cfg, err := pgxpool.ParseConfig(databaseURL)
	if err != nil {
		return nil, fmt.Errorf("parse DATABASE_URL: %w", err)
	}
	cfg.MaxConns = ledgerPoolSize
	cfg.ConnConfig.ConnectTimeout = ledgerConnectTimeout

	connectCtx, cancel := context.WithTimeout(ctx, ledgerConnectTimeout)
	defer cancel()
	pool, err := pgxpool.NewWithConfig(connectCtx, cfg)
	if err != nil {
		return nil, fmt.Errorf("connect pgxpool: %w", err)
	}
	if err := pool.Ping(connectCtx); err != nil {
		pool.Close()
		return nil, fmt.Errorf("ping postgres: %w", err)
	}

	// The writer dispatcher runs on a context **independent** of the caller's
	// ctx so that Close() can shut it down with its own bounded deadline
	// without racing against the caller's cancellation. Without this split a
	// caller cancelling ctx would also kill in-flight queries, defeating the
	// purpose of Close()'s drain.
	dispatcherCtx, dispatcherCancel := context.WithCancel(context.Background())
	l := &PgLedger{
		pool:             pool,
		ch:               make(chan ledgerOp, ledgerChannelCapacity),
		metrics:          metrics,
		dispatcherCancel: dispatcherCancel,
	}
	l.wg.Add(1)
	go l.dispatch(dispatcherCtx)

	slog.Info("PgLedger connected — trade ledger writes enabled",
		"component", "ledger",
		"channel_capacity", ledgerChannelCapacity,
		"pool_size", ledgerPoolSize,
		"max_inflight", ledgerMaxInflight)
	return l, nil
}

// Close drains in-flight writes and shuts the pool down. Bounded by
// ledgerCloseDrainTimeout so a wedged Postgres can never hang executor
// shutdown — rows still in flight at the deadline are abandoned and the
// pool tears down regardless. Safe to call from the executor's shutdown
// path.
func (l *PgLedger) Close() {
	close(l.ch)
	done := make(chan struct{})
	go func() {
		l.wg.Wait()
		close(done)
	}()
	select {
	case <-done:
		// Clean drain.
	case <-time.After(ledgerCloseDrainTimeout):
		slog.Warn("PgLedger Close() drain timed out; abandoning in-flight writes",
			"component", "ledger",
			"timeout", ledgerCloseDrainTimeout)
		l.dispatcherCancel()
		// Wait briefly for the cancelled dispatcher to return so Pool.Close
		// is not racing with goroutines still touching the pool.
		select {
		case <-done:
		case <-time.After(time.Second):
		}
	}
	l.pool.Close()
}

func (l *PgLedger) InsertBundle(b NewBundle) {
	l.enqueue(ledgerOp{kind: "insert_bundle", bundle: &b})
}

func (l *PgLedger) InsertInclusion(i NewInclusion) {
	l.enqueue(ledgerOp{kind: "insert_inclusion", incl: &i})
}

func (l *PgLedger) UpsertPnLDaily(d PnLDailyDelta) {
	l.enqueue(ledgerOp{kind: "upsert_pnl_daily", pnl: &d})
}

// enqueue is the common non-blocking enqueue path. Saturation drops the row
// and bumps `aether_ledger_drops_total{op}`.
//
// Bumps QueueDepth **before** the send so the dispatcher's matching Dec()
// always pairs against an Inc() that has already landed. The previous order
// (send, then Inc) allowed a brief negative-gauge window on dashboards
// during heavy enqueue/dequeue interleaving. On a failed send we revert the
// Inc so the gauge stays consistent with the actual channel depth.
func (l *PgLedger) enqueue(op ledgerOp) {
	l.metrics.QueueDepth.Inc()
	select {
	case l.ch <- op:
	default:
		l.metrics.QueueDepth.Dec()
		l.metrics.DropsTotal.WithLabelValues(op.kind).Inc()
		slog.Warn("ledger channel full — dropping row",
			"component", "ledger",
			"op", op.kind,
			"capacity", ledgerChannelCapacity)
	}
}

// dispatch dequeues ops and spawns up to ledgerMaxInflight concurrent writer
// goroutines per op. The semaphore matches the pgxpool size so the pool runs
// at capacity without acquire-queueing.
func (l *PgLedger) dispatch(ctx context.Context) {
	defer l.wg.Done()
	sem := make(chan struct{}, ledgerMaxInflight)
	var inflight sync.WaitGroup
	// `defer inflight.Wait()` guarantees every spawned writer drains before
	// the dispatcher returns, on every exit path — channel close, ctx
	// cancel, or a future error branch. Without this, a ctx-cancel return
	// left dangling writer goroutines holding pool connections after the
	// dispatcher had reported `wg.Done()`.
	defer inflight.Wait()
	defer slog.Info("PgLedger writer dispatcher exiting", "component", "ledger")
	for op := range l.ch {
		l.metrics.QueueDepth.Dec()
		select {
		case sem <- struct{}{}:
		case <-ctx.Done():
			return
		}
		inflight.Add(1)
		go func(op ledgerOp) {
			defer inflight.Done()
			defer func() { <-sem }()
			l.runOne(ctx, op)
		}(op)
	}
}

func (l *PgLedger) runOne(ctx context.Context, op ledgerOp) {
	start := time.Now()
	var err error
	switch op.kind {
	case "insert_bundle":
		err = l.insertBundleInner(ctx, op.bundle)
	case "insert_inclusion":
		err = l.insertInclusionInner(ctx, op.incl)
	case "upsert_pnl_daily":
		err = l.upsertPnLDailyInner(ctx, op.pnl)
	default:
		err = fmt.Errorf("unknown op %q", op.kind)
	}
	elapsedMs := float64(time.Since(start).Microseconds()) / 1000.0
	l.metrics.WriteLatencyMs.WithLabelValues(op.kind).Observe(elapsedMs)
	if err != nil {
		l.metrics.WritesTotal.WithLabelValues(op.kind, "err").Inc()
		slog.Warn("ledger write failed; row dropped",
			"component", "ledger",
			"op", op.kind,
			"err", err,
			"elapsed_ms", elapsedMs)
		return
	}
	l.metrics.WritesTotal.WithLabelValues(op.kind, "ok").Inc()
}

func (l *PgLedger) insertBundleInner(ctx context.Context, b *NewBundle) error {
	var gasUsed *int64
	if b.GasUsed != nil {
		v := int64(*b.GasUsed)
		gasUsed = &v
	}
	// bundles.builders is JSONB; pgx's default mapping for a Go []string is
	// the Postgres text[] OID, not JSONB. Marshalling to a []byte JSON array
	// here lands the right wire format ("[\"flashbots\", ...]") regardless of
	// whether b.Builders is nil or empty.
	buildersJSON, err := json.Marshal(b.Builders)
	if err != nil {
		return fmt.Errorf("marshal builders: %w", err)
	}
	_, err = l.pool.Exec(ctx, `
		INSERT INTO bundles (
			bundle_id, arb_id, submitted_at, target_block,
			signed_tx_hex, gas_used, is_shadow, builders
		) VALUES (
			$1, $2, $3, $4, $5, $6, $7, $8::jsonb
		)
		ON CONFLICT (bundle_id) DO NOTHING
	`,
		b.BundleID, b.ArbID, b.SubmittedAt, int64(b.TargetBlock),
		b.SignedTxHex, gasUsed, b.IsShadow, buildersJSON,
	)
	return err
}

func (l *PgLedger) insertInclusionInner(ctx context.Context, i *NewInclusion) error {
	var includedBlock *int64
	if i.IncludedBlock != nil {
		v := int64(*i.IncludedBlock)
		includedBlock = &v
	}
	var landed []byte
	if i.LandedTxHash != nil {
		// *[32]byte → []byte for pgx BYTEA bind. Length stays 32 by type.
		landed = i.LandedTxHash[:]
	}
	_, err := l.pool.Exec(ctx, `
		INSERT INTO inclusion_results (
			bundle_id, builder, included, included_block, landed_tx_hash, error, resolved_at
		) VALUES (
			$1, $2, $3, $4, $5, $6, $7
		)
		ON CONFLICT (bundle_id, builder) DO UPDATE SET
			included       = EXCLUDED.included,
			included_block = EXCLUDED.included_block,
			landed_tx_hash = EXCLUDED.landed_tx_hash,
			error          = EXCLUDED.error,
			resolved_at    = EXCLUDED.resolved_at
	`,
		i.BundleID, i.Builder, i.Included, includedBlock, landed, i.Error, i.ResolvedAt,
	)
	return err
}

func (l *PgLedger) upsertPnLDailyInner(ctx context.Context, d *PnLDailyDelta) error {
	profit := bigIntToString(d.RealizedProfitWei)
	gas := bigIntToString(d.GasSpentWei)
	day := d.Day.UTC().Format("2006-01-02")

	// Accumulate deltas atomically: the COALESCE + arithmetic in the UPDATE
	// branch lets multiple writers contribute to the same day without lost
	// updates. NUMERIC(78,0) preserves U256 economics losslessly.
	_, err := l.pool.Exec(ctx, `
		INSERT INTO pnl_daily (
			day, realized_profit_wei, gas_spent_wei, bundle_count, inclusion_count, updated_at
		) VALUES (
			$1::date, $2::numeric, $3::numeric, $4, $5, now()
		)
		ON CONFLICT (day) DO UPDATE SET
			realized_profit_wei = pnl_daily.realized_profit_wei + EXCLUDED.realized_profit_wei,
			gas_spent_wei       = pnl_daily.gas_spent_wei       + EXCLUDED.gas_spent_wei,
			bundle_count        = pnl_daily.bundle_count        + EXCLUDED.bundle_count,
			inclusion_count     = pnl_daily.inclusion_count     + EXCLUDED.inclusion_count,
			updated_at          = now()
	`,
		day, profit, gas, d.BundleCount, d.InclusionCount,
	)
	return err
}

// bigIntToString safely renders a possibly-nil *big.Int for the NUMERIC(78,0)
// bind. nil → "0" so a missing field never crashes the writer or sends NULL
// where the schema requires NOT NULL.
func bigIntToString(v *big.Int) string {
	if v == nil {
		return "0"
	}
	return v.String()
}

// LedgerFromEnv constructs a Ledger from `DATABASE_URL`. When unset / empty
// it returns a NoopLedger so dev / CI / shadow runs without Postgres keep
// working unchanged. A connect failure also degrades to NoopLedger and logs
// the reason, matching the Rust ledger_from_env contract.
func LedgerFromEnv(ctx context.Context, databaseURL string, metrics *LedgerMetrics) Ledger {
	if databaseURL == "" {
		return NewNoopLedger()
	}
	pg, err := NewPgLedger(ctx, databaseURL, metrics)
	if err != nil {
		slog.Error("PgLedger connect failed; falling back to NoopLedger",
			"component", "ledger", "err", err)
		// Return NoopLedger so the executor stays runnable; LedgerMetrics
		// just sits idle.
		return NewNoopLedger()
	}
	return pg
}
