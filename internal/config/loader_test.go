package config

import (
	"os"
	"path/filepath"
	"testing"
)

// ---------------------------------------------------------------------------
// ConfigDir / ConfigPath
// ---------------------------------------------------------------------------

func TestConfigDir_Default(t *testing.T) {
	t.Parallel()

	// Ensure the env var is unset for this test.
	// Since tests may run in parallel, we use a subprocess-style check.
	// For simplicity, we just verify the function returns "./config" when
	// the env var is empty. The env var test below uses t.Setenv.
	orig := os.Getenv("AETHER_CONFIG_DIR")
	if orig != "" {
		t.Skip("AETHER_CONFIG_DIR is set in environment, skipping default test")
	}

	got := ConfigDir()
	if got != "./config" {
		t.Errorf("ConfigDir() = %q, want %q", got, "./config")
	}
}

func TestConfigDir_EnvVar(t *testing.T) {
	t.Setenv("AETHER_CONFIG_DIR", "/custom/config/dir")

	got := ConfigDir()
	if got != "/custom/config/dir" {
		t.Errorf("ConfigDir() = %q, want %q", got, "/custom/config/dir")
	}
}

func TestConfigPath(t *testing.T) {
	t.Setenv("AETHER_CONFIG_DIR", "/myconfig")

	got := ConfigPath("risk.yaml")
	want := "/myconfig/risk.yaml"
	if got != want {
		t.Errorf("ConfigPath(\"risk.yaml\") = %q, want %q", got, want)
	}
}

// ---------------------------------------------------------------------------
// Risk config loading
// ---------------------------------------------------------------------------

func TestLoadRiskConfig_ValidFile(t *testing.T) {
	t.Parallel()

	// Find the actual config/risk.yaml in the repo.
	path := findRepoConfig(t, "risk.yaml")

	cfg, err := LoadRiskConfig(path)
	if err != nil {
		t.Fatalf("LoadRiskConfig(%q): %v", path, err)
	}

	// Verify key values from config/risk.yaml
	if cfg.CircuitBreakers.MaxGasGwei != 300 {
		t.Errorf("MaxGasGwei = %v, want 300", cfg.CircuitBreakers.MaxGasGwei)
	}
	if cfg.CircuitBreakers.ConsecutiveRevertsPause != 10 {
		t.Errorf("ConsecutiveRevertsPause = %d, want 10", cfg.CircuitBreakers.ConsecutiveRevertsPause)
	}
	if cfg.CircuitBreakers.CompetitiveRevertAlertPct != 90 {
		t.Errorf("CompetitiveRevertAlertPct = %v, want 90", cfg.CircuitBreakers.CompetitiveRevertAlertPct)
	}
	if cfg.CircuitBreakers.RevertWindowMinutes != 10 {
		t.Errorf("RevertWindowMinutes = %d, want 10", cfg.CircuitBreakers.RevertWindowMinutes)
	}
	if cfg.CircuitBreakers.DailyLossHaltETH != 0.5 {
		t.Errorf("DailyLossHaltETH = %v, want 0.5", cfg.CircuitBreakers.DailyLossHaltETH)
	}
	if cfg.CircuitBreakers.MinETHBalance != 0.1 {
		t.Errorf("MinETHBalance = %v, want 0.1", cfg.CircuitBreakers.MinETHBalance)
	}
	if cfg.CircuitBreakers.MaxNodeLatencyMs != 500 {
		t.Errorf("MaxNodeLatencyMs = %d, want 500", cfg.CircuitBreakers.MaxNodeLatencyMs)
	}
	if cfg.CircuitBreakers.BundleMissRateAlertPct != 80 {
		t.Errorf("BundleMissRateAlertPct = %v, want 80", cfg.CircuitBreakers.BundleMissRateAlertPct)
	}
	if cfg.CircuitBreakers.BundleMissRateWindowMinutes != 60 {
		t.Errorf("BundleMissRateWindowMinutes = %d, want 60", cfg.CircuitBreakers.BundleMissRateWindowMinutes)
	}
	if cfg.PositionLimits.MaxSingleTradeETH != 50.0 {
		t.Errorf("MaxSingleTradeETH = %v, want 50.0", cfg.PositionLimits.MaxSingleTradeETH)
	}
	if cfg.PositionLimits.MaxDailyVolumeETH != 500.0 {
		t.Errorf("MaxDailyVolumeETH = %v, want 500.0", cfg.PositionLimits.MaxDailyVolumeETH)
	}
	if cfg.PositionLimits.MinProfitETH != 0.001 {
		t.Errorf("MinProfitETH = %v, want 0.001", cfg.PositionLimits.MinProfitETH)
	}
	if cfg.PositionLimits.MaxTipSharePct != 95 {
		t.Errorf("MaxTipSharePct = %v, want 95", cfg.PositionLimits.MaxTipSharePct)
	}
	if cfg.System.InitialState != "running" {
		t.Errorf("InitialState = %q, want %q", cfg.System.InitialState, "running")
	}
	if !cfg.System.ManualResetRequiredFromHalted {
		t.Error("ManualResetRequiredFromHalted = false, want true")
	}
}

func TestLoadRiskConfig_FileNotFound(t *testing.T) {
	t.Parallel()

	_, err := LoadRiskConfig("/nonexistent/path/risk.yaml")
	if err == nil {
		t.Fatal("expected error for non-existent file, got nil")
	}
}

func TestLoadRiskConfig_InvalidYAML(t *testing.T) {
	t.Parallel()

	path := writeTempFile(t, "bad-risk.yaml", []byte("{{{{invalid yaml"))

	_, err := LoadRiskConfig(path)
	if err == nil {
		t.Fatal("expected error for invalid YAML, got nil")
	}
}

func TestValidateRiskConfig_ZeroMaxGasGwei(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.CircuitBreakers.MaxGasGwei = 0

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for MaxGasGwei=0, got nil")
	}
}

func TestValidateRiskConfig_NegativeMaxGasGwei(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.CircuitBreakers.MaxGasGwei = -10

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for negative MaxGasGwei, got nil")
	}
}

func TestValidateRiskConfig_ZeroMinProfitETH(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.PositionLimits.MinProfitETH = 0

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for MinProfitETH=0, got nil")
	}
}

func TestValidateRiskConfig_NegativeMinProfitETH(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.PositionLimits.MinProfitETH = -0.001

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for negative MinProfitETH, got nil")
	}
}

func TestValidateRiskConfig_MaxTipShareOver100(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.PositionLimits.MaxTipSharePct = 101

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for MaxTipSharePct > 100, got nil")
	}
}

func TestValidateRiskConfig_ZeroMaxTipShare(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.PositionLimits.MaxTipSharePct = 0

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for MaxTipSharePct=0, got nil")
	}
}

func TestValidateRiskConfig_ZeroConsecutiveReverts(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.CircuitBreakers.ConsecutiveRevertsPause = 0

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for ConsecutiveRevertsPause=0, got nil")
	}
}

func TestValidateRiskConfig_Valid(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	err := ValidateRiskConfig(cfg)
	if err != nil {
		t.Fatalf("expected no error for valid config, got: %v", err)
	}
}

func TestValidateRiskConfig_ZeroCompetitiveRevertAlertPct(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.CircuitBreakers.CompetitiveRevertAlertPct = 0

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for CompetitiveRevertAlertPct=0, got nil")
	}
}

func TestValidateRiskConfig_CompetitiveRevertAlertPctOver100(t *testing.T) {
	t.Parallel()

	cfg := validRiskFileConfig()
	cfg.CircuitBreakers.CompetitiveRevertAlertPct = 101

	err := ValidateRiskConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for CompetitiveRevertAlertPct=101, got nil")
	}
}

// ---------------------------------------------------------------------------
// Builders config loading
// ---------------------------------------------------------------------------

func TestLoadBuildersConfig_ValidFile(t *testing.T) {
	t.Parallel()

	path := findRepoConfig(t, "builders.yaml")

	cfg, err := LoadBuildersConfig(path)
	if err != nil {
		t.Fatalf("LoadBuildersConfig(%q): %v", path, err)
	}

	if len(cfg.Builders) != 2 {
		t.Fatalf("expected 2 builders, got %d", len(cfg.Builders))
	}
	if cfg.Builders[0].Name != "flashbots" {
		t.Errorf("builders[0].name = %q, want %q", cfg.Builders[0].Name, "flashbots")
	}
	if cfg.Builders[0].URL != "https://relay.flashbots.net" {
		t.Errorf("builders[0].url = %q, want %q", cfg.Builders[0].URL, "https://relay.flashbots.net")
	}
	if !cfg.Builders[0].Enabled {
		t.Error("builders[0].enabled = false, want true")
	}
	if cfg.Builders[0].TimeoutMs != 2000 {
		t.Errorf("builders[0].timeout_ms = %d, want 2000", cfg.Builders[0].TimeoutMs)
	}
	if cfg.Builders[1].Name != "titan" {
		t.Errorf("builders[1].name = %q, want %q", cfg.Builders[1].Name, "titan")
	}
	if !cfg.Submission.FanOut {
		t.Error("submission.fan_out = false, want true")
	}
	if cfg.Submission.MaxRetries != 2 {
		t.Errorf("submission.max_retries = %d, want 2", cfg.Submission.MaxRetries)
	}
}

func TestLoadBuildersConfig_FileNotFound(t *testing.T) {
	t.Parallel()

	_, err := LoadBuildersConfig("/nonexistent/builders.yaml")
	if err == nil {
		t.Fatal("expected error for non-existent file, got nil")
	}
}

func TestLoadBuildersConfig_InvalidYAML(t *testing.T) {
	t.Parallel()

	path := writeTempFile(t, "bad-builders.yaml", []byte("{{{{invalid yaml"))

	_, err := LoadBuildersConfig(path)
	if err == nil {
		t.Fatal("expected error for invalid YAML, got nil")
	}
}

func TestValidateBuildersConfig_EmptyBuilders(t *testing.T) {
	t.Parallel()

	cfg := BuildersFileConfig{}
	err := ValidateBuildersConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for empty builders list, got nil")
	}
}

func TestValidateBuildersConfig_EmptyName(t *testing.T) {
	t.Parallel()

	cfg := BuildersFileConfig{
		Builders: []BuilderEntry{{Name: "", URL: "http://test", TimeoutMs: 1000}},
	}
	err := ValidateBuildersConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for empty builder name, got nil")
	}
}

func TestValidateBuildersConfig_EmptyURL(t *testing.T) {
	t.Parallel()

	cfg := BuildersFileConfig{
		Builders: []BuilderEntry{{Name: "test", URL: "", TimeoutMs: 1000}},
	}
	err := ValidateBuildersConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for empty builder URL, got nil")
	}
}

func TestValidateBuildersConfig_ZeroTimeout(t *testing.T) {
	t.Parallel()

	cfg := BuildersFileConfig{
		Builders: []BuilderEntry{{Name: "test", URL: "http://test", TimeoutMs: 0}},
	}
	err := ValidateBuildersConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for zero timeout, got nil")
	}
}

// ---------------------------------------------------------------------------
// Nodes config loading
// ---------------------------------------------------------------------------

func TestLoadNodesConfig_ValidFile(t *testing.T) {
	t.Parallel()

	path := findRepoConfig(t, "nodes.yaml")

	cfg, err := LoadNodesConfig(path)
	if err != nil {
		t.Fatalf("LoadNodesConfig(%q): %v", path, err)
	}

	if len(cfg.Nodes) != 2 {
		t.Fatalf("expected 2 nodes, got %d", len(cfg.Nodes))
	}
	if cfg.Nodes[0].Name != "alchemy-ws" {
		t.Errorf("nodes[0].name = %q, want %q", cfg.Nodes[0].Name, "alchemy-ws")
	}
	if cfg.Nodes[0].Type != "websocket" {
		t.Errorf("nodes[0].type = %q, want %q", cfg.Nodes[0].Type, "websocket")
	}
	if cfg.Nodes[0].Priority != 1 {
		t.Errorf("nodes[0].priority = %d, want 1", cfg.Nodes[0].Priority)
	}
	if cfg.Nodes[1].Name != "local-reth" {
		t.Errorf("nodes[1].name = %q, want %q", cfg.Nodes[1].Name, "local-reth")
	}
	if cfg.Nodes[1].Type != "ipc" {
		t.Errorf("nodes[1].type = %q, want %q", cfg.Nodes[1].Type, "ipc")
	}
	if cfg.MinHealthyNodes != 2 {
		t.Errorf("min_healthy_nodes = %d, want 2", cfg.MinHealthyNodes)
	}
}

func TestLoadNodesConfig_FileNotFound(t *testing.T) {
	t.Parallel()

	_, err := LoadNodesConfig("/nonexistent/nodes.yaml")
	if err == nil {
		t.Fatal("expected error for non-existent file, got nil")
	}
}

func TestLoadNodesConfig_InvalidYAML(t *testing.T) {
	t.Parallel()

	path := writeTempFile(t, "bad-nodes.yaml", []byte("{{{{invalid yaml"))

	_, err := LoadNodesConfig(path)
	if err == nil {
		t.Fatal("expected error for invalid YAML, got nil")
	}
}

func TestValidateNodesConfig_EmptyNodes(t *testing.T) {
	t.Parallel()

	cfg := NodesFileConfig{MinHealthyNodes: 1}
	err := ValidateNodesConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for empty nodes list, got nil")
	}
}

func TestValidateNodesConfig_InvalidType(t *testing.T) {
	t.Parallel()

	cfg := NodesFileConfig{
		Nodes:           []NodeEntry{{Name: "test", URL: "ws://test", Type: "ftp"}},
		MinHealthyNodes: 1,
	}
	err := ValidateNodesConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for invalid node type 'ftp', got nil")
	}
}

func TestValidateNodesConfig_ZeroMinHealthy(t *testing.T) {
	t.Parallel()

	cfg := NodesFileConfig{
		Nodes:           []NodeEntry{{Name: "test", URL: "ws://test", Type: "websocket"}},
		MinHealthyNodes: 0,
	}
	err := ValidateNodesConfig(cfg)
	if err == nil {
		t.Fatal("expected validation error for min_healthy_nodes=0, got nil")
	}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// findRepoConfig locates a config file by walking up from the test's working
// directory until it finds a "config" directory containing the given file.
func findRepoConfig(t *testing.T, filename string) string {
	t.Helper()

	// Start from the current working directory
	dir, err := os.Getwd()
	if err != nil {
		t.Fatalf("Getwd: %v", err)
	}

	// Walk up until we find config/<filename>
	for {
		candidate := filepath.Join(dir, "config", filename)
		if _, err := os.Stat(candidate); err == nil {
			return candidate
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatalf("could not find config/%s in any parent directory", filename)
		}
		dir = parent
	}
}

// writeTempFile creates a temporary file with the given contents and returns
// its path. The file is cleaned up when the test finishes.
func writeTempFile(t *testing.T, name string, data []byte) string {
	t.Helper()
	dir := t.TempDir()
	path := filepath.Join(dir, name)
	if err := os.WriteFile(path, data, 0644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	return path
}

// validRiskFileConfig returns a RiskFileConfig that passes validation.
func validRiskFileConfig() RiskFileConfig {
	var cfg RiskFileConfig
	cfg.CircuitBreakers.MaxGasGwei = 300
	cfg.CircuitBreakers.ConsecutiveRevertsPause = 10
	cfg.CircuitBreakers.RevertWindowMinutes = 10
	cfg.CircuitBreakers.CompetitiveRevertAlertPct = 90
	cfg.CircuitBreakers.DailyLossHaltETH = 0.5
	cfg.CircuitBreakers.MinETHBalance = 0.1
	cfg.CircuitBreakers.MaxNodeLatencyMs = 500
	cfg.CircuitBreakers.BundleMissRateAlertPct = 80
	cfg.CircuitBreakers.BundleMissRateWindowMinutes = 60
	cfg.PositionLimits.MaxSingleTradeETH = 50.0
	cfg.PositionLimits.MaxDailyVolumeETH = 500.0
	cfg.PositionLimits.MinProfitETH = 0.001
	cfg.PositionLimits.MaxTipSharePct = 95
	return cfg
}
