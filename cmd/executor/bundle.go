package main

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"log/slog"
	"math/big"
	"os"
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

// BundleConstructor builds bundles from validated arbs.
type BundleConstructor struct {
	nonceManager *NonceManager
	gasOracle    *GasOracle
	signer       *TransactionSigner
	chainID      int64
}

// NewBundleConstructor creates a new bundle constructor.
// The signer is used to sign transactions; if nil, transactions are left unsigned.
func NewBundleConstructor(nm *NonceManager, go_ *GasOracle, signer *TransactionSigner, chainID int64) *BundleConstructor {
	return &BundleConstructor{
		nonceManager: nm,
		gasOracle:    go_,
		signer:       signer,
		chainID:      chainID,
	}
}

// BuildBundle constructs a single-transaction bundle containing only the arb tx.
// The coinbase tip is now handled inline by the Solidity contract, so no
// separate tip transaction is needed.
func (bc *BundleConstructor) BuildBundle(
	arbCalldata []byte,
	executorAddr string,
	gasEstimate uint64,
	targetBlock uint64,
) (*Bundle, error) {
	gasFees := bc.gasOracle.CurrentFees()
	nonce := bc.nonceManager.Next()
	chainID := big.NewInt(bc.chainID)
	executor := common.HexToAddress(executorAddr)

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

	// Sign transaction if signer is available.
	if bc.signer != nil {
		signed, err := bc.signer.SignTx(arbTx)
		if err != nil {
			return nil, fmt.Errorf("sign arb tx: %w", err)
		}

		raw, err := signed.MarshalBinary()
		if err != nil {
			return nil, fmt.Errorf("RLP-encode arb tx: %w", err)
		}

		return &Bundle{
			Transactions: []*types.Transaction{signed},
			RawTxs:       [][]byte{raw},
			BlockNumber:  targetBlock,
			Timestamp:    time.Now(),
		}, nil
	}

	// No signer — return unsigned (for testing).
	return &Bundle{
		Transactions: []*types.Transaction{arbTx},
		BlockNumber:  targetBlock,
		Timestamp:    time.Now(),
	}, nil
}

// GenerateBundleID creates a unique bundle identifier
func GenerateBundleID() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		slog.Error("crypto/rand failure", "err", err)
		os.Exit(1)
	}
	return hex.EncodeToString(b)
}
