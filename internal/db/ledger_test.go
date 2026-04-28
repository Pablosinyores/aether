package db

import (
	"math/big"
	"testing"
	"time"

	"github.com/google/uuid"
)

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
