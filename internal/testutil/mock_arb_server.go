// Package testutil provides test helpers and mock gRPC servers for
// integration testing the Go executor services against the proto-defined
// ArbService, HealthService, and ControlService interfaces.
package testutil

import (
	"context"
	"fmt"
	"net"
	"sync"

	pb "github.com/aether-arb/aether/internal/pb"
	"google.golang.org/grpc"
)

// MockArbServer implements all three proto services for integration tests.
// It lets callers configure which arbs are streamed, health responses, and
// control outcomes.
type MockArbServer struct {
	pb.UnimplementedArbServiceServer
	pb.UnimplementedHealthServiceServer
	pb.UnimplementedControlServiceServer

	mu   sync.Mutex
	arbs []*pb.ValidatedArb

	// HealthStatus controls the response from Check(). Default: RUNNING.
	HealthStatus pb.SystemState

	// SubmitArbError if set, SubmitArb returns this error.
	SubmitArbError error

	// StreamDelay if > 0, the stream sends arbs one at a time with no delay
	// but can be used by tests to verify ordering.

	grpcServer *grpc.Server
	listener   net.Listener
}

// NewMockArbServer creates a mock server with default healthy state.
func NewMockArbServer() *MockArbServer {
	return &MockArbServer{
		HealthStatus: pb.SystemState_RUNNING,
	}
}

// SetArbs configures the arbs that will be streamed to clients.
func (m *MockArbServer) SetArbs(arbs []*pb.ValidatedArb) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.arbs = arbs
}

// Start starts the gRPC server on a random available port.
// Returns the address (host:port) the server is listening on.
func (m *MockArbServer) Start() (string, error) {
	lis, err := net.Listen("tcp", "localhost:0")
	if err != nil {
		return "", fmt.Errorf("listen: %w", err)
	}
	m.listener = lis

	m.grpcServer = grpc.NewServer()
	pb.RegisterArbServiceServer(m.grpcServer, m)
	pb.RegisterHealthServiceServer(m.grpcServer, m)
	pb.RegisterControlServiceServer(m.grpcServer, m)

	go func() {
		_ = m.grpcServer.Serve(lis)
	}()

	return lis.Addr().String(), nil
}

// Stop gracefully shuts down the gRPC server.
func (m *MockArbServer) Stop() {
	if m.grpcServer != nil {
		m.grpcServer.GracefulStop()
	}
}

// Addr returns the listener address. Only valid after Start().
func (m *MockArbServer) Addr() string {
	if m.listener != nil {
		return m.listener.Addr().String()
	}
	return ""
}

// --- ArbService ---

// SubmitArb accepts a ValidatedArb and returns success.
func (m *MockArbServer) SubmitArb(_ context.Context, arb *pb.ValidatedArb) (*pb.SubmitArbResponse, error) {
	if m.SubmitArbError != nil {
		return nil, m.SubmitArbError
	}
	return &pb.SubmitArbResponse{
		Accepted:   true,
		BundleHash: fmt.Sprintf("mock-bundle-%s", arb.Id),
	}, nil
}

// StreamArbs streams all configured arbs to the client, then closes.
func (m *MockArbServer) StreamArbs(req *pb.StreamArbsRequest, stream grpc.ServerStreamingServer[pb.ValidatedArb]) error {
	m.mu.Lock()
	arbs := make([]*pb.ValidatedArb, len(m.arbs))
	copy(arbs, m.arbs)
	m.mu.Unlock()

	minProfit := req.GetMinProfitEth()

	for _, arb := range arbs {
		// Apply min profit filter (matching Rust server behavior)
		if minProfit > 0 {
			profitWei := profitWeiToFloat(arb.NetProfitWei)
			if profitWei < minProfit {
				continue
			}
		}

		if err := stream.Send(arb); err != nil {
			return err
		}
	}

	return nil
}

// profitWeiToFloat converts raw wei bytes to ETH float for filtering.
func profitWeiToFloat(weiBytes []byte) float64 {
	if len(weiBytes) == 0 {
		return 0
	}
	// Simple approximation: parse bytes as uint, divide by 1e18
	var val uint64
	for _, b := range weiBytes {
		val = val<<8 | uint64(b)
	}
	return float64(val) / 1e18
}

// --- HealthService ---

// Check returns the configured health status.
func (m *MockArbServer) Check(_ context.Context, _ *pb.HealthCheckRequest) (*pb.HealthCheckResponse, error) {
	m.mu.Lock()
	status := m.HealthStatus
	m.mu.Unlock()

	healthy := status == pb.SystemState_RUNNING || status == pb.SystemState_DEGRADED
	return &pb.HealthCheckResponse{
		Healthy:       healthy,
		Status:        status.String(),
		UptimeSeconds: 3600,
	}, nil
}

// --- ControlService ---

// SetState updates the mock health status.
func (m *MockArbServer) SetState(_ context.Context, req *pb.SetStateRequest) (*pb.SetStateResponse, error) {
	m.mu.Lock()
	prev := m.HealthStatus
	m.HealthStatus = req.State
	m.mu.Unlock()

	return &pb.SetStateResponse{
		Success:       true,
		PreviousState: prev,
	}, nil
}

// ReloadConfig is a no-op in the mock.
func (m *MockArbServer) ReloadConfig(_ context.Context, _ *pb.ReloadConfigRequest) (*pb.ReloadConfigResponse, error) {
	return &pb.ReloadConfigResponse{
		Success:     true,
		PoolsLoaded: 100,
	}, nil
}
