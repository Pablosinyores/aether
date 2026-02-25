package main

// Risk management logic has been extracted to internal/risk/ package
// so it can be shared across services (executor, risk, monitor).
//
// This file is intentionally kept minimal. The cmd/risk binary serves
// as a standalone risk management service entry point.
// All types and logic live in:
//   - internal/risk/state.go   — SystemState, SystemStateMachine
//   - internal/risk/manager.go — RiskConfig, RiskManager, PreflightCheck
