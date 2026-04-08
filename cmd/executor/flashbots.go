package main

import (
	"crypto/ecdsa"
	"fmt"
	"strings"

	"github.com/ethereum/go-ethereum/accounts"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/crypto"
)

// FlashbotsSigner signs JSON-RPC request bodies for the X-Flashbots-Signature
// header required by the Flashbots relay and compatible builders.
//
// The signing process follows the Flashbots spec:
//
//	payload -> keccak256 -> hex string -> EIP-191 TextHash -> secp256k1 sign
//
// The resulting header value is "address:0xsignature".
type FlashbotsSigner struct {
	key     *ecdsa.PrivateKey
	address common.Address
}

// NewFlashbotsSigner creates a FlashbotsSigner from a hex-encoded private key.
// The key can optionally have a "0x" prefix.
func NewFlashbotsSigner(hexKey string) (*FlashbotsSigner, error) {
	if hexKey == "" {
		return nil, fmt.Errorf("private key is empty")
	}

	cleaned := strings.TrimPrefix(hexKey, "0x")

	key, err := crypto.HexToECDSA(cleaned)
	if err != nil {
		// Never include key material in the error message.
		return nil, fmt.Errorf("failed to parse private key: invalid format")
	}

	pubKey, ok := key.Public().(*ecdsa.PublicKey)
	if !ok {
		return nil, fmt.Errorf("failed to derive public key")
	}

	return &FlashbotsSigner{
		key:     key,
		address: crypto.PubkeyToAddress(*pubKey),
	}, nil
}

// Address returns the Ethereum address derived from the private key.
func (fs *FlashbotsSigner) Address() common.Address {
	return fs.address
}

// Sign produces a Flashbots-compatible signature for the given payload.
// Returns the header value in "address:0xsignature" format.
func (fs *FlashbotsSigner) Sign(payload []byte) (string, error) {
	// Step 1: keccak256 hash of the raw payload
	// Step 2: hex-encode the hash (produces "0x..." string)
	hashHex := crypto.Keccak256Hash(payload).Hex()

	// Step 3: apply EIP-191 personal message prefix to the hex string
	// Step 4: sign with secp256k1
	sig, err := crypto.Sign(accounts.TextHash([]byte(hashHex)), fs.key)
	if err != nil {
		return "", fmt.Errorf("signing failed: %w", err)
	}

	// Adjust V value from 0/1 to 27/28 per Ethereum convention.
	sig[64] += 27

	return fmt.Sprintf("%s:%s", fs.address.Hex(), hexutil.Encode(sig)), nil
}
