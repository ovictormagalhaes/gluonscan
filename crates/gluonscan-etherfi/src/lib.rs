//! # gluonscan-etherfi
//!
//! ether.fi liquid-restaking adapter (Ethereum). A wallet's ether.fi position *is* its weETH /
//! eETH balance — there is no separate deposit receipt — so the adapter reads both ERC-20 balances
//! on-chain and returns them as a [`StakePosition`]. The tokens are reported as `receipt_tokens`
//! so a consumer that also lists idle wallet balances drops them and does not double-count the
//! stake as loose holdings.
//!
//! eETH is rebasing (its `balanceOf` already tracks the ETH-equivalent, restaking yield included)
//! and weETH is its non-rebasing wrapper; both are priced directly by contract address downstream,
//! so valuation stays in the pricing layer and this read never converts or fabricates an amount.
//! Restaking yield accrues inside the balance (eETH rebase) or the weETH exchange rate, and the
//! ether.fi points / ETHFI / EIGEN rewards are off-chain season-gated Merkle claims rather than a
//! continuously-accruing on-chain balance, so `rewards` is always empty.

use alloy_primitives::{address, Address};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, StakePosition, Staleness, Token, Wallet,
};
use gluonscan_evm::{decode_u256, encode_balance_of, eth_call};

const WEETH: Address = address!("Cd5fE23C85820F7B72D0926FC9b05b43E359b7ee");
const EETH: Address = address!("35fA164735182de50811E8e2E824cFb9B6118ac2");

const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum];

/// ether.fi liquid-restaking adapter. Reads weETH + eETH balances on Ethereum via on-chain
/// `balanceOf`.
#[derive(Debug, Default, Clone)]
pub struct EtherFi;

impl EtherFi {
    /// Construct the adapter.
    pub fn new() -> Self {
        EtherFi
    }
}

#[async_trait]
impl ProtocolAdapter for EtherFi {
    fn protocol(&self) -> Protocol {
        Protocol::EtherFi
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
                message: format!("ether.fi not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let rpc = cx.rpc()?.as_ref();

        // Both reads are required: a weETH balance without a successful eETH read (or vice versa) is
        // a partial result, so any RPC failure propagates and fails the whole read closed.
        let weeth_raw =
            decode_u256(&eth_call(rpc, chain, None, WEETH, encode_balance_of(owner)).await?)?;
        let eeth_raw =
            decode_u256(&eth_call(rpc, chain, None, EETH, encode_balance_of(owner)).await?)?;

        let mut staked = Vec::new();
        let mut receipt_tokens = Vec::new();
        if !weeth_raw.is_zero() {
            staked.push(Amount::from_raw(
                Token::evm("weETH", Some(WEETH), 18),
                weeth_raw,
            )?);
            receipt_tokens.push(WEETH);
        }
        if !eeth_raw.is_zero() {
            staked.push(Amount::from_raw(
                Token::evm("eETH", Some(EETH), 18),
                eeth_raw,
            )?);
            receipt_tokens.push(EETH);
        }

        // Empty balances are a valid "no position" result, not an error.
        let positions = if staked.is_empty() {
            Vec::new()
        } else {
            vec![Position::Stake(StakePosition::new(staked))]
        };

        let reading = Reading::new(
            Protocol::EtherFi,
            chain,
            Source::OnChain,
            positions,
            Provenance::new(Source::OnChain, chain, cx.clock.now(), Staleness::Live),
        )
        .with_receipt_tokens(receipt_tokens);
        Ok(Complete::new(reading))
    }
}
