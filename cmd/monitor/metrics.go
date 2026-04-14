package main

import (
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"sync"
	"sync/atomic"
)

// Metrics holds all Prometheus-style metrics
type Metrics struct {
	mu sync.RWMutex

	// Counters
	OpportunitiesDetected atomic.Int64
	BundlesSubmitted      atomic.Int64
	BundlesIncluded       atomic.Int64
	RevertsBug            atomic.Int64
	RevertsCompetitive    atomic.Int64

	// Gauges
	GasPriceGwei atomic.Int64 // Stored as gwei * 100 for precision
	DailyPnLWei  atomic.Int64 // Stored as signed int
	ETHBalance   atomic.Int64 // Stored as wei / 1e12 for reasonable range

	// Histograms (simplified: store last value)
	DetectionLatencyMs  atomic.Int64
	SimulationLatencyMs atomic.Int64
	EndToEndLatencyMs   atomic.Int64
}

// NewMetrics creates a new metrics instance
func NewMetrics() *Metrics {
	return &Metrics{}
}

// ServeMetrics starts an HTTP server for Prometheus scraping
func (m *Metrics) ServeMetrics(addr string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/metrics", m.handleMetrics)
	mux.HandleFunc("/health", m.handleHealth)

	slog.Info("metrics server listening", "addr", addr)
	return http.ListenAndServe(addr, mux)
}

func (m *Metrics) handleMetrics(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; version=0.0.4")

	fmt.Fprintf(w, "# HELP aether_opportunities_detected_total Total arbitrage opportunities detected\n")
	fmt.Fprintf(w, "# TYPE aether_opportunities_detected_total counter\n")
	fmt.Fprintf(w, "aether_opportunities_detected_total %d\n\n", m.OpportunitiesDetected.Load())

	fmt.Fprintf(w, "# HELP aether_bundles_submitted_total Total bundles submitted to builders\n")
	fmt.Fprintf(w, "# TYPE aether_bundles_submitted_total counter\n")
	fmt.Fprintf(w, "aether_bundles_submitted_total %d\n\n", m.BundlesSubmitted.Load())

	fmt.Fprintf(w, "# HELP aether_bundles_included_total Total bundles included on-chain\n")
	fmt.Fprintf(w, "# TYPE aether_bundles_included_total counter\n")
	fmt.Fprintf(w, "aether_bundles_included_total %d\n\n", m.BundlesIncluded.Load())

	fmt.Fprintf(w, "# HELP aether_reverts_total Total reverts by type\n")
	fmt.Fprintf(w, "# TYPE aether_reverts_total counter\n")
	fmt.Fprintf(w, "aether_reverts_total{type=\"bug\"} %d\n", m.RevertsBug.Load())
	fmt.Fprintf(w, "aether_reverts_total{type=\"competitive\"} %d\n\n", m.RevertsCompetitive.Load())

	fmt.Fprintf(w, "# HELP aether_gas_price_gwei Current gas price in gwei\n")
	fmt.Fprintf(w, "# TYPE aether_gas_price_gwei gauge\n")
	fmt.Fprintf(w, "aether_gas_price_gwei %.2f\n\n", float64(m.GasPriceGwei.Load())/100.0)

	fmt.Fprintf(w, "# HELP aether_detection_latency_ms Detection latency in milliseconds\n")
	fmt.Fprintf(w, "# TYPE aether_detection_latency_ms gauge\n")
	fmt.Fprintf(w, "aether_detection_latency_ms %d\n\n", m.DetectionLatencyMs.Load())

	fmt.Fprintf(w, "# HELP aether_simulation_latency_ms Simulation latency in milliseconds\n")
	fmt.Fprintf(w, "# TYPE aether_simulation_latency_ms gauge\n")
	fmt.Fprintf(w, "aether_simulation_latency_ms %d\n\n", m.SimulationLatencyMs.Load())

	fmt.Fprintf(w, "# HELP aether_end_to_end_latency_ms End-to-end latency in milliseconds\n")
	fmt.Fprintf(w, "# TYPE aether_end_to_end_latency_ms gauge\n")
	fmt.Fprintf(w, "aether_end_to_end_latency_ms %d\n\n", m.EndToEndLatencyMs.Load())

	fmt.Fprintf(w, "# HELP aether_eth_balance Current ETH balance\n")
	fmt.Fprintf(w, "# TYPE aether_eth_balance gauge\n")
	fmt.Fprintf(w, "aether_eth_balance %.6f\n\n", float64(m.ETHBalance.Load())/1e6)
}

func (m *Metrics) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	fmt.Fprintf(w, `{"status":"ok","opportunities":%d,"bundles_submitted":%d,"bundles_included":%d}`,
		m.OpportunitiesDetected.Load(),
		m.BundlesSubmitted.Load(),
		m.BundlesIncluded.Load())
}

func main() {
	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))

	fmt.Println("aether-monitor: metrics, dashboard, and alerting service")

	metricsPort := os.Getenv("METRICS_PORT")
	if metricsPort == "" {
		metricsPort = "9090"
	}
	dashboardPort := os.Getenv("DASHBOARD_PORT")
	if dashboardPort == "" {
		dashboardPort = "8080"
	}

	metrics := NewMetrics()
	dashboard := NewDashboard(metrics)
	alerter := NewAlerter([]AlertChannel{ChannelPagerDuty, ChannelTelegram, ChannelDiscord})

	// Start metrics server
	go func() {
		if err := metrics.ServeMetrics(":" + metricsPort); err != nil {
			slog.Error("metrics server failed", "err", err)
			os.Exit(1)
		}
	}()

	// Start dashboard
	go func() {
		if err := dashboard.ServeDashboard(":" + dashboardPort); err != nil {
			slog.Error("dashboard server failed", "err", err)
			os.Exit(1)
		}
	}()

	slog.Info("monitor service started")
	slog.Info("metrics endpoint", "url", fmt.Sprintf("http://localhost:%s/metrics", metricsPort))
	slog.Info("dashboard endpoint", "url", fmt.Sprintf("http://localhost:%s/", dashboardPort))

	// Send startup alert
	alerter.Send(SeverityInfo, "System Started", "Aether monitor service started")

	// Block forever (in production, would have signal handling)
	select {}
}
