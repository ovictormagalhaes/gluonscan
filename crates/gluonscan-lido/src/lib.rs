//! # gluonscan-lido
//!
//! Lido liquid-staking adapter (Ethereum). A wallet's Lido position *is* its stETH / wstETH
//! balance — there is no separate deposit receipt, so the adapter reads both ERC-20 balances
//! on-chain and returns them as a [`StakePosition`]. The tokens are reported as
//! `receipt_tokens` so a consumer that also lists idle wallet balances drops them and does not
//! double-count the stake as loose holdings.
//!
//! stETH is rebasing (its `balanceOf` already tracks the ETH-equivalent, rewards included) and
//! wstETH is its non-rebasing wrapper; both are priced directly by contract address downstream, so
//! valuation stays in the pricing layer and this read never values or fabricates an amount;
//! `underlying` is the on-chain redemption amount, unpriced.
//! Lido staking rewards are auto-compounded into the balance, never separately claimable, so
//! `rewards` is always empty.

use alloy_primitives::{address, Address};
use async_trait::async_trait;
use gluonscan_core::{
    is_dust, Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol,
    ProtocolAdapter, Provenance, Rate, RateBasis, RateWindow, Rates, Reading, Source,
    StakePosition, Staleness, Timestamp, Token, Wallet,
};
use gluonscan_evm::{convert_amount, decode_u256, encode_balance_of, eth_call};
use rust_decimal::Decimal;
use std::str::FromStr;

const STETH: Address = address!("ae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
const WSTETH: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");
/// `getStETHByWstETH(uint256)`: the stETH a wstETH amount unwraps to.
const GET_STETH_BY_WSTETH: [u8; 4] = [0xbb, 0x29, 0x52, 0xfc];

const APR_LAST_URL: &str = "https://eth-api.lido.fi/v1/protocol/steth/apr/last";
const APR_SMA_URL: &str = "https://eth-api.lido.fi/v1/protocol/steth/apr/sma";
/// The SMA endpoint averages this many daily oracle reports.
const SMA_REPORTS: usize = 7;
/// The SMA's newest report must be at most this old (one missed daily report of slack).
const MAX_SMA_AGE_SECS: i64 = 48 * 3600;

/// The stETH staking APRs from the Lido API, as fractions. Both are simple APRs, never APYs.
///
/// - `apr/last`: the APR of the last daily oracle report, an [`RateWindow::Epoch`] of 24h dated at
///   the report's `timeUnix`.
/// - `apr/sma`: the mean of the last 7 daily reports' APRs, a 168h [`RateWindow::Trailing`] dated at
///   the newest report. Omitted unless all 7 reports are present.
///
/// Best-effort: any failure (network, schema, parse) leaves that rate out — rates are informational
/// and must never fail the balance read closed. The API returns percentages (`2.216` = 2.216%).
async fn fetch_rates(cx: &Ctx) -> Rates {
    let mut rates = Rates::new();
    if let Some(rate) = fetch_json(cx, APR_LAST_URL)
        .await
        .and_then(|j| parse_last(&j))
    {
        rates.insert(rate);
    }
    if let Some(rate) = fetch_json(cx, APR_SMA_URL)
        .await
        .and_then(|j| parse_sma(&j, cx.clock.now().0))
    {
        rates.insert(rate);
    }
    rates
}

async fn fetch_json(cx: &Ctx, url: &str) -> Option<serde_json::Value> {
    let raw = cx.http.get(url, &[]).await.ok()?;
    serde_json::from_str(&raw).ok()
}

fn parse_last(json: &serde_json::Value) -> Option<Rate> {
    let data = json.get("data")?;
    Some(Rate::new(
        percent(data.get("apr")?)?,
        RateBasis::Apr,
        RateWindow::Epoch { hours: Some(24) },
        Source::Api,
        Timestamp(data.get("timeUnix")?.as_i64()?),
    ))
}

fn parse_sma(json: &serde_json::Value, now: i64) -> Option<Rate> {
    let data = json.get("data")?;
    let reports = data.get("aprs")?.as_array()?;
    if reports.len() < SMA_REPORTS {
        return None;
    }
    let newest = reports
        .iter()
        .filter_map(|r| r.get("timeUnix")?.as_i64())
        .max()?;
    if newest > now || now - newest > MAX_SMA_AGE_SECS {
        return None;
    }
    Some(Rate::new(
        percent(data.get("smaApr")?)?,
        RateBasis::Apr,
        RateWindow::Trailing { hours: 168 },
        Source::Api,
        Timestamp(newest),
    ))
}

/// A JSON percentage number as a fraction, parsed from serde_json's shortest round-trip literal
/// rather than by `f64` arithmetic.
fn percent(v: &serde_json::Value) -> Option<Decimal> {
    if !v.is_number() {
        return None;
    }
    let literal = v.to_string();
    Decimal::from_str(&literal)
        .or_else(|_| Decimal::from_scientific(&literal))
        .ok()?
        .checked_div(Decimal::ONE_HUNDRED)
}

const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum];

/// Lido liquid-staking adapter. Reads stETH + wstETH balances on Ethereum via on-chain `balanceOf`.
#[derive(Debug, Default, Clone)]
pub struct Lido;

impl Lido {
    /// Construct the adapter.
    pub fn new() -> Self {
        Lido
    }
}

#[async_trait]
impl ProtocolAdapter for Lido {
    fn protocol(&self) -> Protocol {
        Protocol::Lido
    }

    fn source(&self) -> Source {
        Source::OnChain
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
        if chain != Chain::Ethereum {
            return Err(Error::Permanent {
                message: format!("Lido not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let rpc = cx.rpc()?.as_ref();

        // Both reads are required: a wstETH balance without a successful stETH read (or vice versa)
        // is a partial result, so any RPC failure propagates and fails the whole read closed.
        let steth_raw =
            decode_u256(&eth_call(rpc, chain, None, STETH, encode_balance_of(owner)).await?)?;
        let wsteth_raw =
            decode_u256(&eth_call(rpc, chain, None, WSTETH, encode_balance_of(owner)).await?)?;

        let mut staked = Vec::new();
        let mut underlying = Vec::new();
        let mut receipt_tokens = Vec::new();
        if !steth_raw.is_zero() {
            receipt_tokens.push(STETH);
        }
        if !is_dust(steth_raw, 18) {
            staked.push(Amount::from_raw(
                Token::evm("stETH", Some(STETH), 18),
                steth_raw,
            )?);
            // stETH rebases: its balance already is the underlying.
            underlying.push(None);
        }
        if !wsteth_raw.is_zero() {
            receipt_tokens.push(WSTETH);
        }
        if !is_dust(wsteth_raw, 18) {
            staked.push(Amount::from_raw(
                Token::evm("wstETH", Some(WSTETH), 18),
                wsteth_raw,
            )?);
            let steth = convert_amount(rpc, chain, WSTETH, GET_STETH_BY_WSTETH, wsteth_raw).await;
            underlying.push(
                steth.and_then(|raw| {
                    Amount::from_raw(Token::evm("stETH", Some(STETH), 18), raw).ok()
                }),
            );
        }

        // Empty balances are a valid "no position" result, not an error.
        let positions = if staked.is_empty() {
            Vec::new()
        } else {
            let rates = fetch_rates(cx).await;
            vec![Position::Stake(
                StakePosition::new(staked)
                    .with_rates(rates)
                    .with_underlying(underlying),
            )]
        };

        let reading = Reading::new(
            Protocol::Lido,
            chain,
            Source::OnChain,
            positions,
            Provenance::new(Source::OnChain, chain, cx.clock.now(), Staleness::Live),
        )
        .with_receipt_tokens(receipt_tokens);
        Ok(Complete::new(reading))
    }
}
