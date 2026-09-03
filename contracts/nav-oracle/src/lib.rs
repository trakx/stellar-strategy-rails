#![no_std]
//! # NAVOracle
//!
//! The authoritative on-chain NAV feed for a Trakx tokenized strategy product.
//!
//! NAV is computed off-chain by the engine that prices Trakx's indices today and
//! published here as a signed submission. This contract is the single on-chain
//! pricing reference for mint and redeem, and — through its SEP-40 interface — a
//! composable price feed any other Soroban protocol can read.
//!
//! The contract is deliberately small. Its safety properties are:
//!
//! - only the authorized publisher can move the price (`require_auth`);
//! - ticks are strictly monotonic and never ahead of the ledger;
//! - a submission deviating beyond the configured bound is **rejected**, leaving
//!   state untouched and emitting nothing — the system pauses at the last valid
//!   price rather than settling at a wrong one;
//! - genuine extreme moves pass through a separate path gated on a second,
//!   independent admin signature;
//! - `is_stale()` fails safe: it reports stale both when the NAV is old and when
//!   no NAV exists at all.
//!
//! See `docs/ARCHITECTURE.md` §4.1 for the specification this implements.

mod types;

pub use types::*;

use soroban_sdk::{contract, contractimpl, Address, BytesN, Env, Vec};

/// Ledgers in a day, at Stellar's ~5 second close time.
const DAY_IN_LEDGERS: u32 = 17_280;
/// TTL floor below which an entry is bumped, and the target it is bumped to.
/// Rent maintenance is also driven from the backend via `extend_ttl`.
const TTL_THRESHOLD: u32 = 30 * DAY_IN_LEDGERS;
const TTL_EXTEND_TO: u32 = 90 * DAY_IN_LEDGERS;

const BPS_DENOMINATOR: i128 = 10_000;

#[contract]
pub struct NavOracle;

/// SEP-40 price feed interface. Implemented verbatim so that any SEP-40 consumer
/// reads Trakx product NAVs with no custom integration.
pub trait PriceFeedTrait {
    fn base(env: Env) -> Asset;
    fn assets(env: Env) -> Vec<Asset>;
    fn decimals(env: Env) -> u32;
    fn resolution(env: Env) -> u32;
    fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData>;
    fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>>;
    fn lastprice(env: Env, asset: Asset) -> Option<PriceData>;
}

#[contractimpl]
impl NavOracle {
    /// Runs once, atomically, in the deploy transaction — there is no separate
    /// `initialize` call and therefore no window in which an unconfigured
    /// contract exists on-chain.
    ///
    /// `admin` and `override_admin` must be independent signers: together they
    /// gate the deviation-bound override.
    pub fn __constructor(
        env: Env,
        admin: Address,
        override_admin: Address,
        publisher: Address,
        feed: FeedDefinition,
        config: OracleConfig,
    ) -> Result<(), Error> {
        if feed.resolution == 0 {
            return Err(Error::InvalidConfig);
        }
        validate_config(&config)?;

        let storage = env.storage().instance();
        storage.set(&DataKey::Admin, &admin);
        storage.set(&DataKey::OverrideAdmin, &override_admin);
        storage.set(&DataKey::Publisher, &publisher);
        storage.set(&DataKey::Feed, &feed);
        storage.set(&DataKey::Config, &config);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        Ok(())
    }

    // --- Publication -------------------------------------------------------

    /// Publish a NAV. Gated on the authorized publisher, and subject to every
    /// validation rule including the deviation bound.
    ///
    /// `timestamp` is the effective time of the valuation, not the time of the
    /// call; it is trimmed to the feed's resolution before being stored.
    pub fn submit_nav(env: Env, price: i128, timestamp: u64) -> Result<(), Error> {
        publisher(&env).require_auth();
        record_nav(&env, price, timestamp, false)
    }

    /// Publish a NAV that legitimately breaches the deviation bound.
    ///
    /// Reserved for genuine extreme market moves. Requires the admin **and** the
    /// independent override admin to sign; every other validation rule — the
    /// rate limit, monotonicity, the future-timestamp check — still applies.
    pub fn submit_nav_override(env: Env, price: i128, timestamp: u64) -> Result<(), Error> {
        admin(&env).require_auth();
        override_admin(&env).require_auth();
        record_nav(&env, price, timestamp, true)
    }

    // --- Reads -------------------------------------------------------------

    /// The latest accepted record, including `published_at` for forward pricing.
    pub fn latest_nav(env: Env) -> Option<NavRecord> {
        latest(&env)
    }

    /// Seconds since the latest NAV was accepted.
    pub fn nav_age(env: Env) -> Result<u64, Error> {
        let latest = latest(&env).ok_or(Error::NoPriceAvailable)?;
        Ok(env.ledger().timestamp().saturating_sub(latest.published_at))
    }

    /// Whether dependent operations must pause. Fails safe: an unpublished feed
    /// is stale, not fresh.
    pub fn is_stale(env: Env) -> bool {
        match latest(&env) {
            None => true,
            Some(latest) => {
                let age = env.ledger().timestamp().saturating_sub(latest.published_at);
                age > config(&env).staleness_threshold
            }
        }
    }

    pub fn config(env: Env) -> OracleConfig {
        config(&env)
    }

    pub fn feed(env: Env) -> FeedDefinition {
        feed(&env)
    }

    pub fn admin(env: Env) -> Address {
        admin(&env)
    }

    pub fn override_admin(env: Env) -> Address {
        override_admin(&env)
    }

    pub fn publisher(env: Env) -> Address {
        publisher(&env)
    }

    pub fn pending_upgrade(env: Env) -> Option<PendingUpgrade> {
        env.storage().instance().get(&DataKey::PendingUpgrade)
    }

    // --- Administration ----------------------------------------------------

    /// Retune risk parameters without redeploying. Applies to subsequent
    /// submissions only; records already accepted are never revisited.
    pub fn set_config(env: Env, config: OracleConfig) -> Result<(), Error> {
        admin(&env).require_auth();
        validate_config(&config)?;
        let storage = env.storage().instance();
        storage.set(&DataKey::Config, &config);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        ConfigUpdated { config }.publish(&env);
        Ok(())
    }

    /// Rotate the publishing key. The NAV service's signing key is operational
    /// and rotates on a schedule; the feed identity does not.
    pub fn set_publisher(env: Env, publisher: Address) {
        admin(&env).require_auth();
        let storage = env.storage().instance();
        storage.set(&DataKey::Publisher, &publisher);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        PublisherUpdated { publisher }.publish(&env);
    }

    // --- Upgrade under governance -----------------------------------------

    /// Announce an upgrade. It becomes applicable only after the configured
    /// timelock, and the announcement is on-chain, so holders can observe — and
    /// if they wish, exit — before any change activates.
    pub fn schedule_upgrade(env: Env, wasm_hash: BytesN<32>) -> Result<(), Error> {
        admin(&env).require_auth();
        let storage = env.storage().instance();
        if storage.has(&DataKey::PendingUpgrade) {
            return Err(Error::UpgradeAlreadyScheduled);
        }
        let available_at = env.ledger().timestamp() + config(&env).upgrade_timelock;
        storage.set(
            &DataKey::PendingUpgrade,
            &PendingUpgrade {
                wasm_hash: wasm_hash.clone(),
                available_at,
            },
        );
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        UpgradeScheduled {
            wasm_hash,
            available_at,
        }
        .publish(&env);
        Ok(())
    }

    pub fn cancel_upgrade(env: Env) -> Result<(), Error> {
        admin(&env).require_auth();
        let storage = env.storage().instance();
        let pending: PendingUpgrade = storage
            .get(&DataKey::PendingUpgrade)
            .ok_or(Error::UpgradeNotScheduled)?;
        storage.remove(&DataKey::PendingUpgrade);
        UpgradeCancelled {
            wasm_hash: pending.wasm_hash,
        }
        .publish(&env);
        Ok(())
    }

    pub fn apply_upgrade(env: Env) -> Result<(), Error> {
        admin(&env).require_auth();
        let storage = env.storage().instance();
        let pending: PendingUpgrade = storage
            .get(&DataKey::PendingUpgrade)
            .ok_or(Error::UpgradeNotScheduled)?;
        if env.ledger().timestamp() < pending.available_at {
            return Err(Error::UpgradeTimelockActive);
        }
        storage.remove(&DataKey::PendingUpgrade);
        UpgradeApplied {
            wasm_hash: pending.wasm_hash.clone(),
        }
        .publish(&env);
        env.deployer()
            .update_current_contract_wasm(pending.wasm_hash);
        Ok(())
    }

    // --- State rent --------------------------------------------------------

    /// Permissionless TTL maintenance, called by the backend on a schedule.
    /// Keeping it callable by anyone means the feed cannot be archived by
    /// operator neglect.
    pub fn extend_ttl(env: Env) {
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        let persistent = env.storage().persistent();
        persistent.extend_ttl(&DataKey::Latest, TTL_THRESHOLD, TTL_EXTEND_TO);
        persistent.extend_ttl(&DataKey::History, TTL_THRESHOLD, TTL_EXTEND_TO);
    }
}

#[contractimpl]
impl PriceFeedTrait for NavOracle {
    /// The asset NAV is denominated in — USDC for Trakx strategy products.
    fn base(env: Env) -> Asset {
        feed(&env).base
    }

    /// One feed, one quoted asset: the strategy token this oracle prices.
    fn assets(env: Env) -> Vec<Asset> {
        Vec::from_array(&env, [feed(&env).quote])
    }

    fn decimals(env: Env) -> u32 {
        feed(&env).decimals
    }

    fn resolution(env: Env) -> u32 {
        resolution(&env)
    }

    /// Price at an exact SEP-40 tick. Per SEP-40, an unknown asset or an
    /// unavailable tick returns `None` rather than an error, leaving the
    /// handling to the consumer.
    fn price(env: Env, asset: Asset, timestamp: u64) -> Option<PriceData> {
        if asset != quote(&env) {
            return None;
        }
        let tick = trim(timestamp, resolution(&env));
        let latest = latest(&env)?;
        if latest.timestamp == tick {
            return Some(to_price_data(&latest));
        }
        history(&env)
            .iter()
            .find(|record| record.timestamp == tick)
            .map(|record| to_price_data(&record))
    }

    /// The last `records` price points, most recent first.
    fn prices(env: Env, asset: Asset, records: u32) -> Option<Vec<PriceData>> {
        if asset != quote(&env) || records == 0 {
            return None;
        }
        let history = history(&env);
        let mut out = Vec::new(&env);
        let mut index = history.len();
        while index > 0 && out.len() < records {
            index -= 1;
            out.push_back(to_price_data(&history.get_unchecked(index)));
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
        if asset != quote(&env) {
            return None;
        }
        latest(&env).map(|record| to_price_data(&record))
    }
}

// --- Internals -------------------------------------------------------------

/// The single write path. Every rule is enforced here, and any violation
/// returns before a byte of state is written or an event is emitted.
fn record_nav(env: &Env, price: i128, timestamp: u64, bypass_deviation: bool) -> Result<(), Error> {
    if price <= 0 {
        return Err(Error::InvalidPrice);
    }
    let now = env.ledger().timestamp();
    if timestamp > now {
        return Err(Error::TimestampInFuture);
    }

    let config = config(env);
    let tick = trim(timestamp, resolution(env));

    if let Some(last) = latest(env) {
        if tick <= last.timestamp {
            return Err(Error::TimestampNotMonotonic);
        }
        if now.saturating_sub(last.published_at) < config.min_submission_interval {
            return Err(Error::SubmissionTooSoon);
        }
        if !bypass_deviation && deviation_bps(last.price, price) > config.max_deviation_bps as i128
        {
            return Err(Error::DeviationExceeded);
        }
    }

    let record = NavRecord {
        price,
        timestamp: tick,
        published_at: now,
    };

    let persistent = env.storage().persistent();
    let mut history = history(env);
    history.push_back(record.clone());
    while history.len() > config.history_size {
        history.remove(0);
    }
    persistent.set(&DataKey::History, &history);
    persistent.set(&DataKey::Latest, &record);
    persistent.extend_ttl(&DataKey::History, TTL_THRESHOLD, TTL_EXTEND_TO);
    persistent.extend_ttl(&DataKey::Latest, TTL_THRESHOLD, TTL_EXTEND_TO);
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);

    NavUpdated {
        asset: quote(env),
        price: record.price,
        timestamp: record.timestamp,
        published_at: record.published_at,
        overridden: bypass_deviation,
    }
    .publish(env);

    Ok(())
}

/// Absolute deviation from `previous` to `current`, in basis points.
fn deviation_bps(previous: i128, current: i128) -> i128 {
    let delta = (current - previous).abs();
    delta * BPS_DENOMINATOR / previous
}

/// SEP-40 tick: `floor(timestamp / resolution) * resolution`.
fn trim(timestamp: u64, resolution: u32) -> u64 {
    timestamp - (timestamp % resolution as u64)
}

fn validate_config(config: &OracleConfig) -> Result<(), Error> {
    if config.staleness_threshold == 0 || config.max_deviation_bps == 0 || config.history_size == 0
    {
        return Err(Error::InvalidConfig);
    }
    Ok(())
}

fn to_price_data(record: &NavRecord) -> PriceData {
    PriceData {
        price: record.price,
        timestamp: record.timestamp,
    }
}

fn latest(env: &Env) -> Option<NavRecord> {
    env.storage().persistent().get(&DataKey::Latest)
}

fn history(env: &Env) -> Vec<NavRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::History)
        .unwrap_or_else(|| Vec::new(env))
}

fn config(env: &Env) -> OracleConfig {
    env.storage().instance().get(&DataKey::Config).unwrap()
}

fn admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Admin).unwrap()
}

fn override_admin(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::OverrideAdmin)
        .unwrap()
}

fn publisher(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::Publisher).unwrap()
}

fn feed(env: &Env) -> FeedDefinition {
    env.storage().instance().get(&DataKey::Feed).unwrap()
}

fn quote(env: &Env) -> Asset {
    feed(env).quote
}

fn resolution(env: &Env) -> u32 {
    feed(env).resolution
}

#[cfg(test)]
mod test;
