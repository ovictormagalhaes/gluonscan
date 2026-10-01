//! # gluonscan-uniswap
//!
//! Uniswap V3 adapter. Discovery is a single subgraph query ([`Source::Subgraph`]); uncollected
//! fees come from an on-chain `collect()` static call per active position (Full detail only). It
//! returns the **complete** position resource: principal amounts (from the Q64.96 liquidity math),
//! lifetime deposited/withdrawn/collected, fee tier, tick range, and the in-range flag.

use std::collections::HashMap;
use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, Complete, Ctx, Detail, Error, LiquidityPosition, Position,
    PositionStatus, Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token,
    Wallet,
};
use gluonscan_evm::{decode_two_u256, encode_collect, eth_call};
use gluonscan_math::{
    get_amounts_for_liquidity, get_sqrt_ratio_at_tick, is_in_range, MAX_TICK, MIN_TICK,
};
use rust_decimal::Decimal;

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::Fees];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum, Chain::Base, Chain::Arbitrum];

/// Uniswap V3 adapter (subgraph discovery + on-chain fees). One instance serves every chain it has
/// a subgraph URL for, so it can back a single long-lived engine that routes by chain.
#[derive(Debug, Default, Clone)]
pub struct UniswapV3 {
    subgraphs: HashMap<Chain, String>,
}

impl UniswapV3 {
    /// Construct with no subgraphs configured (a chain with none fails closed on read).
    pub fn new() -> Self {
        UniswapV3 {
            subgraphs: HashMap::new(),
        }
    }

    /// Set one chain's subgraph URL (real gateway URL, or a test double).
    pub fn with_subgraph(mut self, chain: Chain, url: impl Into<String>) -> Self {
        self.subgraphs.insert(chain, url.into());
        self
    }

    /// Set all per-chain subgraph URLs at once.
    pub fn with_subgraphs(mut self, subgraphs: HashMap<Chain, String>) -> Self {
        self.subgraphs = subgraphs;
        self
    }

    fn subgraph_url(&self, chain: Chain) -> Result<&str, Error> {
        self.subgraphs
            .get(&chain)
            .map(String::as_str)
            .ok_or_else(|| Error::Permanent {
                message: format!("no Uniswap V3 subgraph configured for {chain:?}"),
            })
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
        owner: &Wallet,
        chain: Chain,
        detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if !SUPPORTED_CHAINS.contains(&chain) {
            return Err(Error::Permanent {
                message: format!("Uniswap V3 not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;

        let body = query_body(&format!("{owner:#x}"));
        let raw = cx.http.post(self.subgraph_url(chain)?, body, &[]).await?;
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

        let reading = Reading::new(
            Protocol::UniswapV3,
            chain,
            Source::Subgraph,
            positions,
            Provenance::new(Source::Subgraph, chain, cx.clock.now(), Staleness::Live),
        );
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
        // tickLower/tickUpper are `{tickIdx}` on the canonical subgraph but a bare scalar on some
        // (e.g. Base); accept both. pool.tick is always a scalar.
        let lower = tick_at(item, "tickLower")?;
        let upper = tick_at(item, "tickUpper")?;
        let current =
            item.pointer("/pool/tick")
                .and_then(to_i32)
                .ok_or_else(|| Error::Integrity {
                    message: "Uniswap position missing `/pool/tick`".into(),
                })?;
        if !(MIN_TICK..=MAX_TICK).contains(&lower) || !(MIN_TICK..=MAX_TICK).contains(&upper) {
            return Err(Error::Integrity {
                message: format!("Uniswap ticks out of range: {lower}..{upper}"),
            });
        }

        let token0 = parse_token(item.pointer("/pool/token0"))?;
        let token1 = parse_token(item.pointer("/pool/token1"))?;
        let fee_tier_bps = str_at(item, "/pool/feeTier")
            .ok()
            .and_then(|s| s.parse().ok());

        // Principal amounts from the Q64.96 liquidity math (no network).
        let sqrt_price =
            U256::from_str(&str_at(item, "/pool/sqrtPrice")?).map_err(|e| Error::Integrity {
                message: format!("Uniswap sqrtPrice not a number: {e}"),
            })?;
        let liq = U256::from_str(&liquidity).map_err(|e| Error::Integrity {
            message: format!("Uniswap liquidity not a number: {e}"),
        })?;
        let (amt0, amt1) = get_amounts_for_liquidity(
            sqrt_price,
            get_sqrt_ratio_at_tick(lower),
            get_sqrt_ratio_at_tick(upper),
            liq,
        );
        let assets = vec![
            raw_amount(token0.clone(), amt0)?,
            raw_amount(token1.clone(), amt1)?,
        ];

        // Lifetime totals — the subgraph reports these as human-scaled decimals.
        let deposited = vec![
            human_amount(token0.clone(), item, "/depositedToken0")?,
            human_amount(token1.clone(), item, "/depositedToken1")?,
        ];
        let withdrawn = vec![
            human_amount(token0.clone(), item, "/withdrawnToken0")?,
            human_amount(token1.clone(), item, "/withdrawnToken1")?,
        ];
        // Known subgraph corruption: token0's collected fees mirrored into token1, seen as
        // byte-identical non-zero values across two distinct tokens (it produced phantom fees and
        // absurd APY downstream). That is not physically plausible, so fail closed rather than emit
        // degraded fee data.
        if let (Some(a), Some(b)) = (
            item.pointer("/collectedFeesToken0")
                .and_then(|v| v.as_str()),
            item.pointer("/collectedFeesToken1")
                .and_then(|v| v.as_str()),
        ) {
            if a == b
                && Decimal::from_str(a)
                    .map(|v| v != Decimal::ZERO)
                    .unwrap_or(false)
            {
                return Err(Error::Integrity {
                    message: "Uniswap subgraph collectedFees token0 == token1 (known corruption); \
                         refusing to emit phantom fees"
                        .into(),
                });
            }
        }
        let collected_fees = vec![
            human_amount(token0.clone(), item, "/collectedFeesToken0")?,
            human_amount(token1.clone(), item, "/collectedFeesToken1")?,
        ];

        // Uncollected fees are an on-chain read; fetched only at Full detail.
        let mut uncollected_fees = Vec::new();
        if with_fees && liquidity != "0" {
            let token_id = U256::from_str(&id).map_err(|e| Error::Integrity {
                message: format!("Uniswap position id not a number: {e}"),
            })?;
            let manager = UniswapV3::position_manager(chain).ok_or_else(|| Error::Permanent {
                message: format!("no Uniswap position manager for {chain:?}"),
            })?;
            let data = encode_collect(token_id, owner);
            let out = eth_call(cx.rpc()?.as_ref(), chain, Some(owner), manager, data).await?;
            let (fee0, fee1) = decode_two_u256(&out)?;
            uncollected_fees.push(raw_amount(token0.clone(), fee0)?);
            uncollected_fees.push(raw_amount(token1.clone(), fee1)?);
        }

        // Zero-liquidity positions are returned (not dropped) so the consumer can account for a
        // dormant LP without it touching totals. The subgraph exposes no APR (derived from pool
        // stats by the consumer).
        let status = if liquidity == "0" {
            PositionStatus::Inactive
        } else {
            PositionStatus::Active
        };
        Ok(Position::Liquidity(
            LiquidityPosition::new(
                token0,
                token1,
                lower,
                upper,
                current,
                is_in_range(current, lower, upper),
            )
            .with_fee_tier_bps(fee_tier_bps)
            .with_assets(assets)
            .with_uncollected_fees(uncollected_fees)
            .with_deposited(deposited)
            .with_withdrawn(withdrawn)
            .with_collected_fees(collected_fees)
            .with_status(status),
        ))
    }
}

fn query_body(owner: &str) -> String {
    let query = r#"query($owner:String!){positions(where:{owner:$owner}){id liquidity depositedToken0 depositedToken1 withdrawnToken0 withdrawnToken1 collectedFeesToken0 collectedFeesToken1 tickLower{tickIdx} tickUpper{tickIdx} pool{tick sqrtPrice feeTier token0{id symbol name decimals} token1{id symbol name decimals}}}}"#;
    serde_json::json!({ "query": query, "variables": { "owner": owner } }).to_string()
}

/// Read an `i32` from a JSON number or numeric string.
fn to_i32(v: &serde_json::Value) -> Option<i32> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        .and_then(|n| i32::try_from(n).ok())
}

/// Parse a tick from either `{ tickIdx: N }` (canonical subgraph) or a bare scalar `N` (Base).
fn tick_at(item: &serde_json::Value, field: &str) -> Result<i32, Error> {
    let v = item.get(field).ok_or_else(|| Error::Integrity {
        message: format!("Uniswap position missing `{field}`"),
    })?;
    to_i32(v.get("tickIdx").unwrap_or(v)).ok_or_else(|| Error::Integrity {
        message: format!("Uniswap `{field}` is not a tick: {v}"),
    })
}

fn str_at(v: &serde_json::Value, pointer: &str) -> Result<String, Error> {
    v.pointer(pointer)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Integrity {
            message: format!("Uniswap position missing `{pointer}`"),
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
    let decimals: u8 = v
        .pointer("/decimals")
        .and_then(|d| {
            d.as_u64()
                .or_else(|| d.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .and_then(|n| u8::try_from(n).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("Uniswap token `{symbol}` bad decimals"),
        })?;
    let address = v
        .pointer("/id")
        .and_then(|s| s.as_str())
        .and_then(|s| Address::from_str(s).ok());
    let name = v
        .pointer("/name")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    Ok(Token::evm(symbol, address, decimals).with_name(name))
}

/// Build an [`Amount`] from a raw base-unit integer, scaling by the token's decimals.
fn raw_amount(token: Token, raw: U256) -> Result<Amount, Error> {
    let amount = scaled(raw, token.decimals)?;
    Ok(Amount {
        token,
        raw,
        amount,
        usd: None,
    })
}

/// Build an [`Amount`] from a subgraph BigDecimal (already in human token units).
fn human_amount(token: Token, item: &serde_json::Value, pointer: &str) -> Result<Amount, Error> {
    let s = str_at(item, pointer)?;
    let amount = Decimal::from_str(&s).map_err(|e| Error::Integrity {
        message: format!("Uniswap `{pointer}` not a decimal: {e}"),
    })?;
    Amount::from_decimal(token, amount)
}
