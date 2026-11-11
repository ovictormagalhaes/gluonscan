//! Interest and yield rates, each tagged with its compounding basis, averaging window, source and
//! timestamp.
//!
//! A rate without these tags is ambiguous: an instantaneous borrow APY sampled during a utilization
//! spike, a 7-day mean and a publisher's daily APR are different numbers that all read as "the
//! rate". [`Rates`] holds several of them side by side so the consumer picks the one it means.

use crate::{Source, Timestamp};
use rust_decimal::Decimal;

/// How a rate compounds. An APR and an APY are never converted into each other here: the
/// conversion needs a compounding frequency the source does not state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RateBasis {
    /// Annual percentage yield (compounded).
    Apy,
    /// Annual percentage rate (simple, not compounded).
    Apr,
}

/// The period a rate describes.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RateWindow {
    /// The value at read time (or at the source's last refresh).
    Instant,
    /// The arithmetic mean over the trailing `hours`, ending at [`Rate::as_of`].
    Trailing {
        /// Window length in hours.
        hours: u32,
    },
    /// The publisher's last reported period (an oracle report, a daily data point). `hours` is the
    /// period length when the publisher states or implies it.
    Epoch {
        /// Period length in hours, when known.
        hours: Option<u32>,
    },
    /// Market-implied to maturity (e.g. a Pendle PT's implied APY).
    Implied,
}

/// One rate as a FRACTION (`0.031` = 3.1%), with its basis, window, origin and timestamp.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rate {
    /// The rate as a fraction.
    pub value: Decimal,
    /// How it compounds.
    pub basis: RateBasis,
    /// The period it describes.
    pub window: RateWindow,
    /// The backend that produced it (independent of the reading's own source).
    pub source: Source,
    /// When the value was valid: the upstream timestamp when the source reports one, else the read
    /// time.
    pub as_of: Timestamp,
}

impl Rate {
    /// A rate of `value` (fraction) on `basis` over `window`, from `source`, valid at `as_of`.
    pub fn new(
        value: Decimal,
        basis: RateBasis,
        window: RateWindow,
        source: Source,
        as_of: Timestamp,
    ) -> Self {
        Rate {
            value,
            basis,
            window,
            source,
            as_of,
        }
    }
}

/// Why an adapter that computes a rate for a window could not emit it. A retryable gap and a real
/// one read the same as an absent rate, so the adapter records which it is.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RateGap {
    /// No usable data reached the adapter: a transport failure that outlasted its retries, a body
    /// that does not parse, or a source it could not request at all. A later read may fill it when
    /// the cause was transient.
    Unavailable,
    /// The source answered, but its points cover too little of the window.
    InsufficientCoverage,
    /// The source answered, but its newest point is too old, or dated after the read.
    StalePoint,
}

/// A set of [`Rate`]s with at most one per `(basis, window)`, ordered by basis then window. Empty
/// means the source reported no rate (never a fabricated zero). It also holds the [`RateGap`] of
/// each window an adapter tried and failed to rate, keyed by window alone (not by basis); a window
/// never has both a rate and a gap. Iterating and collecting carry the rates only, not the gaps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rates {
    rates: Vec<Rate>,
    gaps: Vec<(RateWindow, RateGap)>,
}

impl Rates {
    /// An empty set.
    pub fn new() -> Self {
        Rates::default()
    }

    /// Insert `rate`, replacing and returning any rate already held for the same
    /// `(basis, window)`, and clearing any gap recorded for its window. The set stays ordered
    /// regardless of insertion order.
    pub fn insert(&mut self, rate: Rate) -> Option<Rate> {
        self.gaps.retain(|(w, _)| *w != rate.window);
        let key = (rate.basis, rate.window);
        match self
            .rates
            .binary_search_by(|r| (r.basis, r.window).cmp(&key))
        {
            Ok(i) => Some(std::mem::replace(&mut self.rates[i], rate)),
            Err(i) => {
                self.rates.insert(i, rate);
                None
            }
        }
    }

    /// Record why `window` has no rate, replacing any gap already recorded for it. Ignored, returning
    /// `false`, when the set already holds a rate for `window`.
    pub fn insert_gap(&mut self, window: RateWindow, gap: RateGap) -> bool {
        if self.rates.iter().any(|r| r.window == window) {
            return false;
        }
        match self.gaps.binary_search_by(|(w, _)| w.cmp(&window)) {
            Ok(i) => self.gaps[i].1 = gap,
            Err(i) => self.gaps.insert(i, (window, gap)),
        }
        true
    }

    /// Record a gap (builder-style). See [`Rates::insert_gap`].
    #[must_use]
    pub fn with_gap(mut self, window: RateWindow, gap: RateGap) -> Self {
        self.insert_gap(window, gap);
        self
    }

    /// Why each window the adapter tried to rate has no rate, ordered by window.
    pub fn gaps(&self) -> &[(RateWindow, RateGap)] {
        &self.gaps
    }

    /// The gap recorded for `window`, if any.
    pub fn gap(&self, window: RateWindow) -> Option<RateGap> {
        self.gaps
            .binary_search_by(|(w, _)| w.cmp(&window))
            .ok()
            .map(|i| self.gaps[i].1)
    }

    /// Insert `rate` (builder-style). See [`Rates::insert`].
    #[must_use]
    pub fn with(mut self, rate: Rate) -> Self {
        self.insert(rate);
        self
    }

    /// The rate for `(basis, window)`, if present. No other window is substituted.
    pub fn get(&self, basis: RateBasis, window: RateWindow) -> Option<&Rate> {
        self.rates
            .binary_search_by(|r| (r.basis, r.window).cmp(&(basis, window)))
            .ok()
            .map(|i| &self.rates[i])
    }

    /// The rates, ordered by basis then window.
    pub fn iter(&self) -> std::slice::Iter<'_, Rate> {
        self.rates.iter()
    }

    /// Whether no rate is present. Gaps are not counted.
    pub fn is_empty(&self) -> bool {
        self.rates.is_empty()
    }

    /// How many rates are present. Gaps are not counted.
    pub fn len(&self) -> usize {
        self.rates.len()
    }
}

impl FromIterator<Rate> for Rates {
    /// Collect rates; a later rate replaces an earlier one with the same `(basis, window)`.
    fn from_iter<I: IntoIterator<Item = Rate>>(iter: I) -> Self {
        let mut rates = Rates::new();
        for rate in iter {
            rates.insert(rate);
        }
        rates
    }
}

impl Extend<Rate> for Rates {
    fn extend<I: IntoIterator<Item = Rate>>(&mut self, iter: I) {
        for rate in iter {
            self.insert(rate);
        }
    }
}

impl<'a> IntoIterator for &'a Rates {
    type Item = &'a Rate;
    type IntoIter = std::slice::Iter<'a, Rate>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for Rates {
    type Item = Rate;
    type IntoIter = std::vec::IntoIter<Rate>;

    fn into_iter(self) -> Self::IntoIter {
        self.rates.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(value: &str, basis: RateBasis, window: RateWindow) -> Rate {
        Rate::new(
            Decimal::from_str_exact(value).unwrap(),
            basis,
            window,
            Source::Api,
            Timestamp(1),
        )
    }

    #[test]
    fn duplicate_key_replaces_and_returns_the_old_rate() {
        let mut rates = Rates::new();
        assert!(rates
            .insert(rate("0.39", RateBasis::Apy, RateWindow::Instant))
            .is_none());
        let old = rates.insert(rate("0.07", RateBasis::Apy, RateWindow::Instant));
        assert_eq!(old.unwrap().value, Decimal::from_str_exact("0.39").unwrap());
        assert_eq!(rates.len(), 1);
        assert_eq!(
            rates
                .get(RateBasis::Apy, RateWindow::Instant)
                .unwrap()
                .value,
            Decimal::from_str_exact("0.07").unwrap()
        );
    }

    #[test]
    fn same_window_on_another_basis_is_a_distinct_rate() {
        let rates = Rates::new()
            .with(rate("0.022", RateBasis::Apr, RateWindow::Instant))
            .with(rate("0.023", RateBasis::Apy, RateWindow::Instant));
        assert_eq!(rates.len(), 2);
        assert!(rates.get(RateBasis::Apr, RateWindow::Instant).is_some());
        assert!(rates.get(RateBasis::Apy, RateWindow::Instant).is_some());
    }

    #[test]
    fn get_never_substitutes_another_window() {
        let rates = Rates::new().with(rate("0.07", RateBasis::Apy, RateWindow::Instant));
        assert!(rates
            .get(RateBasis::Apy, RateWindow::Trailing { hours: 24 })
            .is_none());
        assert!(rates.get(RateBasis::Apr, RateWindow::Instant).is_none());
        assert!(rates
            .get(RateBasis::Apy, RateWindow::Epoch { hours: None })
            .is_none());
        assert!(Rates::new().is_empty());
    }

    #[test]
    fn ordering_is_by_basis_then_window_whatever_the_insertion_order() {
        let a = Rates::from_iter([
            rate("1", RateBasis::Apr, RateWindow::Epoch { hours: Some(24) }),
            rate("2", RateBasis::Apy, RateWindow::Implied),
            rate("3", RateBasis::Apy, RateWindow::Trailing { hours: 168 }),
            rate("4", RateBasis::Apy, RateWindow::Instant),
            rate("5", RateBasis::Apy, RateWindow::Trailing { hours: 24 }),
            rate("6", RateBasis::Apr, RateWindow::Trailing { hours: 168 }),
        ]);
        let b: Rates = a.iter().rev().cloned().collect();
        assert_eq!(a, b);
        let keys: Vec<_> = a.iter().map(|r| (r.basis, r.window)).collect();
        assert_eq!(
            keys,
            vec![
                (RateBasis::Apy, RateWindow::Instant),
                (RateBasis::Apy, RateWindow::Trailing { hours: 24 }),
                (RateBasis::Apy, RateWindow::Trailing { hours: 168 }),
                (RateBasis::Apy, RateWindow::Implied),
                (RateBasis::Apr, RateWindow::Trailing { hours: 168 }),
                (RateBasis::Apr, RateWindow::Epoch { hours: Some(24) }),
            ]
        );
    }

    #[test]
    fn collecting_keeps_the_last_duplicate() {
        let rates: Rates = [
            rate("0.1", RateBasis::Apy, RateWindow::Instant),
            rate("0.2", RateBasis::Apy, RateWindow::Instant),
        ]
        .into_iter()
        .collect();
        assert_eq!(rates.len(), 1);
        assert_eq!(
            rates.iter().next().unwrap().value,
            Decimal::from_str_exact("0.2").unwrap()
        );
    }

    #[test]
    fn a_gap_is_kept_per_window_and_never_beside_a_rate() {
        let w24 = RateWindow::Trailing { hours: 24 };
        let w168 = RateWindow::Trailing { hours: 168 };
        let mut rates = Rates::new()
            .with_gap(w168, RateGap::StalePoint)
            .with_gap(w24, RateGap::Unavailable);
        assert!(rates.is_empty(), "gaps are not rates");
        assert_eq!(
            rates.gaps(),
            &[(w24, RateGap::Unavailable), (w168, RateGap::StalePoint)]
        );

        assert!(rates.insert_gap(w24, RateGap::InsufficientCoverage));
        assert_eq!(rates.gap(w24), Some(RateGap::InsufficientCoverage));
        assert_eq!(rates.gaps().len(), 2);

        rates.insert(rate("0.05", RateBasis::Apy, w24));
        assert_eq!(rates.gap(w24), None, "a rate clears its window's gap");
        assert!(
            !rates.insert_gap(w24, RateGap::Unavailable),
            "a rated window takes no gap"
        );
        assert_eq!(rates.gap(w24), None);
        assert_eq!(rates.gaps(), &[(w168, RateGap::StalePoint)]);
        assert_eq!(rates.gap(RateWindow::Instant), None);
    }
}
