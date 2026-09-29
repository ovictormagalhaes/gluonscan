//! Normalized domain model — the shapes a fetch returns, already converted and priced.

use crate::{Chain, Error, Protocol, Source};
use alloy_primitives::{Address, U256};
use rust_decimal::Decimal;
use std::str::FromStr;

/// Convert a raw base-unit integer to a human-scaled [`Decimal`].
///
/// Returns [`Error::Integrity`] (never panics, never fabricates) if the token declares more than 28
/// decimals or the value exceeds `Decimal`'s range. Splits into integer and fractional parts so a
/// large balance does not overflow `Decimal`'s 96-bit mantissa on the way in.
pub fn scaled(raw: U256, decimals: u8) -> Result<Decimal, Error> {
    if decimals > 28 {
        return Err(Error::Integrity {
            message: format!("token has {decimals} decimals (>28 is unsupported)"),
        });
    }
    let pow = U256::from(10u64).pow(U256::from(decimals));
    let int_part = raw / pow;
    let frac_part = raw % pow;
    let int_dec = Decimal::from_str(&int_part.to_string()).map_err(|_| Error::Integrity {
        message: "amount exceeds Decimal range".into(),
    })?;
    let frac_i128: i128 = frac_part
        .to_string()
        .parse()
        .map_err(|_| Error::Integrity {
            message: "amount fractional overflow".into(),
        })?;
    let frac_dec = Decimal::try_from_i128_with_scale(frac_i128, decimals as u32).map_err(|_| {
        Error::Integrity {
            message: "amount scale overflow".into(),
        }
    })?;
    int_dec
        .checked_add(frac_dec)
        .ok_or_else(|| Error::Integrity {
            message: "amount overflow".into(),
        })
}

/// A value that is guaranteed complete. There is **no public constructor for a partial value**:
/// [`Complete::new`] wraps an already-assembled `T`, and an adapter only calls it once every
/// required source has succeeded. Incomplete data is therefore untypeable at the API boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
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

/// A token held idle in the wallet — not deployed in any protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBalance {
    /// The idle token amount.
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

/// The kind of a yield-bearing token position.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YieldKind {
    /// A principal token (e.g. Pendle PT).
    PrincipalToken,
    /// A yield token (e.g. Pendle YT).
    YieldToken,
    /// A liquidity token.
    LiquidityToken,
}

/// A yield-bearing token holding (e.g. a Pendle PT/YT), with its maturity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldPosition {
    /// The held amount (priced when a price was available).
    pub amount: Amount,
    /// Which kind of yield token.
    pub kind: YieldKind,
    /// Maturity, when applicable.
    pub expiry: Option<Timestamp>,
}

/// An NFT held by the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftPosition {
    /// The collection identifier (contract address or program id).
    pub collection: String,
    /// The token id within the collection.
    pub token_id: String,
    /// The item's name, when available.
    pub name: Option<String>,
    /// A floor price, when available.
    pub floor_price: Option<Money>,
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
    /// A token held idle in the wallet (not in any protocol).
    Wallet(WalletBalance),
    /// A lending position.
    Lending(LendingPosition),
    /// A liquidity position.
    Liquidity(LiquidityPosition),
    /// A locked/staked position.
    Lock(LockPosition),
    /// A yield-bearing token position.
    Yield(YieldPosition),
    /// An NFT holding.
    Nft(NftPosition),
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
