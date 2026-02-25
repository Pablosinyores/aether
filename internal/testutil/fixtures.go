package testutil

import (
	"math/big"

	pb "github.com/aether-arb/aether/internal/pb"
)

// ETHToWeiBytes converts an ETH float to wei bytes suitable for proto fields.
func ETHToWeiBytes(eth float64) []byte {
	f := new(big.Float).SetFloat64(eth)
	f.Mul(f, new(big.Float).SetFloat64(1e18))
	wei, _ := f.Int(nil)
	return wei.Bytes()
}

// ProfitableTriangleArb returns a ValidatedArb representing a 3-hop
// WETH→USDC→DAI→WETH triangle arbitrage with 0.01 ETH profit.
func ProfitableTriangleArb() *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id: "arb-triangle-001",
		Hops: []*pb.ArbHop{
			{Protocol: pb.ProtocolType_UNISWAP_V2, PoolAddress: []byte{0x01}, TokenIn: []byte{0xAA}, TokenOut: []byte{0xBB}, AmountIn: ETHToWeiBytes(10.0), ExpectedOut: ETHToWeiBytes(25000.0), EstimatedGas: 60000},
			{Protocol: pb.ProtocolType_SUSHISWAP, PoolAddress: []byte{0x02}, TokenIn: []byte{0xBB}, TokenOut: []byte{0xCC}, AmountIn: ETHToWeiBytes(25000.0), ExpectedOut: ETHToWeiBytes(24900.0), EstimatedGas: 60000},
			{Protocol: pb.ProtocolType_CURVE, PoolAddress: []byte{0x03}, TokenIn: []byte{0xCC}, TokenOut: []byte{0xAA}, AmountIn: ETHToWeiBytes(24900.0), ExpectedOut: ETHToWeiBytes(10.01), EstimatedGas: 130000},
		},
		TotalGas:        350000,
		NetProfitWei:    ETHToWeiBytes(0.01),
		BlockNumber:     18000000,
		FlashloanAmount: ETHToWeiBytes(10.0),
		FlashloanToken:  []byte{0xAA},
		Calldata:        []byte{0xab, 0xcd, 0xef, 0x01},
	}
}

// Profitable2HopArb returns a ValidatedArb representing a 2-hop
// arbitrage with 0.005 ETH profit.
func Profitable2HopArb() *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id: "arb-2hop-001",
		Hops: []*pb.ArbHop{
			{Protocol: pb.ProtocolType_UNISWAP_V3, PoolAddress: []byte{0x04}, TokenIn: []byte{0xAA}, TokenOut: []byte{0xBB}, AmountIn: ETHToWeiBytes(5.0), ExpectedOut: ETHToWeiBytes(5.005), EstimatedGas: 100000},
			{Protocol: pb.ProtocolType_BALANCER_V2, PoolAddress: []byte{0x05}, TokenIn: []byte{0xBB}, TokenOut: []byte{0xAA}, AmountIn: ETHToWeiBytes(5.005), ExpectedOut: ETHToWeiBytes(5.005), EstimatedGas: 120000},
		},
		TotalGas:        320000,
		NetProfitWei:    ETHToWeiBytes(0.005),
		BlockNumber:     18000001,
		FlashloanAmount: ETHToWeiBytes(5.0),
		FlashloanToken:  []byte{0xAA},
		Calldata:        []byte{0xab, 0xcd, 0xef, 0x02},
	}
}

// MarginalProfitArb returns an arb just above the 0.001 ETH minimum threshold.
func MarginalProfitArb() *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id: "arb-marginal-001",
		Hops: []*pb.ArbHop{
			{Protocol: pb.ProtocolType_UNISWAP_V2, PoolAddress: []byte{0x06}, TokenIn: []byte{0xAA}, TokenOut: []byte{0xBB}, AmountIn: ETHToWeiBytes(1.0), ExpectedOut: ETHToWeiBytes(1.0011), EstimatedGas: 60000},
		},
		TotalGas:        160000,
		NetProfitWei:    ETHToWeiBytes(0.0011),
		BlockNumber:     18000002,
		FlashloanAmount: ETHToWeiBytes(1.0),
		FlashloanToken:  []byte{0xAA},
		Calldata:        []byte{0xab, 0xcd, 0xef, 0x03},
	}
}

// LowProfitArb returns an arb below the 0.001 ETH minimum (should be rejected).
func LowProfitArb() *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id: "arb-lowprofit-001",
		Hops: []*pb.ArbHop{
			{Protocol: pb.ProtocolType_UNISWAP_V2, PoolAddress: []byte{0x07}, TokenIn: []byte{0xAA}, TokenOut: []byte{0xBB}, AmountIn: ETHToWeiBytes(1.0), ExpectedOut: ETHToWeiBytes(1.00005), EstimatedGas: 60000},
		},
		TotalGas:        160000,
		NetProfitWei:    ETHToWeiBytes(0.0001),
		BlockNumber:     18000003,
		FlashloanAmount: ETHToWeiBytes(1.0),
		FlashloanToken:  []byte{0xAA},
		Calldata:        []byte{0xab, 0xcd, 0xef, 0x04},
	}
}

// LargeTradeArb returns an arb exceeding the 50 ETH single trade limit.
func LargeTradeArb() *pb.ValidatedArb {
	return &pb.ValidatedArb{
		Id: "arb-largetrade-001",
		Hops: []*pb.ArbHop{
			{Protocol: pb.ProtocolType_UNISWAP_V2, PoolAddress: []byte{0x08}, TokenIn: []byte{0xAA}, TokenOut: []byte{0xBB}, AmountIn: ETHToWeiBytes(60.0), ExpectedOut: ETHToWeiBytes(60.5), EstimatedGas: 60000},
		},
		TotalGas:        160000,
		NetProfitWei:    ETHToWeiBytes(0.5),
		BlockNumber:     18000004,
		FlashloanAmount: ETHToWeiBytes(60.0),
		FlashloanToken:  []byte{0xAA},
		Calldata:        []byte{0xab, 0xcd, 0xef, 0x05},
	}
}

// BatchArbs returns a slice of arbs with mixed profitability for stream tests.
func BatchArbs() []*pb.ValidatedArb {
	return []*pb.ValidatedArb{
		ProfitableTriangleArb(),
		Profitable2HopArb(),
		MarginalProfitArb(),
		LowProfitArb(),
		LargeTradeArb(),
	}
}
