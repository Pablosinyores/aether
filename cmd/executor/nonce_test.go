package main

import (
	"context"
	"fmt"
	"sync"
	"testing"

	"github.com/ethereum/go-ethereum/common"
)

func TestNonce_NextSequential(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)

	for i := uint64(0); i < 10; i++ {
		got := nm.Next()
		if got != i {
			t.Fatalf("Next() call %d: got %d, want %d", i, got, i)
		}
	}
}

func TestNonce_NextSequential_NonZeroStart(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(42)

	for i := uint64(0); i < 10; i++ {
		got := nm.Next()
		want := 42 + i
		if got != want {
			t.Fatalf("Next() call %d: got %d, want %d", i, got, want)
		}
	}
}

func TestNonce_NextConcurrent(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)
	const goroutines = 100

	var mu sync.Mutex
	seen := make(map[uint64]bool)
	var wg sync.WaitGroup

	wg.Add(goroutines)
	for i := 0; i < goroutines; i++ {
		go func() {
			defer wg.Done()
			nonce := nm.Next()
			mu.Lock()
			defer mu.Unlock()
			if seen[nonce] {
				t.Errorf("duplicate nonce: %d", nonce)
			}
			seen[nonce] = true
		}()
	}
	wg.Wait()

	if len(seen) != goroutines {
		t.Errorf("expected %d unique nonces, got %d", goroutines, len(seen))
	}

	// Verify all nonces are in range [0, goroutines)
	for nonce := range seen {
		if nonce >= goroutines {
			t.Errorf("nonce %d out of expected range [0, %d)", nonce, goroutines)
		}
	}
}

func TestNonce_Current(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(5)

	// Current returns 5 without incrementing
	if got := nm.Current(); got != 5 {
		t.Errorf("Current(): got %d, want 5", got)
	}

	// Call again — still 5
	if got := nm.Current(); got != 5 {
		t.Errorf("Current() second call: got %d, want 5", got)
	}

	// Next increments
	nm.Next()

	// Now current should be 6
	if got := nm.Current(); got != 6 {
		t.Errorf("Current() after Next(): got %d, want 6", got)
	}
}

func TestNonce_Sync(t *testing.T) {
	t.Parallel()

	t.Run("sync with higher nonce updates", func(t *testing.T) {
		t.Parallel()
		nm := NewNonceManager(5)

		nm.Sync(10)
		if got := nm.Current(); got != 10 {
			t.Errorf("after Sync(10): Current()=%d, want 10", got)
		}
		if got := nm.PendingCount(); got != 0 {
			t.Errorf("after Sync: PendingCount()=%d, want 0", got)
		}
	})

	t.Run("sync with lower nonce is no-op", func(t *testing.T) {
		t.Parallel()
		nm := NewNonceManager(10)

		nm.Sync(5)
		if got := nm.Current(); got != 10 {
			t.Errorf("after Sync(5): Current()=%d, want 10", got)
		}
	})

	t.Run("sync with equal nonce is no-op", func(t *testing.T) {
		t.Parallel()
		nm := NewNonceManager(7)

		nm.Sync(7)
		if got := nm.Current(); got != 7 {
			t.Errorf("after Sync(7): Current()=%d, want 7", got)
		}
	})
}

func TestNonce_Reset(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)

	// Use some nonces
	nm.Next()
	nm.Next()
	nm.Next()

	if got := nm.PendingCount(); got != 3 {
		t.Errorf("PendingCount before reset: got %d, want 3", got)
	}

	// Reset to specific value
	nm.Reset(100)

	if got := nm.Current(); got != 100 {
		t.Errorf("Current after Reset(100): got %d, want 100", got)
	}

	if got := nm.PendingCount(); got != 0 {
		t.Errorf("PendingCount after reset: got %d, want 0", got)
	}

	// Next should start from 100
	if got := nm.Next(); got != 100 {
		t.Errorf("Next after Reset(100): got %d, want 100", got)
	}
}

func TestNonce_PendingCount(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(0)

	if got := nm.PendingCount(); got != 0 {
		t.Errorf("initial PendingCount: got %d, want 0", got)
	}

	nm.Next()
	if got := nm.PendingCount(); got != 1 {
		t.Errorf("after 1 Next: PendingCount=%d, want 1", got)
	}

	nm.Next()
	if got := nm.PendingCount(); got != 2 {
		t.Errorf("after 2 Next: PendingCount=%d, want 2", got)
	}

	nm.Next()
	nm.Next()
	nm.Next()
	if got := nm.PendingCount(); got != 5 {
		t.Errorf("after 5 Next: PendingCount=%d, want 5", got)
	}
}

// --- Mock RPC tests ---

// mockNonceProvider implements NonceProvider for testing.
// Not concurrency-safe — use only in sequential test scenarios.
type mockNonceProvider struct {
	nonce uint64
	err   error
	calls int
}

func (m *mockNonceProvider) PendingNonceAt(_ context.Context, _ common.Address) (uint64, error) {
	m.calls++
	return m.nonce, m.err
}

func TestNonce_SyncFromChain_MockRPC(t *testing.T) {
	t.Parallel()

	mock := &mockNonceProvider{nonce: 42}
	addr := common.HexToAddress("0x1234567890abcdef1234567890abcdef12345678")

	nm := NewNonceManager(0)
	nm.SetSyncSource(addr, mock)

	if err := nm.SyncFromChain(context.Background()); err != nil {
		t.Fatalf("SyncFromChain: %v", err)
	}
	if mock.calls != 1 {
		t.Errorf("expected 1 RPC call, got %d", mock.calls)
	}
	if got := nm.Current(); got != 42 {
		t.Errorf("nonce after sync: got %d, want 42", got)
	}
}

func TestNonce_SyncFromChain_RPCError_KeepsNonce(t *testing.T) {
	t.Parallel()

	mock := &mockNonceProvider{err: fmt.Errorf("connection refused")}
	addr := common.HexToAddress("0x1234567890abcdef1234567890abcdef12345678")

	nm := NewNonceManager(10)
	nm.SetSyncSource(addr, mock)

	err := nm.SyncFromChain(context.Background())
	if err == nil {
		t.Fatal("expected error from SyncFromChain")
	}
	// Nonce should remain at 10.
	if got := nm.Current(); got != 10 {
		t.Errorf("nonce after failed sync: got %d, want 10", got)
	}
}

func TestNonce_SyncFromChain_NoClient(t *testing.T) {
	t.Parallel()

	nm := NewNonceManager(5)
	// No client configured — should be a no-op.
	if err := nm.SyncFromChain(context.Background()); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got := nm.Current(); got != 5 {
		t.Errorf("nonce without client: got %d, want 5", got)
	}
}

func TestNonce_SyncFromChain_HigherNonceUpdates(t *testing.T) {
	t.Parallel()

	mock := &mockNonceProvider{nonce: 100}
	addr := common.HexToAddress("0xaaaa")

	nm := NewNonceManager(50)
	nm.SetSyncSource(addr, mock)

	// Use some nonces locally.
	nm.Next() // 50
	nm.Next() // 51

	if err := nm.SyncFromChain(context.Background()); err != nil {
		t.Fatalf("SyncFromChain: %v", err)
	}
	if got := nm.Current(); got != 100 {
		t.Errorf("nonce after sync to higher: got %d, want 100", got)
	}
	if got := nm.PendingCount(); got != 0 {
		t.Errorf("pending count after sync: got %d, want 0", got)
	}
}

func TestNonce_SyncFromChain_LowerNonceIgnored(t *testing.T) {
	t.Parallel()

	mock := &mockNonceProvider{nonce: 5}
	addr := common.HexToAddress("0xbbbb")

	nm := NewNonceManager(20)
	nm.SetSyncSource(addr, mock)

	if err := nm.SyncFromChain(context.Background()); err != nil {
		t.Fatalf("SyncFromChain: %v", err)
	}
	// Lower on-chain nonce should not downgrade local nonce.
	if got := nm.Current(); got != 20 {
		t.Errorf("nonce after lower sync: got %d, want 20", got)
	}
}
