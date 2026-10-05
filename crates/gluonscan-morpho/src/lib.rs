//! # gluonscan-morpho
//!
//! Morpho Blue lending adapter, **API backend** ([`Source::Api`]): reads a wallet's positions from
//! the official Morpho GraphQL API and normalizes each market into a [`LendingPosition`].
//!
//! Morpho Blue markets are **isolated** — one market is one position with its own collateral, debt
//! and health factor — so this adapter emits one [`LendingPosition`] per non-empty market, not a
//! single aggregate. A single LLTV per market is both the max borrow LTV and the liquidation
//! threshold; debt has no borrow factor (it counts at 100%).
//!
//! No RPC — HTTP-only. Claimable incentives moved to Merkl (a separate per-wallet system) and are a
//! planned follow-up; `rewards` is left empty here rather than guessed.

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, Complete, Ctx, Detail, Error, LendingPosition,
    Position, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, SuppliedAsset,
    Token, Wallet,
};
use rust_decimal::Decimal;
use serde_json::Value;

const MORPHO_API: &str = "https://blue-api.morpho.org/graphql";

const QUERY: &str = r#"query($address:String!,$chainId:Int){userByAddress(address:$address,chainId:$chainId){marketPositions{healthFactor state{supplyAssets borrowAssets collateral} market{marketId lltv loanAsset{symbol address decimals} collateralAsset{symbol address decimals} state{supplyApy borrowApy}}}}}"#;

const CAPABILITIES: &[Capability] = &[
    Capability::Positions,
    Capability::HealthFactor,
    Capability::RiskConfig,
];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum, Chain::Base];

/// Morpho Blue adapter backed by the official GraphQL API.
#[derive(Debug, Default, Clone)]
pub struct Morpho {
    endpoint: Option<String>,
}

impl Morpho {
    /// Construct with the default public endpoint.
    pub fn new() -> Self {
        Morpho { endpoint: None }
    }

    /// Override the endpoint (for a private gateway or tests).
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Some(url.into());
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(MORPHO_API)
    }
}

#[async_trait]
impl ProtocolAdapter for Morpho {
    fn protocol(&self) -> Protocol {
        Protocol::Morpho
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
        if !SUPPORTED_CHAINS.contains(&chain) {
            return Err(Error::Permanent {
                message: format!("Morpho not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let chain_id = chain.evm_chain_id().ok_or_else(|| Error::Permanent {
            message: format!("Morpho is EVM-only; {chain:?} has no chain id"),
        })?;

        let body = serde_json::json!({
            "query": QUERY,
            "variables": { "address": format!("{owner:#x}"), "chainId": chain_id },
        })
        .to_string();
        let raw = cx.http.post(self.endpoint(), body, &[]).await?;

        let json: Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Morpho response was not valid JSON: {e}"),
        })?;
        check_graphql_errors(&json)?;

        // A never-seen wallet comes back as `userByAddress: null` — "nothing here", not an error.
        // But a non-null user with no `marketPositions` array is a malformed payload: fail closed
        // rather than silently read it as empty.
        let empty = Vec::new();
        let market_positions = match json.pointer("/data/userByAddress") {
            Some(user) if !user.is_null() => user
                .get("marketPositions")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Integrity {
                    message: "Morpho userByAddress present but marketPositions is not an array"
                        .into(),
                })?,
            _ => &empty,
        };

        let mut positions = Vec::new();
        for mp in market_positions {
            if let Some(position) = parse_market_position(mp)? {
                positions.push(Position::Lending(position));
            }
        }

        let reading = Reading::new(
            Protocol::Morpho,
            chain,
            Source::Api,
            positions,
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
    }
}

/// Map one `marketPosition`. Returns `None` for an empty (historical/dust) market so it is dropped.
fn parse_market_position(mp: &Value) -> Result<Option<LendingPosition>, Error> {
    let state = mp.get("state").ok_or_else(|| Error::Integrity {
        message: "Morpho marketPosition missing `state`".into(),
    })?;
    let supply_raw = big_uint(state.get("supplyAssets"), "supplyAssets")?;
    let borrow_raw = big_uint(state.get("borrowAssets"), "borrowAssets")?;
    let collateral_raw = big_uint(state.get("collateral"), "collateral")?;

    if supply_raw.is_zero() && borrow_raw.is_zero() && collateral_raw.is_zero() {
        return Ok(None);
    }

    let market = mp.get("market").ok_or_else(|| Error::Integrity {
        message: "Morpho marketPosition missing `market`".into(),
    })?;
    // The market's unique on-chain id. Morpho Blue markets are isolated and a wallet can hold
    // several that share the same (collateral, loan, LLTV) and differ only by oracle/IRM — so this
    // is the ONLY stable identity that keeps them from colliding in a downstream dedup. We are past
    // the zero-market early return, so this is a real market: fail closed if its id is absent rather
    // than emit `None`, which would silently reintroduce the very collision this id prevents.
    let market_id = market
        .get("marketId")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| Error::Integrity {
            message: "Morpho non-empty market missing `marketId`".into(),
        })?;
    let loan_token = token_of(market.get("loanAsset"), "loanAsset")?;
    let lltv = wad_ratio(market.get("lltv"))?;
    let (supply_apy, borrow_apy) = market_apys(market.get("state"));

    let mut supplied = Vec::new();
    if !collateral_raw.is_zero() {
        // Collateral must be denominable — a non-null collateralAsset is required or we fail closed.
        let collateral_token =
            token_of(market.get("collateralAsset"), "collateralAsset").map_err(|_| {
                Error::Integrity {
                    message: "Morpho position has collateral but the market's collateral asset is \
                              missing"
                        .into(),
                }
            })?;
        supplied.push(
            SuppliedAsset::new(Amount::from_raw(collateral_token, collateral_raw)?)
                .with_collateral(true, true)
                .with_liquidation_threshold(Some(lltv))
                .with_max_ltv(Some(lltv))
                .with_apy(Some(Decimal::ZERO)),
        );
    }
    if !supply_raw.is_zero() {
        supplied.push(
            SuppliedAsset::new(Amount::from_raw(loan_token.clone(), supply_raw)?)
                .with_collateral(false, false)
                .with_apy(supply_apy),
        );
    }

    let mut borrowed = Vec::new();
    if !borrow_raw.is_zero() {
        borrowed.push(
            BorrowedAsset::new(Amount::from_raw(loan_token, borrow_raw)?)
                .with_borrow_factor(Some(Decimal::ONE))
                .with_apy(borrow_apy),
        );
    }

    let health_factor = mp.get("healthFactor").and_then(Value::as_f64).and_then(dec);
    if !borrowed.is_empty() && health_factor.is_none() {
        // Never hide liquidation risk: debt without a health factor is incomplete.
        return Err(Error::Integrity {
            message: "Morpho position has debt but no health factor".into(),
        });
    }

    Ok(Some(
        LendingPosition::new(supplied, borrowed)
            .with_health_factor(health_factor)
            .with_market_id(Some(market_id)),
    ))
}

/// Build a [`Token`] from an asset node (`{symbol, address, decimals}`).
fn token_of(node: Option<&Value>, field: &str) -> Result<Token, Error> {
    let node = node
        .filter(|v| !v.is_null())
        .ok_or_else(|| Error::Integrity {
            message: format!("Morpho asset `{field}` missing"),
        })?;
    let symbol = node
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Integrity {
            message: format!("Morpho `{field}` missing symbol"),
        })?;
    let decimals = node
        .get("decimals")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Integrity {
            message: format!("Morpho `{field}` missing decimals"),
        })? as u8;
    // A lending leg must be priceable by address, so a missing/malformed address fails closed
    // rather than emitting an address-less token the pricing layer can't resolve.
    let address = node
        .get("address")
        .and_then(Value::as_str)
        .and_then(|s| Address::from_str(s).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("Morpho `{field}` missing or malformed address"),
        })?;
    Ok(Token::evm(symbol, Some(address), decimals))
}

/// Exact non-negative integer from Morpho's `BigInt`. The API serializes a `BigInt` as a JSON
/// string once it exceeds 2^53 and as a bare number below that, so a string is parsed verbatim and a
/// bare number always fits `u64` (hence exact). An explicit JSON null means zero (no supply/borrow).
/// Any other shape — including a bare number too large for `u64`, which the API never emits — fails
/// closed rather than risk an f64 round-trip that would silently corrupt the amount.
fn big_uint(v: Option<&Value>, field: &str) -> Result<U256, Error> {
    let v = v.ok_or_else(|| Error::Integrity {
        message: format!("Morpho field `{field}` missing"),
    })?;
    if v.is_null() {
        return Ok(U256::ZERO);
    }
    if let Some(s) = v.as_str() {
        return U256::from_str(s.trim()).map_err(|e| Error::Integrity {
            message: format!("Morpho field `{field}` not a base-10 integer: {e}"),
        });
    }
    if let Some(n) = v.as_u64() {
        return Ok(U256::from(n));
    }
    Err(Error::Integrity {
        message: format!("Morpho field `{field}` is not a representable integer"),
    })
}

/// A WAD-scaled ratio string (e.g. `"860000000000000000"` → `0.86`).
fn wad_ratio(v: Option<&Value>) -> Result<Decimal, Error> {
    let s = v.and_then(Value::as_str).ok_or_else(|| Error::Integrity {
        message: "Morpho market missing lltv".into(),
    })?;
    let wad = Decimal::from_str(s).map_err(|e| Error::Integrity {
        message: format!("Morpho lltv not a number: {e}"),
    })?;
    Ok(wad / Decimal::from(1_000_000_000_000_000_000u64))
}

/// Supply/borrow APY (ratios). Missing market state leaves them unknown (`None`), never a fake `0`.
fn market_apys(state: Option<&Value>) -> (Option<Decimal>, Option<Decimal>) {
    let supply = state
        .and_then(|s| s.get("supplyApy"))
        .and_then(Value::as_f64)
        .and_then(dec);
    let borrow = state
        .and_then(|s| s.get("borrowApy"))
        .and_then(Value::as_f64)
        .and_then(dec);
    (supply, borrow)
}

fn dec(x: f64) -> Option<Decimal> {
    Decimal::try_from(x).ok()
}

fn check_graphql_errors(json: &Value) -> Result<(), Error> {
    if let Some(errors) = json.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let msg: Vec<String> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .map(String::from)
                .collect();
            return Err(Error::Integrity {
                message: format!("Morpho GraphQL error: {}", msg.join("; ")),
            });
        }
    }
    if json.get("data").map(Value::is_null).unwrap_or(true) {
        return Err(Error::Integrity {
            message: "Morpho response missing `data`".into(),
        });
    }
    Ok(())
}
