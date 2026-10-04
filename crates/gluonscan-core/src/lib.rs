//! # gluonscan-core
//!
//! The read + normalize **contract** for gluonscan: domain types, the adapter ports (traits),
//! the integrity wrapper [`Complete`], the typed [`Error`], and the taxonomy enums
//! ([`Chain`], [`Protocol`], [`Source`], [`Capability`], [`Detail`]).
//!
//! This crate does **no I/O**: no HTTP, no RPC, no runtime. Everything that touches the network
//! is injected through a port (see [`ports`]). The types a fetch returns ARE the contract — there
//! is no wire/DTO layer here; consumers map these types to whatever they need.
//!
//! The public API exposes [`alloy_primitives`] types ([`Address`], [`U256`]); it is re-exported so
//! consumers can name the exact version this crate builds against and avoid a version mismatch.
//!
//! ## Invariants
//! - **Complete or error.** An adapter wraps a value in [`Complete<T>`](Complete) only after every
//!   required source succeeded — the convention that keeps partial data out of a reading.
//! - **No fabricated value.** A missing price is [`Error::AbsentPrice`], never `0` or `1`.

pub mod error;
pub mod history;
pub mod hygiene;
pub mod model;
pub mod ports;

mod chain;
pub use chain::{Chain, Ecosystem};
pub use error::Error;
pub use history::{EventKind, History, HistoryEvent};
pub use model::{
    scaled, to_raw, Amount, BorrowedAsset, Complete, Currency, LendingPosition, LiquidityPosition,
    LockPosition, Money, NftPosition, PerpPosition, PerpSide, Position, PositionStatus, Provenance,
    Reading, StakePosition, Staleness, SuppliedAsset, Timestamp, Token, TokenAddress,
    WalletBalance, YieldKind, YieldPosition,
};
pub use ports::{ChainProvider, Clock, Ctx, Http, PriceSource, ProtocolAdapter};

pub use alloy_primitives::{self, Address, U256};

/// A wallet identifier across ecosystems.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Wallet {
    /// An EVM address.
    Evm(Address),
    /// A Solana base58 public key.
    Solana(String),
    /// A Bitcoin address.
    Bitcoin(String),
}

impl Wallet {
    /// The EVM address, or a permanent error if this is not an EVM wallet.
    pub fn evm(&self) -> Result<Address, Error> {
        match self {
            Wallet::Evm(a) => Ok(*a),
            _ => Err(Error::Permanent {
                message: "expected an EVM wallet".into(),
            }),
        }
    }

    /// The Solana base58 public key, or a permanent error if this is not a Solana wallet.
    pub fn solana(&self) -> Result<&str, Error> {
        match self {
            Wallet::Solana(s) => Ok(s),
            _ => Err(Error::Permanent {
                message: "expected a Solana wallet".into(),
            }),
        }
    }

    /// The Bitcoin address, or a permanent error if this is not a Bitcoin wallet.
    pub fn bitcoin(&self) -> Result<&str, Error> {
        match self {
            Wallet::Bitcoin(s) => Ok(s),
            _ => Err(Error::Permanent {
                message: "expected a Bitcoin wallet".into(),
            }),
        }
    }
}

/// A priceable asset key, chain-agnostic — unlike a bare EVM [`Address`], it can name a chain's
/// native coin or a Solana SPL mint, so BTC and SPL balances are priceable too.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Asset {
    /// The chain's native coin (BTC on Bitcoin, ETH on Ethereum/L2s, SOL on Solana).
    Native,
    /// An EVM token contract.
    Token(Address),
    /// A Solana SPL mint (base58).
    Mint(String),
}

impl Asset {
    /// A short, human-readable key for diagnostics and [`Error::AbsentPrice`].
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Asset::Native => "native".to_string(),
            Asset::Token(a) => format!("{a:#x}"),
            Asset::Mint(m) => m.clone(),
        }
    }
}

/// A supported DeFi protocol. Identity only — a protocol may have several backend
/// implementations (see [`Source`]); the user picks and configures which.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Aave V3 lending.
    AaveV3,
    /// Uniswap V3 concentrated liquidity.
    UniswapV3,
    /// Pendle yield tokens.
    Pendle,
    /// Raydium CLMM (Solana).
    Raydium,
    /// Kamino lending/liquidity (Solana).
    Kamino,
    /// Lido liquid staking (stETH / wstETH).
    Lido,
    /// Morpho Blue lending.
    Morpho,
    /// ether.fi liquid restaking (weETH / eETH).
    EtherFi,
    /// Ethena staked USDe (sUSDe).
    Ethena,
    /// Idle wallet token balances (not a protocol; a capability).
    Wallet,
    /// The chain's native coin balance (ETH, BNB, SOL, BTC, ...). Separate from [`Protocol::Wallet`]
    /// so a single engine can route a native-balance read and a token-balance read independently on
    /// the same chain (both would otherwise share one protocol and the engine would pick the first).
    Native,
    /// Wallet NFTs.
    Nfts,
}

/// The backend a reading came from. A protocol can expose the same [`Capability`] through more
/// than one source; capabilities are routed per source and each is configured independently.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// A protocol's own HTTP API.
    Api,
    /// A subgraph (The Graph or equivalent).
    Subgraph,
    /// Direct on-chain reads (JSON-RPC / account decoding).
    OnChain,
}

/// A unit of data an adapter can fetch. Capability is the addressing unit for routing: the union
/// of a protocol's backends' capabilities is its coverage; an unsupported one yields
/// [`Error::Unsupported`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Current positions (balances / supplies / liquidity).
    Positions,
    /// Historical events (deposits, withdrawals, borrows, repays, collects).
    History,
    /// Uncollected / claimable fees.
    Fees,
    /// Account health factor.
    HealthFactor,
    /// Per-asset risk parameters (LTV, liquidation threshold).
    RiskConfig,
    /// Claimable protocol rewards / incentives (emission tokens), distinct from LP trading fees.
    Rewards,
}

/// The **minimum** detail a caller requests, which is also the cost ceiling it accepts. An adapter
/// runs the cheapest fetch plan that satisfies it; receiving richer-than-requested (when free) is
/// fine, doing extra work is not.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Detail {
    /// Does the wallet hold anything in this protocol?
    Presence,
    /// What is there, at coarse resolution.
    Summary,
    /// Everything: exact amounts, fees, ranges, health factor.
    Full,
}
