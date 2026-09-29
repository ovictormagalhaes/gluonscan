//! # gluonscan-uniswap
//!
//! Uniswap V3 adapter. Discovery is a single subgraph query ([`Source::Subgraph`]); uncollected
//! fees come from an on-chain `collect()` static call per active position. The in-range flag is
//! computed locally from the pool tick. Principal token amounts (from the Q64.96 liquidity math)
//! are a follow-up increment; this backend currently returns fees + range.

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, LiquidityPosition, Position, Protocol,
    ProtocolAdapter, Provenance, Reading, Source, Staleness, Token,
};
use gluonscan_evm::{decode_two_u256, encode_collect, eth_call};
use gluonscan_math::is_in_range;
use rust_decimal::Decimal;

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::Fees];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum, Chain::Base, Chain::Arbitrum];

/// Uniswap V3 adapter (subgraph discovery + on-chain fees).
#[derive(Debug, Default, Clone)]
pub struct UniswapV3 {
    subgraph: Option<String>,
}

impl UniswapV3 {
    /// Construct with default (placeholder) subgraph endpoints.
    pub fn new() -> Self {
        UniswapV3 { subgraph: None }
    }

    /// Override the subgraph endpoint (real gateway URL, or a test double).
    pub fn with_subgraph(mut self, url: impl Into<String>) -> Self {
        self.subgraph = Some(url.into());
        self
    }

    fn subgraph_url(&self, chain: Chain) -> String {
        self.subgraph
            .clone()
            .unwrap_or_else(|| format!("https://subgraph.invalid/uniswap-v3/{chain:?}"))
    }

    /// The NonfungiblePositionManager address per supported chain.
    fn position_manager(chain: Chain) -> Option<Address> {
        let s = match chain {
            Chain::Ethereum | Chain::Arbitrum => "0xC36442b4a4522E871399CD717aBDD847Ab11FE88",
            Chain::Base => "0x03a520b32C04Bf3bEEf7BEb72E919cf822Ed34f1",
            _ => return None,
        };
        Address::from_str(s).ok()
    }
}

#[async_trait]
impl ProtocolAdapter for UniswapV3 {
    fn protocol(&self) -> Protocol {
        Protocol::UniswapV3
    }

    fn source(&self) -> Source {
        Source::Subgraph
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
        detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if !SUPPORTED_CHAINS.contains(&chain) {
            return Err(Error::Permanent {
                message: format!("Uniswap V3 not configured for {chain:?}"),
            });
        }

        let body = query_body(&format!("{owner:#x}"));
        let raw = cx.http.post(&self.subgraph_url(chain), body).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Uniswap subgraph response not JSON: {e}"),
        })?;
        let items = json
            .pointer("/data/positions")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Integrity {
                message: "Uniswap response missing data.positions".into(),
            })?;

        let with_fees = matches!(detail, Detail::Full);
        let mut positions = Vec::with_capacity(items.len());
        for item in items {
            positions.push(
                self.parse_position(item, owner, chain, with_fees, cx)
                    .await?,
            );
        }

        let reading = Reading {
            protocol: Protocol::UniswapV3,
            chain,
            source: Source::Subgraph,
            positions,
            provenance: Provenance {
                source: Source::Subgraph,
                chain,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}

impl UniswapV3 {
    async fn parse_position(
        &self,
        item: &serde_json::Value,
        owner: Address,
        chain: Chain,
        with_fees: bool,
        cx: &Ctx,
    ) -> Result<Position, Error> {
        let id = str_at(item, "/id")?;
        let liquidity = str_at(item, "/liquidity")?;
        let lower: i32 = parse_at(item, "/tickLower/tickIdx")?;
        let upper: i32 = parse_at(item, "/tickUpper/tickIdx")?;
        let current: i32 = parse_at(item, "/pool/tick")?;

        let token0 = parse_token(item.pointer("/pool/token0"))?;
        let token1 = parse_token(item.pointer("/pool/token1"))?;

        let mut uncollected_fees = Vec::new();
        let active = liquidity != "0";
        if with_fees && active {
            let token_id = U256::from_str(&id).map_err(|e| Error::Integrity {
                message: format!("Uniswap position id not a number: {e}"),
            })?;
            let manager = UniswapV3::position_manager(chain).ok_or_else(|| Error::Permanent {
                message: format!("no Uniswap position manager for {chain:?}"),
            })?;
            let data = encode_collect(token_id, owner);
            let out = eth_call(cx.rpc()?.as_ref(), chain, manager, data).await?;
            let (fee0, fee1) = decode_two_u256(&out)?;
            uncollected_fees.push(amount(token0.clone(), fee0));
            uncollected_fees.push(amount(token1.clone(), fee1));
        }

        Ok(Position::Liquidity(LiquidityPosition {
            assets: Vec::new(),
            uncollected_fees,
            in_range: Some(is_in_range(current, lower, upper)),
        }))
    }
}

fn query_body(owner: &str) -> String {
    let query = r#"query($owner:String!){positions(where:{owner:$owner}){id liquidity tickLower{tickIdx} tickUpper{tickIdx} pool{tick token0{id symbol decimals} token1{id symbol decimals}}}}"#;
    serde_json::json!({ "query": query, "variables": { "owner": owner } }).to_string()
}

fn str_at(v: &serde_json::Value, pointer: &str) -> Result<String, Error> {
    v.pointer(pointer)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Integrity {
            message: format!("Uniswap position missing `{pointer}`"),
        })
}

fn parse_at<T: FromStr>(v: &serde_json::Value, pointer: &str) -> Result<T, Error> {
    str_at(v, pointer)?
        .parse::<T>()
        .map_err(|_| Error::Integrity {
            message: format!("Uniswap position field `{pointer}` did not parse"),
        })
}

fn parse_token(v: Option<&serde_json::Value>) -> Result<Token, Error> {
    let v = v.ok_or_else(|| Error::Integrity {
        message: "Uniswap position missing token".into(),
    })?;
    let symbol = v
        .pointer("/symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let decimals: u8 = str_at(v, "/decimals")?
        .parse()
        .map_err(|_| Error::Integrity {
            message: format!("Uniswap token `{symbol}` bad decimals"),
        })?;
    let address = v
        .pointer("/id")
        .and_then(|s| s.as_str())
        .and_then(|s| Address::from_str(s).ok());
    Ok(Token {
        symbol,
        address,
        decimals,
    })
}

fn amount(token: Token, raw: U256) -> Amount {
    let scale = token.decimals.min(28) as u32;
    let mantissa: i128 = raw.to_string().parse().unwrap_or(i128::MAX);
    Amount {
        amount: Decimal::from_i128_with_scale(mantissa, scale),
        token,
        usd: None,
    }
}
