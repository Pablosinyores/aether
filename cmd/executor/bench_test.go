package main

import (
	"context"
	"math/big"
	"testing"

	"github.com/aether-arb/aether/internal/risk"
	"github.com/aether-arb/aether/internal/testutil"
)

func BenchmarkBuildBundle(b *testing.B) {
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)

	calldata := []byte{0xab, 0xcd, 0xef, 0x01, 0x02, 0x03}

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		_, _ = bundler.BuildBundle(calldata, "0x0000000000000000000000000000000000000000", 200000, 18000000)
	}
}

func BenchmarkPreflightCheck(b *testing.B) {
	rm := risk.NewRiskManager(risk.DefaultRiskConfig())
	profit := ethToWei(0.01)
	trade := ethToWei(5.0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.PreflightCheck(profit, trade, 30.0, 90.0, 0.5)
	}
}

func BenchmarkPreflightCheck_Rejected(b *testing.B) {
	rm := risk.NewRiskManager(risk.DefaultRiskConfig())
	profit := ethToWei(0.0001) // below threshold
	trade := ethToWei(5.0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.PreflightCheck(profit, trade, 30.0, 90.0, 0.5)
	}
}

func BenchmarkNonceNext(b *testing.B) {
	nm := NewNonceManager(0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		nm.Next()
	}
}

func BenchmarkGasOracleCurrentFees(b *testing.B) {
	go_ := NewGasOracle(300.0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		go_.CurrentFees()
	}
}

func BenchmarkProcessArb(b *testing.B) {
	rm, bundler, submitter := newTestComponents()
	ctx := context.Background()
	arb := testutil.ProfitableTriangleArb()

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		_, _ = processArb(ctx, arb, rm, bundler, submitter,
			"0x0000000000000000000000000000000000000000", 0.5)
	}
}

func BenchmarkSubmitToAll(b *testing.B) {
	submitter := NewSubmitter(defaultBuilderConfigs())
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 1)

	bundle, _ := bundler.BuildBundle(
		[]byte{0xab, 0xcd},
		"0x0000000000000000000000000000000000000000",
		200000,
		18000000,
	)
	ctx := context.Background()

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		submitter.SubmitToAll(ctx, bundle)
	}
}

func BenchmarkWeiToETH(b *testing.B) {
	wei := ethToWei(1.5)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		risk.WeiToETH(wei)
	}
}

func BenchmarkGasOracleUpdate(b *testing.B) {
	go_ := NewGasOracle(300.0)
	baseFee := big.NewInt(30e9)
	priorityFee := big.NewInt(2e9)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		go_.Update(baseFee, priorityFee)
	}
}

func BenchmarkGenerateBundleID(b *testing.B) {
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		GenerateBundleID()
	}
}
