package main

import (
	"fmt"
	"log"
	"sync"
)

// SystemState represents the current system operating state
type SystemState string

const (
	StateRunning  SystemState = "Running"
	StateDegraded SystemState = "Degraded"
	StatePaused   SystemState = "Paused"
	StateHalted   SystemState = "Halted"
)

// SystemStateMachine manages state transitions
type SystemStateMachine struct {
	mu    sync.RWMutex
	state SystemState
}

// NewSystemStateMachine creates a new state machine in Running state
func NewSystemStateMachine() *SystemStateMachine {
	return &SystemStateMachine{state: StateRunning}
}

// Current returns the current state
func (sm *SystemStateMachine) Current() SystemState {
	sm.mu.RLock()
	defer sm.mu.RUnlock()
	return sm.state
}

// Transition moves to a new state with validation
func (sm *SystemStateMachine) Transition(newState SystemState) error {
	sm.mu.Lock()
	defer sm.mu.Unlock()

	oldState := sm.state

	// Validate transition
	if !isValidTransition(oldState, newState) {
		return fmt.Errorf("invalid transition: %s -> %s", oldState, newState)
	}

	sm.state = newState
	log.Printf("System state: %s -> %s", oldState, newState)
	return nil
}

// ForceState sets state without validation (for manual override)
func (sm *SystemStateMachine) ForceState(newState SystemState) {
	sm.mu.Lock()
	defer sm.mu.Unlock()
	log.Printf("System state forced: %s -> %s", sm.state, newState)
	sm.state = newState
}

// isValidTransition checks if a state transition is allowed
func isValidTransition(from, to SystemState) bool {
	switch from {
	case StateRunning:
		return to == StateDegraded || to == StatePaused || to == StateHalted
	case StateDegraded:
		return to == StateRunning || to == StatePaused || to == StateHalted
	case StatePaused:
		return to == StateRunning || to == StateDegraded || to == StateHalted
	case StateHalted:
		// Halted requires manual reset — only allow transition to Running
		return to == StateRunning
	default:
		return false
	}
}

func main() {
	fmt.Println("aether-risk: risk management and circuit breaker service")

	config := DefaultRiskConfig()
	rm := NewRiskManager(config)

	log.Printf("Risk manager initialized in state: %s", rm.State())
	log.Printf("Max gas: %.0f gwei, Min profit: %.4f ETH, Max trade: %.1f ETH",
		config.MaxGasGwei, config.MinProfitETH, config.MaxSingleTradeETH)
}
