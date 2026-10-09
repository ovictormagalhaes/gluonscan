//! # gluonscan-kamino
//!
//! Kamino adapter (Solana lending), [`Source::Api`] + on-chain mint decimals: reads a wallet's
//! obligations from the Kamino HTTP API across its markets and normalizes them into
//! [`LendingPosition`]s. USD values come from the API's scaled-fraction `marketValueSf` (divided by
//! `2^60`); the human token amount is the raw underlying amount scaled by the mint's decimals (read
//! on-chain). The health factor is derived (`borrowLiquidationLimit / borrowFactorAdjustedDebt`)
//! since the API does not expose it directly.

use std::collections::HashMap;
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
                positions.push(Position::Lending(
                    parse_obligation(ob, &reserve_map, rpc, &mut decimals_cache).await?,
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
    reserves: &HashMap<String, ReserveMeta>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
) -> Result<LendingPosition, Error> {
    // An owner can hold several obligations across markets; the obligation address is the only
    // stable identity separating them.
    let obligation = ob
        .get("obligationAddress")
        .and_then(|v| v.as_str())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::Integrity {
            message: "Kamino obligation missing `obligationAddress`".into(),
        })?
        .to_string();

    // The API does not return a health factor; derive it from the refreshed stats.
    let liq_limit = decimal_at(ob, "/refreshedStats/borrowLiquidationLimit");
    let adj_debt = decimal_at(ob, "/refreshedStats/userTotalBorrowBorrowFactorAdjusted");
    let health_factor = match (liq_limit, adj_debt) {
        (Some(l), Some(a)) if a > Decimal::ZERO => l.checked_div(a),
        _ => None,
    };

    // The per-asset legs live under the decoded on-chain account `state` (the top-level
    // `deposits`/`borrows` are empty aggregation maps). Deposit amounts are plain base units;
    // borrow amounts are scaled fractions (`borrowedAmountSf`, divide by 2^60).
    let mut supplied = Vec::new();
    for dep in ob
        .pointer("/state/deposits")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let raw = int_at(dep, "depositedAmount")?;
        if raw.is_zero() {
            continue;
        }
        let (amount, meta) =
            build_leg(dep, "depositReserve", raw, reserves, rpc, decimals_cache).await?;
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
    for bor in ob
        .pointer("/state/borrows")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let raw = sf_to_base(&int_at(bor, "borrowedAmountSf")?);
        if raw.is_zero() {
            continue;
        }
        let (amount, meta) =
            build_leg(bor, "borrowReserve", raw, reserves, rpc, decimals_cache).await?;
        borrowed.push(BorrowedAsset::new(amount).with_apy(meta.and_then(|m| m.borrow_apy)));
    }

    if !borrowed.is_empty() && health_factor.is_none() {
        return Err(Error::Integrity {
            message: "Kamino obligation has debt but no health factor could be derived".into(),
        });
    }

    Ok(LendingPosition::new(supplied, borrowed)
        .with_health_factor(health_factor)
        .with_market_id(Some(obligation)))
}

/// Build the [`Amount`] for one deposit/borrow leg from a pre-computed raw base-unit amount: scaled
/// by the mint's on-chain decimals, priced from the scaled-fraction `marketValueSf` (`/ 2^60`).
async fn build_leg<'a>(
    leg: &serde_json::Value,
    reserve_key: &str,
    raw: U256,
    reserves: &'a HashMap<String, ReserveMeta>,
    rpc: &dyn ChainProvider,
    decimals_cache: &mut HashMap<String, u8>,
) -> Result<(Amount, Option<&'a ReserveMeta>), Error> {
    let reserve = leg
        .get(reserve_key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Integrity {
            message: format!("Kamino leg missing `{reserve_key}`"),
        })?;
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

    let usd = leg
        .get("marketValueSf")
        .and_then(|v| v.as_str())
        .and_then(sf_to_usd)
        .map(|amount| Money {
            amount,
            currency: Currency::Usd,
        });

    let amount = Amount::from_raw(Token::solana(symbol, Some(mint), decimals), raw)?.with_usd(usd);

    Ok((amount, meta))
}

/// Read a base-10 integer amount (a JSON string) into a [`U256`].
fn int_at(leg: &serde_json::Value, key: &str) -> Result<U256, Error> {
    let s = leg
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Integrity {
            message: format!("Kamino leg missing `{key}`"),
        })?;
    U256::from_str(s).map_err(|e| Error::Integrity {
        message: format!("Kamino `{key}` not an integer: {e}"),
    })
}

/// Convert a scaled-fraction amount (`2^60` scale) to whole base units by integer division.
fn sf_to_base(sf: &U256) -> U256 {
    // 2^60 = 1_152_921_504_606_846_976.
    sf / U256::from(1_152_921_504_606_846_976u64)
}

/// Kamino's scaled-fraction (`Sf`) values are fixed-point with a `2^60` scale.
fn sf_to_usd(sf: &str) -> Option<Decimal> {
    let n = Decimal::from_str(sf).ok()?;
    // 2^60 = 1_152_921_504_606_846_976.
    n.checked_div(Decimal::from(1_152_921_504_606_846_976u64))
}

/// Read a decimal from a JSON string or number literal at `pointer` (never via `f64`).
fn decimal_at(v: &serde_json::Value, pointer: &str) -> Option<Decimal> {
    let node = v.pointer(pointer)?;
    if let Some(s) = node.as_str() {
        Decimal::from_str(s).ok()
    } else if node.is_number() {
        Decimal::from_str(&node.to_string()).ok()
    } else {
        None
    }
}
