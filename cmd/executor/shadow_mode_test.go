package main

import (
	"encoding/json"
	"math/big"
	"os"
	"path/filepath"
	"testing"

	pb "github.com/aether-arb/aether/internal/pb"
)

func TestIsShadowMode_Truthy(t *testing.T) {
	// Every value strconv.ParseBool accepts as true must enable shadow mode.
	// Covers the historical legacy inputs ("1", "true") and the new stdlib
	// set ("t", "T", "TRUE", "True").
	truthy := []string{"1", "t", "T", "TRUE", "true", "True"}
	for _, v := range truthy {
		t.Setenv("AETHER_SHADOW", v)
		if !isShadowMode() {
			t.Errorf("isShadowMode() with AETHER_SHADOW=%q: got false, want true", v)
		}
	}
}

func TestIsShadowMode_Falsy(t *testing.T) {
	falsy := []string{"0", "f", "F", "FALSE", "false", "False"}
	for _, v := range falsy {
		t.Setenv("AETHER_SHADOW", v)
		if isShadowMode() {
			t.Errorf("isShadowMode() with AETHER_SHADOW=%q: got true, want false", v)
		}
	}
}

func TestIsShadowMode_UnsetOrEmptyOrGarbage(t *testing.T) {
	// Defensive: garbage input (pasted-typo, leftover "on"/"yes" from the
	// legacy impl, arbitrary string) must fall through to false rather than
	// silently enabling shadow and suppressing real bundle submission.
	cases := []struct {
		name, val string
	}{
		{"empty", ""},
		{"whitespace", "   "},
		{"legacy yes", "yes"},
		{"legacy on", "on"},
		{"typo", "truue"},
		{"number garbage", "42"},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			t.Setenv("AETHER_SHADOW", c.val)
			if isShadowMode() {
				t.Fatalf("isShadowMode() with AETHER_SHADOW=%q: got true, want false", c.val)
			}
		})
	}
	// Also test genuinely unset.
	os.Unsetenv("AETHER_SHADOW")
	if isShadowMode() {
		t.Fatal("isShadowMode() with AETHER_SHADOW unset: got true, want false")
	}
}

func TestWeiToEth(t *testing.T) {
	cases := []struct {
		name string
		wei  *big.Int
		want float64
	}{
		{"nil", nil, 0},
		{"zero", big.NewInt(0), 0},
		{"one eth", new(big.Int).SetUint64(1e18), 1.0},
		{"0.001 eth", new(big.Int).SetUint64(1e15), 0.001},
		{"42 wei", big.NewInt(42), 4.2e-17},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := weiToEth(c.wei)
			// Loose tolerance — big.Float.Float64 rounding for very small
			// values approaches the f64 epsilon.
			const eps = 1e-20
			if got < c.want-eps || got > c.want+eps {
				t.Errorf("weiToEth(%v) = %v, want %v (±%v)", c.wei, got, c.want, eps)
			}
		})
	}
}

func TestShadowBundleDumpDir_DefaultAndOverride(t *testing.T) {
	// Default: ./reports/bundles when AETHER_SHADOW_DUMP_DIR is unset.
	os.Unsetenv("AETHER_SHADOW_DUMP_DIR")
	got := shadowBundleDumpDir()
	if got != "reports/bundles" {
		t.Errorf("default shadowBundleDumpDir = %q, want \"reports/bundles\"", got)
	}
	// Override via env.
	t.Setenv("AETHER_SHADOW_DUMP_DIR", "/tmp/custom-bundles")
	if shadowBundleDumpDir() != "/tmp/custom-bundles" {
		t.Errorf("override did not take effect")
	}
	// Whitespace-only override should fall back to default.
	t.Setenv("AETHER_SHADOW_DUMP_DIR", "   ")
	if shadowBundleDumpDir() != "reports/bundles" {
		t.Errorf("whitespace-only override leaked into result")
	}
}

// TestDumpShadowBundle_RoundTrip is the JSON-schema pin the reviewer asked
// for: feed a realistic ValidatedArb + Bundle through dumpShadowBundle and
// assert every key downstream consumers grep for is present in the output,
// with the right types.
func TestDumpShadowBundle_RoundTrip(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("AETHER_SHADOW_DUMP_DIR", dir)

	weth := []byte{0xc0, 0x2a, 0xaa, 0x39, 0xb2, 0x23, 0xfe, 0x8d, 0x0a, 0x0e,
		0x5c, 0x4f, 0x27, 0xea, 0xd9, 0x08, 0x3c, 0x75, 0x6c, 0xc2}
	dai := []byte{0x6b, 0x17, 0x54, 0x74, 0xe8, 0x90, 0x94, 0xc4, 0x4d, 0xa9,
		0x8b, 0x95, 0x4e, 0xed, 0xea, 0xc4, 0x95, 0x27, 0x1d, 0x0f}
	pool := []byte{0xa4, 0x78, 0xc2, 0x97, 0x5a, 0xb1, 0xea, 0x89, 0xe8, 0x19,
		0x68, 0x11, 0xf5, 0x1a, 0x7b, 0x7a, 0xde, 0x33, 0xeb, 0x11}

	arb := &pb.ValidatedArb{
		Id: "test-arb-1",
		Hops: []*pb.ArbHop{
			{
				Protocol:     pb.ProtocolType_UNISWAP_V2,
				PoolAddress:  pool,
				TokenIn:      weth,
				TokenOut:     dai,
				AmountIn:    new(big.Int).SetUint64(1_000_000_000_000_000_000).Bytes(), // 1e18
				ExpectedOut: mustBigInt("1800000000000000000000").Bytes(),             // 1.8e21
				EstimatedGas: 150_000,
			},
		},
		FlashloanToken:  weth,
		FlashloanAmount: new(big.Int).SetUint64(1_000_000_000_000_000_000).Bytes(),
		NetProfitWei:    new(big.Int).SetUint64(500_000_000_000_000_000).Bytes(), // 0.5 ETH
		TotalGas:        350_000,
		BlockNumber:     24_643_151,
		Calldata:        []byte{0x01, 0x02, 0x03, 0x04},
	}
	bundle := &Bundle{
		RawTxs:      [][]byte{{0xf8, 0x6b}, {0xf8, 0x6c}},
		BlockNumber: 24_643_152,
	}

	if err := dumpShadowBundle(arb, bundle, 0.5, 20.0, 95.0); err != nil {
		t.Fatalf("dumpShadowBundle: %v", err)
	}

	// File landed at <dir>/<sanitised-id>.json
	path := filepath.Join(dir, "test-arb-1.json")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read dump: %v", err)
	}

	var payload map[string]interface{}
	if err := json.Unmarshal(raw, &payload); err != nil {
		t.Fatalf("decode: %v", err)
	}

	// Required keys — any rename silently breaks consumers that jq-grep by
	// name, so pin them.
	required := []string{
		"ts", "arb_id", "target_block", "source_block", "path", "hops",
		"flashloan_token", "flashloan_amount", "net_profit_wei",
		"net_profit_eth", "total_gas", "gas_price_gwei", "tip_share_pct",
		"tx_count", "raw_tx_hex", "calldata_hex",
	}
	for _, k := range required {
		if _, ok := payload[k]; !ok {
			t.Errorf("missing required key %q in shadow bundle JSON", k)
		}
	}

	// Spot-check that tokens got labelled (not left as raw hex) when they
	// match the known set.
	pathSlice, ok := payload["path"].([]interface{})
	if !ok || len(pathSlice) != 2 {
		t.Fatalf("path = %v, want [WETH, DAI]", payload["path"])
	}
	if pathSlice[0] != "WETH" || pathSlice[1] != "DAI" {
		t.Errorf("path token labels = %v, want [WETH, DAI]", pathSlice)
	}

	// tx_count must match the bundle's RawTxs length.
	if got, want := payload["tx_count"], float64(2); got != want {
		t.Errorf("tx_count = %v, want %v", got, want)
	}

	// calldata_hex round-trips the raw calldata.
	if got, want := payload["calldata_hex"], "0x01020304"; got != want {
		t.Errorf("calldata_hex = %v, want %v", got, want)
	}
}

func TestDumpShadowBundle_SanitisesArbID(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("AETHER_SHADOW_DUMP_DIR", dir)

	// A malicious or malformed arb_id shouldn't allow directory traversal
	// or shell-special chars to leak into the filename.
	arb := &pb.ValidatedArb{
		Id:              "../../etc/passwd\x00\n",
		Hops:            []*pb.ArbHop{},
		FlashloanToken:  []byte{},
		FlashloanAmount: []byte{},
		NetProfitWei:    []byte{},
	}
	bundle := &Bundle{RawTxs: nil, BlockNumber: 0}

	if err := dumpShadowBundle(arb, bundle, 0, 0, 0); err != nil {
		t.Fatalf("dumpShadowBundle: %v", err)
	}

	// Dir must contain exactly one .json file; its name must not include
	// `..`, `/`, null, or newline.
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
			t.Errorf("sanitised filename %q still containsSubstr %q", name, bad)
		}
	}
}

// containsSubstr is a trivial helper so we don't depend on strings.Contains
// in a test that's already testing shell-char sanitisation.
func containsSubstr(haystack, needle string) bool {
	for i := 0; i+len(needle) <= len(haystack); i++ {
		if haystack[i:i+len(needle)] == needle {
			return true
		}
	}
	return false
}

// mustBigInt parses a base-10 big-int literal or panics. Used for test
// fixtures that exceed int64 (ExpectedOut values in DAI's 18-decimal
// representation trivially overflow `big.NewInt`).
func mustBigInt(s string) *big.Int {
	n, ok := new(big.Int).SetString(s, 10)
	if !ok {
		panic("mustBigInt: bad literal " + s)
	}
	return n
}
