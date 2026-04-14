// Package config provides YAML/TOML configuration loading for Aether Go services.
// It reads config files from a configurable directory (AETHER_CONFIG_DIR env var,
// defaulting to ./config) and validates their contents before returning typed structs.
package config

import (
	"fmt"
	"os"
	"path/filepath"

	"gopkg.in/yaml.v3"
)

// ConfigDir returns the config directory path.
// Uses AETHER_CONFIG_DIR env var, falls back to "./config".
func ConfigDir() string {
	if dir := os.Getenv("AETHER_CONFIG_DIR"); dir != "" {
		return dir
	}
	return "./config"
}

// ConfigPath returns the full path for a config file.
func ConfigPath(filename string) string {
	return filepath.Join(ConfigDir(), filename)
}

// ---------------------------------------------------------------------------
// Risk config (config/risk.yaml)
// ---------------------------------------------------------------------------

// RiskFileConfig maps the risk.yaml file structure.
type RiskFileConfig struct {
	CircuitBreakers struct {
		MaxGasGwei                  float64 `yaml:"max_gas_gwei"`
		ConsecutiveRevertsPause     int     `yaml:"consecutive_reverts_pause"`
		RevertWindowMinutes         int     `yaml:"revert_window_minutes"`
		DailyLossHaltETH            float64 `yaml:"daily_loss_halt_eth"`
		MinETHBalance               float64 `yaml:"min_eth_balance"`
		MaxNodeLatencyMs            int64   `yaml:"max_node_latency_ms"`
		BundleMissRateAlertPct      float64 `yaml:"bundle_miss_rate_alert_pct"`
		BundleMissRateWindowMinutes int     `yaml:"bundle_miss_rate_window_minutes"`
		CompetitiveRevertAlertPct   float64 `yaml:"competitive_revert_alert_pct"`
	} `yaml:"circuit_breakers"`
	PositionLimits struct {
		MaxSingleTradeETH float64 `yaml:"max_single_trade_eth"`
		MaxDailyVolumeETH float64 `yaml:"max_daily_volume_eth"`
		MinProfitETH      float64 `yaml:"min_profit_eth"`
		MinTipSharePct    float64 `yaml:"min_tip_share_pct"`
		MaxTipSharePct    float64 `yaml:"max_tip_share_pct"`
	} `yaml:"position_limits"`
	System struct {
		InitialState                  string `yaml:"initial_state"`
		ManualResetRequiredFromHalted bool   `yaml:"manual_reset_required_from_halted"`
	} `yaml:"system"`
}

// LoadRiskConfig reads and parses a risk YAML config file.
func LoadRiskConfig(path string) (RiskFileConfig, error) {
	var cfg RiskFileConfig

	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, fmt.Errorf("read risk config %s: %w", path, err)
	}

	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse risk config %s: %w", path, err)
	}

	if err := ValidateRiskConfig(cfg); err != nil {
		return cfg, fmt.Errorf("validate risk config: %w", err)
	}

	return cfg, nil
}

// ValidateRiskConfig ensures all required fields have valid values.
func ValidateRiskConfig(cfg RiskFileConfig) error {
	cb := cfg.CircuitBreakers
	pl := cfg.PositionLimits

	if cb.MaxGasGwei <= 0 {
		return fmt.Errorf("circuit_breakers.max_gas_gwei must be > 0, got %v", cb.MaxGasGwei)
	}
	if cb.ConsecutiveRevertsPause <= 0 {
		return fmt.Errorf("circuit_breakers.consecutive_reverts_pause must be > 0, got %d", cb.ConsecutiveRevertsPause)
	}
	if cb.RevertWindowMinutes <= 0 {
		return fmt.Errorf("circuit_breakers.revert_window_minutes must be > 0, got %d", cb.RevertWindowMinutes)
	}
	if cb.DailyLossHaltETH <= 0 {
		return fmt.Errorf("circuit_breakers.daily_loss_halt_eth must be > 0, got %v", cb.DailyLossHaltETH)
	}
	if cb.MinETHBalance <= 0 {
		return fmt.Errorf("circuit_breakers.min_eth_balance must be > 0, got %v", cb.MinETHBalance)
	}
	if cb.MaxNodeLatencyMs <= 0 {
		return fmt.Errorf("circuit_breakers.max_node_latency_ms must be > 0, got %d", cb.MaxNodeLatencyMs)
	}
	if cb.BundleMissRateAlertPct <= 0 || cb.BundleMissRateAlertPct > 100 {
		return fmt.Errorf("circuit_breakers.bundle_miss_rate_alert_pct must be in (0, 100], got %v", cb.BundleMissRateAlertPct)
	}
	if cb.BundleMissRateWindowMinutes <= 0 {
		return fmt.Errorf("circuit_breakers.bundle_miss_rate_window_minutes must be > 0, got %d", cb.BundleMissRateWindowMinutes)
	}
	if cb.CompetitiveRevertAlertPct <= 0 || cb.CompetitiveRevertAlertPct > 100 {
		return fmt.Errorf("circuit_breakers.competitive_revert_alert_pct must be in (0, 100], got %v", cb.CompetitiveRevertAlertPct)
	}

	if pl.MaxSingleTradeETH <= 0 {
		return fmt.Errorf("position_limits.max_single_trade_eth must be > 0, got %v", pl.MaxSingleTradeETH)
	}
	if pl.MaxDailyVolumeETH <= 0 {
		return fmt.Errorf("position_limits.max_daily_volume_eth must be > 0, got %v", pl.MaxDailyVolumeETH)
	}
	if pl.MinProfitETH <= 0 {
		return fmt.Errorf("position_limits.min_profit_eth must be > 0, got %v", pl.MinProfitETH)
	}
	if pl.MinTipSharePct <= 0 || pl.MinTipSharePct > 100 {
		return fmt.Errorf("position_limits.min_tip_share_pct must be in (0, 100], got %v", pl.MinTipSharePct)
	}
	if pl.MaxTipSharePct <= 0 || pl.MaxTipSharePct > 100 {
		return fmt.Errorf("position_limits.max_tip_share_pct must be in (0, 100], got %v", pl.MaxTipSharePct)
	}
	if pl.MinTipSharePct >= pl.MaxTipSharePct {
		return fmt.Errorf("position_limits.min_tip_share_pct must be < position_limits.max_tip_share_pct, got min=%v max=%v", pl.MinTipSharePct, pl.MaxTipSharePct)
	}

	return nil
}

// ---------------------------------------------------------------------------
// Builders config (config/builders.yaml)
// ---------------------------------------------------------------------------

// BuilderEntry represents a single block builder configuration.
type BuilderEntry struct {
	Name      string `yaml:"name"`
	URL       string `yaml:"url"`
	AuthType  string `yaml:"auth_type"`
	AuthKey   string `yaml:"auth_key"`
	Enabled   bool   `yaml:"enabled"`
	TimeoutMs int    `yaml:"timeout_ms"`
}

// BuildersFileConfig maps the builders.yaml file structure.
type BuildersFileConfig struct {
	Builders   []BuilderEntry `yaml:"builders"`
	Submission struct {
		FanOut     bool `yaml:"fan_out"`
		MaxRetries int  `yaml:"max_retries"`
	} `yaml:"submission"`
}

// LoadBuildersConfig reads and parses a builders YAML config file.
// Environment variables in ${VAR} format are expanded before parsing,
// allowing secrets like API keys to be injected at runtime.
func LoadBuildersConfig(path string) (BuildersFileConfig, error) {
	var cfg BuildersFileConfig

	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, fmt.Errorf("read builders config %s: %w", path, err)
	}

	data = expandEnvVars(data)

	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse builders config %s: %w", path, err)
	}

	if err := ValidateBuildersConfig(cfg); err != nil {
		return cfg, fmt.Errorf("validate builders config: %w", err)
	}

	return cfg, nil
}

// ValidateBuildersConfig ensures the builders config has valid entries.
func ValidateBuildersConfig(cfg BuildersFileConfig) error {
	if len(cfg.Builders) == 0 {
		return fmt.Errorf("builders list must not be empty")
	}
	for i, b := range cfg.Builders {
		if b.Name == "" {
			return fmt.Errorf("builders[%d].name must not be empty", i)
		}
		if b.URL == "" {
			return fmt.Errorf("builders[%d].url must not be empty", i)
		}
		if b.TimeoutMs <= 0 {
			return fmt.Errorf("builders[%d].timeout_ms must be > 0, got %d", i, b.TimeoutMs)
		}
		switch b.AuthType {
		case "flashbots", "none", "":
			// valid
		case "api_key":
			if b.AuthKey == "" {
				return fmt.Errorf("builders[%d].auth_key must not be empty when auth_type is api_key", i)
			}
		default:
			return fmt.Errorf("builders[%d].auth_type must be flashbots, api_key, or none, got %q", i, b.AuthType)
		}
	}
	return nil
}

// ---------------------------------------------------------------------------
// Nodes config (config/nodes.yaml)
// ---------------------------------------------------------------------------

// NodeEntry represents a single Ethereum node configuration.
type NodeEntry struct {
	Name     string `yaml:"name"`
	URL      string `yaml:"url"`
	Type     string `yaml:"type"`
	Priority int    `yaml:"priority"`
}

// NodesFileConfig maps the nodes.yaml file structure.
type NodesFileConfig struct {
	Nodes           []NodeEntry `yaml:"nodes"`
	MinHealthyNodes int         `yaml:"min_healthy_nodes"`
}

// ---------------------------------------------------------------------------
// Executor config (config/executor.yaml)
// ---------------------------------------------------------------------------

// ExecutorFileConfig maps the executor.yaml file structure.
type ExecutorFileConfig struct {
	ExecutorAddress string `yaml:"executor_address"`
	ExpectedChainID int64  `yaml:"expected_chain_id"`
}

// LoadExecutorConfig reads and parses an executor YAML config file.
// Environment variables in ${VAR} format are expanded before parsing,
// so AETHER_EXECUTOR_ADDRESS can be injected via the yaml itself.
func LoadExecutorConfig(path string) (ExecutorFileConfig, error) {
	var cfg ExecutorFileConfig

	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, fmt.Errorf("read executor config %s: %w", path, err)
	}

	data = expandEnvVars(data)

	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse executor config %s: %w", path, err)
	}

	if err := ValidateExecutorConfig(cfg); err != nil {
		return cfg, fmt.Errorf("validate executor config: %w", err)
	}

	return cfg, nil
}

// ValidateExecutorConfig ensures required fields are present and well-formed.
// Address syntax is validated here; on-chain bytecode check happens at runtime
// against the connected Ethereum node.
func ValidateExecutorConfig(cfg ExecutorFileConfig) error {
	if cfg.ExecutorAddress == "" {
		return fmt.Errorf("executor_address must not be empty")
	}
	addr := cfg.ExecutorAddress
	if len(addr) != 42 || addr[:2] != "0x" {
		return fmt.Errorf("executor_address must be a 0x-prefixed 20-byte hex string, got %q", addr)
	}
	zero := "0x0000000000000000000000000000000000000000"
	if addr == zero {
		return fmt.Errorf("executor_address must not be the zero address")
	}
	if cfg.ExpectedChainID <= 0 {
		return fmt.Errorf("expected_chain_id must be > 0, got %d", cfg.ExpectedChainID)
	}
	return nil
}

// expandEnvVars replaces ${VAR} patterns in the input with their
// corresponding environment variable values. Unset variables are
// replaced with an empty string.
func expandEnvVars(data []byte) []byte {
	return []byte(os.ExpandEnv(string(data)))
}

// LoadNodesConfig reads and parses a nodes YAML config file.
// Environment variables in ${VAR} format are expanded before parsing.
func LoadNodesConfig(path string) (NodesFileConfig, error) {
	var cfg NodesFileConfig

	data, err := os.ReadFile(path)
	if err != nil {
		return cfg, fmt.Errorf("read nodes config %s: %w", path, err)
	}

	data = expandEnvVars(data)

	if err := yaml.Unmarshal(data, &cfg); err != nil {
		return cfg, fmt.Errorf("parse nodes config %s: %w", path, err)
	}

	if err := ValidateNodesConfig(cfg); err != nil {
		return cfg, fmt.Errorf("validate nodes config: %w", err)
	}

	return cfg, nil
}

// ValidateNodesConfig ensures the nodes config has valid entries.
func ValidateNodesConfig(cfg NodesFileConfig) error {
	if len(cfg.Nodes) == 0 {
		return fmt.Errorf("nodes list must not be empty")
	}
	for i, n := range cfg.Nodes {
		if n.Name == "" {
			return fmt.Errorf("nodes[%d].name must not be empty", i)
		}
		if n.URL == "" {
			return fmt.Errorf("nodes[%d].url must not be empty", i)
		}
		if n.Type == "" {
			return fmt.Errorf("nodes[%d].type must not be empty", i)
		}
		validType := n.Type == "websocket" || n.Type == "ipc" || n.Type == "http"
		if !validType {
			return fmt.Errorf("nodes[%d].type must be websocket, ipc, or http, got %q", i, n.Type)
		}
	}
	if cfg.MinHealthyNodes <= 0 {
		return fmt.Errorf("min_healthy_nodes must be > 0, got %d", cfg.MinHealthyNodes)
	}
	return nil
}
