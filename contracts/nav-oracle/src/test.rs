extern crate std;

use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
    Address, Env, Event, IntoVal, Vec,
};

use crate::{
    Asset, Error, FeedDefinition, NavOracle, NavOracleClient, NavRecord, NavUpdated, OracleConfig,
    PriceData,
};

/// NAV is published with 14 decimals, the precision convention of Soroban price
/// feeds. `ONE` is a NAV of 1.0 USDC per token.
const ONE: i128 = 100_000_000_000_000;
/// Hourly ticks: a daily official NAV at minimum, intraday publication when the
/// product calls for it.
const RESOLUTION: u32 = 3_600;
const DECIMALS: u32 = 14;
const START: u64 = 1_700_000_000;
/// `START` trimmed to the hourly tick.
const START_TICK: u64 = 1_699_999_200;

const HOUR: u64 = 3_600;

fn feed() -> FeedDefinition {
    FeedDefinition {
        base: Asset::Other(symbol_short!("USDC")),
        quote: Asset::Other(symbol_short!("EDO")),
        decimals: DECIMALS,
        resolution: RESOLUTION,
    }
}

fn config() -> OracleConfig {
    OracleConfig {
        staleness_threshold: 26 * HOUR,
        max_deviation_bps: 1_000, // 10%
        min_submission_interval: 5 * 60,
        history_size: 5,
        upgrade_timelock: 48 * HOUR,
    }
}

struct Setup {
    env: Env,
    contract_id: Address,
    admin: Address,
    co_admin: Address,
    publisher: Address,
    base: Asset,
    quote: Asset,
}

impl Setup {
    fn new() -> Self {
        Self::with_config(config())
    }

    fn with_config(config: OracleConfig) -> Self {
        let env = Env::default();
        env.ledger().set_timestamp(START);

        let admin = Address::generate(&env);
        let co_admin = Address::generate(&env);
        let publisher = Address::generate(&env);
        let base = feed().base;
        let quote = feed().quote;

        let contract_id = env.register(
            NavOracle,
            (
                admin.clone(),
                co_admin.clone(),
                publisher.clone(),
                feed(),
                config,
            ),
        );

        Setup {
            env,
            contract_id,
            admin,
            co_admin,
            publisher,
            base,
            quote,
        }
    }

    fn client(&self) -> NavOracleClient<'_> {
        NavOracleClient::new(&self.env, &self.contract_id)
    }

    fn advance(&self, seconds: u64) {
        let now = self.env.ledger().timestamp();
        self.env.ledger().set_timestamp(now + seconds);
    }

    fn now(&self) -> u64 {
        self.env.ledger().timestamp()
    }

    fn event_count(&self) -> usize {
        self.env.events().all().events().len()
    }

    /// A `submit_nav` authorized by exactly one address, so that authorization
    /// itself is under test rather than mocked away.
    fn submit_signed_by(&self, signer: &Address, price: i128, timestamp: u64) -> bool {
        let client = self.client();
        client
            .mock_auths(&[MockAuth {
                address: signer,
                invoke: &MockAuthInvoke {
                    contract: &self.contract_id,
                    fn_name: "submit_nav",
                    args: (price, timestamp).into_val(&self.env),
                    sub_invokes: &[],
                },
            }])
            .try_submit_nav(&price, &timestamp)
            .is_ok()
    }
}

// --- Construction ----------------------------------------------------------

#[test]
fn constructor_stores_feed_definition_and_config() {
    let setup = Setup::new();
    let client = setup.client();

    assert_eq!(client.base(), setup.base);
    assert_eq!(
        client.assets(),
        Vec::from_array(&setup.env, [setup.quote.clone()])
    );
    assert_eq!(client.decimals(), DECIMALS);
    assert_eq!(client.resolution(), RESOLUTION);
    assert_eq!(client.feed(), feed());
    assert_eq!(client.config(), config());
    assert_eq!(client.admin(), setup.admin);
    assert_eq!(client.co_admin(), setup.co_admin);
    assert_eq!(client.publisher(), setup.publisher);
}

#[test]
fn unpublished_feed_reports_stale_and_serves_no_price() {
    let setup = Setup::new();
    let client = setup.client();

    assert_eq!(client.latest_nav(), None);
    assert_eq!(client.lastprice(&setup.quote), None);
    assert_eq!(client.prices(&setup.quote, &5), None);
    // Fails safe: no NAV is treated as stale, so consumers pause rather than
    // transacting against an absent price.
    assert!(client.is_stale());
    assert_eq!(client.try_nav_age(), Err(Ok(Error::NoPriceAvailable)));
}

#[test]
fn constructor_rejects_invalid_config() {
    let mut invalid = config();
    invalid.history_size = 0;

    let env = Env::default();
    let (admin, co_admin, publisher) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.register(NavOracle, (admin, co_admin, publisher, feed(), invalid))
    }));
    assert!(result.is_err());
}

#[test]
fn constructor_rejects_an_out_of_range_nav_scale() {
    // The scale is immutable after deployment — SEP-40 forbids changing it —
    // so a fat-fingered value can only be caught here.
    let env = Env::default();
    for decimals in [0u32, 5, 19, 77] {
        let (admin, co_admin, publisher) = (
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        );
        let feed = FeedDefinition { decimals, ..feed() };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.register(NavOracle, (admin, co_admin, publisher, feed, config()))
        }));
        assert!(result.is_err());
    }
}

#[test]
fn constructor_rejects_an_address_holding_two_roles() {
    // `require_auth` twice on one address is satisfied by a single signature,
    // so a shared address would collapse the 2-of-2 with nothing to show for it.
    let env = Env::default();
    let shared = Address::generate(&env);
    let other = Address::generate(&env);

    for roles in [
        (shared.clone(), shared.clone(), other.clone()),
        (shared.clone(), other.clone(), shared.clone()),
        (other.clone(), shared.clone(), shared.clone()),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            env.register(NavOracle, (roles.0, roles.1, roles.2, feed(), config()))
        }));
        assert!(result.is_err());
    }
}

// --- Publication -----------------------------------------------------------

#[test]
fn submit_nav_stores_the_record_and_emits_the_event() {
    let setup = Setup::new();
    let client = setup.client();
    client.mock_all_auths().submit_nav(&(100 * ONE), &START);

    // Read before any further invocation: the test environment reports the
    // events of the most recent one.
    assert_eq!(
        setup.env.events().all(),
        std::vec![NavUpdated {
            asset: setup.quote.clone(),
            price: 100 * ONE,
            timestamp: START_TICK,
            published_at: START,
            overridden: false,
        }
        .to_xdr(&setup.env, &setup.contract_id)]
    );

    // The stored tick is trimmed to the resolution, per SEP-40 ...
    assert_eq!(
        client.lastprice(&setup.quote),
        Some(PriceData {
            price: 100 * ONE,
            timestamp: START_TICK,
        })
    );
    // ... while `published_at` keeps the real acceptance time, which is what
    // forward pricing and staleness are measured against.
    assert_eq!(
        client.latest_nav(),
        Some(NavRecord {
            price: 100 * ONE,
            timestamp: START_TICK,
            published_at: START,
        })
    );
    assert_eq!(client.nav_age(), 0);
    assert!(!client.is_stale());
}

#[test]
fn submit_nav_requires_the_authorized_publisher() {
    let setup = Setup::new();
    let stranger = Address::generate(&setup.env);

    assert!(!setup.submit_signed_by(&stranger, 100 * ONE, START));
    assert!(!setup.submit_signed_by(&setup.admin, 100 * ONE, START));
    assert_eq!(setup.client().latest_nav(), None);

    assert!(setup.submit_signed_by(&setup.publisher, 100 * ONE, START));
}

#[test]
fn rejects_non_positive_nav() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();

    assert_eq!(
        client.try_submit_nav(&0, &START),
        Err(Ok(Error::InvalidPrice))
    );
    assert_eq!(
        client.try_submit_nav(&-ONE, &START),
        Err(Ok(Error::InvalidPrice))
    );
}

#[test]
fn rejects_timestamp_ahead_of_the_ledger() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();

    assert_eq!(
        client.try_submit_nav(&(100 * ONE), &(START + 1)),
        Err(Ok(Error::TimestampInFuture))
    );
}

#[test]
fn rejects_ticks_that_are_not_strictly_later() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    setup.advance(HOUR);
    // Same hour bucket as the accepted record: rejected even though the raw
    // timestamp is later.
    assert_eq!(
        client.try_submit_nav(&(101 * ONE), &(START + 100)),
        Err(Ok(Error::TimestampNotMonotonic))
    );
    // An earlier tick is rejected too — replays cannot rewrite the feed.
    assert_eq!(
        client.try_submit_nav(&(101 * ONE), &(START - 2 * HOUR)),
        Err(Ok(Error::TimestampNotMonotonic))
    );

    client.submit_nav(&(101 * ONE), &setup.now());
    assert_eq!(client.latest_nav().unwrap().price, 101 * ONE);
}

#[test]
fn rejects_a_valuation_older_than_the_staleness_window() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    setup.advance(10 * 24 * HOUR);

    // A skewed clock or a replayed payload must not be able to publish an
    // ancient valuation: `published_at` would be now, so the feed would report
    // itself fresh while serving it.
    let ancient = setup.now() - config().staleness_threshold - 1;
    assert_eq!(
        client.try_submit_nav(&(100 * ONE), &ancient),
        Err(Ok(Error::TimestampTooOld))
    );

    // Just inside the window is fine.
    let recent = setup.now() - config().staleness_threshold;
    client.submit_nav(&(100 * ONE), &recent);
    assert_eq!(client.latest_nav().unwrap().price, 100 * ONE);
}

#[test]
fn rate_limits_submissions_to_the_minimum_interval() {
    // A product publishing at most every two hours, on an hourly tick: the rate
    // limit has to bind independently of tick availability.
    let setup = Setup::with_config(OracleConfig {
        min_submission_interval: 2 * HOUR,
        ..config()
    });
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    // A new tick is available, but not enough wall-clock time has passed.
    setup.advance(HOUR);
    assert_eq!(
        client.try_submit_nav(&(101 * ONE), &setup.now()),
        Err(Ok(Error::SubmissionTooSoon))
    );

    setup.advance(HOUR);
    client.submit_nav(&(101 * ONE), &setup.now());
    assert_eq!(client.latest_nav().unwrap().price, 101 * ONE);
}

// --- Circuit breaker -------------------------------------------------------

#[test]
fn deviation_within_the_bound_is_accepted() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    setup.advance(HOUR);
    // +9.99%, inside the 10% bound.
    client.submit_nav(&(10_999 * ONE / 100), &setup.now());
    assert_eq!(client.latest_nav().unwrap().price, 10_999 * ONE / 100);
}

#[test]
fn deviation_beyond_the_bound_is_rejected_leaving_state_untouched() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    let before = client.latest_nav();

    setup.advance(HOUR);
    // +10.01% up, and the symmetric move down: both breach the bound.
    assert_eq!(
        client.try_submit_nav(&(11_001 * ONE / 100), &setup.now()),
        Err(Ok(Error::DeviationExceeded))
    );
    // Nothing was emitted by the rejected invocation — a rejection is silent
    // on-chain, and surfaces to operators as a failed transaction.
    assert_eq!(setup.event_count(), 0);

    assert_eq!(
        client.try_submit_nav(&(89 * ONE), &setup.now()),
        Err(Ok(Error::DeviationExceeded))
    );
    assert_eq!(setup.event_count(), 0);

    // The contract fails safe: the last valid price still stands and consumers
    // keep reading it while `nav_age` grows towards the staleness pause.
    assert_eq!(client.latest_nav(), before);
    assert_eq!(client.prices(&setup.quote, &5).unwrap().len(), 1);

    // The same call within the bound does emit — the assertion above is about
    // the rejection, not about events being unobservable in this test.
    client.submit_nav(&(105 * ONE), &setup.now());
    assert_eq!(setup.event_count(), 1);
}

#[test]
fn override_admits_an_extreme_move_but_needs_both_signatures() {
    let setup = Setup::new();
    let client = setup.client();
    client.mock_all_auths().submit_nav(&(100 * ONE), &START);
    setup.advance(HOUR);

    let extreme = 130 * ONE;
    let now = setup.now();

    // The admin alone cannot override: the second, independent signature is
    // what separates an override from a compromised admin key.
    let admin_only = client
        .mock_auths(&[MockAuth {
            address: &setup.admin,
            invoke: &MockAuthInvoke {
                contract: &setup.contract_id,
                fn_name: "submit_nav_override",
                args: (extreme, now).into_val(&setup.env),
                sub_invokes: &[],
            },
        }])
        .try_submit_nav_override(&extreme, &now);
    assert!(admin_only.is_err());
    assert_eq!(client.latest_nav().unwrap().price, 100 * ONE);

    client.mock_all_auths().submit_nav_override(&extreme, &now);
    // The event carries `overridden: true`, so the override is auditable on
    // the ledger and not indistinguishable from a routine publication.
    assert_eq!(
        setup.env.events().all(),
        std::vec![NavUpdated {
            asset: setup.quote.clone(),
            price: extreme,
            timestamp: now - (now % RESOLUTION as u64),
            published_at: now,
            overridden: true,
        }
        .to_xdr(&setup.env, &setup.contract_id)]
    );
    assert_eq!(client.latest_nav().unwrap().price, extreme);
}

#[test]
fn override_is_not_blocked_by_the_rate_limit() {
    // The sequence the circuit breaker actually produces: a legitimate extreme
    // move is rejected, an operator alert fires, and the override follows
    // immediately — well inside the minimum interval, since the rejection never
    // advanced the last publication time.
    let setup = Setup::with_config(OracleConfig {
        min_submission_interval: 2 * HOUR,
        ..config()
    });
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    setup.advance(HOUR);
    assert_eq!(
        client.try_submit_nav(&(130 * ONE), &setup.now()),
        Err(Ok(Error::SubmissionTooSoon))
    );

    client.submit_nav_override(&(130 * ONE), &setup.now());
    assert_eq!(client.latest_nav().unwrap().price, 130 * ONE);
}

#[test]
fn override_bypasses_only_the_deviation_bound_and_the_rate_limit() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);
    setup.advance(HOUR);

    // Monotonicity and the sign check are not part of the override's remit.
    assert_eq!(
        client.try_submit_nav_override(&(130 * ONE), &START),
        Err(Ok(Error::TimestampNotMonotonic))
    );
    assert_eq!(
        client.try_submit_nav_override(&0, &setup.now()),
        Err(Ok(Error::InvalidPrice))
    );
}

// --- Staleness -------------------------------------------------------------

#[test]
fn nav_age_grows_and_crosses_the_staleness_threshold() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    setup.advance(config().staleness_threshold);
    assert_eq!(client.nav_age(), config().staleness_threshold);
    assert!(!client.is_stale());

    setup.advance(1);
    assert!(client.is_stale());

    // A fresh publication clears the pause.
    client.submit_nav(&(101 * ONE), &setup.now());
    assert_eq!(client.nav_age(), 0);
    assert!(!client.is_stale());
}

// --- History ---------------------------------------------------------------

#[test]
fn history_is_a_bounded_ring_buffer_ordered_newest_first() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();

    let mut ticks = std::vec::Vec::new();
    for i in 0..7i128 {
        if i > 0 {
            setup.advance(HOUR);
        }
        let now = setup.now();
        client.submit_nav(&((100 + i) * ONE), &now);
        ticks.push(now - (now % RESOLUTION as u64));
    }

    let prices = client.prices(&setup.quote, &10).unwrap();
    assert_eq!(prices.len(), config().history_size);
    // Newest first, and capped at the configured capacity.
    assert_eq!(prices.get_unchecked(0).price, 106 * ONE);
    assert_eq!(prices.get_unchecked(4).price, 102 * ONE);

    assert_eq!(client.prices(&setup.quote, &2).unwrap().len(), 2);

    // Retained ticks are addressable; evicted ones are gone.
    assert_eq!(
        client.price(&setup.quote, &ticks[2]),
        Some(PriceData {
            price: 102 * ONE,
            timestamp: ticks[2],
        })
    );
    assert_eq!(client.price(&setup.quote, &ticks[0]), None);
}

#[test]
fn price_lookup_trims_the_requested_timestamp_to_the_tick() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    // Any instant inside the bucket resolves to the bucket's record.
    assert_eq!(
        client.price(&setup.quote, &(START_TICK + RESOLUTION as u64 - 1)),
        Some(PriceData {
            price: 100 * ONE,
            timestamp: START_TICK,
        })
    );
    assert_eq!(client.price(&setup.quote, &(START_TICK - 1)), None);
}

#[test]
fn price_returns_none_for_a_tick_the_publisher_skipped() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    // The routine case for a daily NAV on an hourly tick: most ticks carry no
    // record, and SEP-40 requires `None` rather than the nearest neighbour.
    setup.advance(6 * HOUR);
    client.submit_nav(&(101 * ONE), &setup.now());

    assert_eq!(client.price(&setup.quote, &(START_TICK + 3 * HOUR)), None);
}

#[test]
fn unknown_asset_returns_none_rather_than_an_error() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    client.submit_nav(&(100 * ONE), &START);

    // SEP-40 requires the feed to delegate handling to the consumer.
    let unknown = Asset::Other(symbol_short!("BTC"));
    assert_eq!(client.lastprice(&unknown), None);
    assert_eq!(client.price(&unknown, &START_TICK), None);
    assert_eq!(client.prices(&unknown, &5), None);
    // Zero records is a degenerate request, not an error either.
    assert_eq!(client.prices(&setup.quote, &0), None);
}

// --- Administration --------------------------------------------------------

#[test]
fn set_config_requires_governance_and_applies_only_to_later_submissions() {
    let setup = Setup::new();
    let client = setup.client();
    client.mock_all_auths().submit_nav(&(100 * ONE), &START);
    setup.advance(HOUR);

    let mut widened = config();
    widened.max_deviation_bps = 5_000; // 50%

    let stranger = Address::generate(&setup.env);
    let unauthorized = client
        .mock_auths(&[MockAuth {
            address: &stranger,
            invoke: &MockAuthInvoke {
                contract: &setup.contract_id,
                fn_name: "set_config",
                args: (widened.clone(),).into_val(&setup.env),
                sub_invokes: &[],
            },
        }])
        .try_set_config(&widened);
    assert!(unauthorized.is_err());

    let client = client.mock_all_auths();
    // Before the change, the move breaches the bound.
    assert_eq!(
        client.try_submit_nav(&(130 * ONE), &setup.now()),
        Err(Ok(Error::DeviationExceeded))
    );

    client.set_config(&widened);
    assert_eq!(client.config(), widened);
    client.submit_nav(&(130 * ONE), &setup.now());
    assert_eq!(client.latest_nav().unwrap().price, 130 * ONE);
}

/// Invokes `$method` authorized by `$signer` alone, so that the second
/// governance signature is genuinely absent rather than mocked away.
macro_rules! signed_by_one {
    ($setup:expr, $signer:expr, $fn_name:literal, ($($arg:expr),+ $(,)?), $method:ident) => {{
        let setup = &$setup;
        setup
            .client()
            .mock_auths(&[MockAuth {
                address: $signer,
                invoke: &MockAuthInvoke {
                    contract: &setup.contract_id,
                    fn_name: $fn_name,
                    args: ($($arg.clone()),+,).into_val(&setup.env),
                    sub_invokes: &[],
                },
            }])
            .$method($(&$arg),+)
    }};
}

#[test]
fn a_single_governance_key_cannot_reach_a_price_the_breaker_would_reject() {
    let setup = Setup::new();
    let client = setup.client();
    client.mock_all_auths().submit_nav(&(100 * ONE), &START);
    setup.advance(HOUR);

    // The bypass this guards against: install your own publisher, or widen the
    // deviation bound, and then publish anything through `submit_nav` — reaching
    // the override's outcome without the second signature ever being given.
    let own_publisher = Address::generate(&setup.env);
    assert!(signed_by_one!(
        setup,
        &setup.admin,
        "set_publisher",
        (own_publisher),
        try_set_publisher
    )
    .is_err());

    let widened = OracleConfig {
        max_deviation_bps: u32::MAX,
        ..config()
    };
    assert!(signed_by_one!(setup, &setup.admin, "set_config", (widened), try_set_config).is_err());

    // The co-admin alone gets no further.
    assert!(signed_by_one!(
        setup,
        &setup.co_admin,
        "set_config",
        (widened),
        try_set_config
    )
    .is_err());

    assert_eq!(client.publisher(), setup.publisher);
    assert_eq!(client.config(), config());
    assert_eq!(
        client
            .mock_all_auths()
            .try_submit_nav(&(130 * ONE), &setup.now()),
        Err(Ok(Error::DeviationExceeded))
    );
}

#[test]
fn set_governance_rotates_both_keys_under_both_signatures() {
    let setup = Setup::new();
    let client = setup.client();
    let (new_admin, new_co_admin) = (Address::generate(&setup.env), Address::generate(&setup.env));

    assert!(signed_by_one!(
        setup,
        &setup.admin,
        "set_governance",
        (new_admin, new_co_admin),
        try_set_governance
    )
    .is_err());

    client
        .mock_all_auths()
        .set_governance(&new_admin, &new_co_admin);
    assert_eq!(client.admin(), new_admin);
    assert_eq!(client.co_admin(), new_co_admin);

    // Rotation cannot be used to smuggle in a shared address.
    assert_eq!(
        client
            .mock_all_auths()
            .try_set_governance(&new_admin, &new_admin),
        Err(Ok(Error::RolesNotDistinct))
    );
    assert_eq!(
        client.mock_all_auths().try_set_publisher(&new_admin),
        Err(Ok(Error::RolesNotDistinct))
    );
}

#[test]
fn set_config_rejects_values_that_would_disable_a_safeguard() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();

    for invalid in [
        OracleConfig {
            max_deviation_bps: 0,
            ..config()
        },
        OracleConfig {
            staleness_threshold: 0,
            ..config()
        },
        OracleConfig {
            history_size: 0,
            ..config()
        },
        // An unbounded history would make every publication rewrite a larger
        // vector, degrading and then breaking the write path.
        OracleConfig {
            history_size: 10_000,
            ..config()
        },
        // A zero timelock would let an upgrade be announced and applied in one
        // transaction, so holders never see the announcement.
        OracleConfig {
            upgrade_timelock: 0,
            ..config()
        },
        OracleConfig {
            upgrade_timelock: u64::MAX,
            ..config()
        },
    ] {
        assert_eq!(
            client.try_set_config(&invalid),
            Err(Ok(Error::InvalidConfig))
        );
    }
    assert_eq!(client.config(), config());
}

#[test]
fn set_publisher_rotates_the_publishing_key() {
    let setup = Setup::new();
    let client = setup.client();
    let rotated = Address::generate(&setup.env);

    client.mock_all_auths().set_publisher(&rotated);
    assert_eq!(client.publisher(), rotated);

    // The retired key can no longer move the price; the new one can.
    assert!(!setup.submit_signed_by(&setup.publisher, 100 * ONE, START));
    assert!(setup.submit_signed_by(&rotated, 100 * ONE, START));
}

// --- Upgrade ---------------------------------------------------------------

#[test]
fn upgrade_is_announced_on_chain_and_held_for_the_timelock() {
    let setup = Setup::new();
    let client = setup.client().mock_all_auths();
    let wasm_hash = soroban_sdk::BytesN::from_array(&setup.env, &[7u8; 32]);

    assert_eq!(client.pending_upgrade(), None);
    assert_eq!(
        client.try_apply_upgrade(),
        Err(Ok(Error::UpgradeNotScheduled))
    );

    client.schedule_upgrade(&wasm_hash);
    let pending = client.pending_upgrade().unwrap();
    assert_eq!(pending.wasm_hash, wasm_hash);
    assert_eq!(pending.available_at, START + config().upgrade_timelock);

    // No second announcement can quietly replace the first.
    assert_eq!(
        client.try_schedule_upgrade(&wasm_hash),
        Err(Ok(Error::UpgradeAlreadyScheduled))
    );

    setup.advance(config().upgrade_timelock - 1);
    assert_eq!(
        client.try_apply_upgrade(),
        Err(Ok(Error::UpgradeTimelockActive))
    );

    // Once the timelock elapses the gate opens: the call no longer stops at
    // `UpgradeTimelockActive`. It now fails deeper, in the host, because no wasm
    // with this hash is installed — exercising that last step needs a deployed
    // fixture and belongs with the testnet integration of Phase 1.
    setup.advance(1);
    assert_ne!(
        client.try_apply_upgrade(),
        Err(Ok(Error::UpgradeTimelockActive))
    );

    client.cancel_upgrade();
    assert_eq!(client.pending_upgrade(), None);
    assert_eq!(
        client.try_cancel_upgrade(),
        Err(Ok(Error::UpgradeNotScheduled))
    );
}

#[test]
fn upgrade_requires_both_governance_signatures() {
    let setup = Setup::new();
    let client = setup.client();
    let stranger = Address::generate(&setup.env);
    let wasm_hash = soroban_sdk::BytesN::from_array(&setup.env, &[7u8; 32]);

    // Neither an outsider nor one half of governance can announce an upgrade.
    for signer in [&stranger, &setup.admin, &setup.co_admin] {
        assert!(signed_by_one!(
            setup,
            signer,
            "schedule_upgrade",
            (wasm_hash),
            try_schedule_upgrade
        )
        .is_err());
    }
    assert_eq!(client.pending_upgrade(), None);
}

// --- State rent ------------------------------------------------------------

#[test]
fn extend_ttl_tolerates_the_window_before_the_first_publication() {
    // The maintenance job runs from deployment. The NAV entries do not exist
    // yet, and extending a missing entry traps — taking the instance entry it
    // was also meant to bump down with it.
    let setup = Setup::new();
    NavOracleClient::new(&setup.env, &setup.contract_id).extend_ttl();
}

#[test]
fn extend_ttl_is_permissionless_and_keeps_the_feed_readable() {
    let setup = Setup::new();
    let client = setup.client();
    client.mock_all_auths().submit_nav(&(100 * ONE), &START);

    // No auth mocked: rent maintenance must never depend on an operator being
    // available to sign.
    NavOracleClient::new(&setup.env, &setup.contract_id).extend_ttl();

    assert_eq!(client.latest_nav().unwrap().price, 100 * ONE);
}
