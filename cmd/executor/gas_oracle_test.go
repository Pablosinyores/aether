package main

import (
	"context"
	"fmt"
	"math/big"
	"sync"
	"testing"

	"github.com/ethereum/go-ethereum"
)

func TestNewGasOracle_Defaults(t *testing.T) {
	t.Parallel()

	go_ := NewGasOracle(300.0)
	fees := go_.CurrentFees()

	// Default baseFee = 30 gwei
	expectedBaseFee := big.NewInt(30e9)
	if fees.BaseFee.Cmp(expectedBaseFee) != 0 {
		t.Errorf("default BaseFee: got %s, want %s", fees.BaseFee.String(), expectedBaseFee.String())
	}

	// Default priorityFee = 2 gwei
	expectedPriority := big.NewInt(2e9)
	if fees.MaxPriorityFee.Cmp(expectedPriority) != 0 {
		t.Errorf("default MaxPriorityFee: got %s, want %s", fees.MaxPriorityFee.String(), expectedPriority.String())
	}

	// Default maxFee = 2 * 30 gwei + 2 gwei = 62 gwei
	expectedMaxFee := big.NewInt(62e9)
	if fees.MaxFeePerGas.Cmp(expectedMaxFee) != 0 {
		t.Errorf("default MaxFeePerGas: got %s, want %s", fees.MaxFeePerGas.String(), expectedMaxFee.String())
	}

	// Default GasPriceGwei
	if fees.GasPriceGwei != 30.0 {
		t.Errorf("default GasPriceGwei: got %f, want 30.0", fees.GasPriceGwei)
	}
}

func TestGasOracle_Update(t *testing.T) {
	t.Parallel()

	go_ := NewGasOracle(300.0)

	newBaseFee := big.NewInt(50e9)   // 50 gwei
	newPriority := big.NewInt(5e9)   // 5 gwei
	go_.Update(newBaseFee, newPriority)

	fees := go_.CurrentFees()

	if fees.BaseFee.Cmp(newBaseFee) != 0 {
		t.Errorf("updated BaseFee: got %s, want %s", fees.BaseFee.String(), newBaseFee.String())
	}

	if fees.MaxPriorityFee.Cmp(newPriority) != 0 {
		t.Errorf("updated MaxPriorityFee: got %s, want %s", fees.MaxPriorityFee.String(), newPriority.String())
	}

	// maxFee = 2*50 + 5 = 105 gwei
	expectedMaxFee := big.NewInt(105e9)
	if fees.MaxFeePerGas.Cmp(expectedMaxFee) != 0 {
		t.Errorf("updated MaxFeePerGas: got %s, want %s", fees.MaxFeePerGas.String(), expectedMaxFee.String())
	}

	// GasPriceGwei should reflect baseFee in gwei
	if fees.GasPriceGwei != 50.0 {
		t.Errorf("updated GasPriceGwei: got %f, want 50.0", fees.GasPriceGwei)
	}
}

func TestGasOracle_CurrentFees_ThreadSafe(t *testing.T) {
	t.Parallel()

	go_ := NewGasOracle(300.0)

	var wg sync.WaitGroup
	const readers = 50

	// Start concurrent readers
	wg.Add(readers)
	for i := 0; i < readers; i++ {
		go func() {
			defer wg.Done()
			for j := 0; j < 100; j++ {
				fees := go_.CurrentFees()
				// Verify returned fees are valid (non-nil)
				if fees.BaseFee == nil {
					t.Error("BaseFee is nil during concurrent read")
				}
				if fees.MaxFeePerGas == nil {
					t.Error("MaxFeePerGas is nil during concurrent read")
				}
				if fees.MaxPriorityFee == nil {
					t.Error("MaxPriorityFee is nil during concurrent read")
				}
			}
		}()
	}

	// Start a concurrent writer
	wg.Add(1)
	go func() {
		defer wg.Done()
		for j := 0; j < 100; j++ {
			go_.Update(big.NewInt(int64(j)*1e9), big.NewInt(1e9))
		}
	}()

	wg.Wait()
	// No panics = pass
}

func TestGasOracle_IsGasTooHigh(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name       string
		maxGwei    float64
		baseFeeGwei int64
		want       bool
	}{
		{"below threshold", 300, 200, false},
		{"above threshold", 300, 350, true},
		{"at threshold", 300, 300, false},      // GasPriceGwei == maxGasGwei → not strictly greater
		{"way above", 100, 500, true},
		{"just below", 300, 299, false},
		{"just above", 300, 301, true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			go_ := NewGasOracle(tc.maxGwei)
			// Update with the test baseFee (the GasPriceGwei is derived from baseFee)
			go_.Update(big.NewInt(tc.baseFeeGwei*1e9), big.NewInt(2e9))

			got := go_.IsGasTooHigh()
			if got != tc.want {
				fees := go_.CurrentFees()
				t.Errorf("IsGasTooHigh(): got %v, want %v (gasPriceGwei=%.1f, max=%.1f)",
					got, tc.want, fees.GasPriceGwei, tc.maxGwei)
			}
		})
	}
}

func TestGasOracle_MaxFeeFormula(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name           string
		baseFeeGwei    int64
		priorityGwei   int64
		expectedMaxGwei int64
	}{
		{"30+2", 30, 2, 62},     // 2*30 + 2
		{"50+5", 50, 5, 105},    // 2*50 + 5
		{"100+10", 100, 10, 210}, // 2*100 + 10
		{"1+1", 1, 1, 3},        // 2*1 + 1
		{"0+0", 0, 0, 0},        // 2*0 + 0
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			go_ := NewGasOracle(300.0)
			go_.Update(big.NewInt(tc.baseFeeGwei*1e9), big.NewInt(tc.priorityGwei*1e9))

			fees := go_.CurrentFees()
			expectedMaxFee := big.NewInt(tc.expectedMaxGwei * 1e9)

			if fees.MaxFeePerGas.Cmp(expectedMaxFee) != 0 {
				t.Errorf("MaxFeePerGas: got %s, want %s",
					fees.MaxFeePerGas.String(), expectedMaxFee.String())
			}
		})
	}
}

// --- Mock RPC tests ---

// mockFeeHistoryProvider implements FeeHistoryProvider for testing.
type mockFeeHistoryProvider struct {
	result *ethereum.FeeHistory
	err    error
	calls  int
}

func (m *mockFeeHistoryProvider) FeeHistory(_ context.Context, _ uint64, _ *big.Int, _ []float64) (*ethereum.FeeHistory, error) {
	m.calls++
	return m.result, m.err
}

func TestGasOracle_FetchOnce_RealFees(t *testing.T) {
	t.Parallel()

	mock := &mockFeeHistoryProvider{
		result: &ethereum.FeeHistory{
			OldestBlock:  big.NewInt(100),
			BaseFee:      []*big.Int{big.NewInt(45e9)},        // 45 gwei
			Reward:       [][]*big.Int{{big.NewInt(3e9)}},      // 3 gwei priority
			GasUsedRatio: []float64{0.5},
		},
	}

	go_ := NewGasOracle(300.0)
	go_.SetClient(mock)

	fees, err := go_.FetchOnce(context.Background())
	if err != nil {
		t.Fatalf("FetchOnce returned error: %v", err)
	}
	if mock.calls != 1 {
		t.Errorf("expected 1 RPC call, got %d", mock.calls)
	}

	// baseFee should be 45 gwei
	if fees.BaseFee.Cmp(big.NewInt(45e9)) != 0 {
		t.Errorf("BaseFee: got %s, want 45000000000", fees.BaseFee)
	}
	// priorityFee should be 3 gwei
	if fees.MaxPriorityFee.Cmp(big.NewInt(3e9)) != 0 {
		t.Errorf("MaxPriorityFee: got %s, want 3000000000", fees.MaxPriorityFee)
	}
	// maxFee = 2*45 + 3 = 93 gwei
	if fees.MaxFeePerGas.Cmp(big.NewInt(93e9)) != 0 {
		t.Errorf("MaxFeePerGas: got %s, want 93000000000", fees.MaxFeePerGas)
	}
	// GasPriceGwei should be 45
	if fees.GasPriceGwei != 45.0 {
		t.Errorf("GasPriceGwei: got %f, want 45.0", fees.GasPriceGwei)
	}
}

func TestGasOracle_FetchOnce_RPCError_KeepsLast(t *testing.T) {
	t.Parallel()

	mock := &mockFeeHistoryProvider{
		err: fmt.Errorf("connection refused"),
	}

	go_ := NewGasOracle(300.0)
	// Set a known state first.
	go_.Update(big.NewInt(50e9), big.NewInt(5e9))
	go_.SetClient(mock)

	fees, err := go_.FetchOnce(context.Background())
	if err == nil {
		t.Fatal("expected error from FetchOnce")
	}
	// Fees should remain at 50 gwei / 5 gwei from the prior Update.
	if fees.BaseFee.Cmp(big.NewInt(50e9)) != 0 {
		t.Errorf("BaseFee after error: got %s, want 50000000000", fees.BaseFee)
	}
	if fees.MaxPriorityFee.Cmp(big.NewInt(5e9)) != 0 {
		t.Errorf("MaxPriorityFee after error: got %s, want 5000000000", fees.MaxPriorityFee)
	}
}

func TestGasOracle_FetchOnce_NoClient(t *testing.T) {
	t.Parallel()

	go_ := NewGasOracle(300.0)
	// No client set — should return defaults without error.
	fees, err := go_.FetchOnce(context.Background())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fees.BaseFee.Cmp(big.NewInt(30e9)) != 0 {
		t.Errorf("BaseFee without client: got %s, want 30000000000", fees.BaseFee)
	}
}

func TestGasOracle_IsGasTooHigh_WithRealData(t *testing.T) {
	t.Parallel()

	// Simulate a high gas environment: 350 gwei base fee.
	mock := &mockFeeHistoryProvider{
		result: &ethereum.FeeHistory{
			OldestBlock:  big.NewInt(200),
			BaseFee:      []*big.Int{big.NewInt(350e9)},
			Reward:       [][]*big.Int{{big.NewInt(2e9)}},
			GasUsedRatio: []float64{0.9},
		},
	}

	go_ := NewGasOracle(300.0)
	go_.SetClient(mock)

	if _, err := go_.FetchOnce(context.Background()); err != nil {
		t.Fatalf("FetchOnce: %v", err)
	}

	if !go_.IsGasTooHigh() {
		t.Error("expected IsGasTooHigh()=true with 350 gwei base fee and 300 gwei threshold")
	}
}

func TestGasOracle_FetchOnce_MultipleBaseFees(t *testing.T) {
	t.Parallel()

	// eth_feeHistory may return baseFee for N+1 blocks (pending included).
	// We take the last entry as it represents the pending block.
	mock := &mockFeeHistoryProvider{
		result: &ethereum.FeeHistory{
			OldestBlock:  big.NewInt(100),
			BaseFee:      []*big.Int{big.NewInt(40e9), big.NewInt(42e9)},
			Reward:       [][]*big.Int{{big.NewInt(1e9)}},
			GasUsedRatio: []float64{0.6},
		},
	}

	go_ := NewGasOracle(300.0)
	go_.SetClient(mock)

	fees, err := go_.FetchOnce(context.Background())
	if err != nil {
		t.Fatalf("FetchOnce: %v", err)
	}
	// Should use the last baseFee entry (42 gwei = pending block).
	if fees.BaseFee.Cmp(big.NewInt(42e9)) != 0 {
		t.Errorf("BaseFee: got %s, want 42000000000", fees.BaseFee)
	}
}
