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

/// A value an adapter has assembled from a fully-successful read. [`Complete::new`] is the single
/// wrapping point: adapters call it only after every required source has succeeded, so a `Complete`
/// in a reading signals "no partial data reached here." It is a construction convention the adapters
/// uphold — `new` is public so they (separate crates) can build it — not a type-system proof.
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
#[non_exhaustive]
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

impl Provenance {
    /// Provenance for a value read at `at` from `source` on `chain`, with no block height.
    pub fn new(source: Source, chain: Chain, at: Timestamp, staleness: Staleness) -> Self {
        Provenance {
            source,
            chain,
            block: None,
            at,
            staleness,
        }
    }

    /// Attach a block height (builder-style).
    #[must_use]
    pub fn with_block(mut self, block: Option<u64>) -> Self {
        self.block = block;
        self
    }
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

impl Money {
    /// A monetary value denominated in `currency`.
    pub fn new(amount: Decimal, currency: Currency) -> Self {
        Money { amount, currency }
    }

    /// A USD-denominated value.
    pub fn usd(amount: Decimal) -> Self {
        Money {
            amount,
            currency: Currency::Usd,
        }
    }
}

/// A token's on-chain identity, chain-agnostic: an EVM contract address or a Solana SPL mint.
/// EVM addresses keep their 20-byte type; Solana mints are base58 strings (they do not fit an
/// [`Address`]). `Display` renders an EVM address as `0x…` and a Solana mint as its base58 form, so
/// a consumer can use one string field for both.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenAddress {
    /// An EVM contract address.
    Evm(Address),
    /// A Solana SPL mint (base58).
    Solana(String),
}

impl TokenAddress {
    /// The EVM address, if this token is an EVM contract (for on-chain calls that need one).
    pub fn as_evm(&self) -> Option<Address> {
        match self {
            TokenAddress::Evm(a) => Some(*a),
            _ => None,
        }
    }
}

impl std::fmt::Display for TokenAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenAddress::Evm(a) => write!(f, "{a:#x}"),
            TokenAddress::Solana(m) => f.write_str(m),
        }
    }
}

/// A token identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Ticker symbol.
    pub symbol: String,
    /// Full name, when the source provides it.
    pub name: Option<String>,
    /// On-chain identity (EVM contract or Solana mint), when applicable.
    pub address: Option<TokenAddress>,
    /// Decimal precision.
    pub decimals: u8,
}

impl Token {
    /// A token with a chain-agnostic address (no name yet).
    pub fn new(symbol: impl Into<String>, address: Option<TokenAddress>, decimals: u8) -> Self {
        Token {
            symbol: symbol.into(),
            name: None,
            address,
            decimals,
        }
    }

    /// An EVM token from an optional contract address (no name yet).
    pub fn evm(symbol: impl Into<String>, address: Option<Address>, decimals: u8) -> Self {
        Token::new(symbol, address.map(TokenAddress::Evm), decimals)
    }

    /// A Solana token from an optional SPL mint (no name yet).
    pub fn solana(symbol: impl Into<String>, mint: Option<String>, decimals: u8) -> Self {
        Token::new(symbol, mint.map(TokenAddress::Solana), decimals)
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

/// A plain token balance held by the owner: a token idle in an on-chain wallet, or the cash/equity
/// balance of a protocol account (e.g. the USD-margin equity of a perps account). It carries value
/// but no protocol-specific position mechanics.
///
/// When present, the `possible_spam` / `verified_contract` flags are passed through from the source
/// indexer, not acted on: gluonscan returns every balance it sees and lets the consumer decide what
/// to hide. A balance from a non-indexer source (e.g. an account-equity figure) leaves them `None`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBalance {
    /// The token amount held.
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

    /// Attach the indexer's spam flag (builder-style).
    #[must_use]
    pub fn with_possible_spam(mut self, possible_spam: Option<bool>) -> Self {
        self.possible_spam = possible_spam;
        self
    }

    /// Attach the indexer's contract-verification flag (builder-style).
    #[must_use]
    pub fn with_verified_contract(mut self, verified_contract: Option<bool>) -> Self {
        self.verified_contract = verified_contract;
        self
    }
}

/// A supplied (deposited) lending asset with its collateral risk parameters and supply rate.
///
/// The risk fields let a consumer recompute the health factor offline (capture-once, reprice-later)
/// and render risk without a second round trip. Fractions are `0..1` (e.g. `0.83` = 83%).
#[non_exhaustive]
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

    /// Set the liquidation threshold fraction (builder-style).
    #[must_use]
    pub fn with_liquidation_threshold(mut self, lt: Option<Decimal>) -> Self {
        self.liquidation_threshold = lt;
        self
    }

    /// Set the maximum loan-to-value fraction (builder-style).
    #[must_use]
    pub fn with_max_ltv(mut self, max_ltv: Option<Decimal>) -> Self {
        self.max_ltv = max_ltv;
        self
    }

    /// Set the collateral flags: currently enabled, and eligible (builder-style).
    #[must_use]
    pub fn with_collateral(mut self, is_collateral: bool, can_be_collateral: bool) -> Self {
        self.is_collateral = is_collateral;
        self.can_be_collateral = can_be_collateral;
        self
    }

    /// Set the supply APY fraction (builder-style).
    #[must_use]
    pub fn with_apy(mut self, apy: Option<Decimal>) -> Self {
        self.apy = apy;
        self
    }
}

/// A borrowed lending asset (debt) with its borrow factor and rate.
#[non_exhaustive]
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

    /// Set the borrow factor fraction (builder-style).
    #[must_use]
    pub fn with_borrow_factor(mut self, borrow_factor: Option<Decimal>) -> Self {
        self.borrow_factor = borrow_factor;
        self
    }

    /// Set the borrow APY fraction (builder-style).
    #[must_use]
    pub fn with_apy(mut self, apy: Option<Decimal>) -> Self {
        self.apy = apy;
        self
    }
}

/// A lending position (supplies and/or borrows) with an optional account health factor.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LendingPosition {
    /// Supplied (deposited) assets with per-asset risk parameters.
    pub supplied: Vec<SuppliedAsset>,
    /// Borrowed assets (debt).
    pub borrowed: Vec<BorrowedAsset>,
    /// Account health factor, when the account carries debt.
    pub health_factor: Option<Decimal>,
    /// Claimable protocol rewards / incentives, when the source exposes them.
    pub rewards: Vec<Amount>,
    /// Source identity of the isolated market this position belongs to, when the protocol is
    /// per-market isolated (e.g. Morpho Blue's on-chain `marketId`). `None` for cross-collateralized
    /// protocols (Aave, Kamino), where one position already represents the whole account. Consumers
    /// that dedup or group positions MUST include this: two isolated markets can share a loan or
    /// collateral token (and even the same LLTV, differing only by oracle/IRM), so without the
    /// source id they collapse and a position is lost.
    pub market_id: Option<String>,
}

impl LendingPosition {
    /// A lending position from its supplied and borrowed legs (no health factor yet).
    pub fn new(supplied: Vec<SuppliedAsset>, borrowed: Vec<BorrowedAsset>) -> Self {
        LendingPosition {
            supplied,
            borrowed,
            health_factor: None,
            rewards: Vec::new(),
            market_id: None,
        }
    }

    /// Attach the isolated-market source id (builder-style). See [`LendingPosition::market_id`].
    #[must_use]
    pub fn with_market_id(mut self, market_id: Option<String>) -> Self {
        self.market_id = market_id;
        self
    }

    /// Attach the account health factor (builder-style).
    #[must_use]
    pub fn with_health_factor(mut self, health_factor: Option<Decimal>) -> Self {
        self.health_factor = health_factor;
        self
    }

    /// Set the claimable rewards / incentives (builder-style).
    #[must_use]
    pub fn with_rewards(mut self, rewards: Vec<Amount>) -> Self {
        self.rewards = rewards;
        self
    }
}

/// A concentrated-liquidity position. Carries the complete resource an AMM exposes — not a
/// cherry-picked subset (see the return-completeness invariant).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityPosition {
    /// Protocol-specific position identifier (Uniswap V3 NFT token id, Raydium position NFT mint),
    /// when the source exposes one. Lets a consumer tell two positions in the same pool apart and
    /// address a single position (e.g. for history / detail).
    pub id: Option<String>,
    /// The pool (pair) address or id, when the source exposes one.
    pub pool: Option<String>,
    /// The pool's first token.
    pub token0: Token,
    /// The pool's second token.
    pub token1: Token,
    /// Fee tier in hundredths of a basis point (e.g. 3000 = 0.30%), when known.
    pub fee_tier_bps: Option<u32>,
    /// The pool's current sqrt price as a string (Q64.96 for Uniswap V3; the source's native scale),
    /// when exposed. For the consumer's own price-range rendering.
    pub sqrt_price: Option<String>,
    /// The pool's tick spacing, when known.
    pub tick_spacing: Option<i32>,
    /// Unix timestamp (seconds) the position was opened, when the source exposes it.
    pub created_at: Option<i64>,
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
    /// Claimable protocol rewards / incentives (emission tokens), distinct from trading fees —
    /// present only when the source exposes them.
    pub rewards: Vec<Amount>,
    /// Annualized rate (APR) as a fraction (e.g. `0.14` = 14%), when a source provides it.
    pub apr: Option<Decimal>,
    /// Whether the position still holds liquidity, or is dormant (closed but not burned). A dormant
    /// position is returned, not dropped, so the consumer can account for it without touching totals.
    pub status: PositionStatus,
}

impl LiquidityPosition {
    /// A liquidity position from its pool geometry. The amount vectors, fee tier and APR default to
    /// empty/absent; attach them with the `with_*` setters. `status` defaults per [`PositionStatus`].
    pub fn new(
        token0: Token,
        token1: Token,
        tick_lower: i32,
        tick_upper: i32,
        tick_current: i32,
        in_range: bool,
    ) -> Self {
        LiquidityPosition {
            id: None,
            pool: None,
            token0,
            token1,
            fee_tier_bps: None,
            sqrt_price: None,
            tick_spacing: None,
            created_at: None,
            tick_lower,
            tick_upper,
            tick_current,
            in_range,
            assets: Vec::new(),
            uncollected_fees: Vec::new(),
            deposited: Vec::new(),
            withdrawn: Vec::new(),
            collected_fees: Vec::new(),
            rewards: Vec::new(),
            apr: None,
            status: PositionStatus::Active,
        }
    }

    /// Set the position identifier (builder-style).
    #[must_use]
    pub fn with_id(mut self, id: Option<String>) -> Self {
        self.id = id;
        self
    }

    /// Set the pool (pair) address/id (builder-style).
    #[must_use]
    pub fn with_pool(mut self, pool: Option<String>) -> Self {
        self.pool = pool;
        self
    }

    /// Set the fee tier in hundredths of a basis point (builder-style).
    #[must_use]
    pub fn with_fee_tier_bps(mut self, fee_tier_bps: Option<u32>) -> Self {
        self.fee_tier_bps = fee_tier_bps;
        self
    }

    /// Set the pool's current sqrt price (builder-style).
    #[must_use]
    pub fn with_sqrt_price(mut self, sqrt_price: Option<String>) -> Self {
        self.sqrt_price = sqrt_price;
        self
    }

    /// Set the pool's tick spacing (builder-style).
    #[must_use]
    pub fn with_tick_spacing(mut self, tick_spacing: Option<i32>) -> Self {
        self.tick_spacing = tick_spacing;
        self
    }

    /// Set the position's creation timestamp (unix seconds, builder-style).
    #[must_use]
    pub fn with_created_at(mut self, created_at: Option<i64>) -> Self {
        self.created_at = created_at;
        self
    }

    /// Set the current principal amounts `[token0, token1]` (builder-style).
    #[must_use]
    pub fn with_assets(mut self, assets: Vec<Amount>) -> Self {
        self.assets = assets;
        self
    }

    /// Set the uncollected (claimable) fees `[token0, token1]` (builder-style).
    #[must_use]
    pub fn with_uncollected_fees(mut self, uncollected_fees: Vec<Amount>) -> Self {
        self.uncollected_fees = uncollected_fees;
        self
    }

    /// Set the lifetime deposited amounts `[token0, token1]` (builder-style).
    #[must_use]
    pub fn with_deposited(mut self, deposited: Vec<Amount>) -> Self {
        self.deposited = deposited;
        self
    }

    /// Set the lifetime withdrawn amounts `[token0, token1]` (builder-style).
    #[must_use]
    pub fn with_withdrawn(mut self, withdrawn: Vec<Amount>) -> Self {
        self.withdrawn = withdrawn;
        self
    }

    /// Set the lifetime collected fees `[token0, token1]` (builder-style).
    #[must_use]
    pub fn with_collected_fees(mut self, collected_fees: Vec<Amount>) -> Self {
        self.collected_fees = collected_fees;
        self
    }

    /// Set the claimable rewards / incentives (emission tokens, builder-style).
    #[must_use]
    pub fn with_rewards(mut self, rewards: Vec<Amount>) -> Self {
        self.rewards = rewards;
        self
    }

    /// Set the annualized rate fraction (builder-style).
    #[must_use]
    pub fn with_apr(mut self, apr: Option<Decimal>) -> Self {
        self.apr = apr;
        self
    }

    /// Set the position status (builder-style).
    #[must_use]
    pub fn with_status(mut self, status: PositionStatus) -> Self {
        self.status = status;
        self
    }
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
#[non_exhaustive]
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

impl YieldPosition {
    /// A yield holding of `kind` for `amount` (no maturity or APY yet).
    pub fn new(amount: Amount, kind: YieldKind) -> Self {
        YieldPosition {
            amount,
            kind,
            expiry: None,
            apy: None,
        }
    }

    /// Set the maturity (builder-style).
    #[must_use]
    pub fn with_expiry(mut self, expiry: Option<Timestamp>) -> Self {
        self.expiry = expiry;
        self
    }

    /// Set the implied/aggregated APY fraction (builder-style).
    #[must_use]
    pub fn with_apy(mut self, apy: Option<Decimal>) -> Self {
        self.apy = apy;
        self
    }
}

/// An NFT held by the wallet.
#[non_exhaustive]
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

impl NftPosition {
    /// An NFT identified by collection + token id (no name/floor/spam metadata yet).
    pub fn new(collection: impl Into<String>, token_id: impl Into<String>) -> Self {
        NftPosition {
            collection: collection.into(),
            token_id: token_id.into(),
            name: None,
            floor_price: None,
            possible_spam: None,
        }
    }

    /// Attach a display name (builder-style).
    #[must_use]
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    /// Attach a floor price (builder-style).
    #[must_use]
    pub fn with_floor_price(mut self, floor_price: Option<Money>) -> Self {
        self.floor_price = floor_price;
        self
    }

    /// Attach the indexer's spam flag (builder-style).
    #[must_use]
    pub fn with_possible_spam(mut self, possible_spam: Option<bool>) -> Self {
        self.possible_spam = possible_spam;
        self
    }
}

/// A locked position (assets locked until an unlock time, e.g. Pendle vePENDLE).
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockPosition {
    /// The locked assets.
    pub locked: Vec<Amount>,
    /// Unlock timestamp, when applicable.
    pub unlock_at: Option<Timestamp>,
}

impl LockPosition {
    /// A lock over `locked` assets (no unlock time yet).
    pub fn new(locked: Vec<Amount>) -> Self {
        LockPosition {
            locked,
            unlock_at: None,
        }
    }

    /// Set the unlock timestamp (builder-style).
    #[must_use]
    pub fn with_unlock_at(mut self, unlock_at: Option<Timestamp>) -> Self {
        self.unlock_at = unlock_at;
        self
    }
}

/// A staked position (assets staked in a protocol, e.g. Pendle sPENDLE liquid staking).
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StakePosition {
    /// The staked assets.
    pub staked: Vec<Amount>,
    /// Claimable staking rewards, when the source exposes them.
    pub rewards: Vec<Amount>,
    /// Current staking yield as a FRACTION (`0.03` = 3%), when the source exposes it. This is the
    /// protocol-wide rate (the same for every staker), informational only — it never affects a
    /// position's value, so a source that can't supply it leaves this `None` rather than failing the
    /// read.
    pub apy: Option<Decimal>,
}

impl StakePosition {
    /// A stake over `staked` assets (no rewards yet).
    pub fn new(staked: Vec<Amount>) -> Self {
        StakePosition {
            staked,
            rewards: Vec::new(),
            apy: None,
        }
    }

    /// Set the claimable staking rewards (builder-style).
    #[must_use]
    pub fn with_rewards(mut self, rewards: Vec<Amount>) -> Self {
        self.rewards = rewards;
        self
    }

    /// Set the current staking yield as a fraction (builder-style). See [`StakePosition::apy`].
    #[must_use]
    pub fn with_apy(mut self, apy: Option<Decimal>) -> Self {
        self.apy = apy;
        self
    }
}

/// Which direction a perpetual position is held.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerpSide {
    /// Long — profits when the mark price rises above entry.
    Long,
    /// Short — profits when the mark price falls below entry.
    Short,
}

/// A perpetual / derivative position: the margin backing it plus the open contract's size, prices
/// and risk. Amounts the source does not expose stay `None` rather than being fabricated.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpPosition {
    /// The market identifier, e.g. `"ETH-USD"`.
    pub market: String,
    /// Long or short.
    pub side: PerpSide,
    /// Collateral (margin) backing the position.
    pub collateral: Vec<Amount>,
    /// Position size in the base asset (absolute magnitude; direction is `side`).
    pub size: Decimal,
    /// Average entry price, when known.
    pub entry_price: Option<Decimal>,
    /// Current mark price, when known.
    pub mark_price: Option<Decimal>,
    /// Leverage multiple (e.g. `5` = 5x), when known.
    pub leverage: Option<Decimal>,
    /// Unrealized PnL, when known.
    pub unrealized_pnl: Option<Money>,
    /// Accrued funding (negative = paid by this position), when known.
    pub funding: Option<Money>,
    /// Liquidation price, when known.
    pub liquidation_price: Option<Decimal>,
}

impl PerpPosition {
    /// A perpetual position in `market`, held `side`, of absolute `size` (no prices/risk yet).
    pub fn new(market: impl Into<String>, side: PerpSide, size: Decimal) -> Self {
        PerpPosition {
            market: market.into(),
            side,
            collateral: Vec::new(),
            size,
            entry_price: None,
            mark_price: None,
            leverage: None,
            unrealized_pnl: None,
            funding: None,
            liquidation_price: None,
        }
    }

    /// Set the collateral / margin (builder-style).
    #[must_use]
    pub fn with_collateral(mut self, collateral: Vec<Amount>) -> Self {
        self.collateral = collateral;
        self
    }

    /// Set the average entry price (builder-style).
    #[must_use]
    pub fn with_entry_price(mut self, entry_price: Option<Decimal>) -> Self {
        self.entry_price = entry_price;
        self
    }

    /// Set the current mark price (builder-style).
    #[must_use]
    pub fn with_mark_price(mut self, mark_price: Option<Decimal>) -> Self {
        self.mark_price = mark_price;
        self
    }

    /// Set the leverage multiple (builder-style).
    #[must_use]
    pub fn with_leverage(mut self, leverage: Option<Decimal>) -> Self {
        self.leverage = leverage;
        self
    }

    /// Set the unrealized PnL (builder-style).
    #[must_use]
    pub fn with_unrealized_pnl(mut self, unrealized_pnl: Option<Money>) -> Self {
        self.unrealized_pnl = unrealized_pnl;
        self
    }

    /// Set the accrued funding (builder-style).
    #[must_use]
    pub fn with_funding(mut self, funding: Option<Money>) -> Self {
        self.funding = funding;
        self
    }

    /// Set the liquidation price (builder-style).
    #[must_use]
    pub fn with_liquidation_price(mut self, liquidation_price: Option<Decimal>) -> Self {
        self.liquidation_price = liquidation_price;
        self
    }
}

/// A single normalized position within a protocol.
#[allow(
    clippy::large_enum_variant,
    reason = "variants kept inline rather than boxed: this is the public domain type matched everywhere downstream, and the size spread does not justify an API-breaking indirection"
)]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    /// A plain token balance held by the owner: a token idle in an on-chain wallet, or the
    /// cash/equity balance of a protocol account (e.g. the USD-margin equity of a perps account).
    /// Carries a value but no protocol-specific position mechanics.
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
    /// A perpetual / derivative position.
    Perp(PerpPosition),
    /// An NFT holding.
    Nft(NftPosition),
}

/// A protocol's normalized reading for one wallet on one chain, with provenance.
#[non_exhaustive]
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

impl Reading {
    /// A reading of `positions` for `protocol` on `chain` from `source`, with `provenance` and no
    /// receipt tokens. Attach receipt tokens with [`Reading::with_receipt_tokens`].
    pub fn new(
        protocol: Protocol,
        chain: Chain,
        source: Source,
        positions: Vec<Position>,
        provenance: Provenance,
    ) -> Self {
        Reading {
            protocol,
            chain,
            source,
            positions,
            receipt_tokens: Vec::new(),
            provenance,
        }
    }

    /// Set the receipt / wrapper token contracts (builder-style).
    #[must_use]
    pub fn with_receipt_tokens(mut self, receipt_tokens: Vec<Address>) -> Self {
        self.receipt_tokens = receipt_tokens;
        self
    }
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
    fn rewards_default_empty_and_set_via_builder() {
        let token = Token::new("AERO", None, 18);
        let reward = Amount::from_decimal(token, Decimal::from_str_exact("12.5").unwrap()).unwrap();

        assert!(StakePosition::new(vec![]).rewards.is_empty());
        assert_eq!(
            StakePosition::new(vec![])
                .with_rewards(vec![reward.clone()])
                .rewards
                .len(),
            1
        );
        assert_eq!(
            LendingPosition::new(vec![], vec![])
                .with_rewards(vec![reward])
                .rewards
                .len(),
            1
        );
    }

    #[test]
    fn stake_apy_defaults_none_and_set_via_builder() {
        // A source that can't supply the yield leaves it unset; never fabricated.
        assert!(StakePosition::new(vec![]).apy.is_none());
        assert_eq!(
            StakePosition::new(vec![])
                .with_apy(Some(Decimal::from_str_exact("0.0485").unwrap()))
                .apy,
            Some(Decimal::from_str_exact("0.0485").unwrap())
        );
    }

    #[test]
    fn market_id_defaults_none_and_set_via_builder() {
        // Cross-collateralized protocols (Aave, Kamino) never call the builder and must leave it
        // unset — never fabricated. Isolated-market protocols set the source market id.
        assert!(LendingPosition::new(vec![], vec![]).market_id.is_none());
        assert_eq!(
            LendingPosition::new(vec![], vec![])
                .with_market_id(Some("0x9103".into()))
                .market_id
                .as_deref(),
            Some("0x9103")
        );
    }

    #[test]
    fn perp_position_builder_keeps_unset_fields_none() {
        let perp = PerpPosition::new(
            "ETH-USD",
            PerpSide::Long,
            Decimal::from_str_exact("2").unwrap(),
        )
        .with_entry_price(Some(Decimal::from_str_exact("3000").unwrap()))
        .with_liquidation_price(Some(Decimal::from_str_exact("2400").unwrap()));

        assert_eq!(perp.market, "ETH-USD");
        assert_eq!(perp.side, PerpSide::Long);
        assert_eq!(perp.size, Decimal::from_str_exact("2").unwrap());
        assert_eq!(
            perp.entry_price,
            Some(Decimal::from_str_exact("3000").unwrap())
        );
        // A field the source never provided stays None — never fabricated.
        assert!(perp.mark_price.is_none());
        assert!(perp.collateral.is_empty());

        assert!(matches!(Position::Perp(perp), Position::Perp(_)));
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
