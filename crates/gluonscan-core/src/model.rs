//! Normalized domain model — the shapes a fetch returns, already converted and priced.

use crate::{Chain, Protocol, Source};
use alloy_primitives::Address;
use rust_decimal::Decimal;

/// A value that is guaranteed complete. There is **no public constructor for a partial value**:
/// [`Complete::new`] wraps an already-assembled `T`, and an adapter only calls it once every
/// required source has succeeded. Incomplete data is therefore untypeable at the API boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complete<T>(T);

impl<T> Complete<T> {
    /// Wrap a fully-assembled value. Call only when the read is complete.
    pub fn new(value: T) -> Self {
        Complete(value)
    }

    /// Borrow the inner value.
    pub fn get(&self) -> &T {
        &self.0
    }

    /// Consume and return the inner value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// A Unix timestamp (seconds). Injected via a [`Clock`](crate::Clock) so reads are reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

/// How fresh a value is relative to when it was read.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    /// Read live this cycle.
    Live,
    /// Older than the freshness threshold.
    Stale,
}

/// Where a value came from and when — travels with every reading so trust is inspectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The backend that produced the value.
    pub source: Source,
    /// The chain it was read from.
    pub chain: Chain,
    /// Block height, when applicable.
    pub block: Option<u64>,
    /// When it was read.
    pub at: Timestamp,
    /// Freshness assessment.
    pub staleness: Staleness,
}

/// A fiat/quote currency. Kept explicit so amounts always carry their unit.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Currency {
    /// US dollar.
    Usd,
}

/// A monetary value in a specific currency. Never `f64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Money {
    /// The numeric amount.
    pub amount: Decimal,
    /// The currency the amount is denominated in.
    pub currency: Currency,
}

/// A token identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Ticker symbol.
    pub symbol: String,
    /// Contract address, when applicable.
    pub address: Option<Address>,
    /// Decimal precision.
    pub decimals: u8,
}

/// A token amount, optionally priced. `usd == None` means *not priced* (never a fabricated zero).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    /// The token.
    pub token: Token,
    /// Human-scaled amount (base units divided by `10^decimals`).
    pub amount: Decimal,
    /// USD value if a price was available.
    pub usd: Option<Money>,
}

/// A plain wallet holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    /// The held amount.
    pub amount: Amount,
}

/// A lending position (supplies and/or borrows) with an optional account health factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingPosition {
    /// Supplied (deposited) assets.
    pub supplied: Vec<Amount>,
    /// Borrowed assets (debt).
    pub borrowed: Vec<Amount>,
    /// Account health factor, when the account carries debt.
    pub health_factor: Option<Decimal>,
}

/// A concentrated-liquidity position. Carries the complete resource an AMM exposes — not a
/// cherry-picked subset (see the return-completeness invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityPosition {
    /// The pool's first token.
    pub token0: Token,
    /// The pool's second token.
    pub token1: Token,
    /// Fee tier in hundredths of a basis point (e.g. 3000 = 0.30%), when known.
    pub fee_tier_bps: Option<u32>,
    /// Lower tick bound of the range.
    pub tick_lower: i32,
    /// Upper tick bound of the range.
    pub tick_upper: i32,
    /// The pool's current tick.
    pub tick_current: i32,
    /// Whether the current tick is within the range.
    pub in_range: bool,
    /// Current principal amounts locked, `[token0, token1]`.
    pub assets: Vec<Amount>,
    /// Uncollected (claimable) fees, `[token0, token1]` — present only when fees were fetched.
    pub uncollected_fees: Vec<Amount>,
    /// Lifetime deposited amounts, `[token0, token1]`.
    pub deposited: Vec<Amount>,
    /// Lifetime withdrawn amounts, `[token0, token1]`.
    pub withdrawn: Vec<Amount>,
    /// Lifetime collected fees, `[token0, token1]`.
    pub collected_fees: Vec<Amount>,
}

/// A locked/staked position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockPosition {
    /// The locked assets.
    pub locked: Vec<Amount>,
    /// Unlock timestamp, when applicable.
    pub unlock_at: Option<Timestamp>,
}

/// A single normalized position within a protocol.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    /// A wallet holding.
    Holding(Holding),
    /// A lending position.
    Lending(LendingPosition),
    /// A liquidity position.
    Liquidity(LiquidityPosition),
    /// A locked/staked position.
    Lock(LockPosition),
}

/// A protocol's normalized reading for one wallet on one chain, with provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Which protocol.
    pub protocol: Protocol,
    /// Which chain.
    pub chain: Chain,
    /// Which backend produced it.
    pub source: Source,
    /// The normalized positions (empty = no position, not an error).
    pub positions: Vec<Position>,
    /// Where/when this reading came from.
    pub provenance: Provenance,
}
