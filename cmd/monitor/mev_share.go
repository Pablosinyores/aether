package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"strings"
	"time"
)

// DefaultMevShareEndpoint is the production Flashbots MEV-Share SSE stream.
// Override at runtime via MEV_SHARE_URL when pointing at the Sepolia endpoint
// or a local mock for development.
const DefaultMevShareEndpoint = "https://mev-share.flashbots.net"

// MevShareHint mirrors the public Flashbots MEV-Share event schema. Every
// field is optional — a sender controls which fields they share by setting
// the Flashbots Protect `hints` array, so the consumer must handle partial
// events without erroring. See:
// https://docs.flashbots.net/flashbots-mev-share/searchers/event-stream
type MevShareHint struct {
	Hash string `json:"hash"`
	Logs []struct {
		Address string   `json:"address"`
		Topics  []string `json:"topics"`
		Data    string   `json:"data"`
	} `json:"logs,omitempty"`
	Txs []struct {
		Hash             string `json:"hash,omitempty"`
		CallData         string `json:"callData,omitempty"`
		FunctionSelector string `json:"functionSelector,omitempty"`
		To               string `json:"to,omitempty"`
		From             string `json:"from,omitempty"`
		Value            string `json:"value,omitempty"`
		MaxFeePerGas     string `json:"maxFeePerGas,omitempty"`
		MaxPriorityFeePerGas string `json:"maxPriorityFeePerGas,omitempty"`
		Nonce            string `json:"nonce,omitempty"`
		ChainID          string `json:"chainId,omitempty"`
		Gas              string `json:"gas,omitempty"`
		Type             string `json:"type,omitempty"`
	} `json:"txs,omitempty"`
}

// MevShareConsumer reads the Flashbots MEV-Share SSE stream and bumps
// metrics for every hint received. It is log-only — no bundle is built and
// no submission is performed.
//
// Reconnection is automatic with exponential backoff. The consumer never
// gives up: if the upstream is down the metrics simply do not move, which
// is observable via the AlertmanagerDown / staleness rules.
type MevShareConsumer struct {
	endpoint string
	metrics  *Metrics
	client   *http.Client
}

// NewMevShareConsumer builds a consumer pinned to `endpoint`. The HTTP
// client uses a long timeout because SSE is a streaming connection — a short
// timeout would force a reconnect every minute regardless of stream health.
func NewMevShareConsumer(endpoint string, metrics *Metrics) *MevShareConsumer {
	if endpoint == "" {
		endpoint = DefaultMevShareEndpoint
	}
	return &MevShareConsumer{
		endpoint: endpoint,
		metrics:  metrics,
		client:   &http.Client{Timeout: 0}, // 0 = no client-side timeout, OK for SSE
	}
}

// Run blocks the calling goroutine, looping over connect → consume → reconnect
// until ctx is cancelled. Errors during a connection are logged and counted
// in `aether_mev_share_errors_total` but do not propagate.
func (c *MevShareConsumer) Run(ctx context.Context) {
	backoff := 2 * time.Second
	const maxBackoff = 30 * time.Second
	for {
		select {
		case <-ctx.Done():
			slog.Info("mev-share consumer shutdown", "endpoint", c.endpoint)
			return
		default:
		}

		if err := c.streamOnce(ctx); err != nil {
			c.metrics.MevShareErrors.Add(1)
			slog.Warn("mev-share stream error",
				"err", err, "backoff", backoff, "endpoint", c.endpoint)
			select {
			case <-ctx.Done():
				return
			case <-time.After(backoff):
			}
			backoff *= 2
			if backoff > maxBackoff {
				backoff = maxBackoff
			}
		} else {
			// Clean exit (server closed): reset backoff for next attempt.
			backoff = 2 * time.Second
		}
	}
}

// streamOnce opens a single SSE connection, parses events line-by-line until
// the server closes or an error occurs.
func (c *MevShareConsumer) streamOnce(ctx context.Context) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.endpoint, nil)
	if err != nil {
		return fmt.Errorf("build request: %w", err)
	}
	req.Header.Set("Accept", "text/event-stream")
	req.Header.Set("Cache-Control", "no-cache")
	req.Header.Set("Connection", "keep-alive")

	resp, err := c.client.Do(req)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("upstream status %d", resp.StatusCode)
	}

	slog.Info("mev-share stream connected", "endpoint", c.endpoint)
	scanner := bufio.NewScanner(resp.Body)
	// SSE events can carry up to a few KB of nested log data; raise the
	// default 64KB scanner buffer to 1MB to be safe.
	scanner.Buffer(make([]byte, 0, 4096), 1<<20)

	var dataLine strings.Builder
	for scanner.Scan() {
		line := scanner.Text()

		// SSE protocol: blank line terminates an event. `:` prefix is a
		// comment / keepalive (Flashbots sends `:ping` every 15s).
		if line == "" {
			if dataLine.Len() > 0 {
				c.handleData(dataLine.String())
				dataLine.Reset()
			}
			continue
		}
		if strings.HasPrefix(line, ":") {
			continue
		}
		if strings.HasPrefix(line, "data:") {
			payload := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
			dataLine.WriteString(payload)
		}
		// `event:` / `id:` / `retry:` lines are ignored — Flashbots does
		// not currently differentiate event types.
	}
	if err := scanner.Err(); err != nil && err != io.EOF {
		return fmt.Errorf("read: %w", err)
	}
	return nil
}

// handleData parses one SSE event payload into a MevShareHint and updates
// metrics. Bad JSON bumps the error counter and is otherwise dropped.
func (c *MevShareConsumer) handleData(payload string) {
	if payload == "" {
		return
	}
	var hint MevShareHint
	if err := json.Unmarshal([]byte(payload), &hint); err != nil {
		c.metrics.MevShareErrors.Add(1)
		slog.Debug("mev-share decode failed", "err", err)
		return
	}

	c.metrics.MevShareHintsTotal.Add(1)
	if len(hint.Logs) > 0 {
		c.metrics.MevShareHintsWithLogs.Add(1)
	}
	for _, tx := range hint.Txs {
		if tx.CallData != "" {
			c.metrics.MevShareHintsWithCalldata.Add(1)
			break
		}
	}

	slog.Debug("mev-share hint",
		"hash", hint.Hash,
		"log_count", len(hint.Logs),
		"tx_count", len(hint.Txs))
}

// mempoolTrackingEnabled returns true when MEMPOOL_TRACKING is set to a
// truthy value. Mirrors the Rust ingestion-side check so both processes
// honour the same flag.
func mempoolTrackingEnabled() bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv("MEMPOOL_TRACKING"))) {
	case "1", "true", "yes", "on":
		return true
	default:
		return false
	}
}
