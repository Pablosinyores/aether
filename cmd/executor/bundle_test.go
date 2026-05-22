package main

import (
	"bytes"
	"encoding/hex"
	"math/big"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

func TestBuildBundle_Basic(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	calldata := []byte{0xAB, 0xCD}
	executor := "0x1234567890abcdef1234567890abcdef12345678"

	bundle, err := bc.BuildBundle(calldata, executor, 500000, 18000000)
	if err != nil {
		t.Fatalf("BuildBundle returned error: %v", err)
	}

	// Must have exactly 1 transaction (arb only, tip is in-contract).
	if len(bundle.Transactions) != 1 {
		t.Fatalf("expected 1 transaction, got %d", len(bundle.Transactions))
	}

	arbTx := bundle.Transactions[0]

	// Nonce
	if arbTx.Nonce() != 0 {
		t.Errorf("arb tx nonce: expected 0, got %d", arbTx.Nonce())
	}

	// EIP-1559 fields set on arb tx
	if arbTx.GasFeeCap() == nil || arbTx.GasFeeCap().Sign() <= 0 {
		t.Error("arb tx GasFeeCap not set or zero")
	}
	if arbTx.GasTipCap() == nil || arbTx.GasTipCap().Sign() <= 0 {
		t.Error("arb tx GasTipCap not set or zero")
	}

	// Block number
	if bundle.BlockNumber != 18000000 {
		t.Errorf("expected block 18000000, got %d", bundle.BlockNumber)
	}

	// ChainID
	if arbTx.ChainId().Int64() != 1 {
		t.Errorf("expected chainID 1, got %d", arbTx.ChainId().Int64())
	}

	// Executor address
	expectedAddr := common.HexToAddress("0x1234567890abcdef1234567890abcdef12345678")
	if arbTx.To() == nil || *arbTx.To() != expectedAddr {
		t.Errorf("arb tx To: expected %s, got %v", expectedAddr.Hex(), arbTx.To())
	}

	// Calldata
	if len(arbTx.Data()) != 2 || arbTx.Data()[0] != 0xAB || arbTx.Data()[1] != 0xCD {
		t.Errorf("arb tx calldata mismatch")
	}
}

func TestBuildBundle_GasEstimate(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	gasEstimates := []uint64{21000, 500000, 1000000, 250000}

	for _, gasEst := range gasEstimates {
		// Reset nonce for each iteration so test is independent.
		nm.Reset(0)

		bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", gasEst, 100)
		if err != nil {
			t.Fatalf("BuildBundle error: %v", err)
		}

		if len(bundle.Transactions) != 1 {
			t.Fatalf("expected 1 transaction, got %d", len(bundle.Transactions))
		}

		arbTx := bundle.Transactions[0]
		if arbTx.Gas() != gasEst {
			t.Errorf("gas estimate: got %d, want %d", arbTx.Gas(), gasEst)
		}
	}
}

func TestBuildBundle_WithSigner(t *testing.T) {
	t.Parallel()

	signer, err := NewTransactionSigner(testPrivateKeyHex, 1)
	if err != nil {
		t.Fatalf("NewTransactionSigner failed: %v", err)
	}

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, signer, 1)

	bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", 300000, 18000000)
	if err != nil {
		t.Fatalf("BuildBundle error: %v", err)
	}

	// Should have 1 signed raw tx.
	if len(bundle.RawTxs) != 1 {
		t.Fatalf("expected 1 raw tx, got %d", len(bundle.RawTxs))
	}

	raw := bundle.RawTxs[0]
	if len(raw) == 0 {
		t.Error("raw tx is empty")
	}
	// EIP-1559 tx type prefix
	if raw[0] != 0x02 {
		t.Errorf("raw tx: expected type 0x02, got 0x%02x", raw[0])
	}

	// Verify sender can be recovered from the signed transaction.
	ethSigner := types.LatestSignerForChainID(big.NewInt(1))
	sender, recoverErr := types.Sender(ethSigner, bundle.Transactions[0])
	if recoverErr != nil {
		t.Fatalf("failed to recover sender: %v", recoverErr)
	}
	if sender != signer.Address() {
		t.Errorf("sender: got %s, want %s", sender.Hex(), signer.Address().Hex())
	}
}

func TestGenerateBundleID_Uniqueness(t *testing.T) {
	t.Parallel()

	seen := make(map[string]bool)
	for i := 0; i < 100; i++ {
		id := GenerateBundleID()
		if seen[id] {
			t.Fatalf("duplicate bundle ID generated: %s (iteration %d)", id, i)
		}
		seen[id] = true
	}
}

func TestGenerateBundleID_Format(t *testing.T) {
	t.Parallel()

	for i := 0; i < 10; i++ {
		id := GenerateBundleID()

		// 16 bytes -> 32 hex characters
		if len(id) != 32 {
			t.Errorf("expected ID length 32, got %d: %s", len(id), id)
		}

		// Must be valid hex
		_, err := hex.DecodeString(id)
		if err != nil {
			t.Errorf("ID is not valid hex: %s, error: %v", id, err)
		}
	}
}

// intETHToWei is a test helper that converts an integer ETH amount to wei.
func intETHToWei(t *testing.T, eth int64) *big.Int {
	t.Helper()
	oneETH := new(big.Int).Exp(big.NewInt(10), big.NewInt(18), nil)
	return new(big.Int).Mul(big.NewInt(eth), oneETH)
}

// ---------------------------------------------------------------------------
// BuildMempoolBackrunBundle
// ---------------------------------------------------------------------------

const victimHashHex = "0xdeadbeef00000000000000000000000000000000000000000000000000000001"

// victimRawTxBytes stands in for the canonical EIP-2718 raw signed bytes of
// the pending victim tx that the Rust engine captures from the mempool and
// ships over gRPC. The contents are opaque to the executor; only ordering
// and presence matter here.
var victimRawTxBytes = []byte{0x02, 0xF8, 0x6B, 0x01, 0x02, 0x03, 0x04}

func newBundleConstructorForTest(t *testing.T) *BundleConstructor {
	t.Helper()
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	signer, err := NewTransactionSigner("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 1)
	if err != nil {
		t.Fatalf("test signer: %v", err)
	}
	return NewBundleConstructor(nm, go_, signer, 1)
}

func TestBuildMempoolBackrunBundle_HappyPath(t *testing.T) {
	t.Parallel()

	bc := newBundleConstructorForTest(t)
	bundle, err := bc.BuildMempoolBackrunBundle(
		[]byte{0xAB, 0xCD},
		"0x1234567890abcdef1234567890abcdef12345678",
		500000,
		18_000_001,
		victimHashHex,
		victimRawTxBytes,
	)
	if err != nil {
		t.Fatalf("BuildMempoolBackrunBundle: %v", err)
	}

	if bundle.Source != SourceMempoolBackrun {
		t.Errorf("bundle.Source: want %q, got %q", SourceMempoolBackrun, bundle.Source)
	}
	if bundle.VictimTxHashHex != victimHashHex {
		t.Errorf("VictimTxHashHex: want %s, got %s", victimHashHex, bundle.VictimTxHashHex)
	}
	// Envelope must be [victim_raw, our_arb_signed]: the victim's raw signed
	// tx first, our signed arb second.
	if len(bundle.RawTxs) != 2 {
		t.Fatalf("RawTxs: want 2 ([victim_raw, our_arb]), got %d", len(bundle.RawTxs))
	}
	if !bytes.Equal(bundle.RawTxs[0], victimRawTxBytes) {
		t.Fatalf("RawTxs[0] must be the supplied victim raw tx; want %x, got %x",
			victimRawTxBytes, bundle.RawTxs[0])
	}
	arbRaw, err := bundle.Transactions[0].MarshalBinary()
	if err != nil {
		t.Fatalf("marshal arb tx: %v", err)
	}
	if !bytes.Equal(bundle.RawTxs[1], arbRaw) {
		t.Fatalf("RawTxs[1] must be our signed arb tx raw bytes")
	}
	if len(bundle.RevertingTxHashes) != 1 {
		t.Fatalf("RevertingTxHashes len: want 1, got %d", len(bundle.RevertingTxHashes))
	}
	if bundle.RevertingTxHashes[0] == victimHashHex {
		t.Fatalf("RevertingTxHashes must NOT include victim hash; got %v", bundle.RevertingTxHashes)
	}
	if bundle.RevertingTxHashes[0] != bundle.Transactions[0].Hash().Hex() {
		t.Fatalf("RevertingTxHashes[0]=%s, expected arb tx hash %s",
			bundle.RevertingTxHashes[0], bundle.Transactions[0].Hash().Hex())
	}
	if bundle.BlockNumber != 18_000_001 {
		t.Errorf("BlockNumber: want 18_000_001, got %d", bundle.BlockNumber)
	}
}

func TestBuildMempoolBackrunBundle_RejectsEmptyVictimRawTx(t *testing.T) {
	t.Parallel()

	bc := newBundleConstructorForTest(t)
	_, err := bc.BuildMempoolBackrunBundle(
		[]byte{0xAB, 0xCD},
		"0x1234567890abcdef1234567890abcdef12345678",
		500000,
		18_000_001,
		victimHashHex,
		nil,
	)
	if err == nil {
		t.Fatal("expected error for empty victim_raw_tx, got nil")
	}
}

func TestBuildMempoolBackrunBundle_RejectsBadVictimHash(t *testing.T) {
	t.Parallel()

	bc := newBundleConstructorForTest(t)
	cases := []struct {
		name string
		hash string
	}{
		{"missing 0x prefix", "deadbeef00000000000000000000000000000000000000000000000000000001"},
		{"wrong length", "0x1234"},
		{"empty", ""},
	}
	for _, c := range cases {
		c := c
		t.Run(c.name, func(t *testing.T) {
			t.Parallel()
			_, err := bc.BuildMempoolBackrunBundle([]byte{0x01}, "0x1234567890abcdef1234567890abcdef12345678", 100000, 1, c.hash, []byte{0x02})
			if err == nil {
				t.Fatalf("expected error for %s, got nil", c.name)
			}
		})
	}
}

func TestBuildMempoolBackrunBundle_UnsignedFallback(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	victimRaw := []byte{0x02, 0xCA, 0xFE}
	bundle, err := bc.BuildMempoolBackrunBundle(
		[]byte{0xAB},
		"0x1234567890abcdef1234567890abcdef12345678",
		200000,
		18_000_001,
		victimHashHex,
		victimRaw,
	)
	if err != nil {
		t.Fatalf("unsigned BuildMempoolBackrunBundle: %v", err)
	}
	// Even on the unsigned (test-only) path the victim raw tx is seated as
	// RawTxs[0]; only our arb tx is unsigned and therefore has no raw bytes.
	if len(bundle.RawTxs) != 1 {
		t.Fatalf("unsigned bundle should carry only the victim raw tx, got %d RawTxs", len(bundle.RawTxs))
	}
	if !bytes.Equal(bundle.RawTxs[0], victimRaw) {
		t.Errorf("RawTxs[0]: want victim raw %x, got %x", victimRaw, bundle.RawTxs[0])
	}
	if len(bundle.Transactions) != 1 {
		t.Errorf("expected 1 unsigned transaction, got %d", len(bundle.Transactions))
	}
	if bundle.VictimTxHashHex != victimHashHex {
		t.Errorf("VictimTxHashHex: want %s, got %s", victimHashHex, bundle.VictimTxHashHex)
	}
}
