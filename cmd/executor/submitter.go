package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/trace"
)

// BuilderConfig holds configuration for a block builder.
type BuilderConfig struct {
	Name      string
	URL       string
	AuthKey   string
	AuthType  string // "flashbots", "api_key", or "none"
	Enabled   bool
	TimeoutMs int
}

func defaultBuilderConfigs() []BuilderConfig {
	return []BuilderConfig{
		{Name: "flashbots", URL: "https://relay.flashbots.net", AuthType: "flashbots", Enabled: true, TimeoutMs: 2000},
		{Name: "titan", URL: "https://rpc.titanbuilder.xyz", AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "beaver", URL: "https://rpc.beaverbuild.org", AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "rsync", URL: "https://rsync-builder.xyz", AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
}

// SubmissionResult represents the result of submitting to a single builder.
type SubmissionResult struct {
	Builder    string
	Success    bool
	BundleHash string
	Error      error
	Latency    time.Duration
}

// BuilderMetrics tracks per-builder submission statistics using atomic counters.
type BuilderMetrics struct {
	Total        atomic.Int64
	Successes    atomic.Int64
	Failures     atomic.Int64
	LastLatency  atomic.Int64 // microseconds
	TotalLatency atomic.Int64 // microseconds
}

// Submitter handles fan-out submission to multiple block builders via
// the eth_sendBundle JSON-RPC method.
type Submitter struct {
	builders   []BuilderConfig
	submitFn   func(context.Context, BuilderConfig, *Bundle) SubmissionResult
	httpClient *http.Client
	signer     *FlashbotsSigner
	metrics    map[string]*BuilderMetrics
	mu         sync.RWMutex
}

// NewSubmitter creates a new submitter with real HTTP-based bundle submission.
// If searcherKey is non-empty, a FlashbotsSigner is created for signing requests
// to builders that require Flashbots-style authentication.
func NewSubmitter(builders []BuilderConfig, searcherKey string) (*Submitter, error) {
	var signer *FlashbotsSigner
	if searcherKey != "" {
		var err error
		signer, err = NewFlashbotsSigner(searcherKey)
		if err != nil {
			return nil, fmt.Errorf("create flashbots signer: %w", err)
		}
	}

	metrics := make(map[string]*BuilderMetrics, len(builders))
	names := make([]string, 0, len(builders))
	for _, b := range builders {
		metrics[b.Name] = &BuilderMetrics{}
		names = append(names, b.Name)
	}
	// Ensure both {result="success"} and {result="failure"} series exist
	// for every configured builder from t=0 so the AetherBuilderDown alert
	// can reason about builders that have not yet produced either outcome.
	PreRegisterBuilderLabels(names)

	transport := &http.Transport{
		MaxIdleConnsPerHost: len(builders),
		ForceAttemptHTTP2:   true,
		IdleConnTimeout:     90 * time.Second,
	}

	return &Submitter{
		builders: builders,
		httpClient: &http.Client{
			Transport: transport,
		},
		signer:  signer,
		metrics: metrics,
	}, nil
}

// SubmitToAll sends the bundle to all enabled builders concurrently.
func (s *Submitter) SubmitToAll(ctx context.Context, bundle *Bundle) []SubmissionResult {
	enabled := 0
	for _, b := range s.builders {
		if b.Enabled {
			enabled++
		}
	}

	ctx, fanoutSpan := tracer.Start(ctx, "submit.fanout",
		trace.WithAttributes(attribute.Int("builders_enabled", enabled)),
	)
	defer fanoutSpan.End()

	if s.submitFn == nil && len(bundle.RawTxs) == 0 {
		fanoutSpan.SetStatus(codes.Error, "no signed transactions in bundle")
		return []SubmissionResult{{
			Builder: "all",
			Success: false,
			Error:   fmt.Errorf("no signed transactions in bundle"),
		}}
	}

	var wg sync.WaitGroup
	resultCh := make(chan SubmissionResult, len(s.builders))

	for _, builder := range s.builders {
		if !builder.Enabled {
			continue
		}
		wg.Add(1)
		go func(b BuilderConfig) {
			defer wg.Done()
			start := time.Now()

			builderCtx, builderSpan := tracer.Start(ctx, "submit.builder",
				trace.WithAttributes(attribute.String("builder", b.Name)),
			)
			result := s.submitToBuilder(builderCtx, b, bundle)
			result.Latency = time.Since(start)
			s.recordMetrics(b.Name, result)
			if result.Success {
				builderSpan.SetAttributes(attribute.String("bundle_hash", result.BundleHash))
			} else if result.Error != nil {
				builderSpan.RecordError(result.Error)
				builderSpan.SetStatus(codes.Error, "builder rejected bundle")
			}
			builderSpan.End()

			resultCh <- result
		}(builder)
	}

	// Close channel when all goroutines complete.
	go func() {
		wg.Wait()
		close(resultCh)
	}()

	results := make([]SubmissionResult, 0, len(s.builders))
	for result := range resultCh {
		if result.Success {
			slog.Info("bundle accepted by builder",
				"builder", result.Builder,
				"bundle_hash", result.BundleHash,
				"latency", result.Latency)
		} else {
			slog.Warn("bundle rejected by builder",
				"builder", result.Builder,
				"err", result.Error,
				"latency", result.Latency)
		}
		results = append(results, result)
	}

	return results
}

// jsonRPCRequest is the standard JSON-RPC 2.0 request envelope.
type jsonRPCRequest struct {
	JSONRPC string        `json:"jsonrpc"`
	ID      int           `json:"id"`
	Method  string        `json:"method"`
	Params  []interface{} `json:"params"`
}

// jsonRPCResponse is the standard JSON-RPC 2.0 response envelope.
type jsonRPCResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      int             `json:"id"`
	Result  json.RawMessage `json:"result,omitempty"`
	Error   *jsonRPCError   `json:"error,omitempty"`
}

// jsonRPCError represents a JSON-RPC error object.
type jsonRPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

// submitToBuilder sends the bundle to a single builder via HTTP POST.
func (s *Submitter) submitToBuilder(ctx context.Context, builder BuilderConfig, bundle *Bundle) SubmissionResult {
	if s.submitFn != nil {
		res := s.submitFn(ctx, builder, bundle)
		if res.Builder == "" {
			res.Builder = builder.Name
		}
		return res
	}

	timeout := time.Duration(builder.TimeoutMs) * time.Millisecond
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	// Hex-encode each raw signed transaction.
	txHexes := make([]string, len(bundle.RawTxs))
	for i, raw := range bundle.RawTxs {
		txHexes[i] = "0x" + hex.EncodeToString(raw)
	}

	params := map[string]interface{}{
		"txs":         txHexes,
		"blockNumber": fmt.Sprintf("0x%x", bundle.BlockNumber),
	}

	reqBody := jsonRPCRequest{
		JSONRPC: "2.0",
		ID:      1,
		Method:  "eth_sendBundle",
		Params:  []interface{}{params},
	}

	bodyBytes, err := json.Marshal(reqBody)
	if err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("marshal request: %w", err),
		}
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, builder.URL, bytes.NewReader(bodyBytes))
	if err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("create request: %w", err),
		}
	}
	req.Header.Set("Content-Type", "application/json")

	if err := s.setAuthHeaders(req, builder, bodyBytes); err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   err,
		}
	}

	resp, err := s.httpClient.Do(req)
	if err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("http request to %s: %w", builder.Name, err),
		}
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20)) // 1MB limit
	if err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("read response from %s: %w", builder.Name, err),
		}
	}

	if resp.StatusCode != http.StatusOK {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("builder %s returned HTTP %d: %s", builder.Name, resp.StatusCode, truncateBytes(respBody, 512)),
		}
	}

	var rpcResp jsonRPCResponse
	if err := json.Unmarshal(respBody, &rpcResp); err != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("parse response from %s: %w", builder.Name, err),
		}
	}

	if rpcResp.Error != nil {
		return SubmissionResult{
			Builder: builder.Name,
			Success: false,
			Error:   fmt.Errorf("builder %s rejected bundle: code=%d msg=%s", builder.Name, rpcResp.Error.Code, rpcResp.Error.Message),
		}
	}

	// Extract bundle hash from result if present.
	var result struct {
		BundleHash string `json:"bundleHash"`
	}
	if rpcResp.Result != nil {
		if err := json.Unmarshal(rpcResp.Result, &result); err != nil {
			slog.Warn("builder returned unparseable result", "builder", builder.Name, "err", err, "raw", string(rpcResp.Result))
		}
	}

	bundleHash := result.BundleHash
	if bundleHash == "" {
		bundleHash = fmt.Sprintf("0x%s", GenerateBundleID())
	}

	return SubmissionResult{
		Builder:    builder.Name,
		Success:    true,
		BundleHash: bundleHash,
	}
}

// setAuthHeaders adds authentication headers based on the builder's auth type.
func (s *Submitter) setAuthHeaders(req *http.Request, builder BuilderConfig, body []byte) error {
	switch builder.AuthType {
	case "flashbots":
		if s.signer == nil {
			return fmt.Errorf("builder %s requires flashbots auth but no searcher key configured", builder.Name)
		}
		sig, err := s.signer.Sign(body)
		if err != nil {
			return fmt.Errorf("sign request for %s: %w", builder.Name, err)
		}
		req.Header.Set("X-Flashbots-Signature", sig)
	case "api_key":
		req.Header.Set("X-Api-Key", builder.AuthKey)
	case "none", "":
		// No auth required.
	}
	return nil
}

// recordMetrics updates atomic counters for a builder's submission result.
func (s *Submitter) recordMetrics(builder string, result SubmissionResult) {
	s.mu.RLock()
	m, ok := s.metrics[builder]
	s.mu.RUnlock()
	if !ok {
		return
	}

	m.Total.Add(1)
	if result.Success {
		m.Successes.Add(1)
	} else {
		m.Failures.Add(1)
	}
	latencyUs := result.Latency.Microseconds()
	m.LastLatency.Store(latencyUs)
	m.TotalLatency.Add(latencyUs)

	recordBuilderResult(builder, result.Success, result.Latency)
}

// Metrics returns a snapshot of per-builder metrics.
func (s *Submitter) Metrics() map[string]*BuilderMetrics {
	s.mu.RLock()
	defer s.mu.RUnlock()

	// Return the same map — callers read atomics, no copy needed.
	return s.metrics
}

// GetBundleStats queries the Flashbots relay for bundle inclusion stats.
// Only works when a FlashbotsSigner is configured.
func (s *Submitter) GetBundleStats(ctx context.Context, bundleHash string, blockNumber uint64) (json.RawMessage, error) {
	if s.signer == nil {
		return nil, fmt.Errorf("signer required for flashbots_getBundleStatsV2")
	}

	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	// Find the flashbots builder URL.
	var flashbotsURL string
	for _, b := range s.builders {
		if b.AuthType == "flashbots" {
			flashbotsURL = b.URL
			break
		}
	}
	if flashbotsURL == "" {
		return nil, fmt.Errorf("no flashbots builder configured")
	}

	params := map[string]interface{}{
		"bundleHash":  bundleHash,
		"blockNumber": fmt.Sprintf("0x%x", blockNumber),
	}

	reqBody := jsonRPCRequest{
		JSONRPC: "2.0",
		ID:      1,
		Method:  "flashbots_getBundleStatsV2",
		Params:  []interface{}{params},
	}

	bodyBytes, err := json.Marshal(reqBody)
	if err != nil {
		return nil, fmt.Errorf("marshal stats request: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, flashbotsURL, bytes.NewReader(bodyBytes))
	if err != nil {
		return nil, fmt.Errorf("create stats request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")

	sig, err := s.signer.Sign(bodyBytes)
	if err != nil {
		return nil, fmt.Errorf("sign stats request: %w", err)
	}
	req.Header.Set("X-Flashbots-Signature", sig)

	resp, err := s.httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("stats request: %w", err)
	}
	defer resp.Body.Close()

	respBody, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20)) // 1MB limit
	if err != nil {
		return nil, fmt.Errorf("read stats response: %w", err)
	}

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("stats request returned HTTP %d: %s", resp.StatusCode, truncateBytes(respBody, 512))
	}

	var rpcResp jsonRPCResponse
	if err := json.Unmarshal(respBody, &rpcResp); err != nil {
		return nil, fmt.Errorf("parse stats response: %w", err)
	}

	if rpcResp.Error != nil {
		return nil, fmt.Errorf("stats error: code=%d msg=%s", rpcResp.Error.Code, rpcResp.Error.Message)
	}

	return rpcResp.Result, nil
}

// SuccessCount returns how many builders accepted the bundle.
func SuccessCount(results []SubmissionResult) int {
	count := 0
	for _, r := range results {
		if r.Success {
			count++
		}
	}
	return count
}

// truncateBytes truncates a byte slice to max bytes for log output,
// appending a truncation marker if the input exceeds the limit.
func truncateBytes(b []byte, max int) string {
	if len(b) <= max {
		return string(b)
	}
	return string(b[:max]) + "...(truncated)"
}
