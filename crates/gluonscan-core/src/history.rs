//! Historical protocol events — the read + normalize surface for a wallet's past actions.
//!
//! This is the raw, normalized event stream. Storage, valuation, and derived analytics (PnL,
//! impermanent loss, cost basis) are the consumer's job — the engine only reads and normalizes.

use crate::{Amount, Chain, Protocol, Provenance, Timestamp};

/// A normalized protocol event kind.
///
/// [`Deposit`](Self::Deposit), [`Withdraw`](Self::Withdraw), [`Borrow`](Self::Borrow),
/// [`Repay`](Self::Repay), [`Liquidation`](Self::Liquidation) and
/// [`CollectFees`](Self::CollectFees) are owner actions (or a liquidation of the owner);
/// [`SupplyInterest`](Self::SupplyInterest) and [`BorrowInterest`](Self::BorrowInterest) are
/// interest accrual, not actions.
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
    /// A supplied balance grew by the interest it earned between two readings. Not an action
    /// of the owner: no tokens moved in or out, the amount is the growth of the supplied balance.
    /// Only emitted by sources whose history is derived from balance readings (Kamino), never
    /// alongside a [`Deposit`](Self::Deposit) of the same token at the same time.
    SupplyInterest,
    /// A borrowed balance grew by the interest owed between two readings. Not an action of the
    /// owner: nothing was borrowed, the amount is the growth of the debt. Only emitted by sources
    /// whose history is derived from balance readings (Kamino), never alongside a
    /// [`Borrow`](Self::Borrow) of the same token at the same time.
    BorrowInterest,
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
    /// The source's fee-collection history failed an integrity check, so no
    /// [`EventKind::CollectFees`] events are emitted. Collected fees over this history are
    /// **unknown** (not zero); the other event kinds are unaffected.
    pub collected_fees_unavailable: bool,
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
            collected_fees_unavailable: false,
        }
    }

    /// Mark the fee-collection history as unknown (builder-style): drops every
    /// [`EventKind::CollectFees`] event and sets
    /// [`collected_fees_unavailable`](Self::collected_fees_unavailable).
    #[must_use]
    pub fn with_collected_fees_unavailable(mut self) -> Self {
        self.events.retain(|e| e.kind != EventKind::CollectFees);
        self.collected_fees_unavailable = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Source, Staleness, Token};
    use rust_decimal::Decimal;

    #[test]
    fn collected_fees_unavailable_withholds_only_collects() {
        let token = Token::new("WETH", None, 18);
        let amount = Amount::from_decimal(token, Decimal::ONE).unwrap();
        let ev = |kind| HistoryEvent::new(kind, amount.clone(), "0x1", Timestamp(1));
        let h = History::new(
            Protocol::UniswapV3,
            Chain::Ethereum,
            vec![ev(EventKind::Deposit), ev(EventKind::CollectFees)],
            Provenance::new(
                Source::Subgraph,
                Chain::Ethereum,
                Timestamp(1),
                Staleness::Live,
            ),
        );
        assert!(!h.collected_fees_unavailable);
        let h = h.with_collected_fees_unavailable();
        assert!(h.collected_fees_unavailable);
        assert_eq!(h.events.len(), 1);
        assert_eq!(h.events[0].kind, EventKind::Deposit);
    }
}
