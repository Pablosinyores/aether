package main

import (
	"context"
	"fmt"
	"log"
	"sync"
	"time"
)

// BuilderConfig holds configuration for a block builder
type BuilderConfig struct {
	Name      string
	URL       string
	AuthKey   string
	Enabled   bool
	TimeoutMs int
}

func defaultBuilderConfigs() []BuilderConfig {
	return []BuilderConfig{
		{Name: "flashbots", URL: "https://relay.flashbots.net", Enabled: true, TimeoutMs: 2000},
		{Name: "titan", URL: "https://rpc.titanbuilder.xyz", Enabled: true, TimeoutMs: 2000},
		{Name: "beaver", URL: "https://rpc.beaverbuild.org", Enabled: true, TimeoutMs: 2000},
		{Name: "rsync", URL: "https://rsync-builder.xyz", Enabled: true, TimeoutMs: 2000},
	}
}

// SubmissionResult represents the result of submitting to a single builder
type SubmissionResult struct {
	Builder    string
	Success    bool
	BundleHash string
	Error      error
	Latency    time.Duration
}

// Submitter handles fan-out submission to multiple builders
type Submitter struct {
	builders []BuilderConfig
}

// NewSubmitter creates a new submitter
func NewSubmitter(builders []BuilderConfig) *Submitter {
	return &Submitter{builders: builders}
}

// SubmitToAll sends the bundle to all enabled builders concurrently
func (s *Submitter) SubmitToAll(ctx context.Context, bundle *Bundle) []SubmissionResult {
	var wg sync.WaitGroup
	results := make([]SubmissionResult, 0, len(s.builders))
	resultCh := make(chan SubmissionResult, len(s.builders))

	for _, builder := range s.builders {
		if !builder.Enabled {
			continue
		}
		wg.Add(1)
		go func(b BuilderConfig) {
			defer wg.Done()
			start := time.Now()

			result := s.submitToBuilder(ctx, b, bundle)
			result.Latency = time.Since(start)

			resultCh <- result
		}(builder)
	}

	// Close channel when all goroutines complete
	go func() {
		wg.Wait()
		close(resultCh)
	}()

	// Collect results
	for result := range resultCh {
		if result.Success {
			log.Printf("Bundle accepted by %s (hash: %s, latency: %v)",
				result.Builder, result.BundleHash, result.Latency)
		} else {
			log.Printf("Bundle rejected by %s: %v (latency: %v)",
				result.Builder, result.Error, result.Latency)
		}
		results = append(results, result)
	}

	return results
}

// submitToBuilder sends bundle to a single builder
func (s *Submitter) submitToBuilder(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
	timeout := time.Duration(builder.TimeoutMs) * time.Millisecond
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	// In production, this would make an HTTP POST to the builder's eth_sendBundle endpoint
	// For now, simulate the submission
	bundleHash := fmt.Sprintf("0x%s-%s", GenerateBundleID(), builder.Name)

	select {
	case <-ctx.Done():
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   ctx.Err(),
		}
	default:
		return SubmissionResult{
			Builder:    builder.Name,
			Success:    true,
			BundleHash: bundleHash,
		}
	}
}

// SuccessCount returns how many builders accepted the bundle
func SuccessCount(results []SubmissionResult) int {
	count := 0
	for _, r := range results {
		if r.Success {
			count++
		}
	}
	return count
}
