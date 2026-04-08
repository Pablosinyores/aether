package main

import (
	"io"
	"math"
	"math/big"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/prometheus/client_golang/prometheus/promhttp"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

func TestMetricsEndpoint_ContainsRequiredMetrics(t *testing.T) {
	server := httptest.NewServer(promhttp.Handler())
	defer server.Close()

	resp, err := http.Get(server.URL)
	if err != nil {
		t.Fatalf("metrics endpoint request failed: %v", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Fatalf("metrics endpoint status: got %d, want 200", resp.StatusCode)
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("metrics endpoint read failed: %v", err)
	}

	required := []string{
		"aether_executor_bundles_submitted_total",
		"aether_executor_bundles_included_total",
		"aether_executor_profit_wei_total",
		"aether_executor_gas_spent_wei_total",
		"aether_executor_risk_rejections_total",
		"aether_end_to_end_latency_ms",
		"aether_gas_price_gwei",
		"aether_daily_pnl_eth",
		"aether_eth_balance",
	}

	payload := string(body)
	for _, name := range required {
		if !strings.Contains(payload, name) {
			t.Fatalf("metrics output missing %q", name)
		}
	}
}

func TestMetricsCounters_Increment(t *testing.T) {
	baseSubmitted := testutil.ToFloat64(bundlesSubmitted)
	baseIncluded := testutil.ToFloat64(bundlesIncluded)
	baseProfit := testutil.ToFloat64(profitTotalWei)
	baseGas := testutil.ToFloat64(gasSpentWei)
	baseRisk := testutil.ToFloat64(riskRejections)

	recordBundleSubmitted()
	recordBundleIncluded(big.NewInt(200), 30.0, 21000)
	recordRiskRejection()

	gotSubmitted := testutil.ToFloat64(bundlesSubmitted)
	if gotSubmitted != baseSubmitted+1 {
		t.Fatalf("bundles_submitted: got %.0f, want %.0f", gotSubmitted, baseSubmitted+1)
	}

	gotIncluded := testutil.ToFloat64(bundlesIncluded)
	if gotIncluded != baseIncluded+1 {
		t.Fatalf("bundles_included: got %.0f, want %.0f", gotIncluded, baseIncluded+1)
	}

	gotProfit := testutil.ToFloat64(profitTotalWei)
	if gotProfit != baseProfit+200 {
		t.Fatalf("profit_total_wei: got %.0f, want %.0f", gotProfit, baseProfit+200)
	}

	expectedGas := 30.0 * 1e9 * 21000
	gotGas := testutil.ToFloat64(gasSpentWei)
	if gotGas < baseGas+expectedGas-1 || gotGas > baseGas+expectedGas+1 {
		t.Fatalf("gas_spent_wei: got %.0f, want %.0f", gotGas, baseGas+expectedGas)
	}

	gotRisk := testutil.ToFloat64(riskRejections)
	if gotRisk != baseRisk+1 {
		t.Fatalf("risk_rejections: got %.0f, want %.0f", gotRisk, baseRisk+1)
	}
}

func TestGasPriceGauge(t *testing.T) {
	recordGasPrice(42.5)
	got := testutil.ToFloat64(gasPriceGwei)
	if got != 42.5 {
		t.Fatalf("gas_price_gwei: got %f, want 42.5", got)
	}
}

func TestDailyPnl(t *testing.T) {
	// Reset PnL state
	pnlMu.Lock()
	pnlWei.SetInt64(0)
	pnlDay = time.Now().UTC().Truncate(24 * time.Hour)
	pnlMu.Unlock()

	// Profit of 0.01 ETH, gas cost of 0.001 ETH
	profit := new(big.Int).Mul(big.NewInt(10), new(big.Int).SetUint64(1e15)) // 0.01 ETH
	gasCost := float64(1e15)                                                 // 0.001 ETH
	addPnl(profit, gasCost)

	got := testutil.ToFloat64(dailyPnlEth)
	// 0.01 - 0.001 = 0.009 ETH exactly
	const want = 0.009
	if math.Abs(got-want) > 1e-12 {
		t.Fatalf("daily_pnl_eth: got %f, want %f", got, want)
	}
}

func TestEndToEndLatency(t *testing.T) {
	// Record a latency from 100ms ago — just verify no panic
	detectedAt := time.Now().Add(-100 * time.Millisecond).UnixNano()
	recordEndToEndLatency(detectedAt)

	// Zero timestamp should be a no-op
	recordEndToEndLatency(0)
}
