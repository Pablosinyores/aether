package main

import (
	"encoding/hex"
	"math/big"
	"testing"
)

func TestBuildBundle_Basic(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, 90.0, 1)

	profit := intETHToWei(t, 1) // 1 ETH
	calldata := []byte{0xAB, 0xCD}
	executor := "0x1234567890abcdef1234567890abcdef12345678"

	bundle, err := bc.BuildBundle(calldata, executor, profit, 500000, 18000000)
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
	if arbTx.Nonce != 0 {
		t.Errorf("arb tx nonce: expected 0, got %d", arbTx.Nonce)
	}
	if tipTx.Nonce != 1 {
		t.Errorf("tip tx nonce: expected 1, got %d", tipTx.Nonce)
	}

	// EIP-1559 fields set on arb tx
	if arbTx.MaxFeePerGas == nil || arbTx.MaxFeePerGas.Sign() <= 0 {
		t.Error("arb tx MaxFeePerGas not set or zero")
	}
	if arbTx.MaxPriorityFeePerGas == nil || arbTx.MaxPriorityFeePerGas.Sign() <= 0 {
		t.Error("arb tx MaxPriorityFeePerGas not set or zero")
	}

	// EIP-1559 fields set on tip tx
	if tipTx.MaxFeePerGas == nil || tipTx.MaxFeePerGas.Sign() <= 0 {
		t.Error("tip tx MaxFeePerGas not set or zero")
	}
	if tipTx.MaxPriorityFeePerGas == nil || tipTx.MaxPriorityFeePerGas.Sign() <= 0 {
		t.Error("tip tx MaxPriorityFeePerGas not set or zero")
	}

	// Block number
	if bundle.BlockNumber != 18000000 {
		t.Errorf("expected block 18000000, got %d", bundle.BlockNumber)
	}

	// ChainID
	if arbTx.ChainID != 1 {
		t.Errorf("expected chainID 1, got %d", arbTx.ChainID)
	}

	// Executor address
	if arbTx.To != executor {
		t.Errorf("arb tx To: expected %s, got %s", executor, arbTx.To)
	}

	// Calldata
	if len(arbTx.Data) != 2 || arbTx.Data[0] != 0xAB || arbTx.Data[1] != 0xCD {
		t.Errorf("arb tx calldata mismatch")
	}
}

func TestBuildBundle_TipCalculation(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name        string
		tipSharePct float64
		profitETH   int64
		wantTipETH  float64 // Expected tip in ETH
	}{
		{"90% of 1 ETH", 90, 1, 0.9},
		{"50% of 1 ETH", 50, 1, 0.5},
		{"10% of 2 ETH", 10, 2, 0.2},
		{"95% of 1 ETH", 95, 1, 0.95},
		{"1% of 10 ETH", 1, 10, 0.1},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			nm := NewNonceManager(0)
			go_ := NewGasOracle(300.0)
			bc := NewBundleConstructor(nm, go_, tc.tipSharePct, 1)

			profit := intETHToWei(t, tc.profitETH)
			bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", profit, 300000, 100)
			if err != nil {
				t.Fatalf("BuildBundle error: %v", err)
			}

			tipTx := bundle.Transactions[1]
			// Expected: profitWei * tipSharePct / 100
			expectedTip := new(big.Int).Mul(profit, big.NewInt(int64(tc.tipSharePct)))
			expectedTip.Div(expectedTip, big.NewInt(100))

			if tipTx.Value.Cmp(expectedTip) != 0 {
				t.Errorf("tip amount: got %s, want %s", tipTx.Value.String(), expectedTip.String())
			}
		})
	}
}

func TestBuildBundle_ZeroProfit(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, 90.0, 1)

	bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", big.NewInt(0), 300000, 100)
	if err != nil {
		t.Fatalf("BuildBundle error: %v", err)
	}

	tipTx := bundle.Transactions[1]
	if tipTx.Value.Sign() != 0 {
		t.Errorf("expected tip=0 for zero profit, got %s", tipTx.Value.String())
	}
}

func TestBuildBundle_GasEstimate(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bc := NewBundleConstructor(nm, go_, 90.0, 1)

	gasEstimates := []uint64{21000, 500000, 1000000, 250000}

	for _, gasEst := range gasEstimates {
		bundle, err := bc.BuildBundle([]byte{0x01}, "0xExecutor", big.NewInt(1000), gasEst, 100)
		if err != nil {
			t.Fatalf("BuildBundle error: %v", err)
		}

		arbTx := bundle.Transactions[0]
		if arbTx.Gas != gasEst {
			t.Errorf("gas estimate: got %d, want %d", arbTx.Gas, gasEst)
		}

		// Tip tx always uses 21000 (simple ETH transfer)
		tipTx := bundle.Transactions[1]
		if tipTx.Gas != 21000 {
			t.Errorf("tip tx gas: got %d, want 21000", tipTx.Gas)
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
