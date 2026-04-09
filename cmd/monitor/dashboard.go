package main

import (
	"bufio"
	"fmt"
	"html/template"
	"io"
	"log"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

// Dashboard serves a live HTML dashboard by scraping the Prometheus endpoints
// exposed by the Rust engine (:9092) and Go executor (:9090).
type Dashboard struct {
	rustMetricsURL string
	goMetricsURL   string
}

// NewDashboard creates a dashboard wired to the live Prometheus endpoints.
func NewDashboard(_ *Metrics) *Dashboard {
	rustPort := os.Getenv("RUST_METRICS_PORT")
	if rustPort == "" {
		rustPort = "9092"
	}
	goPort := os.Getenv("GO_METRICS_PORT")
	if goPort == "" {
		goPort = "9090"
	}
	return &Dashboard{
		rustMetricsURL: "http://127.0.0.1:" + rustPort + "/metrics",
		goMetricsURL:   "http://127.0.0.1:" + goPort + "/metrics",
	}
}

// scrapeMetric fetches a Prometheus text endpoint and returns the value of the
// named metric (first match, scalar only — counters/gauges, not histograms).
func scrapeMetric(url, name string) string {
	client := &http.Client{Timeout: 2 * time.Second}
	resp, err := client.Get(url)
	if err != nil {
		return "—"
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	scanner := bufio.NewScanner(strings.NewReader(string(body)))
	prefix := name + " "
	for scanner.Scan() {
		line := scanner.Text()
		if strings.HasPrefix(line, prefix) {
			return strings.TrimSpace(strings.TrimPrefix(line, prefix))
		}
	}
	return "—"
}

// scrapeAll fetches both endpoints once and returns a flat map of metric→value.
func (d *Dashboard) scrapeAll() map[string]string {
	urls := []struct{ url, prefix string }{
		{d.rustMetricsURL, "rust"},
		{d.goMetricsURL, "go"},
	}
	out := make(map[string]string)
	for _, u := range urls {
		client := &http.Client{Timeout: 2 * time.Second}
		resp, err := client.Get(u.url)
		if err != nil {
			continue
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		scanner := bufio.NewScanner(strings.NewReader(string(body)))
		for scanner.Scan() {
			line := scanner.Text()
			if strings.HasPrefix(line, "#") || line == "" {
				continue
			}
			parts := strings.SplitN(line, " ", 2)
			if len(parts) == 2 {
				out[parts[0]] = strings.TrimSpace(parts[1])
			}
		}
	}
	return out
}

// fmtFloat formats a metric string as a float with 4 decimal places, or returns it as-is.
func fmtFloat(s string) string {
	if s == "—" {
		return s
	}
	f, err := strconv.ParseFloat(s, 64)
	if err != nil {
		return s
	}
	if f == float64(int64(f)) {
		return fmt.Sprintf("%.0f", f)
	}
	return fmt.Sprintf("%.4f", f)
}

// ServeDashboard starts the dashboard HTTP server.
func (d *Dashboard) ServeDashboard(addr string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/", d.handleDashboard)
	mux.HandleFunc("/api/stats", d.handleStats)

	log.Printf("Dashboard server listening on %s", addr)
	return http.ListenAndServe(addr, mux)
}

func (d *Dashboard) handleDashboard(w http.ResponseWriter, r *http.Request) {
	m := d.scrapeAll()

	tmpl := `<!DOCTYPE html>
<html><head><title>Aether Dashboard</title>
<style>
*{box-sizing:border-box}
body{font-family:monospace;background:#0d1117;color:#c9d1d9;padding:24px;margin:0}
h1{color:#58a6ff;margin-bottom:4px;font-size:22px}
.sub{color:#8b949e;font-size:12px;margin-bottom:24px}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(210px,1fr));gap:12px}
.card{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:16px}
.label{font-size:11px;color:#8b949e;text-transform:uppercase;letter-spacing:.5px;margin-bottom:6px}
.value{font-size:26px;font-weight:700;color:#58a6ff}
.value.green{color:#3fb950}
.value.yellow{color:#d29922}
.value.red{color:#f85149}
.section{font-size:12px;color:#8b949e;text-transform:uppercase;letter-spacing:1px;margin:20px 0 8px}
</style>
<meta http-equiv="refresh" content="5">
</head><body>
<h1>Aether MEV Bot</h1>
<div class="sub">Live · refreshes every 5s &nbsp;|&nbsp; Rust :{{.RustPort}} &nbsp;|&nbsp; Go :{{.GoPort}}</div>

<div class="section">Pipeline</div>
<div class="grid">
  <div class="card"><div class="label">Blocks Processed</div><div class="value">{{.Blocks}}</div></div>
  <div class="card"><div class="label">Cycles Detected</div><div class="value">{{.Cycles}}</div></div>
  <div class="card"><div class="label">Simulations Run</div><div class="value">{{.Sims}}</div></div>
  <div class="card"><div class="label">Arbs Published</div><div class="value green">{{.Arbs}}</div></div>
  <div class="card"><div class="label">Bundles Submitted</div><div class="value">{{.BundlesSub}}</div></div>
  <div class="card"><div class="label">Bundles Included</div><div class="value green">{{.BundlesInc}}</div></div>
  <div class="card"><div class="label">Risk Rejections</div><div class="value yellow">{{.Rejected}}</div></div>
</div>

<div class="section">Financials</div>
<div class="grid">
  <div class="card"><div class="label">Daily PnL (ETH)</div><div class="value green">{{.DailyPnL}}</div></div>
  <div class="card"><div class="label">Gas Price (gwei)</div><div class="value">{{.GasPrice}}</div></div>
  <div class="card"><div class="label">ETH Balance</div><div class="value">{{.EthBalance}}</div></div>
</div>

<div class="section">Latency</div>
<div class="grid">
  <div class="card"><div class="label">E2E Latency (avg ms)</div><div class="value">{{.E2ELatency}}</div></div>
  <div class="card"><div class="label">Sim Latency (avg ms)</div><div class="value">{{.SimLatency}}</div></div>
</div>
</body></html>`

	// Compute averages from histogram sum/count
	e2eLatency := "—"
	if cnt := m["aether_end_to_end_latency_ms_count"]; cnt != "" && cnt != "0" {
		sum, e1 := strconv.ParseFloat(m["aether_end_to_end_latency_ms_sum"], 64)
		count, e2 := strconv.ParseFloat(cnt, 64)
		if e1 == nil && e2 == nil && count > 0 {
			e2eLatency = fmt.Sprintf("%.2f", sum/count)
		}
	}
	simLatency := "—"
	if cnt := m["aether_simulation_latency_ms_count"]; cnt != "" && cnt != "0" {
		sum, e1 := strconv.ParseFloat(m["aether_simulation_latency_ms_sum"], 64)
		count, e2 := strconv.ParseFloat(cnt, 64)
		if e1 == nil && e2 == nil && count > 0 {
			simLatency = fmt.Sprintf("%.2f", sum/count)
		}
	}

	rustPort := os.Getenv("RUST_METRICS_PORT")
	if rustPort == "" {
		rustPort = "9092"
	}
	goPort := os.Getenv("GO_METRICS_PORT")
	if goPort == "" {
		goPort = "9090"
	}

	data := map[string]string{
		"RustPort":   rustPort,
		"GoPort":     goPort,
		"Blocks":     fmtFloat(m["aether_blocks_processed_total"]),
		"Cycles":     fmtFloat(m["aether_cycles_detected_total"]),
		"Sims":       fmtFloat(m["aether_simulations_run_total"]),
		"Arbs":       fmtFloat(m["aether_arbs_published_total"]),
		"BundlesSub": fmtFloat(m["aether_executor_bundles_submitted_total"]),
		"BundlesInc": fmtFloat(m["aether_executor_bundles_included_total"]),
		"Rejected":   fmtFloat(m["aether_executor_risk_rejections_total"]),
		"DailyPnL":   fmtFloat(m["aether_daily_pnl_eth"]),
		"GasPrice":   fmtFloat(m["aether_gas_price_gwei"]),
		"EthBalance": fmtFloat(m["aether_eth_balance"]),
		"E2ELatency": e2eLatency,
		"SimLatency": simLatency,
	}

	t, _ := template.New("dashboard").Parse(tmpl)
	t.Execute(w, data)
}

func (d *Dashboard) handleStats(w http.ResponseWriter, r *http.Request) {
	m := d.scrapeAll()
	w.Header().Set("Content-Type", "application/json")
	fmt.Fprintf(w, `{"blocks":%s,"cycles":%s,"simulations":%s,"arbs_published":%s,"bundles_submitted":%s,"bundles_included":%s,"risk_rejections":%s,"daily_pnl_eth":%s,"gas_price_gwei":%s,"eth_balance":%s}`,
		orZero(m["aether_blocks_processed_total"]),
		orZero(m["aether_cycles_detected_total"]),
		orZero(m["aether_simulations_run_total"]),
		orZero(m["aether_arbs_published_total"]),
		orZero(m["aether_executor_bundles_submitted_total"]),
		orZero(m["aether_executor_bundles_included_total"]),
		orZero(m["aether_executor_risk_rejections_total"]),
		orZero(m["aether_daily_pnl_eth"]),
		orZero(m["aether_gas_price_gwei"]),
		orZero(m["aether_eth_balance"]),
	)
}

func orZero(s string) string {
	if s == "" || s == "—" {
		return "0"
	}
	return s
}
