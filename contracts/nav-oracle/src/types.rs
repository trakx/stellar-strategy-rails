//! Data types, storage keys, errors and events for the NAV Oracle.

use soroban_sdk::{contracterror, contractevent, contracttype, Address, BytesN, Symbol};

/// Asset identifier, as defined by SEP-40.
///
/// `Stellar` carries the address of a Classic asset deployed to Soroban through
/// its Stellar Asset Contract; `Other` names an off-chain asset (a fiat
/// currency, a fund unit) by symbol.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

/// A price point, as defined by SEP-40.
///
/// `price` is a scaled integer: the real price is `price / 10^decimals()`.
/// `timestamp` is trimmed to the feed's resolution (see [`NavRecord`]).
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

/// A NAV publication as stored by this contract.
///
/// Carries both timestamps that the system needs, which SEP-40's [`PriceData`]
/// cannot express on its own:
///
/// - `timestamp` is the SEP-40 tick, `floor(t / resolution) * resolution`. It is
///   the key under which the record is addressable by `price(asset, timestamp)`,
///   and the value compared for monotonicity.
/// - `published_at` is the ledger timestamp at which the submission was
///   accepted. Staleness is measured against it, and it is the value consumers
///   must use for forward pricing ("settle only at a NAV published strictly
///   after the request"), which a down-rounded tick would silently break.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavRecord {
    pub price: i128,
    pub timestamp: u64,
    pub published_at: u64,
}

/// The immutable definition of the feed, fixed at deployment.
///
/// SEP-40 requires `decimals` and `resolution` never to change once consumers
/// depend on them, so they are deliberately separated from [`OracleConfig`],
/// which exists precisely to be retuned.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedDefinition {
    /// The asset NAV is denominated in — USDC for Trakx strategy products.
    pub base: Asset,
    /// The single asset this feed prices: the strategy token.
    pub quote: Asset,
    /// Scale of the published NAV: the real value is `price / 10^decimals`.
    pub decimals: u32,
    /// Tick length in seconds. Timestamps are trimmed to it.
    pub resolution: u32,
}

/// Risk parameters. Held in storage rather than in code so that they are tuned
/// per product through `set_config` without a redeploy.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleConfig {
    /// Seconds after which the feed reports itself stale and consumers pause.
    pub staleness_threshold: u64,
    /// Circuit breaker: maximum accepted deviation from the last NAV, in basis
    /// points. Submissions beyond it are rejected without touching state.
    pub max_deviation_bps: u32,
    /// On-chain rate limit: minimum seconds between two accepted submissions.
    pub min_submission_interval: u64,
    /// Capacity of the historical ring buffer, in records.
    pub history_size: u32,
    /// Seconds an upgrade must sit announced on-chain before it can be applied.
    pub upgrade_timelock: u64,
}

/// An upgrade announced on-chain and awaiting its timelock.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUpgrade {
    pub wasm_hash: BytesN<32>,
    pub available_at: u64,
}

/// Storage keys.
///
/// Everything read on every invocation lives in `instance` storage (one entry,
/// one TTL to maintain). The NAV itself and its history live in `persistent`
/// storage: they are the contract's durable record and must outlive the
/// instance entry's rent cycle.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    OverrideAdmin,
    Publisher,
    Feed,
    Config,
    PendingUpgrade,
    /// Persistent: latest accepted [`NavRecord`].
    Latest,
    /// Persistent: bounded ring buffer of past [`NavRecord`]s, oldest first.
    History,
}

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// A configuration value is outside its permitted range.
    InvalidConfig = 1,
    /// NAV must be strictly positive.
    InvalidPrice = 2,
    /// Submitted timestamp is ahead of the current ledger.
    TimestampInFuture = 3,
    /// Submitted tick is not strictly later than the last accepted one.
    TimestampNotMonotonic = 4,
    /// Rate limit: `min_submission_interval` has not elapsed.
    SubmissionTooSoon = 5,
    /// Circuit breaker: deviation from the last NAV exceeds `max_deviation_bps`.
    DeviationExceeded = 6,
    /// No NAV has been published yet.
    NoPriceAvailable = 7,
    /// `apply_upgrade`/`cancel_upgrade` with nothing scheduled.
    UpgradeNotScheduled = 8,
    /// `apply_upgrade` before the timelock elapsed.
    UpgradeTimelockActive = 9,
    /// `schedule_upgrade` while another upgrade is already announced.
    UpgradeAlreadyScheduled = 10,
}

/// Emitted on every accepted NAV publication. The off-chain reconciliation
/// service and any third-party consumer subscribe to this rather than polling.
#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavUpdated {
    #[topic]
    pub asset: Asset,
    pub price: i128,
    pub timestamp: u64,
    pub published_at: u64,
    /// True when the value was admitted through the dual-signature override,
    /// bypassing the deviation bound.
    pub overridden: bool,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigUpdated {
    pub config: OracleConfig,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherUpdated {
    #[topic]
    pub publisher: Address,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeScheduled {
    #[topic]
    pub wasm_hash: BytesN<32>,
    pub available_at: u64,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeCancelled {
    #[topic]
    pub wasm_hash: BytesN<32>,
}

#[contractevent]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeApplied {
    #[topic]
    pub wasm_hash: BytesN<32>,
}
