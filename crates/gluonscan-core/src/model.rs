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
    /// Full name, when the source provides it.
    pub name: Option<String>,
    /// Contract address, when applicable.
    pub address: Option<Address>,
    /// Decimal precision.
    pub decimals: u8,
}

impl Token {
    /// A token with just symbol/address/decimals (no name yet).
    pub fn new(symbol: impl Into<String>, address: Option<Address>, decimals: u8) -> Self {
        Token {
            symbol: symbol.into(),
            name: None,
            address,
            decimals,
        }
    }

    /// Attach a display name (builder-style).
    #[must_use]
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }
}

/// Reconstruct the exact raw base-unit integer from a human [`Decimal`] amount.
///
/// The inverse of [`scaled`], for sources that hand back a human-scaled decimal rather than a
/// base-unit integer. Digits finer than one base unit are dropped (they are below the token's
/// precision); a negative amount is an [`Error::Integrity`]. Works via the decimal's mantissa/scale
/// so large amounts never overflow `Decimal` on the way out.
pub fn to_raw(amount: Decimal, decimals: u8) -> Result<U256, Error> {
    if amount.is_sign_negative() {
        return Err(Error::Integrity {
            message: "negative amount has no base-unit representation".into(),
        });
    }
    let mantissa = u128::try_from(amount.mantissa()).map_err(|_| Error::Integrity {
        message: "amount mantissa out of range".into(),
    })?;
    let base = U256::from(mantissa);
    let scale = amount.scale();
    let dec = decimals as u32;
    if dec >= scale {
        Ok(base * U256::from(10u64).pow(U256::from(dec - scale)))
    } else {
        Ok(base / U256::from(10u64).pow(U256::from(scale - dec)))
    }
}

/// A token amount, optionally priced. `usd == None` means *not priced* (never a fabricated zero).
/// The `raw` base-unit integer is the source of truth; `amount` is its human-scaled view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    /// The token.
    pub token: Token,
    /// The exact raw quantity in base units (`amount * 10^decimals`).
    pub raw: U256,
    /// Human-scaled amount (base units divided by `10^decimals`).
    pub amount: Decimal,
    /// USD value if a price was available.
    pub usd: Option<Money>,
}

impl Amount {
    /// Build from a raw base-unit integer (the source of truth), scaling to a human [`Decimal`].
    /// `usd` is left `None`.
    pub fn from_raw(token: Token, raw: U256) -> Result<Self, Error> {
        let amount = scaled(raw, token.decimals)?;
        Ok(Amount {
            token,
            raw,
            amount,
            usd: None,
        })
    }

    /// Build from a human [`Decimal`] amount, reconstructing the raw base-unit integer via
    /// [`to_raw`]. `usd` is left `None`.
    pub fn from_decimal(token: Token, amount: Decimal) -> Result<Self, Error> {
        let raw = to_raw(amount, token.decimals)?;
        Ok(Amount {
            token,
            raw,
            amount,
            usd: None,
        })
    }

    /// Attach a USD value (builder-style).
    #[must_use]
    pub fn with_usd(mut self, usd: Option<Money>) -> Self {
        self.usd = usd;
        self
    }
}

/// A token held idle in the wallet — not deployed in any protocol.
///
/// The `possible_spam` / `verified_contract` flags are passed through from the indexer, not acted
/// on: gluonscan returns every balance it sees and lets the consumer decide what to hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBalance {
    /// The idle token amount.
    pub amount: Amount,
    /// The indexer flagged this token as likely spam, when it says so.
    pub possible_spam: Option<bool>,
    /// The indexer considers the token contract verified, when it says so.
    pub verified_contract: Option<bool>,
}

impl WalletBalance {
    /// A balance with no spam/verification metadata.
    pub fn new(amount: Amount) -> Self {
        WalletBalance {
            amount,
            possible_spam: None,
            verified_contract: None,
        }
    }
}

/// A supplied (deposited) lending asset with its collateral risk parameters and supply rate.
///
/// The risk fields let a consumer recompute the health factor offline (capture-once, reprice-later)
/// and render risk without a second round trip. Fractions are `0..1` (e.g. `0.83` = 83%).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedAsset {
    /// The supplied amount.
    pub amount: Amount,
    /// Liquidation threshold as a fraction, when known.
    pub liquidation_threshold: Option<Decimal>,
    /// Maximum loan-to-value as a fraction, when known.
    pub max_ltv: Option<Decimal>,
    /// Whether this asset is currently enabled as collateral.
    pub is_collateral: bool,
    /// Whether this asset is eligible to be collateral.
    pub can_be_collateral: bool,
    /// Supply APY as a fraction (e.g. `0.031` = 3.1%), when known.
    pub apy: Option<Decimal>,
}

impl SuppliedAsset {
    /// A supplied asset with no risk metadata yet (all optionals absent, not collateral).
    pub fn new(amount: Amount) -> Self {
        SuppliedAsset {
            amount,
            liquidation_threshold: None,
            max_ltv: None,
            is_collateral: false,
            can_be_collateral: false,
            apy: None,
        }
    }
}

/// A borrowed lending asset (debt) with its borrow factor and rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowedAsset {
    /// The borrowed amount (debt).
    pub amount: Amount,
    /// Borrow factor as a fraction (Aave pins `1.0`; Kamino per-reserve), when known.
    pub borrow_factor: Option<Decimal>,
    /// Borrow APY as a fraction, when known.
    pub apy: Option<Decimal>,
}

impl BorrowedAsset {
    /// A borrowed asset with no rate/factor metadata yet.
    pub fn new(amount: Amount) -> Self {
        BorrowedAsset {
            amount,
            borrow_factor: None,
            apy: None,
        }
    }
}

/// A lending position (supplies and/or borrows) with an optional account health factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingPosition {
    /// Supplied (deposited) assets with per-asset risk parameters.
    pub supplied: Vec<SuppliedAsset>,
    /// Borrowed assets (debt).
    pub borrowed: Vec<BorrowedAsset>,
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
    /// Annualized rate (APR) as a fraction (e.g. `0.14` = 14%), when a source provides it.
    pub apr: Option<Decimal>,
    /// Whether the position still holds liquidity, or is dormant (closed but not burned). A dormant
    /// position is returned, not dropped, so the consumer can account for it without touching totals.
    pub status: PositionStatus,
}

/// Whether a position is live or dormant.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PositionStatus {
    /// Holds liquidity / a live balance.
    #[default]
    Active,
    /// Closed/emptied but still on-chain (e.g. an LP with zero liquidity, NFT intact).
    Inactive,
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
    /// Implied/aggregated APY as a fraction, when the source provides it.
    pub apy: Option<Decimal>,
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
    /// The indexer flagged this NFT as likely spam, when it says so. Returned, not dropped.
    pub possible_spam: Option<bool>,
}

/// A locked position (assets locked until an unlock time, e.g. Pendle vePENDLE).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockPosition {
    /// The locked assets.
    pub locked: Vec<Amount>,
    /// Unlock timestamp, when applicable.
    pub unlock_at: Option<Timestamp>,
}

/// A staked position (assets staked in a protocol, e.g. Pendle sPENDLE liquid staking).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StakePosition {
    /// The staked assets.
    pub staked: Vec<Amount>,
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
    /// A locked position.
    Lock(LockPosition),
    /// A staked position.
    Stake(StakePosition),
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
    /// Receipt / wrapper token contracts this reading represents (e.g. Pendle PT/YT, sPENDLE, a
    /// protocol's LP-position NFT). A consumer that also lists raw wallet balances drops these to
    /// avoid double-counting a protocol position as a loose token.
    pub receipt_tokens: Vec<Address>,
    /// Where/when this reading came from.
    pub provenance: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_and_to_raw_round_trip() {
        // 1.5 USDC (6 decimals) = 1_500_000 base units.
        let raw = U256::from(1_500_000u64);
        let dec = scaled(raw, 6).unwrap();
        assert_eq!(dec, Decimal::from_str_exact("1.5").unwrap());
        assert_eq!(to_raw(dec, 6).unwrap(), raw);
    }

    #[test]
    fn to_raw_drops_sub_base_unit_digits() {
        // More fractional digits than the token has decimals: excess precision is truncated.
        let over = Decimal::from_str_exact("1.2345678").unwrap(); // 7 dp for a 6-dp token
        assert_eq!(to_raw(over, 6).unwrap(), U256::from(1_234_567u64));
    }

    #[test]
    fn to_raw_rejects_negative() {
        assert!(to_raw(Decimal::from_str_exact("-1").unwrap(), 6).is_err());
    }

    #[test]
    fn from_raw_sets_both_views() {
        let t = Token::new("WETH", None, 18);
        let a = Amount::from_raw(t, U256::from(2_000_000_000_000_000_000u64)).unwrap();
        assert_eq!(a.raw, U256::from(2_000_000_000_000_000_000u64));
        assert_eq!(a.amount, Decimal::from_str_exact("2").unwrap());
        assert!(a.usd.is_none());
    }
}
