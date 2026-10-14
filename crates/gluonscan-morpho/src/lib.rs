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
//! The read is HTTP-only. Claimable incentives moved to Merkl (a separate per-wallet system) and are
//! a planned follow-up; `rewards` is left empty here rather than guessed.
//!
//! The API's indexer can answer with a wallet's markets missing (an empty list) while they still
//! hold funds on-chain, which reads exactly like a full exit. [`holds_market`] asks the Morpho Blue
//! contract directly, so a consumer that saw a market before can confirm a disappearance before
//! accepting it.

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, BorrowedAsset, Capability, Chain, ChainProvider, Complete, Ctx, Detail, Error,
    LendingPosition, Position, Protocol, ProtocolAdapter, Provenance, Rate, RateBasis, RateWindow,
    Rates, Reading, Source, Staleness, SuppliedAsset, Timestamp, Token, Wallet,
};
use gluonscan_evm::eth_call;
use rust_decimal::Decimal;
use serde_json::Value;

const MORPHO_API: &str = "https://blue-api.morpho.org/graphql";

/// The Morpho Blue singleton, deployed at the same address on Ethereum and Base.
pub const MORPHO_BLUE: Address =
    alloy_primitives::address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
/// `position(bytes32 id, address user)` → `(supplyShares, borrowShares, collateral)`.
const POSITION_SELECTOR: [u8; 4] = [0x93, 0xc5, 0x20, 0x62];

/// Whether `owner` still holds anything (supply shares, borrow shares or collateral) in the market
/// `market_id` (the 32-byte id as `0x…` hex), read from the Morpho Blue contract. Any failure is an
/// error, never `false`: a disappearance is only accepted once the chain confirms it.
pub async fn holds_market(
    rpc: &dyn ChainProvider,
    chain: Chain,
    market_id: &str,
    owner: Address,
) -> Result<bool, Error> {
    let id = alloy_primitives::hex::decode(market_id.trim_start_matches("0x"))
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| Error::Integrity {
            message: format!("Morpho market id `{market_id}` is not 32 bytes of hex"),
        })?;
    let mut data = POSITION_SELECTOR.to_vec();
    data.extend_from_slice(&id);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(owner.as_slice());
    let out = eth_call(rpc, chain, None, MORPHO_BLUE, data).await?;
    if out.len() < 96 {
        return Err(Error::Integrity {
            message: format!(
                "Morpho position() returned {} bytes, expected 96",
                out.len()
            ),
        });
    }
    Ok(out[..96].iter().any(|b| *b != 0))
}

const QUERY: &str = r#"query($address:String!,$chainId:Int){userByAddress(address:$address,chainId:$chainId){marketPositions{healthFactor state{supplyAssets borrowAssets collateral} market{marketId lltv loanAsset{symbol address decimals} collateralAsset{symbol address decimals} state{timestamp supplyApy borrowApy dailySupplyApy dailyBorrowApy weeklySupplyApy weeklyBorrowApy monthlySupplyApy monthlyBorrowApy}}}}}"#;

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

        // Even a never-seen wallet comes back as a user with an empty `marketPositions` list, so a
        // null or missing user is the API failing to answer, not "nothing here": reading it as
        // empty would drop every position the wallet holds. A user without the array is malformed.
        let user = json
            .pointer("/data/userByAddress")
            .filter(|u| !u.is_null())
            .ok_or_else(|| Error::Transient {
                message: "Morpho API returned no user for the wallet".into(),
                retry_after: None,
            })?;
        let market_positions = user
            .get("marketPositions")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Integrity {
                message: "Morpho userByAddress present but marketPositions is not an array".into(),
            })?;

        let mut positions = Vec::new();
        for mp in market_positions {
            if let Some(position) = parse_market_position(mp, cx.clock.now())? {
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
fn parse_market_position(mp: &Value, read_at: Timestamp) -> Result<Option<LendingPosition>, Error> {
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
    let (supply_rates, borrow_rates) = market_rates(market.get("state"), read_at);

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
                // Morpho Blue never lends collateral out: `supplyCollateral` only credits
                // `position.collateral`, which accrues no interest. The 0 is the contract's rule,
                // not a missing API field; any yield of the collateral token itself is not Morpho's.
                .with_rate(Rate::new(
                    Decimal::ZERO,
                    RateBasis::Apy,
                    RateWindow::Instant,
                    Source::OnChain,
                    read_at,
                )),
        );
    }
    if !supply_raw.is_zero() {
        supplied.push(
            SuppliedAsset::new(Amount::from_raw(loan_token.clone(), supply_raw)?)
                .with_collateral(false, false)
                .with_rates(supply_rates),
        );
    }

    let mut borrowed = Vec::new();
    if !borrow_raw.is_zero() {
        borrowed.push(
            BorrowedAsset::new(Amount::from_raw(loan_token, borrow_raw)?)
                .with_borrow_factor(Some(Decimal::ONE))
                .with_rates(borrow_rates),
        );
    }

    // A health factor only exists against debt. Morpho's API can return a stale/inconsistent HF for
    // a market whose borrow is currently zero (eventual consistency between the HF field and the
    // position state). Never surface a health factor without debt: downstream it reads as liquidation
    // risk that does not exist (an orphaned HF on a collateral-only card). HF is Some iff there is a
    // borrow leg — the exact dual of the debt-without-HF guard below.
    let health_factor = if borrowed.is_empty() {
        None
    } else {
        mp.get("healthFactor").and_then(Value::as_f64).and_then(dec)
    };
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
/// bare number always fits `u64` (hence exact). The API reports an empty side as `0`, so a JSON null
/// is an answer it failed to compute: reading it as zero could drop a live market as empty, so it
/// fails as retryable. Any other shape — including a bare number too large for `u64`, which the API never emits — fails
/// closed rather than risk an f64 round-trip that would silently corrupt the amount.
fn big_uint(v: Option<&Value>, field: &str) -> Result<U256, Error> {
    let v = v.ok_or_else(|| Error::Integrity {
        message: format!("Morpho field `{field}` missing"),
    })?;
    if v.is_null() {
        return Err(Error::Transient {
            message: format!("Morpho field `{field}` is null"),
            retry_after: None,
        });
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

/// The market state's APY fields (excluding rewards) per window: instantaneous, daily, weekly and
/// monthly (30-day) trailing.
const RATE_FIELDS: &[(&str, &str, RateWindow)] = &[
    ("supplyApy", "borrowApy", RateWindow::Instant),
    (
        "dailySupplyApy",
        "dailyBorrowApy",
        RateWindow::Trailing { hours: 24 },
    ),
    (
        "weeklySupplyApy",
        "weeklyBorrowApy",
        RateWindow::Trailing { hours: 168 },
    ),
    (
        "monthlySupplyApy",
        "monthlyBorrowApy",
        RateWindow::Trailing { hours: 720 },
    ),
];

/// Supply and borrow APYs (fractions) of the market, one per window the state reports, dated at the
/// state's own `timestamp` (the read time when absent). A missing field leaves that window out,
/// never a fake `0`.
fn market_rates(state: Option<&Value>, read_at: Timestamp) -> (Rates, Rates) {
    let as_of = state
        .and_then(|s| s.get("timestamp"))
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .map_or(read_at, Timestamp);
    let field = |name: &str| {
        state
            .and_then(|s| s.get(name))
            .and_then(Value::as_f64)
            .and_then(dec)
    };
    let mut supply = Rates::new();
    let mut borrow = Rates::new();
    for (supply_field, borrow_field, window) in RATE_FIELDS {
        for (rates, name) in [(&mut supply, supply_field), (&mut borrow, borrow_field)] {
            if let Some(apy) = field(name) {
                rates.insert(Rate::new(apy, RateBasis::Apy, *window, Source::Api, as_of));
            }
        }
    }
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
