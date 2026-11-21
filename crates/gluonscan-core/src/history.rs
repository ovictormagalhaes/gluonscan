//! Historical protocol events — the read + normalize surface for a wallet's past actions.
//!
//! This is the raw, normalized event stream. Storage, valuation, and derived analytics (PnL,
//! impermanent loss, cost basis) are the consumer's job — the engine only reads and normalizes.

use rust_decimal::Decimal;

use crate::{Amount, Chain, Money, PerpSide, Protocol, Provenance, Timestamp};

/// A normalized protocol event kind.
///
/// [`Deposit`](Self::Deposit), [`Withdraw`](Self::Withdraw), [`Borrow`](Self::Borrow),
/// [`Repay`](Self::Repay), [`Liquidation`](Self::Liquidation) and
/// [`CollectFees`](Self::CollectFees) are owner actions (or a liquidation of the owner);
/// [`SupplyInterest`](Self::SupplyInterest) and [`BorrowInterest`](Self::BorrowInterest) are
/// interest accrual, not actions. [`PerpOpen`](Self::PerpOpen), [`PerpClose`](Self::PerpClose)
/// and [`PerpFlip`](Self::PerpFlip) are perpetual fills; they, and a
/// [`Liquidation`](Self::Liquidation) of a perp position, carry a [`PerpFill`] in
/// [`HistoryEvent::perp`].
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
    /// Position was liquidated. On a perp position ([`HistoryEvent::perp`] set) the amount is the
    /// size the liquidation closed.
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
    /// A perpetual fill that opened or increased a position. Carries a [`PerpFill`].
    PerpOpen,
    /// A perpetual fill that reduced or closed a position. Carries a [`PerpFill`].
    PerpClose,
    /// A single perpetual fill that closed a position and opened the opposite side. Carries a
    /// [`PerpFill`]; its [`side`](PerpFill::side) is the side it opened.
    PerpFlip,
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

/// The trade detail of one perpetual fill. Prices, PnL and fees are as the venue reports them; a
/// figure the venue does not state stays `None`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpFill {
    /// The market traded, e.g. `"BTC"`.
    pub market: String,
    /// The side of the position this fill opened or added to ([`EventKind::PerpOpen`],
    /// [`EventKind::PerpFlip`]) or reduced ([`EventKind::PerpClose`], [`EventKind::Liquidation`]).
    pub side: PerpSide,
    /// Fill price.
    pub price: Decimal,
    /// Signed position size before the fill (positive long, negative short, zero flat).
    pub start_size: Decimal,
    /// PnL the fill realized on the part of the position it closed, before fees. Zero on a fill
    /// that only opens.
    pub realized_pnl: Option<Money>,
    /// The fee the fill paid (negative = a rebate received), when the fee token is USD-valued.
    pub fee: Option<Money>,
    /// The venue's unique identifier for this fill (several fills can share one transaction).
    pub fill_id: String,
}

impl PerpFill {
    /// A fill on `market` for the position `side`, at `price`, from signed `start_size`.
    pub fn new(
        market: impl Into<String>,
        side: PerpSide,
        price: Decimal,
        start_size: Decimal,
        fill_id: impl Into<String>,
    ) -> Self {
        PerpFill {
            market: market.into(),
            side,
            price,
            start_size,
            realized_pnl: None,
            fee: None,
            fill_id: fill_id.into(),
        }
    }

    /// Attach the realized PnL (builder-style).
    #[must_use]
    pub fn with_realized_pnl(mut self, realized_pnl: Option<Money>) -> Self {
        self.realized_pnl = realized_pnl;
        self
    }

    /// Attach the fee (builder-style).
    #[must_use]
    pub fn with_fee(mut self, fee: Option<Money>) -> Self {
        self.fee = fee;
        self
    }
}

/// One historical event: what happened, to how much of which token, and when.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    /// What happened.
    pub kind: EventKind,
    /// The token and quantity involved (USD left `None` — valuation is the consumer's). On a perp
    /// fill ([`perp`](Self::perp) set) it is the fill's size in the market's base asset, under an
    /// address-less token named after the market: a quantity traded, not a token transfer.
    pub amount: Amount,
    /// The transaction identifier (hash / signature). Several events can share one (fills of one
    /// order); a perp fill's own identity is [`PerpFill::fill_id`].
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
    /// The trade detail of a perpetual fill ([`EventKind::PerpOpen`], [`EventKind::PerpClose`],
    /// [`EventKind::PerpFlip`], or a [`EventKind::Liquidation`] of a perp position). `None` on
    /// every other event.
    pub perp: Option<PerpFill>,
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
            perp: None,
        }
    }

    /// Attach a perpetual fill's trade detail (builder-style). See [`perp`](Self::perp).
    #[must_use]
    pub fn with_perp(mut self, perp: Option<PerpFill>) -> Self {
        self.perp = perp;
        self
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
    /// The source serves only its most recent events and this history reached that limit, so
    /// events before the source's retention boundary may exist but are not in
    /// [`events`](Self::events).
    pub earlier_events_unavailable: bool,
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
            earlier_events_unavailable: false,
        }
    }

    /// Mark the history as cut at the source's retention limit (builder-style). See
    /// [`earlier_events_unavailable`](Self::earlier_events_unavailable).
    #[must_use]
    pub fn with_earlier_events_unavailable(mut self) -> Self {
        self.earlier_events_unavailable = true;
        self
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
