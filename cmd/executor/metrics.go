package main

import (
	"context"
	"log"
	"math"
	"math/big"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

// Naming convention:
//   aether_executor_* — executor-process-specific counters (bundle ops, risk)
//   aether_*          — system-level spec metrics shared across processes
//                       (latency, gas price, PnL, ETH balance)
var (
	bundlesSubmitted = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_executor_bundles_submitted_total",
		Help: "Total bundles submitted for builder fanout",
	})
	bundlesIncluded = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_executor_bundles_included_total",
		Help: "Total bundles with at least one builder acceptance",
	})
	profitTotalWei = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_executor_profit_wei_total",
		Help: "Total estimated net profit for included bundles in wei",
	})
	gasSpentWei = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_executor_gas_spent_wei_total",
		Help: "Total estimated gas spent for included bundles in wei",
	})
	riskRejections = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_executor_risk_rejections_total",
		Help: "Total arbs rejected by preflight risk checks",
	})
	endToEndLatencyMs = prometheus.NewHistogram(prometheus.HistogramOpts{
		Name:    "aether_end_to_end_latency_ms",
		Help:    "End-to-end latency from arb detection to bundle submission in ms",
		Buckets: []float64{10, 50, 75, 100, 250, 500, 1000, 2000, 5000},
	})
	gasPriceGwei = prometheus.NewGauge(prometheus.GaugeOpts{
		Name: "aether_gas_price_gwei",
		Help: "Current gas oracle base fee reading in gwei",
	})
	dailyPnlEth = prometheus.NewGauge(prometheus.GaugeOpts{
		Name: "aether_daily_pnl_eth",
		Help: "Cumulative daily profit minus gas costs in ETH, resets at UTC midnight",
	})
	ethBalanceGauge = prometheus.NewGauge(prometheus.GaugeOpts{
		Name: "aether_eth_balance",
		Help: "Current ETH balance of the searcher wallet",
	})
)

func init() {
	prometheus.MustRegister(
		bundlesSubmitted,
		bundlesIncluded,
		profitTotalWei,
		gasSpentWei,
		riskRejections,
		endToEndLatencyMs,
		gasPriceGwei,
		dailyPnlEth,
		ethBalanceGauge,
	)
}

func startMetricsServer() {
	addr := metricsAddr()
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.Handler())

	go func() {
		log.Printf("Metrics server listening on %s", addr)
		if err := http.ListenAndServe(addr, mux); err != nil && err != http.ErrServerClosed {
			log.Printf("Metrics server error: %v", err)
		}
	}()
}

func metricsAddr() string {
	port := strings.TrimSpace(os.Getenv("METRICS_PORT"))
	if port == "" {
		port = "9090"
	}
	if strings.HasPrefix(port, ":") {
		return port
	}
	if _, err := strconv.Atoi(port); err == nil {
		return ":" + port
	}
	return port
}

func recordBundleSubmitted() {
	bundlesSubmitted.Inc()
}

func recordBundleIncluded(profitWei *big.Int, gasGwei float64, gasUsed uint64) {
	bundlesIncluded.Inc()
	addBigIntCounter(profitTotalWei, profitWei)
	addGasSpent(gasGwei, gasUsed)
	gasCostWei := gasGwei * 1e9 * float64(gasUsed)
	addPnl(profitWei, gasCostWei)
}

func recordRiskRejection() {
	riskRejections.Inc()
}

func addBigIntCounter(counter prometheus.Counter, value *big.Int) {
	if value == nil || value.Sign() == 0 {
		return
	}
	f, accuracy := new(big.Float).SetInt(value).Float64()
	if accuracy != big.Exact {
		log.Printf("Metrics precision loss: %s truncated to %.0f", value.String(), f)
	}
	if f == 0 {
		return
	}
	counter.Add(f)
}

func addGasSpent(gasGwei float64, gasUsed uint64) {
	if gasGwei <= 0 || gasUsed == 0 {
		return
	}
	gasWei := gasGwei * 1e9
	gasSpent := gasWei * float64(gasUsed)
	gasSpentWei.Add(gasSpent)
}

// --- End-to-end latency ---

// recordEndToEndLatency observes the time elapsed since receivedAt (the
// Go-side wall clock stamped when the arb arrived from the gRPC stream).
// Using a Go-side timestamp avoids cross-process clock skew that would
// corrupt measurements against the p99 > 100ms alert threshold.
func recordEndToEndLatency(receivedAt time.Time) {
	if receivedAt.IsZero() {
		return
	}
	latencyMs := float64(time.Since(receivedAt).Nanoseconds()) / 1e6
	if latencyMs >= 0 {
		endToEndLatencyMs.Observe(latencyMs)
	}
}

// --- Gas price gauge ---

func recordGasPrice(gwei float64) {
	gasPriceGwei.Set(gwei)
}

// --- Daily PnL tracker ---

var (
	pnlMu  sync.Mutex
	pnlWei = new(big.Int)
	pnlDay time.Time
)

func addPnl(profitWei *big.Int, gasCostWei float64) {
	pnlMu.Lock()
	defer pnlMu.Unlock()

	today := time.Now().UTC().Truncate(24 * time.Hour)
	if !today.Equal(pnlDay) {
		pnlWei.SetInt64(0)
		pnlDay = today
	}

	if profitWei != nil {
		pnlWei.Add(pnlWei, profitWei)
	}
	if gasCostWei > 0 && !math.IsNaN(gasCostWei) {
		gasCost := new(big.Int).SetUint64(uint64(gasCostWei))
		pnlWei.Sub(pnlWei, gasCost)
	}

	ethVal, _ := new(big.Float).Quo(
		new(big.Float).SetInt(pnlWei),
		new(big.Float).SetFloat64(1e18),
	).Float64()
	dailyPnlEth.Set(ethVal)
}

// --- ETH balance watcher ---

func balanceWatchLoop(ctx context.Context, client *ethclient.Client, addr common.Address, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			fetchCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
			bal, err := client.BalanceAt(fetchCtx, addr, nil)
			cancel()
			if err != nil {
				log.Printf("WARNING: eth_getBalance failed: %v", err)
				continue
			}
			ethVal, _ := new(big.Float).Quo(
				new(big.Float).SetInt(bal),
				new(big.Float).SetFloat64(1e18),
			).Float64()
			ethBalanceGauge.Set(ethVal)
		}
	}
}
