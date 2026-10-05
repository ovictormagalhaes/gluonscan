//! # gluonscan-ethena
//!
//! Ethena staked-USDe adapter (Ethereum). A wallet's Ethena staking position *is* its **sUSDe**
//! balance — the ERC-4626 vault share that accrues the protocol yield as its redemption value
//! rises. The adapter reads that balance on-chain and returns it as a [`StakePosition`], reporting
//! sUSDe as a `receipt_token` so a consumer that also lists idle wallet balances drops it and does
//! not double-count the stake.
//!
//! USDe held directly is the plain (unstaked) stablecoin, not a protocol position, so it is left to
//! the idle-wallet reader. Yield accrues inside the sUSDe share price (`convertToAssets`), not as a
//! separate claimable, so `rewards` is always empty. Valuation is left to the pricing layer (sUSDe
//! is priced by contract address), so the read never converts or fabricates an amount.

use alloy_primitives::{address, Address};
use async_trait::async_trait;
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter,
    Provenance, Reading, Source, StakePosition, Staleness, Token, Wallet,
};
use gluonscan_evm::{decode_u256, encode_balance_of, eth_call};
use rust_decimal::Decimal;

const SUSDE: Address = address!("9D39A5DE30e57443BfF2A8307A4256c8797A3497");

/// Current sUSDe staking yield, as a fraction (`0.0496` = 4.96%), from DeFiLlama. Best-effort: any
/// failure yields `None` (the APY is informational and must never fail the balance read closed).
///
/// DeFiLlama (a CDN-backed, server-reachable yields source) is used rather than Ethena's own app API
/// (`app.ethena.fi`), which is unreachable from datacenter IPs. The sUSDe pool id is stable; the
/// chart's last point is the current yield as a percentage.
async fn fetch_apr(cx: &Ctx) -> Option<Decimal> {
    const SUSDE_POOL: &str = "66985a81-9c51-46ca-9977-42b4fe7bc6df";
    let url = format!("https://yields.llama.fi/chart/{SUSDE_POOL}");
    let raw = cx.http.get(&url, &[]).await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pct = json.get("data")?.as_array()?.last()?.get("apy")?.as_f64()?;
    Decimal::try_from(pct / 100.0).ok()
}

const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[Chain::Ethereum];

/// Ethena staked-USDe adapter. Reads the sUSDe balance on Ethereum via on-chain `balanceOf`.
#[derive(Debug, Default, Clone)]
pub struct Ethena;

impl Ethena {
    /// Construct the adapter.
    pub fn new() -> Self {
        Ethena
    }
}

#[async_trait]
impl ProtocolAdapter for Ethena {
    fn protocol(&self) -> Protocol {
        Protocol::Ethena
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
                message: format!("Ethena not configured for {chain:?}"),
            });
        }
        let owner = owner.evm()?;
        let rpc = cx.rpc()?.as_ref();

        let susde_raw =
            decode_u256(&eth_call(rpc, chain, None, SUSDE, encode_balance_of(owner)).await?)?;

        // A zero balance is a valid "no position" result, not an error.
        let (positions, receipt_tokens) = if susde_raw.is_zero() {
            (Vec::new(), Vec::new())
        } else {
            let staked = vec![Amount::from_raw(
                Token::evm("sUSDe", Some(SUSDE), 18),
                susde_raw,
            )?];
            let apy = fetch_apr(cx).await;
            (
                vec![Position::Stake(StakePosition::new(staked).with_apy(apy))],
                vec![SUSDE],
            )
        };

        let reading = Reading::new(
            Protocol::Ethena,
            chain,
            Source::OnChain,
            positions,
            Provenance::new(Source::OnChain, chain, cx.clock.now(), Staleness::Live),
        )
        .with_receipt_tokens(receipt_tokens);
        Ok(Complete::new(reading))
    }
}
