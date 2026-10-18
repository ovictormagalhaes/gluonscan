//! # gluonscan-kamino
//!
//! Kamino adapter (Solana lending), [`Source::Api`] + on-chain mint decimals: reads a wallet's
//! obligations from the Kamino HTTP API across its markets and normalizes them into
//! [`LendingPosition`]s. Per-leg amounts and USD values come from the newest point of the
//! obligation's hourly `metrics/history`, which the API refreshes: underlying token amounts with
//! interest accrued, valued at the current oracle price. The obligation account's own `state` is
//! not used for amounts: its deposits are collateral shares, and its debt and market values are
//! frozen at the owner's last transaction. A point older than [`MAX_POINT_AGE_SECS`], or one that
//! predates the obligation's last on-chain update, fails the read as retryable. The health factor
//! is derived (`borrowLiquidationLimit / borrowFactorAdjustedDebt`) since the API does not expose
//! it directly.
//!
//! Each leg carries the reserve's instantaneous supply/borrow APY from `reserves/metrics`, plus the
//! 24h and 7d trailing means of the reserve's hourly `metrics/history`, each emitted only when the
//! window holds at least [`MIN_TRAILING_COVERAGE_PCT`] of its hourly points and the newest point is
//! at most [`MAX_RATE_POINT_AGE_SECS`] old. The instantaneous borrow APY swings with utilization within a
//! day, so a single sample can sit far from what the debt actually accrued.
//!
//! History ([`ProtocolAdapter::read_history`]) is the per-mint delta between consecutive points of
//! the obligation's `metrics/history`, in whole base units (fractions of a base unit dropped, as
//! for the live legs). The selector is `"obligation"` or `"obligation|market"`; every part must be
//! base58, and the obligation must be listed among the owner's obligations in that market, so a
//! foreign obligation is refused as [`Error::Permanent`]. A missing `history` array, a point without
//! its timestamp, side arrays or mints, or an amount that does not parse fails the read as
//! [`Error::Integrity`] instead of reading as a zero balance.
//!
//! A balance grows between points by its interest alone, so each rise of a held balance is first
//! checked against [`MAX_ACCRUAL_PER_DAY`], scaled by the time between the two points (at most
//! [`MAX_ACCRUAL_GAP_SECS`]): a rise within it is [`EventKind::SupplyInterest`] (deposits) or
//! [`EventKind::BorrowInterest`] (borrows), not an action. A rise above it is a
//! [`EventKind::Deposit`] / [`EventKind::Borrow`] of the whole delta (that period's accrual
//! included), as is a rise from a zero balance; every fall is a [`EventKind::Withdraw`] /
//! [`EventKind::Repay`] (net of that period's accrual).
//!
//! A daily point (stamped at the day's 00:00 UTC) carries the newest hourly point published inside
//! that day, so the API revises it until the day's last hourly point (23:00) is published, about
//! 1-2h after the UTC day closes. A point is final only [`HISTORY_DAY_SECS`] +
//! [`HISTORY_SETTLE_SECS`] after its timestamp (by the context clock); before that it yields no
//! events, since its amounts, and with them the event kind and id, could still change. A day's
//! activity appears a few hours after that day has closed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;

use alloy_primitives::{U256, U512};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, ChainProvider, Complete, Ctx, Currency, Detail,
    Error, EventKind, History, HistoryEvent, LendingPosition, Money, Position, Protocol,
    ProtocolAdapter, Provenance, Rate, RateBasis, RateWindow, Rates, Reading, Source, Staleness,
    SuppliedAsset, Timestamp, Token, Wallet,
};
use gluonscan_solana::get_mint_decimals;
use rust_decimal::Decimal;

const KAMINO_API: &str = "https://api.kamino.finance";
const CAPABILITIES: &[Capability] = &[
    Capability::Positions,
    Capability::HealthFactor,
    Capability::RiskConfig,
    Capability::History,
];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Solana];

/// Kamino's known lending markets (Main, JLP, Altcoins).
const MARKETS: &[&str] = &[
    "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF",
    "ByVuX9fRdEHsZomwLVryQQHRhQi3yMXZ6uz6DA4gutja",
    "DxXdAyU3kCjnyggvHmY5nAwg5cRbbmdyX3npfDMjjMek",
];

/// Oldest obligation history point accepted as a live reading. The API stamps one point per hour
/// but publishes it about an hour later, so the newest point is normally up to ~2h old; the margin
/// absorbs one late publication. A transaction after the point is caught separately.
pub const MAX_POINT_AGE_SECS: i64 = 4 * 3600;
/// How far back the live-point request looks; wider than [`MAX_POINT_AGE_SECS`] so an aged point
/// is reported as stale instead of as missing.
const POINT_LOOKBACK_SECS: i64 = 12 * 3600;

/// Trailing windows, in hours, averaged from a reserve's hourly rate history.
const TRAILING_WINDOWS: &[u32] = &[24, 168];
/// Share of a window's hourly points, in percent, that must be present for its mean to be emitted.
pub const MIN_TRAILING_COVERAGE_PCT: u64 = 90;
/// Oldest newest-point accepted for a trailing mean; older history yields no trailing rate.
pub const MAX_RATE_POINT_AGE_SECS: i64 = 3 * 3600;
/// How far back a reserve's rate history is requested: the longest window plus a margin.
const RATE_LOOKBACK_SECS: i64 = (168 + 4) * 3600;
/// Reserve rate histories fetched at once.
const RATE_HISTORY_CONCURRENCY: usize = 4;

/// Largest growth of a held balance per day, as a fraction of the previous balance, that history
/// reads as interest accrual (0.2%/day, about 73% APR). Scaled by the exact time between two
/// points, credited for at most [`MAX_ACCRUAL_GAP_SECS`]; a growth at or under the scaled bound is
/// [`EventKind::SupplyInterest`] / [`EventKind::BorrowInterest`], one above it a
/// [`EventKind::Deposit`] / [`EventKind::Borrow`]. A reserve whose daily accrual exceeds the bound
/// (FDUSD paid about 159% APY for weeks), or accrual across a data gap longer than
/// [`MAX_ACCRUAL_GAP_SECS`] that exceeds the capped bound, therefore reads as
/// [`EventKind::Deposit`] / [`EventKind::Borrow`], and an action small enough to fit under the
/// bound reads as interest.
pub const MAX_ACCRUAL_PER_DAY: Decimal = Decimal::from_parts(2, 0, 0, false, 3);
/// Most time between two history points credited to accrual (2 days). Across a longer data gap the
/// uncapped bound would cover a real deposit or borrow (about 20% of the balance over 103 days), so
/// a rise there is interest only within two days' bound.
pub const MAX_ACCRUAL_GAP_SECS: u64 = 2 * HISTORY_DAY_SECS.unsigned_abs();
/// Span of one daily history point. A point is final [`HISTORY_DAY_SECS`] +
/// [`HISTORY_SETTLE_SECS`] after its timestamp.
pub const HISTORY_DAY_SECS: i64 = 86_400;
/// How long after a day closes its daily point may still be revised: the point carries the newest
/// hourly point published inside the day, and the day's last hourly point (23:00) is published
/// about 1-2h after the day closes. The same margin as [`MAX_POINT_AGE_SECS`], which bounds how
/// late an hourly point is published.
pub const HISTORY_SETTLE_SECS: i64 = MAX_POINT_AGE_SECS;

/// Kamino adapter backed by the public HTTP API + on-chain mint decimals.
#[derive(Debug, Default, Clone)]
pub struct KaminoApi {
    base: Option<String>,
}

impl KaminoApi {
    /// Construct with the default public API base.
    pub fn new() -> Self {
        KaminoApi { base: None }
    }

    /// Override the API base (private gateway or test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = Some(url.into());
        self
    }

    fn base(&self) -> &str {
        self.base.as_deref().unwrap_or(KAMINO_API)
    }
}

#[async_trait]
impl ProtocolAdapter for KaminoApi {
    fn protocol(&self) -> Protocol {
        Protocol::Kamino
    }

    fn source(&self) -> Source {
        Source::Api
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
        if chain != Chain::Solana {
            return Err(Error::Permanent {
                message: format!("Kamino is Solana-only; got {chain:?}"),
            });
        }
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();
        let mut decimals_cache: HashMap<String, u8> = HashMap::new();

        let mut positions = Vec::new();
        for market in MARKETS {
            let url = format!(
                "{}/kamino-market/{market}/users/{wallet}/obligations",
                self.base()
            );
            let raw = cx.http.get(&url, &[]).await?;
            let obligations: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                    message: format!("Kamino obligations not JSON: {e}"),
                })?;
            // A 200 with an unexpected (non-array) shape is a source failure, not "no positions":
            // fail closed rather than silently drop this market's obligations.
            let items = obligations.as_array().ok_or_else(|| Error::Integrity {
                message: format!(
                    "Kamino market {market} returned a non-array obligations response"
                ),
            })?;
            if items.is_empty() {
                continue;
            }

            // Join the reserve metadata (symbol, mint, risk, rates) for this market.
            let res_url = format!("{}/kamino-market/{market}/reserves/metrics", self.base());
            let res_raw = cx.http.get(&res_url, &[]).await?;
            let reserves: serde_json::Value =
                serde_json::from_str(&res_raw).map_err(|e| Error::Integrity {
                    message: format!("Kamino reserves not JSON: {e}"),
                })?;
            let reserve_map = build_reserve_map(&reserves)?;

            let mut obligations = Vec::with_capacity(items.len());
            for ob in items {
                // An emptied obligation stays listed but holds no legs: nothing to read live.
                let live = if holds_legs(ob)? {
                    latest_obligation_point(self.base(), market, ob, cx).await?
                } else {
                    serde_json::Value::Null
                };
                obligations.push((ob, live));
            }

            let held: BTreeSet<String> = obligations
                .iter()
                .flat_map(|(_, live)| {
                    live_reserves(live, "deposits")
                        .into_iter()
                        .chain(live_reserves(live, "borrows"))
                })
                .collect();
            let trailing = trailing_rates(self.base(), market, &held, cx).await;
            let rates = LegRates {
                reserves: &reserve_map,
                trailing: &trailing,
                read_at: cx.clock.now(),
            };

            for (ob, live) in &obligations {
                positions.push(Position::Lending(
                    parse_obligation(ob, live, market, &rates, rpc, &mut decimals_cache).await?,
                ));
            }
        }

        let reading = Reading::new(
            Protocol::Kamino,
            Chain::Solana,
            Source::Api,
            positions,
            Provenance::new(Source::Api, Chain::Solana, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
    }

    async fn read_history(
        &self,
        owner: &Wallet,
        chain: Chain,
        position: Option<&str>,
        since: Option<Timestamp>,
        cx: &Ctx,
    ) -> Result<Complete<History>, Error> {
        if chain != Chain::Solana {
            return Err(Error::Permanent {
                message: format!("Kamino is Solana-only; got {chain:?}"),
            });
        }
        // A Kamino position is identified by its obligation + market. The market may be supplied
        // (`"obligation|market"`) or resolved from the owner's obligations (`"obligation"` alone).
        let selector = position.ok_or_else(|| Error::Permanent {
            message:
                "Kamino history needs a position selector (\"obligation\" or \"obligation|market\")"
                    .into(),
        })?;
        let wallet = owner.solana()?;
        let (obligation, market) = match selector.split_once('|') {
            Some((o, m)) => (o, Some(m)),
            None => (selector, None),
        };
        // The selector comes from the caller and, like the wallet, is interpolated into request
        // paths: anything but base58 is refused before a request is made.
        let parts = [
            ("obligation", Some(obligation)),
            ("market", market),
            ("wallet", Some(wallet)),
        ];
        for (what, value) in parts {
            if let Some(value) = value.filter(|v| !is_base58(v)) {
                return Err(Error::Permanent {
                    message: format!("Kamino history {what} {value:?} is not base58"),
                });
            }
        }
        let rpc = cx.rpc()?.as_ref();
        // A supplied market only saves the scan: the obligation must still be one of the owner's.
        let market = match market {
            Some(market) => {
                if !owner_holds_obligation(self.base(), market, wallet, obligation, cx).await? {
                    return Err(Error::Permanent {
                        message: format!(
                            "Kamino obligation {obligation} is not held by the owner in market {market}"
                        ),
                    });
                }
                market.to_string()
            }
            None => resolve_market(self.base(), wallet, obligation, cx).await?,
        };
        let market = market.as_str();
        let since_ts = since.map(|t| t.0).unwrap_or(0);

        // mint -> symbol from the market's reserves (decimals are read on-chain per mint).
        let res_url = format!("{}/kamino-market/{market}/reserves/metrics", self.base());
        let res_raw = cx.http.get(&res_url, &[]).await?;
        let reserves: serde_json::Value =
            serde_json::from_str(&res_raw).map_err(|e| Error::Integrity {
                message: format!("Kamino reserves not JSON: {e}"),
            })?;
        let mut mint_symbol: HashMap<String, String> = HashMap::new();
        for meta in build_reserve_map(&reserves)?.values() {
            if !meta.mint.is_empty() {
                mint_symbol.insert(meta.mint.clone(), meta.symbol.clone());
            }
        }

        // Obligation history snapshots (cumulative per-mint balances); events are the deltas between
        // consecutive snapshots.
        let hist_url = format!(
            "{}/v2/kamino-market/{market}/obligations/{obligation}/metrics/history",
            self.base()
        );
        let hist_raw = cx.http.get(&hist_url, &[]).await?;
        let hist: serde_json::Value =
            serde_json::from_str(&hist_raw).map_err(|e| Error::Integrity {
                message: format!("Kamino history not JSON: {e}"),
            })?;
        // A body without the array is the source failing, not an obligation with no history.
        let points = hist
            .pointer("/history")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Integrity {
                message: format!("Kamino obligation {obligation} history has no `history` array"),
            })?;
        // Every point is parsed up front: an unreadable point fails the read instead of reading as
        // a zero balance and producing events of the full balance on either side of it.
        let mut snaps = points
            .iter()
            .map(|p| Snapshot::parse(p, obligation))
            .collect::<Result<Vec<_>, Error>>()?;
        snaps.sort_by_key(|s| s.ts);

        let now = cx.clock.now();
        let mut decimals_cache: HashMap<String, u8> = HashMap::new();
        let mut events: Vec<HistoryEvent> = Vec::new();
        for window in snaps.windows(2) {
            let (prev, curr) = (&window[0], &window[1]);
            // A point is revised by the API until the day's last hourly point is published: its
            // delta (and so the event kind and id) is not final before then. A clock before the
            // point is the same case.
            let settled_at = curr
                .ts
                .saturating_add(HISTORY_DAY_SECS)
                .saturating_add(HISTORY_SETTLE_SECS);
            if settled_at > now.0 {
                continue;
            }
            // Sorted, so never negative; equal timestamps allow no accrual.
            let gap_secs = u64::try_from(curr.ts - prev.ts).unwrap_or(0);
            let sides = [
                (
                    &prev.deposits,
                    &curr.deposits,
                    SideKinds {
                        rise: EventKind::Deposit,
                        fall: EventKind::Withdraw,
                        accrual: EventKind::SupplyInterest,
                    },
                ),
                (
                    &prev.borrows,
                    &curr.borrows,
                    SideKinds {
                        rise: EventKind::Borrow,
                        fall: EventKind::Repay,
                        accrual: EventKind::BorrowInterest,
                    },
                ),
            ];
            for (before, after, kinds) in sides {
                diff_side(
                    before,
                    after,
                    kinds,
                    gap_secs,
                    obligation,
                    curr.ts,
                    &mint_symbol,
                    rpc,
                    &mut decimals_cache,
                    &mut events,
                )
                .await?;
            }
        }
        events.retain(|e| e.at.0 > since_ts);
        events.sort_by_key(|e| e.at.0);

        let history = History::new(
            Protocol::Kamino,
            Chain::Solana,
            events,
            Provenance::new(Source::Api, Chain::Solana, now, Staleness::Live),
        );
        Ok(Complete::new(history))
    }
}

/// Find which Kamino market holds `obligation` for `wallet` by scanning the known markets' obligation
/// lists. Used when the history selector carries no explicit market. A transport failure propagates
/// (fail-closed); a not-found obligation is a permanent error.
async fn resolve_market(
    base: &str,
    wallet: &str,
    obligation: &str,
    cx: &Ctx,
) -> Result<String, Error> {
    for market in MARKETS {
        if owner_holds_obligation(base, market, wallet, obligation, cx).await? {
            return Ok((*market).to_string());
        }
    }
    Err(Error::Permanent {
        message: format!("Kamino obligation {obligation} not found in any market for the owner"),
    })
}

/// Whether `obligation` is listed among `wallet`'s obligations in `market` (the same
/// `users/{wallet}/obligations` list [`KaminoApi::read`] walks). A failed or malformed read is an
/// error, never `false`. Base58 is case-sensitive, so the match is exact.
async fn owner_holds_obligation(
    base: &str,
    market: &str,
    wallet: &str,
    obligation: &str,
    cx: &Ctx,
) -> Result<bool, Error> {
    let url = format!("{base}/kamino-market/{market}/users/{wallet}/obligations");
    let raw = cx.http.get(&url, &[]).await?;
    let obligations: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Kamino obligations not JSON: {e}"),
        })?;
    let items = obligations.as_array().ok_or_else(|| Error::Integrity {
        message: format!("Kamino market {market} returned a non-array obligations response"),
    })?;
    Ok(items.iter().any(|ob| {
        ob.get("obligationAddress")
            .and_then(|v| v.as_str())
            .is_some_and(|a| a == obligation)
    }))
}

/// The newest point of an obligation's hourly `metrics/history`, checked to be a live reading of the
/// obligation as it stands now: no older than [`MAX_POINT_AGE_SECS`], and not older than the
/// obligation's last on-chain update (a deposit, withdraw, borrow or repay after the point would be
/// missing from it). Both checks fail as [`Error::Transient`]: the next hourly point resolves them.
async fn latest_obligation_point(
    base: &str,
    market: &str,
    ob: &serde_json::Value,
    cx: &Ctx,
) -> Result<serde_json::Value, Error> {
    let obligation = obligation_address(ob)?;
    let now = cx.clock.now().0;
    let url = format!(
        "{base}/v2/kamino-market/{market}/obligations/{obligation}/metrics/history?start={}&end={}",
        rfc3339(now - POINT_LOOKBACK_SECS),
        rfc3339(now + 3600)
    );
    let raw = cx.http.get(&url, &[]).await?;
    let hist: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("Kamino obligation history not JSON: {e}"),
    })?;
    let points = hist
        .pointer("/history")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Integrity {
            message: format!("Kamino obligation {obligation} history has no `history` array"),
        })?;
    let latest = points
        .iter()
        .filter(|p| snapshot_ts(p) > 0)
        .max_by_key(|p| snapshot_ts(p))
        .ok_or_else(|| Error::Transient {
            message: format!("Kamino obligation {obligation} has no history point yet"),
            retry_after: None,
        })?;
    let point_ts = snapshot_ts(latest);
    if now - point_ts > MAX_POINT_AGE_SECS {
        return Err(Error::Transient {
            message: format!(
                "Kamino obligation {obligation} newest history point is {}s old",
                now - point_ts
            ),
            retry_after: None,
        });
    }
    let updated = ob
        .pointer("/state/lastUpdate/timestamp")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| Error::Integrity {
            message: format!(
                "Kamino obligation {obligation} has no numeric `state.lastUpdate.timestamp`"
            ),
        })?;
    if updated > point_ts {
        return Err(Error::Transient {
            message: format!(
                "Kamino obligation {obligation} changed on-chain after its newest history point"
            ),
            retry_after: None,
        });
    }
    Ok(latest.clone())
}

fn obligation_address(ob: &serde_json::Value) -> Result<&str, Error> {
    let address = ob
        .get("obligationAddress")
        .and_then(|v| v.as_str())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::Integrity {
            message: "Kamino obligation missing `obligationAddress`".into(),
        })?;
    // It is interpolated into a request path, so only base58 characters are accepted.
    if !is_base58(address) {
        return Err(Error::Integrity {
            message: format!("Kamino obligation address {address:?} is not base58"),
        });
    }
    Ok(address)
}

/// Trailing supply and borrow rates of one reserve.
#[derive(Debug, Default)]
struct ReserveRates {
    supply: Rates,
    borrow: Rates,
}

/// Everything needed to rate a leg: the reserves' instantaneous APYs and their trailing means.
struct LegRates<'a> {
    reserves: &'a HashMap<String, ReserveMeta>,
    trailing: &'a HashMap<String, ReserveRates>,
    read_at: Timestamp,
}

impl LegRates<'_> {
    /// The leg's rates: the trailing means of its reserve, plus the instantaneous APY from
    /// `reserves/metrics`, which carries no timestamp and is dated at the read.
    fn for_leg(&self, reserve: &str, borrow: bool) -> Rates {
        let trailing = self.trailing.get(reserve);
        let meta = self.reserves.get(reserve);
        let (mut rates, instant) = if borrow {
            (
                trailing.map(|t| t.borrow.clone()).unwrap_or_default(),
                meta.and_then(|m| m.borrow_apy),
            )
        } else {
            (
                trailing.map(|t| t.supply.clone()).unwrap_or_default(),
                meta.and_then(|m| m.supply_apy),
            )
        };
        if let Some(apy) = instant {
            rates.insert(Rate::new(
                apy,
                RateBasis::Apy,
                RateWindow::Instant,
                Source::Api,
                self.read_at,
            ));
        }
        rates
    }
}

/// The trailing rates of every `held` reserve, fetched concurrently. Best-effort: a reserve whose
/// history cannot be fetched or parsed just has no trailing rates; the read never fails over them.
async fn trailing_rates(
    base: &str,
    market: &str,
    held: &BTreeSet<String>,
    cx: &Ctx,
) -> HashMap<String, ReserveRates> {
    let now = cx.clock.now().0;
    // Built eagerly from owned values: a lazy `map` over borrowed items makes the future
    // higher-ranked over their lifetime, which the `Send` bound of the adapter future rejects.
    let mut fetches = Vec::with_capacity(held.len());
    for reserve in held {
        if is_base58(reserve) {
            fetches.push(reserve_trailing_rates(
                base,
                market,
                reserve.clone(),
                now,
                cx,
            ));
        }
    }
    stream::iter(fetches)
        .buffered(RATE_HISTORY_CONCURRENCY)
        .collect()
        .await
}

async fn reserve_trailing_rates(
    base: &str,
    market: &str,
    reserve: String,
    now: i64,
    cx: &Ctx,
) -> (String, ReserveRates) {
    let rates = reserve_history(base, market, &reserve, now, cx)
        .await
        .map(|hist| reserve_rates(&hist, now))
        .unwrap_or_default();
    (reserve, rates)
}

async fn reserve_history(
    base: &str,
    market: &str,
    reserve: &str,
    now: i64,
    cx: &Ctx,
) -> Option<serde_json::Value> {
    let url = format!(
        "{base}/kamino-market/{market}/reserves/{reserve}/metrics/history?env=mainnet-beta&start={}&end={}&frequency=hour",
        rfc3339(now - RATE_LOOKBACK_SECS),
        rfc3339(now + 3600)
    );
    let raw = cx.http.get(&url, &[]).await.ok()?;
    serde_json::from_str(&raw).ok()
}

/// The trailing APYs of a reserve's hourly history (`{"history":[{"timestamp","metrics":{...}}]}`),
/// one per [`TRAILING_WINDOWS`] entry that passes [`trailing_mean`]'s coverage rule.
fn reserve_rates(hist: &serde_json::Value, now: i64) -> ReserveRates {
    let mut supply = BTreeMap::new();
    let mut borrow = BTreeMap::new();
    for point in hist
        .get("history")
        .and_then(|h| h.as_array())
        .into_iter()
        .flatten()
    {
        let ts = snapshot_ts(point);
        if ts <= 0 {
            continue;
        }
        if let Some(apy) = decimal_at(point, "/metrics/supplyInterestAPY") {
            supply.insert(ts, apy);
        }
        if let Some(apy) = decimal_at(point, "/metrics/borrowInterestAPY") {
            borrow.insert(ts, apy);
        }
    }
    let windows = |points: &BTreeMap<i64, Decimal>| -> Rates {
        TRAILING_WINDOWS
            .iter()
            .filter_map(|&hours| {
                let (mean, as_of) = trailing_mean(points, hours, now)?;
                Some(Rate::new(
                    mean,
                    RateBasis::Apy,
                    RateWindow::Trailing { hours },
                    Source::Api,
                    Timestamp(as_of),
                ))
            })
            .collect()
    };
    ReserveRates {
        supply: windows(&supply),
        borrow: windows(&borrow),
    }
}

/// The arithmetic mean of the hourly `points` (unix seconds -> value) in the `hours` window ending
/// at the newest point, and that point's timestamp. `None` unless the newest point is not after
/// `now` and at most [`MAX_RATE_POINT_AGE_SECS`] old, and the window's points fall in at least
/// [`MIN_TRAILING_COVERAGE_PCT`] of its `hours` distinct clock hours.
fn trailing_mean(points: &BTreeMap<i64, Decimal>, hours: u32, now: i64) -> Option<(Decimal, i64)> {
    let (&newest, _) = points.last_key_value()?;
    if hours == 0 || newest > now || now - newest > MAX_RATE_POINT_AGE_SECS {
        return None;
    }
    let start = newest - i64::from(hours) * 3600;
    let mut count = 0u64;
    let mut sum = Decimal::ZERO;
    // Coverage is counted in clock hours, so sub-hourly points cannot pass a few hours off as a day.
    let mut covered_hours = BTreeSet::new();
    for (ts, value) in points.range(start + 1..=newest) {
        count += 1;
        sum = sum.checked_add(*value)?;
        covered_hours.insert(ts.div_euclid(3600));
    }
    if (covered_hours.len() as u64) * 100 < u64::from(hours) * MIN_TRAILING_COVERAGE_PCT {
        return None;
    }
    Some((sum.checked_div(Decimal::from(count))?, newest))
}

/// Whether `s` is non-empty base58, so it is safe to interpolate into a request path.
fn is_base58(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l'))
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Epoch seconds from a snapshot's RFC3339 `timestamp` (0 if missing/unparseable).
fn snapshot_ts(snap: &serde_json::Value) -> i64 {
    snap.get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// One point of an obligation's `metrics/history`: its time and the whole base units held per mint
/// on each side.
struct Snapshot {
    ts: i64,
    deposits: HashMap<String, U256>,
    borrows: HashMap<String, U256>,
}

impl Snapshot {
    /// Parse a point, failing closed on anything that would otherwise read as a zero balance: a
    /// missing or unparseable timestamp, a side that is not an array, a leg without its mint, or an
    /// amount that is not a non-negative decimal.
    fn parse(point: &serde_json::Value, obligation: &str) -> Result<Snapshot, Error> {
        let ts = snapshot_ts(point);
        if ts <= 0 {
            return Err(Error::Integrity {
                message: format!(
                    "Kamino obligation {obligation} history point has no RFC 3339 `timestamp`"
                ),
            });
        }
        Ok(Snapshot {
            ts,
            deposits: side_amounts(point, "deposits", obligation)?,
            borrows: side_amounts(point, "borrows", obligation)?,
        })
    }
}

/// Whole base units per mint on one side (`"deposits"` | `"borrows"`) of a history point. The API
/// carries accrued fractions of a base unit; they are dropped exactly as the live reader drops them
/// ([`base_units`]), so consecutive points compare in the same units.
fn side_amounts(
    point: &serde_json::Value,
    side: &str,
    obligation: &str,
) -> Result<HashMap<String, U256>, Error> {
    let legs = point
        .get(side)
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Integrity {
            message: format!("Kamino obligation {obligation} history point has no `{side}` array"),
        })?;
    let mut out: HashMap<String, U256> = HashMap::new();
    for leg in legs {
        let mint = leg
            .get("mintAddress")
            .and_then(|v| v.as_str())
            .filter(|m| !m.is_empty())
            .ok_or_else(|| Error::Integrity {
                message: format!(
                    "Kamino obligation {obligation} history {side} leg missing `mintAddress`"
                ),
            })?;
        let raw = leg
            .get("amount")
            .and_then(|v| v.as_str())
            .and_then(base_units)
            .ok_or_else(|| Error::Integrity {
                message: format!(
                    "Kamino obligation {obligation} history {side} leg for {mint} has no numeric `amount`"
                ),
            })?;
        let total = out.entry(mint.to_string()).or_insert(U256::ZERO);
        *total = total.checked_add(raw).ok_or_else(|| Error::Integrity {
            message: format!("Kamino obligation {obligation} history {side} amount overflows"),
        })?;
    }
    Ok(out)
}

/// The event kinds of one obligation side.
#[derive(Clone, Copy)]
struct SideKinds {
    /// A rise above the accrual bound, or from zero (deposit / borrow).
    rise: EventKind,
    /// A fall (withdraw / repay).
    fall: EventKind,
    /// A rise of a held balance within the accrual bound (supply / borrow interest).
    accrual: EventKind,
}

/// Whether `growth` of a `prev` balance over `gap_secs` fits [`MAX_ACCRUAL_PER_DAY`]: with
/// `t = min(gap_secs, MAX_ACCRUAL_GAP_SECS)`, exactly `growth <= prev * MAX_ACCRUAL_PER_DAY * t /
/// 86_400`, compared as `growth * 10^scale * 86_400 <= prev * mantissa * t` in 512 bits, where no
/// operand can overflow. A zero `prev` or `gap_secs` allows no accrual.
fn within_accrual_bound(prev: U256, growth: U256, gap_secs: u64) -> bool {
    const SECS_PER_DAY: u64 = HISTORY_DAY_SECS.unsigned_abs();
    let credited_secs = gap_secs.min(MAX_ACCRUAL_GAP_SECS);
    let mantissa = MAX_ACCRUAL_PER_DAY.mantissa().unsigned_abs();
    let denominator = 10u128.pow(MAX_ACCRUAL_PER_DAY.scale());
    // A `Decimal` mantissa is under 2^96 and its scale at most 28 (10^28 < 2^94): both products
    // stay under 2^256 * 2^96 * 2^64, well inside 512 bits.
    let lhs = widen(growth) * U512::from(denominator) * U512::from(SECS_PER_DAY);
    let rhs = widen(prev) * U512::from(mantissa) * U512::from(credited_secs);
    lhs <= rhs
}

fn widen(v: U256) -> U512 {
    let mut limbs = [0u64; 8];
    limbs[..4].copy_from_slice(v.as_limbs());
    U512::from_limbs(limbs)
}

/// Diff one side between two consecutive points `gap_secs` apart, pushing an event per mint whose
/// balance changed: a rise of a held balance within [`MAX_ACCRUAL_PER_DAY`] is `kinds.accrual`,
/// any other rise `kinds.rise`, a fall `kinds.fall`. Sub-dust deltas are skipped.
#[allow(
    clippy::too_many_arguments,
    reason = "internal side-diff helper threading prev/curr state, injected rpc + decimals cache, and an output event sink — not a data cluster"
)]
async fn diff_side(
    prev: &HashMap<String, U256>,
    curr: &HashMap<String, U256>,
    kinds: SideKinds,
    gap_secs: u64,
    obligation: &str,
    ts: i64,
    mint_symbol: &HashMap<String, String>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
    events: &mut Vec<HistoryEvent>,
) -> Result<(), Error> {
    let mut mints: Vec<&String> = prev.keys().chain(curr.keys()).collect();
    mints.sort();
    mints.dedup();
    for mint in mints {
        let before = prev.get(mint).copied().unwrap_or(U256::ZERO);
        let after = curr.get(mint).copied().unwrap_or(U256::ZERO);
        let (kind, delta) = match after.cmp(&before) {
            std::cmp::Ordering::Equal => continue,
            std::cmp::Ordering::Greater => {
                let growth = after - before;
                if !before.is_zero() && within_accrual_bound(before, growth, gap_secs) {
                    (kinds.accrual, growth)
                } else {
                    (kinds.rise, growth)
                }
            }
            std::cmp::Ordering::Less => (kinds.fall, before - after),
        };
        let decimals = match decimals_cache.get(mint) {
            Some(d) => *d,
            None => {
                let d = get_mint_decimals(rpc, mint).await?;
                decimals_cache.insert(mint.clone(), d);
                d
            }
        };
        let symbol = mint_symbol.get(mint).cloned().unwrap_or_default();
        let amount = Amount::from_raw(Token::solana(symbol, Some(mint.clone()), decimals), delta)?;
        // Sub-dust snapshot deltas are rounding noise, not real deposits/withdrawals or interest.
        if amount.amount < Decimal::new(1, 6) {
            continue;
        }
        // No on-chain tx for a snapshot delta: a stable synthetic id keyed by the diff coordinates.
        let tx = format!("kamino:{obligation}:{ts}:{}:{mint}", kind_tag(kind));
        events.push(HistoryEvent::new(kind, amount, tx, Timestamp(ts)));
    }
    Ok(())
}

fn kind_tag(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Deposit => "deposit",
        EventKind::Withdraw => "withdraw",
        EventKind::Borrow => "borrow",
        EventKind::Repay => "repay",
        EventKind::SupplyInterest => "supply_interest",
        EventKind::BorrowInterest => "borrow_interest",
        _ => "event",
    }
}

/// Per-reserve metadata joined from `reserves/metrics`.
struct ReserveMeta {
    symbol: String,
    mint: String,
    max_ltv: Option<Decimal>,
    supply_apy: Option<Decimal>,
    borrow_apy: Option<Decimal>,
}

fn build_reserve_map(reserves: &serde_json::Value) -> Result<HashMap<String, ReserveMeta>, Error> {
    let arr = reserves.as_array().ok_or_else(|| Error::Integrity {
        message: "Kamino reserves response was not an array".into(),
    })?;
    let mut map = HashMap::with_capacity(arr.len());
    for r in arr {
        let Some(reserve) = r.get("reserve").and_then(|v| v.as_str()) else {
            continue;
        };
        map.insert(
            reserve.to_string(),
            ReserveMeta {
                symbol: r
                    .get("liquidityToken")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                mint: r
                    .get("liquidityTokenMint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                max_ltv: decimal_at(r, "/maxLtv"),
                supply_apy: decimal_at(r, "/supplyApy"),
                borrow_apy: decimal_at(r, "/borrowApy"),
            },
        );
    }
    Ok(map)
}

async fn parse_obligation(
    ob: &serde_json::Value,
    live: &serde_json::Value,
    market: &str,
    rates: &LegRates<'_>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
) -> Result<LendingPosition, Error> {
    // An owner can hold several obligations across markets; the obligation address is the only
    // stable identity separating them.
    let obligation = obligation_address(ob)?.to_string();

    // The API does not return a health factor; derive it from the point's refreshed stats so it
    // describes the same moment as the legs.
    let liq_limit = decimal_at(live, "/refreshedStats/borrowLiquidationLimit");
    let adj_debt = decimal_at(live, "/refreshedStats/userTotalBorrowBorrowFactorAdjusted");
    let health_factor = match (liq_limit, adj_debt) {
        (Some(l), Some(a)) if a > Decimal::ZERO => l.checked_div(a),
        _ => None,
    };

    // The live point and the account must hold the same reserves; a leg present on one side only
    // means the point does not describe this obligation.
    for side in ["deposits", "borrows"] {
        let held = state_reserves(ob, side);
        let seen = live_reserves(live, side);
        if held != seen {
            return Err(Error::Integrity {
                message: format!(
                    "Kamino obligation {obligation} {side} differ between the account {held:?} and its newest history point {seen:?}"
                ),
            });
        }
    }

    let mut supplied = Vec::new();
    for dep in live_legs(live, "deposits") {
        let Some((raw, reserve, usd)) = live_leg(dep)? else {
            continue;
        };
        let (amount, meta) =
            build_leg(reserve, raw, usd, rates.reserves, rpc, decimals_cache).await?;
        // Per-asset liquidation threshold is not exposed by reserves/metrics (only maxLtv);
        // a consumer recomputes it from the reserve config if needed.
        supplied.push(
            SuppliedAsset::new(amount)
                .with_max_ltv(meta.and_then(|m| m.max_ltv))
                .with_collateral(true, true)
                .with_rates(rates.for_leg(reserve, false)),
        );
    }

    let mut borrowed = Vec::new();
    for bor in live_legs(live, "borrows") {
        let Some((raw, reserve, usd)) = live_leg(bor)? else {
            continue;
        };
        let (amount, _) = build_leg(reserve, raw, usd, rates.reserves, rpc, decimals_cache).await?;
        borrowed.push(BorrowedAsset::new(amount).with_rates(rates.for_leg(reserve, true)));
    }

    if !borrowed.is_empty() && health_factor.is_none() {
        return Err(Error::Integrity {
            message: "Kamino obligation has debt but no health factor could be derived".into(),
        });
    }

    Ok(LendingPosition::new(supplied, borrowed)
        .with_health_factor(health_factor)
        .with_market_id(Some(obligation))
        .with_parent_market(Some(market.to_string())))
}

fn live_legs<'a>(
    live: &'a serde_json::Value,
    side: &str,
) -> impl Iterator<Item = &'a serde_json::Value> {
    live.get(side)
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
}

/// Reserves with a non-zero leg in the newest history point.
fn live_reserves(live: &serde_json::Value, side: &str) -> BTreeSet<String> {
    live_legs(live, side)
        .filter(|l| {
            l.get("amount")
                .and_then(|v| v.as_str())
                .and_then(base_units)
                .is_some_and(|a| !a.is_zero())
        })
        .filter_map(|l| {
            l.get("reserve")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Whether the account `state` holds any leg. A `state` without both leg arrays is not an empty
/// obligation but an unreadable one, so it fails closed.
fn holds_legs(ob: &serde_json::Value) -> Result<bool, Error> {
    for side in ["deposits", "borrows"] {
        if !ob
            .pointer(&format!("/state/{side}"))
            .is_some_and(|v| v.is_array())
        {
            return Err(Error::Integrity {
                message: format!("Kamino obligation account has no `state.{side}` array"),
            });
        }
    }
    Ok(!state_reserves(ob, "deposits").is_empty() || !state_reserves(ob, "borrows").is_empty())
}

/// Reserves holding at least one whole base unit in the on-chain account `state`, the same
/// threshold [`live_reserves`] applies (unused slots are zeroed; debt is a `2^60` scaled fraction).
/// A missing or unparseable amount counts as held, so it surfaces as a mismatch instead of
/// vanishing.
fn state_reserves(ob: &serde_json::Value, side: &str) -> BTreeSet<String> {
    let (key, amount, one_unit) = if side == "borrows" {
        ("borrowReserve", "borrowedAmountSf", U256::from(1u64 << 60))
    } else {
        ("depositReserve", "depositedAmount", U256::from(1u64))
    };
    ob.pointer(&format!("/state/{side}"))
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|l| {
            l.get(amount)
                .and_then(|v| v.as_str())
                .and_then(|s| U256::from_str(s).ok())
                .is_none_or(|a| a >= one_unit)
        })
        .filter_map(|l| l.get(key).and_then(|v| v.as_str()).map(str::to_string))
        .collect()
}

/// One leg of a history point: whole base units (the API carries accrued fractions of a base unit,
/// which are dropped), its reserve, and its refreshed USD value. `None` for a leg under one base
/// unit, whose USD the API may print in exponent form and which is not a holding.
fn live_leg(leg: &serde_json::Value) -> Result<Option<(U256, &str, Decimal)>, Error> {
    let raw = leg
        .get("amount")
        .and_then(|v| v.as_str())
        .and_then(base_units)
        .ok_or_else(|| Error::Integrity {
            message: "Kamino history leg missing a numeric `amount`".into(),
        })?;
    if raw.is_zero() {
        return Ok(None);
    }
    let reserve = leg
        .get("reserve")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Integrity {
            message: "Kamino history leg missing `reserve`".into(),
        })?;
    let usd = leg
        .get("marketValueRefreshed")
        .and_then(|v| v.as_str())
        .and_then(api_decimal)
        .ok_or_else(|| Error::Integrity {
            message: format!(
                "Kamino history leg for reserve {reserve} has no numeric `marketValueRefreshed`"
            ),
        })?;
    Ok(Some((raw, reserve, usd)))
}

/// The whole base units of a non-negative amount as the API prints it, the fraction dropped: a
/// decimal (`5200000000.42793513`), or exponent form with a negative exponent, which the API uses
/// for values under 1e-7 (`4e-7` is under one base unit, so 0). A positive or zero exponent, a
/// sign, or anything but digits around one `.` is not an amount the API prints, so it is `None`.
fn base_units(s: &str) -> Option<U256> {
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => {
            let e = e.parse::<i64>().ok()?;
            if e >= 0 {
                return None;
            }
            (m, e)
        }
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Shifting the point left by -exp keeps the leading `int.len() + exp` digits of int+frac.
    let keep = (int.len() as i64).saturating_add(exp);
    if keep <= 0 {
        return Some(U256::ZERO);
    }
    let whole = &int[..usize::try_from(keep).ok()?.min(int.len())];
    U256::from_str_radix(whole, 10).ok()
}

/// Parse a non-negative number as the Kamino API prints it: a decimal whose fraction can exceed
/// `Decimal`'s 28 digits, or exponent notation for very small values (`4e-7`). Magnitudes below
/// `Decimal`'s smallest step read as zero.
fn api_decimal(s: &str) -> Option<Decimal> {
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac = &frac[..frac.len().min(12)];
    let base = Decimal::from_str(&if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    })
    .ok()?;
    let step = Decimal::from(10u64.checked_pow(exp.unsigned_abs().min(19))?);
    match exp {
        0 => Some(base),
        e if e > 0 => base.checked_mul(Decimal::from(10u64.checked_pow(e.unsigned_abs())?)),
        e if e < -28 => Some(Decimal::ZERO),
        e if e < -19 => base
            .checked_div(step)?
            .checked_div(Decimal::from(10u64.pow(e.unsigned_abs() - 19))),
        _ => base.checked_div(step),
    }
}

/// Build the [`Amount`] for one deposit/borrow leg from whole base units, scaled by the mint's
/// on-chain decimals and carrying the leg's refreshed USD value.
async fn build_leg<'a>(
    reserve: &str,
    raw: U256,
    usd: Decimal,
    reserves: &'a HashMap<String, ReserveMeta>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
) -> Result<(Amount, Option<&'a ReserveMeta>), Error> {
    let meta = reserves.get(reserve);
    let (symbol, mint) = match meta {
        Some(m) => (m.symbol.clone(), m.mint.clone()),
        None => (String::new(), String::new()),
    };
    if mint.is_empty() {
        return Err(Error::Integrity {
            message: format!("Kamino reserve {reserve} not found in metrics"),
        });
    }

    let decimals = match decimals_cache.get(&mint) {
        Some(d) => *d,
        None => {
            let d = get_mint_decimals(rpc, &mint).await?;
            decimals_cache.insert(mint.clone(), d);
            d
        }
    };

    let usd = Money {
        amount: usd,
        currency: Currency::Usd,
    };
    let amount =
        Amount::from_raw(Token::solana(symbol, Some(mint), decimals), raw)?.with_usd(Some(usd));

    Ok((amount, meta))
}

/// Read a decimal from a JSON string or number literal at `pointer` (never via `f64`).
fn decimal_at(v: &serde_json::Value, pointer: &str) -> Option<Decimal> {
    let node = v.pointer(pointer)?;
    if let Some(s) = node.as_str() {
        Decimal::from_str(s).ok().or_else(|| api_decimal(s))
    } else if node.is_number() {
        Decimal::from_str(&node.to_string()).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    const H: i64 = 3600;

    /// `n` consecutive hourly points of `value`, the newest at `newest`.
    fn hourly(newest: i64, n: i64, value: &str) -> BTreeMap<i64, Decimal> {
        (0..n).map(|i| (newest - i * H, d(value))).collect()
    }

    #[test]
    fn trailing_mean_coverage_boundary_is_ninety_percent() {
        let newest = 1_000 * H;
        // 10h window: 9 of 10 points is exactly 90% and passes; 8 does not.
        assert_eq!(
            trailing_mean(&hourly(newest, 9, "0.05"), 10, newest),
            Some((d("0.05"), newest))
        );
        assert_eq!(trailing_mean(&hourly(newest, 8, "0.05"), 10, newest), None);
        // 24h: 21.6 points needed, so 22 passes and 21 does not.
        assert!(trailing_mean(&hourly(newest, 22, "0.05"), 24, newest).is_some());
        assert_eq!(trailing_mean(&hourly(newest, 21, "0.05"), 24, newest), None);
        // 168h: 151.2 needed, so 152 passes and 151 does not.
        assert!(trailing_mean(&hourly(newest, 152, "0.05"), 168, newest).is_some());
        assert_eq!(
            trailing_mean(&hourly(newest, 151, "0.05"), 168, newest),
            None
        );
    }

    #[test]
    fn trailing_mean_counts_only_points_inside_the_window() {
        let newest = 1_000 * H;
        // 30 points but a 24h window: only the newest 24 are averaged. The 6 older ones carry a
        // different value, so including them would move the mean.
        let mut points = hourly(newest, 24, "0.06");
        points.extend((24..30).map(|i| (newest - i * H, d("0.90"))));
        assert_eq!(
            trailing_mean(&points, 24, newest),
            Some((d("0.06"), newest))
        );
        // A gap inside the window counts against coverage, it is not filled from outside.
        let gappy: BTreeMap<i64, Decimal> = hourly(newest, 30, "0.06")
            .into_iter()
            .filter(|(ts, _)| (newest - ts) / H % 8 != 3)
            .collect();
        assert_eq!(trailing_mean(&gappy, 24, newest), None);
    }

    #[test]
    fn trailing_mean_rejects_a_stale_newest_point() {
        let newest = 1_000 * H;
        let points = hourly(newest, 24, "0.05");
        assert!(trailing_mean(&points, 24, newest + MAX_RATE_POINT_AGE_SECS).is_some());
        assert_eq!(
            trailing_mean(&points, 24, newest + MAX_RATE_POINT_AGE_SECS + 1),
            None
        );
    }

    #[test]
    fn trailing_mean_counts_distinct_hours_not_points() {
        let newest = 1_000 * H;
        // 24 points five minutes apart cover only 2 clock hours of a 24h window.
        let dense: BTreeMap<i64, Decimal> =
            (0..24).map(|i| (newest - i * 300, d("0.05"))).collect();
        assert_eq!(trailing_mean(&dense, 24, newest), None);
    }

    #[test]
    fn trailing_mean_rejects_a_newest_point_after_now() {
        let newest = 1_000 * H;
        let points = hourly(newest, 24, "0.05");
        assert!(trailing_mean(&points, 24, newest).is_some());
        assert_eq!(trailing_mean(&points, 24, newest - 1), None);
    }

    #[test]
    fn trailing_mean_of_empty_history_is_none() {
        assert_eq!(trailing_mean(&BTreeMap::new(), 24, 1_000 * H), None);
    }

    #[test]
    fn reserve_rates_skip_unusable_points_and_windows_independently() {
        let newest = 1_000 * H;
        // 30 hourly points: enough for 24h, not for 168h. Borrow is missing from one point.
        let history: Vec<serde_json::Value> = (0..30)
            .map(|i| {
                let ts = rfc3339(newest - i * H);
                if i == 5 {
                    serde_json::json!({"timestamp": ts, "metrics": {"supplyInterestAPY": 0.05}})
                } else {
                    serde_json::json!({"timestamp": ts, "metrics": {"supplyInterestAPY": 0.05, "borrowInterestAPY": 0.07}})
                }
            })
            .chain([serde_json::json!({"timestamp": "garbage", "metrics": {"borrowInterestAPY": 9}})])
            .collect();
        let rates = reserve_rates(&serde_json::json!({ "history": history }), newest);
        let w24 = RateWindow::Trailing { hours: 24 };
        assert_eq!(
            rates.supply.get(RateBasis::Apy, w24).unwrap().value,
            d("0.05")
        );
        assert_eq!(
            rates.borrow.get(RateBasis::Apy, w24).unwrap().value,
            d("0.07")
        );
        assert_eq!(
            rates.borrow.get(RateBasis::Apy, w24).unwrap().as_of,
            Timestamp(newest)
        );
        assert!(rates
            .supply
            .get(RateBasis::Apy, RateWindow::Trailing { hours: 168 })
            .is_none());
        assert_eq!(rates.supply.len(), 1);
        assert_eq!(rates.borrow.len(), 1);
        assert!(reserve_rates(&serde_json::json!({}), newest)
            .supply
            .is_empty());
    }

    #[test]
    fn base_units_keeps_whole_units_and_refuses_malformed_amounts() {
        let u = |n: u64| Some(U256::from(n));
        assert_eq!(base_units("5200000000"), u(5_200_000_000));
        assert_eq!(base_units("5200000000.42793513"), u(5_200_000_000));
        assert_eq!(base_units("0.4"), u(0));
        assert_eq!(base_units("5."), u(5));
        // Exponent form with a negative exponent: under one base unit reads as 0, a larger
        // mantissa keeps its whole units.
        assert_eq!(base_units("4e-7"), u(0));
        assert_eq!(base_units("5E-8"), u(0));
        assert_eq!(base_units("1.5e-9"), u(0));
        assert_eq!(base_units("12345e-2"), u(123));
        assert_eq!(base_units("12345.678e-5"), u(0));
        assert_eq!(base_units("1e-400000"), u(0));
        for bad in [
            "5000000000.junk",
            "5.3.2",
            "7e9",
            "7e+9",
            "7e0",
            "7e-",
            "e-7",
            "-5",
            "-4e-7",
            "+5",
            ".5",
            "",
            " 5",
            "5 ",
        ] {
            assert_eq!(base_units(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn api_decimal_reads_the_shapes_the_api_prints() {
        assert_eq!(
            api_decimal("8860.502505082070346209969544206781459677"),
            Some(d("8860.502505082070"))
        );
        assert_eq!(
            api_decimal("2.5981563428673729727e-9"),
            Some(d("0.000000002598156342867"))
        );
        assert_eq!(api_decimal("4e-7"), Some(d("0.0000004")));
        assert_eq!(api_decimal("7.805e2"), Some(d("780.5")));
        assert_eq!(
            api_decimal("1.2455e-24"),
            Some(d("0.0000000000000000000000012455"))
        );
        assert_eq!(api_decimal("3e-40"), Some(Decimal::ZERO));
        assert_eq!(api_decimal("-780.5"), None);
        assert_eq!(api_decimal("1e400"), None);
        assert_eq!(api_decimal("12a"), None);
        assert_eq!(api_decimal(""), None);
    }

    #[test]
    fn accrual_bound_is_exact_and_scales_with_the_gap() {
        const DAY: u64 = 86_400;
        let u = U256::from;
        let prev = u(1_000_000_000u64);
        // 0.2% of 1e9 is 2e6 per day: at the bound is accrual, one base unit over is not.
        assert!(within_accrual_bound(prev, u(2_000_000u64), DAY));
        assert!(!within_accrual_bound(prev, u(2_000_001u64), DAY));
        // Two days: twice the bound.
        assert!(within_accrual_bound(prev, u(4_000_000u64), 2 * DAY));
        assert!(!within_accrual_bound(prev, u(4_000_001u64), 2 * DAY));
        // A longer gap is credited two days at most: 103 days would allow 2.06e8.
        assert!(within_accrual_bound(prev, u(4_000_000u64), 103 * DAY));
        assert!(!within_accrual_bound(prev, u(4_000_001u64), 103 * DAY));
        assert!(!within_accrual_bound(prev, u(200_000_000u64), 103 * DAY));
        // Half a day is half the bound, never rounded up to a whole day.
        assert!(within_accrual_bound(prev, u(1_000_000u64), DAY / 2));
        assert!(!within_accrual_bound(prev, u(1_000_001u64), DAY / 2));
        // One hour: 2e6 / 24 = 83_333.33, so 83_333 fits and 83_334 does not.
        assert!(within_accrual_bound(prev, u(83_333u64), 3_600));
        assert!(!within_accrual_bound(prev, u(83_334u64), 3_600));
        // No balance or no elapsed time allows no accrual.
        assert!(!within_accrual_bound(U256::ZERO, u(1u64), DAY));
        assert!(!within_accrual_bound(prev, u(1u64), 0));
    }

    #[test]
    fn accrual_bound_never_overflows() {
        const DAY: u64 = 86_400;
        let max = U256::MAX;
        assert!(!within_accrual_bound(max, max, DAY));
        // MAX / 500 is 0.2% of MAX rounded down: inside one day's bound.
        assert!(within_accrual_bound(max, max / U256::from(500u64), DAY));
        assert!(!within_accrual_bound(
            max,
            max / U256::from(500u64) + U256::from(1u64),
            DAY
        ));
        // The longest gap is credited two days: twice the one-day bound, never the whole balance.
        assert!(within_accrual_bound(
            max,
            max / U256::from(250u64),
            u64::MAX
        ));
        assert!(!within_accrual_bound(max, max, u64::MAX));
    }

    #[test]
    fn history_constants_have_their_documented_values() {
        assert_eq!(MAX_ACCRUAL_PER_DAY, d("0.002"));
        assert_eq!(HISTORY_DAY_SECS, 86_400);
        assert_eq!(MAX_ACCRUAL_GAP_SECS, 2 * 86_400);
        assert_eq!(HISTORY_SETTLE_SECS, 4 * 3_600);
    }
}
