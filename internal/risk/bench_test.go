package risk

import (
	"math/big"
	"sync"
	"testing"
)

func ethToWei(eth float64) *big.Int {
	f := new(big.Float).SetFloat64(eth)
	f.Mul(f, new(big.Float).SetFloat64(1e18))
	wei, _ := f.Int(nil)
	return wei
}

func BenchmarkPreflightCheck_Approved(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())
	profit := ethToWei(0.01)
	trade := ethToWei(5.0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.PreflightCheck(profit, trade, 30.0, 90.0, 0.5)
	}
}

func BenchmarkPreflightCheck_RejectedGas(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())
	profit := ethToWei(0.01)
	trade := ethToWei(5.0)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.PreflightCheck(profit, trade, 350.0, 90.0, 0.5)
	}
}

func BenchmarkPreflightCheck_Concurrent(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())
	profit := ethToWei(0.01)
	trade := ethToWei(5.0)

	b.ResetTimer()
	b.ReportAllocs()
	b.RunParallel(func(pb *testing.PB) {
		for pb.Next() {
			rm.PreflightCheck(profit, trade, 30.0, 90.0, 0.5)
		}
	})
}

func BenchmarkRecordRevert(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.RecordRevert()
	}
}

func BenchmarkRecordBundleResult(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.RecordBundleResult(i%3 != 0) // 2/3 included
	}
}

func BenchmarkBundleMissRate(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())
	for i := 0; i < 100; i++ {
		rm.RecordBundleResult(i%3 != 0)
	}

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.BundleMissRate()
	}
}

func BenchmarkWeiToETH(b *testing.B) {
	wei := ethToWei(1.5)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		WeiToETH(wei)
	}
}

func BenchmarkRecordTrade(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())
	volume := ethToWei(0.1)
	pnl := ethToWei(0.001)

	b.ResetTimer()
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		rm.RecordTrade(volume, pnl)
	}
}

func BenchmarkRecordBundleResult_Concurrent(b *testing.B) {
	rm := NewRiskManager(DefaultRiskConfig())

	b.ResetTimer()
	b.ReportAllocs()
	b.RunParallel(func(pb *testing.PB) {
		i := 0
		for pb.Next() {
			rm.RecordBundleResult(i%2 == 0)
			i++
		}
	})
}

func BenchmarkStateMachineTransitions(b *testing.B) {
	b.ReportAllocs()
	for i := 0; i < b.N; i++ {
		sm := NewSystemStateMachine()
		_ = sm.Transition(StateDegraded)
		_ = sm.Transition(StatePaused)
		_ = sm.Transition(StateRunning)
	}
}

func BenchmarkStateMachineCurrent_Concurrent(b *testing.B) {
	sm := NewSystemStateMachine()

	// Background writer
	var wg sync.WaitGroup
	done := make(chan struct{})
	wg.Add(1)
	go func() {
		defer wg.Done()
		for {
			select {
			case <-done:
				return
			default:
				_ = sm.Transition(StateDegraded)
				_ = sm.Transition(StateRunning)
			}
		}
	}()

	b.ResetTimer()
	b.ReportAllocs()
	b.RunParallel(func(pb *testing.PB) {
		for pb.Next() {
			sm.Current()
		}
	})
	b.StopTimer()

	close(done)
	wg.Wait()
}
