# NAVOracle

The authoritative on-chain NAV feed for a Trakx tokenized strategy product, and
the first Soroban deliverable of Phase 1 in
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) §12.

NAV is computed off-chain by the engine that prices Trakx's indices today, then
published here as a signed submission. This contract is the single on-chain
pricing reference for mint and redeem — and, through its SEP-40 interface, a
composable price feed any other Soroban protocol can read without a custom
integration.

## Running it

No local Rust or Stellar toolchain is required; the Docker image *is* the
toolchain.

```
make test     # 25 unit tests
make build    # release .wasm for wasm32v1-none
make lint     # clippy, warnings denied
make check    # fmt + lint + test
```

Built against `soroban-sdk` 27.0.6 (Rust 1.91).

## Interface

### SEP-40 — `PriceFeedTrait`

Implemented verbatim, so any SEP-40 consumer reads Trakx product NAVs with no
adapter.

| Function | Behaviour |
|---|---|
| `base()` | The asset NAV is denominated in — USDC |
| `assets()` | The single strategy token this feed prices |
| `decimals()` | Scale of the published NAV; fixed at deployment |
| `resolution()` | Tick length in seconds; fixed at deployment |
| `lastprice(asset)` | Most recent NAV, or `None` |
| `price(asset, timestamp)` | NAV at a tick, or `None` |
| `prices(asset, records)` | Last *n* NAVs, most recent first, or `None` |

Per SEP-40, an unknown asset or an unavailable tick returns `None` rather than
an error: handling is the consumer's to decide.

### Publication and operations

| Function | Auth | Purpose |
|---|---|---|
| `__constructor(admin, override_admin, publisher, feed, config)` | — | Runs once, atomically, in the deploy transaction |
| `submit_nav(price, timestamp)` | publisher | Publish a NAV, subject to every validation rule |
| `submit_nav_override(price, timestamp)` | admin **and** override admin | Admit a genuine extreme move past the deviation bound and the rate limit |
| `latest_nav()` | — | Latest record, including `published_at` |
| `nav_age()` / `is_stale()` | — | Staleness, for the consumer's pause decision |
| `set_config(config)` | admin | Retune risk parameters without a redeploy |
| `set_publisher(publisher)` | admin | Rotate the operational publishing key |
| `schedule_upgrade` / `apply_upgrade` / `cancel_upgrade` | admin | Governed upgrade behind a timelock |
| `extend_ttl()` | — | Permissionless state-rent maintenance |

## Specification traceability

Every requirement in `docs/ARCHITECTURE.md` §4.1 and §4.3, and where it is
implemented and covered.

| Requirement (§4.1 / §4.3) | Implementation | Test |
|---|---|---|
| SEP-40-compatible feed, composable by third parties | `impl PriceFeedTrait for NavOracle` | `constructor_stores_feed_definition_and_config`, `unknown_asset_returns_none_rather_than_an_error` |
| Only the authorized publisher may publish | `require_auth()` on `submit_nav` | `submit_nav_requires_the_authorized_publisher` |
| Monotonic timestamps | strict tick comparison in `record_nav` | `rejects_ticks_that_are_not_strictly_later` |
| Minimum-interval guard rate-limits submissions | `min_submission_interval` | `rate_limits_submissions_to_the_minimum_interval` |
| Circuit breaker: reject beyond `MAX_DEVIATION`, **state untouched, no event** | `max_deviation_bps` in `record_nav` | `deviation_beyond_the_bound_is_rejected_leaving_state_untouched` |
| Override path gated on a second, independent admin signature | `submit_nav_override` | `override_admits_an_extreme_move_but_needs_both_signatures`, `override_bypasses_only_the_deviation_bound_and_the_rate_limit`, `override_is_not_blocked_by_the_rate_limit` |
| `NAVUpdated` event on every accepted publication | `NavUpdated` (`#[contractevent]`) | `submit_nav_stores_the_record_and_emits_the_event` |
| `nav_age()` staleness threshold pauses dependent operations | `nav_age()`, `is_stale()` | `nav_age_grows_and_crosses_the_staleness_threshold` |
| NAV as scaled `i128` with an explicit scale factor | `FeedDefinition::decimals` | `constructor_stores_feed_definition_and_config` |
| Bounded ring buffer of historical entries | `DataKey::History`, capped at `history_size` | `history_is_a_bounded_ring_buffer_ordered_newest_first` |
| Risk parameters in storage, not in code; new intents only | `OracleConfig`, `set_config` | `set_config_requires_the_admin_and_applies_only_to_later_submissions` |
| `__constructor` — no front-running window between deploy and init | `__constructor` | `constructor_rejects_invalid_config` |
| Upgradeable under governance, behind a timelock, announced on-chain | `schedule_upgrade` / `apply_upgrade` | `upgrade_is_announced_on_chain_and_held_for_the_timelock`, `upgrade_requires_the_admin` |
| Persistent vs. instance storage split, TTL extended programmatically | `DataKey` split, `extend_ttl` | `extend_ttl_is_permissionless_and_keeps_the_feed_readable` |

## Design notes

**Two timestamps, deliberately.** SEP-40 defines a price point's timestamp as
`floor(t / resolution) * resolution`, and consumers address historical prices by
that tick. But §4.2's forward-pricing rule — settle only at a NAV published
*strictly after* the request — would break silently against a down-rounded tick:
a NAV published at 17:00 and trimmed to a daily bucket would appear to precede a
request made at 10:00. So `NavRecord` carries both: `timestamp` is the SEP-40
tick, and `published_at` is the ledger time of acceptance, which is what
staleness and forward pricing are measured against. `PriceData`, the SEP-40
type, exposes only the tick; `latest_nav()` exposes both.

**The feed definition is not configuration.** `decimals` and `resolution` must
never change once consumers depend on them, so they sit in `FeedDefinition`,
written once by the constructor. `OracleConfig` holds exactly what is meant to
be retuned per product — deviation bound, staleness threshold, rate limit,
history depth, upgrade timelock — through `set_config`, with no redeploy.

**Rejection is silent.** A submission breaching the deviation bound writes no
state and emits no event; it surfaces as a failed transaction that the publisher
turns into an operator alert. Meanwhile `nav_age()` keeps growing, and dependent
operations pause once the staleness threshold is crossed. The system fails safe
— paused at the last valid price — rather than settling at a wrong one.

**The override bypasses the rate limit too.** A rejected submission does not
advance the last publication time, so a feed stopped by the circuit breaker
would otherwise stay mispriced until the minimum interval elapsed — defeating
the purpose of an override, which exists to unblock it. The two independent
signatures are the spam control on that path; the rate limit protects the
single-key publisher path, which the override is not. A NAV so large that
scaling it to basis points would overflow is treated as a deviation breach
rather than a panic: `price` arrives unvalidated from the publisher, and a typed
rejection is the right answer to a nonsensical one.

**Staleness fails safe on an empty feed.** `is_stale()` returns `true` when no
NAV has ever been published, so a consumer that checks it cannot transact
against an absent price during the window between deployment and first
publication.

**`extend_ttl` is permissionless.** State-rent maintenance is driven by the
backend on a schedule, but nothing about it needs authority — and making it
callable by anyone means the feed cannot be archived through operator neglect.

## Not in this contract

Settlement — the escrow, the intent state machine, the two-phase claim and
finalize — is the SubscriptionEscrow of §4.2, the Phase 2 deliverable. This
contract's only relationship to it is the cross-contract NAV read at settlement.
