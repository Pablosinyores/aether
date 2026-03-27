package main

import (
	"math/big"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

// Well-known test private key (Hardhat/Anvil account #0).
// This is a publicly known test key — never use on mainnet.
const testPrivateKeyHex = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
const testExpectedAddr = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

func TestNewTransactionSigner(t *testing.T) {
	signer, err := NewTransactionSigner(testPrivateKeyHex, 1)
	if err != nil {
		t.Fatalf("NewTransactionSigner failed: %v", err)
	}

	expected := common.HexToAddress(testExpectedAddr)
	if signer.Address() != expected {
		t.Errorf("address mismatch: got %s, want %s", signer.Address().Hex(), expected.Hex())
	}
}

func TestNewTransactionSignerWith0xPrefix(t *testing.T) {
	signer, err := NewTransactionSigner("0x"+testPrivateKeyHex, 1)
	if err != nil {
		t.Fatalf("NewTransactionSigner with 0x prefix failed: %v", err)
	}

	expected := common.HexToAddress(testExpectedAddr)
	if signer.Address() != expected {
		t.Errorf("address mismatch: got %s, want %s", signer.Address().Hex(), expected.Hex())
	}
}

func TestNewTransactionSignerEmptyKey(t *testing.T) {
	_, err := NewTransactionSigner("", 1)
	if err == nil {
		t.Fatal("expected error for empty key")
	}
}

func TestNewTransactionSignerInvalidHex(t *testing.T) {
	_, err := NewTransactionSigner("not-a-valid-hex-key", 1)
	if err == nil {
		t.Fatal("expected error for invalid hex key")
	}
	// Ensure the error message doesn't contain the key material.
	if contains(err.Error(), "not-a-valid-hex-key") {
		t.Error("error message should not contain the raw key input")
	}
}

func TestSignTxProducesValidSignature(t *testing.T) {
	signer, err := NewTransactionSigner(testPrivateKeyHex, 1)
	if err != nil {
		t.Fatalf("NewTransactionSigner failed: %v", err)
	}

	// Create an unsigned EIP-1559 transaction.
	tx := types.NewTx(&types.DynamicFeeTx{
		ChainID:   big.NewInt(1),
		Nonce:     0,
		GasTipCap: big.NewInt(2_000_000_000),  // 2 gwei
		GasFeeCap: big.NewInt(30_000_000_000), // 30 gwei
		Gas:       21000,
		To:        addrPtr(common.HexToAddress("0x0000000000000000000000000000000000000001")),
		Value:     big.NewInt(1_000_000_000), // 1 gwei
	})

	signed, err := signer.SignTx(tx)
	if err != nil {
		t.Fatalf("SignTx failed: %v", err)
	}

	// Recover the sender from the signed transaction.
	ethSigner := types.LatestSignerForChainID(big.NewInt(1))
	sender, err := types.Sender(ethSigner, signed)
	if err != nil {
		t.Fatalf("failed to recover sender: %v", err)
	}

	if sender != signer.Address() {
		t.Errorf("recovered sender %s doesn't match signer address %s", sender.Hex(), signer.Address().Hex())
	}
}

func TestSignAndMarshalProducesBytes(t *testing.T) {
	signer, err := NewTransactionSigner(testPrivateKeyHex, 1)
	if err != nil {
		t.Fatalf("NewTransactionSigner failed: %v", err)
	}

	tx := types.NewTx(&types.DynamicFeeTx{
		ChainID:   big.NewInt(1),
		Nonce:     5,
		GasTipCap: big.NewInt(2_000_000_000),
		GasFeeCap: big.NewInt(30_000_000_000),
		Gas:       100000,
		To:        addrPtr(common.HexToAddress("0x0000000000000000000000000000000000000001")),
		Value:     big.NewInt(0),
		Data:      []byte{0x01, 0x02, 0x03},
	})

	raw, err := signer.SignAndMarshal(tx)
	if err != nil {
		t.Fatalf("SignAndMarshal failed: %v", err)
	}

	if len(raw) == 0 {
		t.Fatal("SignAndMarshal returned empty bytes")
	}

	// First byte of an EIP-1559 RLP envelope is 0x02.
	if raw[0] != 0x02 {
		t.Errorf("expected EIP-1559 tx type prefix 0x02, got 0x%02x", raw[0])
	}
}

func TestSignerErrorDoesNotLeakKey(t *testing.T) {
	badKey := "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbe"
	_, err := NewTransactionSigner(badKey, 1)
	if err == nil {
		return // key happened to be valid, skip
	}
	if contains(err.Error(), badKey) {
		t.Error("error message leaked the private key material")
	}
}

// --- helpers ---

func addrPtr(a common.Address) *common.Address {
	return &a
}

func contains(s, substr string) bool {
	return len(s) >= len(substr) && searchString(s, substr)
}

func searchString(s, sub string) bool {
	for i := 0; i <= len(s)-len(sub); i++ {
		if s[i:i+len(sub)] == sub {
			return true
		}
	}
	return false
}
