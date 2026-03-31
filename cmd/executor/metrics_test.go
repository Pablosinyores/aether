package main

import (
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

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
		"bundles_submitted",
		"bundles_included",
		"profit_total_wei",
		"gas_spent_wei",
		"risk_rejections",
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
