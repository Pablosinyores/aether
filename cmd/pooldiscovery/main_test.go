package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// ---------------------------------------------------------------------------
// PoolDiscoverer
// ---------------------------------------------------------------------------

func TestNewPoolDiscoverer(t *testing.T) {
	t.Parallel()

	pd := NewPoolDiscoverer("http://localhost:8545", 50)
	if pd.rpcURL != "http://localhost:8545" {
		t.Errorf("rpcURL = %q, want %q", pd.rpcURL, "http://localhost:8545")
	}
	if pd.limit != 50 {
		t.Errorf("limit = %d, want 50", pd.limit)
	}
	if len(pd.factories) != 2 {
		t.Errorf("factories count = %d, want 2", len(pd.factories))
	}
}

func TestDiscoverPools(t *testing.T) {
	t.Parallel()

	pd := NewPoolDiscoverer("simulated", 100)
	pools, err := pd.DiscoverPools()
	if err != nil {
		t.Fatalf("DiscoverPools() error: %v", err)
	}
	if len(pools) == 0 {
		t.Fatal("DiscoverPools() returned 0 pools, expected > 0")
	}

	// All returned pools should contain WETH or USDC
	for _, p := range pools {
		hasTarget := containsTargetToken(p.Token0) || containsTargetToken(p.Token1)
		if !hasTarget {
			t.Errorf("Pool %s has tokens %s/%s, neither is WETH or USDC",
				p.Address, p.Token0, p.Token1)
		}
	}
}

func TestDiscoverPools_Limit(t *testing.T) {
	t.Parallel()

	pd := NewPoolDiscoverer("simulated", 3)
	pools, err := pd.DiscoverPools()
	if err != nil {
		t.Fatalf("DiscoverPools() error: %v", err)
	}
	if len(pools) > 3 {
		t.Errorf("DiscoverPools() returned %d pools, expected <= 3", len(pools))
	}
}

func TestDiscoverPools_HasAllProtocols(t *testing.T) {
	t.Parallel()

	pd := NewPoolDiscoverer("simulated", 100)
	pools, err := pd.DiscoverPools()
	if err != nil {
		t.Fatalf("DiscoverPools() error: %v", err)
	}

	protocols := make(map[string]bool)
	for _, p := range pools {
		protocols[p.Protocol] = true
	}

	expected := []string{"uniswap_v2", "uniswap_v3", "sushiswap"}
	for _, proto := range expected {
		if !protocols[proto] {
			t.Errorf("Missing protocol %q in discovered pools", proto)
		}
	}
}

// ---------------------------------------------------------------------------
// FilterPools
// ---------------------------------------------------------------------------

func TestFilterPools_KeepsWETHAndUSDC(t *testing.T) {
	t.Parallel()

	pools := []PoolEntry{
		{Protocol: "uniswap_v2", Token0: USDC, Token1: WETH, Address: "0x1"},
		{Protocol: "uniswap_v2", Token0: USDT, Token1: DAI, Address: "0x2"},
		{Protocol: "uniswap_v2", Token0: WBTC, Token1: WETH, Address: "0x3"},
	}

	filtered := FilterPools(pools)
	if len(filtered) != 2 {
		t.Fatalf("FilterPools() returned %d pools, want 2", len(filtered))
	}
	if filtered[0].Address != "0x1" {
		t.Errorf("filtered[0].Address = %q, want %q", filtered[0].Address, "0x1")
	}
	if filtered[1].Address != "0x3" {
		t.Errorf("filtered[1].Address = %q, want %q", filtered[1].Address, "0x3")
	}
}

func TestFilterPools_Empty(t *testing.T) {
	t.Parallel()

	filtered := FilterPools(nil)
	if len(filtered) != 0 {
		t.Errorf("FilterPools(nil) returned %d pools, want 0", len(filtered))
	}
}

func TestFilterPools_AllFiltered(t *testing.T) {
	t.Parallel()

	pools := []PoolEntry{
		{Protocol: "uniswap_v2", Token0: USDT, Token1: DAI, Address: "0x1"},
	}

	filtered := FilterPools(pools)
	if len(filtered) != 0 {
		t.Errorf("FilterPools() returned %d pools, want 0 (no WETH/USDC)", len(filtered))
	}
}

// ---------------------------------------------------------------------------
// containsTargetToken
// ---------------------------------------------------------------------------

func TestContainsTargetToken(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name  string
		addr  string
		want  bool
	}{
		{"WETH", WETH, true},
		{"USDC", USDC, true},
		{"USDT", USDT, false},
		{"DAI", DAI, false},
		{"WBTC", WBTC, false},
		{"empty", "", false},
		{"WETH lowercase", strings.ToLower(WETH), true},
		{"USDC lowercase", strings.ToLower(USDC), true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			got := containsTargetToken(tt.addr)
			if got != tt.want {
				t.Errorf("containsTargetToken(%q) = %v, want %v", tt.addr, got, tt.want)
			}
		})
	}
}

// ---------------------------------------------------------------------------
// FormatTOML
// ---------------------------------------------------------------------------

func TestFormatTOML_SingleV2Pool(t *testing.T) {
	t.Parallel()

	pools := []PoolEntry{
		{
			Protocol: "uniswap_v2",
			Address:  "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
			Token0:   USDC,
			Token1:   WETH,
			FeeBps:   30,
			Tier:     "hot",
		},
	}

	toml := FormatTOML(pools)

	// Check required fields are present
	if !strings.Contains(toml, `protocol = "uniswap_v2"`) {
		t.Error("TOML missing protocol field")
	}
	if !strings.Contains(toml, `address = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc"`) {
		t.Error("TOML missing address field")
	}
	if !strings.Contains(toml, `fee_bps = 30`) {
		t.Error("TOML missing fee_bps field")
	}
	if !strings.Contains(toml, `tier = "hot"`) {
		t.Error("TOML missing tier field")
	}
	if !strings.Contains(toml, "[[pools]]") {
		t.Error("TOML missing [[pools]] array header")
	}
	// V2 pools should NOT have tick_spacing
	if strings.Contains(toml, "tick_spacing") {
		t.Error("TOML should not contain tick_spacing for V2 pool")
	}
}

func TestFormatTOML_V3Pool_HasTickSpacing(t *testing.T) {
	t.Parallel()

	pools := []PoolEntry{
		{
			Protocol:    "uniswap_v3",
			Address:     "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640",
			Token0:      USDC,
			Token1:      WETH,
			FeeBps:      5,
			Tier:        "hot",
			TickSpacing: 10,
		},
	}

	toml := FormatTOML(pools)

	if !strings.Contains(toml, "tick_spacing = 10") {
		t.Error("TOML missing tick_spacing for V3 pool")
	}
}

func TestFormatTOML_MultiplePools(t *testing.T) {
	t.Parallel()

	pools := []PoolEntry{
		{Protocol: "uniswap_v2", Address: "0x1", Token0: USDC, Token1: WETH, FeeBps: 30, Tier: "hot"},
		{Protocol: "sushiswap", Address: "0x2", Token0: USDC, Token1: WETH, FeeBps: 30, Tier: "hot"},
	}

	toml := FormatTOML(pools)

	count := strings.Count(toml, "[[pools]]")
	if count != 2 {
		t.Errorf("TOML has %d [[pools]] sections, want 2", count)
	}
	if !strings.Contains(toml, "Total pools: 2") {
		t.Error("TOML header missing pool count")
	}
}

func TestFormatTOML_Empty(t *testing.T) {
	t.Parallel()

	toml := FormatTOML(nil)

	if !strings.Contains(toml, "Total pools: 0") {
		t.Error("TOML header should show 0 pools")
	}
	if strings.Contains(toml, "[[pools]]") {
		t.Error("TOML should not contain [[pools]] when empty")
	}
}

// ---------------------------------------------------------------------------
// wellKnownV2Pools / wellKnownV3Pools
// ---------------------------------------------------------------------------

func TestWellKnownV2Pools_UniswapV2(t *testing.T) {
	t.Parallel()

	pools := wellKnownV2Pools("uniswap_v2", 30)
	if len(pools) == 0 {
		t.Fatal("wellKnownV2Pools returned 0 pools for uniswap_v2")
	}
	for _, p := range pools {
		if p.Protocol != "uniswap_v2" {
			t.Errorf("pool protocol = %q, want %q", p.Protocol, "uniswap_v2")
		}
		if p.FeeBps != 30 {
			t.Errorf("pool fee_bps = %d, want 30", p.FeeBps)
		}
		if p.Address == "" {
			t.Error("pool address is empty")
		}
	}
}

func TestWellKnownV2Pools_SushiSwap(t *testing.T) {
	t.Parallel()

	pools := wellKnownV2Pools("sushiswap", 30)
	if len(pools) == 0 {
		t.Fatal("wellKnownV2Pools returned 0 pools for sushiswap")
	}
	for _, p := range pools {
		if p.Protocol != "sushiswap" {
			t.Errorf("pool protocol = %q, want %q", p.Protocol, "sushiswap")
		}
	}
}

func TestWellKnownV2Pools_UnknownProtocol(t *testing.T) {
	t.Parallel()

	pools := wellKnownV2Pools("unknown_dex", 30)
	if len(pools) != 0 {
		t.Errorf("wellKnownV2Pools returned %d pools for unknown protocol, want 0", len(pools))
	}
}

func TestWellKnownV3Pools(t *testing.T) {
	t.Parallel()

	pools := wellKnownV3Pools()
	if len(pools) == 0 {
		t.Fatal("wellKnownV3Pools returned 0 pools")
	}
	for _, p := range pools {
		if p.Protocol != "uniswap_v3" {
			t.Errorf("pool protocol = %q, want %q", p.Protocol, "uniswap_v3")
		}
		if p.TickSpacing <= 0 {
			t.Errorf("pool tick_spacing = %d, want > 0", p.TickSpacing)
		}
	}
}

// ---------------------------------------------------------------------------
// Integration: Write to file
// ---------------------------------------------------------------------------

func TestWriteTOMLToFile(t *testing.T) {
	t.Parallel()

	pd := NewPoolDiscoverer("simulated", 5)
	pools, err := pd.DiscoverPools()
	if err != nil {
		t.Fatalf("DiscoverPools() error: %v", err)
	}

	toml := FormatTOML(pools)

	// Write to temp file
	dir := t.TempDir()
	outPath := filepath.Join(dir, "pools.toml")
	if err := os.WriteFile(outPath, []byte(toml), 0644); err != nil {
		t.Fatalf("WriteFile error: %v", err)
	}

	// Read back and verify
	data, err := os.ReadFile(outPath)
	if err != nil {
		t.Fatalf("ReadFile error: %v", err)
	}

	content := string(data)
	if !strings.Contains(content, "[[pools]]") {
		t.Error("Written file missing [[pools]] header")
	}
	if !strings.Contains(content, "protocol") {
		t.Error("Written file missing protocol field")
	}
}
