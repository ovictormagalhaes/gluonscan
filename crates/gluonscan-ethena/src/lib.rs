//! # gluonscan-ethena
//!
//! Ethena staked-USDe adapter (Ethereum). A wallet's Ethena staking position *is* its **sUSDe**
//! balance — the ERC-4626 vault share that accrues the protocol yield as its redemption value
//! rises. The adapter reads that balance on-chain and returns it as a [`StakePosition`], reporting
//! sUSDe as a `receipt_token` so a consumer that also lists idle wallet balances drops it and does
//! not double-count the stake.
//!
//! USDe held directly is the plain (unstaked) stablecoin, not a protocol position, so it is left to
//! the idle-wallet reader. Yield accrues inside the sUSDe share price (`convertToAssets`), not as a
//! separate claimable, so `rewards` is always empty. Valuation is left to the pricing layer (sUSDe
//! is priced by contract address), so the read never converts or fabricates an amount.

use alloy_primitives::{address, Address};
use async_trait::async_trait;
use gluonscan_core::{
    is_dust, Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol,
    ProtocolAdapter, Provenance, Rate, RateBasis, RateWindow, Rates, Reading, Source,
    StakePosition, Staleness, Timestamp, Token, Wallet,
};
use gluonscan_evm::{decode_u256, encode_balance_of, eth_call};
use rust_decimal::Decimal;
use std::str::FromStr;

const SUSDE: Address = address!("9D39A5DE30e57443BfF2A8307A4256c8797A3497");

/// Distinct UTC days averaged into the trailing week.
const WEEK_POINTS: usize = 7;
/// The week's points may span at most this long (one missed day of slack).
const MAX_WEEK_SPAN_SECS: i64 = 8 * 86_400;
/// The newest point must be at most this old for the trailing week to be emitted.
const MAX_NEWEST_AGE_SECS: i64 = 48 * 3600;

/// The sUSDe staking yield from DeFiLlama, as APY fractions (`0.0496` = 4.96%).
///
/// - The chart's newest daily point: an [`RateWindow::Epoch`] of 24h dated at the point.
/// - The mean of the newest 7 points: a 168h [`RateWindow::Trailing`] dated at the newest point,
///   emitted only when the 7 points span at most 8 days and the newest is at most 48h old.
///
/// Best-effort: any failure leaves the rates empty (they are informational and must never fail the
/// balance read closed).
///
/// DeFiLlama (a CDN-backed, server-reachable yields source) is used rather than Ethena's own app API
/// (`app.ethena.fi`), which is unreachable from datacenter IPs. The sUSDe pool id is stable.
async fn fetch_rates(cx: &Ctx) -> Rates {
    const SUSDE_POOL: &str = "66985a81-9c51-46ca-9977-42b4fe7bc6df";
    let url = format!("https://yields.llama.fi/chart/{SUSDE_POOL}");
    let Ok(raw) = cx.http.get(&url, &[]).await else {
        return Rates::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Rates::new();
    };
    chart_rates(&chart_points(&json), cx.clock.now().0)
}

/// The chart's `(unix seconds, APY fraction)` points, oldest first. A point without a parseable
/// timestamp or a numeric `apy` is skipped.
fn chart_points(json: &serde_json::Value) -> Vec<(i64, Decimal)> {
    let mut points: Vec<(i64, Decimal)> = json
        .get("data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let ts = chrono::DateTime::parse_from_rfc3339(p.get("timestamp")?.as_str()?)
                .ok()?
                .timestamp();
            Some((ts, percent(p.get("apy")?)?))
        })
        .collect();
    points.sort_by_key(|(ts, _)| *ts);
    points
}

fn chart_rates(points: &[(i64, Decimal)], now: i64) -> Rates {
    let mut rates = Rates::new();
    let Some(&(newest_ts, newest)) = points.last() else {
        return rates;
    };
    rates.insert(Rate::new(
        newest,
        RateBasis::Apy,
        RateWindow::Epoch { hours: Some(24) },
        Source::Api,
        Timestamp(newest_ts),
    ));
    if let Some(mean) = trailing_week(points, now) {
        rates.insert(Rate::new(
            mean,
            RateBasis::Apy,
            RateWindow::Trailing { hours: 168 },
            Source::Api,
            Timestamp(newest_ts),
        ));
    }
    rates
}

/// Mean of the newest [`WEEK_POINTS`] points (sorted oldest first), or `None` when there are fewer,
/// they span more than [`MAX_WEEK_SPAN_SECS`], or the newest is older than [`MAX_NEWEST_AGE_SECS`].
fn trailing_week(points: &[(i64, Decimal)], now: i64) -> Option<Decimal> {
    // DefiLlama can append an intraday point next to the daily one; keeping only the newest point
    // of each UTC day stops seven points from covering fewer than seven days.
    let mut days: Vec<(i64, Decimal)> = Vec::with_capacity(points.len());
    for &(ts, value) in points {
        match days.last_mut() {
            Some(last) if last.0.div_euclid(86_400) == ts.div_euclid(86_400) => *last = (ts, value),
            _ => days.push((ts, value)),
        }
    }
    let week = days.get(days.len().checked_sub(WEEK_POINTS)?..)?;
    let (oldest_ts, _) = week.first()?;
    let (newest_ts, _) = week.last()?;
    if newest_ts - oldest_ts > MAX_WEEK_SPAN_SECS || now - newest_ts > MAX_NEWEST_AGE_SECS {
        return None;
    }
    let sum = week
        .iter()
        .try_fold(Decimal::ZERO, |acc, (_, v)| acc.checked_add(*v))?;
    sum.checked_div(Decimal::from(WEEK_POINTS))
}

/// A JSON percentage number as a fraction, parsed from serde_json's shortest round-trip literal
/// rather than by `f64` arithmetic.
fn percent(v: &serde_json::Value) -> Option<Decimal> {
    if !v.is_number() {
        return None;
    }
    let literal = v.to_string();
    Decimal::from_str(&literal)
        .or_else(|_| Decimal::from_scientific(&literal))
        .ok()?
        .checked_div(Decimal::ONE_HUNDRED)
}

const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum];

/// Ethena staked-USDe adapter. Reads the sUSDe balance on Ethereum via on-chain `balanceOf`.
#[derive(Debug, Default, Clone)]
pub struct Ethena;

impl Ethena {
    /// Construct the adapter.
    pub fn new() -> Self {
        Ethena
    }
}

#[async_trait]
impl ProtocolAdapter for Ethena {
    fn protocol(&self) -> Protocol {
        Protocol::Ethena
    }

    fn source(&self) -> Source {
        Source::OnChain
    }

    fn capabilities(&self) -> &'static [Capability] {
        CAPABILITIES
    }

    fn supported_chains(&self) -> &'static [Chain] {
        SUPPORTED_CHAINS
    }

    async fn read(
        &self,
        owner: &Wallet,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if chain != Chain::Ethereum {
            return Err(Error::Permanent {
                message: format!("Ethena not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let rpc = cx.rpc()?.as_ref();

        let susde_raw =
            decode_u256(&eth_call(rpc, chain, None, SUSDE, encode_balance_of(owner)).await?)?;

        // A zero balance is a valid "no position" result, not an error. Dust is no position either,
        // but sUSDe stays a receipt token so a consumer's wallet list does not surface it.
        let (positions, receipt_tokens) = if susde_raw.is_zero() {
            (Vec::new(), Vec::new())
        } else if is_dust(susde_raw, 18) {
            (Vec::new(), vec![SUSDE])
        } else {
            let staked = vec![Amount::from_raw(
                Token::evm("sUSDe", Some(SUSDE), 18),
                susde_raw,
            )?];
            let rates = fetch_rates(cx).await;
            (
                vec![Position::Stake(
                    StakePosition::new(staked).with_rates(rates),
                )],
                vec![SUSDE],
            )
        };

        let reading = Reading::new(
            Protocol::Ethena,
            chain,
            Source::OnChain,
            positions,
            Provenance::new(Source::OnChain, chain, cx.clock.now(), Staleness::Live),
        )
        .with_receipt_tokens(receipt_tokens);
        Ok(Complete::new(reading))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn daily(newest: i64, values: &[&str]) -> Vec<(i64, Decimal)> {
        let n = values.len() as i64;
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                (
                    newest - (n - 1 - i as i64) * DAY,
                    d(v) / Decimal::ONE_HUNDRED,
                )
            })
            .collect()
    }

    #[test]
    fn trailing_week_is_the_mean_of_the_newest_seven_points() {
        let points = daily(100 * DAY, &["9", "1", "2", "3", "4", "5", "6", "7"]);
        assert_eq!(trailing_week(&points, 100 * DAY), Some(d("0.04")));
    }

    #[test]
    fn trailing_week_needs_seven_points() {
        let points = daily(100 * DAY, &["1", "2", "3", "4", "5", "6"]);
        assert_eq!(trailing_week(&points, 100 * DAY), None);
        assert_eq!(trailing_week(&[], 100 * DAY), None);
    }

    #[test]
    fn trailing_week_span_boundary_is_eight_days() {
        let mut points = daily(100 * DAY, &["1", "2", "3", "4", "5", "6", "7"]);
        // Stretch the oldest point back so the 7 points span exactly 8 days: accepted.
        points[0].0 = 92 * DAY;
        assert!(trailing_week(&points, 100 * DAY).is_some());
        // One second wider: a gap, so the week is not covered.
        points[0].0 = 92 * DAY - 1;
        assert_eq!(trailing_week(&points, 100 * DAY), None);
    }

    #[test]
    fn trailing_week_keeps_one_point_per_utc_day() {
        // Six daily points plus two intraday ones on the newest day: only 6 distinct days.
        let mut points = daily(100 * DAY, &["1", "2", "3", "4", "5", "6"]);
        points.push((100 * DAY + 3600, d("0.50")));
        points.push((100 * DAY + 7200, d("0.07")));
        assert_eq!(trailing_week(&points, 100 * DAY + 7200), None);
        // A seventh day makes the week; the newest point of each day is the one averaged.
        let mut points = daily(100 * DAY, &["9", "1", "2", "3", "4", "5", "6"]);
        points.push((101 * DAY + 3600, d("0.50")));
        points.push((101 * DAY + 7200, d("0.07")));
        assert_eq!(trailing_week(&points, 101 * DAY + 7200), Some(d("0.04")));
    }

    #[test]
    fn trailing_week_newest_age_boundary_is_48h() {
        let points = daily(100 * DAY, &["1", "2", "3", "4", "5", "6", "7"]);
        assert!(trailing_week(&points, 100 * DAY + 48 * 3600).is_some());
        assert_eq!(trailing_week(&points, 100 * DAY + 48 * 3600 + 1), None);
    }

    #[test]
    fn stale_week_keeps_the_newest_point_as_its_own_epoch() {
        let points = daily(100 * DAY, &["1", "2", "3", "4", "5", "6", "7"]);
        let rates = chart_rates(&points, 110 * DAY);
        assert_eq!(rates.len(), 1);
        let epoch = rates
            .get(RateBasis::Apy, RateWindow::Epoch { hours: Some(24) })
            .unwrap();
        assert_eq!(epoch.value, d("0.07"));
        assert_eq!(epoch.as_of, Timestamp(100 * DAY));
    }
}
