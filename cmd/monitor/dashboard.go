package main

import (
	"fmt"
	"html/template"
	"log"
	"net/http"
)

// Dashboard serves a simple HTML dashboard
type Dashboard struct {
	metrics *Metrics
}

// NewDashboard creates a new dashboard
func NewDashboard(metrics *Metrics) *Dashboard {
	return &Dashboard{metrics: metrics}
}

// ServeDashboard starts the dashboard HTTP server
func (d *Dashboard) ServeDashboard(addr string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/", d.handleDashboard)
	mux.HandleFunc("/api/stats", d.handleStats)

	log.Printf("Dashboard server listening on %s", addr)
	return http.ListenAndServe(addr, mux)
}

func (d *Dashboard) handleDashboard(w http.ResponseWriter, r *http.Request) {
	tmpl := `<!DOCTYPE html>
<html><head><title>Aether Dashboard</title>
<style>body{font-family:monospace;background:#1a1a2e;color:#e0e0e0;padding:20px}
.metric{background:#16213e;padding:15px;margin:10px;border-radius:8px;display:inline-block;min-width:200px}
.value{font-size:24px;color:#0f3460;font-weight:bold}
h1{color:#e94560}</style>
<meta http-equiv="refresh" content="5">
</head><body>
<h1>Aether MEV Bot Dashboard</h1>
<div class="metric"><div>Opportunities Detected</div><div class="value">{{.Opportunities}}</div></div>
<div class="metric"><div>Bundles Submitted</div><div class="value">{{.BundlesSubmitted}}</div></div>
<div class="metric"><div>Bundles Included</div><div class="value">{{.BundlesIncluded}}</div></div>
<div class="metric"><div>Detection Latency</div><div class="value">{{.DetectionLatencyMs}}ms</div></div>
<div class="metric"><div>E2E Latency</div><div class="value">{{.EndToEndLatencyMs}}ms</div></div>
</body></html>`

	t, _ := template.New("dashboard").Parse(tmpl)
	data := map[string]int64{
		"Opportunities":      d.metrics.OpportunitiesDetected.Load(),
		"BundlesSubmitted":   d.metrics.BundlesSubmitted.Load(),
		"BundlesIncluded":    d.metrics.BundlesIncluded.Load(),
		"DetectionLatencyMs": d.metrics.DetectionLatencyMs.Load(),
		"EndToEndLatencyMs":  d.metrics.EndToEndLatencyMs.Load(),
	}
	t.Execute(w, data)
}

func (d *Dashboard) handleStats(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	fmt.Fprintf(w, `{"opportunities":%d,"bundles_submitted":%d,"bundles_included":%d,"detection_latency_ms":%d,"e2e_latency_ms":%d}`,
		d.metrics.OpportunitiesDetected.Load(),
		d.metrics.BundlesSubmitted.Load(),
		d.metrics.BundlesIncluded.Load(),
		d.metrics.DetectionLatencyMs.Load(),
		d.metrics.EndToEndLatencyMs.Load())
}
