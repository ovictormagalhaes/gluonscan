//! # gluonscan-hyperliquid
//!
//! Hyperliquid perpetuals adapter, **API backend** ([`Source::Api`]): reads a wallet's open perp
//! positions from the public `clearinghouseState` info endpoint and normalizes each into a
//! [`PerpPosition`].
//!
//! Hyperliquid is USD-margined: size, prices, PnL, funding and margin all come back as decimal
//! strings and are parsed straight into [`Decimal`] — never through `f64`. The mark price is derived
//! from the notional the API already returns (`positionValue / |size|`), so no second call is
//! needed. Values the API does not provide (a null `liquidationPx`, an absent `cumFunding`) stay
//! `None` rather than being fabricated.
//!
//! Alongside the per-position reads, the account's margin equity (`marginSummary.accountValue`) is
//! emitted once as a USDC [`Position::Wallet`]: that is the account's portfolio value (collateral
//! plus unrealized PnL), whereas per-position `marginUsed` is a margin requirement, not owned
//! capital. Keeping them separate lets a consumer total equity without double-counting margin.
//!
//! Each position's margin mode (isolated / cross) comes free in the same `clearinghouseState`
//! payload (`leverage.type`). At [`Detail::Full`] the adapter additionally reads the account's
//! resting trigger orders from `frontendOpenOrders` and attaches each position's take-profit and
//! stop-loss levels. That enrichment is best-effort: TP/SL are advisory reference lines, not
//! net-worth-bearing, so a failure there leaves them `None` and never fails the equity read — a
//! correct account value with no TP/SL is strictly better than failing the whole account over a
//! secondary detail.

use std::collections::HashMap;
use std::str::FromStr;

use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, MarginMode, Money, PerpPosition,
    PerpSide, Position, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token,
    Wallet, WalletBalance,
};
use rust_decimal::Decimal;
use serde_json::Value;

const INFO_API: &str = "https://api.hyperliquid.xyz/info";

const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Hyperliquid];

/// Hyperliquid perpetuals adapter backed by the public `clearinghouseState` info API.
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
        if chain != Chain::Hyperliquid {
            return Err(Error::Permanent {
                message: format!("Hyperliquid not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let user = format!("{owner:#x}");

        let body = serde_json::json!({
            "type": "clearinghouseState",
            "user": user,
        })
        .to_string();
        let raw = cx.http.post(self.endpoint(), body, &[]).await?;

        let json: Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Hyperliquid response was not valid JSON: {e}"),
        })?;
        // A valid request always returns the state object with an `assetPositions` array (empty when
        // the account holds no perps). A missing array means a malformed/error payload → fail closed.
        let asset_positions = json
            .get("assetPositions")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Integrity {
                message: "Hyperliquid response missing `assetPositions`".into(),
            })?;

        // Resting take-profit / stop-loss levels are advisory detail, not net-worth-bearing, so they
        // are only fetched at Full and never fail the read: a transport error or a malformed order
        // leaves the affected levels `None`. Fetched once, keyed by market, before the position loop.
        let tpsl = if detail == Detail::Full {
            fetch_position_tpsl(self.endpoint(), &user, cx).await
        } else {
            HashMap::new()
        };

        let mut positions = Vec::with_capacity(asset_positions.len() + 1);
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
            positions.push(Position::Perp(perp));
        }

        // The account's margin equity (`marginSummary.accountValue`) is the portfolio figure: it is
        // the mark-to-market USD value of everything in the account — deposited collateral plus every
        // position's unrealized PnL, whether or not that collateral currently backs an open position.
        // Per-position `collateral` (marginUsed) is a margin *requirement*, not owned capital, so it is
        // not summable to equity; the equity is carried once, here, as the account's USDC balance. It
        // is net-worth-bearing, so a present response that omits it fails closed rather than silently
        // undercounting. A zero (or wiped) account contributes nothing; a *negative* equity is a
        // present net-worth figure that cannot be a balance, so it fails closed rather than being
        // silently dropped (which would overcount the account back up to its collateral legs).
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
        if !account_value.is_zero() {
            let equity = Amount::from_decimal(Token::new("USDC", None, 6), account_value)?
                .with_usd(Some(Money::usd(account_value)));
            positions.push(Position::Wallet(WalletBalance::new(equity)));
        }

        let reading = Reading::new(
            Protocol::Hyperliquid,
            chain,
            Source::Api,
            positions,
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
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

/// Parse a required decimal-string field.
fn dec(v: &Value, field: &str) -> Result<Decimal, Error> {
    v.get(field)
        .and_then(Value::as_str)
        .and_then(|s| Decimal::from_str(s).ok())
        .ok_or_else(|| integrity(field))
}

fn integrity(field: &str) -> Error {
    Error::Integrity {
        message: format!("Hyperliquid position missing/invalid `{field}`"),
    }
}
