package main

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"log"
	"math/big"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

// Bundle represents a Flashbots-style bundle with signed transactions.
type Bundle struct {
	Transactions []*types.Transaction // Signed go-ethereum transactions
	RawTxs       [][]byte             // RLP-encoded signed bytes (for eth_sendBundle)
	BlockNumber  uint64
	Timestamp    time.Time
}

// BundleConstructor builds bundles from validated arbs
type BundleConstructor struct {
	nonceManager *NonceManager
	gasOracle    *GasOracle
	signer       *TransactionSigner
	tipSharePct  float64
	chainID      int64
}

// NewBundleConstructor creates a new bundle constructor.
// The signer is used to sign transactions; if nil, transactions are left unsigned.
func NewBundleConstructor(nm *NonceManager, go_ *GasOracle, signer *TransactionSigner, tipPct float64, chainID int64) *BundleConstructor {
	return &BundleConstructor{
		nonceManager: nm,
		gasOracle:    go_,
		signer:       signer,
		tipSharePct:  tipPct,
		chainID:      chainID,
	}
}

// BuildBundle constructs a [arb_tx, tip_tx] bundle from an arb opportunity.
// The coinbase parameter is the block proposer's address for the tip payment.
func (bc *BundleConstructor) BuildBundle(
	arbCalldata []byte,
	executorAddr string,
	profitWei *big.Int,
	gasEstimate uint64,
	targetBlock uint64,
	coinbase common.Address,
) (*Bundle, error) {
	gasFees := bc.gasOracle.CurrentFees()
	nonce := bc.nonceManager.Next()
	chainID := big.NewInt(bc.chainID)
	executor := common.HexToAddress(executorAddr)

	// Arb transaction (calls AetherExecutor.executeArb)
	arbTx := types.NewTx(&types.DynamicFeeTx{
		ChainID:   chainID,
		Nonce:     nonce,
		GasTipCap: gasFees.MaxPriorityFee,
		GasFeeCap: gasFees.MaxFeePerGas,
		Gas:       gasEstimate,
		To:        &executor,
		Value:     big.NewInt(0),
		Data:      arbCalldata,
	})

	// Tip transaction (send % of profit to block proposer)
	tipAmount := new(big.Int).Mul(profitWei, big.NewInt(int64(bc.tipSharePct)))
	tipAmount.Div(tipAmount, big.NewInt(100))

	tipTx := types.NewTx(&types.DynamicFeeTx{
		ChainID:   chainID,
		Nonce:     nonce + 1,
		GasTipCap: gasFees.MaxPriorityFee,
		GasFeeCap: gasFees.MaxFeePerGas,
		Gas:       21000, // Simple ETH transfer
		To:        &coinbase,
		Value:     tipAmount,
	})

	txs := []*types.Transaction{arbTx, tipTx}

	// Sign transactions if signer is available.
	if bc.signer != nil {
		signedTxs := make([]*types.Transaction, len(txs))
		rawTxs := make([][]byte, len(txs))

		for i, tx := range txs {
			signed, err := bc.signer.SignTx(tx)
			if err != nil {
				return nil, fmt.Errorf("failed to sign tx %d: %w", i, err)
			}

			raw, err := signed.MarshalBinary()
			if err != nil {
				return nil, fmt.Errorf("failed to RLP-encode tx %d: %w", i, err)
			}

			signedTxs[i] = signed
			rawTxs[i] = raw
		}

		return &Bundle{
			Transactions: signedTxs,
			RawTxs:       rawTxs,
			BlockNumber:  targetBlock,
			Timestamp:    time.Now(),
		}, nil
	}

	// No signer — return unsigned (for testing).
	return &Bundle{
		Transactions: txs,
		BlockNumber:  targetBlock,
		Timestamp:    time.Now(),
	}, nil
}

// GenerateBundleID creates a unique bundle identifier
func GenerateBundleID() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		log.Fatalf("crypto/rand failure: %v", err)
	}
	return hex.EncodeToString(b)
}
