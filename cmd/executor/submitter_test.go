package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func testBundleWithRawTxs() *Bundle {
	return &Bundle{
		BlockNumber: 18000000,
		RawTxs:      [][]byte{{0xde, 0xad}, {0xbe, 0xef}},
	}
}

func TestSubmitToAll_AllEnabled(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xabc123"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "builder-a", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "builder-b", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "builder-c", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "builder-d", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}

	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())

	if len(results) != 4 {
		t.Fatalf("expected 4 results, got %d", len(results))
	}

	for _, r := range results {
		if !r.Success {
			t.Errorf("builder %s: expected success, got failure: %v", r.Builder, r.Error)
		}
		if r.BundleHash != "0xabc123" {
			t.Errorf("builder %s: expected hash 0xabc123, got %s", r.Builder, r.BundleHash)
		}
		if r.Latency <= 0 {
			t.Errorf("builder %s: expected positive latency, got %v", r.Builder, r.Latency)
		}
	}
}

func TestSubmitToAll_SomeDisabled(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xok"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "flashbots", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "titan", URL: srv.URL, AuthType: "none", Enabled: false, TimeoutMs: 2000},
		{Name: "beaver", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
		{Name: "rsync", URL: srv.URL, AuthType: "none", Enabled: false, TimeoutMs: 2000},
	}

	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())

	if len(results) != 2 {
		t.Fatalf("expected 2 results (2 disabled), got %d", len(results))
	}

	builderNames := make(map[string]bool)
	for _, r := range results {
		builderNames[r.Builder] = true
	}
	if builderNames["titan"] {
		t.Error("disabled builder 'titan' should not have submitted")
	}
	if builderNames["rsync"] {
		t.Error("disabled builder 'rsync' should not have submitted")
	}
	if !builderNames["flashbots"] {
		t.Error("enabled builder 'flashbots' should have submitted")
	}
	if !builderNames["beaver"] {
		t.Error("enabled builder 'beaver' should have submitted")
	}
}

func TestSubmitToAll_NoRawTxs(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "test", URL: "http://localhost:1", AuthType: "none", Enabled: true, TimeoutMs: 1000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	bundle := &Bundle{BlockNumber: 18000000}

	results := sub.SubmitToAll(context.Background(), bundle)

	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0].Success {
		t.Error("expected failure when no signed txs")
	}
	if !strings.Contains(results[0].Error.Error(), "no signed transactions") {
		t.Errorf("expected 'no signed transactions' error, got: %v", results[0].Error)
	}
}

func TestSubmitToAll_CancelledContext(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "test", URL: "http://localhost:1", AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	results := sub.SubmitToAll(ctx, testBundleWithRawTxs())
	for _, r := range results {
		if r.Builder == "" {
			t.Error("result has empty builder name")
		}
	}
}

func TestSubmitToBuilder_EthSendBundleFormat(t *testing.T) {
	t.Parallel()

	var capturedBody []byte
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		capturedBody = body

		if ct := r.Header.Get("Content-Type"); ct != "application/json" {
			t.Errorf("expected Content-Type application/json, got %s", ct)
		}

		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xdeadbeef"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "test-builder", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	bundle := &Bundle{
		BlockNumber: 0x112A880, // 18000000
		RawTxs:      [][]byte{{0xaa, 0xbb}, {0xcc, 0xdd}},
	}

	results := sub.SubmitToAll(context.Background(), bundle)
	if len(results) != 1 || !results[0].Success {
		t.Fatalf("expected 1 successful result, got %v", results)
	}

	// Verify JSON-RPC request format.
	var req struct {
		JSONRPC string        `json:"jsonrpc"`
		Method  string        `json:"method"`
		Params  []interface{} `json:"params"`
	}
	if err := json.Unmarshal(capturedBody, &req); err != nil {
		t.Fatalf("unmarshal captured request: %v", err)
	}
	if req.JSONRPC != "2.0" {
		t.Errorf("jsonrpc = %s, want 2.0", req.JSONRPC)
	}
	if req.Method != "eth_sendBundle" {
		t.Errorf("method = %s, want eth_sendBundle", req.Method)
	}
	if len(req.Params) != 1 {
		t.Fatalf("expected 1 param set, got %d", len(req.Params))
	}

	params, ok := req.Params[0].(map[string]interface{})
	if !ok {
		t.Fatalf("params[0] is not a map")
	}
	txs, ok := params["txs"].([]interface{})
	if !ok {
		t.Fatalf("txs is not an array")
	}
	if len(txs) != 2 {
		t.Fatalf("expected 2 txs, got %d", len(txs))
	}
	if txs[0].(string) != "0xaabb" {
		t.Errorf("tx[0] = %s, want 0xaabb", txs[0])
	}
	if txs[1].(string) != "0xccdd" {
		t.Errorf("tx[1] = %s, want 0xccdd", txs[1])
	}
	if params["blockNumber"].(string) != "0x112a880" {
		t.Errorf("blockNumber = %s, want 0x112a880", params["blockNumber"])
	}
}

func TestSubmitToBuilder_FlashbotsSignature(t *testing.T) {
	t.Parallel()

	var capturedSigHeader string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedSigHeader = r.Header.Get("X-Flashbots-Signature")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xfb1234"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "flashbots", URL: srv.URL, AuthType: "flashbots", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, testSearcherKey)
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 || !results[0].Success {
		t.Fatalf("expected successful submission, got %v", results)
	}

	if capturedSigHeader == "" {
		t.Fatal("X-Flashbots-Signature header missing")
	}
	parts := strings.SplitN(capturedSigHeader, ":", 2)
	if len(parts) != 2 {
		t.Fatalf("expected address:signature format, got %q", capturedSigHeader)
	}
	if !strings.HasPrefix(parts[0], "0x") || len(parts[0]) != 42 {
		t.Errorf("address part invalid: %s", parts[0])
	}
	if !strings.HasPrefix(parts[1], "0x") || len(parts[1]) != 132 {
		t.Errorf("signature part invalid (len=%d): %s", len(parts[1]), parts[1])
	}
}

func TestSubmitToBuilder_FlashbotsWithoutKey(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "flashbots", URL: "http://localhost:1", AuthType: "flashbots", Enabled: true, TimeoutMs: 1000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0].Success {
		t.Error("expected failure without searcher key for flashbots auth")
	}
	if !strings.Contains(results[0].Error.Error(), "no searcher key") {
		t.Errorf("expected 'no searcher key' error, got: %v", results[0].Error)
	}
}

func TestSubmitToBuilder_ApiKeyAuth(t *testing.T) {
	t.Parallel()

	var capturedApiKey string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		capturedApiKey = r.Header.Get("X-Api-Key")
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xapi123"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "titan", URL: srv.URL, AuthType: "api_key", AuthKey: "test-api-key-123", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 || !results[0].Success {
		t.Fatalf("expected success, got %v", results)
	}
	if capturedApiKey != "test-api-key-123" {
		t.Errorf("X-Api-Key = %q, want test-api-key-123", capturedApiKey)
	}
}

func TestSubmitToBuilder_BuilderRejection(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"bundle simulation reverted"}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "test-builder", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0].Success {
		t.Error("expected failure on RPC error response")
	}
	if !strings.Contains(results[0].Error.Error(), "bundle simulation reverted") {
		t.Errorf("expected rejection message in error, got: %v", results[0].Error)
	}
	if !strings.Contains(results[0].Error.Error(), "-32000") {
		t.Errorf("expected error code -32000 in error, got: %v", results[0].Error)
	}
}

func TestSubmitToBuilder_HTTPError(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = w.Write([]byte("internal server error"))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "bad-builder", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0].Success {
		t.Error("expected failure on HTTP 500")
	}
	if !strings.Contains(results[0].Error.Error(), "HTTP 500") {
		t.Errorf("expected HTTP 500 in error, got: %v", results[0].Error)
	}
}

func TestSubmitToBuilder_Timeout(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(5 * time.Second)
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "slow-builder", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 100},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	results := sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	if results[0].Success {
		t.Error("expected failure on timeout")
	}
}

func TestMetrics_PerBuilderTracking(t *testing.T) {
	t.Parallel()

	var callCount atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		n := callCount.Add(1)
		w.Header().Set("Content-Type", "application/json")
		if n%2 == 0 {
			_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"rejected"}}`))
		} else {
			_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0xok"}}`))
		}
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "metric-builder", URL: srv.URL, AuthType: "none", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	for i := 0; i < 3; i++ {
		sub.SubmitToAll(context.Background(), testBundleWithRawTxs())
	}

	metrics := sub.Metrics()
	m, ok := metrics["metric-builder"]
	if !ok {
		t.Fatal("expected metrics for 'metric-builder'")
	}
	if m.Total.Load() != 3 {
		t.Errorf("total = %d, want 3", m.Total.Load())
	}
	if m.Successes.Load()+m.Failures.Load() != 3 {
		t.Errorf("successes(%d) + failures(%d) != 3", m.Successes.Load(), m.Failures.Load())
	}
	if m.LastLatency.Load() <= 0 {
		t.Error("expected positive last latency")
	}
}

func TestGetBundleStats(t *testing.T) {
	t.Parallel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		var req struct {
			Method string `json:"method"`
		}
		_ = json.Unmarshal(body, &req)

		if req.Method != "flashbots_getBundleStatsV2" {
			t.Errorf("expected method flashbots_getBundleStatsV2, got %s", req.Method)
		}

		sig := r.Header.Get("X-Flashbots-Signature")
		if sig == "" {
			t.Error("X-Flashbots-Signature header missing on stats request")
		}

		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"jsonrpc":"2.0","id":1,"result":{"isSimulated":true,"isSentToMiners":true}}`))
	}))
	defer srv.Close()

	builders := []BuilderConfig{
		{Name: "flashbots", URL: srv.URL, AuthType: "flashbots", Enabled: true, TimeoutMs: 2000},
	}
	sub, err := NewSubmitter(builders, testSearcherKey)
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	result, err := sub.GetBundleStats(context.Background(), "0xabc123", 18000000)
	if err != nil {
		t.Fatalf("GetBundleStats: %v", err)
	}

	var stats map[string]interface{}
	if err := json.Unmarshal(result, &stats); err != nil {
		t.Fatalf("unmarshal stats: %v", err)
	}
	if stats["isSimulated"] != true {
		t.Errorf("isSimulated = %v, want true", stats["isSimulated"])
	}
}

func TestGetBundleStats_NoSigner(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "flashbots", URL: "http://localhost:1", AuthType: "flashbots", Enabled: true, TimeoutMs: 1000},
	}
	sub, err := NewSubmitter(builders, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	_, err = sub.GetBundleStats(context.Background(), "0xabc", 18000000)
	if err == nil {
		t.Fatal("expected error without signer")
	}
	if !strings.Contains(err.Error(), "signer required") {
		t.Errorf("expected 'signer required' error, got: %v", err)
	}
}

func TestSuccessCount(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name    string
		results []SubmissionResult
		want    int
	}{
		{"all success", []SubmissionResult{{Success: true}, {Success: true}, {Success: true}}, 3},
		{"all failure", []SubmissionResult{{Success: false}, {Success: false}}, 0},
		{"mixed", []SubmissionResult{{Success: true}, {Success: false}, {Success: true}, {Success: false}}, 2},
		{"empty", []SubmissionResult{}, 0},
		{"single success", []SubmissionResult{{Success: true}}, 1},
		{"single failure", []SubmissionResult{{Success: false}}, 0},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			got := SuccessCount(tc.results)
			if got != tc.want {
				t.Errorf("SuccessCount: got %d, want %d", got, tc.want)
			}
		})
	}
}

func TestNewSubmitter(t *testing.T) {
	t.Parallel()

	builders := []BuilderConfig{
		{Name: "builder-a", URL: "https://a.example.com", AuthType: "none", Enabled: true, TimeoutMs: 1000},
		{Name: "builder-b", URL: "https://b.example.com", AuthType: "api_key", AuthKey: "key", Enabled: false, TimeoutMs: 2000},
		{Name: "builder-c", URL: "https://c.example.com", AuthType: "flashbots", Enabled: true, TimeoutMs: 3000},
	}

	sub, err := NewSubmitter(builders, testSearcherKey)
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}

	if len(sub.builders) != 3 {
		t.Fatalf("expected 3 builders, got %d", len(sub.builders))
	}
	if sub.httpClient == nil {
		t.Error("expected non-nil HTTP client")
	}
	if sub.signer == nil {
		t.Error("expected non-nil signer when searcher key provided")
	}

	metrics := sub.Metrics()
	for _, b := range builders {
		if _, ok := metrics[b.Name]; !ok {
			t.Errorf("missing metrics for builder %s", b.Name)
		}
	}
}

func TestNewSubmitter_NoKey(t *testing.T) {
	t.Parallel()

	sub, err := NewSubmitter([]BuilderConfig{{Name: "test", URL: "http://localhost", Enabled: true, TimeoutMs: 1000}}, "")
	if err != nil {
		t.Fatalf("NewSubmitter: %v", err)
	}
	if sub.signer != nil {
		t.Error("expected nil signer when no searcher key")
	}
}

func TestNewSubmitter_InvalidKey(t *testing.T) {
	t.Parallel()

	_, err := NewSubmitter([]BuilderConfig{{Name: "test", URL: "http://localhost", Enabled: true, TimeoutMs: 1000}}, "not-a-valid-hex-key")
	if err == nil {
		t.Fatal("expected error with invalid key")
	}
}

func TestTruncateBytes(t *testing.T) {
	t.Parallel()

	short := []byte("hello")
	if got := truncateBytes(short, 512); got != "hello" {
		t.Errorf("truncateBytes(short) = %q, want %q", got, "hello")
	}

	long := make([]byte, 1024)
	for i := range long {
		long[i] = 'x'
	}
	got := truncateBytes(long, 512)
	if len(got) != 512+len("...(truncated)") {
		t.Errorf("truncateBytes(long) length = %d, want %d", len(got), 512+len("...(truncated)"))
	}
	if !strings.HasSuffix(got, "...(truncated)") {
		t.Errorf("truncateBytes(long) should end with ...(truncated)")
	}
}
