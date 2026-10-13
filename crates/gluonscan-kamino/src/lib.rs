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

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

use alloy_primitives::U256;
use async_trait::async_trait;
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, ChainProvider, Complete, Ctx, Currency, Detail,
    Error, EventKind, History, HistoryEvent, LendingPosition, Money, Position, Protocol,
    ProtocolAdapter, Provenance, Reading, Source, Staleness, SuppliedAsset, Timestamp, Token,
    Wallet,
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

            for ob in items {
                // An emptied obligation stays listed but holds no legs: nothing to read live.
                let live = if holds_legs(ob)? {
                    latest_obligation_point(self.base(), market, ob, cx).await?
                } else {
                    serde_json::Value::Null
                };
                positions.push(Position::Lending(
                    parse_obligation(ob, &live, market, &reserve_map, rpc, &mut decimals_cache)
                        .await?,
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
        let (obligation, market) = match selector.split_once('|') {
            Some((o, m)) if !o.is_empty() && !m.is_empty() => (o.to_string(), m.to_string()),
            _ => {
                let wallet = owner.solana()?;
                let market = resolve_market(self.base(), wallet, selector, cx).await?;
                (selector.to_string(), market)
            }
        };
        let (obligation, market) = (obligation.as_str(), market.as_str());
        let rpc = cx.rpc()?.as_ref();
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
        let mut snaps: Vec<&serde_json::Value> = hist
            .pointer("/history")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        snaps.sort_by_key(|s| snapshot_ts(s));

        let mut decimals_cache: HashMap<String, u8> = HashMap::new();
        let mut events: Vec<HistoryEvent> = Vec::new();
        for window in snaps.windows(2) {
            let (prev, curr) = (window[0], window[1]);
            let ts = snapshot_ts(curr);
            diff_side(
                prev,
                curr,
                "deposits",
                EventKind::Deposit,
                EventKind::Withdraw,
                obligation,
                ts,
                &mint_symbol,
                rpc,
                &mut decimals_cache,
                &mut events,
            )
            .await?;
            diff_side(
                prev,
                curr,
                "borrows",
                EventKind::Borrow,
                EventKind::Repay,
                obligation,
                ts,
                &mint_symbol,
                rpc,
                &mut decimals_cache,
                &mut events,
            )
            .await?;
        }
        events.retain(|e| e.at.0 > since_ts);
        events.sort_by_key(|e| e.at.0);

        let history = History::new(
            Protocol::Kamino,
            Chain::Solana,
            events,
            Provenance::new(Source::Api, Chain::Solana, cx.clock.now(), Staleness::Live),
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
        let url = format!("{base}/kamino-market/{market}/users/{wallet}/obligations");
        let raw = cx.http.get(&url, &[]).await?;
        let obligations: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                message: format!("Kamino obligations not JSON: {e}"),
            })?;
        let items = obligations.as_array().ok_or_else(|| Error::Integrity {
            message: format!("Kamino market {market} returned a non-array obligations response"),
        })?;
        for ob in items {
            let matches = ob
                .get("obligationAddress")
                .and_then(|v| v.as_str())
                .map(|a| a.eq_ignore_ascii_case(obligation))
                .unwrap_or(false);
            if matches {
                return Ok((*market).to_string());
            }
        }
    }
    Err(Error::Permanent {
        message: format!("Kamino obligation {obligation} not found in any market for the owner"),
    })
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
    if !address
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() && !matches!(b, b'0' | b'O' | b'I' | b'l'))
    {
        return Err(Error::Integrity {
            message: format!("Kamino obligation address {address:?} is not base58"),
        });
    }
    Ok(address)
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

/// Sum raw base-unit amounts per mint from a snapshot side (`"deposits"` | `"borrows"`).
fn side_amounts(snap: &serde_json::Value, side: &str) -> HashMap<String, i128> {
    let mut out: HashMap<String, i128> = HashMap::new();
    for e in snap
        .get(side)
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(mint) = e.get("mintAddress").and_then(|v| v.as_str()) else {
            continue;
        };
        let raw = e
            .get("amount")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i128>().ok())
            .unwrap_or(0);
        *out.entry(mint.to_string()).or_insert(0) += raw;
    }
    out
}

/// Diff one snapshot side between two consecutive snapshots, pushing an event per mint whose balance
/// changed: a positive delta is `inc_kind` (deposit/borrow), a negative delta is `dec_kind`
/// (withdraw/repay). Sub-dust deltas are skipped.
#[allow(
    clippy::too_many_arguments,
    reason = "internal side-diff helper threading prev/curr state, injected rpc + decimals cache, and an output event sink — not a data cluster"
)]
async fn diff_side(
    prev: &serde_json::Value,
    curr: &serde_json::Value,
    side: &str,
    inc_kind: EventKind,
    dec_kind: EventKind,
    obligation: &str,
    ts: i64,
    mint_symbol: &HashMap<String, String>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
    events: &mut Vec<HistoryEvent>,
) -> Result<(), Error> {
    let prev_m = side_amounts(prev, side);
    let curr_m = side_amounts(curr, side);
    let mut mints: Vec<&String> = prev_m.keys().chain(curr_m.keys()).collect();
    mints.sort();
    mints.dedup();
    for mint in mints {
        let delta = curr_m.get(mint).copied().unwrap_or(0) - prev_m.get(mint).copied().unwrap_or(0);
        if delta == 0 {
            continue;
        }
        let kind = if delta > 0 { inc_kind } else { dec_kind };
        let decimals = match decimals_cache.get(mint) {
            Some(d) => *d,
            None => {
                let d = get_mint_decimals(rpc, mint).await?;
                decimals_cache.insert(mint.clone(), d);
                d
            }
        };
        let symbol = mint_symbol.get(mint).cloned().unwrap_or_default();
        let amount = Amount::from_raw(
            Token::solana(symbol, Some(mint.clone()), decimals),
            U256::from(delta.unsigned_abs()),
        )?;
        // Sub-dust snapshot deltas are rounding noise, not real deposits/withdrawals.
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
    reserves: &HashMap<String, ReserveMeta>,
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
        let (amount, meta) = build_leg(reserve, raw, usd, reserves, rpc, decimals_cache).await?;
        // Per-asset liquidation threshold is not exposed by reserves/metrics (only maxLtv);
        // a consumer recomputes it from the reserve config if needed.
        supplied.push(
            SuppliedAsset::new(amount)
                .with_max_ltv(meta.and_then(|m| m.max_ltv))
                .with_collateral(true, true)
                .with_apy(meta.and_then(|m| m.supply_apy)),
        );
    }

    let mut borrowed = Vec::new();
    for bor in live_legs(live, "borrows") {
        let Some((raw, reserve, usd)) = live_leg(bor)? else {
            continue;
        };
        let (amount, meta) = build_leg(reserve, raw, usd, reserves, rpc, decimals_cache).await?;
        borrowed.push(BorrowedAsset::new(amount).with_apy(meta.and_then(|m| m.borrow_apy)));
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

/// The integer part of a non-negative decimal string, as base units.
fn base_units(s: &str) -> Option<U256> {
    let int = s.split('.').next()?;
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    U256::from_str(int).ok()
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
}
