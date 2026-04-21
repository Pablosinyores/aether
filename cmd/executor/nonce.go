package main

import (
	"context"
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/ethereum/go-ethereum/common"
)

// NonceProvider is the interface for fetching on-chain nonce.
type NonceProvider interface {
	PendingNonceAt(ctx context.Context, account common.Address) (uint64, error)
}

// NonceManager handles atomic nonce management with periodic on-chain sync
type NonceManager struct {
	current atomic.Uint64
	pending atomic.Int32 // Number of pending transactions
	address common.Address
	client  NonceProvider
}

// NewNonceManager creates a new nonce manager
func NewNonceManager(initialNonce uint64) *NonceManager {
	nm := &NonceManager{}
	nm.current.Store(initialNonce)
	return nm
}

// SetSyncSource configures the address and client for on-chain nonce sync.
func (nm *NonceManager) SetSyncSource(address common.Address, client NonceProvider) {
	nm.address = address
	nm.client = client
}

// Next atomically gets and increments the nonce
func (nm *NonceManager) Next() uint64 {
	nm.pending.Add(1)
	return nm.current.Add(1) - 1
}

// Current returns the current nonce without incrementing
func (nm *NonceManager) Current() uint64 {
	return nm.current.Load()
}

// Sync updates the nonce from on-chain state
func (nm *NonceManager) Sync(onChainNonce uint64) {
	current := nm.current.Load()
	if onChainNonce > current {
		nm.current.Store(onChainNonce)
		nm.pending.Store(0)
		slog.Info("nonce synced", "from", current, "to", onChainNonce)
	}
}

// SyncFromChain updates the nonce from pending on-chain state when configured.
func (nm *NonceManager) SyncFromChain(ctx context.Context) error {
	if nm.client == nil || nm.address == (common.Address{}) {
		return nil
	}

	onChainNonce, err := nm.client.PendingNonceAt(ctx, nm.address)
	if err != nil {
		return err
	}

	nm.Sync(onChainNonce)
	return nil
}

// Reset forces the nonce to a specific value
func (nm *NonceManager) Reset(nonce uint64) {
	nm.current.Store(nonce)
	nm.pending.Store(0)
}

// PendingCount returns the number of pending transactions
func (nm *NonceManager) PendingCount() int32 {
	return nm.pending.Load()
}

// SyncLoop periodically syncs nonce from on-chain state when configured.
func (nm *NonceManager) SyncLoop(ctx context.Context, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			if nm.client == nil || nm.address == (common.Address{}) {
				slog.Debug("nonce sync check", "current", nm.Current(), "pending", nm.PendingCount())
				continue
			}

			if err := nm.SyncFromChain(ctx); err != nil {
				slog.Error("nonce sync failed", "err", err)
			}
		}
	}
}
