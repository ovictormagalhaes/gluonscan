//! Historical protocol events — the read + normalize surface for a wallet's past actions.
//!
//! This is the raw, normalized event stream. Storage, valuation, and derived analytics (PnL,
//! impermanent loss, cost basis) are the consumer's job — the engine only reads and normalizes.

use crate::{Amount, Chain, Protocol, Provenance, Timestamp};

/// A normalized protocol event kind.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Supplied, added liquidity, or deposited.
    Deposit,
    /// Withdrew, removed liquidity, or redeemed.
    Withdraw,
    /// Borrowed.
    Borrow,
    /// Repaid debt.
    Repay,
    /// Position was liquidated.
    Liquidation,
    /// Collected (claimed) fees.
    CollectFees,
}

/// One historical event: what happened, to how much of which token, and when.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// What happened.
    pub kind: EventKind,
    /// The token and quantity involved (USD left `None` — valuation is the consumer's).
    pub amount: Amount,
    /// The transaction identifier (hash / signature).
    pub tx: String,
    /// When it happened.
    pub at: Timestamp,
    /// Block height, when the source provides it.
    pub block: Option<u64>,
}

impl HistoryEvent {
    /// An event of `kind` moving `amount` in transaction `tx` at time `at` (no block yet).
    pub fn new(kind: EventKind, amount: Amount, tx: impl Into<String>, at: Timestamp) -> Self {
        HistoryEvent {
            kind,
            amount,
            tx: tx.into(),
            at,
            block: None,
        }
    }

    /// Attach the block height (builder-style).
    #[must_use]
    pub fn with_block(mut self, block: Option<u64>) -> Self {
        self.block = block;
        self
    }
}

/// A protocol's normalized event history for one wallet on one chain, with provenance.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// Which protocol.
    pub protocol: Protocol,
    /// Which chain.
    pub chain: Chain,
    /// The events, oldest first.
    pub events: Vec<HistoryEvent>,
    /// Where/when this history came from.
    pub provenance: Provenance,
}

impl History {
    /// A history of `events` for `protocol` on `chain`, with `provenance`.
    pub fn new(
        protocol: Protocol,
        chain: Chain,
        events: Vec<HistoryEvent>,
        provenance: Provenance,
    ) -> Self {
        History {
            protocol,
            chain,
            events,
            provenance,
        }
    }
}
