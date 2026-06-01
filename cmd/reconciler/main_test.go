package main

import (
	"testing"
	"time"
)

func TestRetentionConfigFromEnv(t *testing.T) {
	t.Run("defaults when unset", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "")
		t.Setenv("MEMPOOL_RETENTION_INTERVAL_MINS", "")
		retain, interval, enabled := retentionConfigFromEnv()
		if !enabled {
			t.Fatal("expected retention enabled by default")
		}
		if retain != defaultRetentionHorizon {
			t.Errorf("retain = %v, want %v", retain, defaultRetentionHorizon)
		}
		if interval != defaultRetentionInterval {
			t.Errorf("interval = %v, want %v", interval, defaultRetentionInterval)
		}
	})

	t.Run("zero disables", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "0")
		if _, _, enabled := retentionConfigFromEnv(); enabled {
			t.Fatal("expected retention disabled at 0")
		}
	})

	t.Run("negative disables", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "-5")
		if _, _, enabled := retentionConfigFromEnv(); enabled {
			t.Fatal("expected retention disabled at negative")
		}
	})

	t.Run("sub-floor horizon clamped to minimum", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "0.1")
		retain, _, enabled := retentionConfigFromEnv()
		if !enabled {
			t.Fatal("expected enabled")
		}
		if retain != minRetentionHorizon {
			t.Errorf("retain = %v, want floor %v", retain, minRetentionHorizon)
		}
	})

	t.Run("custom horizon honored", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "24")
		if retain, _, _ := retentionConfigFromEnv(); retain != 24*time.Hour {
			t.Errorf("retain = %v, want 24h", retain)
		}
	})

	t.Run("sub-floor interval clamped to minimum", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_INTERVAL_MINS", "0.1")
		if _, interval, _ := retentionConfigFromEnv(); interval != minRetentionInterval {
			t.Errorf("interval = %v, want floor %v", interval, minRetentionInterval)
		}
	})

	t.Run("garbage falls back to default, stays enabled", func(t *testing.T) {
		t.Setenv("MEMPOOL_RETENTION_HOURS", "not-a-number")
		retain, _, enabled := retentionConfigFromEnv()
		if !enabled {
			t.Fatal("expected enabled on unparseable value")
		}
		if retain != defaultRetentionHorizon {
			t.Errorf("retain = %v, want default %v", retain, defaultRetentionHorizon)
		}
	})
}
