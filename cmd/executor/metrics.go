package main

import (
	"log"
	"math/big"
	"net/http"
	"os"
	"strconv"
	"strings"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

var (
	bundlesSubmitted = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_bundles_submitted_total",
		Help: "Total bundles submitted for builder fanout",
	})
	bundlesIncluded = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_bundles_included_total",
		Help: "Total bundles with at least one builder acceptance",
	})
	profitTotalWei = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_profit_total_wei",
		Help: "Total estimated net profit for included bundles in wei",
	})
	gasSpentWei = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_gas_spent_wei_total",
		Help: "Total estimated gas spent for included bundles in wei",
	})
	riskRejections = prometheus.NewCounter(prometheus.CounterOpts{
		Name: "aether_risk_rejections_total",
		Help: "Total arbs rejected by preflight risk checks",
	})
)

func init() {
	prometheus.MustRegister(
		bundlesSubmitted,
		bundlesIncluded,
		profitTotalWei,
		gasSpentWei,
		riskRejections,
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
}

func recordRiskRejection() {
	riskRejections.Inc()
}

func addBigIntCounter(counter prometheus.Counter, value *big.Int) {
	if value == nil || value.Sign() == 0 {
		return
	}
	f, _ := new(big.Float).SetInt(value).Float64()
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
