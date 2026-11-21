//! # gluonscan-hyperliquid
//!
//! Hyperliquid adapter, **API backend** ([`Source::Api`]): reads a wallet's open perp positions
//! from the public `clearinghouseState` info endpoint and normalizes each into a [`PerpPosition`],
//! and its spot balances from `spotClearinghouseState`.
//!
//! Hyperliquid is USD-margined: size, prices, PnL, funding and margin all come back as decimal
//! strings and are parsed straight into [`Decimal`] — never through `f64`. The mark price is derived
//! from the notional the API already returns (`positionValue / |size|`), so no second call is
//! needed. Values the API does not provide (a null `liquidationPx`, an absent `cumFunding`) stay
//! `None` rather than being fabricated.
//!
//! The account's balances are emitted as [`Position::Wallet`]s: USDC once, plus every other spot
//! token priced at its USDC pair's mark. Where the USDC lives depends on the account mode
//! (`userAbstraction`): a **unified** account holds one balance per asset in the spot
//! clearinghouse, which already includes the perp margin and its unrealized PnL, so the USDC is the
//! spot balance alone; a **separate** account (`disabled` / `default` / `dexAbstraction`) keeps
//! perp equity (`marginSummary.accountValue`) apart from spot, so the USDC is the sum of the two.
//! Per-position `marginUsed` is a margin requirement, not owned capital, so it is never summed.
//! Portfolio margin (spot borrowing) and unrecognized modes fail closed rather than risking a
//! double count. An account with only spot balances and no perp position still reports them.
//!
//! Each position's margin mode (isolated / cross) comes free in the same `clearinghouseState`
//! payload (`leverage.type`). At [`Detail::Full`] the adapter additionally reads the account's
//! resting trigger orders from `frontendOpenOrders` (take-profit / stop-loss levels) and its recent
//! fills from `userFills` (each position's opening date). That enrichment is best-effort: it is not
//! net-worth-bearing, so a failure there leaves the fields `None` and never fails the equity read.
//!
//! History ([`Capability::History`]) is the account's perp fills from `userFillsByTime`, each an
//! open, close, flip or liquidation with its price, realized PnL and fee. Hyperliquid serves only
//! an account's most recent fills; a history that reaches that limit says so
//! ([`History::earlier_events_unavailable`]).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, EventKind, History, HistoryEvent,
    MarginMode, Money, PerpFill, PerpPosition, PerpSide, Position, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, Staleness, Timestamp, Token, Wallet, WalletBalance,
};
use rust_decimal::Decimal;
use serde_json::Value;

const INFO_API: &str = "https://api.hyperliquid.xyz/info";

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::History];

/// The spot token index of USDC.
const USDC_TOKEN_INDEX: u64 = 0;
/// Decimals for a spot balance whose token is not in the spot metadata.
const UNKNOWN_TOKEN_DECIMALS: u8 = 18;
/// Decimals carried by a perp fill's size, which has no on-chain base unit.
const PERP_SIZE_DECIMALS: u8 = 18;
/// Fills per `userFillsByTime` page.
const FILLS_PER_PAGE: usize = 2000;
/// How many of an account's most recent fills Hyperliquid serves at all.
const RETAINED_FILLS: usize = 10_000;
/// Pages that hold the retained fills, plus one for the boundary fills each page repeats.
const MAX_FILL_PAGES: usize = RETAINED_FILLS / FILLS_PER_PAGE + 1;
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Hyperliquid];

/// Hyperliquid adapter backed by the public info API (perp and spot clearinghouse state, fills).
#[derive(Debug, Default, Clone)]
pub struct Hyperliquid {
    endpoint: Option<String>,
}

impl Hyperliquid {
    /// Construct with the default public endpoint.
    pub fn new() -> Self {
        Hyperliquid { endpoint: None }
    }

    /// Override the endpoint (for a private gateway or tests).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(INFO_API)
    }
}

#[async_trait]
impl ProtocolAdapter for Hyperliquid {
    fn protocol(&self) -> Protocol {
        Protocol::Hyperliquid
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
        detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        let user = hyperliquid_user(owner, chain)?;
        let endpoint = self.endpoint();
        let spot_request = serde_json::json!({ "type": "spotClearinghouseState", "user": user });

        // Spot is read before the perp clearinghouse so that a separate-balance account can read it
        // again after it and detect USDC that moved between the two (see below).
        let balances = parse_spot_balances(&info(endpoint, spot_request.clone(), cx).await?)?;

        let json = info(
            endpoint,
            serde_json::json!({ "type": "clearinghouseState", "user": user }),
            cx,
        )
        .await?;
        // A valid request always returns the state object with an `assetPositions` array (empty when
        // the account holds no perps). A missing array means a malformed/error payload → fail closed.
        let asset_positions = json
            .get("assetPositions")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Integrity {
                message: "Hyperliquid response missing `assetPositions`".into(),
            })?;

        // The perp clearinghouse's margin equity. Net-worth-bearing in a separate-balance account, so
        // a present response that omits it fails closed rather than silently undercounting, and a
        // negative equity (a present figure that cannot be a balance) fails closed rather than being
        // dropped.
        let account_value = json
            .get("marginSummary")
            .and_then(|m| m.get("accountValue"))
            .and_then(Value::as_str)
            .and_then(|s| Decimal::from_str(s).ok())
            .ok_or_else(|| Error::Integrity {
                message: "Hyperliquid response missing/invalid `marginSummary.accountValue`".into(),
            })?;
        if account_value.is_sign_negative() {
            return Err(Error::Integrity {
                message: "Hyperliquid `marginSummary.accountValue` is negative".into(),
            });
        }

        let reading = |positions| {
            Reading::new(
                Protocol::Hyperliquid,
                chain,
                Source::Api,
                positions,
                Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
            )
        };
        if asset_positions.is_empty()
            && account_value.is_zero()
            && balances.iter().all(|b| b.total.is_zero())
        {
            return Ok(Complete::new(reading(Vec::new())));
        }

        let mode = account_mode(endpoint, &user, cx).await?;

        // A separate-balance account's USDC is summed from the spot and perp reads, which are not
        // one instant: a transfer between them landing in the gap would count the amount twice or
        // not at all. Any such transfer moves the spot USDC, so a second spot read that disagrees
        // with the first means the sum is not of one state, and the read is retried.
        if mode == AccountMode::Separate {
            let again = parse_spot_balances(&info(endpoint, spot_request, cx).await?)?;
            if spot_usdc(&again) != spot_usdc(&balances) {
                return Err(Error::Transient {
                    message: "Hyperliquid spot USDC changed during the read".into(),
                    retry_after: None,
                });
            }
        }

        // Resting take-profit / stop-loss levels and the fills that date each position are advisory
        // detail, not net-worth-bearing, so they are only fetched at Full and never fail the read: a
        // transport error or a malformed payload leaves the affected fields `None`.
        let (tpsl, fills) = if detail == Detail::Full && !asset_positions.is_empty() {
            (
                fetch_position_tpsl(endpoint, &user, cx).await,
                fetch_recent_fills(endpoint, &user, cx).await,
            )
        } else {
            (HashMap::new(), None)
        };

        let mut positions = Vec::with_capacity(asset_positions.len() + balances.len());
        for ap in asset_positions {
            let pos = ap.get("position").ok_or_else(|| Error::Integrity {
                message: "Hyperliquid assetPosition missing `position`".into(),
            })?;
            let mut perp = parse_perp(pos)?;
            if let Some(t) = tpsl.get(&perp.market) {
                perp = perp
                    .with_take_profit_price(t.take_profit)
                    .with_stop_loss_price(t.stop_loss);
            }
            if let Some(fills) = &fills {
                let opened = opened_at(fills, &perp.market, perp.side);
                perp = perp.with_opened_at(opened);
            }
            positions.push(Position::Perp(perp));
        }

        // The account's USDC. In a unified account the spot balance is the single balance for the
        // asset across spot and perps and already holds the perp margin and its unrealized PnL
        // (Hyperliquid documents the spot state as the source of truth there), so the perp equity is
        // not added again. In a separate-balance account the perp clearinghouse holds its own USDC.
        let usdc = match mode {
            AccountMode::Unified => spot_usdc(&balances),
            AccountMode::Separate => spot_usdc(&balances) + account_value,
        };
        if !usdc.is_zero() {
            // USDC keeps the 6 decimals of its canonical token (Hyperliquid's own wei precision is
            // 8), so the account balance has one identity across readings; `amount` stays exact.
            let amount = Amount::from_decimal(Token::new("USDC", None, 6), usdc)?
                .with_usd(Some(Money::usd(usdc)));
            positions.push(Position::Wallet(WalletBalance::new(amount)));
        }

        let others: Vec<&SpotBalance> = balances
            .iter()
            .filter(|b| b.token != Some(USDC_TOKEN_INDEX) && !b.total.is_zero())
            .collect();
        if !others.is_empty() {
            let held: HashSet<u64> = others.iter().filter_map(|b| b.token).collect();
            let market = fetch_spot_market(endpoint, &held, cx).await?;
            for b in others {
                let meta = b.token.and_then(|t| market.tokens.get(&t));
                let decimals = meta.map_or(UNKNOWN_TOKEN_DECIMALS, |m| m.wei_decimals);
                let usd = b
                    .token
                    .and_then(|t| market.usdc_price.get(&t))
                    .map(|px| Money::usd(*px * b.total));
                let amount =
                    Amount::from_decimal(Token::new(b.coin.clone(), None, decimals), b.total)?
                        .with_usd(usd);
                positions.push(Position::Wallet(WalletBalance::new(amount)));
            }
        }

        Ok(Complete::new(reading(positions)))
    }

    /// The account's perp fills. `position` selects one perp market (a [`PerpPosition::market`],
    /// e.g. `"BTC"`); `None` reads every market. `since` is exclusive at whole seconds, as the trait
    /// states, so a fill later in `since`'s own second is not returned: an incremental reader
    /// re-reads from `since - 1` and deduplicates by [`PerpFill::fill_id`].
    async fn read_history(
        &self,
        owner: &Wallet,
        chain: Chain,
        position: Option<&str>,
        since: Option<Timestamp>,
        cx: &Ctx,
    ) -> Result<Complete<History>, Error> {
        let user = hyperliquid_user(owner, chain)?;
        // `since` is exclusive and second-granular; `startTime` is inclusive and in milliseconds.
        let mut start_ms = since.map_or(0, |s| s.0.saturating_add(1).saturating_mul(1000).max(0));
        let mut seen = HashSet::new();
        let mut fills = Vec::new();
        let mut complete = false;
        for _ in 0..MAX_FILL_PAGES {
            let page = info(
                self.endpoint(),
                serde_json::json!({
                    "type": "userFillsByTime",
                    "user": user,
                    "startTime": start_ms,
                }),
                cx,
            )
            .await?;
            let page = page.as_array().ok_or_else(|| Error::Integrity {
                message: "Hyperliquid `userFillsByTime` response is not an array".into(),
            })?;
            let full = page.len() >= FILLS_PER_PAGE;
            let mut last_ms = start_ms;
            for raw in page {
                let fill = parse_fill(raw, &user)?;
                last_ms = last_ms.max(fill.time_ms);
                if seen.insert(fill.tid.clone()) {
                    fills.push(fill);
                }
            }
            if !full {
                complete = true;
                break;
            }
            // The page is ascending from `startTime`, so the next one starts at its last fill's time
            // (inclusive: that boundary fill comes back and is deduplicated by id). A full page
            // inside one millisecond cannot be paged past without skipping fills.
            if last_ms == start_ms {
                return Err(Error::Integrity {
                    message:
                        "Hyperliquid fills cannot be paginated: a full page shares one timestamp"
                            .into(),
                });
            }
            start_ms = last_ms;
        }

        // Every page came back full: fills after the last page (the newest) were not read, and a
        // history missing its latest trades must not pass as complete.
        if !complete {
            return Err(Error::Transient {
                message: "Hyperliquid fills did not end within the pages read".into(),
                retry_after: None,
            });
        }

        fills.sort_by(|a, b| fill_order(a).cmp(&fill_order(b)));
        // Hyperliquid serves only an account's most recent fills. Fewer than that many since `start`
        // means every fill since then is inside the window; reaching it means older ones may exist.
        let truncated = fills.len() >= RETAINED_FILLS;
        let mut events = Vec::new();
        for fill in fills {
            if !is_perp_market(&fill.coin) || position.is_some_and(|m| m != fill.coin) {
                continue;
            }
            events.push(fill_event(fill)?);
        }

        let mut history = History::new(
            Protocol::Hyperliquid,
            chain,
            events,
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        );
        if truncated {
            history = history.with_earlier_events_unavailable();
        }
        Ok(Complete::new(history))
    }
}

fn parse_perp(pos: &Value) -> Result<PerpPosition, Error> {
    let market = pos
        .get("coin")
        .and_then(Value::as_str)
        .ok_or_else(|| integrity("coin"))?;

    // `szi` is the signed position size: its sign is the side, its magnitude is the size.
    let szi = dec(pos, "szi")?;
    if szi.is_zero() {
        return Err(Error::Integrity {
            message: "Hyperliquid position has zero size".into(),
        });
    }
    let side = if szi.is_sign_negative() {
        PerpSide::Short
    } else {
        PerpSide::Long
    };
    let size = szi.abs();

    let entry = dec(pos, "entryPx")?;
    let position_value = dec(pos, "positionValue")?;
    // Mark derived from the notional the API already returned — no extra call, self-consistent with
    // the reported PnL. The zero-size guard above keeps this division safe.
    let mark = position_value / size;
    let pnl = dec(pos, "unrealizedPnl")?;
    let margin = dec(pos, "marginUsed")?;

    // Absent leverage → None; present but not an integer → fail closed (never silently drop a
    // present-but-malformed value).
    let leverage = match pos.get("leverage").and_then(|l| l.get("value")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(Decimal::from(
            v.as_i64().ok_or_else(|| integrity("leverage.value"))?,
        )),
    };

    // Margin mode rides in the same `leverage` object. It is advisory detail, not net-worth-bearing,
    // and Hyperliquid may add modes over time, so an unrecognized or absent value maps to None rather
    // than failing the read — the equity figure must not hinge on a label.
    let margin_mode = match pos
        .get("leverage")
        .and_then(|l| l.get("type"))
        .and_then(Value::as_str)
    {
        Some("isolated") => Some(MarginMode::Isolated),
        Some("cross") => Some(MarginMode::Cross),
        _ => None,
    };

    let liquidation_price = match pos.get("liquidationPx") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| integrity("liquidationPx"))?,
        ),
    };

    // Absent cumFunding → None (legitimately unknown); present but unparseable → fail closed, same
    // as liquidationPx. Hyperliquid reports cumFunding from the exchange's side: positive means the
    // position PAID funding (the account's `userFunding` usdc deltas sum to the exact negation).
    // The model reads negative = paid, so the value is negated.
    let funding = match pos.get("cumFunding").and_then(|c| c.get("sinceOpen")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(Money::usd(
            -v.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| integrity("cumFunding.sinceOpen"))?,
        )),
    };

    // Collateral is the USDC margin backing this position — a margin *requirement*, reported for
    // detail. Its USD value is deliberately left unset: the margin is already encompassed by the
    // account equity (emitted once as the account's USDC balance), so pricing it here would let a
    // consumer that sums position values count the same capital twice. The amount (USDC units)
    // carries the margin figure without presenting it as a separate priced holding.
    let collateral = vec![Amount::from_decimal(Token::new("USDC", None, 6), margin)?];

    Ok(PerpPosition::new(market, side, size)
        .with_collateral(collateral)
        .with_entry_price(Some(entry))
        .with_mark_price(Some(mark))
        .with_leverage(leverage)
        .with_unrealized_pnl(Some(Money::usd(pnl)))
        .with_funding(funding)
        .with_liquidation_price(liquidation_price)
        .with_margin_mode(margin_mode))
}

/// A position's resting take-profit / stop-loss trigger prices, as read from `frontendOpenOrders`.
#[derive(Default)]
struct PositionTpsl {
    take_profit: Option<Decimal>,
    stop_loss: Option<Decimal>,
}

/// Read the account's position-attached TP/SL trigger prices, keyed by market (`coin`).
///
/// Best-effort by contract: TP/SL are advisory reference lines, not net-worth-bearing, so every
/// failure mode degrades to "no levels" rather than failing the account read. A transport error or
/// a non-array body yields an empty map; within a valid array, only orders Hyperliquid flags as the
/// position's own TP/SL (`isPositionTpsl`) with a parseable `triggerPx` contribute — anything else
/// (a standalone trigger order, a malformed price) is skipped, never fabricated. The first trigger
/// of each kind per market wins (a position carries at most one take-profit and one stop-loss).
async fn fetch_position_tpsl(
    endpoint: &str,
    user: &str,
    cx: &Ctx,
) -> HashMap<String, PositionTpsl> {
    let body = serde_json::json!({ "type": "frontendOpenOrders", "user": user }).to_string();
    let Ok(raw) = cx.http.post(endpoint, body, &[]).await else {
        return HashMap::new();
    };
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(orders) = json.as_array() else {
        return HashMap::new();
    };

    let mut map: HashMap<String, PositionTpsl> = HashMap::new();
    for order in orders {
        if order.get("isPositionTpsl").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(coin) = order.get("coin").and_then(Value::as_str) else {
            continue;
        };
        let Some(trigger) = order
            .get("triggerPx")
            .and_then(Value::as_str)
            .and_then(|s| Decimal::from_str(s).ok())
        else {
            continue;
        };
        // Hyperliquid's `orderType` for a position trigger is one of "Take Profit Market/Limit" or
        // "Stop Market/Limit"; the kind, not the execution style, is what classifies the level.
        let order_type = order
            .get("orderType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let entry = map.entry(coin.to_string()).or_default();
        if order_type.contains("take profit") {
            entry.take_profit.get_or_insert(trigger);
        } else if order_type.contains("stop") {
            entry.stop_loss.get_or_insert(trigger);
        }
    }
    map
}

/// The Hyperliquid user address for `owner`, after checking the chain is Hyperliquid.
fn hyperliquid_user(owner: &Wallet, chain: Chain) -> Result<String, Error> {
    if chain != Chain::Hyperliquid {
        return Err(Error::Permanent {
            message: format!("Hyperliquid not configured for {chain:?}"),
        });
    }
    Ok(format!("{:#x}", owner.evm()?))
}

/// POST one info request and parse the JSON reply; a non-JSON reply fails closed.
async fn info(endpoint: &str, body: Value, cx: &Ctx) -> Result<Value, Error> {
    let raw = cx.http.post(endpoint, body.to_string(), &[]).await?;
    serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("Hyperliquid response was not valid JSON: {e}"),
    })
}

/// How an account's balances are split between spot and perps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountMode {
    /// One balance per asset across spot and perps, held in the spot clearinghouse.
    Unified,
    /// Spot and the perp clearinghouse hold separate balances.
    Separate,
}

/// Read the account's abstraction mode (`userAbstraction`). It decides whether the perp equity is
/// already inside the spot balance, so a mode this adapter does not model fails the read rather
/// than risking a double count or a missed balance. Portfolio margin can borrow against spot
/// collateral, whose debt the spot balances alone do not settle, so it is not modeled yet.
async fn account_mode(endpoint: &str, user: &str, cx: &Ctx) -> Result<AccountMode, Error> {
    let json = info(
        endpoint,
        serde_json::json!({ "type": "userAbstraction", "user": user }),
        cx,
    )
    .await?;
    match json.as_str() {
        Some("unifiedAccount") => Ok(AccountMode::Unified),
        Some("disabled" | "default" | "dexAbstraction") => Ok(AccountMode::Separate),
        Some("portfolioMargin") => Err(Error::Permanent {
            message: "Hyperliquid portfolio-margin accounts are not supported".into(),
        }),
        Some(other) => Err(Error::Integrity {
            message: format!("Hyperliquid account mode `{other}` is not recognized"),
        }),
        None => Err(Error::Integrity {
            message: "Hyperliquid `userAbstraction` response is not a string".into(),
        }),
    }
}

/// One spot balance from `spotClearinghouseState`.
struct SpotBalance {
    coin: String,
    /// The spot token index; absent on balances that are not a listed spot token.
    token: Option<u64>,
    total: Decimal,
}

/// Parse `spotClearinghouseState.balances`. Net-worth-bearing: a missing array, a balance without a
/// coin or a parseable total, or a negative total fails closed.
fn parse_spot_balances(json: &Value) -> Result<Vec<SpotBalance>, Error> {
    let balances = json
        .get("balances")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Integrity {
            message: "Hyperliquid spot response missing `balances`".into(),
        })?;
    balances
        .iter()
        .map(|b| {
            let coin = b
                .get("coin")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_field("spot balance", "coin"))?;
            let total = b
                .get("total")
                .and_then(Value::as_str)
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| bad_field("spot balance", "total"))?;
            if total.is_sign_negative() {
                return Err(Error::Integrity {
                    message: format!("Hyperliquid spot balance `{coin}` is negative"),
                });
            }
            let token = match b.get("token") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .ok_or_else(|| bad_field("spot balance", "token"))?,
                ),
            };
            Ok(SpotBalance {
                coin: coin.to_string(),
                token,
                total,
            })
        })
        .collect()
}

/// The account's spot USDC.
fn spot_usdc(balances: &[SpotBalance]) -> Decimal {
    balances
        .iter()
        .filter(|b| b.token == Some(USDC_TOKEN_INDEX))
        .map(|b| b.total)
        .sum()
}

struct SpotTokenMeta {
    wei_decimals: u8,
}

/// The spot tokens' metadata and their USDC mark prices, keyed by token index.
struct SpotMarket {
    tokens: HashMap<u64, SpotTokenMeta>,
    usdc_price: HashMap<u64, Decimal>,
}

/// Read `spotMetaAndAssetCtxs`: token metadata plus the mark price of each `held` token's USDC pair.
/// The contexts are matched to pairs by name (`ctx.coin == pair.name`), not by position: the context
/// list is longer than the pair universe and not aligned with it. A held token without a USDC pair,
/// whose pair has no context, or whose mark is zero (no market, not a $0 price) is left unpriced; a
/// present but unparseable or negative mark of a held token fails closed. Pairs of tokens the
/// account does not hold are not read, so their state cannot fail the account.
async fn fetch_spot_market(
    endpoint: &str,
    held: &HashSet<u64>,
    cx: &Ctx,
) -> Result<SpotMarket, Error> {
    let json = info(
        endpoint,
        serde_json::json!({ "type": "spotMetaAndAssetCtxs" }),
        cx,
    )
    .await?;
    let malformed = || Error::Integrity {
        message: "Hyperliquid `spotMetaAndAssetCtxs` response is malformed".into(),
    };
    let meta = json.get(0).ok_or_else(malformed)?;
    let ctxs = json
        .get(1)
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;

    let mut tokens = HashMap::new();
    for t in meta
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?
    {
        let index = t
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(malformed)?;
        let wei_decimals = t
            .get("weiDecimals")
            .and_then(Value::as_u64)
            .and_then(|d| u8::try_from(d).ok())
            .ok_or_else(malformed)?;
        tokens.insert(index, SpotTokenMeta { wei_decimals });
    }

    let marks: HashMap<&str, &Value> = ctxs
        .iter()
        .filter_map(|c| Some((c.get("coin")?.as_str()?, c.get("markPx")?)))
        .collect();
    let mut usdc_price = HashMap::new();
    for pair in meta
        .get("universe")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?
    {
        let pair_tokens = pair.get("tokens").and_then(Value::as_array);
        let (Some(base), Some(USDC_TOKEN_INDEX)) = (
            pair_tokens.and_then(|t| t.first()).and_then(Value::as_u64),
            pair_tokens.and_then(|t| t.get(1)).and_then(Value::as_u64),
        ) else {
            continue;
        };
        if !held.contains(&base) {
            continue;
        }
        let Some(mark) = pair
            .get("name")
            .and_then(Value::as_str)
            .and_then(|n| marks.get(n))
        else {
            continue;
        };
        let px = mark
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .filter(|px| !px.is_sign_negative())
            .ok_or_else(|| Error::Integrity {
                message: format!("Hyperliquid spot mark for token {base} is invalid"),
            })?;
        if !px.is_zero() {
            usdc_price.insert(base, px);
        }
    }
    Ok(SpotMarket { tokens, usdc_price })
}

/// One fill from `userFills` / `userFillsByTime`.
struct Fill {
    coin: String,
    price: Decimal,
    size: Decimal,
    buy: bool,
    start_position: Decimal,
    time_ms: i64,
    closed_pnl: Decimal,
    fee: Decimal,
    fee_token: String,
    hash: String,
    tid: String,
    /// The fill liquidated this account (not a fill this account took as liquidator).
    liquidated: bool,
}

/// Parse one fill; any missing or unparseable field fails closed.
fn parse_fill(raw: &Value, user: &str) -> Result<Fill, Error> {
    let s = |field: &str| {
        raw.get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| bad_field("fill", field))
    };
    let d = |field: &str| Decimal::from_str(s(field)?).map_err(|_| bad_field("fill", field));
    let size = d("sz")?;
    if size <= Decimal::ZERO {
        return Err(bad_field("fill", "sz"));
    }
    let buy = match s("side")? {
        "B" => true,
        "A" => false,
        _ => return Err(bad_field("fill", "side")),
    };
    let tid = match raw.get("tid") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(t)) => t.clone(),
        _ => return Err(bad_field("fill", "tid")),
    };
    let liquidated = raw
        .get("liquidation")
        .and_then(|l| l.get("liquidatedUser"))
        .and_then(Value::as_str)
        .is_some_and(|u| u.eq_ignore_ascii_case(user));
    Ok(Fill {
        coin: s("coin")?.to_string(),
        price: d("px")?,
        size,
        buy,
        start_position: d("startPosition")?,
        time_ms: raw
            .get("time")
            .and_then(Value::as_i64)
            .ok_or_else(|| bad_field("fill", "time"))?,
        closed_pnl: d("closedPnl")?,
        fee: d("fee")?,
        fee_token: s("feeToken")?.trim().to_string(),
        hash: s("hash")?.to_string(),
        tid,
        liquidated,
    })
}

/// A fill on the default perp dex. Spot fills name a pair (`@107`, `PURR/USDC`) and builder-dex
/// perps carry a dex prefix (`xyz:XYZ100`); neither is a position this adapter reads.
fn is_perp_market(coin: &str) -> bool {
    !coin.starts_with('@') && !coin.contains('/') && !coin.contains(':')
}

fn side_of(signed: Decimal) -> PerpSide {
    if signed.is_sign_negative() {
        PerpSide::Short
    } else {
        PerpSide::Long
    }
}

/// What a fill did to the position, from its signed size before and after the fill, and the side
/// it opened (open, flip) or reduced (close, liquidation).
fn classify(fill: &Fill) -> (EventKind, PerpSide) {
    let start = fill.start_position;
    let end = if fill.buy {
        start + fill.size
    } else {
        start - fill.size
    };
    if fill.liquidated && !start.is_zero() {
        return (EventKind::Liquidation, side_of(start));
    }
    if start.is_zero() {
        (EventKind::PerpOpen, side_of(end))
    } else if end.is_zero() {
        (EventKind::PerpClose, side_of(start))
    } else if start.is_sign_negative() == end.is_sign_negative() {
        if end.abs() > start.abs() {
            (EventKind::PerpOpen, side_of(start))
        } else {
            (EventKind::PerpClose, side_of(start))
        }
    } else {
        (EventKind::PerpFlip, side_of(end))
    }
}

fn fill_event(fill: Fill) -> Result<HistoryEvent, Error> {
    let (kind, side) = classify(&fill);
    // A perp size has no on-chain base unit; the amount carries the base-asset quantity at a
    // precision wide enough to hold it exactly.
    let amount = Amount::from_decimal(
        Token::new(fill.coin.clone(), None, PERP_SIZE_DECIMALS),
        fill.size,
    )?;
    let fee = (fill.fee_token == "USDC").then(|| Money::usd(fill.fee));
    let perp = PerpFill::new(fill.coin, side, fill.price, fill.start_position, fill.tid)
        .with_realized_pnl(Some(Money::usd(fill.closed_pnl)))
        .with_fee(fee);
    Ok(HistoryEvent::new(
        kind,
        amount,
        fill.hash,
        Timestamp(fill.time_ms.div_euclid(1000)),
    )
    .with_perp(Some(perp)))
}

/// Chronological order of fills: by time, then by fill id (decimal digits, so shorter is lower).
fn fill_order(f: &Fill) -> (i64, usize, &str) {
    (f.time_ms, f.tid.len(), f.tid.as_str())
}

/// The account's most recent fills (`userFills`), best-effort: a transport error or a malformed
/// payload yields `None`, so the positions read without an opening date instead of failing.
async fn fetch_recent_fills(endpoint: &str, user: &str, cx: &Ctx) -> Option<Vec<Fill>> {
    let json = info(
        endpoint,
        serde_json::json!({ "type": "userFills", "user": user }),
        cx,
    )
    .await
    .ok()?;
    json.as_array()?
        .iter()
        .map(|raw| parse_fill(raw, user).ok())
        .collect()
}

/// When the open `side` position in `market` was opened: the most recent fill that took the
/// position from flat or from the opposite side to `side`. Any later such fill would be more recent
/// and so also inside the window of recent fills, so the latest one found is the opening fill. When
/// none is in the window the opening is older than the window and the date stays `None`.
fn opened_at(fills: &[Fill], market: &str, side: PerpSide) -> Option<Timestamp> {
    fills
        .iter()
        .filter(|f| f.coin == market)
        .filter(|f| {
            let (kind, opened) = classify(f);
            opened == side
                && (kind == EventKind::PerpFlip
                    || (kind == EventKind::PerpOpen && f.start_position.is_zero()))
        })
        .max_by_key(|f| fill_order(f))
        .map(|f| Timestamp(f.time_ms.div_euclid(1000)))
}

/// Parse a required decimal-string field.
fn dec(v: &Value, field: &str) -> Result<Decimal, Error> {
    v.get(field)
        .and_then(Value::as_str)
        .and_then(|s| Decimal::from_str(s).ok())
        .ok_or_else(|| integrity(field))
}

fn integrity(field: &str) -> Error {
    bad_field("position", field)
}

fn bad_field(what: &str, field: &str) -> Error {
    Error::Integrity {
        message: format!("Hyperliquid {what} missing/invalid `{field}`"),
    }
}
