---
layout: home

hero:
  name: Aether
  text: Cross-DEX Arbitrage Engine
  tagline: Sub-15ms opportunity detection across 6 DEX protocols on Ethereum Mainnet. Flash loan-backed execution with zero capital at risk.
  actions:
    - theme: brand
      text: Get Started
      link: /guide/getting-started
    - theme: alt
      text: How It Works
      link: /guide/how-it-works
    - theme: alt
      text: Architecture
      link: /architecture/overview

features:
  - title: Sub-Millisecond Detection
    details: Bellman-Ford negative cycle scan with SPFA optimization detects arbitrage in under 3ms across 5,000+ monitored pools.
  - title: Zero Capital Risk
    details: Every trade is flash loan-backed via Aave V3. Unprofitable transactions revert atomically on-chain. No capital ever at risk.
  - title: 6 DEX Protocols
    details: Uniswap V2/V3, SushiSwap, Curve, Balancer, and Bancor. Adding a new DEX is a single trait implementation.
  - title: Rust + Go + Solidity
    details: Rust for the latency-critical hot path. Go for coordination and monitoring. Solidity for on-chain settlement.
  - title: Multi-Builder Submission
    details: Simultaneous bundle submission to Flashbots, Titan, Beaver, and rsync builders for maximum inclusion probability.
  - title: Full Observability
    details: Prometheus metrics, Grafana dashboards, automated circuit breakers, and incident response playbooks.
---
