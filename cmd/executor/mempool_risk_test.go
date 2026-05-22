package main

import (
	"encoding/json"
	"math/big"
	"os"
	"path/filepath"
	"testing"
	"time"

	pb "github.com/aether-arb/aether/internal/pb"
)

// ---------------------------------------------------------------------------
// LoadMempoolRiskConfig
// ---------------------------------------------------------------------------

func TestLoadMempoolRiskConfig_Defaults(t *testing.T) {
	// Defaults must match the Stage A values cited in the runbook so a
	// silent change here doesn't desync code from documentation.
	for _, k := range []string{
		"AETHER_MEMPOOL_MIN_PROFIT_WEI",
		"AETHER_MEMPOOL_MAX_TIP_BPS",
		"AETHER_MEMPOOL_VICTIM_FRESHNESS_MS",
		"AETHER_MEMPOOL_MAX_INFLIGHT",
	} {
		os.Unsetenv(k)
	}
	cfg := LoadMempoolRiskConfig()
	if cfg.MinProfitWei.Cmp(new(big.Int).SetUint64(1_000_000_000_000_000)) != 0 {
		t.Errorf("MinProfitWei default = %s, want 1e15", cfg.MinProfitWei)
	}
	if cfg.MaxTipShareBps != 9500 {
		t.Errorf("MaxTipShareBps default = %d, want 9500", cfg.MaxTipShareBps)
	}
	if cfg.MaxVictimFreshnessMs != 500 {
		t.Errorf("MaxVictimFreshnessMs default = %d, want 500", cfg.MaxVictimFreshnessMs)
	}
	if cfg.MaxInflightPerTargetBlock != 5 {
		t.Errorf("MaxInflightPerTargetBlock default = %d, want 5", cfg.MaxInflightPerTargetBlock)
	}
}

func TestLoadMempoolRiskConfig_EnvOverride(t *testing.T) {
	t.Setenv("AETHER_MEMPOOL_MIN_PROFIT_WEI", "50000000000000000") // 5e16
	t.Setenv("AETHER_MEMPOOL_MAX_TIP_BPS", "8500")
	t.Setenv("AETHER_MEMPOOL_VICTIM_FRESHNESS_MS", "300")
	t.Setenv("AETHER_MEMPOOL_MAX_INFLIGHT", "2")

	cfg := LoadMempoolRiskConfig()
	if cfg.MinProfitWei.Cmp(new(big.Int).SetUint64(50_000_000_000_000_000)) != 0 {
		t.Errorf("MinProfitWei override = %s, want 5e16", cfg.MinProfitWei)
	}
	if cfg.MaxTipShareBps != 8500 {
		t.Errorf("MaxTipShareBps override = %d", cfg.MaxTipShareBps)
	}
	if cfg.MaxVictimFreshnessMs != 300 {
		t.Errorf("MaxVictimFreshnessMs override = %d", cfg.MaxVictimFreshnessMs)
	}
	if cfg.MaxInflightPerTargetBlock != 2 {
		t.Errorf("MaxInflightPerTargetBlock override = %d", cfg.MaxInflightPerTargetBlock)
	}
}

func TestLoadMempoolRiskConfig_BadEnvFallsThrough(t *testing.T) {
	// Garbage env values must NOT silently disable a gate. The default has
	// to win so a typo in a runbook command can't bypass a real gate.
	t.Setenv("AETHER_MEMPOOL_MIN_PROFIT_WEI", "not-a-number")
	t.Setenv("AETHER_MEMPOOL_MAX_TIP_BPS", "")
	t.Setenv("AETHER_MEMPOOL_VICTIM_FRESHNESS_MS", "0") // 0 means "use default"
	t.Setenv("AETHER_MEMPOOL_MAX_INFLIGHT", "abc")

	cfg := LoadMempoolRiskConfig()
	if cfg.MinProfitWei.Uint64() != 1_000_000_000_000_000 {
		t.Errorf("MinProfitWei: garbage didn't fall through, got %s", cfg.MinProfitWei)
	}
	if cfg.MaxTipShareBps != 9500 {
		t.Errorf("MaxTipShareBps: empty didn't fall through, got %d", cfg.MaxTipShareBps)
	}
	if cfg.MaxVictimFreshnessMs != 500 {
		t.Errorf("MaxVictimFreshnessMs: zero didn't fall through, got %d", cfg.MaxVictimFreshnessMs)
	}
	if cfg.MaxInflightPerTargetBlock != 5 {
		t.Errorf("MaxInflightPerTargetBlock: garbage didn't fall through, got %d", cfg.MaxInflightPerTargetBlock)
	}
}

// ---------------------------------------------------------------------------
// MempoolRiskGate — every gate path
// ---------------------------------------------------------------------------

func testCfg() MempoolRiskConfig {
	return MempoolRiskConfig{
		MinProfitWei:              new(big.Int).SetUint64(1_000_000_000_000_000), // 1e15
		MaxTipShareBps:            9500,
		MaxVictimFreshnessMs:      500,
		MaxInflightPerTargetBlock: 3,
	}
}

func TestMempoolRiskGate_Approves_HappyPath(t *testing.T) {
	cfg := testCfg()
	inflight := NewMempoolInflightTracker()
	now := time.Unix(1_700_000_000, 0)

	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei:  new(big.Int).SetUint64(2_000_000_000_000_000), // 2e15
		TipShareBps:     9000,
		VictimSeenAt:    now.Add(-100 * time.Millisecond),
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xdeadbeef01",
	}, inflight, now)

	if !res.Approved {
		t.Fatalf("happy path rejected: reason=%s", res.Reason)
	}
	if res.Reason != "" {
		t.Errorf("approved result still carries reason: %q", res.Reason)
	}
	// Four gates evaluated, all passed.
	if len(res.Gates) != 4 {
		t.Errorf("gate trace len=%d, want 4", len(res.Gates))
	}
	for _, g := range res.Gates {
		if !g.Passed {
			t.Errorf("gate %s should have passed", g.Gate)
		}
	}
}

func TestMempoolRiskGate_Rejects_MinProfit(t *testing.T) {
	cfg := testCfg()
	inflight := NewMempoolInflightTracker()
	now := time.Unix(1_700_000_000, 0)

	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei:  new(big.Int).SetUint64(500_000_000_000_000), // 5e14 — below 1e15 floor
		TipShareBps:     9000,
		VictimSeenAt:    now.Add(-100 * time.Millisecond),
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xdeadbeef02",
	}, inflight, now)

	if res.Approved {
		t.Fatal("expected min_profit reject")
	}
	if res.Reason != "min_profit" {
		t.Errorf("reason = %q, want min_profit", res.Reason)
	}
}

func TestMempoolRiskGate_Rejects_NilProfit(t *testing.T) {
	// A nil GrossProfitWei must reject as min_profit rather than panic.
	// Caller-side bugs (missing fields) should fail closed.
	cfg := testCfg()
	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei: nil,
	}, NewMempoolInflightTracker(), time.Unix(1, 0))
	if res.Approved {
		t.Fatal("nil profit should reject")
	}
	if res.Reason != "min_profit" {
		t.Errorf("nil reject reason = %q, want min_profit", res.Reason)
	}
}

func TestMempoolRiskGate_Rejects_MaxTipShare(t *testing.T) {
	cfg := testCfg()
	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei: new(big.Int).SetUint64(2_000_000_000_000_000),
		TipShareBps:    9600, // > 9500
	}, NewMempoolInflightTracker(), time.Unix(1, 0))
	if res.Approved || res.Reason != "max_tip_share" {
		t.Errorf("got approved=%v reason=%q, want reject max_tip_share", res.Approved, res.Reason)
	}
}

func TestMempoolRiskGate_Rejects_VictimStale(t *testing.T) {
	cfg := testCfg()
	now := time.Unix(1_700_000_000, 0)
	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei:  new(big.Int).SetUint64(2_000_000_000_000_000),
		TipShareBps:     9000,
		VictimSeenAt:    now.Add(-600 * time.Millisecond), // > 500ms
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xdead",
	}, NewMempoolInflightTracker(), now)
	if res.Approved || res.Reason != "victim_stale" {
		t.Errorf("got approved=%v reason=%q, want reject victim_stale", res.Approved, res.Reason)
	}
}

func TestMempoolRiskGate_Rejects_Duplicate(t *testing.T) {
	cfg := testCfg()
	inflight := NewMempoolInflightTracker()
	now := time.Unix(1_700_000_000, 0)
	args := MempoolPreflightArgs{
		GrossProfitWei:  new(big.Int).SetUint64(2_000_000_000_000_000),
		TipShareBps:     9000,
		VictimSeenAt:    now,
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xduplicate01",
	}
	// First call records the pair as in-flight.
	if r := MempoolRiskGate(cfg, args, inflight, now); !r.Approved {
		t.Fatalf("first call should approve: %s", r.Reason)
	}
	// Second call with same (target_block, victim) must reject as duplicate.
	r2 := MempoolRiskGate(cfg, args, inflight, now)
	if r2.Approved || r2.Reason != "duplicate" {
		t.Errorf("got approved=%v reason=%q, want reject duplicate", r2.Approved, r2.Reason)
	}
}

func TestMempoolRiskGate_Rejects_MaxInflightPerBlock(t *testing.T) {
	cfg := testCfg() // MaxInflightPerTargetBlock = 3
	inflight := NewMempoolInflightTracker()
	now := time.Unix(1_700_000_000, 0)

	// Three distinct victims, same target_block — all approve.
	for i := 0; i < 3; i++ {
		args := MempoolPreflightArgs{
			GrossProfitWei:  new(big.Int).SetUint64(2_000_000_000_000_000),
			TipShareBps:     9000,
			VictimSeenAt:    now,
			TargetBlock:     18_000_000,
			VictimTxHashHex: "0xvic" + string(rune('0'+i)),
		}
		if r := MempoolRiskGate(cfg, args, inflight, now); !r.Approved {
			t.Fatalf("victim %d should approve: %s", i, r.Reason)
		}
	}
	// 4th distinct victim must reject (count >= cap).
	args := MempoolPreflightArgs{
		GrossProfitWei:  new(big.Int).SetUint64(2_000_000_000_000_000),
		TipShareBps:     9000,
		VictimSeenAt:    now,
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xvic4",
	}
	r := MempoolRiskGate(cfg, args, inflight, now)
	if r.Approved || r.Reason != "max_inflight_per_block" {
		t.Errorf("got approved=%v reason=%q, want reject max_inflight_per_block", r.Approved, r.Reason)
	}
}

func TestMempoolRiskGate_OrderIsStable(t *testing.T) {
	// If a candidate would fail multiple gates, the cheapest one (min_profit)
	// must report the rejection. Reordering gates would break dashboards
	// that bucket by reason.
	cfg := testCfg()
	now := time.Unix(1_700_000_000, 0)
	res := MempoolRiskGate(cfg, MempoolPreflightArgs{
		GrossProfitWei:  big.NewInt(1), // below floor
		TipShareBps:     9999,           // also above cap
		VictimSeenAt:    now.Add(-time.Hour),
		TargetBlock:     18_000_000,
		VictimTxHashHex: "0xa",
	}, NewMempoolInflightTracker(), now)
	if res.Reason != "min_profit" {
		t.Errorf("reason = %q, want min_profit (first failing gate)", res.Reason)
	}
}

// ---------------------------------------------------------------------------
// MempoolInflightTracker
// ---------------------------------------------------------------------------

func TestMempoolInflightTracker_RecordSeenCount(t *testing.T) {
	tr := NewMempoolInflightTracker()
	now := time.Unix(1_700_000_000, 0)

	if tr.Seen(100, "0xa") {
		t.Error("empty tracker reported Seen=true")
	}
	if tr.CountForBlock(100) != 0 {
		t.Error("empty tracker reported nonzero count")
	}

	tr.Record(100, "0xa", now)
	if !tr.Seen(100, "0xa") {
		t.Error("after Record, Seen=false")
	}
	if tr.CountForBlock(100) != 1 {
		t.Errorf("count = %d, want 1", tr.CountForBlock(100))
	}

	// Same victim, same block — Seen() is idempotent semantically (set semantics).
	tr.Record(100, "0xa", now)
	if tr.CountForBlock(100) != 1 {
		t.Errorf("count after duplicate record = %d, want 1", tr.CountForBlock(100))
	}

	tr.Record(100, "0xb", now)
	if tr.CountForBlock(100) != 2 {
		t.Errorf("count after second victim = %d, want 2", tr.CountForBlock(100))
	}

	// Different block.
	if tr.CountForBlock(101) != 0 {
		t.Error("block 101 leak from block 100")
	}
}

func TestMempoolInflightTracker_ReapsOldEntries(t *testing.T) {
	tr := NewMempoolInflightTracker()
	old := time.Unix(1_700_000_000, 0)

	tr.Record(100, "0xa", old)
	if tr.CountForBlock(100) != 1 {
		t.Fatal("setup: expected count=1")
	}

	// Force a reap by recording at old + 145s (cutoff is 144s).
	tr.Record(200, "0xb", old.Add(145*time.Second))

	if tr.CountForBlock(100) != 0 {
		t.Errorf("old block 100 still tracked (count=%d), want reaped",
			tr.CountForBlock(100))
	}
	if tr.CountForBlock(200) != 1 {
		t.Errorf("block 200 count = %d, want 1", tr.CountForBlock(200))
	}
}

// ---------------------------------------------------------------------------
// dumpMempoolShadowBundle
// ---------------------------------------------------------------------------

func TestDumpMempoolShadowBundle_Schema(t *testing.T) {
	// Override session-dir resolver so this test gets a fresh tmpdir
	// regardless of which other tests ran first.
	dir := t.TempDir()
	old := mempoolShadowSessionDir
	mempoolShadowSessionDir = func() string { return dir }
	defer func() { mempoolShadowSessionDir = old }()

	weth := []byte{0xc0, 0x2a, 0xaa, 0x39, 0xb2, 0x23, 0xfe, 0x8d, 0x0a, 0x0e,
		0x5c, 0x4f, 0x27, 0xea, 0xd9, 0x08, 0x3c, 0x75, 0x6c, 0xc2}

	arb := &pb.ValidatedArb{
		Id:              "mempool-arb-1",
		FlashloanToken:  weth,
		FlashloanAmount: new(big.Int).SetUint64(1_000_000_000_000_000_000).Bytes(),
		NetProfitWei:    new(big.Int).SetUint64(500_000_000_000_000_000).Bytes(),
		TotalGas:        350_000,
		BlockNumber:     24_000_000,
		Calldata:        []byte{0x01, 0x02},
		VictimTxHash:    []byte{0xab, 0xcd},
		TargetBlock:     24_000_001,
	}
	bundle := &Bundle{
		RawTxs:            [][]byte{{0x02, 0xbe, 0xef}, {0xf8, 0x6b}},
		BlockNumber:       24_000_001,
		Source:            SourceMempoolBackrun,
		VictimRawTx:       []byte{0x02, 0xbe, 0xef},
		VictimTxHashHex:   "0xdeadbeef",
		RevertingTxHashes: []string{"0xarbtx"},
	}
	gasFees := GasFees{
		BaseFee:        new(big.Int).SetUint64(20_000_000_000),  // 20 gwei
		MaxFeePerGas:   new(big.Int).SetUint64(40_000_000_000),  // 40 gwei
		MaxPriorityFee: new(big.Int).SetUint64(2_000_000_000),   // 2 gwei
		GasPriceGwei:   20.0,
	}
	decision := MempoolPreflightResult{
		Approved: true,
		Gates: []MempoolGateTrace{
			{Gate: "min_profit", Passed: true, Value: "500000000000000000"},
			{Gate: "max_tip_share", Passed: true, Value: "9000"},
		},
	}

	if err := dumpMempoolShadowBundle(arb, bundle, gasFees, 90.0, decision); err != nil {
		t.Fatalf("dumpMempoolShadowBundle: %v", err)
	}

	raw, err := os.ReadFile(filepath.Join(dir, "mempool-arb-1.json"))
	if err != nil {
		t.Fatalf("read dump: %v", err)
	}
	var payload map[string]interface{}
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("decode: %v", err)
	}

	// Schema must match the runbook + issue #140 spec verbatim — downstream
	// jq queries pin these names.
	required := []string{
		"arb_id", "source", "victim_tx_hash", "target_block", "built_at",
		"envelope", "expected_gross_profit_wei", "expected_net_profit_wei",
		"tip_share_bps", "gas_used", "base_fee_wei", "priority_fee_wei",
		"max_fee_per_gas_wei", "flashloan_provider", "flashloan_token",
		"flashloan_amount", "risk_decisions",
	}
	for _, k := range required {
		if _, ok := payload[k]; !ok {
			t.Errorf("missing required key %q", k)
		}
	}

	if got := payload["source"]; got != SourceMempoolBackrun {
		t.Errorf("source = %v, want %s", got, SourceMempoolBackrun)
	}
	if got := payload["victim_tx_hash"]; got != "0xdeadbeef" {
		t.Errorf("victim_tx_hash = %v", got)
	}
	if got := payload["flashloan_provider"]; got != "aave_v3" {
		t.Errorf("flashloan_provider = %v, want aave_v3", got)
	}

	// expected_gross_profit_wei = net + (gas * max_fee) = 5e17 + 350_000 * 40e9
	//                           = 500_000_000_000_000_000 + 14_000_000_000_000_000
	//                           = 514_000_000_000_000_000
	wantGross := "514000000000000000"
	if got := payload["expected_gross_profit_wei"]; got != wantGross {
		t.Errorf("expected_gross_profit_wei = %v, want %s", got, wantGross)
	}

	// risk_decisions must be a list of {gate,passed,value} maps.
	gates, ok := payload["risk_decisions"].([]interface{})
	if !ok || len(gates) != 2 {
		t.Fatalf("risk_decisions = %v, want 2 entries", payload["risk_decisions"])
	}
	first := gates[0].(map[string]interface{})
	if first["gate"] != "min_profit" || first["passed"] != true {
		t.Errorf("first gate = %v", first)
	}

	// Envelope must lead with the victim's RAW signed tx (never a bare hash —
	// builders reject hashes in eth_sendBundle), followed by our raw arb.
	env := payload["envelope"].(map[string]interface{})
	txs := env["txs"].([]interface{})
	if len(txs) != 2 || txs[0] != "0x02beef" || txs[1] != "0xf86b" {
		t.Errorf("envelope.txs = %v, want [victim_raw, raw_arb]", txs)
	}
	for _, tx := range txs {
		if tx == "0xdeadbeef" {
			t.Fatal("victim hash leaked into envelope.txs — builders reject bare hashes")
		}
	}
	// revertingTxHashes must be the arb hash only — never the victim.
	rev := env["revertingTxHashes"].([]interface{})
	if len(rev) != 1 || rev[0] != "0xarbtx" {
		t.Errorf("revertingTxHashes = %v, want [arb_hash] only", rev)
	}
	for _, h := range rev {
		if h == "0xdeadbeef" {
			t.Fatal("revertingTxHashes leaked victim hash — adverse-fill protection broken")
		}
	}
}

func TestDumpMempoolShadowBundle_SanitisesArbID(t *testing.T) {
	dir := t.TempDir()
	old := mempoolShadowSessionDir
	mempoolShadowSessionDir = func() string { return dir }
	defer func() { mempoolShadowSessionDir = old }()

	arb := &pb.ValidatedArb{
		Id:              "../../etc/passwd\x00",
		FlashloanToken:  []byte{},
		FlashloanAmount: []byte{},
		NetProfitWei:    []byte{},
	}
	bundle := &Bundle{}
	gasFees := GasFees{
		BaseFee:        big.NewInt(1),
		MaxFeePerGas:   big.NewInt(1),
		MaxPriorityFee: big.NewInt(1),
	}
	if err := dumpMempoolShadowBundle(arb, bundle, gasFees, 0, MempoolPreflightResult{}); err != nil {
		t.Fatalf("dump: %v", err)
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("readdir: %v", err)
	}
	if len(entries) != 1 {
		t.Fatalf("got %d files, want 1", len(entries))
	}
	name := entries[0].Name()
	for _, bad := range []string{"..", "/", "\x00", "\n"} {
		if containsSubstr(name, bad) {
			t.Errorf("filename %q still contains %q", name, bad)
		}
	}
}

func TestMempoolShadowSessionDir_RespectsReportsDir(t *testing.T) {
	t.Setenv("AETHER_REPORTS_DIR", "/tmp/aether-test-reports")
	// Use a fresh resolver instance to bypass the package-level sync.Once
	// that another test in this binary may have already triggered.
	fn := newMempoolShadowSessionDirOnce()
	got := fn()
	if !startsWith(got, "/tmp/aether-test-reports/shadow_mempool_") {
		t.Errorf("session dir = %q, want under /tmp/aether-test-reports/shadow_mempool_<ts>/", got)
	}
	if !endsWith(got, "/bundles") {
		t.Errorf("session dir = %q, want suffix /bundles", got)
	}
}

func startsWith(s, prefix string) bool {
	return len(s) >= len(prefix) && s[:len(prefix)] == prefix
}

func endsWith(s, suffix string) bool {
	return len(s) >= len(suffix) && s[len(s)-len(suffix):] == suffix
}
