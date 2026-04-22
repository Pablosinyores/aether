package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/big"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"

	"github.com/aether-arb/aether/internal/config"
	aethergrpc "github.com/aether-arb/aether/internal/grpc"
	pb "github.com/aether-arb/aether/internal/pb"
	"github.com/aether-arb/aether/internal/risk"
	"github.com/aether-arb/aether/internal/tracing"
)

var tracer trace.Tracer = otel.Tracer("aether-executor")

// Config holds executor service configuration.
// ChainID, ExecutorAddr, and the live ETH balance are no longer carried here —
// they are resolved against the connected node at startup (see main) and the
// balance is updated continuously by balanceWatchLoop.
type Config struct {
	GRPCAddress    string
	BuilderConfigs []BuilderConfig
	MaxGasGwei     float64
}

func defaultConfig() Config {
	return Config{
		GRPCAddress:    "localhost:50051",
		BuilderConfigs: defaultBuilderConfigs(),
		MaxGasGwei:     300.0,
	}
}

// loadConfig attempts to load the executor Config from YAML config files,
// falling back to defaults for any config that cannot be loaded.
func loadConfig() Config {
	cfg := defaultConfig()

	// Try loading builders from config/builders.yaml
	buildersPath := config.ConfigPath("builders.yaml")
	bc, err := config.LoadBuildersConfig(buildersPath)
	if err != nil {
		slog.Warn("builders.yaml not loaded, using defaults", "path", buildersPath, "err", err)
	} else {
		builders := make([]BuilderConfig, 0, len(bc.Builders))
		for _, b := range bc.Builders {
			builders = append(builders, BuilderConfig{
				Name:      b.Name,
				URL:       b.URL,
				AuthType:  b.AuthType,
				AuthKey:   b.AuthKey,
				Enabled:   b.Enabled,
				TimeoutMs: b.TimeoutMs,
			})
		}
		cfg.BuilderConfigs = builders
		slog.Info("builders loaded", "count", len(builders), "path", buildersPath)
	}

	// Override gRPC address from environment if set.
	if addr := os.Getenv("GRPC_ADDRESS"); addr != "" {
		cfg.GRPCAddress = addr
		slog.Info("grpc address overridden from env", "addr", addr)
	}

	return cfg
}

// loadRiskConfig attempts to load risk parameters from config/risk.yaml,
// falling back to DefaultRiskConfig if the file cannot be loaded.
func loadRiskConfig() risk.RiskConfig {
	riskPath := config.ConfigPath("risk.yaml")
	rc, err := risk.LoadRiskConfig(riskPath)
	if err != nil {
		slog.Warn("risk.yaml not loaded, using defaults", "path", riskPath, "err", err)
		return risk.DefaultRiskConfig()
	}
	slog.Info("risk config loaded", "path", riskPath)
	return rc
}

func main() {
	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))

	fmt.Println("aether-executor: bundle construction and submission service")

	// Initialise OTLP tracing. No-op when OTEL_EXPORTER_OTLP_ENDPOINT is unset.
	tracerShutdownCtx, tracerShutdownCancel := context.WithCancel(context.Background())
	defer tracerShutdownCancel()
	shutdownTracer, err := tracing.Init(tracerShutdownCtx, "aether-executor")
	if err != nil {
		slog.Warn("otlp tracer init failed, continuing without traces", "err", err)
		shutdownTracer = func(context.Context) error { return nil }
	}
	tracer = otel.Tracer("aether-executor")
	defer func() {
		flushCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := shutdownTracer(flushCtx); err != nil {
			slog.Warn("tracer shutdown error", "err", err)
		}
	}()

	cfg := loadConfig()

	// Executor on-chain parameters (contract address, expected chain ID) are
	// required: the service refuses to start without them. This prevents the
	// old fail-open behaviour where a zero-address stub silently routed
	// bundles to nowhere. Deployments inject the address via
	// ${AETHER_EXECUTOR_ADDRESS} which executor.yaml expands at load time —
	// ExpandEnv runs inside LoadExecutorConfig before validation, so no
	// separate post-load override path is needed.
	execPath := config.ConfigPath("executor.yaml")
	execCfg, err := config.LoadExecutorConfig(execPath)
	if err != nil {
		slog.Error("executor config missing or invalid", "path", execPath, "err", err)
		os.Exit(1)
	}
	slog.Info("executor config loaded", "executor_address", execCfg.ExecutorAddress, "expected_chain_id", execCfg.ExpectedChainID)

	// ETH_RPC_URL is now required — the chain-ID check, bytecode check, and
	// live balance polling all need a node connection.
	rpcURL := os.Getenv("ETH_RPC_URL")
	if rpcURL == "" {
		slog.Error("ETH_RPC_URL not set — required for chain-id / bytecode / balance checks")
		os.Exit(1)
	}
	dialCtx, dialCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer dialCancel()
	ethClient, err := ethclient.DialContext(dialCtx, rpcURL)
	if err != nil {
		slog.Error("failed to connect to ETH_RPC_URL", "url", redactRPCURL(rpcURL), "err", redactRPCError(err, rpcURL))
		os.Exit(1)
	}
	slog.Info("connected to ethereum node")

	// Cross-check chain ID: the node must agree with the expected chain in
	// executor.yaml. A mismatch here typically means someone pointed a
	// mainnet config at a testnet RPC (or vice versa) — refuse to start.
	chainCtx, chainCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer chainCancel()
	chainID, err := ethClient.ChainID(chainCtx)
	if err != nil {
		slog.Error("eth_chainId failed", "err", redactRPCError(err, rpcURL))
		os.Exit(1)
	}
	if chainID.Int64() != execCfg.ExpectedChainID {
		slog.Error("chain-id mismatch", "node_chain_id", chainID.Int64(), "config_chain_id", execCfg.ExpectedChainID)
		os.Exit(1)
	}
	slog.Info("chain ID verified", "chain_id", chainID.Int64())

	// Verify the configured executor contract actually exists on-chain. A
	// zero-bytecode result means we'd be sending bundles to a non-contract.
	codeCtx, codeCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer codeCancel()
	code, err := ethClient.CodeAt(codeCtx, common.HexToAddress(execCfg.ExecutorAddress), nil)
	if err != nil {
		slog.Error("eth_getCode failed", "executor_address", execCfg.ExecutorAddress, "err", redactRPCError(err, rpcURL))
		os.Exit(1)
	}
	if len(code) == 0 {
		slog.Error("executor address has no bytecode on chain", "executor_address", execCfg.ExecutorAddress, "chain_id", chainID.Int64())
		os.Exit(1)
	}
	slog.Info("executor contract verified on-chain", "executor_address", execCfg.ExecutorAddress, "code_bytes", len(code))

	// Load searcher private key for transaction signing and bundle submission.
	searcherKey := os.Getenv("SEARCHER_KEY")

	// Create submitter BEFORE clearing the key - it needs the key for FlashbotsSigner.
	submitter, err := NewSubmitter(cfg.BuilderConfigs, searcherKey)
	if err != nil {
		slog.Error("failed to create submitter", "err", err)
		os.Exit(1)
	}

	var txSigner *TransactionSigner
	if searcherKey != "" {
		var signerErr error
		txSigner, signerErr = NewTransactionSigner(searcherKey, chainID.Int64())
		if signerErr != nil {
			slog.Error("failed to load SEARCHER_KEY", "err", signerErr)
			os.Exit(1)
		}
		slog.Info("searcher signer loaded", "addr", txSigner.Address().Hex())
	} else {
		slog.Warn("SEARCHER_KEY not set, transactions will not be signed")
	}

	os.Unsetenv("SEARCHER_KEY")

	// Setup graceful shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Initialize components
	nonceManager := NewNonceManager(0)
	if txSigner != nil {
		nonceManager.SetSyncSource(txSigner.Address(), ethClient)
		if err := nonceManager.SyncFromChain(ctx); err != nil {
			slog.Warn("failed to sync nonce", "addr", txSigner.Address().Hex(), "err", err)
		}
	} else {
		slog.Warn("SEARCHER_KEY not set, nonce manager will use initial nonce 0")
	}

	gasOracle := NewGasOracle(cfg.MaxGasGwei)
	gasOracle.SetClient(ethClient)
	// Fetch real gas prices before first arb evaluation.
	if _, err := gasOracle.FetchOnce(ctx); err != nil {
		slog.Warn("initial gas oracle fetch failed", "err", err)
	}
	bundler := NewBundleConstructor(nonceManager, gasOracle, txSigner, chainID.Int64())
	riskMgr := risk.NewRiskManager(loadRiskConfig())
	riskMgr.SetMetricsObserver(executorMetricsObserver{})

	// Live searcher ETH balance, written by balanceWatchLoop and read by
	// consumeArbStream → processArb → risk.PreflightCheck. When no searcher
	// key is configured there is no address to query, so the live balance
	// stays at zero and preflight will reject every arb — correct behaviour
	// for a misconfigured deployment.
	liveBalance := NewLiveBalance()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	var wg sync.WaitGroup

	// Start nonce sync loop
	wg.Add(1)
	go func() {
		defer wg.Done()
		nonceManager.SyncLoop(ctx, 30*time.Second)
	}()

	// Start gas oracle update loop
	wg.Add(1)
	go func() {
		defer wg.Done()
		gasOracle.UpdateLoop(ctx, 12*time.Second)
	}()

	// Start ETH balance watcher. The initial fetch is synchronous and fatal
	// on failure: LiveBalance starts at zero, and risk.PreflightCheck rejects
	// anything below MinETHBalance, so a transient startup blip would
	// silently kill every arb for up to 30s until balanceWatchLoop's first
	// tick. This matches the fatal-on-startup pattern of the dial, chain-ID,
	// and bytecode checks.
	if txSigner != nil {
		if err := fetchAndStoreBalance(ctx, ethClient, txSigner.Address(), liveBalance); err != nil {
			slog.Error("initial eth_getBalance failed", "addr", txSigner.Address().Hex(), "err", redactRPCError(err, rpcURL))
			os.Exit(1)
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			balanceWatchLoop(ctx, ethClient, txSigner.Address(), 30*time.Second, liveBalance, rpcURL)
		}()
	}

	startMetricsServer()

	transport := "TCP"
	if strings.HasPrefix(cfg.GRPCAddress, "unix:") {
		transport = "UDS"
	}
	slog.Info("executor service started", "grpc_target", cfg.GRPCAddress, "transport", transport)
	slog.Info("builders configured", "count", len(cfg.BuilderConfigs))

	// Connect to Rust engine gRPC server.
	// grpc.NewClient is lazy — the connection is established on first RPC,
	// so this call returns immediately even if the Rust server is not running.
	grpcClient, err := aethergrpc.Dial(cfg.GRPCAddress)
	if err != nil {
		slog.Warn("could not create gRPC client, executor will start without arb stream", "addr", cfg.GRPCAddress, "err", err)
	} else {
		defer grpcClient.Close()

		// Start arb stream consumer goroutine
		wg.Add(1)
		go func() {
			defer wg.Done()
			consumeArbStream(ctx, grpcClient, bundler, submitter, riskMgr, execCfg.ExecutorAddress, liveBalance)
		}()
	}

	// Wait for shutdown signal
	select {
	case sig := <-sigCh:
		slog.Info("received signal, shutting down", "signal", sig.String())
		cancel()
	case <-ctx.Done():
	}

	// Wait for goroutines
	wg.Wait()
	slog.Info("executor service stopped")
}

// processArb handles a single validated arb through the full pipeline:
// parse -> preflight -> bundle -> submit -> record result.
// receivedAt is the Go-side wall clock when the arb arrived from the gRPC
// stream — used for end-to-end latency to avoid cross-process clock skew.
func processArb(
	ctx context.Context,
	arb *pb.ValidatedArb,
	receivedAt time.Time,
	rm *risk.RiskManager,
	bundler *BundleConstructor,
	submitter *Submitter,
	executorAddr string,
	ethBalance float64,
) (submitted bool, err error) {
	ctx, span := tracer.Start(ctx, "processArb",
		trace.WithAttributes(
			attribute.String("arb_id", arb.Id),
			attribute.Int("hops", len(arb.Hops)),
			attribute.Int64("target_block", int64(arb.BlockNumber+1)),
		),
	)
	defer span.End()

	profitWei := new(big.Int).SetBytes(arb.NetProfitWei)
	tradeValueWei := new(big.Int).SetBytes(arb.FlashloanAmount)

	gasFees := bundler.gasOracle.CurrentFees()
	gasGwei := gasFees.GasPriceGwei
	tipSharePct := rm.CalculateTipShare(profitWei, gasGwei)

	_, preflightSpan := tracer.Start(ctx, "preflight")
	result := rm.PreflightCheck(profitWei, tradeValueWei, gasGwei, tipSharePct, ethBalance)
	preflightSpan.SetAttributes(
		attribute.Bool("approved", result.Approved),
		attribute.String("reason", result.Reason),
	)
	preflightSpan.End()
	if !result.Approved {
		recordRiskRejection()
		slog.InfoContext(ctx, "arb rejected by preflight", "arb_id", arb.Id, "reason", result.Reason)
		span.SetAttributes(attribute.String("outcome", "rejected"))
		return false, nil
	}

	_, buildSpan := tracer.Start(ctx, "bundle.build")
	bundle, err := bundler.BuildBundle(arb.Calldata, executorAddr, arb.TotalGas, arb.BlockNumber+1)
	if err != nil {
		buildSpan.RecordError(err)
		buildSpan.SetStatus(codes.Error, "build bundle failed")
		buildSpan.End()
		span.RecordError(err)
		span.SetStatus(codes.Error, "build bundle failed")
		return false, fmt.Errorf("build bundle: %w", err)
	}
	buildSpan.End()

	// Shadow mode: the bundle is fully built and signed, but we skip the
	// network submission. Used by historical replay + pre-prod measurement
	// to exercise the full pipeline without touching Flashbots.
	if isShadowMode() {
		recordEndToEndLatency(receivedAt)
		recordShadowBundle()
		profitEth := weiToEth(profitWei)
		slog.InfoContext(ctx, "shadow bundle built, skipping submission",
			"arb_id", arb.Id,
			"target_block", arb.BlockNumber+1,
			"tip_tx_count", len(bundle.RawTxs),
			"profit_eth", profitEth,
			"gas", arb.TotalGas,
			"tip_share_pct", tipSharePct,
		)
		if err := dumpShadowBundle(arb, bundle, profitEth, gasGwei, tipSharePct); err != nil {
			slog.WarnContext(ctx, "shadow bundle json dump failed", "arb_id", arb.Id, "err", err)
		}
		span.SetAttributes(attribute.String("outcome", "shadow"))
		return true, nil
	}

	// Submit to all builders
	recordEndToEndLatency(receivedAt)
	recordBundleSubmitted()
	results := submitter.SubmitToAll(ctx, bundle)
	recordSubmissionReverts(rm, results)
	successes := SuccessCount(results)

	slog.InfoContext(ctx, "arb submitted", "arb_id", arb.Id, "builders", len(results), "accepted", successes)

	// Record result for miss rate tracking
	included := successes > 0
	if included {
		recordBundleIncluded(profitWei, gasGwei, arb.TotalGas)
	}
	rm.RecordBundleResult(included)

	span.SetAttributes(
		attribute.Int("builders", len(results)),
		attribute.Int("accepted", successes),
		attribute.Bool("included", included),
	)
	return included, nil
}

// recordSubmissionReverts classifies and records a single revert per arb
// attempt. When multiple builders reject the same arb, we take the worst-case
// classification (bug > competitive) so the circuit breaker is not silently
// bypassed, but we never inflate the count beyond one per submission.
func recordSubmissionReverts(rm *risk.RiskManager, results []SubmissionResult) {
	worstType := risk.RevertCompetitive
	foundRevert := false
	for _, res := range results {
		if res.Success || res.Error == nil {
			continue
		}
		errMsg := res.Error.Error()
		if !looksLikeRevert(errMsg) {
			continue
		}
		foundRevert = true
		if risk.ClassifyRevert(errMsg) == risk.RevertBug {
			worstType = risk.RevertBug
		}
	}
	if foundRevert {
		rm.RecordRevert(worstType)
	}
}

// looksLikeRevert returns true when the error message looks like an EVM revert
// rather than an infrastructure failure (timeout, TLS error, etc.).
//
// Competitive patterns are delegated to ClassifyRevert to avoid duplicating the
// pattern list. Only "revert"/"reverted" keywords are checked here to catch bug
// reverts that ClassifyRevert doesn't recognise as competitive.
func looksLikeRevert(errMsg string) bool {
	lower := strings.ToLower(strings.TrimSpace(errMsg))
	if lower == "" {
		return true
	}
	// If ClassifyRevert recognises it as competitive, it is a revert.
	if risk.ClassifyRevert(errMsg) == risk.RevertCompetitive {
		return true
	}
	// Catch remaining bug reverts by keyword.
	return strings.Contains(lower, "revert") || strings.Contains(lower, "reverted")
}

// consumeArbStream connects to the Rust engine's StreamArbs RPC and
// processes validated arbitrage opportunities as they arrive. On stream
// errors it reconnects with a backoff delay. The function exits when ctx
// is cancelled.
func consumeArbStream(ctx context.Context, client *aethergrpc.Client, bundler *BundleConstructor, submitter *Submitter, rm *risk.RiskManager, executorAddr string, liveBalance *LiveBalance) {
	const (
		minProfitETH   = 0.001 // Minimum profit threshold in ETH
		reconnectDelay = 5 * time.Second
	)

	for {
		select {
		case <-ctx.Done():
			return
		default:
		}

		stream, err := client.StreamArbs(ctx, minProfitETH)
		if err != nil {
			slog.WarnContext(ctx, "StreamArbs connect error, will retry", "err", err, "retry_in", reconnectDelay.String())
			select {
			case <-ctx.Done():
				return
			case <-time.After(reconnectDelay):
				continue
			}
		}

		slog.InfoContext(ctx, "connected to rust engine arb stream")

		for {
			arb, err := stream.Recv()
			if err != nil {
				slog.WarnContext(ctx, "arb stream recv error, reconnecting", "err", err)
				break
			}
			receivedAt := time.Now() // Go-side clock avoids cross-process skew

			slog.InfoContext(ctx, "arb received", "arb_id", arb.Id, "hops", len(arb.Hops), "gas", arb.TotalGas, "block", arb.BlockNumber)

			submitted, err := processArb(ctx, arb, receivedAt, rm, bundler, submitter, executorAddr, liveBalance.Get())
			switch {
			case err != nil:
				slog.ErrorContext(ctx, "error processing arb", "arb_id", arb.Id, "err", err)
			case !submitted:
				slog.InfoContext(ctx, "arb skipped", "arb_id", arb.Id, "reason", "risk-manager veto or below threshold")
			}
		}
	}
}

// executorMetricsObserver adapts risk-layer state events to Prometheus.
// Kept as a struct so cmd/executor keeps the Prometheus dependency and
// internal/risk stays pure.
type executorMetricsObserver struct{}

func (executorMetricsObserver) OnStateChange(s risk.SystemState) {
	setSystemState(stateToInt(s))
}

func (executorMetricsObserver) OnCircuitBreakerTrip(reason string) {
	recordCircuitBreakerTrip(reason)
}

// stateToInt maps system states to a numeric gauge value. -1 surfaces an
// anomaly on dashboards if a new state is added without updating this mapping.
func stateToInt(s risk.SystemState) int {
	switch s {
	case risk.StateRunning:
		return 0
	case risk.StateDegraded:
		return 1
	case risk.StatePaused:
		return 2
	case risk.StateHalted:
		return 3
	default:
		return -1
	}
}

// isShadowMode reports whether AETHER_SHADOW is set to a truthy value.
// Evaluated on every call so tests can flip the env without restart.
// Uses strconv.ParseBool to stay in lockstep with Go's stdlib truthy
// semantics (1/t/T/TRUE/true/True/0/f/F/FALSE/false/False); any garbage
// input falls through to `false` instead of silently enabling shadow mode.
func isShadowMode() bool {
	raw := strings.TrimSpace(os.Getenv("AETHER_SHADOW"))
	if raw == "" {
		return false
	}
	v, err := strconv.ParseBool(raw)
	if err != nil {
		return false
	}
	return v
}

// shadowBundleDumpDir returns the target dir for shadow-bundle JSONs.
// Defaults to ./reports/bundles so the e2e script picks them up without any
// extra wiring. Override via AETHER_SHADOW_DUMP_DIR for custom orchestrations.
func shadowBundleDumpDir() string {
	if d := strings.TrimSpace(os.Getenv("AETHER_SHADOW_DUMP_DIR")); d != "" {
		return d
	}
	return "reports/bundles"
}

// Well-known mainnet token labels for human-readable bundle dumps.
// Keep in sync with the set in aether-replay so the comparison script can
// match paths across the two sides.
var tokenLabels = map[string]string{
	strings.ToLower("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"): "WETH",
	strings.ToLower("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"): "USDC",
	strings.ToLower("0xdAC17F958D2ee523a2206206994597C13D831ec7"): "USDT",
	strings.ToLower("0x6B175474E89094C44Da98b954EedeAC495271d0F"): "DAI",
	strings.ToLower("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"): "WBTC",
	strings.ToLower("0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"): "AAVE",
}

func tokenLabel(addrBytes []byte) string {
	if len(addrBytes) == 0 {
		return "?"
	}
	hex := strings.ToLower(fmt.Sprintf("0x%x", addrBytes))
	if lbl, ok := tokenLabels[hex]; ok {
		return lbl
	}
	if len(hex) >= 10 {
		return hex[:10] + "…"
	}
	return hex
}

// dumpShadowBundle writes a single JSON file per shadow-mode bundle. One file
// per arb makes the output easy to inspect (`jq . reports/bundles/*.json`) and
// easy to correlate with aether-replay's CSV for hit-rate comparisons.
func dumpShadowBundle(
	arb *pb.ValidatedArb,
	bundle *Bundle,
	profitEth float64,
	gasGwei float64,
	tipSharePct float64,
) error {
	dir := shadowBundleDumpDir()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("mkdir %s: %w", dir, err)
	}

	// Build the human-readable token path from the hops.
	path := make([]string, 0, len(arb.Hops)+1)
	if len(arb.Hops) > 0 {
		path = append(path, tokenLabel(arb.Hops[0].TokenIn))
	}
	for _, h := range arb.Hops {
		path = append(path, tokenLabel(h.TokenOut))
	}

	// Serialise hops + raw txs (hex-encoded) for forensic inspection.
	hopsOut := make([]map[string]interface{}, 0, len(arb.Hops))
	for _, h := range arb.Hops {
		hopsOut = append(hopsOut, map[string]interface{}{
			"protocol":      h.Protocol.String(),
			"pool_address":  fmt.Sprintf("0x%x", h.PoolAddress),
			"token_in":      tokenLabel(h.TokenIn),
			"token_out":     tokenLabel(h.TokenOut),
			"amount_in":     new(big.Int).SetBytes(h.AmountIn).String(),
			"expected_out":  new(big.Int).SetBytes(h.ExpectedOut).String(),
			"estimated_gas": h.EstimatedGas,
		})
	}

	rawHex := make([]string, 0, len(bundle.RawTxs))
	for _, b := range bundle.RawTxs {
		rawHex = append(rawHex, fmt.Sprintf("0x%x", b))
	}

	payload := map[string]interface{}{
		"ts":                time.Now().UTC().Format(time.RFC3339Nano),
		"arb_id":            arb.Id,
		"target_block":      bundle.BlockNumber,
		"source_block":      arb.BlockNumber,
		"path":              path,
		"hops":              hopsOut,
		"flashloan_token":   tokenLabel(arb.FlashloanToken),
		"flashloan_amount":  new(big.Int).SetBytes(arb.FlashloanAmount).String(),
		"net_profit_wei":    new(big.Int).SetBytes(arb.NetProfitWei).String(),
		"net_profit_eth":    profitEth,
		"total_gas":         arb.TotalGas,
		"gas_price_gwei":    gasGwei,
		"tip_share_pct":     tipSharePct,
		"tx_count":          len(bundle.RawTxs),
		"raw_tx_hex":        rawHex,
		"calldata_hex":      fmt.Sprintf("0x%x", arb.Calldata),
	}

	out, err := json.MarshalIndent(payload, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal: %w", err)
	}

	// Sanitise arb_id for safe filename use.
	safeID := strings.Map(func(r rune) rune {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9', r == '-', r == '_':
			return r
		default:
			return '_'
		}
	}, arb.Id)
	if safeID == "" {
		safeID = "anon"
	}
	filename := filepath.Join(dir, safeID+".json")
	return os.WriteFile(filename, out, 0o644)
}

func weiToEth(wei *big.Int) float64 {
	if wei == nil || wei.Sign() == 0 {
		return 0
	}
	f, _ := new(big.Float).Quo(new(big.Float).SetInt(wei), big.NewFloat(1e18)).Float64()
	return f
}
