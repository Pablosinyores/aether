// Prometheus surface for the mempool reconciliation writer.
//
// Kept in the `db` package alongside LedgerMetrics so the two reconciliation
// halves (DB + reconciler binary) can register their counters against a
// single shared registry. The reconciler-loop metrics (`block_accuracy`,
// `pool_path_accuracy`) live with the binary in `cmd/reconciler/` because
// they are computed from in-process counters and never touch the DB.

package db

import "github.com/prometheus/client_golang/prometheus"

// MempoolReconciliationMetrics groups the families the
// PgMempoolReconciliation writer goroutine updates. Names mirror the
// `aether_ledger_*` namespace shape (`aether_mempool_reconciler_*`) so
// dashboards can apply a single template.
type MempoolReconciliationMetrics struct {
	// Bumped on every successful reconciliation insert (or on every row
	// returned by MarkStaleAsDropped). `outcome` is one of the
	// OutcomeConfirmed / OutcomeDropped / OutcomeReplaced /
	// OutcomeStillPending constants.
	ReconciledTotal *prometheus.CounterVec
	// Reconciliation writes the bounded channel rejected because it was
	// full. Single-labelled (no `op`) because this writer only does one
	// kind of insert.
	DropsTotal      prometheus.Counter
	QueueDepth      prometheus.Gauge
	// Per-write latency from dequeue to query completion. `result` =
	// "ok"|"err" so an alert can fire on a sudden `err` spike.
	WriteLatencyMs  *prometheus.HistogramVec
}

// NewMempoolReconciliationMetrics constructs the families and registers
// them with the supplied Prometheus registerer. A separate registerer
// argument (vs the default `prometheus.MustRegister`) makes the binary's
// /metrics endpoint composable — the reconciler can publish under its own
// process registry while the engine publishes under its own, and a future
// joint binary can pass the same registry to both halves.
func NewMempoolReconciliationMetrics(reg prometheus.Registerer) *MempoolReconciliationMetrics {
	m := &MempoolReconciliationMetrics{
		ReconciledTotal: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "aether_mempool_reconciled_total",
			Help: "Mempool predictions resolved by the reconciler, by outcome",
		}, []string{"outcome"}),
		DropsTotal: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "aether_mempool_reconciler_drops_total",
			Help: "Reconciliation writes dropped because the bounded channel was full",
		}),
		QueueDepth: prometheus.NewGauge(prometheus.GaugeOpts{
			Name: "aether_mempool_reconciler_queue_depth",
			Help: "Pending reconciliation writes sitting in the writer-goroutine channel",
		}),
		WriteLatencyMs: prometheus.NewHistogramVec(prometheus.HistogramOpts{
			Name:    "aether_mempool_reconciler_write_latency_ms",
			Help:    "Per-write latency of reconciliation inserts from dequeue to query completion",
			Buckets: []float64{0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0},
		}, []string{"result"}),
	}
	reg.MustRegister(m.ReconciledTotal, m.DropsTotal, m.QueueDepth, m.WriteLatencyMs)
	return m
}
