package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"html/template"
	"io"
	"log/slog"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"
)

// dashboardTmpl is the HTML template for the dashboard, parsed once at init.
const dashboardTmpl = `<!DOCTYPE html>
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
  <div class="card"><div class="label">Detection (avg ms)</div><div class="value">{{.DetectLatency}}</div></div>
  <div class="card"><div class="label">Simulation (avg ms)</div><div class="value">{{.SimLatency}}</div></div>
  <div class="card"><div class="label">Executor (avg ms)</div><div class="value">{{.E2ELatency}}</div></div>
  <div class="card"><div class="label">Total Pipeline (avg ms)</div><div class="value green">{{.TotalLatency}}</div></div>
</div>
</body></html>`

// maxMetricsBody caps the response size when scraping Prometheus endpoints.
const maxMetricsBody = 10 << 20 // 10 MB

// Dashboard serves a live HTML dashboard by scraping the Prometheus endpoints
// exposed by the Rust engine (:9092) and Go executor (:9090).
type Dashboard struct {
	rustMetricsURL string
	goMetricsURL   string
	rustPort       string
	goPort         string
	httpClient     *http.Client
	tmpl           *template.Template
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
		rustPort:       rustPort,
		goPort:         goPort,
		httpClient:     &http.Client{Timeout: 2 * time.Second},
		tmpl:           template.Must(template.New("dashboard").Parse(dashboardTmpl)),
	}
}

// scrapeAll fetches both endpoints once and returns a flat map of metric->value.
func (d *Dashboard) scrapeAll() map[string]string {
	urls := []string{d.rustMetricsURL, d.goMetricsURL}
	out := make(map[string]string)
	for _, u := range urls {
		resp, err := d.httpClient.Get(u)
		if err != nil {
			continue
		}
		body, _ := io.ReadAll(io.LimitReader(resp.Body, maxMetricsBody))
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
	if s == "" || s == "\u2014" {
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

// parseOrZero parses a Prometheus metric value as a float64, returning 0 on failure.
func parseOrZero(s string) float64 {
	f, err := strconv.ParseFloat(s, 64)
	if err != nil {
		return 0
	}
	return f
}

// histogramAvg computes the average from a Prometheus histogram's _sum and _count.
// Returns -1 if the metric is missing or has zero count.
func histogramAvg(m map[string]string, name string) float64 {
	cnt := m[name+"_count"]
	if cnt == "" || cnt == "0" {
		return -1
	}
	sum, e1 := strconv.ParseFloat(m[name+"_sum"], 64)
	count, e2 := strconv.ParseFloat(cnt, 64)
	if e1 != nil || e2 != nil || count <= 0 {
		return -1
	}
	return sum / count
}

// fmtAvg formats a histogram average as a string, or returns "—" if negative (missing).
func fmtAvg(v float64) string {
	if v < 0 {
		return "\u2014"
	}
	return fmt.Sprintf("%.2f", v)
}

// ServeDashboard starts the dashboard HTTP server.
func (d *Dashboard) ServeDashboard(addr string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/", d.handleDashboard)
	mux.HandleFunc("/api/stats", d.handleStats)

	slog.Info("dashboard server listening", "addr", addr)
	return http.ListenAndServe(addr, mux)
}

func (d *Dashboard) handleDashboard(w http.ResponseWriter, r *http.Request) {
	m := d.scrapeAll()

	// Compute averages from histogram sum/count
	detectAvg := histogramAvg(m, "aether_detection_latency_ms")
	simAvg := histogramAvg(m, "aether_simulation_latency_ms")
	e2eAvg := histogramAvg(m, "aether_end_to_end_latency_ms")

	// Total pipeline = detection + simulation + executor processing
	totalLatency := "\u2014"
	if detectAvg >= 0 && simAvg >= 0 && e2eAvg >= 0 {
		totalLatency = fmt.Sprintf("%.2f", detectAvg+simAvg+e2eAvg)
	}

	data := map[string]string{
		"RustPort":      d.rustPort,
		"GoPort":        d.goPort,
		"Blocks":        fmtFloat(m["aether_blocks_processed_total"]),
		"Cycles":        fmtFloat(m["aether_cycles_detected_total"]),
		"Sims":          fmtFloat(m["aether_simulations_run_total"]),
		"Arbs":          fmtFloat(m["aether_arbs_published_total"]),
		"BundlesSub":    fmtFloat(m["aether_executor_bundles_submitted_total"]),
		"BundlesInc":    fmtFloat(m["aether_executor_bundles_included_total"]),
		"Rejected":      fmtFloat(m["aether_executor_risk_rejections_total"]),
		"DailyPnL":      fmtFloat(m["aether_daily_pnl_eth"]),
		"GasPrice":      fmtFloat(m["aether_gas_price_gwei"]),
		"EthBalance":    fmtFloat(m["aether_eth_balance"]),
		"DetectLatency": fmtAvg(detectAvg),
		"SimLatency":    fmtAvg(simAvg),
		"E2ELatency":    fmtAvg(e2eAvg),
		"TotalLatency":  totalLatency,
	}

	d.tmpl.Execute(w, data)
}

func (d *Dashboard) handleStats(w http.ResponseWriter, r *http.Request) {
	m := d.scrapeAll()
	data := map[string]float64{
		"blocks":            parseOrZero(m["aether_blocks_processed_total"]),
		"cycles":            parseOrZero(m["aether_cycles_detected_total"]),
		"simulations":       parseOrZero(m["aether_simulations_run_total"]),
		"arbs_published":    parseOrZero(m["aether_arbs_published_total"]),
		"bundles_submitted": parseOrZero(m["aether_executor_bundles_submitted_total"]),
		"bundles_included":  parseOrZero(m["aether_executor_bundles_included_total"]),
		"risk_rejections":   parseOrZero(m["aether_executor_risk_rejections_total"]),
		"daily_pnl_eth":     parseOrZero(m["aether_daily_pnl_eth"]),
		"gas_price_gwei":    parseOrZero(m["aether_gas_price_gwei"]),
		"eth_balance":       parseOrZero(m["aether_eth_balance"]),
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(data)
}
