package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"
)

// Config holds executor service configuration
type Config struct {
	GRPCAddress    string
	BuilderConfigs []BuilderConfig
	SearcherKey    string // Hex-encoded private key (loaded from env/KMS in production)
	ChainID        int64
	MaxGasGwei     float64
	TipSharePct    float64
}

func defaultConfig() Config {
	return Config{
		GRPCAddress:    "localhost:50051",
		BuilderConfigs: defaultBuilderConfigs(),
		ChainID:        1,
		MaxGasGwei:     300.0,
		TipSharePct:    90.0,
	}
}

func main() {
	fmt.Println("aether-executor: bundle construction and submission service")

	cfg := defaultConfig()

	// Initialize components
	nonceManager := NewNonceManager(0)
	gasOracle := NewGasOracle(cfg.MaxGasGwei)
	submitter := NewSubmitter(cfg.BuilderConfigs)
	bundler := NewBundleConstructor(nonceManager, gasOracle, cfg.TipSharePct, cfg.ChainID)

	// Setup graceful shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

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

	// Prevent unused variable errors — these will be used when gRPC client is wired
	_ = submitter
	_ = bundler

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
