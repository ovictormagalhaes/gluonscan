//! # gluonscan-pendle
//!
//! Pendle adapter. Discovery is the Pendle market catalog over HTTP ([`Source::Api`]) — the full
//! set of PT/YT tokens (address, decimals, price, maturity) for the chain — followed by an on-chain
//! `balanceOf` per token. Any non-zero balance is a held [`YieldPosition`], priced from the catalog.
//!
//! On Ethereum it also reads the vePENDLE lock ([`Position::Lock`] — locked PENDLE + governance
//! power + unlock time).

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, ChainProvider, Complete, Ctx, Currency, Detail, Error,
    LockPosition, Money, Position, Protocol, ProtocolAdapter, Provenance, Reading, Source,
    Staleness, Timestamp, Token, TokenAddress, Wallet, YieldKind, YieldPosition,
};
use gluonscan_evm::{
    decode_two_u256, decode_u256, encode_balance_of, encode_selector_with_address, eth_call,
};
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

        // 1. Catalog (wallet-independent): every PT/YT for the chain. The API paginates under
        // `results`; walk pages until a short/empty one. `total` is deliberately NOT trusted as the
        // stop condition — a missing or inaccurate `total` must never silently drop held positions
        // in later pages. A hard bound turns a runaway (always-full) response into a fail-closed
        // error rather than an infinite loop or silent truncation.
        const PAGE: usize = 100;
        const MAX_MARKETS: usize = 100_000;
        let mut markets = Vec::new();
        let mut skip = 0usize;
        loop {
            let url = format!(
                "{}/v1/{}/markets?limit={PAGE}&skip={skip}",
                self.base(),
                chain_id
            );
            let raw = cx.http.get(&url, &[]).await?;
            let json: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                    message: format!("Pendle catalog not JSON: {e}"),
                })?;
            let page = json
                .pointer("/results")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Error::Integrity {
                    message: "Pendle catalog missing `results`".into(),
                })?;
            let n = page.len();
            markets.extend(page.iter().cloned());
            if n < PAGE {
                break;
            }
            skip += n;
            if skip > MAX_MARKETS {
                return Err(Error::Integrity {
                    message: "Pendle catalog exceeded the sane page bound; refusing to truncate"
                        .into(),
                });
            }
        }

        let mut candidates = Vec::new();
        for m in &markets {
            // Only active markets carry live PT/YT worth a balance check.
            if !m.get("isActive").and_then(|v| v.as_bool()).unwrap_or(true) {
                continue;
            }
            let expiry = m
                .get("expiry")
                .and_then(|e| e.as_str())
                .and_then(iso_to_unix)
                .map(Timestamp);
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
        // PT/YT (and sPENDLE) are ERC-20s the wallet holds; report them as receipt tokens so a
        // consumer listing raw balances does not double-count them as loose tokens.
        let mut receipt_tokens = Vec::new();
        for c in candidates {
            let token_addr = c
                .token
                .address
                .as_ref()
                .and_then(TokenAddress::as_evm)
                .ok_or_else(|| Error::Integrity {
                    message: format!("Pendle token `{}` has no EVM address", c.token.symbol),
                })?;
            let out = eth_call(rpc, chain, None, token_addr, encode_balance_of(owner)).await?;
            let balance = decode_u256(&out)?;
            if balance.is_zero() {
                continue;
            }
            receipt_tokens.push(token_addr);
            positions.push(Position::Yield(
                YieldPosition::new(priced_amount(c.token, balance, c.price_usd)?, c.kind)
                    .with_expiry(c.expiry)
                    .with_apy(c.apy),
            ));
        }

        // 3. The vePENDLE lock lives only on Ethereum mainnet.
        if chain == Chain::Ethereum {
            positions.extend(read_lock(rpc, chain, owner).await?);
        }

        let reading = Reading::new(
            Protocol::Pendle,
            chain,
            Source::Api,
            positions,
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        )
        .with_receipt_tokens(receipt_tokens);
        Ok(Complete::new(reading))
    }
}

/// Read the Ethereum-only vePENDLE lock for `owner`.
async fn read_lock(
    rpc: &dyn ChainProvider,
    chain: Chain,
    owner: Address,
) -> Result<Vec<Position>, Error> {
    // Ethereum mainnet contracts (lowercased so parsing never depends on EIP-55 checksum).
    let pendle_token = Address::from_str("0x808507121b80c02388fad14726482e061b8da827").ok();
    let ve = Address::from_str("0x4f30a9d41b80ecc5b94306ab4364951ae3170210")
        .expect("valid vePENDLE address");

    let mut out = Vec::new();

    // vePENDLE: positionData(user) -> (lockedPendle, expiry); balanceOf(user) -> governance power.
    let pd = eth_call(
        rpc,
        chain,
        None,
        ve,
        encode_selector_with_address([0xcb, 0x6b, 0x4f, 0x3c], owner),
    )
    .await?;
    let (locked_pendle, expiry) = decode_two_u256(&pd)?;
    if locked_pendle > U256::ZERO {
        let gov = decode_u256(&eth_call(rpc, chain, None, ve, encode_balance_of(owner)).await?)?;
        out.push(Position::Lock(
            LockPosition::new(vec![
                Amount::from_raw(Token::evm("PENDLE", pendle_token, 18), locked_pendle)?,
                Amount::from_raw(Token::evm("vePENDLE", Some(ve), 18), gov)?,
            ])
            .with_unlock_at(Some(Timestamp(expiry.saturating_to::<i64>()))),
        ));
    }

    Ok(out)
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
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("Pendle token `{symbol}` missing or out-of-range decimals"),
        })?;
    let address = v
        .get("address")
        .and_then(|s| s.as_str())
        .and_then(|s| Address::from_str(s).ok());
    let name = v.get("name").and_then(|s| s.as_str()).map(str::to_string);
    Ok(Token::evm(symbol, address, decimals).with_name(name))
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

/// Parse an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS...Z`) to Unix seconds via the
/// days-from-civil algorithm — no date crate needed.
fn iso_to_unix(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let se: i64 = s.get(17..19)?.parse().ok()?;
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = (if yy >= 0 { yy } else { yy - 399 }) / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
