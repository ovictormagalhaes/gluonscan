//! # gluonscan-core
//!
//! The read + normalize **contract** for gluonscan: domain types, the adapter ports (traits),
//! the integrity wrapper [`Complete`], the typed [`Error`], and the taxonomy enums
//! ([`Chain`], [`Protocol`], [`Source`], [`Capability`], [`Detail`], [`Execution`]).
//!
//! This crate does **no I/O**: no HTTP, no RPC, no runtime. Everything that touches the network
//! is injected through a port (see [`ports`]). The types a fetch returns ARE the contract — there
//! is no wire/DTO layer here; consumers map these types to whatever they need.
//!
//! ## Invariants
//! - **Complete or error.** A fetch returns [`Complete<T>`](Complete) only when every required
//!   source succeeded. There is no way to wrap a partial value.
//! - **No fabricated value.** A missing price is [`Error::AbsentPrice`], never `0` or `1`.

pub mod error;
pub mod model;
pub mod ports;

mod chain;
pub use chain::{Chain, Ecosystem};
pub use error::Error;
pub use model::{
    scaled, Amount, Complete, Currency, LendingPosition, LiquidityPosition, LockPosition, Money,
    NftPosition, Position, Provenance, Reading, Staleness, Timestamp, Token, WalletBalance,
    YieldKind, YieldPosition,
};
pub use ports::{ChainProvider, Clock, Ctx, Http, PriceSource, ProtocolAdapter};

pub use alloy_primitives::Address;

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
    /// Idle wallet token balances (not a protocol; a capability).
    Wallet,
    /// Wallet NFT holdings.
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

/// How the engine schedules the fetch work. This — and nothing else — is what separates a strictly
/// sequential caller from a massively parallel one; the same adapters serve both.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Execution {
    /// One fetch in flight at a time, in order.
    #[default]
    Sequential,
    /// Bounded fan-out; still capped by each provider's own rate limiter.
    Parallel {
        /// Maximum concurrent in-flight fetches.
        max_inflight: usize,
    },
}
