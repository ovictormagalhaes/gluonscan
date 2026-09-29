//! # gluonscan-kamino
//!
//! Kamino adapter (Solana lending), [`Source::Api`]: reads a wallet's obligations from the Kamino
//! HTTP API across its known markets and normalizes them into [`LendingPosition`]s. HTTP-only for
//! the position read (the Kamino API resolves USD values); on-chain reads are only needed for the
//! historical path (a follow-up).
//!
//! Note: this slice models the obligation shape directly; the real API's scaled-fraction fields and
//! the `reserves/metrics` join (for exact token/price resolution) are a reconciliation follow-up.

use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Currency, Detail, Error, LendingPosition, Money,
    Position, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet,
};
use rust_decimal::Decimal;
use std::str::FromStr;

const KAMINO_API: &str = "https://api.kamino.finance";
const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::HealthFactor];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Solana];

/// Kamino's known lending markets (Main, JLP, Altcoins).
const MARKETS: &[&str] = &[
    "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF",
    "ByVuX9fRdEHsZomwLVryQQHRhQi3yMXZ6uz6DA4gutja",
    "DxXdAyU3kCjnyggvHmY5nAwg5cRbbmdyX3npfDMjjMek",
];

/// Kamino adapter backed by the public HTTP API.
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
            for ob in items {
                positions.push(Position::Lending(parse_obligation(ob)?));
            }
        }

        let reading = Reading {
            protocol: Protocol::Kamino,
            chain: Chain::Solana,
            source: Source::Api,
            positions,
            provenance: Provenance {
                source: Source::Api,
                chain: Chain::Solana,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}

fn parse_obligation(ob: &serde_json::Value) -> Result<LendingPosition, Error> {
    let supplied = parse_amounts(ob.get("deposits"))?;
    let borrowed = parse_amounts(ob.get("borrows"))?;
    let health_factor = ob
        .pointer("/refreshedStats/healthFactor")
        .and_then(json_decimal);

    if !borrowed.is_empty() && health_factor.is_none() {
        return Err(Error::Integrity {
            message: "Kamino obligation has debt but no health factor".into(),
        });
    }

    Ok(LendingPosition {
        supplied,
        borrowed,
        health_factor,
    })
}

fn parse_amounts(list: Option<&serde_json::Value>) -> Result<Vec<Amount>, Error> {
    let Some(items) = list.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let symbol = item
            .get("symbol")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let decimals = item
            .get("decimals")
            .and_then(|d| d.as_u64())
            .ok_or_else(|| Error::Integrity {
                message: format!("Kamino asset `{symbol}` missing decimals"),
            })? as u8;
        let amount = item
            .get("amount")
            .and_then(json_decimal)
            .ok_or_else(|| Error::Integrity {
                message: format!("Kamino asset `{symbol}` missing amount"),
            })?;
        let usd = item
            .get("usdValue")
            .and_then(json_decimal)
            .map(|amount| Money {
                amount,
                currency: Currency::Usd,
            });
        out.push(
            Amount::from_decimal(
                Token {
                    symbol,
                    address: None,
                    decimals,
                },
                amount,
            )?
            .with_usd(usd),
        );
    }
    Ok(out)
}

/// Read a decimal from a JSON string or number literal (never via `f64`).
fn json_decimal(v: &serde_json::Value) -> Option<Decimal> {
    if let Some(s) = v.as_str() {
        Decimal::from_str(s).ok()
    } else if v.is_number() {
        Decimal::from_str(&v.to_string()).ok()
    } else {
        None
    }
}
