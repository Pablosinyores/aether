package risk

import (
	"sync"
	"testing"
)

func TestNewSystemStateMachine(t *testing.T) {
	t.Parallel()

	sm := NewSystemStateMachine()
	if got := sm.Current(); got != StateRunning {
		t.Errorf("initial state: got %s, want %s", got, StateRunning)
	}
}

func TestTransition_ValidPaths(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		from SystemState
		to   SystemState
	}{
		{"Running->Degraded", StateRunning, StateDegraded},
		{"Running->Paused", StateRunning, StatePaused},
		{"Running->Halted", StateRunning, StateHalted},
		{"Degraded->Running", StateDegraded, StateRunning},
		{"Degraded->Paused", StateDegraded, StatePaused},
		{"Degraded->Halted", StateDegraded, StateHalted},
		{"Paused->Running", StatePaused, StateRunning},
		{"Paused->Degraded", StatePaused, StateDegraded},
		{"Paused->Halted", StatePaused, StateHalted},
		{"Halted->Running", StateHalted, StateRunning},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			sm := NewSystemStateMachine()
			// Force to the starting state
			sm.ForceState(tc.from)

			err := sm.Transition(tc.to)
			if err != nil {
				t.Errorf("Transition(%s -> %s) returned error: %v", tc.from, tc.to, err)
			}
			if got := sm.Current(); got != tc.to {
				t.Errorf("after Transition: state=%s, want %s", got, tc.to)
			}
		})
	}
}

func TestTransition_InvalidPaths(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		from SystemState
		to   SystemState
	}{
		{"Halted->Degraded", StateHalted, StateDegraded},
		{"Halted->Paused", StateHalted, StatePaused},
		{"Running->Running", StateRunning, StateRunning},
		{"Degraded->Degraded", StateDegraded, StateDegraded},
		{"Paused->Paused", StatePaused, StatePaused},
		{"Halted->Halted", StateHalted, StateHalted},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			sm := NewSystemStateMachine()
			sm.ForceState(tc.from)

			err := sm.Transition(tc.to)
			if err == nil {
				t.Errorf("Transition(%s -> %s) should have returned error, got nil", tc.from, tc.to)
			}

			// State should remain unchanged
			if got := sm.Current(); got != tc.from {
				t.Errorf("state changed on invalid transition: got %s, want %s", got, tc.from)
			}
		})
	}
}

func TestForceState(t *testing.T) {
	t.Parallel()

	// ForceState should work for any transition, even invalid ones
	tests := []struct {
		name string
		from SystemState
		to   SystemState
	}{
		{"Halted->Degraded", StateHalted, StateDegraded},
		{"Halted->Paused", StateHalted, StatePaused},
		{"Running->Running", StateRunning, StateRunning},
		{"Paused->Halted", StatePaused, StateHalted},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			sm := NewSystemStateMachine()
			sm.ForceState(tc.from)
			sm.ForceState(tc.to)

			if got := sm.Current(); got != tc.to {
				t.Errorf("ForceState(%s): got %s, want %s", tc.to, got, tc.to)
			}
		})
	}
}

func TestCurrent_ThreadSafe(t *testing.T) {
	t.Parallel()

	sm := NewSystemStateMachine()

	var wg sync.WaitGroup
	const readers = 100

	wg.Add(readers)
	for i := 0; i < readers; i++ {
		go func() {
			defer wg.Done()
			for j := 0; j < 100; j++ {
				state := sm.Current()
				// State should always be a valid value
				switch state {
				case StateRunning, StateDegraded, StatePaused, StateHalted:
					// OK
				default:
					t.Errorf("unexpected state: %s", state)
				}
			}
		}()
	}

	wg.Wait()
	// No panics = pass
}
