package main

import (
	"encoding/hex"
	"math/big"
	"testing"

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
	expectedAddr := "0x1234567890abcdef1234567890abcdef12345678"
	if arbTx.To() == nil || arbTx.To().Hex() != "0x1234567890AbcdEF1234567890aBcdef12345678" {
		t.Errorf("arb tx To: expected %s, got %v", expectedAddr, arbTx.To())
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
