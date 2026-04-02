package main

import (
	"encoding/hex"
	"strings"
	"testing"

	"github.com/ethereum/go-ethereum/accounts"
	"github.com/ethereum/go-ethereum/crypto"
)

// testSearcherKey is Hardhat/Foundry account #0 — a well-known test key.
const testSearcherKey = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

func TestNewFlashbotsSigner_Valid(t *testing.T) {
	t.Parallel()

	signer, err := NewFlashbotsSigner(testSearcherKey)
	if err != nil {
		t.Fatalf("NewFlashbotsSigner: %v", err)
	}

	addr := signer.Address().Hex()
	if !strings.HasPrefix(addr, "0x") {
		t.Errorf("address should start with 0x, got %s", addr)
	}
	if len(addr) != 42 {
		t.Errorf("address should be 42 chars, got %d: %s", len(addr), addr)
	}
}

func TestNewFlashbotsSigner_WithPrefix(t *testing.T) {
	t.Parallel()

	signer, err := NewFlashbotsSigner("0x" + testSearcherKey)
	if err != nil {
		t.Fatalf("NewFlashbotsSigner with 0x prefix: %v", err)
	}

	signerNoPrefix, _ := NewFlashbotsSigner(testSearcherKey)
	if signer.Address() != signerNoPrefix.Address() {
		t.Errorf("address mismatch: %s vs %s", signer.Address(), signerNoPrefix.Address())
	}
}

func TestNewFlashbotsSigner_Empty(t *testing.T) {
	t.Parallel()

	_, err := NewFlashbotsSigner("")
	if err == nil {
		t.Fatal("expected error for empty key")
	}
}

func TestNewFlashbotsSigner_Invalid(t *testing.T) {
	t.Parallel()

	_, err := NewFlashbotsSigner("not-a-hex-key")
	if err == nil {
		t.Fatal("expected error for invalid key")
	}
	if strings.Contains(err.Error(), "not-a-hex-key") {
		t.Error("error should not contain the raw key input")
	}
}

func TestFlashbotsSigner_Sign(t *testing.T) {
	t.Parallel()

	signer, err := NewFlashbotsSigner(testSearcherKey)
	if err != nil {
		t.Fatalf("NewFlashbotsSigner: %v", err)
	}

	payload := []byte(`{"jsonrpc":"2.0","id":1,"method":"eth_sendBundle","params":[]}`)
	sig, err := signer.Sign(payload)
	if err != nil {
		t.Fatalf("Sign: %v", err)
	}

	// Format: address:0xsignature
	parts := strings.SplitN(sig, ":", 2)
	if len(parts) != 2 {
		t.Fatalf("expected address:signature format, got %q", sig)
	}

	if parts[0] != signer.Address().Hex() {
		t.Errorf("address in signature = %s, want %s", parts[0], signer.Address().Hex())
	}

	// Signature: 0x + 65 bytes = 132 hex chars
	if !strings.HasPrefix(parts[1], "0x") {
		t.Errorf("signature should start with 0x, got %s", parts[1])
	}
	if len(parts[1]) != 132 {
		t.Errorf("signature should be 132 chars (65 bytes hex), got %d", len(parts[1]))
	}
}

func TestFlashbotsSigner_Deterministic(t *testing.T) {
	t.Parallel()

	signer, _ := NewFlashbotsSigner(testSearcherKey)
	payload := []byte(`test payload for determinism check`)

	sig1, _ := signer.Sign(payload)
	sig2, _ := signer.Sign(payload)

	if sig1 != sig2 {
		t.Errorf("signatures should be deterministic:\nsig1=%s\nsig2=%s", sig1, sig2)
	}
}

func TestFlashbotsSigner_DifferentPayloads(t *testing.T) {
	t.Parallel()

	signer, _ := NewFlashbotsSigner(testSearcherKey)

	sig1, _ := signer.Sign([]byte("payload 1"))
	sig2, _ := signer.Sign([]byte("payload 2"))

	if sig1 == sig2 {
		t.Error("different payloads should produce different signatures")
	}

	addr1 := strings.SplitN(sig1, ":", 2)[0]
	addr2 := strings.SplitN(sig2, ":", 2)[0]
	if addr1 != addr2 {
		t.Errorf("address should be same across signatures: %s vs %s", addr1, addr2)
	}
}

func TestFlashbotsSigner_KnownAddress(t *testing.T) {
	t.Parallel()

	signer, err := NewFlashbotsSigner(testSearcherKey)
	if err != nil {
		t.Fatalf("NewFlashbotsSigner: %v", err)
	}

	expected := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
	got := signer.Address().Hex()
	if !strings.EqualFold(got, expected) {
		t.Errorf("address = %s, want %s", got, expected)
	}
}

// TestFlashbotsSigner_EIP191Recovery verifies the signature can be recovered
// using the same EIP-191 process the Flashbots relay uses for verification.
func TestFlashbotsSigner_EIP191Recovery(t *testing.T) {
	t.Parallel()

	signer, err := NewFlashbotsSigner(testSearcherKey)
	if err != nil {
		t.Fatalf("NewFlashbotsSigner: %v", err)
	}

	payload := []byte(`{"jsonrpc":"2.0","id":1,"method":"eth_sendBundle","params":[{"txs":["0xdeadbeef"],"blockNumber":"0x112a880"}]}`)

	sigStr, err := signer.Sign(payload)
	if err != nil {
		t.Fatalf("Sign: %v", err)
	}

	// Extract raw signature bytes.
	parts := strings.SplitN(sigStr, ":", 2)
	sigHex := strings.TrimPrefix(parts[1], "0x")
	sigBytes, err := hex.DecodeString(sigHex)
	if err != nil {
		t.Fatalf("decode signature hex: %v", err)
	}

	// Undo V adjustment (27/28 -> 0/1) for ecrecover.
	sigBytes[64] -= 27

	// Reproduce the relay's verification: keccak256 -> hex -> TextHash -> ecrecover.
	hashHex := crypto.Keccak256Hash(payload).Hex()
	recoveredPub, err := crypto.Ecrecover(accounts.TextHash([]byte(hashHex)), sigBytes)
	if err != nil {
		t.Fatalf("Ecrecover: %v", err)
	}

	pubKey, err := crypto.UnmarshalPubkey(recoveredPub)
	if err != nil {
		t.Fatalf("UnmarshalPubkey: %v", err)
	}

	recovered := crypto.PubkeyToAddress(*pubKey)
	if recovered != signer.Address() {
		t.Errorf("recovered address %s doesn't match signer %s", recovered.Hex(), signer.Address().Hex())
	}
}
