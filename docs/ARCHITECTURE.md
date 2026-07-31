# Technical Architecture — Stellar Strategy Rails

**Product:** Tokenized wrapper for the EDO Theory automated long/short strategy
**Chain:** Stellar — hybrid architecture: Classic Asset for the token, Soroban smart contracts for on-chain logic
**Settlement asset:** native USDC on Stellar
**Issuer:** Trakx (AMF-registered, France)

---

## 1. Design principles

1. **Hybrid architecture: Classic for value, Soroban for logic.** The strategy token is a Stellar Classic Asset with `AUTH_REQUIRED`, `AUTH_REVOCABLE` and `AUTH_CLAWBACK_ENABLED` flags — investor access control enforced at protocol level, with battle-tested security and minimal fees for transfers. All programmable logic that belongs on-chain lives in Soroban smart contracts: the **NAV Oracle** and the **Subscription Escrow**. Contracts interact with the Classic Asset through its built-in Stellar Asset Contract (SAC) interface.
2. **Execute-then-mint.** Tokens are only issued after the corresponding strategy exposure has been adjusted. Every token in circulation is therefore fully backed by the underlying position at all times. There are no liquidity pools and no on-chain market making: the chain is a distribution and ownership registry, not a liquidity venue.
3. **NAV computed off-chain, verifiable on-chain.** NAV is calculated off-chain by the same engine pricing Trakx's indices today (constituent prices, accrued fees, management fees), then published to the Soroban NAV Oracle as a signed submission. The oracle is the single on-chain pricing reference for all mint/redeem operations — and a composable price feed any other Soroban protocol can read.
4. **Write-ahead operational ledger, verified on-chain.** The on-chain state is the authoritative record of ownership and supply. Operationally, because issuance is primary (Trakx initiates every mint and redeem), the platform keeps a write-ahead operational ledger that records supply changes at execution time — before chain confirmation — so hedging never waits on network latency. Chain observation (Soroban RPC event streams, cursor sweep) continuously reconciles the two; a delayed stream can therefore never produce an incorrect hedge — only a reconciliation alert.
5. **Investor settlement on Stellar; treasury mobility via CCTP.** Investors always pay and receive native USDC **on Stellar**. Moving funds between Stellar and the venues where the strategy executes (EVM-based exchange infrastructure) is an internal Trakx treasury operation using Circle's CCTP (native burn-and-mint USDC transfers between Stellar and CCTP-enabled chains). The investor-facing product surface never depends on, or is exposed to, the treasury leg.

## 2. Component overview

```mermaid
flowchart LR
    subgraph Investor
        W[Wallet<br/>Freighter / xBull]
        P[Investor Portal<br/>React + Stellar Wallets Kit]
    end

    subgraph Trakx["Trakx Backend (C#/.NET, CQRS)"]
        KYC[KYC / Identity<br/>existing stack]
        CS[Compliance Service<br/>trustline authorization]
        NAV[NAV Service<br/>calculation + signed publication]
        MR[Mint/Redeem Engine<br/>execute-then-mint]
        LG[(Internal Ledger<br/>write-ahead ops journal)]
        RC[Reconciliation<br/>streaming + events + sweep]
        TR[Treasury Ops<br/>CCTP Stellar ↔ EVM]
    end

    subgraph Soroban["Soroban Smart Contracts"]
        ORC[NAVOracle<br/>SEP-40-compatible feed]
        ESC[SubscriptionEscrow<br/>USDC custody for pending ops]
    end

    subgraph Stellar["Stellar — Classic Layer"]
        ISS[Issuer Account<br/>AUTH_REQUIRED · AUTH_REVOCABLE · AUTH_CLAWBACK<br/>multisig, cold]
        DST[Distribution Account<br/>operational]
        USDC[USDC<br/>native asset / SAC]
    end

    subgraph Offchain["Strategy Infrastructure"]
        EX[Exchange / Execution<br/>long-short strategy]
    end

    W --> P
    P --> CS
    P -->|subscribe / redeem| ESC
    KYC --> CS
    CS -->|SetTrustLineFlags| ISS
    NAV -->|submit_nav signed| ORC
    ESC -->|events| MR
    MR -->|finalize at oracle NAV| ESC
    ORC -.->|price read| ESC
    MR --> LG
    MR -->|token issuance| DST
    MR -->|adjust exposure| EX
    ESC ---|SAC interface| USDC
    RC -->|Soroban RPC: events + classic ops| Stellar
    RC --> LG
    TR -->|CCTP burn/mint native USDC| USDC
    TR --> EX
```

## 3. Account & contract model

| Component | Role | Controls |
|---|---|---|
| **Issuer account** | Defines the Classic Asset; sets `AUTH_REQUIRED`, `AUTH_REVOCABLE`, `AUTH_CLAWBACK_ENABLED`; authorizes/revokes trustlines | Cold; multisig (medium/high thresholds); keys under Trakx custody policy. `AUTH_CLAWBACK_ENABLED` is part of day-0 configuration: it must be set **before the first trustline is opened**, since clawback applies only to trustlines created after the flag is enabled and cannot be added retroactively |
| **Distribution account** | Operational account for issuing tokens to investors and receiving them on redemption | Warm; limited balance; multisig on high-value ops |
| **NAVOracle (Soroban)** | On-chain NAV reference: stores latest NAV + history, validates publisher, exposes read functions | `require_auth()` on the authorized publisher address; upgrade/admin gated by issuer multisig |
| **SubscriptionEscrow (Soroban)** | Holds investor USDC for pending subscriptions/redemptions; settles at oracle NAV; refund path | `require_auth()` on operator functions; refunds callable by depositor after timeout |
| **Subscription flows (USDC)** | Monitored via Soroban RPC (contract events + Classic operations) | |

**Stellar Asset Contract deployment.** The EDO asset's **SAC instance is deployed at issuance** (`deploySACWithAsset`), so Soroban contracts — the SubscriptionEscrow in particular — interact with the token through the standard **SEP-41 token interface** (`transfer`, `burn`, and the admin functions `mint` / `set_authorized`). This is what lets the escrow hold and move the Classic Asset natively from contract code.

**Standards note (SEP-57).** We track **SEP-57 — T-REX (Token for Regulated EXchanges)**, the OpenZeppelin-led draft that ports the ERC-3643 permissioned-token framework to Soroban. Phase 1 deliberately uses protocol-native Classic controls (`AUTH_REQUIRED` / `AUTH_REVOCABLE` / `AUTH_CLAWBACK_ENABLED`): they are battle-tested, require no bespoke compliance contracts, and keep the security surface minimal. As SEP-57 matures, the rails can adopt its richer on-chain identity and compliance-module model for products that need it, without changing the issuance architecture.

## 4. Soroban contracts

### 4.1 NAVOracle

A single, deliberately small Rust contract acting as the authoritative on-chain NAV feed for each strategy token.

- **Interface:** SEP-40-compatible price feed functions (`lastprice`, `price(timestamp)`), plus `submit_nav(payload, signature)` for publication and `nav_age()` for staleness checks. SEP-40 compatibility makes the feed **composable**: any other Soroban protocol can consume Trakx product NAVs without custom integration.
- **State (persistent storage):** `LATEST_NAV { value, timestamp }`, a bounded ring buffer of historical entries, `AUTHORIZED_PUBLISHER: Address`, `STALENESS_THRESHOLD: u64`. TTL on persistent entries is extended programmatically by the backend (state-rent maintenance).
- **Publication flow:** the off-chain NAV service signs the payload; `submit_nav` enforces `require_auth()` on the publisher address and validates monotonic timestamps. On success the contract emits a `NAVUpdated(asset, value, timestamp)` event consumed by the backend, the reconciliation service and any third-party subscriber. Publication cadence is configurable: a daily official NAV at minimum, with intraday settlement publications supported (Stellar's sub-cent fees make frequent publication economically negligible). A minimum-interval guard rate-limits submissions on-chain.
- **Circuit breaker (on-chain deviation bound):** `submit_nav` rejects any value deviating from the last accepted NAV beyond a configurable per-product bound (`MAX_DEVIATION`). A rejected submission leaves state untouched, emits no `NAVUpdated`, and surfaces as a failed transaction that the publisher turns into an operator alert; meanwhile `nav_age()` keeps growing and dependent operations pause automatically once the staleness threshold is crossed — the system fails safe (paused, at the last valid price) rather than settling at a wrong one. Genuine extreme market moves are recorded through an override path (`submit_nav_override`) gated on a second, independent admin signature. The bound is a configuration parameter per deployed product, calibrated from each strategy's historical NAV series, complementing the off-chain sanity checks performed before signing (constituent-level outlier detection against stored price history, tolerance scaled by each constituent's volatility).
- **Consumers:** the SubscriptionEscrow reads the oracle for settlement pricing; automated operations pause if `nav_age()` exceeds the staleness threshold.

### 4.2 SubscriptionEscrow

The contract that moves the money path of primary issuance on-chain while preserving execute-then-mint.

- **Subscribe:** the investor calls `subscribe(amount)`; USDC moves from their wallet into the contract via the SAC. The contract records the pending intent **with its ledger timestamp** and emits `SubscriptionRequested(investor, amount)`.
- **Finalize:** after the backend confirms strategy exposure adjustment (the execute leg), it calls `finalize_subscription(investor)`. The contract reads the current NAV from NAVOracle, computes the token quantity, releases the USDC to the treasury path, and emits `SubscriptionSettled(investor, usdc, tokens, nav)`. Token issuance itself is a Classic Asset payment from the distribution account, executed by the backend in the same logical operation and reconciled against the event.
- **Forward pricing (next-NAV settlement):** the contract only finalizes against a NAV whose timestamp is **strictly later** than the request timestamp — the standard cut-off rule of traditional fund administration, enforced on-chain. Any NAV visible at request time is therefore indicative only; settlement always occurs at the next published NAV. This structurally eliminates stale-price arbitrage (subscribing at a morning NAV after the market has moved intraday, at the expense of existing holders), and combined with intraday publication cadence it bounds the investor's market-drift exposure between request and settlement to the publication interval.
- **Redeem:** symmetric — tokens are returned to the distribution account, `redeem` records intent, exposure is unwound, `finalize_redemption` pays out USDC from the contract at oracle NAV.
- **Refund path:** if a pending operation is not finalized within a timeout, the depositor can call `refund()` and recover their USDC without any Trakx intervention — investor funds are never stranded on operational failure.
- **Authorization:** operator functions gated with `require_auth()` on the operator address (no custom auth logic); depositor functions gated on the depositor's own address.

### 4.3 Implementation practices

Unit tests in Rust for every contract function; integration tests against Stellar testnet; persistent vs. temporary storage split to manage state rent; events for every state transition so off-chain services subscribe rather than poll. Before mainnet, both contracts go through a structured internal security review — threat modeling, invariant checks and property-based testing of state transitions, with authorization paths and TTL/state-rent handling reviewed explicitly; no contract reaches mainnet before critical and high findings are remediated.

## 5. Investor onboarding & whitelist flow

KYC data collection follows **SEP-12**: the investor portal (or any SEP-12-compliant Stellar wallet) POSTs structured fields — name, address, country, income bracket, professional area — to a `/customer` endpoint on the Trakx backend. The identity verification step (document upload, photo, AML check) remains a hosted redirect to the existing provider (Shufti Pro / Sumsub): the backend generates the verification link, the user completes it on the provider's interface, and the backend receives the result via webhook. **No KYC data is written on-chain.** The only on-chain outcome is a `SetTrustLineFlags` operation authorizing the trustline once the investor passes all checks.

```mermaid
sequenceDiagram
    participant I as Investor
    participant P as Portal (SEP-12 client)
    participant K as Trakx KYC Backend
    participant V as Identity Provider (Shufti / Sumsub)
    participant C as Compliance Service
    participant S as Stellar

    I->>P: Connect wallet (Freighter / xBull)
    I->>P: Fill personal info & compliance forms
    P->>K: POST /customer (SEP-12 — structured fields only)
    K-->>P: Return hosted identity verification link
    P->>I: Redirect to identity provider
    I->>V: Upload ID / passport (hosted flow — Trakx never touches documents)
    V-->>K: Webhook — KYC approved + AML clear
    K-->>C: Investor approved (event)
    I->>S: Open trustline to EDO asset
    Note over S: Trustline exists but is NOT authorized yet
    C->>S: SetTrustLineFlags (authorize)
    S-->>I: Trustline authorized — investor can hold EDO
    Note over C,S: Revocation: C can clear the flag at any time (AUTH_REVOCABLE)
```

## 6. Mint flow (subscription, execute-then-mint)

```mermaid
sequenceDiagram
    participant I as Investor (whitelisted)
    participant ESC as SubscriptionEscrow (Soroban)
    participant ORC as NAVOracle (Soroban)
    participant M as Mint/Redeem Engine
    participant L as Internal Ledger
    participant E as Exchange (strategy)
    participant T as Treasury (CCTP)

    I->>ESC: subscribe(amount) — USDC into escrow via SAC
    ESC-->>M: SubscriptionRequested event
    M->>E: Adjust strategy exposure (execute first)
    E-->>M: Execution confirmed
    M->>L: Record mint (write-ahead: supply += q)
    M->>ESC: finalize_subscription(investor)
    ESC->>ORC: read NAV
    ESC-->>M: SubscriptionSettled(usdc, q, nav) — USDC released to treasury path
    M->>I: Issue q EDOT (Classic Asset payment from distribution)
    par Internal treasury leg (async, non-blocking)
        T->>T: CCTP burn on Stellar → mint native USDC on EVM venue
    end
    Note over ESC,I: If not finalized within timeout, investor calls refund() and recovers USDC
    Note over L: Reconciliation later verifies chain state == ledger
```

## 7. Redeem flow

```mermaid
sequenceDiagram
    participant I as Investor
    participant ESC as SubscriptionEscrow (Soroban)
    participant ORC as NAVOracle (Soroban)
    participant M as Mint/Redeem Engine
    participant L as Internal Ledger
    participant E as Exchange (strategy)
    participant T as Treasury (CCTP)

    I->>ESC: redeem(q) — tokens into escrow via SAC transfer (SEP-41), intent recorded with ledger timestamp
    ESC-->>M: RedemptionRequested event
    M->>E: Unwind proportional exposure
    E-->>M: Execution confirmed
    opt Treasury leg (if Stellar-side USDC buffer is insufficient)
        T->>T: CCTP burn on EVM venue → mint native USDC on Stellar
    end
    M->>L: Record redeem (write-ahead: supply -= q)
    M->>ESC: finalize_redemption(investor)
    ESC->>ORC: read NAV (published after request — forward pricing)
    ESC->>I: USDC payout at oracle NAV
    ESC->>DIST: tokens forwarded to distribution account via SAC
```

**Redemption token path (explicit).** Tokens move **investor → escrow contract** via a SAC transfer under the SEP-41 interface, authorized by the investor in the same invocation that records the redemption intent. The escrow's contract address (`C…`) holds EDO tokens transiently between request and settlement; for that, it is authorized by the SAC admin (the issuer) through **`set_authorized`** on the Stellar Asset Contract — the contract-address counterpart of `SetTrustLineFlags`, which applies only to `G…` accounts. On finalize, the escrow pays USDC to the investor at the oracle NAV (forward-priced, symmetric with subscriptions) and forwards the tokens to the distribution account, where they return to inventory; supply reduction, when required, is a subsequent `burn` executed from distribution. This path keeps redemption trustless for the investor — tokens and USDC change hands atomically inside the settlement — at the cost of one explicit issuer authorization for the escrow contract, executed once at deployment.

## 8. Treasury operations (Stellar ↔ EVM via CCTP)

The strategy executes on EVM-based exchange infrastructure, while investors settle exclusively in native USDC on Stellar. Trakx bridges the two internally using Circle's Cross-Chain Transfer Protocol (CCTP), which burns native USDC on the source chain and mints native USDC on the destination chain — no wrapped assets, no third-party bridges.

- **Subscription direction:** USDC released by the escrow is moved to the execution venue as needed to fund exposure. This runs asynchronously; the execute-then-mint ordering is preserved because exposure adjustment (the risk-bearing step) always precedes token issuance.
- **Redemption direction:** USDC is moved back to Stellar to fund payouts. A working USDC buffer is maintained on the Stellar side so that routine redemptions pay out immediately without waiting for a CCTP transfer.
- **Isolation:** the treasury leg is invisible to investors and never a dependency of the investor-facing flow. A delayed CCTP transfer affects internal buffer levels, not investor settlement.
- **Reconciliation:** treasury movements are recorded in the internal ledger and reconciled against both chains, alongside the supply × NAV ≤ collateral invariant.

## 9. Fee model

Fees are computed off-chain by the NAV engine and reflected in the published NAV; collection happens on-chain through token issuance. Rates and splits are per-product configuration parameters — the rails stay strategy-agnostic.

- **Management fee:** accrued daily in the NAV engine. The NAV published to the oracle is always **net of accrued fees**.
- **Performance fee:** accrued continuously on profits above a **fund-level high-water mark (HWM)**: whenever gross NAV exceeds the HWM, the accrual grows and the published net NAV reflects it. Below the HWM, no performance fee accrues and the HWM does not decrease.
- **Monthly crystallization:** at month-end, the accrued performance fee is collected by **minting new strategy tokens to dedicated fee recipient wallets** (share dilution — no assets leave the strategy). The HWM resets to the crystallization NAV and the accrual resets to zero. The fee mint is a Classic Asset payment from the distribution account, recorded in the internal ledger before issuance like any other supply event.
- **Recipients:** fee tokens are split between Trakx and the strategy manager at contractually configured percentages, paid to dedicated recipient wallets. Recipient wallets hold authorized trustlines under the same protocol-level whitelist as any other holder (`AUTH_REQUIRED` applies universally).
- **Reconciliation:** fee mints are first-class supply events covered by the invariants in the reconciliation section. Dilution changes token count, not collateral, so `supply × NAV ≤ collateral` is preserved by construction — NAV per token adjusts.
- **Roadmap note:** the fund-level HWM is the phase-1 model (simple, battle-tested). A per-token HWM variant (HWM travels with each token, dual sale/month-end triggers, Gross-vs-Net NAV entry) is specified internally and may be evaluated for a later phase if greater precision is required.

## 10. Reconciliation

- **Real-time:** Soroban RPC event streams for both **Soroban contract events** (`NAVUpdated`, `SubscriptionRequested/Settled`, `RedemptionRequested`) and **Classic operations** (payments and trustline changes on the issuer, distribution and escrow-related accounts). The team is aware that Horizon is being phased out; the stack standardizes on Soroban RPC, which now serves both Soroban events and Classic operations through a single interface.
- **Sweep:** cursor-based periodic sweep over Soroban RPC as a safety net for anything the streams miss (collector + reconciler pattern), with **Hubble** — Stellar's public analytics dataset — as the backfill source for history beyond the RPC retention window.
- **Invariants checked:** circulating supply (chain) == internal ledger supply; supply × NAV ≤ collateral held in the strategy infrastructure; escrow USDC balance == sum of pending intents.
- Discrepancies never auto-correct money paths; they raise alerts for operator review.

## 11. Trust boundaries & failure modes

| Boundary | Risk | Mitigation |
|---|---|---|
| Issuer keys | Compromise = unauthorized issuance | Cold storage, multisig, minimal signing surface |
| NAVOracle publication | Wrong/stale NAV misprices settlement | `require_auth()` on publisher, monotonic timestamps, on-chain deviation bound (circuit breaker) with admin override, `nav_age()` staleness pause, constituent-level sanity checks off-chain before signing |
| Stale-price arbitrage | Subscribing at an outdated NAV after intraday market moves, at existing holders' expense | Forward pricing enforced on-chain: settlement only at a NAV published after the request timestamp; intraday publication cadence bounds the drift window |
| SubscriptionEscrow | Contract bug affecting custodied USDC | Small bounded surface, Rust unit + testnet integration tests, structured security review (invariant + property-based testing) before mainnet, refund path guarantees depositor recovery |
| RPC event streams | Missed events | Write-ahead ledger (streams only verify), cursor sweep + Hubble backfill |
| Exchange execution | Slippage between quote and fill | Execute-then-mint ordering; Phase 3 slippage buffer on sizing |
| CCTP treasury leg | Delayed/failed cross-chain transfer | Internal-only operation (never blocks investor settlement); Stellar-side USDC buffer for routine redemptions; transfers reconciled in the internal ledger |
| Investor wallet | Lost keys | `AUTH_CLAWBACK_ENABLED` (set at issuance, before any trustline) lets the issuer claw back and reissue tokens under a documented recovery procedure |

## 12. Delivery phases (mapped to SCF tranches)

| Phase | Deliverables | Stellar surface |
|---|---|---|
| **1 — MVP** | Token issuance on testnet (flags, multisig); compliance & whitelist service; **NAVOracle contract live on testnet** (SEP-40-compatible, signed publication, on-chain deviation bound, events) | `SetOptions`, `ChangeTrust`, `SetTrustLineFlags`, `Payment`; Soroban: contract deploy, `require_auth`, events, state rent |
| **2 — Testnet** | **SubscriptionEscrow contract** + automated mint/redeem with next-NAV (forward-priced) settlement; investor portal (Stellar Wallets Kit: Freighter, xBull); reconciliation & reporting incl. contract events | SAC token transfers, cross-contract reads, Soroban RPC event streams (Soroban + Classic) |
| **3 — Mainnet** | Fee engine (management fee + performance fee with fund-level HWM, monthly crystallization via share dilution) & execution-risk framework; security review & remediation; mainnet launch with escrow-based mint/redeem; production hardening & monitoring | Mainnet ops, contract deploys, runbooks |

## 13. Stack

- **Contracts:** Rust / Soroban SDK; soroban-rpc for simulation and submission
- **Backend:** C#/.NET microservices (CQRS), .NET Stellar SDK, Soroban RPC (Hubble for historical backfill)
- **Frontend:** React/TypeScript, [Stellar Wallets Kit](https://stellarwalletskit.dev/)
- **Infra:** AWS EKS, PostgreSQL (internal ledger + reconciliation snapshots), existing Trakx observability stack (alerts on mint/redeem failures, NAV freshness, escrow balance, reconciliation breaks)
