package main

import (
	"context"
	"log"
	"math/big"
	"sync"
	"time"

	"github.com/ethereum/go-ethereum"
)

// weiToGwei converts a wei value to gwei as a float64.
func weiToGwei(wei *big.Int) float64 {
	return float64(wei.Int64()) / 1e9
}

// FeeHistoryProvider is the interface for fetching on-chain fee history.
type FeeHistoryProvider interface {
	FeeHistory(ctx context.Context, blockCount uint64, lastBlock *big.Int, rewardPercentiles []float64) (*ethereum.FeeHistory, error)
}

// GasFees represents current EIP-1559 gas parameters
type GasFees struct {
	BaseFee        *big.Int
	MaxFeePerGas   *big.Int
	MaxPriorityFee *big.Int
	GasPriceGwei   float64
}

// GasOracle predicts EIP-1559 gas prices
type GasOracle struct {
	mu          sync.RWMutex
	currentFees GasFees
	maxGasGwei  float64
	client      FeeHistoryProvider
}

// NewGasOracle creates a new gas oracle
func NewGasOracle(maxGasGwei float64) *GasOracle {
	defaultBaseFee := big.NewInt(30e9) // 30 gwei
	defaultPriority := big.NewInt(2e9) // 2 gwei
	maxFee := new(big.Int).Add(
		new(big.Int).Mul(defaultBaseFee, big.NewInt(2)),
		defaultPriority,
	)

	return &GasOracle{
		currentFees: GasFees{
			BaseFee:        defaultBaseFee,
			MaxFeePerGas:   maxFee,
			MaxPriorityFee: defaultPriority,
			GasPriceGwei:   30.0,
		},
		maxGasGwei: maxGasGwei,
	}
}

// SetClient configures the Ethereum client for on-chain fee history queries.
func (go_ *GasOracle) SetClient(client FeeHistoryProvider) {
	go_.mu.Lock()
	defer go_.mu.Unlock()
	go_.client = client
}

// CurrentFees returns current gas fee estimates (thread-safe)
func (go_ *GasOracle) CurrentFees() GasFees {
	go_.mu.RLock()
	defer go_.mu.RUnlock()
	return GasFees{
		BaseFee:        new(big.Int).Set(go_.currentFees.BaseFee),
		MaxFeePerGas:   new(big.Int).Set(go_.currentFees.MaxFeePerGas),
		MaxPriorityFee: new(big.Int).Set(go_.currentFees.MaxPriorityFee),
		GasPriceGwei:   go_.currentFees.GasPriceGwei,
	}
}

// Update sets new gas fee estimates
func (go_ *GasOracle) Update(baseFee *big.Int, priorityFee *big.Int) {
	go_.mu.Lock()
	defer go_.mu.Unlock()

	// maxFeePerGas = 2 * baseFee + priorityFee (accounts for 1 block base fee increase)
	maxFee := new(big.Int).Mul(baseFee, big.NewInt(2))
	maxFee.Add(maxFee, priorityFee)

	go_.currentFees = GasFees{
		BaseFee:        new(big.Int).Set(baseFee),
		MaxFeePerGas:   maxFee,
		MaxPriorityFee: new(big.Int).Set(priorityFee),
		GasPriceGwei:   float64(baseFee.Int64()) / 1e9,
	}
}

// IsGasTooHigh checks if current gas exceeds the circuit breaker threshold
func (go_ *GasOracle) IsGasTooHigh() bool {
	go_.mu.RLock()
	defer go_.mu.RUnlock()
	return go_.currentFees.GasPriceGwei > go_.maxGasGwei
}

// FetchOnce queries eth_feeHistory for the latest base fee and priority fee,
// updates internal state, and returns the new fees. If the RPC call fails,
// the last known fees are kept and the error is returned.
func (go_ *GasOracle) FetchOnce(ctx context.Context) (GasFees, error) {
	go_.mu.RLock()
	client := go_.client
	go_.mu.RUnlock()

	if client == nil {
		return go_.CurrentFees(), nil
	}

	// Request 1 block of fee history with 50th-percentile reward (priority fee).
	feeHistory, err := client.FeeHistory(ctx, 1, nil, []float64{50.0})
	if err != nil {
		return go_.CurrentFees(), err
	}

	// BaseFee: use the latest entry (index len-1 covers the pending block).
	var baseFee *big.Int
	if len(feeHistory.BaseFee) > 0 {
		baseFee = feeHistory.BaseFee[len(feeHistory.BaseFee)-1]
	}
	if baseFee == nil || baseFee.Sign() == 0 {
		// Fallback: keep current value.
		return go_.CurrentFees(), nil
	}

	// Priority fee: 50th-percentile reward from the most recent block.
	priorityFee := big.NewInt(2e9) // 2 gwei default
	if len(feeHistory.Reward) > 0 && len(feeHistory.Reward[0]) > 0 {
		if r := feeHistory.Reward[0][0]; r != nil && r.Sign() > 0 {
			priorityFee = r
		}
	}

	go_.Update(baseFee, priorityFee)
	return go_.CurrentFees(), nil
}

// UpdateLoop periodically fetches gas prices from eth_feeHistory.
// If no client is configured or the RPC call fails, last known values are kept.
func (go_ *GasOracle) UpdateLoop(ctx context.Context, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			fees, err := go_.FetchOnce(ctx)
			if err != nil {
				log.Printf("Gas oracle: RPC error (keeping last known): %v", err)
			}
			log.Printf("Gas oracle: baseFee=%.4f gwei, maxFee=%.4f gwei, priorityFee=%.4f gwei",
				weiToGwei(fees.BaseFee),
				weiToGwei(fees.MaxFeePerGas),
				weiToGwei(fees.MaxPriorityFee))
		}
	}
}
