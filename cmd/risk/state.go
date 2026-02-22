package main

import (
	"fmt"
	"log"

	"github.com/aether-arb/aether/internal/risk"
)

func main() {
	fmt.Println("aether-risk: risk management and circuit breaker service")

	config := risk.DefaultRiskConfig()
	rm := risk.NewRiskManager(config)

	log.Printf("Risk manager initialized in state: %s", rm.State())
	log.Printf("Max gas: %.0f gwei, Min profit: %.4f ETH, Max trade: %.1f ETH",
		config.MaxGasGwei, config.MinProfitETH, config.MaxSingleTradeETH)
}
