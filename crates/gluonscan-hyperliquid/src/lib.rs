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

use std::str::FromStr;

use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, Money, PerpPosition, PerpSide,
    Position, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet,
    WalletBalance,
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
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if chain != Chain::Hyperliquid {
            return Err(Error::Permanent {
                message: format!("Hyperliquid not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;

        let body = serde_json::json!({
            "type": "clearinghouseState",
            "user": format!("{owner:#x}"),
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

        let mut positions = Vec::with_capacity(asset_positions.len() + 1);
        for ap in asset_positions {
            let pos = ap.get("position").ok_or_else(|| Error::Integrity {
                message: "Hyperliquid assetPosition missing `position`".into(),
            })?;
            positions.push(Position::Perp(parse_perp(pos)?));
        }

        // The account's margin equity (`marginSummary.accountValue`) is the portfolio figure: it is
        // the mark-to-market USD value of everything in the account — deposited collateral plus every
        // position's unrealized PnL, whether or not that collateral currently backs an open position.
        // Per-position `collateral` (marginUsed) is a margin *requirement*, not owned capital, so it is
        // not summable to equity; the equity is carried once, here, as the account's USDC balance. It
        // is net-worth-bearing, so a present response that omits it fails closed rather than silently
        // undercounting. A zero (or wiped) account contributes nothing.
        let account_value = json
            .get("marginSummary")
            .and_then(|m| m.get("accountValue"))
            .and_then(Value::as_str)
            .and_then(|s| Decimal::from_str(s).ok())
            .ok_or_else(|| Error::Integrity {
                message: "Hyperliquid response missing/invalid `marginSummary.accountValue`".into(),
            })?;
        if account_value.is_sign_positive() && !account_value.is_zero() {
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

    let liquidation_price = match pos.get("liquidationPx") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| integrity("liquidationPx"))?,
        ),
    };

    // Absent cumFunding → None (legitimately unknown); present but unparseable → fail closed, same
    // as liquidationPx. Negative means funding paid by this position (matches both the model's
    // convention and Hyperliquid's own sign convention), so the value maps through verbatim.
    let funding = match pos.get("cumFunding").and_then(|c| c.get("sinceOpen")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(Money::usd(
            v.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| integrity("cumFunding.sinceOpen"))?,
        )),
    };

    // Collateral is the USDC margin. Hyperliquid is USD-margined and its USDC == USD with no contract
    // to price it by, so the USD value is the API's own authoritative figure, not a fabricated price.
    let collateral = vec![Amount::from_decimal(Token::new("USDC", None, 6), margin)?
        .with_usd(Some(Money::usd(margin)))];

    Ok(PerpPosition::new(market, side, size)
        .with_collateral(collateral)
        .with_entry_price(Some(entry))
        .with_mark_price(Some(mark))
        .with_leverage(leverage)
        .with_unrealized_pnl(Some(Money::usd(pnl)))
        .with_funding(funding)
        .with_liquidation_price(liquidation_price))
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
