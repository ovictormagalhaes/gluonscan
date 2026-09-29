//! # gluonscan-aave
//!
//! Aave V3 adapter, **API backend** ([`Source::Api`]): reads a wallet's supplies, borrows and
//! account health factor from the official Aave V3 GraphQL API and normalizes them into a
//! [`LendingPosition`]. No RPC — this backend is HTTP-only.
//!
//! A future on-chain backend would live in a sibling crate and register for the same
//! [`Protocol::AaveV3`]; the engine routes capabilities between them.

use std::str::FromStr;

use alloy_primitives::Address;
use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Currency, Detail, Error, LendingPosition, Money,
    Position, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token,
};
use rust_decimal::Decimal;

const AAVE_V3_API: &str = "https://api.v3.aave.com/graphql";

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::HealthFactor];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum, Chain::Base, Chain::Arbitrum];

/// The Aave V3 adapter backed by the official GraphQL API.
#[derive(Debug, Default, Clone)]
pub struct AaveApi {
    endpoint: Option<String>,
}

impl AaveApi {
    /// Construct with the default public endpoint.
    pub fn new() -> Self {
        AaveApi { endpoint: None }
    }

    /// Override the endpoint (for a private gateway or tests).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(AAVE_V3_API)
    }

    /// The Aave V3 Pool/market address for a supported chain.
    fn market(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Ethereum => "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2",
            Chain::Base => "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5",
            Chain::Arbitrum => "0x794a61358D6845594F94dc1DB02A252b5b4814aD",
            _ => return None,
        })
    }
}

#[async_trait]
impl ProtocolAdapter for AaveApi {
    fn protocol(&self) -> Protocol {
        Protocol::AaveV3
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
        owner: Address,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        let chain_id = chain.evm_chain_id().ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 is EVM-only; {chain:?} has no chain id"),
        })?;
        let market = AaveApi::market(chain).ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 not configured for {chain:?}"),
        })?;

        let user = format!("{owner:#x}");
        let body = query_body(market, chain_id, &user);
        let raw = cx.http.post(self.endpoint(), body).await?;

        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Aave response was not valid JSON: {e}"),
        })?;
        let data = json.get("data").ok_or_else(|| Error::Integrity {
            message: "Aave response missing `data`".into(),
        })?;

        let supplied = parse_amounts(data.get("userSupplies"), "balance")?;
        let borrowed = parse_amounts(data.get("userBorrows"), "debt")?;

        let has_debt = !borrowed.is_empty();
        let health_factor = parse_health_factor(data.get("userMarketState"));
        if has_debt && health_factor.is_none() {
            // Never hide liquidation risk: a debt position without a health factor is incomplete.
            return Err(Error::Integrity {
                message: "Aave account has debt but no health factor was returned".into(),
            });
        }

        let position = LendingPosition {
            supplied,
            borrowed,
            health_factor,
        };
        let reading = Reading {
            protocol: Protocol::AaveV3,
            chain,
            source: Source::Api,
            positions: vec![Position::Lending(position)],
            provenance: Provenance {
                source: Source::Api,
                chain,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}

fn query_body(market: &str, chain_id: u64, user: &str) -> String {
    // Combined query: supplies + borrows + account state, in one round trip.
    let query = r#"query($market:String!,$chainId:Int!,$user:String!){
      userSupplies(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol address decimals} balance{amount{value} usd}
      }
      userBorrows(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol address decimals} debt{amount{value} usd}
      }
      userMarketState(request:{market:$market,chainId:$chainId,user:$user}){ healthFactor }
    }"#;
    let payload = serde_json::json!({
        "query": query,
        "variables": { "market": market, "chainId": chain_id, "user": user }
    });
    payload.to_string()
}

fn parse_amounts(
    list: Option<&serde_json::Value>,
    balance_key: &str,
) -> Result<Vec<Amount>, Error> {
    let Some(items) = list.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let currency = item.get("currency").ok_or_else(|| Error::Integrity {
            message: "Aave entry missing `currency`".into(),
        })?;
        let symbol = currency
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let decimals = currency
            .get("decimals")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| Error::Integrity {
                message: format!("Aave currency `{symbol}` missing decimals"),
            })? as u8;
        let address = currency
            .get("address")
            .and_then(|v| v.as_str())
            .and_then(|s| Address::from_str(s).ok());

        let balance = item.get(balance_key).ok_or_else(|| Error::Integrity {
            message: format!("Aave entry missing `{balance_key}`"),
        })?;
        let amount = balance
            .get("amount")
            .and_then(|a| a.get("value"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Integrity {
                message: format!("Aave `{symbol}` missing amount value"),
            })?;
        let amount = Decimal::from_str(amount).map_err(|e| Error::Integrity {
            message: format!("Aave `{symbol}` amount not a decimal: {e}"),
        })?;

        // A present-but-null usd is "not priced" (kept as None); we never fabricate a value.
        let usd = balance
            .get("usd")
            .and_then(|v| v.as_str())
            .and_then(|s| Decimal::from_str(s).ok())
            .map(|amount| Money {
                amount,
                currency: Currency::Usd,
            });

        out.push(Amount {
            token: Token {
                symbol,
                address,
                decimals,
            },
            amount,
            usd,
        });
    }
    Ok(out)
}

fn parse_health_factor(state: Option<&serde_json::Value>) -> Option<Decimal> {
    state
        .and_then(|s| s.get("healthFactor"))
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
}
