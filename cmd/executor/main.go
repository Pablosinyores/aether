package main

import (
	"context"
	"fmt"
	"log"
	"math/big"
	"os"
	"os/signal"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/ethclient"

	"github.com/aether-arb/aether/internal/config"
	aethergrpc "github.com/aether-arb/aether/internal/grpc"
	pb "github.com/aether-arb/aether/internal/pb"
	"github.com/aether-arb/aether/internal/risk"
)

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
		log.Printf("Config: builders.yaml not loaded (%v), using defaults", err)
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
		log.Printf("Config: loaded %d builders from %s", len(builders), buildersPath)
	}

	// Override gRPC address from environment if set.
	if addr := os.Getenv("GRPC_ADDRESS"); addr != "" {
		cfg.GRPCAddress = addr
		log.Printf("Config: GRPC_ADDRESS=%s (from env)", addr)
	}

	return cfg
}

// loadRiskConfig attempts to load risk parameters from config/risk.yaml,
// falling back to DefaultRiskConfig if the file cannot be loaded.
func loadRiskConfig() risk.RiskConfig {
	riskPath := config.ConfigPath("risk.yaml")
	rc, err := risk.LoadRiskConfig(riskPath)
	if err != nil {
		log.Printf("Config: risk.yaml not loaded (%v), using defaults", err)
		return risk.DefaultRiskConfig()
	}
	log.Printf("Config: loaded risk config from %s", riskPath)
	return rc
}

func main() {
	fmt.Println("aether-executor: bundle construction and submission service")

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
		log.Fatalf("FATAL: executor config (%s) missing or invalid: %v", execPath, err)
	}
	log.Printf("Config: executor_address=%s expected_chain_id=%d",
		execCfg.ExecutorAddress, execCfg.ExpectedChainID)

	// ETH_RPC_URL is now required — the chain-ID check, bytecode check, and
	// live balance polling all need a node connection.
	rpcURL := os.Getenv("ETH_RPC_URL")
	if rpcURL == "" {
		log.Fatalf("FATAL: ETH_RPC_URL not set — required for chain-id / bytecode / balance checks")
	}
	dialCtx, dialCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer dialCancel()
	ethClient, err := ethclient.DialContext(dialCtx, rpcURL)
	if err != nil {
		log.Fatalf("FATAL: failed to connect to ETH_RPC_URL (%s): %v",
			redactRPCURL(rpcURL), redactRPCError(err, rpcURL))
	}
	log.Printf("Connected to Ethereum node")

	// Cross-check chain ID: the node must agree with the expected chain in
	// executor.yaml. A mismatch here typically means someone pointed a
	// mainnet config at a testnet RPC (or vice versa) — refuse to start.
	chainCtx, chainCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer chainCancel()
	chainID, err := ethClient.ChainID(chainCtx)
	if err != nil {
		log.Fatalf("FATAL: eth_chainId failed: %v", redactRPCError(err, rpcURL))
	}
	if chainID.Int64() != execCfg.ExpectedChainID {
		log.Fatalf("FATAL: chain-id mismatch — node reports %d, config expects %d",
			chainID.Int64(), execCfg.ExpectedChainID)
	}
	log.Printf("Chain ID verified: %d", chainID.Int64())

	// Verify the configured executor contract actually exists on-chain. A
	// zero-bytecode result means we'd be sending bundles to a non-contract.
	codeCtx, codeCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer codeCancel()
	code, err := ethClient.CodeAt(codeCtx, common.HexToAddress(execCfg.ExecutorAddress), nil)
	if err != nil {
		log.Fatalf("FATAL: eth_getCode(%s) failed: %v",
			execCfg.ExecutorAddress, redactRPCError(err, rpcURL))
	}
	if len(code) == 0 {
		log.Fatalf("FATAL: executor address %s has no bytecode on chain %d",
			execCfg.ExecutorAddress, chainID.Int64())
	}
	log.Printf("Executor contract verified on-chain: %s (%d bytes of code)",
		execCfg.ExecutorAddress, len(code))

	// Load searcher private key for transaction signing and bundle submission.
	searcherKey := os.Getenv("SEARCHER_KEY")

	// Create submitter BEFORE clearing the key — it needs the key for FlashbotsSigner.
	submitter, err := NewSubmitter(cfg.BuilderConfigs, searcherKey)
	if err != nil {
		log.Fatalf("Failed to create submitter: %v", err)
	}

	var txSigner *TransactionSigner
	if searcherKey != "" {
		var signerErr error
		txSigner, signerErr = NewTransactionSigner(searcherKey, chainID.Int64())
		if signerErr != nil {
			log.Fatalf("Failed to load SEARCHER_KEY: %v", signerErr)
		}
		log.Printf("Searcher address: %s", txSigner.Address().Hex())
	} else {
		log.Println("WARNING: SEARCHER_KEY not set — transactions will not be signed")
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
			log.Printf("WARNING: failed to sync nonce for %s: %v", txSigner.Address().Hex(), err)
		}
	} else {
		log.Printf("WARNING: SEARCHER_KEY not set — nonce manager will use initial nonce 0")
	}

	gasOracle := NewGasOracle(cfg.MaxGasGwei)
	gasOracle.SetClient(ethClient)
	if _, err := gasOracle.FetchOnce(ctx); err != nil {
		log.Printf("WARNING: initial gas oracle fetch failed: %v", err)
	}
	bundler := NewBundleConstructor(nonceManager, gasOracle, txSigner, chainID.Int64())
	riskMgr := risk.NewRiskManager(loadRiskConfig())

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
			log.Fatalf("FATAL: initial eth_getBalance failed: %v", redactRPCError(err, rpcURL))
		}
		wg.Add(1)
		go func() {
			defer wg.Done()
			balanceWatchLoop(ctx, ethClient, txSigner.Address(), 30*time.Second, liveBalance, rpcURL)
		}()
	}

	startMetricsServer()

	if strings.HasPrefix(cfg.GRPCAddress, "unix:") {
		log.Printf("Executor service started, gRPC target: %s (UDS)", cfg.GRPCAddress)
	} else {
		log.Printf("Executor service started, gRPC target: %s (TCP)", cfg.GRPCAddress)
	}
	log.Printf("Configured %d builders", len(cfg.BuilderConfigs))

	// Connect to Rust engine gRPC server.
	// grpc.NewClient is lazy — the connection is established on first RPC,
	// so this call returns immediately even if the Rust server is not running.
	grpcClient, err := aethergrpc.Dial(cfg.GRPCAddress)
	if err != nil {
		log.Printf("WARNING: could not create gRPC client for %s: %v", cfg.GRPCAddress, err)
		log.Printf("Executor will start without arb stream")
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
		log.Printf("Received signal %v, shutting down...", sig)
		cancel()
	case <-ctx.Done():
	}

	// Wait for goroutines
	wg.Wait()
	log.Println("Executor service stopped")
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
	profitWei := new(big.Int).SetBytes(arb.NetProfitWei)
	tradeValueWei := new(big.Int).SetBytes(arb.FlashloanAmount)

	gasFees := bundler.gasOracle.CurrentFees()
	gasGwei := gasFees.GasPriceGwei
	tipSharePct := rm.CalculateTipShare(profitWei, gasGwei)

	result := rm.PreflightCheck(profitWei, tradeValueWei, gasGwei, tipSharePct, ethBalance)
	if !result.Approved {
		recordRiskRejection()
		log.Printf("Arb %s rejected by preflight: %s", arb.Id, result.Reason)
		return false, nil
	}

	bundle, err := bundler.BuildBundle(arb.Calldata, executorAddr, arb.TotalGas, arb.BlockNumber+1)
	if err != nil {
		return false, fmt.Errorf("build bundle: %w", err)
	}

	// Submit to all builders
	recordEndToEndLatency(receivedAt)
	recordBundleSubmitted()
	results := submitter.SubmitToAll(ctx, bundle)
	recordSubmissionReverts(rm, results)
	successes := SuccessCount(results)

	log.Printf("Arb %s: submitted to %d builders, %d accepted", arb.Id, len(results), successes)

	// Record result for miss rate tracking
	included := successes > 0
	if included {
		recordBundleIncluded(profitWei, gasGwei, arb.TotalGas)
	}
	rm.RecordBundleResult(included)

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
			log.Printf("StreamArbs connect error: %v, retrying in %v...", err, reconnectDelay)
			select {
			case <-ctx.Done():
				return
			case <-time.After(reconnectDelay):
				continue
			}
		}

		log.Println("Connected to Rust engine arb stream")

		for {
			arb, err := stream.Recv()
			if err != nil {
				log.Printf("Arb stream recv error: %v, reconnecting...", err)
				break
			}
			receivedAt := time.Now() // Go-side clock avoids cross-process skew

			log.Printf("Received arb: id=%s hops=%d gas=%d block=%d",
				arb.Id, len(arb.Hops), arb.TotalGas, arb.BlockNumber)

			submitted, err := processArb(ctx, arb, receivedAt, rm, bundler, submitter, executorAddr, liveBalance.Get())
			switch {
			case err != nil:
				log.Printf("Error processing arb %s: %v", arb.Id, err)
			case !submitted:
				log.Printf("Skipped arb %s (risk-manager veto or below threshold)", arb.Id)
			}
		}
	}
}
