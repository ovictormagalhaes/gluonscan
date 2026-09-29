//! # gluonscan-pendle
//!
//! Pendle adapter. Discovery is the Pendle market catalog over HTTP ([`Source::Api`]) — the full
//! set of PT/YT tokens (address, decimals, price, maturity) for the chain — followed by an on-chain
//! `balanceOf` per token. Any non-zero balance is a held [`YieldPosition`], priced from the catalog.
//!
//! (vePENDLE / sPENDLE on Ethereum are a separate capability, a follow-up.)

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, Complete, Ctx, Currency, Detail, Error, Money, Position,
    Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Timestamp, Token, Wallet,
    YieldKind, YieldPosition,
};
use gluonscan_evm::{decode_u256, encode_balance_of, eth_call};
use rust_decimal::Decimal;

const PENDLE_API: &str = "https://api-v2.pendle.finance/core";
const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum, Chain::Arbitrum, Chain::Base];

/// Pendle adapter backed by the public market-catalog API + on-chain balances.
#[derive(Debug, Default, Clone)]
pub struct PendleApi {
    base: Option<String>,
}

impl PendleApi {
    /// Construct with the default public API base.
    pub fn new() -> Self {
        PendleApi { base: None }
    }

    /// Override the API base (private gateway or test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = Some(url.into());
        self
    }

    fn base(&self) -> &str {
        self.base.as_deref().unwrap_or(PENDLE_API)
    }
}

struct Candidate {
    token: Token,
    price_usd: Option<Decimal>,
    expiry: Option<Timestamp>,
    apy: Option<Decimal>,
    kind: YieldKind,
}

#[async_trait]
impl ProtocolAdapter for PendleApi {
    fn protocol(&self) -> Protocol {
        Protocol::Pendle
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
            message: format!("Pendle is EVM-only; {chain:?} has no chain id"),
        })?;
        if !SUPPORTED_CHAINS.contains(&chain) {
            return Err(Error::Permanent {
                message: format!("Pendle not configured for {chain:?}"),
            });
        }

        // 1. Catalog (wallet-independent): every PT/YT for the chain.
        let url = format!("{}/v1/{}/markets", self.base(), chain_id);
        let raw = cx.http.get(&url, &[]).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Pendle catalog not JSON: {e}"),
        })?;
        let markets = json
            .pointer("/markets")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Integrity {
                message: "Pendle catalog missing `markets`".into(),
            })?;

        let mut candidates = Vec::new();
        for m in markets {
            let expiry = m.get("expiry").and_then(|e| e.as_i64()).map(Timestamp);
            let apy = decimal_at(m, "/impliedApy");
            for (key, kind) in [
                ("pt", YieldKind::PrincipalToken),
                ("yt", YieldKind::YieldToken),
            ] {
                if let Some(tok) = m.get(key) {
                    candidates.push(Candidate {
                        token: parse_token(tok)?,
                        price_usd: decimal_at(tok, "/price/usd"),
                        expiry,
                        apy,
                        kind,
                    });
                }
            }
        }

        // 2. On-chain balances; any non-zero is a held position.
        let rpc = cx.rpc()?.as_ref();
        let mut positions = Vec::new();
        for c in candidates {
            let token_addr = c.token.address.ok_or_else(|| Error::Integrity {
                message: format!("Pendle token `{}` has no address", c.token.symbol),
            })?;
            let out = eth_call(rpc, chain, None, token_addr, encode_balance_of(owner)).await?;
            let balance = decode_u256(&out)?;
            if balance.is_zero() {
                continue;
            }
            positions.push(Position::Yield(YieldPosition {
                amount: priced_amount(c.token, balance, c.price_usd)?,
                kind: c.kind,
                expiry: c.expiry,
                apy: c.apy,
            }));
        }

        let reading = Reading {
            protocol: Protocol::Pendle,
            chain,
            source: Source::Api,
            positions,
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

fn parse_token(v: &serde_json::Value) -> Result<Token, Error> {
    let symbol = v
        .get("symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let decimals = v
        .get("decimals")
        .and_then(json_u64)
        .ok_or_else(|| Error::Integrity {
            message: format!("Pendle token `{symbol}` missing decimals"),
        })? as u8;
    let address = v
        .get("address")
        .and_then(|s| s.as_str())
        .and_then(|s| Address::from_str(s).ok());
    Ok(Token {
        symbol,
        address,
        decimals,
    })
}

fn priced_amount(token: Token, raw: U256, price_usd: Option<Decimal>) -> Result<Amount, Error> {
    let amount = scaled(raw, token.decimals)?;
    let usd = match price_usd {
        Some(p) => Some(Money {
            amount: amount.checked_mul(p).ok_or_else(|| Error::Integrity {
                message: "USD value overflow".into(),
            })?,
            currency: Currency::Usd,
        }),
        None => None,
    };
    Ok(Amount {
        token,
        raw,
        amount,
        usd,
    })
}

/// Read a decimal from a JSON string or number literal (never via `f64`, to avoid precision loss).
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

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
