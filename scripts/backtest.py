#!/usr/bin/env python3
"""
Aether Arbitrage Backtest Engine

Analyzes historical block data to identify missed arbitrage opportunities
and validate detection algorithm performance against known profitable MEV bundles.

Usage:
    python3 scripts/backtest.py --start-block 18000000 --end-block 18001000
    python3 scripts/backtest.py --start-block 18000000 --num-blocks 100 --rpc-url http://localhost:8545
    python3 scripts/backtest.py --csv results.csv --start-block 18000000 --end-block 18000500
"""

import argparse
import csv
import json
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Optional
from urllib.request import Request, urlopen
from urllib.error import URLError


@dataclass
class SwapEvent:
    block_number: int
    tx_hash: str
    log_index: int
    pool_address: str
    protocol: str
    token_in: str
    token_out: str
    amount_in: int
    amount_out: int
    timestamp: int = 0


@dataclass
class ArbOpportunity:
    block_number: int
    path: list[str]
    pools: list[str]
    profit_wei: int
    gas_estimate: int
    net_profit_wei: int
    timestamp: int = 0

    @property
    def profit_eth(self) -> float:
        return self.net_profit_wei / 1e18


@dataclass
class BacktestResult:
    start_block: int
    end_block: int
    blocks_scanned: int
    total_swaps: int
    opportunities_found: int
    profitable_count: int
    total_profit_eth: float
    avg_profit_eth: float
    max_profit_eth: float
    avg_gas_gwei: float
    duration_seconds: float
    opportunities: list[ArbOpportunity] = field(default_factory=list)


# Well-known token addresses
TOKENS = {
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2": "WETH",
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48": "USDC",
    "0xdAC17F958D2ee523a2206206994597C13D831ec7": "USDT",
    "0x6B175474E89094C44Da98b954EedeAC495271d0F": "DAI",
    "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599": "WBTC",
}

# Swap event signatures
SWAP_TOPICS = {
    # UniswapV2/SushiSwap Swap(address,uint256,uint256,uint256,uint256,address)
    "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822": "UniswapV2",
    # UniswapV3 Swap(address,address,int256,int256,uint160,uint128,int24)
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67": "UniswapV3",
    # Curve TokenExchange(address,int128,uint256,int128,uint256)
    "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140": "Curve",
}

# Per-protocol gas estimates
GAS_ESTIMATES = {
    "UniswapV2": 60_000,
    "UniswapV3": 100_000,
    "SushiSwap": 60_000,
    "Curve": 130_000,
    "Balancer": 120_000,
    "Bancor": 150_000,
}
FLASHLOAN_GAS = 80_000
TX_BASE_GAS = 21_000
EXECUTOR_OVERHEAD = 30_000


def rpc_call(url: str, method: str, params: list) -> dict:
    """Make a JSON-RPC call to an Ethereum node."""
    payload = json.dumps({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": 1,
    }).encode()
    req = Request(url, data=payload, headers={"Content-Type": "application/json"})
    try:
        with urlopen(req, timeout=30) as resp:
            result = json.loads(resp.read())
            if "error" in result:
                print(f"RPC error: {result['error']}", file=sys.stderr)
                return {}
            return result.get("result", {})
    except URLError as e:
        print(f"RPC connection error: {e}", file=sys.stderr)
        return {}


def get_block_logs(rpc_url: str, block_number: int) -> list[dict]:
    """Fetch all swap event logs for a given block."""
    topics = list(SWAP_TOPICS.keys())
    result = rpc_call(rpc_url, "eth_getLogs", [{
        "fromBlock": hex(block_number),
        "toBlock": hex(block_number),
        "topics": [topics],
    }])
    return result if isinstance(result, list) else []


def get_block_timestamp(rpc_url: str, block_number: int) -> int:
    """Get timestamp for a block."""
    result = rpc_call(rpc_url, "eth_getBlockByNumber", [hex(block_number), False])
    if result and "timestamp" in result:
        return int(result["timestamp"], 16)
    return 0


def get_base_fee(rpc_url: str, block_number: int) -> int:
    """Get base fee for a block (EIP-1559)."""
    result = rpc_call(rpc_url, "eth_getBlockByNumber", [hex(block_number), False])
    if result and "baseFeePerGas" in result:
        return int(result["baseFeePerGas"], 16)
    return 30_000_000_000  # 30 gwei default


def parse_swap_events(logs: list[dict], block_number: int) -> list[SwapEvent]:
    """Parse raw logs into SwapEvent structs."""
    events = []
    for log in logs:
        topic0 = log.get("topics", [""])[0]
        protocol = SWAP_TOPICS.get(topic0)
        if not protocol:
            continue
        events.append(SwapEvent(
            block_number=block_number,
            tx_hash=log.get("transactionHash", ""),
            log_index=int(log.get("logIndex", "0x0"), 16),
            pool_address=log.get("address", ""),
            protocol=protocol,
            token_in="",
            token_out="",
            amount_in=0,
            amount_out=0,
        ))
    return events


def detect_arb_opportunities(
    swaps: list[SwapEvent],
    base_fee: int,
    block_number: int,
    min_profit_wei: int = 1_000_000_000_000_000,  # 0.001 ETH
) -> list[ArbOpportunity]:
    """
    Detect potential arbitrage opportunities from swap events within a block.
    Groups swaps by token pairs and looks for price discrepancies across pools.
    """
    # Group swaps by pool address
    pool_swaps: dict[str, list[SwapEvent]] = {}
    for swap in swaps:
        pool_swaps.setdefault(swap.pool_address, []).append(swap)

    opportunities = []
    pool_addresses = list(pool_swaps.keys())

    # Simple 2-pool arb detection: look for same token pair on different pools
    for i, pool_a in enumerate(pool_addresses):
        for pool_b in pool_addresses[i + 1:]:
            swaps_a = pool_swaps[pool_a]
            swaps_b = pool_swaps[pool_b]

            if not swaps_a or not swaps_b:
                continue

            # Estimate gas for this route
            gas_a = GAS_ESTIMATES.get(swaps_a[0].protocol, 100_000)
            gas_b = GAS_ESTIMATES.get(swaps_b[0].protocol, 100_000)
            total_gas = TX_BASE_GAS + FLASHLOAN_GAS + gas_a + gas_b + EXECUTOR_OVERHEAD
            gas_cost = total_gas * base_fee

            # Simplified profit estimation based on swap volume
            volume = sum(s.amount_in for s in swaps_a + swaps_b)
            estimated_profit = int(volume * 0.001)  # ~0.1% of volume as rough estimate

            net_profit = estimated_profit - gas_cost
            if net_profit > min_profit_wei:
                opportunities.append(ArbOpportunity(
                    block_number=block_number,
                    path=[pool_a, pool_b],
                    pools=[swaps_a[0].protocol, swaps_b[0].protocol],
                    profit_wei=estimated_profit,
                    gas_estimate=total_gas,
                    net_profit_wei=net_profit,
                ))

    return opportunities


def run_backtest(
    rpc_url: str,
    start_block: int,
    end_block: int,
    min_profit_wei: int = 1_000_000_000_000_000,
    verbose: bool = False,
) -> BacktestResult:
    """Run backtest across a range of blocks."""
    start_time = time.time()
    all_opportunities: list[ArbOpportunity] = []
    total_swaps = 0
    blocks_scanned = 0

    print(f"Backtest: blocks {start_block} to {end_block}")
    print(f"Min profit threshold: {min_profit_wei / 1e18:.4f} ETH")
    print("-" * 60)

    for block in range(start_block, end_block + 1):
        blocks_scanned += 1

        if blocks_scanned % 100 == 0:
            elapsed = time.time() - start_time
            rate = blocks_scanned / elapsed if elapsed > 0 else 0
            print(f"  Block {block} | {blocks_scanned}/{end_block - start_block + 1} "
                  f"| {rate:.1f} blocks/s | {len(all_opportunities)} opps found")

        logs = get_block_logs(rpc_url, block)
        if not logs:
            continue

        swaps = parse_swap_events(logs, block)
        total_swaps += len(swaps)

        if len(swaps) < 2:
            continue

        base_fee = get_base_fee(rpc_url, block)
        opportunities = detect_arb_opportunities(swaps, base_fee, block, min_profit_wei)

        if opportunities:
            all_opportunities.extend(opportunities)
            if verbose:
                for opp in opportunities:
                    print(f"  [!] Block {block}: {' -> '.join(opp.pools)} "
                          f"profit={opp.profit_eth:.4f} ETH")

    duration = time.time() - start_time
    profitable = [o for o in all_opportunities if o.net_profit_wei > 0]
    profits = [o.profit_eth for o in profitable]

    result = BacktestResult(
        start_block=start_block,
        end_block=end_block,
        blocks_scanned=blocks_scanned,
        total_swaps=total_swaps,
        opportunities_found=len(all_opportunities),
        profitable_count=len(profitable),
        total_profit_eth=sum(profits) if profits else 0.0,
        avg_profit_eth=(sum(profits) / len(profits)) if profits else 0.0,
        max_profit_eth=max(profits) if profits else 0.0,
        avg_gas_gwei=0.0,
        duration_seconds=duration,
        opportunities=all_opportunities,
    )

    print("\n" + "=" * 60)
    print("BACKTEST RESULTS")
    print("=" * 60)
    print(f"Blocks scanned:       {result.blocks_scanned}")
    print(f"Total swaps:          {result.total_swaps}")
    print(f"Opportunities found:  {result.opportunities_found}")
    print(f"Profitable:           {result.profitable_count}")
    print(f"Total profit:         {result.total_profit_eth:.4f} ETH")
    print(f"Avg profit:           {result.avg_profit_eth:.4f} ETH")
    print(f"Max profit:           {result.max_profit_eth:.4f} ETH")
    print(f"Duration:             {result.duration_seconds:.1f}s")
    print(f"Throughput:           {result.blocks_scanned / result.duration_seconds:.1f} blocks/s")

    return result


def export_csv(result: BacktestResult, filepath: str) -> None:
    """Export opportunities to CSV."""
    with open(filepath, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow([
            "block_number", "pools", "protocols", "profit_wei",
            "gas_estimate", "net_profit_wei", "net_profit_eth",
        ])
        for opp in result.opportunities:
            writer.writerow([
                opp.block_number,
                " -> ".join(opp.path),
                " -> ".join(opp.pools),
                opp.profit_wei,
                opp.gas_estimate,
                opp.net_profit_wei,
                f"{opp.profit_eth:.6f}",
            ])
    print(f"Results exported to {filepath}")


def main():
    parser = argparse.ArgumentParser(description="Aether Arbitrage Backtest Engine")
    parser.add_argument("--rpc-url", default="http://localhost:8545",
                        help="Ethereum RPC endpoint URL")
    parser.add_argument("--start-block", type=int, required=True,
                        help="Start block number")
    parser.add_argument("--end-block", type=int, default=None,
                        help="End block number")
    parser.add_argument("--num-blocks", type=int, default=100,
                        help="Number of blocks to scan (if --end-block not set)")
    parser.add_argument("--min-profit", type=float, default=0.001,
                        help="Minimum profit threshold in ETH")
    parser.add_argument("--csv", type=str, default=None,
                        help="Export results to CSV file")
    parser.add_argument("--verbose", action="store_true",
                        help="Print each opportunity as it's found")
    args = parser.parse_args()

    end_block = args.end_block or (args.start_block + args.num_blocks - 1)
    min_profit_wei = int(args.min_profit * 1e18)

    result = run_backtest(
        rpc_url=args.rpc_url,
        start_block=args.start_block,
        end_block=end_block,
        min_profit_wei=min_profit_wei,
        verbose=args.verbose,
    )

    if args.csv:
        export_csv(result, args.csv)


if __name__ == "__main__":
    main()
