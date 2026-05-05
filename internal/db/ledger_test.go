package db

import (
	"math/big"
	"testing"
	"time"

	"github.com/google/uuid"
)

func TestArbIDFromOppIDDeterministic(t *testing.T) {
	a := ArbIDFromOppID("arb-2026-05-05-001")
	b := ArbIDFromOppID("arb-2026-05-05-001")
	if a != b {
		t.Fatalf("ArbIDFromOppID not deterministic: %s != %s", a, b)
	}
	c := ArbIDFromOppID("arb-2026-05-05-002")
	if a == c {
		t.Fatalf("distinct opp ids must yield distinct UUIDs (got %s for both)", a)
	}
}

func TestArbIDNamespaceMatchesRust(t *testing.T) {
	want := uuid.UUID{
		0x6e, 0xc6, 0xfd, 0x05, 0xb1, 0xc8, 0x4c, 0x4d,
		0x8d, 0x57, 0x4e, 0xc1, 0x77, 0xa2, 0x47, 0x6e,
	}
	if ArbIDNamespace != want {
		t.Fatalf("ArbIDNamespace drift: %v != %v\n"+
			"MUST stay byte-identical to ARB_ID_NAMESPACE in "+
			"crates/grpc-server/src/engine.rs — the join key is symmetric.",
			ArbIDNamespace, want)
	}
}

func TestBundleIDForChangesWithBlock(t *testing.T) {
	arbID := ArbIDFromOppID("arb-x")
	a := BundleIDFor(arbID, 100)
	b := BundleIDFor(arbID, 100)
	if a != b {
		t.Fatalf("BundleIDFor not deterministic for same (arb, block): %s != %s", a, b)
	}
	c := BundleIDFor(arbID, 101)
	if a == c {
		t.Fatalf("BundleIDFor must vary by block (got %s for both)", a)
	}
}

func TestNoopLedgerAcceptsAllWrites(t *testing.T) {
	l := NewNoopLedger()
	l.InsertBundle(NewBundle{
		BundleID:    uuid.New(),
		ArbID:       uuid.New(),
		SubmittedAt: time.Now(),
		TargetBlock: 1,
		SignedTxHex: "0x",
		IsShadow:    true,
		Builders:    []string{"flashbots"},
	})
	l.InsertInclusion(NewInclusion{
		BundleID:   uuid.New(),
		Builder:    "flashbots",
		Included:   false,
		ResolvedAt: time.Now(),
	})
	l.UpsertPnLDaily(PnLDailyDelta{
		Day:               time.Now().UTC(),
		RealizedProfitWei: big.NewInt(0),
		GasSpentWei:       big.NewInt(0),
	})
}
