#!/usr/bin/env python3
"""
Aether Gas Profiler

Profiles gas usage patterns for DEX swap operations on Ethereum mainnet.
Used to calibrate the per-protocol gas estimation model in crates/detector/src/gas.rs.

Usage:
    python3 scripts/gas_profiler.py --rpc-url http://localhost:8545 --num-blocks 1000
    python3 scripts/gas_profiler.py --tx-hash 0xabc123...
    python3 scripts/gas_profiler.py --protocol UniswapV3 --num-blocks 500
"""

import argparse
import json
import statistics
import sys
import time
from collections import defaultdict
from dataclasses import dataclass
from typing import Optional
from urllib.request import Request, urlopen
from urllib.error import URLError


# Protocol router/factory addresses
PROTOCOL_ROUTERS = {
    "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D": "UniswapV2Router",
    "0xE592427A0AEce92De3Edee1F18E0157C05861564": "UniswapV3Router",
    "0xd9e1cE17f2641f24aE83637ab66a2cca9C378B9F": "SushiSwapRouter",
    "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7": "Curve3Pool",
    "0xBA12222222228d8Ba445958a75a0704d566BF2C8": "BalancerVault",
}

# Swap event topic0 signatures
SWAP_SIGNATURES = {
    "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822": "UniswapV2",
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67": "UniswapV3",
    "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140": "Curve",
    "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b": "Balancer",
}


@dataclass
class GasProfile:
    protocol: str
    tx_hash: str
    gas_used: int
    gas_price: int
    block_number: int
    num_swaps: int


@dataclass
class ProtocolStats:
    protocol: str
    sample_count: int
    mean_gas: float
    median_gas: float
    p95_gas: float
    p99_gas: float
    min_gas: int
    max_gas: int
    std_dev: float


def rpc_call(url: str, method: str, params: list) -> dict:
    """Make a JSON-RPC call."""
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


def get_tx_receipt(rpc_url: str, tx_hash: str) -> dict:
    """Get transaction receipt with gas used."""
    return rpc_call(rpc_url, "eth_getTransactionReceipt", [tx_hash])


def get_block_receipts(rpc_url: str, block_number: int) -> list[dict]:
    """Get all transaction receipts for a block."""
    result = rpc_call(rpc_url, "eth_getBlockReceipts", [hex(block_number)])
    return result if isinstance(result, list) else []


def get_latest_block(rpc_url: str) -> int:
    """Get the latest block number."""
    result = rpc_call(rpc_url, "eth_blockNumber", [])
    return int(result, 16) if result else 0


def classify_tx_protocol(receipt: dict) -> Optional[str]:
    """Classify which DEX protocol a transaction interacted with."""
    logs = receipt.get("logs", [])
    for log in logs:
        topics = log.get("topics", [])
        if topics:
            protocol = SWAP_SIGNATURES.get(topics[0])
            if protocol:
                return protocol

    to_addr = receipt.get("to", "")
    return PROTOCOL_ROUTERS.get(to_addr)


def count_swaps_in_tx(receipt: dict) -> int:
    """Count number of swap events in a transaction."""
    count = 0
    for log in receipt.get("logs", []):
        topics = log.get("topics", [])
        if topics and topics[0] in SWAP_SIGNATURES:
            count += 1
    return count


def profile_block(rpc_url: str, block_number: int) -> list[GasProfile]:
    """Profile all swap transactions in a block."""
    receipts = get_block_receipts(rpc_url, block_number)
    profiles = []

    for receipt in receipts:
        if receipt.get("status") != "0x1":
            continue

        protocol = classify_tx_protocol(receipt)
        if not protocol:
            continue

        gas_used = int(receipt.get("gasUsed", "0x0"), 16)
        gas_price = int(receipt.get("effectiveGasPrice", "0x0"), 16)
        num_swaps = count_swaps_in_tx(receipt)

        if num_swaps > 0:
            profiles.append(GasProfile(
                protocol=protocol,
                tx_hash=receipt.get("transactionHash", ""),
                gas_used=gas_used,
                gas_price=gas_price,
                block_number=block_number,
                num_swaps=num_swaps,
            ))

    return profiles


def compute_stats(profiles: list[GasProfile]) -> dict[str, ProtocolStats]:
    """Compute per-protocol gas statistics."""
    by_protocol: dict[str, list[int]] = defaultdict(list)

    for p in profiles:
        # Normalize to per-swap gas usage
        per_swap_gas = p.gas_used // p.num_swaps
        by_protocol[p.protocol].append(per_swap_gas)

    stats = {}
    for protocol, gas_values in sorted(by_protocol.items()):
        if len(gas_values) < 2:
            continue
        gas_values.sort()
        p95_idx = int(len(gas_values) * 0.95)
        p99_idx = int(len(gas_values) * 0.99)
        stats[protocol] = ProtocolStats(
            protocol=protocol,
            sample_count=len(gas_values),
            mean_gas=statistics.mean(gas_values),
            median_gas=statistics.median(gas_values),
            p95_gas=gas_values[p95_idx],
            p99_gas=gas_values[p99_idx],
            min_gas=min(gas_values),
            max_gas=max(gas_values),
            std_dev=statistics.stdev(gas_values),
        )

    return stats


def profile_single_tx(rpc_url: str, tx_hash: str) -> None:
    """Profile a single transaction."""
    receipt = get_tx_receipt(rpc_url, tx_hash)
    if not receipt:
        print(f"Transaction not found: {tx_hash}", file=sys.stderr)
        return

    gas_used = int(receipt.get("gasUsed", "0x0"), 16)
    gas_price = int(receipt.get("effectiveGasPrice", "0x0"), 16)
    status = "Success" if receipt.get("status") == "0x1" else "Reverted"
    protocol = classify_tx_protocol(receipt) or "Unknown"
    num_swaps = count_swaps_in_tx(receipt)
    block = int(receipt.get("blockNumber", "0x0"), 16)

    print(f"Transaction: {tx_hash}")
    print(f"Block:       {block}")
    print(f"Status:      {status}")
    print(f"Protocol:    {protocol}")
    print(f"Swap count:  {num_swaps}")
    print(f"Gas used:    {gas_used:,}")
    print(f"Gas price:   {gas_price / 1e9:.2f} gwei")
    print(f"Gas cost:    {gas_used * gas_price / 1e18:.6f} ETH")
    if num_swaps > 0:
        print(f"Per-swap:    {gas_used // num_swaps:,} gas")

    print("\nEvent breakdown:")
    for log in receipt.get("logs", []):
        topics = log.get("topics", [])
        if topics:
            sig = SWAP_SIGNATURES.get(topics[0], "")
            if sig:
                print(f"  {sig} swap @ {log.get('address', '')[:18]}...")


def run_profiler(
    rpc_url: str,
    num_blocks: int,
    protocol_filter: Optional[str] = None,
) -> dict[str, ProtocolStats]:
    """Profile gas across multiple blocks."""
    latest = get_latest_block(rpc_url)
    if latest == 0:
        print("Could not get latest block number", file=sys.stderr)
        return {}

    start_block = latest - num_blocks + 1
    print(f"Gas Profiler: scanning blocks {start_block} to {latest}")
    print(f"Protocol filter: {protocol_filter or 'all'}")
    print("-" * 60)

    all_profiles: list[GasProfile] = []
    start_time = time.time()

    for i, block in enumerate(range(start_block, latest + 1)):
        if (i + 1) % 50 == 0:
            elapsed = time.time() - start_time
            rate = (i + 1) / elapsed if elapsed > 0 else 0
            print(f"  Block {block} | {i + 1}/{num_blocks} | "
                  f"{rate:.1f} blocks/s | {len(all_profiles)} swap txs")

        profiles = profile_block(rpc_url, block)
        if protocol_filter:
            profiles = [p for p in profiles if p.protocol == protocol_filter]
        all_profiles.extend(profiles)

    duration = time.time() - start_time
    print(f"\nScanned {num_blocks} blocks in {duration:.1f}s "
          f"({num_blocks / duration:.1f} blocks/s)")
    print(f"Total swap transactions: {len(all_profiles)}")

    stats = compute_stats(all_profiles)

    print("\n" + "=" * 80)
    print("GAS USAGE STATISTICS (per swap)")
    print("=" * 80)
    print(f"{'Protocol':<16} {'Count':>7} {'Mean':>10} {'Median':>10} "
          f"{'P95':>10} {'P99':>10} {'StdDev':>10}")
    print("-" * 80)

    for protocol, s in sorted(stats.items()):
        print(f"{s.protocol:<16} {s.sample_count:>7} {s.mean_gas:>10,.0f} "
              f"{s.median_gas:>10,.0f} {s.p95_gas:>10,.0f} {s.p99_gas:>10,.0f} "
              f"{s.std_dev:>10,.0f}")

    print("\n" + "=" * 80)
    print("RECOMMENDED gas.rs CONSTANTS")
    print("=" * 80)
    for protocol, s in sorted(stats.items()):
        const_name = protocol.upper().replace(" ", "_") + "_GAS"
        # Use p95 as the estimate with some buffer
        recommended = int(s.p95_gas * 1.1)
        print(f"pub const {const_name}: u64 = {recommended:,};  "
              f"// p95={s.p95_gas:,.0f}, median={s.median_gas:,.0f}")

    return stats


def main():
    parser = argparse.ArgumentParser(description="Aether Gas Profiler")
    parser.add_argument("--rpc-url", default="http://localhost:8545",
                        help="Ethereum RPC endpoint URL")
    parser.add_argument("--num-blocks", type=int, default=100,
                        help="Number of recent blocks to scan")
    parser.add_argument("--protocol", type=str, default=None,
                        choices=["UniswapV2", "UniswapV3", "Curve", "Balancer", "SushiSwap"],
                        help="Filter to specific protocol")
    parser.add_argument("--tx-hash", type=str, default=None,
                        help="Profile a single transaction")
    args = parser.parse_args()

    if args.tx_hash:
        profile_single_tx(args.rpc_url, args.tx_hash)
    else:
        run_profiler(args.rpc_url, args.num_blocks, args.protocol)


if __name__ == "__main__":
    main()
