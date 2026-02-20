package main

import (
	"context"
	"log"
	"math/big"
	"sync"
	"time"
)

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

// UpdateLoop periodically fetches gas prices (simulated here)
func (go_ *GasOracle) UpdateLoop(ctx context.Context, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			// In production: eth_gasPrice or eth_feeHistory
			fees := go_.CurrentFees()
			log.Printf("Gas oracle: baseFee=%s gwei, maxFee=%s gwei",
				new(big.Int).Div(fees.BaseFee, big.NewInt(1e9)).String(),
				new(big.Int).Div(fees.MaxFeePerGas, big.NewInt(1e9)).String())
		}
	}
}
