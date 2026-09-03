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
//! - every privileged action — configuration, key rotation, the deviation
//!   override, upgrades — requires **both** governance signatures, so no single
//!   key can reach a price the circuit breaker would have rejected;
//! - ticks are strictly monotonic, never ahead of the ledger, and never older
//!   than the staleness window;
//! - a submission deviating beyond the configured bound is **rejected**, leaving
//!   state untouched and emitting nothing — the system pauses at the last valid
//!   price rather than settling at a wrong one;
//! - genuine extreme moves pass through a separate path, gated on the same two
//!   signatures and auditable in the event it emits;
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

/// Bounds on `OracleConfig`, enforced by the contract rather than left to
/// governance. A zero timelock would let an upgrade be announced and applied in
/// one transaction, defeating the announcement; an unbounded one would overflow
/// the deadline arithmetic. History is capped because the whole buffer is
/// deserialized and rewritten on every publication.
const MIN_UPGRADE_TIMELOCK: u64 = 24 * 60 * 60;
const MAX_UPGRADE_TIMELOCK: u64 = 90 * 24 * 60 * 60;
const MAX_HISTORY_SIZE: u32 = 100;

/// Bounds on the published NAV scale. 6 is USDC's own precision on EVM chains
/// and the lowest any price convention uses; 18 is the highest. The scales in
/// actual use sit inside that range — 7 for Stellar Classic assets, 8 for
/// Chainlink feeds, 14 for Soroban price feeds — so a value outside it is a
/// typo rather than a choice, and a large enough one would leave no room in
/// `i128` for the NAV itself.
const MIN_DECIMALS: u32 = 6;
const MAX_DECIMALS: u32 = 18;

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
    /// `admin` and `co_admin` are the two governance signers: every privileged
    /// action requires both. They must be distinct from each other and from the
    /// publisher — the same address in two roles would silently collapse the
    /// 2-of-2 into a single signature.
    pub fn __constructor(
        env: Env,
        admin: Address,
        co_admin: Address,
        publisher: Address,
        feed: FeedDefinition,
        config: OracleConfig,
    ) -> Result<(), Error> {
        validate_feed(&feed)?;
        validate_roles(&admin, &co_admin, &publisher)?;
        validate_config(&config)?;

        let storage = env.storage().instance();
        storage.set(&DataKey::Admin, &admin);
        storage.set(&DataKey::CoAdmin, &co_admin);
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
    /// Reserved for genuine extreme market moves. Requires both governance
    /// signatures, like every other privileged action.
    ///
    /// Bypasses the deviation bound *and* the rate limit: the override exists to
    /// unblock a feed the circuit breaker has stopped, and a rejected submission
    /// does not advance the last publication time, so the rate limit would
    /// otherwise keep the feed mispriced for the rest of the interval. The two
    /// signatures are the spam control here; the rate limit protects the
    /// single-key publisher path, which this is not. Monotonicity and the
    /// timestamp checks still apply.
    pub fn submit_nav_override(env: Env, price: i128, timestamp: u64) -> Result<(), Error> {
        require_governance(&env);
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

    pub fn co_admin(env: Env) -> Address {
        co_admin(&env)
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
        require_governance(&env);
        validate_config(&config)?;
        let storage = env.storage().instance();
        storage.set(&DataKey::Config, &config);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        ConfigUpdated { config }.publish(&env);
        Ok(())
    }

    /// Rotate the publishing key. The NAV service's signing key is operational
    /// and rotates on a schedule; the feed identity does not.
    ///
    /// This needs both governance signatures for the same reason `set_config`
    /// does: a single key able to install its own publisher could publish any
    /// value through `submit_nav`, reaching the outcome the override path is
    /// meant to gate. Rotation stays immediate rather than timelocked, because
    /// its urgent case is a suspected key compromise.
    pub fn set_publisher(env: Env, publisher: Address) -> Result<(), Error> {
        require_governance(&env);
        validate_roles(&admin(&env), &co_admin(&env), &publisher)?;
        let storage = env.storage().instance();
        storage.set(&DataKey::Publisher, &publisher);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        PublisherUpdated { publisher }.publish(&env);
        Ok(())
    }

    /// Rotate the governance keys themselves, signed by both outgoing ones.
    ///
    /// Without this, a key known to be compromised could never be retired: it
    /// could not act alone, but it would remain one half of the pair forever.
    pub fn set_governance(env: Env, admin: Address, co_admin: Address) -> Result<(), Error> {
        require_governance(&env);
        validate_roles(&admin, &co_admin, &publisher(&env))?;
        let storage = env.storage().instance();
        storage.set(&DataKey::Admin, &admin);
        storage.set(&DataKey::CoAdmin, &co_admin);
        storage.extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
        GovernanceUpdated { admin, co_admin }.publish(&env);
        Ok(())
    }

    // --- Upgrade under governance -----------------------------------------

    /// Announce an upgrade. It becomes applicable only after the configured
    /// timelock, and the announcement is on-chain, so holders can observe — and
    /// if they wish, exit — before any change activates.
    pub fn schedule_upgrade(env: Env, wasm_hash: BytesN<32>) -> Result<(), Error> {
        require_governance(&env);
        let storage = env.storage().instance();
        if storage.has(&DataKey::PendingUpgrade) {
            return Err(Error::UpgradeAlreadyScheduled);
        }
        // `upgrade_timelock` is bounded by `validate_config`, so this cannot
        // overflow; `saturating_add` keeps that independent of the bound.
        let available_at = env
            .ledger()
            .timestamp()
            .saturating_add(config(&env).upgrade_timelock);
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
        require_governance(&env);
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
        require_governance(&env);
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
        // The NAV entries do not exist until the first publication, and
        // extending a missing entry traps. The maintenance job runs from
        // deployment, so it must tolerate that window rather than fail every
        // run — and take the instance entry with it.
        let persistent = env.storage().persistent();
        for key in [DataKey::Latest, DataKey::History] {
            if persistent.has(&key) {
                persistent.extend_ttl(&key, TTL_THRESHOLD, TTL_EXTEND_TO);
            }
        }
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
fn record_nav(env: &Env, price: i128, timestamp: u64, is_override: bool) -> Result<(), Error> {
    if price <= 0 {
        return Err(Error::InvalidPrice);
    }
    let now = env.ledger().timestamp();
    if timestamp > now {
        return Err(Error::TimestampInFuture);
    }

    let config = config(env);
    // A valuation already older than the staleness window is not publishable:
    // accepting one would stamp `published_at = now` on it, so the feed would
    // report itself fresh while serving an ancient price.
    if now.saturating_sub(timestamp) > config.staleness_threshold {
        return Err(Error::TimestampTooOld);
    }
    let tick = trim(timestamp, resolution(env));

    if let Some(last) = latest(env) {
        if tick <= last.timestamp {
            return Err(Error::TimestampNotMonotonic);
        }
        if !is_override && now.saturating_sub(last.published_at) < config.min_submission_interval {
            return Err(Error::SubmissionTooSoon);
        }
        if !is_override && !within_deviation_bound(last.price, price, config.max_deviation_bps) {
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
        overridden: is_override,
    }
    .publish(env);

    Ok(())
}

/// Whether the move from `previous` to `current` is within `max_bps`.
///
/// Both values are strictly positive by construction, so the subtraction cannot
/// overflow; scaling to basis points can, for a NAV far beyond any plausible
/// fund value. That is treated as a breach rather than a panic — the publisher's
/// `price` is an unvalidated `i128`, and a typed rejection is the correct
/// response to a nonsensical one.
fn within_deviation_bound(previous: i128, current: i128, max_bps: u32) -> bool {
    match (current - previous).abs().checked_mul(BPS_DENOMINATOR) {
        Some(scaled) => scaled / previous <= max_bps as i128,
        None => false,
    }
}

/// SEP-40 tick: `floor(timestamp / resolution) * resolution`.
fn trim(timestamp: u64, resolution: u32) -> u64 {
    timestamp - (timestamp % resolution as u64)
}

/// The feed definition is immutable once deployed, so it gets the same
/// treatment as the config: the values a deployer could plausibly fat-finger
/// are bounded by the contract rather than trusted.
fn validate_feed(feed: &FeedDefinition) -> Result<(), Error> {
    if feed.resolution == 0 || feed.decimals < MIN_DECIMALS || feed.decimals > MAX_DECIMALS {
        return Err(Error::InvalidConfig);
    }
    Ok(())
}

fn validate_config(config: &OracleConfig) -> Result<(), Error> {
    if config.staleness_threshold == 0
        || config.max_deviation_bps == 0
        || config.history_size == 0
        || config.history_size > MAX_HISTORY_SIZE
        || config.upgrade_timelock < MIN_UPGRADE_TIMELOCK
        || config.upgrade_timelock > MAX_UPGRADE_TIMELOCK
    {
        return Err(Error::InvalidConfig);
    }
    Ok(())
}

/// The three roles must be held by three different addresses. `require_auth`
/// called twice on one address is satisfied by one signature, so a shared
/// address would turn the 2-of-2 into a 1-of-1 with no outward sign.
fn validate_roles(admin: &Address, co_admin: &Address, publisher: &Address) -> Result<(), Error> {
    if admin == co_admin || admin == publisher || co_admin == publisher {
        return Err(Error::RolesNotDistinct);
    }
    Ok(())
}

/// Both governance signatures. Every privileged function goes through here, so
/// that no single key can reach an outcome the other would have to approve.
fn require_governance(env: &Env) {
    admin(env).require_auth();
    co_admin(env).require_auth();
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

fn co_admin(env: &Env) -> Address {
    env.storage().instance().get(&DataKey::CoAdmin).unwrap()
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
