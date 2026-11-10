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

/// What a fee collect's amount holds. A concentrated-liquidity collect (Uniswap V3 `collect`) sends
/// out everything the position is owed: its fees plus any principal a liquidity decrease released
/// and nothing has collected yet. Whether a reported amount still carries that principal depends on
/// where it was read, so every collect event and every collected total says which.
///
/// A consumer deriving realized fees must net principal out of an
/// [`IncludesPrincipal`](Self::IncludesPrincipal) amount and must not derive fees from an
/// [`Unknown`](Self::Unknown) one. Reading either as fees reports principal as profit.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CollectBasis {
    /// Fees only: no principal is in the amount.
    FeesOnly,
    /// Everything the collect sent out: fees plus whatever principal released by a liquidity
    /// decrease it withdrew (possibly none). Net the principal out, per token, against the
    /// position's withdrawals.
    IncludesPrincipal,
    /// Not known whether the amount includes principal. The default, so a source that does not
    /// state a basis never reads as fees-only.
    #[default]
    Unknown,
}

impl CollectBasis {
    /// The basis of a sum of amounts with bases `self` and `other`: unknown if either is, else
    /// including principal if either does, else fees only. A total is never fees-only unless every
    /// part of it is.
    #[must_use]
    pub fn join(self, other: CollectBasis) -> CollectBasis {
        use CollectBasis::{FeesOnly, IncludesPrincipal, Unknown};
        match (self, other) {
            (Unknown, _) | (_, Unknown) => Unknown,
            (IncludesPrincipal, _) | (_, IncludesPrincipal) => IncludesPrincipal,
            (FeesOnly, FeesOnly) => FeesOnly,
        }
    }
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
    /// For an [`EventKind::CollectFees`] event: whether [`amount`](Self::amount) is fees only or
    /// may include principal. [`CollectBasis::Unknown`] unless the source states it, and on every
    /// other kind.
    pub collect_basis: CollectBasis,
    /// For a collect that includes principal, read from the transaction itself: the principal, in
    /// this event's token, that a liquidity decrease **in the same transaction** released. When the
    /// collect sent out everything owed (what wallets and interfaces do) this is the principal part
    /// of [`amount`](Self::amount); principal that a decrease in an earlier transaction released and
    /// this collect withdrew is not in it (net that against the earlier
    /// [`Withdraw`](EventKind::Withdraw)). `None` when the source cannot tell, and on every event
    /// that is not such a collect.
    pub released_principal: Option<Amount>,
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
            collect_basis: CollectBasis::Unknown,
            released_principal: None,
        }
    }

    /// Attach the block height (builder-style).
    #[must_use]
    pub fn with_block(mut self, block: Option<u64>) -> Self {
        self.block = block;
        self
    }

    /// State whether a collect's amount is fees only or may include principal (builder-style).
    #[must_use]
    pub fn with_collect_basis(mut self, basis: CollectBasis) -> Self {
        self.collect_basis = basis;
        self
    }

    /// Attach the principal the same transaction released, in this event's token (builder-style).
    /// See [`released_principal`](Self::released_principal).
    #[must_use]
    pub fn with_released_principal(mut self, principal: Option<Amount>) -> Self {
        self.released_principal = principal;
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

    /// The basis of this history's collected fees taken together: [`CollectBasis::Unknown`] when
    /// they are [unavailable](Self::collected_fees_unavailable), else the
    /// [`join`](CollectBasis::join) of every [`EventKind::CollectFees`] event's basis (fees only
    /// when there is none, since an empty sum holds no principal).
    pub fn collected_fees_basis(&self) -> CollectBasis {
        if self.collected_fees_unavailable {
            return CollectBasis::Unknown;
        }
        self.events
            .iter()
            .filter(|e| e.kind == EventKind::CollectFees)
            .fold(CollectBasis::FeesOnly, |acc, e| acc.join(e.collect_basis))
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

    fn history(events: Vec<HistoryEvent>) -> History {
        History::new(
            Protocol::UniswapV3,
            Chain::Ethereum,
            events,
            Provenance::new(
                Source::Subgraph,
                Chain::Ethereum,
                Timestamp(1),
                Staleness::Live,
            ),
        )
    }

    fn one_weth() -> Amount {
        Amount::from_decimal(Token::new("WETH", None, 18), Decimal::ONE).unwrap()
    }

    fn collect(basis: CollectBasis) -> HistoryEvent {
        HistoryEvent::new(EventKind::CollectFees, one_weth(), "0x1", Timestamp(1))
            .with_collect_basis(basis)
    }

    #[test]
    fn a_total_is_fees_only_only_when_every_part_is() {
        use CollectBasis::{FeesOnly, IncludesPrincipal, Unknown};
        let all = [FeesOnly, IncludesPrincipal, Unknown];
        for a in all {
            for b in all {
                let expected = if a == Unknown || b == Unknown {
                    Unknown
                } else if a == IncludesPrincipal || b == IncludesPrincipal {
                    IncludesPrincipal
                } else {
                    FeesOnly
                };
                assert_eq!(a.join(b), expected, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn an_event_states_no_basis_until_its_source_does() {
        let e = HistoryEvent::new(EventKind::CollectFees, one_weth(), "0x1", Timestamp(1));
        assert_eq!(e.collect_basis, CollectBasis::Unknown);
        assert_eq!(e.released_principal, None);
        assert_eq!(CollectBasis::default(), CollectBasis::Unknown);
        let e = e
            .with_collect_basis(CollectBasis::IncludesPrincipal)
            .with_released_principal(Some(one_weth()));
        assert_eq!(e.collect_basis, CollectBasis::IncludesPrincipal);
        assert_eq!(e.released_principal, Some(one_weth()));
    }

    #[test]
    fn a_mixed_history_never_claims_fees_only() {
        let h = history(vec![
            collect(CollectBasis::FeesOnly),
            collect(CollectBasis::IncludesPrincipal),
        ]);
        assert_eq!(h.collected_fees_basis(), CollectBasis::IncludesPrincipal);

        let h = history(vec![
            collect(CollectBasis::FeesOnly),
            collect(CollectBasis::IncludesPrincipal),
            collect(CollectBasis::Unknown),
        ]);
        assert_eq!(h.collected_fees_basis(), CollectBasis::Unknown);

        let h = history(vec![
            collect(CollectBasis::FeesOnly),
            collect(CollectBasis::FeesOnly),
        ]);
        assert_eq!(h.collected_fees_basis(), CollectBasis::FeesOnly);
    }

    #[test]
    fn only_collects_count_toward_the_basis() {
        let withdraw = HistoryEvent::new(EventKind::Withdraw, one_weth(), "0x1", Timestamp(1));
        let h = history(vec![withdraw, collect(CollectBasis::FeesOnly)]);
        assert_eq!(h.collected_fees_basis(), CollectBasis::FeesOnly);
        assert_eq!(
            history(Vec::new()).collected_fees_basis(),
            CollectBasis::FeesOnly
        );
    }

    #[test]
    fn unavailable_collects_have_an_unknown_basis() {
        let h = history(vec![collect(CollectBasis::FeesOnly)]).with_collected_fees_unavailable();
        assert_eq!(h.collected_fees_basis(), CollectBasis::Unknown);
    }
}
