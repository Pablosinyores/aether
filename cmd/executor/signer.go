package main

import (
	"crypto/ecdsa"
	"errors"
	"fmt"
	"math/big"
	"strings"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/crypto"
)

// TransactionSigner holds a private key and provides transaction signing.
// The private key is held only in memory — never logged or serialized.
type TransactionSigner struct {
	privateKey *ecdsa.PrivateKey
	address    common.Address
	chainID    int64
	signer     types.Signer
}

// NewTransactionSigner creates a signer from a hex-encoded private key.
// The key can optionally have a "0x" prefix. Returns an error if the key
// is missing, malformed, or produces an invalid public key.
func NewTransactionSigner(hexKey string, chainID int64) (*TransactionSigner, error) {
	if hexKey == "" {
		return nil, errors.New("private key is empty")
	}

	if chainID <= 0 {
		return nil, errors.New("chain ID must be positive")
	}

	// Strip 0x prefix if present.
	cleaned := strings.TrimPrefix(hexKey, "0x")

	privateKey, err := crypto.HexToECDSA(cleaned)
	if err != nil {
		// Never include the key material in the error message.
		return nil, fmt.Errorf("failed to parse private key: invalid format")
	}

	publicKey, ok := privateKey.Public().(*ecdsa.PublicKey)
	if !ok {
		return nil, errors.New("failed to derive public key")
	}

	address := crypto.PubkeyToAddress(*publicKey)

	return &TransactionSigner{
		privateKey: privateKey,
		address:    address,
		chainID:    chainID,
		signer:     types.LatestSignerForChainID(toBigInt(chainID)),
	}, nil
}

// Address returns the Ethereum address derived from the private key.
func (ts *TransactionSigner) Address() common.Address {
	return ts.address
}

// SignTx signs an unsigned transaction and returns the signed transaction.
func (ts *TransactionSigner) SignTx(tx *types.Transaction) (*types.Transaction, error) {
	return types.SignTx(tx, ts.signer, ts.privateKey)
}

// SignAndMarshal signs a transaction and returns its RLP-encoded bytes.
// This is the format expected by eth_sendBundle and builder APIs.
func (ts *TransactionSigner) SignAndMarshal(tx *types.Transaction) ([]byte, error) {
	signed, err := ts.SignTx(tx)
	if err != nil {
		return nil, fmt.Errorf("signing failed: %w", err)
	}

	raw, err := signed.MarshalBinary()
	if err != nil {
		return nil, fmt.Errorf("RLP encoding failed: %w", err)
	}

	return raw, nil
}

func toBigInt(n int64) *big.Int {
	return big.NewInt(n)
}
