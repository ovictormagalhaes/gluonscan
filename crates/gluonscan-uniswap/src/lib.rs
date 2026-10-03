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
    scaled, Amount, Capability, Chain, Complete, Ctx, Detail, Error, EventKind, History,
    HistoryEvent, LiquidityPosition, Position, PositionStatus, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, Staleness, Timestamp, Token, Wallet,
};
use gluonscan_evm::{decode_two_u256, encode_collect, eth_call};
use gluonscan_math::{
    get_amounts_for_liquidity, get_sqrt_ratio_at_tick, is_in_range, MAX_TICK, MIN_TICK,
};
use rust_decimal::Decimal;

const CAPABILITIES: &[Capability] = &[Capability::Positions, Capability::Fees, Capability::History];
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

    /// Deposit/withdraw/collect history for one position. The V3 subgraph does not expose mints/burns
    /// under `Position`, so history is reconstructed from `positionSnapshots` (taken at every
    /// mint/burn/collect): deltas between consecutive snapshots become events. Each two-token delta is
    /// emitted as paired single-token events sharing the same tx and timestamp; the consumer re-pairs
    /// by token address. `position` is the NFT token id (required).
    async fn read_history(
        &self,
        _owner: &Wallet,
        chain: Chain,
        position: Option<&str>,
        since: Option<Timestamp>,
        cx: &Ctx,
    ) -> Result<Complete<History>, Error> {
        let url = self.subgraph_url(chain)?;
        let position_id = position.ok_or_else(|| Error::Permanent {
            message: "Uniswap V3 history needs a position selector (the NFT token id)".into(),
        })?;
        let since_ts = since.map(|t| t.0).unwrap_or(0);
        // Include the snapshot just before `since` so the first delta is computed correctly; on first
        // sync (since == 0) the creation snapshot is the baseline.
        let baseline_ts = if since_ts > 0 { since_ts - 1 } else { 0 };

        let body = history_query_body(position_id, baseline_ts);
        let raw = cx.http.post(url, body, &[]).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Uniswap snapshots response not JSON: {e}"),
        })?;
        let snaps = json
            .pointer("/data/positionSnapshots")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Integrity {
                message: "Uniswap response missing data.positionSnapshots".into(),
            })?;

        let mut events: Vec<HistoryEvent> = Vec::new();
        if let Some(first) = snaps.first() {
            let (token0, token1) = snapshot_tokens(first)?;

            let mut prev = SnapshotTotals::default();
            let mut is_first = true;
            for snap in snaps {
                // A snapshot with malformed numeric fields is skipped rather than diffed into
                // synthetic zeroes (never fabricate history amounts).
                let Some(curr) = SnapshotTotals::parse(snap) else {
                    continue;
                };
                let tx = snapshot_tx(snap);

                if is_first {
                    // The baseline snapshot carries no event, except a first-ever sync whose creation
                    // snapshot already holds a deposit.
                    if since_ts == 0 && (curr.dep0 > Decimal::ZERO || curr.dep1 > Decimal::ZERO) {
                        push_pair(
                            &mut events,
                            EventKind::Deposit,
                            &token0,
                            &token1,
                            curr.dep0,
                            curr.dep1,
                            &tx,
                            curr.ts,
                        )?;
                    }
                    prev = curr;
                    is_first = false;
                    continue;
                }

                let d_dep0 = curr.dep0 - prev.dep0;
                let d_dep1 = curr.dep1 - prev.dep1;
                let d_wd0 = curr.wd0 - prev.wd0;
                let d_wd1 = curr.wd1 - prev.wd1;
                let d_col0 = curr.col0 - prev.col0;
                let d_col1 = curr.col1 - prev.col1;

                if d_dep0 > Decimal::ZERO || d_dep1 > Decimal::ZERO {
                    push_pair(
                        &mut events,
                        EventKind::Deposit,
                        &token0,
                        &token1,
                        d_dep0,
                        d_dep1,
                        &tx,
                        curr.ts,
                    )?;
                }
                if d_wd0 > Decimal::ZERO || d_wd1 > Decimal::ZERO {
                    push_pair(
                        &mut events,
                        EventKind::Withdraw,
                        &token0,
                        &token1,
                        d_wd0,
                        d_wd1,
                        &tx,
                        curr.ts,
                    )?;
                }
                if (d_col0 > Decimal::ZERO || d_col1 > Decimal::ZERO) && d_col0 != d_col1 {
                    // Equal non-zero deltas are the known V3 subgraph bug (token1 denominated as
                    // token0); a genuine collect never reports identical amounts, so it is dropped.
                    push_pair(
                        &mut events,
                        EventKind::CollectFees,
                        &token0,
                        &token1,
                        d_col0.max(Decimal::ZERO),
                        d_col1.max(Decimal::ZERO),
                        &tx,
                        curr.ts,
                    )?;
                }

                prev = curr;
            }
        }

        let history = History::new(
            Protocol::UniswapV3,
            chain,
            events,
            Provenance::new(Source::Subgraph, chain, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(history))
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
        let pool_id = item
            .pointer("/pool/id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let sqrt_price = str_at(item, "/pool/sqrtPrice").ok();
        let created_at = item
            .pointer("/transaction/timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok());
        Ok(Position::Liquidity(
            LiquidityPosition::new(
                token0,
                token1,
                lower,
                upper,
                current,
                is_in_range(current, lower, upper),
            )
            .with_id(Some(id))
            .with_pool(pool_id)
            .with_fee_tier_bps(fee_tier_bps)
            .with_sqrt_price(sqrt_price)
            .with_tick_spacing(tick_spacing_for_fee(fee_tier_bps))
            .with_created_at(created_at)
            .with_assets(assets)
            .with_uncollected_fees(uncollected_fees)
            .with_deposited(deposited)
            .with_withdrawn(withdrawn)
            .with_collected_fees(collected_fees)
            .with_status(status),
        ))
    }
}

/// Uniswap V3 tick spacing for a fee tier (hundredths of a bp). The canonical factory mapping.
fn tick_spacing_for_fee(fee_tier_bps: Option<u32>) -> Option<i32> {
    match fee_tier_bps? {
        100 => Some(1),
        500 => Some(10),
        3000 => Some(60),
        10000 => Some(200),
        _ => None,
    }
}

fn query_body(owner: &str) -> String {
    let query = r#"query($owner:String!){positions(where:{owner:$owner}){id liquidity depositedToken0 depositedToken1 withdrawnToken0 withdrawnToken1 collectedFeesToken0 collectedFeesToken1 tickLower{tickIdx} tickUpper{tickIdx} transaction{timestamp} pool{id tick sqrtPrice feeTier token0{id symbol name decimals} token1{id symbol name decimals}}}}"#;
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

/// GraphQL body for one position's snapshot stream at/after `baseline_ts`, with the pool token
/// identities needed to label each delta.
fn history_query_body(position_id: &str, baseline_ts: i64) -> String {
    let query = r#"query($pos:String!,$ts:BigInt!){positionSnapshots(where:{position:$pos,timestamp_gte:$ts},orderBy:timestamp,orderDirection:asc,first:1000){timestamp depositedToken0 depositedToken1 withdrawnToken0 withdrawnToken1 collectedFeesToken0 collectedFeesToken1 transaction{id} pool{token0{id symbol name decimals} token1{id symbol name decimals}}}}"#;
    serde_json::json!({ "query": query, "variables": { "pos": position_id, "ts": baseline_ts } })
        .to_string()
}

/// The `(token0, token1)` identities from a snapshot's pool.
fn snapshot_tokens(snap: &serde_json::Value) -> Result<(Token, Token), Error> {
    let token0 = parse_token(snap.pointer("/pool/token0"))?;
    let token1 = parse_token(snap.pointer("/pool/token1"))?;
    Ok((token0, token1))
}

/// A snapshot's transaction hash (empty when absent — history is still usable for delta math).
fn snapshot_tx(snap: &serde_json::Value) -> String {
    snap.pointer("/transaction/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// The six cumulative lifetime totals a position snapshot carries, plus its timestamp.
#[derive(Default, Clone, Copy)]
struct SnapshotTotals {
    dep0: Decimal,
    dep1: Decimal,
    wd0: Decimal,
    wd1: Decimal,
    col0: Decimal,
    col1: Decimal,
    ts: i64,
}

impl SnapshotTotals {
    /// Parse the cumulative BigDecimal fields + timestamp. `None` if any is malformed, so the caller
    /// skips the snapshot rather than diffing it into synthetic zeroes.
    fn parse(snap: &serde_json::Value) -> Option<SnapshotTotals> {
        let dec = |k: &str| {
            snap.get(k)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Decimal>().ok())
        };
        Some(SnapshotTotals {
            dep0: dec("depositedToken0")?,
            dep1: dec("depositedToken1")?,
            wd0: dec("withdrawnToken0")?,
            wd1: dec("withdrawnToken1")?,
            col0: dec("collectedFeesToken0")?,
            col1: dec("collectedFeesToken1")?,
            ts: snap
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())?,
        })
    }
}

/// Emit a two-token delta as paired single-token events: one per non-zero side, sharing `kind`, `tx`
/// and `ts`. The consumer re-pairs by token address.
#[allow(
    clippy::too_many_arguments,
    reason = "internal helper appending a token-pair event to the output sink; a mix of event data and the accumulator, not a cohesive struct"
)]
fn push_pair(
    events: &mut Vec<HistoryEvent>,
    kind: EventKind,
    token0: &Token,
    token1: &Token,
    a0: Decimal,
    a1: Decimal,
    tx: &str,
    ts: i64,
) -> Result<(), Error> {
    if a0 > Decimal::ZERO {
        events.push(HistoryEvent::new(
            kind,
            Amount::from_decimal(token0.clone(), a0)?,
            tx,
            Timestamp(ts),
        ));
    }
    if a1 > Decimal::ZERO {
        events.push(HistoryEvent::new(
            kind,
            Amount::from_decimal(token1.clone(), a1)?,
            tx,
            Timestamp(ts),
        ));
    }
    Ok(())
}
