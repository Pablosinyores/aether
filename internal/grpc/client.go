// Package grpc provides a client wrapper for communicating with the Rust
// engine gRPC server. It exposes the three proto-defined services —
// ArbService, HealthService, and ControlService — behind a single Client
// that manages the underlying connection.
package grpc

import (
	"context"
	"fmt"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"

	pb "github.com/aether-arb/aether/internal/pb"
)

// Client wraps gRPC connections to the Rust engine server.
type Client struct {
	conn    *grpc.ClientConn
	arb     pb.ArbServiceClient
	health  pb.HealthServiceClient
	control pb.ControlServiceClient
}

// Dial creates a new gRPC client targeting the given address.
//
// Supports two transport modes:
//   - UDS (production):  "unix:///var/run/aether/engine.sock"
//   - TCP (development): "localhost:50051" or "[::1]:50051"
//
// grpc.NewClient is lazy — the actual connection is established on the
// first RPC call rather than during Dial, so this function returns quickly
// even if the Rust server is not yet running.
func Dial(addr string) (*Client, error) {
	// grpc-go natively supports the unix:// scheme, so both TCP addresses
	// (e.g. "localhost:50051") and UDS paths (e.g. "unix:///var/run/aether/engine.sock")
	// work without a custom dialer.
	conn, err := grpc.NewClient(addr,
		grpc.WithTransportCredentials(insecure.NewCredentials()),
	)
	if err != nil {
		return nil, fmt.Errorf("grpc new client %s: %w", addr, err)
	}

	return &Client{
		conn:    conn,
		arb:     pb.NewArbServiceClient(conn),
		health:  pb.NewHealthServiceClient(conn),
		control: pb.NewControlServiceClient(conn),
	}, nil
}

// Close shuts down the gRPC connection.
func (c *Client) Close() error {
	return c.conn.Close()
}

// ArbService returns the arb service client stub.
func (c *Client) ArbService() pb.ArbServiceClient {
	return c.arb
}

// HealthService returns the health service client stub.
func (c *Client) HealthService() pb.HealthServiceClient {
	return c.health
}

// ControlService returns the control service client stub.
func (c *Client) ControlService() pb.ControlServiceClient {
	return c.control
}

// CheckHealth sends a health check to the Rust engine with a 3-second timeout.
func (c *Client) CheckHealth(ctx context.Context) (*pb.HealthCheckResponse, error) {
	ctx, cancel := context.WithTimeout(ctx, 3*time.Second)
	defer cancel()
	return c.health.Check(ctx, &pb.HealthCheckRequest{})
}

// StreamArbs opens a server-side streaming connection to receive validated
// arbs from the Rust engine. Only arbs with estimated profit >= minProfitETH
// are delivered.
func (c *Client) StreamArbs(ctx context.Context, minProfitETH float64) (pb.ArbService_StreamArbsClient, error) {
	return c.arb.StreamArbs(ctx, &pb.StreamArbsRequest{
		MinProfitEth: minProfitETH,
	})
}
