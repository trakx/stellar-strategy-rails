# Stellar Strategy Rails

Compliant tokenization rails for strategy-based investment products on Stellar, built by [Trakx](https://trakx.io/).

**First use case:** EDO Theory — a tokenized wrapper giving whitelisted investors USDC-based exposure to an automated long/short digital asset strategy, with on-chain NAV publication and NAV-based mint/redeem.

## What this is

Trakx is an AMF-registered crypto investment platform (France) operating Crypto Tradable Indices and tokenized investment products. This repository contains the technical architecture and, as development progresses, the implementation of Trakx's Stellar integration:

- **Restricted strategy token** — Stellar Classic Asset with `AUTH_REQUIRED` / `AUTH_REVOCABLE`: only whitelisted investors can hold it, with compliance enforced at protocol level (no custom smart contracts).
- **USDC mint/redeem at NAV** — investors subscribe and redeem in native USDC at the published NAV, using an execute-then-mint model (the underlying strategy exposure is adjusted before tokens are issued).
- **On-chain NAV publication** — daily NAV published as signed on-chain data for transparent, verifiable pricing.
- **Compliance service** — trustline authorization tied to Trakx's existing KYC stack.
- **Investor portal** — wallet-based subscription/redemption via [Stellar Wallets Kit](https://stellarwalletskit.dev/) (Freighter, xBull).

The rails are strategy-agnostic: once validated with EDO, additional tokenized products (e.g., real estate, carbon credits, art) can be onboarded on the same infrastructure.

## Documentation

- [Technical Architecture](docs/ARCHITECTURE.md) — components, account model, flows (onboarding, mint, redeem, NAV publication, reconciliation), trust boundaries, and delivery phases.

## Delivery phases

| Phase | Scope | Status |
|---|---|---|
| 1 — MVP | Token issuance (testnet), compliance & whitelist service, on-chain NAV publication | Planned |
| 2 — Testnet | Automated mint/redeem engine at NAV, investor portal, reconciliation & reporting | Planned |
| 3 — Mainnet | Performance fee & execution risk framework, mainnet launch, production hardening | Planned |

## About Trakx

Trakx provides the technology and legal umbrella for tokenized financial products: issuance, custody, execution, NAV tracking and investor access, distributed B2B2C to brokers and neobanks. Backed by ConsenSys, headquartered and regulated in France.

- Website: https://trakx.io/
- GitHub: https://github.com/trakx
