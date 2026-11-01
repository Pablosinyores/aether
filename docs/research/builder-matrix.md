# Ethereum Mainnet Block Builder Matrix (2025-2026)

## Executive Summary

The Ethereum mainnet builder market in 2025-2026 remains highly concentrated, with **Beaverbuild, Titan, and rsync-builder** consistently producing ~85-95% of MEV-Boost blocks between them, and Flashbots' own builder fading to single-digit share since it open-sourced and pivoted toward BuilderNet. The two MEV-Share endpoints (Flashbots Protect and MEV-Share v2) remain Flashbots-exclusive on the matchmaker side, but Titan, Beaver, and rsync now consume Share hints and honor refund rules. Builder ordering is overwhelmingly priority-gas-first with proprietary kicker bonuses for bundles that carry validator refunds or backrun MEV-Share txs. OFAC posture is bifurcated: Flashbots and BloXroute (regulated) censor; Titan, Beaver, rsync, Eden, Manifold and BuilderNet are non-censoring and reach Ultrasound/Agnostic relays.

---

## Canonical Comparison Table

| Builder | Mainnet share % (recent, late 2025) | Bundle endpoint URL | Auth | `eth_sendBundle` | `mev_sendBundle` (Share v2) | Reverting tx allowed | Ordering policy | OFAC-compliant relay only | Refund mechanism | Geographic POPs | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **Flashbots** | ~5-8% (own builder); 100% of MEV-Share flow | `https://relay.flashbots.net`, `https://mev-share.flashbots.net` | `X-Flashbots-Signature` (ECDSA over body, any EOA) | Yes | Yes (originator) | Via `revertingTxHashes` | Priority-fee + Share refund scoring; harmful-MEV filter | Yes (censoring) | MEV-Share user refund + builder coinbase tip | US-East (AWS us-east), EU | Reference impl; only Share v2 matchmaker |
| **Titan Builder** | ~30-40% | `https://rpc.titanbuilder.xyz` | `X-Flashbots-Signature` (compatible) | Yes | Yes (consumer) | Yes | Priority-fee-first, coinbase-tip aware; no harmful-MEV filter | No (multi-relay incl. Ultrasound, Agnostic) | coinbase.transfer + Share refund honored | NY, AMS, SGP (anycast) | Highest-share builder; permissive |
| **Beaverbuild** | ~30-40% | `https://rpc.beaverbuild.org` | `X-Flashbots-Signature` (compatible) | Yes | Yes (consumer) | Yes (via flag) | Priority + coinbase; no filter | No (incl. Ultrasound, Agnostic, Aestus) | coinbase.transfer; Share refund honored | NY, AMS | Often #1 by share; very permissive |
| **rsync-builder** | ~10-15% | `https://rsync-builder.xyz` | `X-Flashbots-Signature` | Yes | Yes (consumer) | Yes | Priority + coinbase | No (multi-relay incl. Ultrasound) | coinbase.transfer | EU (DE), NY | Operated by rsync staking team |
| **Eden Network** | ~1-3% | `https://api.edennetwork.io/v1/bundle` | `X-Flashbots-Signature` | Yes | Partial (consumes hints) | Yes | Priority + Eden staker preference | No | coinbase.transfer; Eden token bonuses deprecated | NY, EU | Reduced relevance post-2024 |
| **BuilderNet (Flashbots/BuildAI)** | ~3-7% (growing) | `https://buildernet.org` / `https://rpc.buildernet.org` | TLS attestation + `X-Flashbots-Signature` | Yes | Yes | Yes | Priority + TEE-attested fair ordering; harmful-MEV filter optional | No (multi-relay) | coinbase.transfer + protocol-level redistribution | US-East, EU (TEE/SGX) | TEE-based decentralized builder, replaces Flashbots builder long-term |
| **Manifold / Penguin** | <1% (Penguin defunct ~2024) | `https://api.manifoldfinance.com` (Manifold OFAC-aware relay) | `X-Flashbots-Signature` | Yes (when active) | Limited | Yes | Priority-first | No | coinbase.transfer | NY, EU | Manifold runs SecureRPC and Manifold relay; minimal builder share |
| **bloXroute MEV Relay** | ~5-10% (across multiple builder backends) | `https://mev.api.blxrbdn.com` | Bearer auth (BLXR API key) | Yes | Limited | Yes | Priority + BDN propagation bonus | Mixed: "regulated" relay is OFAC-compliant, "ethical" and "max-profit" are not | coinbase.transfer | Global anycast BDN (NY, AMS, FRA, SGP, TYO) | Operates relays + cooperating builder; subscription model |
| **Aqua Builder** | <1% (small independent builder, added 2026) | `https://rpc.aquabuilder.online` | Optional `X-Flashbots-Signature` (required for `eth_cancelBundle` / `replacementUuid`); no whitelist | Yes (refund fields required) | Yes (v0.1, full signed txs; no hash/backrun entries) | Via `revertingTxHashes` | rbuilder default ordering (priority + coinbase) | No (builder-side OFAC filter; submits to Flashbots, Ultra Sound, Titan, Agnostic) | Refund-only: bundles must carry `refundPercent`/`refundRecipient`; 91% of bundle marginal contribution refunded in won blocks, paid daily | Single site (TR) | Free, non-exclusive. Docs: https://aquabuilder.online/docs/ |
| **Agnostic Gnosis** | n/a (relay, not builder) | `https://agnostic-relay.net` | n/a (relay endpoint for validators) | n/a | n/a | n/a | Non-filtering relay | No | n/a | EU | Relay only; pairs with Titan/Beaver/rsync |
| **Ultrasound Relay** | n/a (relay, not builder) | `https://relay.ultrasound.money` | n/a | n/a | n/a | n/a | Non-censoring relay; optimistic submission | No | n/a | NY, AMS, TYO | Relay only; primary non-censoring conduit for top builders |

> Share percentages are approximate, drawn from `mevboost.pics` and `relayscan.io` trailing-7-day windows observed across 2025. Numbers shift weekly; treat as ranking guidance, not absolute.

---

## Per-Builder Deep Dives

### 1. Flashbots

#### Endpoints
- **Bundle submit**: `https://relay.flashbots.net` — `eth_sendBundle`, `eth_callBundle`, `eth_sendPrivateTransaction`, `eth_cancelBundle`.
- **MEV-Share v2**: `https://mev-share.flashbots.net` — `mev_sendBundle`, `mev_simBundle`. SSE event stream at `https://mev-share.flashbots.net/api/v1/events` for hint consumption.
- **Auth**: `X-Flashbots-Signature: <signing_address>:<ecdsa_sig>` where signature is `keccak256(body)` signed by any EOA. The signing key is **reputation-bearing**, not a funded EOA — Flashbots tracks bundle-acceptance reputation per signing key.
- **Rate limits**: Soft-rate-limited per signing key; reputation gates raise it. Documented baseline is ~5 bundles/sec/key; new keys are throttled to ~1/sec until reputation accrues.

#### Accepted bundle shapes
- Full `eth_sendBundle` envelope with `txs`, `blockNumber`, `minTimestamp`, `maxTimestamp`, `revertingTxHashes`, `replacementUuid`.
- `mev_sendBundle` v0.1 spec (the v2 protocol): nested `body` with `{ hash }` or `{ tx }` items, `inclusion: { block, maxBlock }`, `validity: { refund: [...], refundConfig: [...] }`, `privacy: { hints, builders }`.
- `eth_sendPrivateTransaction` with `maxBlockNumber` and `preferences.privacy.builders` list.
- `eth_callBundle` for off-chain simulation against any historical or pending state.
- Max ~50 txs/bundle in practice; gas-limit-bound by block gas limit.

#### Ordering policy
- Priority-fee + coinbase-transfer aggregate. Bundle scoring uses `effective_gas_price = (gas_used * priority_fee + coinbase_transfer) / gas_used`.
- **Harmful-MEV filter**: Flashbots actively rejects/de-prioritizes bundles classified as toxic sandwiches against retail flow routed through Protect. Backruns of MEV-Share txs are favored.
- Block-top positioning is available for high-tip backruns; otherwise append-anywhere.

#### MEV-Share posture
- Originator and matchmaker of the v2 protocol.
- Consumes its own hint stream; can match user txs to searcher backruns and split refund per `refundConfig`.
- Refund split: default 90% to user, 10% to validator; searcher pays via coinbase tip, refund is deducted from that tip.

#### Censorship / OFAC
- **OFAC-compliant**. Will not include bundles touching SDN-listed addresses (Tornado Cash, certain Lazarus addresses).
- Flashbots' own MEV-Boost relay (`boost-relay.flashbots.net`) censors. Their builder submits only to censoring relays for self-built blocks but bundle propagation to other builders is unaffected.

#### Geographic infrastructure
- AWS us-east-1 primary; EU secondary. No documented NY4/NY5 cross-connect; latency from co-located searchers in Equinix NY5 is ~3-8ms over public internet.

#### Recent reputation / changes (2025-2026)
- Open-sourced builder code in 2024; pivoted strategic focus to **BuilderNet** (TEE-attested decentralized builder).
- Flashbots builder market share has declined steadily through 2025 as Titan/Beaver took over.
- Reputation system tightened in mid-2025: signing keys with high revert ratios now silently throttled.

---

### 2. Titan Builder

#### Endpoints
- **Bundle submit**: `https://rpc.titanbuilder.xyz`
- **Auth**: Flashbots-compatible `X-Flashbots-Signature`. Titan accepts any signing key; no whitelist.
- **Rate limits**: Not publicly documented; observed permissive (>20/sec) for established signers.

#### Accepted bundle shapes
- `eth_sendBundle`, `eth_callBundle`, `eth_sendPrivateTransaction`, `eth_cancelBundle` (all Flashbots-compatible).
- `mev_sendBundle` (Share v2) — Titan is a documented Share **consumer**, honors `refundConfig`.
- Reverting tx via `revertingTxHashes`.
- No documented bundle-size cap beyond block gas limit.

#### Ordering policy
- Priority-fee + coinbase tip aggregate.
- **No harmful-MEV filter**: sandwich and frontrun bundles accepted. This is the principal reason for Titan's #1 builder share.
- Append-anywhere ordering; top-of-block via high tip.

#### MEV-Share posture
- Consumer: subscribes to `mev-share.flashbots.net` event stream.
- Not a matchmaker (won't originate Share refunds for its own user-tx flow).
- Refund pass-through when bundle backruns a Share-flagged tx.

#### Censorship / OFAC
- **Non-censoring**. Accepts bundles touching OFAC-sanctioned addresses.
- Reaches Ultrasound, Agnostic, Aestus (non-censoring), plus BloXroute regulated/ethical/max-profit, plus Flashbots relay (Titan blocks submitted to Flashbots relay are filtered there, not at the builder).

#### Geographic infrastructure
- NY, AMS, and SGP POPs (anycast'd to nearest). Co-located searchers in Equinix NY5 typically see <2ms RTT to Titan NY endpoint.

#### Recent reputation / changes
- Sustained #1 or #2 share through 2025.
- Reported in Sorella/Brontes analyses as the most permissive top builder; preferred destination for sandwich searchers.
- No public incidents of bundle leakage in 2025.

---

### 3. Beaverbuild

#### Endpoints
- **Bundle submit**: `https://rpc.beaverbuild.org`
- **Auth**: Flashbots-compatible `X-Flashbots-Signature`. Anonymous signing keys accepted.
- **Rate limits**: Not documented; high in practice.

#### Accepted bundle shapes
- `eth_sendBundle`, `eth_callBundle`, `eth_sendPrivateTransaction`, `eth_cancelBundle`.
- `mev_sendBundle` v0.1 consumed.
- Reverting tx allowed via `revertingTxHashes` and a `droppingTxHashes` extension (beaver-specific) for txs that may be dropped if uncompetitive.

#### Ordering policy
- Priority + coinbase, no harmful-MEV filter.
- Beaver has historically been the most aggressive about including high-coinbase-tip bundles regardless of intent.

#### MEV-Share posture
- Consumer; honors Share refund config.
- Not a matchmaker.

#### Censorship / OFAC
- **Non-censoring**. Reaches Ultrasound, Agnostic, Aestus, plus the BloXroute non-regulated relays.

#### Geographic infrastructure
- NY and AMS POPs. Anonymous operator; infrastructure provider not disclosed, but observed latency from NY5 is sub-2ms.

#### Recent reputation / changes
- Frequently the #1 builder by daily block share in 2025.
- Operator identity remains undisclosed; surfaced as a top-tier independent builder around 2023 and grew rapidly.
- No major incidents in 2025.

---

### 4. rsync-builder

#### Endpoints
- **Bundle submit**: `https://rsync-builder.xyz`
- **Auth**: `X-Flashbots-Signature`.
- **Rate limits**: Not documented.

#### Accepted bundle shapes
- `eth_sendBundle`, `eth_callBundle`, `eth_sendPrivateTransaction`.
- `mev_sendBundle` consumed.
- `revertingTxHashes` accepted.

#### Ordering policy
- Priority + coinbase, no filter.

#### MEV-Share posture
- Consumer.

#### Censorship / OFAC
- Non-censoring; submits to Ultrasound, Agnostic, Aestus.

#### Geographic infrastructure
- EU (Germany) primary, NY secondary.

#### Recent reputation / changes
- Operated by rsync staking team. Consistent ~10-15% share through 2025. No public incidents.

---

### 5. Eden Network Builder

#### Endpoints
- **Bundle submit**: `https://api.edennetwork.io/v1/bundle`
- **Auth**: `X-Flashbots-Signature`.
- **Rate limits**: Not documented; lower throughput observed.

#### Accepted bundle shapes
- `eth_sendBundle`, `eth_callBundle`.
- Limited/partial `mev_sendBundle` support (hint consumption documented; refund honoring less clear).
- Reverting tx allowed.

#### Ordering policy
- Priority + coinbase. Historic Eden token staker preference largely deprecated post-2023.

#### MEV-Share posture
- Consumes hints but minimal documented refund pipeline.

#### Censorship / OFAC
- Non-censoring.

#### Geographic infrastructure
- NY, EU.

#### Recent reputation / changes
- Market share collapsed from ~5-8% (2022) to <3% by 2025. Still operational but de-prioritized in most searcher fan-out configs.

---

### 6. BuilderNet (Flashbots / BuildAI)

#### Endpoints
- **Bundle submit**: `https://buildernet.org` / `https://rpc.buildernet.org` (multiple TEE-operator endpoints; Flashbots, Beaver, and Nethermind run nodes).
- **Auth**: `X-Flashbots-Signature` + optional TLS attestation verification for the TEE endpoint.
- **Rate limits**: Per-signer reputation similar to Flashbots.

#### Accepted bundle shapes
- Full Flashbots envelope: `eth_sendBundle`, `eth_callBundle`, `eth_sendPrivateTransaction`, `eth_cancelBundle`.
- `mev_sendBundle` v0.1 fully supported.
- TEE attestation lets searchers verify their bundle was processed without leakage.

#### Ordering policy
- Priority + coinbase aggregate, with **TEE-attested fair ordering** guarantees.
- Harmful-MEV filter is configurable per BuilderNet node operator; the Flashbots node enables it, Beaver's BuilderNet node does not.

#### MEV-Share posture
- Full Share v2 consumer; integrated with Flashbots' matchmaker.

#### Censorship / OFAC
- Mixed by node: each TEE operator sets its own OFAC policy. The decentralized design means searchers can target non-censoring nodes specifically.
- Reaches multiple relays incl. Ultrasound.

#### Geographic infrastructure
- US-East and EU TEE clusters (Intel SGX / TDX).

#### Recent reputation / changes
- Launched late 2024; growing share through 2025 (~3-7% by end of 2025).
- Public Flashbots positioning: BuilderNet is the long-term replacement for the Flashbots monolithic builder.
- Key feature: searcher bundle privacy via TEE — historically the strongest counter to "builder steals my bundle" concerns.

---

### 7. Manifold / Penguin

#### Manifold
- **Endpoints**: `https://api.manifoldfinance.com` (SecureRPC + Manifold relay). Bundle endpoint historically `https://api.securerpc.com/v1`.
- **Auth**: `X-Flashbots-Signature`.
- **Bundle shapes**: `eth_sendBundle`, `eth_sendPrivateTransaction`.
- **MEV-Share**: limited.
- **OFAC**: Operates both a regulated SecureRPC relay (OFAC-compliant) and a non-regulated path.
- **Geographic**: NY, EU.
- **Recent**: Builder share <1% in 2025; primarily relevant as a relay operator.

#### Penguin Builder
- Effectively defunct since late 2023 / early 2024. Endpoint `https://builder.penguinbuild.org` is sometimes returned by historical fan-out lists but should not be in a 2026 production config.

---

### 8. bloXroute MEV Relay

#### Endpoints
- **Bundle submit**: `https://mev.api.blxrbdn.com` (and BDN-routed equivalents in each region).
- **Auth**: Bearer API key (BloXroute subscription account).
- **Rate limits**: Tied to subscription tier.

#### Accepted bundle shapes
- `blxr_submit_bundle` (BLXR-specific) and `eth_sendBundle`.
- `eth_sendPrivateTransaction` and `blxr_private_tx`.
- Limited `mev_sendBundle` consumption.

#### Ordering policy
- Priority + coinbase. BloXroute's BDN (Backbone Distributed Network) gives propagation latency advantages.

#### MEV-Share posture
- Limited consumer.

#### Censorship / OFAC
- **Bifurcated**: BloXroute operates three relays — `regulated` (OFAC-compliant), `ethical` (non-censoring, no front-running), and `max-profit` (non-censoring, no filter). Builder forwards bundles to all three by default; bundles touching OFAC-listed addresses are dropped from the regulated path only.

#### Geographic infrastructure
- Global BDN anycast: NY, AMS, FRA, SGP, TYO. Lowest-latency global fan-out of any builder.

#### Recent reputation / changes
- Subscription-only access; less commonly used by independent searchers in 2025 but valuable for global propagation.
- No 2025 incidents.

---

### 9. Agnostic (Gnosis) Relay

**This is a MEV-Boost relay, not a builder.**

- **Endpoint** (for validators): `https://agnostic-relay.net`
- **Role**: Non-censoring relay operated by the Gnosis team. Receives blocks from Titan, Beaver, rsync, BuilderNet non-censoring nodes.
- **OFAC**: Non-censoring.
- **Relevance to searchers**: You don't submit to Agnostic directly. You submit to a builder; the builder decides which relays to forward winning blocks to. Agnostic appearing in `relayscan.io` data for a builder confirms that builder is non-censoring.

---

### 10. Ultrasound Relay

**This is a MEV-Boost relay, not a builder.**

- **Endpoint** (for validators): `https://relay.ultrasound.money`
- **Role**: Non-censoring relay operated by the Ultrasound.money team. Pioneered **optimistic block submission** (relay forwards header before full body validation, reducing builder→proposer latency by ~50-100ms).
- **OFAC**: Non-censoring.
- **Geographic**: NY, AMS, TYO POPs.
- **Relevance**: Receives blocks from all top non-censoring builders (Titan, Beaver, rsync, BuilderNet). Highest-share non-censoring relay in 2025. Searchers do not submit here; the builders push winning blocks to this relay.
- **Recent**: Optimistic v2 launched 2024; demoted-builder list maintained publicly. Occasional incidents (one notable 2023 invalid-block bug fully resolved; clean record through 2025).

---

## Implementation Notes for `submitter.go` Fan-Out

### Backrun bundle (non-toxic, OFAC-clean)
Fan out **in parallel** to:
1. **Titan** — fastest acceptance, highest inclusion probability
2. **Beaverbuild** — second-highest share, near-identical latency profile
3. **rsync-builder** — third-highest share
4. **BuilderNet** (Flashbots/Beaver TEE nodes) — TEE-private, growing share
5. **Flashbots** (`relay.flashbots.net` + `mev-share.flashbots.net` for Share v2 matched bundles)
6. **bloXroute** (max-profit endpoint) — if subscription is active
7. **Eden** — low priority but no cost to include
8. **Aqua Builder** — very small share; only if the bundle carries refund fields (refund-only endpoint), no cost to include

### Frontrun / sandwich bundle (if policy-approved by your firm)
Fan out to:
1. **Titan** — most reliably accepts
2. **Beaverbuild** — most reliably accepts
3. **rsync-builder**
4. **bloXroute** (max-profit endpoint only)
5. **BuilderNet** — only non-filtering operator nodes

**Avoid**:
- **Flashbots** — harmful-MEV filter will reject or de-prioritize
- **bloXroute** regulated and ethical endpoints — ethical endpoint filters frontrunning
- BuilderNet Flashbots-operator node

### OFAC-sanctioned address touching (if policy-approved)
Fan out **only** to non-censoring builders:
1. Titan, Beaverbuild, rsync, BuilderNet non-censoring nodes, bloXroute max-profit.
Explicitly **avoid**: Flashbots, bloXroute regulated, Manifold SecureRPC regulated path. Bundles to these will be dropped silently or filtered upstream at the relay.

### Recommended sequential order if not parallel (lowest-latency first from Equinix NY5)
1. Beaverbuild (NY) — ~1ms
2. Titan (NY anycast) — ~1-2ms
3. rsync-builder (NY) — ~2-3ms (EU primary, NY secondary)
4. BuilderNet US-East — ~3-5ms
5. Flashbots (us-east-1) — ~3-8ms
6. bloXroute BDN — ~1-2ms but auth overhead
7. Eden — ~5-10ms
8. Manifold — ~5-10ms (lowest priority)

**Strong recommendation**: always run parallel fan-out via goroutines (matches the existing `cmd/executor/submitter.go` design). Sequential is only useful for low-tier fallback or if rate limits force serialization.

### `config/builders.yaml` shape implications
Each builder entry should expose at minimum:
- `name`, `endpoint`, `auth_type` (`flashbots_signature` | `bloxroute_bearer`), `auth_key_ref`
- `supports_eth_send_bundle`, `supports_mev_send_bundle`, `supports_private_tx`
- `accepts_reverting_tx`, `accepts_ofac_sanctioned`, `accepts_harmful_mev`
- `geographic_pop` (for latency-aware routing)
- `tier` (`primary` | `secondary` | `fallback`)
- `enabled_for_bundle_class` (`backrun`, `frontrun`, `sandwich`, `liquidation`)

This lets `submitter.go` build the recipient set per-bundle by tag match rather than hardcoded lists.

---

## Open Questions

The following were not fully verifiable from public sources at time of writing and should be confirmed empirically (test submissions + observed inclusion) or via direct builder contact:

1. **Exact current builder market share** — `mevboost.pics` and `relayscan.io` data shift weekly; numbers above are 2025 averages. Pull live before pinning a fan-out priority list.
2. **Beaverbuild operator identity** — still undisclosed; some 2025 forum speculation links it to a specific Asian trading firm but unconfirmed.
3. **Titan rate-limit ceilings** — Titan does not publish per-key limits; only empirically observable.
4. **BuilderNet per-node OFAC posture** — each TEE operator's policy is in principle declarable in attestation, but the per-node policy registry is not yet fully public. Verify which BuilderNet nodes are non-censoring before relying on the network for sanctioned-touching bundles.
5. **`mev_sendBundle` adoption breadth at non-Flashbots builders** — Titan/Beaver/rsync documented as consumers, but full conformance to v0.1 spec edge cases (nested matched bundles, refundConfig with multiple recipients) is not independently audited.
6. **bloXroute subscription cost/tier** for sustained high-rate searcher use — pricing is opaque and changes; contact required.
7. **Manifold builder current status** — appears to be inactive as a primary builder in 2025; SecureRPC remains active as a relay/RPC product. Confirm before including.
8. **Eden Network Phase 3 plans** — Eden has signaled product pivots multiple times; current builder roadmap unclear.
9. **Latency from Equinix NY5 specifically** — figures above are approximate; only co-located ping tests give accurate numbers, and these vary with cross-connect setup.
10. **Refund-config conformance** — whether non-Flashbots builders correctly enforce `refundConfig.percent` distributions when a Share-flagged tx is backrun by a bundle they're including. Anecdotal reports suggest Titan honors it; Beaver/rsync less verified.
11. **Penguin / other historical builders** (e.g. `builder0x69`, `f1b.io`) — assumed defunct or marginal in 2026; not included above. Verify before adding any to `builders.yaml`.
