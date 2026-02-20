package main

import (
	"crypto/rand"
	"encoding/hex"
	"math/big"
	"time"
)

// Bundle represents a Flashbots-style bundle
type Bundle struct {
	Transactions []Transaction
	BlockNumber  uint64
	Timestamp    time.Time
}

// Transaction represents a signed Ethereum transaction
type Transaction struct {
	From     string
	To       string
	Data     []byte
	Value    *big.Int
	Gas      uint64
	GasPrice *big.Int // Legacy
	// EIP-1559
	MaxFeePerGas         *big.Int
	MaxPriorityFeePerGas *big.Int
	Nonce                uint64
	ChainID              int64
}

// BundleConstructor builds bundles from validated arbs
type BundleConstructor struct {
	nonceManager *NonceManager
	gasOracle    *GasOracle
	tipSharePct  float64
	chainID      int64
}

// NewBundleConstructor creates a new bundle constructor
func NewBundleConstructor(nm *NonceManager, go_ *GasOracle, tipPct float64, chainID int64) *BundleConstructor {
	return &BundleConstructor{
		nonceManager: nm,
		gasOracle:    go_,
		tipSharePct:  tipPct,
		chainID:      chainID,
	}
}

// BuildBundle constructs a [arb_tx, tip_tx] bundle from an arb opportunity
func (bc *BundleConstructor) BuildBundle(
	arbCalldata []byte,
	executorAddr string,
	profitWei *big.Int,
	gasEstimate uint64,
	targetBlock uint64,
) (*Bundle, error) {
	gasFees := bc.gasOracle.CurrentFees()
	nonce := bc.nonceManager.Next()

	// Arb transaction (calls AetherExecutor.executeArb)
	arbTx := Transaction{
		To:                   executorAddr,
		Data:                 arbCalldata,
		Value:                big.NewInt(0),
		Gas:                  gasEstimate,
		MaxFeePerGas:         gasFees.MaxFeePerGas,
		MaxPriorityFeePerGas: gasFees.MaxPriorityFee,
		Nonce:                nonce,
		ChainID:              bc.chainID,
	}

	// Tip transaction (send % of profit to builder coinbase)
	tipAmount := new(big.Int).Mul(profitWei, big.NewInt(int64(bc.tipSharePct)))
	tipAmount.Div(tipAmount, big.NewInt(100))

	tipTx := Transaction{
		To:                   "0x0000000000000000000000000000000000000000", // Coinbase (filled by builder)
		Data:                 nil,
		Value:                tipAmount,
		Gas:                  21000, // Simple ETH transfer
		MaxFeePerGas:         gasFees.MaxFeePerGas,
		MaxPriorityFeePerGas: gasFees.MaxPriorityFee,
		Nonce:                nonce + 1,
		ChainID:              bc.chainID,
	}

	bundle := &Bundle{
		Transactions: []Transaction{arbTx, tipTx},
		BlockNumber:  targetBlock,
		Timestamp:    time.Now(),
	}

	return bundle, nil
}

// GenerateBundleID creates a unique bundle identifier
func GenerateBundleID() string {
	b := make([]byte, 16)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}
