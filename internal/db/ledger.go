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

func (NoopLedger) InsertBundle(NewBundle)       {}
func (NoopLedger) InsertInclusion(NewInclusion) {}
func (NoopLedger) UpsertPnLDaily(PnLDailyDelta) {}

// ArbIDNamespace is the UUID namespace used to derive deterministic arb_id
// values from the engine's free-form `ArbOpportunity::id` strings. Hard-coded
// so the same opportunity id always maps to the same UUID across runs and
// machines, making `grep <id> logs/* | psql ... WHERE arb_id = …` work
// without a lookup table.
//
// MUST stay byte-identical to the Rust ARB_ID_NAMESPACE constant in
// crates/grpc-server/src/engine.rs — the join key is symmetric and a drift
// here silently breaks log↔DB correlation across the gRPC boundary.
var ArbIDNamespace = uuid.UUID{
	0x6e, 0xc6, 0xfd, 0x05, 0xb1, 0xc8, 0x4c, 0x4d,
	0x8d, 0x57, 0x4e, 0xc1, 0x77, 0xa2, 0x47, 0x6e,
}

// BundleIDNamespace is a separate UUID namespace for deriving deterministic
// bundle_id values from `(arb_id, target_block)`. Distinct from
// ArbIDNamespace so the two id spaces cannot accidentally collide.
var BundleIDNamespace = uuid.UUID{
	0x91, 0x32, 0x7d, 0xa1, 0x3f, 0xa4, 0x47, 0x9c,
	0x82, 0xb1, 0x1f, 0x6e, 0x9d, 0x47, 0x12, 0x07,
}

// ArbIDFromOppID derives a deterministic uuid.UUID (UUIDv5 / SHA-1) from the
// engine's `ArbOpportunity::id` string. Mirrors the Rust `arb_id_for_opp`.
func ArbIDFromOppID(oppID string) uuid.UUID {
	return uuid.NewSHA1(ArbIDNamespace, []byte(oppID))
}

// BundleIDFor derives a deterministic UUIDv5 bundle id from the arb id and
// target block. Same (arb, block) pair always produces the same bundle id so
// `INSERT ... ON CONFLICT (bundle_id) DO NOTHING` is naturally idempotent on
// resubmissions.
func BundleIDFor(arbID uuid.UUID, targetBlock uint64) uuid.UUID {
	buf := make([]byte, 0, 16+8)
	buf = append(buf, arbID[:]...)
	buf = append(buf,
		byte(targetBlock>>56), byte(targetBlock>>48), byte(targetBlock>>40), byte(targetBlock>>32),
		byte(targetBlock>>24), byte(targetBlock>>16), byte(targetBlock>>8), byte(targetBlock),
	)
	return uuid.NewSHA1(BundleIDNamespace, buf)
}
