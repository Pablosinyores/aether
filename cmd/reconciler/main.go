// Mempool reconciler — closes the loop on persisted predictions.
//
// Reads `mempool_predictions` written by the Rust mempool writer (PR #133)
// and produces one `mempool_reconciliation` row per prediction once the
// outcome is known. Together the two tables answer "did the tx land where
// we said it would, in the order we said it would, hitting the pool we
// said it would?" — entirely in SQL.
//
// Architecture (per-block loop):
//   1. ethclient.SubscribeNewHead → chan *types.Header
//   2. For each new header: BlockByHash → iterate Transactions()
//   3. Per tx: LookupPredictionByTxHash; if found, fetch receipt, write
//      `outcome='confirmed'` with block_delta + pool_path_correct
//   4. Every staleSweepInterval: MarkStaleAsDropped(currentHead) → bulk
//      INSERT `outcome='dropped'` for predictions where target+12 ≤ head
//
// Receipt fetch is per-prediction-hit (not per-block tx) so a block of
// 200 txs with 1 prediction hit costs one receipt RPC, not 200.
//
// Run with:
//
//   MEMPOOL_LEDGER_DSN=postgres://aether:aether@localhost:5433/aether \
//   ETH_RPC_URL=wss://eth-mainnet.g.alchemy.com/v2/<key> \
//   RECONCILER_METRICS_ADDR=:9094 \
//   ./aether-reconciler

package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/aether-arb/aether/internal/db"
)

const (
	// staleSweepInterval is the cadence of the dropped-outcome batch
	// query. Twice the average block time so a 24-block prediction
	// reaches the dropped state within ~12 s of its window closing.
	staleSweepInterval = 6 * time.Second

	// receiptFetchTimeout caps how long the reconciler waits for a single
	// `eth_getTransactionReceipt` round-trip. Sized for the p99 mainnet
	// receipt latency from major providers (~1.5 s); if the call wedges
	// past this, the reconciliation row lands without `pool_path_correct`
	// rather than block the per-block loop.
	receiptFetchTimeout = 3 * time.Second

	// blockFetchTimeout caps the `eth_getBlockByHash` call. Generous
	// because a single failure stalls every prediction in that block,
	// not just one.
	blockFetchTimeout = 5 * time.Second
)

func main() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))

	rpcURL := os.Getenv("ETH_RPC_URL")
	if rpcURL == "" {
		slog.Error("ETH_RPC_URL not set")
		os.Exit(1)
	}
	dsn := os.Getenv("MEMPOOL_LEDGER_DSN")
	if dsn == "" {
		slog.Error("MEMPOOL_LEDGER_DSN not set")
		os.Exit(1)
	}
	metricsAddr := os.Getenv("RECONCILER_METRICS_ADDR")
	if metricsAddr == "" {
		metricsAddr = ":9094"
	}

	rootCtx, rootCancel := context.WithCancel(context.Background())
	defer rootCancel()
	installSignalHandler(rootCancel)

	dialCtx, dialCancel := context.WithTimeout(rootCtx, 10*time.Second)
	defer dialCancel()
	ethClient, err := ethclient.DialContext(dialCtx, rpcURL)
	if err != nil {
		slog.Error("dial ETH_RPC_URL failed", "err", err)
		os.Exit(1)
	}
	slog.Info("connected to ethereum node")

	registry := prometheus.NewRegistry()
	dbMetrics := db.NewMempoolReconciliationMetrics(registry)
	loopMetrics := newLoopMetrics(registry)

	pgRecon, err := db.NewPgMempoolReconciliation(rootCtx, dsn, dbMetrics)
	if err != nil {
		slog.Error("PgMempoolReconciliation connect failed", "err", err)
		os.Exit(1)
	}
	defer pgRecon.Close()

	// /metrics endpoint runs on a background server so the binary is
	// scrapeable by Prometheus without coupling to the engine's existing
	// :9092 endpoint.
	metricsServer := startMetricsServer(metricsAddr, registry)
	defer func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = metricsServer.Shutdown(shutdownCtx)
	}()

	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		runHeaderLoop(rootCtx, ethClient, pgRecon, loopMetrics)
	}()
	go func() {
		defer wg.Done()
		runStaleSweepLoop(rootCtx, ethClient, pgRecon)
	}()

	<-rootCtx.Done()
	slog.Info("shutdown signalled; waiting for loops to exit")
	// Give the loops a few seconds to drain in-flight reconciliations.
	doneCh := make(chan struct{})
	go func() {
		wg.Wait()
		close(doneCh)
	}()
	select {
	case <-doneCh:
	case <-time.After(10 * time.Second):
		slog.Warn("loops did not exit within 10s; tearing down anyway")
	}
}

func installSignalHandler(cancel context.CancelFunc) {
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		sig := <-sigCh
		slog.Info("signal received", "sig", sig)
		cancel()
	}()
}

func startMetricsServer(addr string, registry *prometheus.Registry) *http.Server {
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.HandlerFor(registry, promhttp.HandlerOpts{Registry: registry}))
	srv := &http.Server{Addr: addr, Handler: mux, ReadHeaderTimeout: 3 * time.Second}
	go func() {
		slog.Info("metrics server listening", "addr", addr)
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			slog.Error("metrics server failed", "err", err)
		}
	}()
	return srv
}

// runHeaderLoop is the hot path. SubscribeNewHead provides a steady stream
// of *types.Header (one per ~12 s on mainnet); each header drives one
// block-resolution pass.
//
// SubscribeNewHead errors trigger a single reconnect + retry. A second
// failure exits the loop so the orchestrator (systemd / k8s) can restart
// the binary cleanly — a long-lived subscriber that silently stalls is
// worse than a binary that exits and gets restarted.
func runHeaderLoop(
	ctx context.Context,
	client *ethclient.Client,
	recon *db.PgMempoolReconciliation,
	metrics *loopMetrics,
) {
	headers := make(chan *types.Header, 8)
	sub, err := client.SubscribeNewHead(ctx, headers)
	if err != nil {
		slog.Error("SubscribeNewHead failed", "err", err)
		return
	}
	defer sub.Unsubscribe()
	slog.Info("subscribed to newHeads")

	for {
		select {
		case <-ctx.Done():
			return
		case err := <-sub.Err():
			slog.Error("newHeads subscription error", "err", err)
			return
		case header := <-headers:
			handleHeader(ctx, client, recon, metrics, header)
		}
	}
}

// handleHeader resolves every prediction whose pending_tx_hash appears in
// this block. Per-block cost is one block-by-hash + one receipt-by-hash
// per prediction hit. Predictions are the rare case (a few per block on a
// good day) so the receipt fetches do not dominate.
func handleHeader(
	ctx context.Context,
	client *ethclient.Client,
	recon *db.PgMempoolReconciliation,
	metrics *loopMetrics,
	header *types.Header,
) {
	metrics.HeadersProcessed.Inc()

	blockCtx, cancel := context.WithTimeout(ctx, blockFetchTimeout)
	defer cancel()
	block, err := client.BlockByHash(blockCtx, header.Hash())
	if err != nil {
		slog.Warn("BlockByHash failed; skipping reconciliation for this block",
			"block_hash", header.Hash().Hex(),
			"err", err)
		metrics.HeaderFetchErrors.Inc()
		return
	}

	resolvedAt := time.Now().UTC()
	blockNumber := block.NumberU64()

	for txIdx, tx := range block.Transactions() {
		var txHash [32]byte
		copy(txHash[:], tx.Hash().Bytes())

		lookupCtx, lookupCancel := context.WithTimeout(ctx, blockFetchTimeout)
		pred, found, err := recon.LookupPredictionByTxHash(lookupCtx, txHash)
		lookupCancel()
		if err != nil {
			slog.Warn("LookupPredictionByTxHash failed",
				"tx_hash", tx.Hash().Hex(),
				"err", err)
			metrics.LookupErrors.Inc()
			continue
		}
		if !found {
			continue
		}

		actualBlock := blockNumber
		actualIdx := txIdx
		blockDelta := int(int64(actualBlock) - int64(pred.PredictedTargetBlock))

		var poolPathCorrect *bool
		if pred.PoolAddress != nil {
			result, err := receiptHitsPool(ctx, client, tx.Hash(), *pred.PoolAddress)
			if err != nil {
				// Receipt fetch failure leaves pool_path_correct NULL so
				// the row still lands. The TransactionReceiptErrors
				// counter is the alert signal.
				slog.Debug("TransactionReceipt failed; pool_path_correct=NULL",
					"tx_hash", tx.Hash().Hex(),
					"err", err)
				metrics.ReceiptFetchErrors.Inc()
			} else {
				poolPathCorrect = &result
				metrics.PoolPathChecks.WithLabelValues(pred.Protocol, boolLabel(result)).Inc()
			}
		}

		metrics.BlockDelta.Observe(float64(blockDelta))

		recon.InsertReconciliation(db.NewReconciliation{
			PredictionID:      pred.PredictionID,
			ResolutionTs:      resolvedAt,
			Outcome:           db.OutcomeConfirmed,
			ActualTargetBlock: &actualBlock,
			ActualTxIndex:     &actualIdx,
			BlockDelta:        &blockDelta,
			PoolPathCorrect:   poolPathCorrect,
		})
	}
}

// receiptHitsPool fetches the tx's receipt and returns true iff any log
// entry's `Address` matches `poolAddr`. The predicted swap is expected to
// emit a `Swap`/`Sync`/`TokensTraded` event from the pool contract, so the
// address match alone is sufficient — decoding the event topic would
// confirm "yes it was a swap" but adds protocol-specific decode tables
// without changing the answer to "did we route to the pool we expected".
func receiptHitsPool(
	ctx context.Context,
	client *ethclient.Client,
	txHash common.Hash,
	poolAddr [20]byte,
) (bool, error) {
	receiptCtx, cancel := context.WithTimeout(ctx, receiptFetchTimeout)
	defer cancel()
	receipt, err := client.TransactionReceipt(receiptCtx, txHash)
	if err != nil {
		return false, err
	}
	want := common.BytesToAddress(poolAddr[:])
	for _, log := range receipt.Logs {
		if log.Address == want {
			return true, nil
		}
	}
	return false, nil
}

// runStaleSweepLoop runs the periodic dropped-outcome batch. Reads the
// chain head from the eth client on every tick (rather than caching the
// header from runHeaderLoop) so the two loops stay independent — a stalled
// WS subscription does not freeze the dropped sweep.
func runStaleSweepLoop(
	ctx context.Context,
	client *ethclient.Client,
	recon *db.PgMempoolReconciliation,
) {
	ticker := time.NewTicker(staleSweepInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			headCtx, cancel := context.WithTimeout(ctx, blockFetchTimeout)
			head, err := client.BlockNumber(headCtx)
			cancel()
			if err != nil {
				slog.Warn("BlockNumber failed; skipping stale sweep", "err", err)
				continue
			}
			rows, err := recon.MarkStaleAsDropped(ctx, head)
			if err != nil {
				slog.Warn("MarkStaleAsDropped failed", "err", err)
				continue
			}
			if rows > 0 {
				slog.Info("stale sweep marked predictions as dropped",
					"rows", rows, "head", head)
			}
		}
	}
}

func boolLabel(b bool) string {
	if b {
		return "true"
	}
	return "false"
}

// loopMetrics groups the per-loop Prometheus families that are computed
// in-process by the header / sweep loops. The DB-layer metrics live with
// PgMempoolReconciliation.
type loopMetrics struct {
	HeadersProcessed   prometheus.Counter
	HeaderFetchErrors  prometheus.Counter
	LookupErrors       prometheus.Counter
	ReceiptFetchErrors prometheus.Counter
	BlockDelta         prometheus.Histogram
	PoolPathChecks     *prometheus.CounterVec
}

func newLoopMetrics(reg prometheus.Registerer) *loopMetrics {
	m := &loopMetrics{
		HeadersProcessed: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "aether_mempool_reconciler_headers_processed_total",
			Help: "Block headers received from the WS newHeads subscription and processed",
		}),
		HeaderFetchErrors: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "aether_mempool_reconciler_header_fetch_errors_total",
			Help: "BlockByHash failures (per-header)",
		}),
		LookupErrors: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "aether_mempool_reconciler_lookup_errors_total",
			Help: "LookupPredictionByTxHash failures (per-tx)",
		}),
		ReceiptFetchErrors: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "aether_mempool_reconciler_receipt_fetch_errors_total",
			Help: "TransactionReceipt failures; reconciliation row still lands with pool_path_correct=NULL",
		}),
		BlockDelta: prometheus.NewHistogram(prometheus.HistogramOpts{
			Name:    "aether_mempool_block_delta",
			Help:    "Confirmed prediction's actual_target_block minus predicted_target_block. PromQL: 1h-window accuracy = histogram_quantile(0.5, …) over time.",
			Buckets: []float64{-2, -1, 0, 1, 2, 3, 5, 8, 12, 20},
		}),
		PoolPathChecks: prometheus.NewCounterVec(prometheus.CounterOpts{
			Name: "aether_mempool_pool_path_total",
			Help: "Confirmed predictions whose receipt logs were checked against the predicted pool, by protocol and correctness",
		}, []string{"protocol", "correct"}),
	}
	reg.MustRegister(
		m.HeadersProcessed, m.HeaderFetchErrors, m.LookupErrors,
		m.ReceiptFetchErrors, m.BlockDelta, m.PoolPathChecks,
	)
	return m
}
