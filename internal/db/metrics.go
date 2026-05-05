package db

import (
	"github.com/prometheus/client_golang/prometheus"
)

// LedgerMetrics owns the Prometheus families the PgLedger writer goroutine
// updates per insert / drop. Names mirror the Rust side's `aether_ledger_*`
// families exactly so a unified `/metrics` scrape across both binaries
// surfaces a single set of histograms / counters by op, not two parallel
// disjoint sets.
//
// Registered against the default Prometheus registry on construction. Calling
// NewLedgerMetrics more than once panics — there is one ledger per process.
type LedgerMetrics struct {
	WritesTotal     *prometheus.CounterVec
	DropsTotal      *prometheus.CounterVec
	QueueDepth      prometheus.Gauge
	WriteLatencyMs  *prometheus.HistogramVec
}

// NewLedgerMetrics constructs and registers the ledger metric families.
//
// Mirrors the Rust LedgerMetrics::register surface exactly:
//   - aether_ledger_writes_total{op, result}
//   - aether_ledger_drops_total{op}
//   - aether_ledger_queue_depth
//   - aether_ledger_write_latency_ms{op}
func NewLedgerMetrics() *LedgerMetrics {
	m := &LedgerMetrics{
		WritesTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "aether_ledger_writes_total",
			Help: "Trade-ledger writes attempted by the writer goroutine, by op and outcome",
		}, []string{"op", "result"}),
		DropsTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "aether_ledger_drops_total",
			Help: "Trade-ledger writes dropped because the bounded channel was full",
		}, []string{"op"}),
		QueueDepth: prometheus.NewGauge(prometheus.GaugeOpts{
			Name: "aether_ledger_queue_depth",
			Help: "Pending trade-ledger writes sitting in the writer goroutine channel",
		}),
		WriteLatencyMs: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Name: "aether_ledger_write_latency_ms",
			Help: "Per-op latency of trade-ledger writes from dequeue to query completion",
			// Sub-millisecond buckets land first because local-Postgres
			// inserts run ~150-300 µs and we want p50 visible on dashboards
			// without being flattened into the 0.5 ms bucket.
			Buckets: []float64{0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0},
		}, []string{"op"}),
	}
	prometheus.MustRegister(m.WritesTotal, m.DropsTotal, m.QueueDepth, m.WriteLatencyMs)
	return m
}
