// Package db is the trade-ledger access layer for the Go executor side.
// Ships the Ledger interface and a no-op default; a pgxpool-backed impl plus
// executor wiring on the bundle / inclusion paths land in a follow-up.
// Callers can depend on the API today without pulling pgx and without
// altering DATABASE_URL-unset behaviour.
//
// See migrations/0001_trade_ledger.sql for the target schema.
package db

import (
	"log/slog"
	"math/big"
	"sync"
	"time"

	"github.com/google/uuid"
)

// NewBundle is the insert payload for the bundles table. Field shapes mirror
// the SQL schema 1:1 so a pgx-backed implementation is a straight bind.
type NewBundle struct {
	BundleID    uuid.UUID
	ArbID       uuid.UUID
	SubmittedAt time.Time
	TargetBlock uint64
	SignedTxHex string
	GasUsed     *uint64
	IsShadow    bool
	Builders    []string
}

// NewInclusion is the upsert payload for one (bundle, builder) row of the
// inclusion_results table — written when a GetBundleStats poll resolves.
//
// LandedTxHash is *[32]byte (not []byte) so the type system enforces the
// 32-byte tx-hash invariant the schema and Rust counterpart (Option<B256>)
// expect; nil = NULL in the database.
//
// Error is *string (not string) so the SQL-NULL case round-trips faithfully:
// a non-nil *string produces a TEXT row, nil produces NULL. The previous
// string field silently wrote "" instead of NULL, breaking
// `WHERE error IS NULL` queries.
type NewInclusion struct {
	BundleID      uuid.UUID
	Builder       string
	Included      bool
	IncludedBlock *uint64
	LandedTxHash  *[32]byte
	Error         *string
	ResolvedAt    time.Time
}

// PnLDailyDelta is the upsert payload for pnl_daily roll-up.
type PnLDailyDelta struct {
	Day               time.Time
	RealizedProfitWei *big.Int
	GasSpentWei       *big.Int
	BundleCount       int64
	InclusionCount    int64
}

// Ledger is the persistence boundary for the executor side. All methods are
// infallible from the caller's perspective — a connection blip must never
// take the executor down. Implementations log and drop on failure.
type Ledger interface {
	InsertBundle(b NewBundle)
	InsertInclusion(i NewInclusion)
	UpsertPnLDaily(d PnLDailyDelta)
}

// NoopLedger is the default Ledger when DATABASE_URL is unset. Logs once on
// construction so operators can grep startup output and rule out persistence
// as the reason rows are missing.
type NoopLedger struct{}

var noopWarnOnce sync.Once

// NewNoopLedger returns a Ledger that discards every write.
func NewNoopLedger() Ledger {
	noopWarnOnce.Do(func() {
		slog.Info("DATABASE_URL unset — trade ledger disabled (no-op writes)",
			"component", "ledger")
	})
	return NoopLedger{}
}

func (NoopLedger) InsertBundle(NewBundle)         {}
func (NoopLedger) InsertInclusion(NewInclusion)   {}
func (NoopLedger) UpsertPnLDaily(PnLDailyDelta)   {}
