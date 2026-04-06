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

// Config holds executor service configuration
type Config struct {
	GRPCAddress    string
	BuilderConfigs []BuilderConfig
	ChainID        int64
	MaxGasGwei     float64
	TipSharePct    float64
	ExecutorAddr   string  // On-chain AetherExecutor contract address
	EthBalance     float64 // Current ETH balance of searcher wallet (simulated)
}

func defaultConfig() Config {
	return Config{
		GRPCAddress:    "localhost:50051",
		BuilderConfigs: defaultBuilderConfigs(),
		ChainID:        1,
		MaxGasGwei:     300.0,
		TipSharePct:    90.0,
		ExecutorAddr:   "0x0000000000000000000000000000000000000000",
		EthBalance:     0.5,
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

	// Load searcher private key for transaction signing.
	var txSigner *TransactionSigner
	searcherKey := os.Getenv("SEARCHER_KEY")
	os.Unsetenv("SEARCHER_KEY")
	if searcherKey != "" {
		var err error
		txSigner, err = NewTransactionSigner(searcherKey, cfg.ChainID)
		if err != nil {
			log.Fatalf("Failed to load SEARCHER_KEY: %v", err)
		}
		log.Printf("Searcher address: %s", txSigner.Address().Hex())
	} else {
		log.Println("WARNING: SEARCHER_KEY not set — transactions will not be signed")
	}

	// Connect to Ethereum node for block header queries (coinbase).
	var ethClient *ethclient.Client
	if rpcURL := os.Getenv("ETH_RPC_URL"); rpcURL != "" {
		dialCtx, dialCancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer dialCancel()
		var err error
		ethClient, err = ethclient.DialContext(dialCtx, rpcURL)
		if err != nil {
			log.Printf("WARNING: failed to connect to ETH_RPC_URL: %v", err)
		} else {
			log.Printf("Connected to Ethereum node for coinbase lookups")
		}
	} else {
		log.Println("WARNING: ETH_RPC_URL not set — coinbase will use zero address")
	}

	// Setup graceful shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Initialize components
	nonceManager := NewNonceManager(0)
	if txSigner != nil {
		nonceManager.SetSyncSource(txSigner.Address(), ethClient)
		if ethClient != nil {
			if err := nonceManager.SyncFromChain(ctx); err != nil {
				log.Printf("WARNING: failed to sync nonce for %s: %v", txSigner.Address().Hex(), err)
			}
		} else {
			log.Printf("WARNING: ETH_RPC_URL not set — nonce manager will not sync on-chain nonce for %s", txSigner.Address().Hex())
		}
	} else {
		log.Printf("WARNING: SEARCHER_KEY not set — nonce manager will use initial nonce 0")
	}

	gasOracle := NewGasOracle(cfg.MaxGasGwei)
	if ethClient != nil {
		gasOracle.SetClient(ethClient)
		// Fetch real gas prices before first arb evaluation.
		if _, err := gasOracle.FetchOnce(ctx); err != nil {
			log.Printf("WARNING: initial gas oracle fetch failed: %v", err)
		}
	}
	submitter := NewSubmitter(cfg.BuilderConfigs)
	bundler := NewBundleConstructor(nonceManager, gasOracle, txSigner, cfg.TipSharePct, cfg.ChainID)
	riskMgr := risk.NewRiskManager(loadRiskConfig())

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

	log.Printf("Executor service started, gRPC target: %s", cfg.GRPCAddress)
	log.Printf("Configured %d builders", len(cfg.BuilderConfigs))

	// Connect to Rust engine gRPC server.
	// grpc.NewClient is lazy — the TCP connection is established on first RPC,
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
			consumeArbStream(ctx, grpcClient, bundler, submitter, riskMgr, ethClient, cfg.ExecutorAddr, cfg.TipSharePct, cfg.EthBalance)
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
// parse -> preflight -> bundle -> submit -> record result
func processArb(
	ctx context.Context,
	arb *pb.ValidatedArb,
	rm *risk.RiskManager,
	bundler *BundleConstructor,
	submitter *Submitter,
	ethClient *ethclient.Client,
	executorAddr string,
	tipSharePct float64,
	ethBalance float64,
) (submitted bool, err error) {
	// Parse net_profit_wei from proto bytes to big.Int
	profitWei := new(big.Int).SetBytes(arb.NetProfitWei)

	// Parse flashloan_amount as trade value
	tradeValueWei := new(big.Int).SetBytes(arb.FlashloanAmount)

	// Get current gas price from the bundler's gas oracle
	gasFees := bundler.gasOracle.CurrentFees()
	gasGwei := gasFees.GasPriceGwei

	// Preflight risk check
	result := rm.PreflightCheck(profitWei, tradeValueWei, gasGwei, tipSharePct, ethBalance)
	if !result.Approved {
		log.Printf("Arb %s rejected by preflight: %s", arb.Id, result.Reason)
		return false, nil
	}

	// Fetch block.coinbase from latest block header for the tip transaction.
	coinbase := common.Address{}
	if ethClient != nil {
		header, headerErr := ethClient.HeaderByNumber(ctx, nil) // nil = latest
		if headerErr != nil {
			log.Printf("WARNING: failed to fetch latest block header: %v, using zero coinbase", headerErr)
		} else {
			coinbase = header.Coinbase
		}
	}

	// Build bundle
	bundle, err := bundler.BuildBundle(arb.Calldata, executorAddr, profitWei, arb.TotalGas, arb.BlockNumber+1, coinbase)
	if err != nil {
		return false, fmt.Errorf("build bundle: %w", err)
	}

	// Submit to all builders
	results := submitter.SubmitToAll(ctx, bundle)
	recordSubmissionReverts(rm, results)
	successes := SuccessCount(results)

	log.Printf("Arb %s: submitted to %d builders, %d accepted", arb.Id, len(results), successes)

	// Record result for miss rate tracking
	rm.RecordBundleResult(successes > 0)

	return successes > 0, nil
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
func consumeArbStream(ctx context.Context, client *aethergrpc.Client, bundler *BundleConstructor, submitter *Submitter, rm *risk.RiskManager, ethClient *ethclient.Client, executorAddr string, tipSharePct float64, ethBalance float64) {
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

			log.Printf("Received arb: id=%s hops=%d gas=%d block=%d",
				arb.Id, len(arb.Hops), arb.TotalGas, arb.BlockNumber)

			submitted, err := processArb(ctx, arb, rm, bundler, submitter, ethClient, executorAddr, tipSharePct, ethBalance)
			if err != nil {
				log.Printf("Error processing arb %s: %v", arb.Id, err)
			}
			_ = submitted
		}
	}
}
