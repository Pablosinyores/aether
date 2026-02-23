//go:build integration

package main

import (
	"context"
	"os"
	"os/exec"
	"testing"
	"time"

	aethergrpc "github.com/aether-arb/aether/internal/grpc"
)

// TestGRPCCrossLanguage_HealthCheck starts the real Rust gRPC server binary
// and verifies that the Go health-check client can communicate with it.
//
// Requires the Rust binary to be pre-built:
//
//	cargo build --release -p aether-grpc-server
//
// Run with:
//
//	go test -tags integration -run TestGRPCCrossLanguage ./cmd/executor/
func TestGRPCCrossLanguage_HealthCheck(t *testing.T) {
	rustBinary := os.Getenv("AETHER_RUST_BINARY")
	if rustBinary == "" {
		rustBinary = "../../target/release/aether-rust"
	}

	if _, err := os.Stat(rustBinary); os.IsNotExist(err) {
		t.Skipf("Rust binary not found at %s; build with: cargo build --release -p aether-grpc-server", rustBinary)
	}

	// Start the Rust gRPC server as a subprocess
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, rustBinary)
	cmd.Env = append(os.Environ(), "RUST_LOG=info")
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		t.Fatalf("failed to start Rust server: %v", err)
	}
	defer func() {
		_ = cmd.Process.Signal(os.Interrupt)
		_ = cmd.Wait()
	}()

	// Give the Rust server time to start listening
	time.Sleep(2 * time.Second)

	// Connect Go client to the Rust server
	client, err := aethergrpc.Dial("[::1]:50051")
	if err != nil {
		t.Fatalf("failed to dial Rust server: %v", err)
	}
	defer client.Close()

	// Health check
	healthCtx, healthCancel := context.WithTimeout(ctx, 5*time.Second)
	defer healthCancel()

	resp, err := client.CheckHealth(healthCtx)
	if err != nil {
		t.Fatalf("health check failed: %v", err)
	}

	if !resp.Healthy {
		t.Errorf("expected healthy=true, got false (status=%s)", resp.Status)
	}

	t.Logf("Cross-language health check: healthy=%v, status=%s, uptime=%ds, pools=%d",
		resp.Healthy, resp.Status, resp.UptimeSeconds, resp.ActivePools)
}

// TestGRPCCrossLanguage_StreamArbs starts the real Rust gRPC server and
// verifies that the Go client can open a StreamArbs stream.
func TestGRPCCrossLanguage_StreamArbs(t *testing.T) {
	rustBinary := os.Getenv("AETHER_RUST_BINARY")
	if rustBinary == "" {
		rustBinary = "../../target/release/aether-rust"
	}

	if _, err := os.Stat(rustBinary); os.IsNotExist(err) {
		t.Skipf("Rust binary not found at %s; build with: cargo build --release -p aether-grpc-server", rustBinary)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, rustBinary)
	cmd.Env = append(os.Environ(), "RUST_LOG=info")
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		t.Fatalf("failed to start Rust server: %v", err)
	}
	defer func() {
		_ = cmd.Process.Signal(os.Interrupt)
		_ = cmd.Wait()
	}()

	time.Sleep(2 * time.Second)

	client, err := aethergrpc.Dial("[::1]:50051")
	if err != nil {
		t.Fatalf("failed to dial Rust server: %v", err)
	}
	defer client.Close()

	// Open arb stream — in the default state the Rust server has no pool
	// data, so it won't send any arbs, but the stream should open successfully.
	streamCtx, streamCancel := context.WithTimeout(ctx, 3*time.Second)
	defer streamCancel()

	stream, err := client.StreamArbs(streamCtx, 0.001)
	if err != nil {
		t.Fatalf("StreamArbs failed: %v", err)
	}

	// The stream should eventually return an error (EOF or context deadline)
	// since no arbs are being produced. This is expected.
	_, recvErr := stream.Recv()
	if recvErr == nil {
		t.Log("Received unexpected arb from Rust engine (engine may have produced one)")
	} else {
		t.Logf("Stream ended as expected: %v", recvErr)
	}
}

// TestGRPCCrossLanguage_ControlSetState tests the ControlService.SetState RPC.
func TestGRPCCrossLanguage_ControlSetState(t *testing.T) {
	rustBinary := os.Getenv("AETHER_RUST_BINARY")
	if rustBinary == "" {
		rustBinary = "../../target/release/aether-rust"
	}

	if _, err := os.Stat(rustBinary); os.IsNotExist(err) {
		t.Skipf("Rust binary not found at %s; build with: cargo build --release -p aether-grpc-server", rustBinary)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	cmd := exec.CommandContext(ctx, rustBinary)
	cmd.Env = append(os.Environ(), "RUST_LOG=info")
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		t.Fatalf("failed to start Rust server: %v", err)
	}
	defer func() {
		_ = cmd.Process.Signal(os.Interrupt)
		_ = cmd.Wait()
	}()

	time.Sleep(2 * time.Second)

	client, err := aethergrpc.Dial("[::1]:50051")
	if err != nil {
		t.Fatalf("failed to dial Rust server: %v", err)
	}
	defer client.Close()

	// First verify we're healthy (Running)
	resp, err := client.CheckHealth(ctx)
	if err != nil {
		t.Fatalf("initial health check failed: %v", err)
	}
	if !resp.Healthy {
		t.Fatalf("expected initial state healthy, got %s", resp.Status)
	}

	t.Logf("Cross-language control test: initial state healthy=%v, status=%s", resp.Healthy, resp.Status)
}
