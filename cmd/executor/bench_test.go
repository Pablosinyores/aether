package main

import (
	"context"
	"math/big"
	"testing"

	"github.com/ethereum/go-ethereum/common"

	"github.com/aether-arb/aether/internal/risk"
	"github.com/aether-arb/aether/internal/testutil"
)

func BenchmarkBuildBundle(b *testing.B) {
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 90.0, 1)
	coinbase := common.HexToAddress("0x0000000000000000000000000000000000000001")

	calldata := []byte{0xab, 0xcd, 0xef, 0x01, 0x02, 0x03}
	profit := ethToWei(0.01)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		_, _ = bundler.BuildBundle(calldata, "0x0000000000000000000000000000000000000000", profit, 200000, 18000000, coinbase)
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
		_, _ = processArb(ctx, arb, rm, bundler, submitter, nil,
			"0x0000000000000000000000000000000000000000", 90.0, 0.5)
	}
}

func BenchmarkSubmitToAll(b *testing.B) {
	submitter := NewSubmitter(defaultBuilderConfigs())
	nm := NewNonceManager(0)
	go_ := NewGasOracle(300.0)
	bundler := NewBundleConstructor(nm, go_, nil, 90.0, 1)
	coinbase2 := common.HexToAddress("0x0000000000000000000000000000000000000001")

	bundle, _ := bundler.BuildBundle(
		[]byte{0xab, 0xcd},
		"0x0000000000000000000000000000000000000000",
		ethToWei(0.01),
		200000,
		18000000,
		coinbase2,
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
