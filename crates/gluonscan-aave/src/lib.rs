//! # gluonscan-aave
//!
//! Aave V3 adapter, **API backend** ([`Source::Api`]): reads a wallet's supplies, borrows and
//! account health factor from the official Aave V3 GraphQL API and normalizes them into a
//! [`LendingPosition`]. No RPC — this backend is HTTP-only.
//!
//! A future on-chain backend would live in a sibling crate and register for the same
//! [`Protocol::AaveV3`]; the engine routes capabilities between them.

use std::collections::HashMap;
use std::str::FromStr;

use alloy_primitives::Address;
use async_trait::async_trait;
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, Complete, Ctx, Currency, Detail, Error,
    LendingPosition, Money, Position, Protocol, ProtocolAdapter, Provenance, Reading, Source,
    Staleness, SuppliedAsset, Token, Wallet,
};
use rust_decimal::Decimal;

const AAVE_V3_API: &str = "https://api.v3.aave.com/graphql";

const CAPABILITIES: &[Capability] = &[
    Capability::Positions,
    Capability::HealthFactor,
    Capability::RiskConfig,
];
const SUPPORTED_CHAINS: &[Chain] = &[
    Chain::Ethereum,
    Chain::Base,
    Chain::Arbitrum,
    Chain::Optimism,
    Chain::Polygon,
    Chain::Bnb,
];

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
            // Deterministic Aave V3 Pool address shared across these deployments.
            Chain::Arbitrum | Chain::Optimism | Chain::Polygon => {
                "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
            }
            Chain::Bnb => "0x6807dc923806fE8Fd134338EABCA509979a7e0cB",
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
        owner: &Wallet,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        let owner = owner.evm()?;
        let chain_id = chain.evm_chain_id().ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 is EVM-only; {chain:?} has no chain id"),
        })?;
        let market = AaveApi::market(chain).ok_or_else(|| Error::Permanent {
            message: format!("Aave V3 not configured for {chain:?}"),
        })?;

        let user = format!("{owner:#x}");
        let body = query_body(market, chain_id, &user);
        let raw = cx.http.post(self.endpoint(), body, &[]).await?;

        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Aave response was not valid JSON: {e}"),
        })?;
        check_graphql_errors(&json)?;
        let data = json.get("data").ok_or_else(|| Error::Integrity {
            message: "Aave response missing `data`".into(),
        })?;

        let mut supplied = parse_supplied(data.get("userSupplies"))?;
        let borrowed = parse_borrowed(data.get("userBorrows"))?;

        let has_debt = !borrowed.is_empty();
        let health_factor = parse_health_factor(data.get("userMarketState"));
        if has_debt && health_factor.is_none() {
            // Never hide liquidation risk: a debt position without a health factor is incomplete.
            return Err(Error::Integrity {
                message: "Aave account has debt but no health factor was returned".into(),
            });
        }

        // Per-reserve risk config (liquidation threshold, max LTV) is a separate query — fetched at
        // Full detail so offline health-factor recompute has the parameters.
        if matches!(_detail, Detail::Full) && !supplied.is_empty() {
            enrich_risk(cx, self.endpoint(), market, chain_id, &mut supplied).await?;
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
            receipt_tokens: Vec::new(),
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

/// Fail closed if the GraphQL response carries an `errors` array — never return the empty `data`
/// that accompanies a failed query as if it were "no positions".
fn check_graphql_errors(json: &serde_json::Value) -> Result<(), Error> {
    if let Some(errs) = json.get("errors").and_then(|e| e.as_array()) {
        if !errs.is_empty() {
            let msg = errs
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::Integrity {
                message: format!("Aave GraphQL error: {msg}"),
            });
        }
    }
    Ok(())
}

/// Fetch per-reserve `maxLTV` / `liquidationThreshold` for the supplied assets in one batched query
/// (an alias per token) and set them on each [`SuppliedAsset`].
async fn enrich_risk(
    cx: &Ctx,
    endpoint: &str,
    market: &str,
    chain_id: u64,
    supplied: &mut [SuppliedAsset],
) -> Result<(), Error> {
    let addrs: Vec<Address> = {
        let mut seen = Vec::new();
        for s in supplied.iter() {
            if let Some(a) = s.amount.token.address {
                if !seen.contains(&a) {
                    seen.push(a);
                }
            }
        }
        seen
    };
    if addrs.is_empty() {
        return Ok(());
    }

    let mut fields = String::new();
    for (i, a) in addrs.iter().enumerate() {
        fields.push_str(&format!(
            r#"r{i}:reserve(request:{{market:"{market}",chainId:{chain_id},underlyingToken:"{a:#x}"}}){{supplyInfo{{maxLTV{{value}} liquidationThreshold{{value}}}}}} "#
        ));
    }
    let body = serde_json::json!({ "query": format!("query{{{fields}}}") }).to_string();
    let raw = cx.http.post(endpoint, body, &[]).await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("Aave reserve response not JSON: {e}"),
    })?;
    check_graphql_errors(&json)?;
    let data = json.get("data").ok_or_else(|| Error::Integrity {
        message: "Aave reserve response missing `data`".into(),
    })?;

    let mut risk: HashMap<Address, (Option<Decimal>, Option<Decimal>)> = HashMap::new();
    for (i, a) in addrs.iter().enumerate() {
        if let Some(node) = data.get(format!("r{i}")) {
            risk.insert(
                *a,
                (
                    node.pointer("/supplyInfo/maxLTV/value")
                        .and_then(json_decimal),
                    node.pointer("/supplyInfo/liquidationThreshold/value")
                        .and_then(json_decimal),
                ),
            );
        }
    }
    for s in supplied.iter_mut() {
        if let Some(a) = s.amount.token.address {
            if let Some((mlt, lt)) = risk.get(&a) {
                s.max_ltv = *mlt;
                s.liquidation_threshold = *lt;
            }
        }
    }
    Ok(())
}

fn query_body(market: &str, chain_id: u64, user: &str) -> String {
    // Combined query: supplies + borrows + account state, in one round trip.
    let query = r#"query($market:String!,$chainId:Int!,$user:String!){
      userSupplies(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol name address decimals} balance{amount{value} usd}
        apy{value} isCollateral canBeCollateral
      }
      userBorrows(request:{markets:[{address:$market,chainId:$chainId}],user:$user}){
        currency{symbol name address decimals} debt{amount{value} usd} apy{value}
      }
      userMarketState(request:{market:$market,chainId:$chainId,user:$user}){ healthFactor }
    }"#;
    let payload = serde_json::json!({
        "query": query,
        "variables": { "market": market, "chainId": chain_id, "user": user }
    });
    payload.to_string()
}

/// Build the [`Amount`] from an entry's `currency` + the `balance`/`debt` sub-object.
fn parse_amount_entry(item: &serde_json::Value, balance_key: &str) -> Result<Amount, Error> {
    let currency = item.get("currency").ok_or_else(|| Error::Integrity {
        message: "Aave entry missing `currency`".into(),
    })?;
    let symbol = currency
        .get("symbol")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = currency
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
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
        .and_then(json_decimal)
        .map(|amount| Money {
            amount,
            currency: Currency::Usd,
        });

    Ok(Amount::from_decimal(
        Token {
            symbol,
            name,
            address,
            decimals,
        },
        amount,
    )?
    .with_usd(usd))
}

/// Read `apy { value }` as a fraction, when present.
fn parse_apy(item: &serde_json::Value) -> Option<Decimal> {
    item.pointer("/apy/value").and_then(json_decimal)
}

fn parse_supplied(list: Option<&serde_json::Value>) -> Result<Vec<SuppliedAsset>, Error> {
    let Some(items) = list.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let amount = parse_amount_entry(item, "balance")?;
        out.push(SuppliedAsset {
            amount,
            // Filled by the separate reserve() query in enrich_risk (Full detail).
            liquidation_threshold: None,
            max_ltv: None,
            is_collateral: item
                .get("isCollateral")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            can_be_collateral: item
                .get("canBeCollateral")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            apy: parse_apy(item),
        });
    }
    Ok(out)
}

fn parse_borrowed(list: Option<&serde_json::Value>) -> Result<Vec<BorrowedAsset>, Error> {
    let Some(items) = list.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let amount = parse_amount_entry(item, "debt")?;
        out.push(BorrowedAsset {
            amount,
            // Aave V3 pins the borrow factor to 1.0 (no borrow-factor haircut).
            borrow_factor: Some(Decimal::ONE),
            apy: parse_apy(item),
        });
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

fn parse_health_factor(state: Option<&serde_json::Value>) -> Option<Decimal> {
    state
        .and_then(|s| s.get("healthFactor"))
        .and_then(|v| v.as_str())
        .and_then(|s| Decimal::from_str(s).ok())
}
