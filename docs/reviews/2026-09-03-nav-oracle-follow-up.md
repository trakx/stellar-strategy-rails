# Code review — follow-up on 742340c ("Require both governance signatures for every privileged action")

Second-pass review of the fixes for the 9 findings raised against `main...HEAD`
(`nav-oracle` Soroban contract). Verified by reading `lib.rs`, `types.rs`,
`test.rs`, `README.md` and the `docker/Dockerfile` at 742340c.

**Verdict: 7 fixed and verified, 1 partial, 1 correctly rejected. Two items to
close before merge, neither behavioural.**

---

## Per-finding verdicts

| # | Sev | Verdict | Evidence |
|---|---|---|---|
| 1 | HIGH | **Fixed** | `require_governance` (`lib.rs:490-493`) gates `set_config:185`, `set_publisher:203`, `set_governance:217`, `submit_nav_override:127`, `schedule_upgrade:233`, `cancel_upgrade:261`, `apply_upgrade:275`. `validate_roles` (`lib.rs:481-486`) enforced in the constructor and on both rotation paths. |
| 2 | HIGH | **Fixed** | `has()` guard on both persistent keys (`lib.rs:306-311`); regression test `extend_ttl_tolerates_the_window_before_the_first_publication` (`test.rs:855`). |
| 3 | MED | **Fixed** | `upgrade_timelock` clamped to [24h, 90d] (`lib.rs:49-50`, `validate_config:470-471`); deadline now `saturating_add` (`lib.rs:240-243`). |
| 4 | MED | **Fixed** | `validate_roles` in `__constructor` (`lib.rs:89`); test `constructor_rejects_an_address_holding_two_roles` (`test.rs:182`). |
| 5 | MED | **Fixed** | `TimestampTooOld` (`lib.rs:397-399`); test `rejects_a_valuation_older_than_the_staleness_window` (`test.rs:307`). |
| 6 | MED | **Withdrawn — the rejection is correct.** | See below. |
| 7 | LOW | **Fixed** | `MAX_HISTORY_SIZE = 100` (`lib.rs:51`), enforced in `validate_config:469`. |
| 8 | LOW | **Partial — accepted, but see action item A.** | Timelock gate opening asserted (`test.rs:817-821`). |
| 9 | LOW | **Fixed** | Explicit `rustup target add wasm32v1-none` + `rustup component add rustfmt clippy`. |

Test count confirmed: 30 test functions in `test.rs`, up from 25.

### Notes on individual fixes

**#1.** The fix goes further than the finding asked and is better for it.
`set_governance` closes a gap the original review did not raise: without it a key
known to be compromised could not act alone, but would remain one half of
governance permanently. Keeping `set_publisher` immediate rather than timelocked
is the right call and the rationale at `lib.rs:198-201` is the correct place for
it — the urgent case for rotation is a suspected key compromise, and a timelock
there works against the operator. The two signatures are the proportionate
control.

**#9.** The commit message reports that the two image variants were built and
compared, which is the verification the original finding asked for and could not
perform. Noted as resolved on that evidence.

---

## #6 — the rejection stands

The finding assumed an archived `Latest` would make `latest(env)` return `None`,
letting `record_nav` skip monotonicity, the rate limit and the deviation bound.
That is wrong, and the reasoning given in the reply is right. The sharpest form
of the argument is **footprint membership**:

- `record_nav` reads `DataKey::Latest` on every submission (`lib.rs:402`), so the
  key is in the transaction footprint.
- An archived *persistent* entry in the footprint fails the transaction. Control
  never reaches the `if let Some(last)` branch to fall through it.
- The described failure mode requires a read that returns `None`. Persistent
  storage has no such state; *temporary* storage does — which is exactly why the
  finding would have been correct had these entries been temporary.

Second, independent leg: `Latest`, `History` and the instance entry are all
extended in the same transaction with the same parameters (`lib.rs:428-432`), and
were the instance entry archived, `config(&env).unwrap()` (`lib.rs:514`) would
trap before any invariant was evaluated.

**No code change.** An `Initialized` flag would be unreachable code, read on every
submission and never able to be false; it also collides with the repository's own
"no error handling for impossible scenarios" rule. Reference for the
persistent-vs-temporary distinction: Soroban docs, "State Archival".

What is worth doing is action item B below — the finding was reached by a reader
who had the code in front of them, which is the evidence that the load-bearing
property is currently undocumented.

---

## Action items

### A. README is missing the `apply_upgrade` coverage gap — blocking

The commit message for 742340c states the gap was *"recorded as a known gap in
the README"*. It was not. The note exists only as a comment in
`test.rs:814-817`; `grep -i "gap\|testnet\|untested\|not covered"` over
`contracts/nav-oracle/README.md` returns nothing.

On a PR under review this is a commit message asserting something false about the
repository, so it should be closed by adding the note rather than by amending the
claim.

- **File:** `contracts/nav-oracle/README.md`, section `## Not in this contract`
  (line 153), alongside the existing Phase 2 pointer.
- **Content:** one paragraph — the timelock gate is asserted; the final host call
  needs a wasm installed in the ledger, which the in-memory test environment does
  not provide; closing it belongs with the Phase 1 testnet integration. Same
  framing the test comment already uses.
- **Verifiable outcome:** the commit message's claim becomes true.

### B. Record why the NAV entries are persistent, not temporary — recommended

The `DataKey` doc comment (`types.rs:94-99`) explains the instance/persistent
split in terms of durability and rent cycles, but not in terms of the safety
invariant. As #6 showed, that gap is load-bearing: a future change to temporary
storage, plausibly justified as a rent optimisation, would silently make the
rejected finding true.

- **File:** `contracts/nav-oracle/src/types.rs`, appended to the existing
  `DataKey` doc comment.
- **Content:** one sentence — persistent rather than temporary is deliberate,
  because an archived persistent entry fails the transaction instead of reading
  as absent; with temporary storage a submission after expiry would pass with no
  invariants applied.
- **Verifiable outcome:** the reason sits next to the decision that depends on it.

---

## Non-blocking observation

Governance (2-of-2) can widen `max_deviation_bps` via `set_config` and then
publish through the ordinary `submit_nav`. That path emits
`NavUpdated { overridden: false }` plus a `ConfigUpdated`, so an off-chain monitor
keying on `overridden == true` to detect a breaker bypass will not see it.

This is **not** an authorization hole — the claim at `lib.rs:14-16` is about
single keys and remains accurate — and it needs no code change. It is a
monitoring-surface note for whoever writes the alerting: correlate `ConfigUpdated`
with subsequent publications, do not rely on the `overridden` flag alone.

---

## Verification

Both action items are documentation. No behavioural change, so no new tests:
the existing 30 remain the correct coverage.

```
make check     # fmt + clippy (warnings denied) + 30 tests
```
