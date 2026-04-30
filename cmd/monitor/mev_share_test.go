package main

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
	"time"
)

func TestMempoolTrackingEnabled(t *testing.T) {
	cases := []struct {
		val  string
		want bool
	}{
		{"1", true},
		{"true", true},
		{"TRUE", true},
		{"yes", true},
		{"on", true},
		{"off", false},
		{"0", false},
		{"", false},
	}
	for _, c := range cases {
		os.Setenv("MEMPOOL_TRACKING", c.val)
		got := mempoolTrackingEnabled()
		if got != c.want {
			t.Errorf("MEMPOOL_TRACKING=%q: got %v, want %v", c.val, got, c.want)
		}
	}
	os.Unsetenv("MEMPOOL_TRACKING")
}

func TestMevShareConsumerHandlesValidHint(t *testing.T) {
	metrics := NewMetrics()
	consumer := NewMevShareConsumer("http://example", metrics)
	consumer.handleData(`{"hash":"0xabc","logs":[{"address":"0x01","topics":[],"data":"0x"}],"txs":[{"hash":"0xdef","callData":"0x12"}]}`)
	if got := metrics.MevShareHintsTotal.Load(); got != 1 {
		t.Errorf("hints_total = %d, want 1", got)
	}
	if got := metrics.MevShareHintsWithLogs.Load(); got != 1 {
		t.Errorf("with_logs = %d, want 1", got)
	}
	if got := metrics.MevShareHintsWithCalldata.Load(); got != 1 {
		t.Errorf("with_calldata = %d, want 1", got)
	}
}

func TestMevShareConsumerCountsErrorsOnBadJSON(t *testing.T) {
	metrics := NewMetrics()
	consumer := NewMevShareConsumer("http://example", metrics)
	consumer.handleData(`{"this is not": json`)
	if got := metrics.MevShareErrors.Load(); got != 1 {
		t.Errorf("errors = %d, want 1", got)
	}
	if got := metrics.MevShareHintsTotal.Load(); got != 0 {
		t.Errorf("hints_total should be 0 on bad JSON, got %d", got)
	}
}

func TestMevShareConsumerStreamsAndDecodesEvents(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		flusher, _ := w.(http.Flusher)
		fmt.Fprint(w, ":ping\n\n")
		flusher.Flush()
		fmt.Fprintf(w, "data: %s\n\n", `{"hash":"0xa1","logs":[{"address":"0xa","topics":[],"data":"0x"}]}`)
		flusher.Flush()
		fmt.Fprintf(w, "data: %s\n\n", `{"hash":"0xa2","txs":[{"callData":"0xfeedface"}]}`)
		flusher.Flush()
		// Hold the connection open briefly so the scanner can drain both
		// events before the server closes — without this the test races.
		time.Sleep(150 * time.Millisecond)
	}))
	defer server.Close()

	metrics := NewMetrics()
	consumer := NewMevShareConsumer(server.URL, metrics)
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if err := consumer.streamOnce(ctx); err != nil && !strings.Contains(err.Error(), "context") {
		// A clean server close returns nil; a context cancellation is also
		// acceptable. Anything else is a real failure.
		t.Fatalf("streamOnce returned unexpected error: %v", err)
	}
	if got := metrics.MevShareHintsTotal.Load(); got != 2 {
		t.Errorf("hints_total = %d, want 2", got)
	}
	if got := metrics.MevShareHintsWithLogs.Load(); got != 1 {
		t.Errorf("with_logs = %d, want 1", got)
	}
	if got := metrics.MevShareHintsWithCalldata.Load(); got != 1 {
		t.Errorf("with_calldata = %d, want 1", got)
	}
}
