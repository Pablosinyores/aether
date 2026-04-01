package main

import (
	"encoding/hex"
	"math/big"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

var testCoinbase = common.HexToAddress("0x0000000000000000000000000000000000000001")

func TestBuildBundle_Basic(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	profit := intETHToWei(t, 1) // 1 ETH
	calldata := []byte{0xAB, 0xCD}
	executor := "0x1234567890abcdef1234567890abcdef12345678"

	bundle, err := bc.BuildBundle(calldata, executor, profit, 500000, 18000000, testCoinbase, 90.0)
	if err != nil {
		t.Fatalf("BuildBundle returned error: %v", err)
	}

	// Must have exactly 2 transactions: arb + tip
	if len(bundle.Transactions) != 2 {
		t.Fatalf("expected 2 transactions, got %d", len(bundle.Transactions))
	}

	arbTx := bundle.Transactions[0]
	tipTx := bundle.Transactions[1]

	// Nonce sequence: arb=0, tip=1
	if arbTx.Nonce() != 0 {
		t.Errorf("arb tx nonce: expected 0, got %d", arbTx.Nonce())
	}
	if tipTx.Nonce() != 1 {
		t.Errorf("tip tx nonce: expected 1, got %d", tipTx.Nonce())
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
	expectedAddr := common.HexToAddress(executor)
	if arbTx.To() == nil || *arbTx.To() != expectedAddr {
		t.Errorf("arb tx To: expected %s, got %v", expectedAddr.Hex(), arbTx.To())
	}

	// Calldata
	if len(arbTx.Data()) != 2 || arbTx.Data()[0] != 0xAB || arbTx.Data()[1] != 0xCD {
		t.Errorf("arb tx calldata mismatch")
	}
}

func TestBuildBundle_TipCalculation(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name        string
		tipSharePct float64
		profitETH   int64
	}{
		{"90% of 1 ETH", 90, 1},
		{"50% of 1 ETH", 50, 1},
		{"10% of 2 ETH", 10, 2},
		{"95% of 1 ETH", 95, 1},
		{"1% of 10 ETH", 1, 10},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			nm := NewNonceManager(0)
			go_ := NewGasOracle(300.0)
			bc := NewBundleConstructor(nm, go_, nil, 1)

			profit := intETHToWei(t, tc.profitETH)
			bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", profit, 300000, 100, testCoinbase, tc.tipSharePct)
			if err != nil {
				t.Fatalf("BuildBundle error: %v", err)
			}

			tipTx := bundle.Transactions[1]
			expectedTip := new(big.Int).Mul(profit, big.NewInt(int64(tc.tipSharePct)))
			expectedTip.Div(expectedTip, big.NewInt(100))

			if tipTx.Value().Cmp(expectedTip) != 0 {
				t.Errorf("tip amount: got %s, want %s", tipTx.Value().String(), expectedTip.String())
			}
		})
	}
}

func TestBuildBundle_ZeroProfit(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", big.NewInt(0), 300000, 100, testCoinbase, 90.0)
	if err != nil {
		t.Fatalf("BuildBundle error: %v", err)
	}

	tipTx := bundle.Transactions[1]
	if tipTx.Value().Sign() != 0 {
		t.Errorf("expected tip=0 for zero profit, got %s", tipTx.Value().String())
	}
}

func TestBuildBundle_GasEstimate(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	gasEstimates := []uint64{21000, 500000, 1000000, 250000}

	for _, gasEst := range gasEstimates {
		bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", big.NewInt(1000), gasEst, 100, testCoinbase, 90.0)
		if err != nil {
			t.Fatalf("BuildBundle error: %v", err)
		}

		arbTx := bundle.Transactions[0]
		if arbTx.Gas() != gasEst {
			t.Errorf("gas estimate: got %d, want %d", arbTx.Gas(), gasEst)
		}

		tipTx := bundle.Transactions[1]
		if tipTx.Gas() != 21000 {
			t.Errorf("tip tx gas: got %d, want 21000", tipTx.Gas())
		}
	}
}

func TestBuildBundle_TipGoesToCoinbase(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, nil, 1)

	coinbase := common.HexToAddress("0xDEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF")
	bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", big.NewInt(1e18), 300000, 100, coinbase, 90.0)
	if err != nil {
		t.Fatalf("BuildBundle error: %v", err)
	}

	tipTx := bundle.Transactions[1]
	if tipTx.To() == nil || *tipTx.To() != coinbase {
		t.Errorf("tip tx To: expected %s, got %v", coinbase.Hex(), tipTx.To())
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

	profit := intETHToWei(t, 1)
	bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", profit, 300000, 18000000, testCoinbase, 90.0)
	if err != nil {
		t.Fatalf("BuildBundle error: %v", err)
	}

	// Should have signed raw bytes
	if len(bundle.RawTxs) != 2 {
		t.Fatalf("expected 2 raw txs, got %d", len(bundle.RawTxs))
	}

	for i, raw := range bundle.RawTxs {
		if len(raw) == 0 {
			t.Errorf("raw tx %d is empty", i)
		}
		// EIP-1559 tx type prefix
		if raw[0] != 0x02 {
			t.Errorf("raw tx %d: expected type 0x02, got 0x%02x", i, raw[0])
		}
	}

	// Verify sender can be recovered from signed transactions
	ethSigner := types.LatestSignerForChainID(big.NewInt(1))
	for i, tx := range bundle.Transactions {
		sender, err := types.Sender(ethSigner, tx)
		if err != nil {
			t.Fatalf("failed to recover sender from tx %d: %v", i, err)
		}
		if sender != signer.Address() {
			t.Errorf("tx %d sender: got %s, want %s", i, sender.Hex(), signer.Address().Hex())
		}
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

		// 16 bytes → 32 hex characters
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
