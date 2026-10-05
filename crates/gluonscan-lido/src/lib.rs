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
//! valuation stays in the pricing layer and this read never converts or fabricates an amount.
//! Lido staking rewards are auto-compounded into the balance, never separately claimable, so
//! `rewards` is always empty.

use alloy_primitives::{address, Address};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, StakePosition, Staleness, Token, Wallet,
};
use gluonscan_evm::{decode_u256, encode_balance_of, eth_call};
use rust_decimal::Decimal;

const STETH: Address = address!("ae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
const WSTETH: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");

/// Current stETH staking APR, as a fraction (`0.0222` = 2.22%), from the Lido API. Best-effort: any
/// failure (network, schema, parse) yields `None` — the APY is informational and must never fail the
/// balance read closed. The API returns a percentage (`2.216` = 2.216%), normalized here.
async fn fetch_apr(cx: &Ctx) -> Option<Decimal> {
    const APR_URL: &str = "https://eth-api.lido.fi/v1/protocol/steth/apr/last";
    let raw = cx.http.get(APR_URL, &[]).await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let apr = json.get("data")?.get("apr")?.as_f64()?;
    Decimal::try_from(apr / 100.0).ok()
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
        let mut receipt_tokens = Vec::new();
        if !steth_raw.is_zero() {
            staked.push(Amount::from_raw(
                Token::evm("stETH", Some(STETH), 18),
                steth_raw,
            )?);
            receipt_tokens.push(STETH);
        }
        if !wsteth_raw.is_zero() {
            staked.push(Amount::from_raw(
                Token::evm("wstETH", Some(WSTETH), 18),
                wsteth_raw,
            )?);
            receipt_tokens.push(WSTETH);
        }

        // Empty balances are a valid "no position" result, not an error.
        let positions = if staked.is_empty() {
            Vec::new()
        } else {
            let apy = fetch_apr(cx).await;
            vec![Position::Stake(StakePosition::new(staked).with_apy(apy))]
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
