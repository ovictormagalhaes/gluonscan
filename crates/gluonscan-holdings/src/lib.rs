//! # gluonscan-holdings
//!
//! Idle wallet balances — [`Protocol::Wallet`]. [`EvmTokenHoldings`] reads a wallet's ERC-20
//! balances (tokens not deployed in any protocol) from a Moralis-style API (header-authenticated)
//! and normalizes each into a [`Position::Wallet`]. Prices are left `None` — pricing is a separate
//! operation.
//!
//! (Solana SPL balances, native BTC balance, and NFTs are follow-up readers in this crate.)

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, Complete, Ctx, Detail, Error, Position, Protocol,
    ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet, WalletBalance,
};
use gluonscan_solana::get_token_balances;

const MORALIS_API: &str = "https://deep-index.moralis.io/api/v2.2";
const CAPABILITIES: &[Capability] = &[Capability::Positions];
const SUPPORTED_CHAINS: &[Chain] = &[
    Chain::Ethereum,
    Chain::Base,
    Chain::Arbitrum,
    Chain::Optimism,
    Chain::Polygon,
    Chain::Bnb,
];

/// EVM ERC-20 holdings via a Moralis-style API (header-authenticated).
#[derive(Debug, Clone)]
pub struct EvmTokenHoldings {
    base: String,
    api_key: String,
}

impl EvmTokenHoldings {
    /// Construct with an API key (and the default Moralis base).
    pub fn new(api_key: impl Into<String>) -> Self {
        EvmTokenHoldings {
            base: MORALIS_API.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Override the API base (a proxy or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    fn chain_slug(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Ethereum => "eth",
            Chain::Base => "base",
            Chain::Arbitrum => "arbitrum",
            Chain::Optimism => "optimism",
            Chain::Polygon => "polygon",
            Chain::Bnb => "bsc",
            _ => return None,
        })
    }
}

#[async_trait]
impl ProtocolAdapter for EvmTokenHoldings {
    fn protocol(&self) -> Protocol {
        Protocol::Wallet
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
        let slug = EvmTokenHoldings::chain_slug(chain).ok_or_else(|| Error::Permanent {
            message: format!("token holdings not configured for {chain:?}"),
        })?;

        let url = format!("{}/{owner:#x}/erc20?chain={slug}", self.base);
        let headers = [("X-API-Key", self.api_key.as_str())];
        let raw = cx.http.get(&url, &headers).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("holdings response not JSON: {e}"),
        })?;
        let tokens = json.as_array().ok_or_else(|| Error::Integrity {
            message: "holdings response was not an array".into(),
        })?;

        let mut positions = Vec::with_capacity(tokens.len());
        for t in tokens {
            let symbol = t
                .get("symbol")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let decimals =
                t.get("decimals")
                    .and_then(|d| d.as_u64())
                    .ok_or_else(|| Error::Integrity {
                        message: format!("holding `{symbol}` missing decimals"),
                    })? as u8;
            let raw_balance =
                t.get("balance")
                    .and_then(|b| b.as_str())
                    .ok_or_else(|| Error::Integrity {
                        message: format!("holding `{symbol}` missing balance"),
                    })?;
            let raw_u256 = U256::from_str(raw_balance).map_err(|e| Error::Integrity {
                message: format!("holding `{symbol}` balance not a number: {e}"),
            })?;
            let address = t
                .get("token_address")
                .and_then(|a| a.as_str())
                .and_then(|s| Address::from_str(s).ok());

            positions.push(Position::Wallet(WalletBalance {
                amount: Amount {
                    amount: scaled(raw_u256, decimals)?,
                    token: Token {
                        symbol,
                        address,
                        decimals,
                    },
                    usd: None,
                },
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
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

/// Solana SPL token balances (idle tokens in the wallet) via the injected on-chain transport.
#[derive(Debug, Default, Clone)]
pub struct SolanaTokenHoldings;

impl SolanaTokenHoldings {
    /// Construct the reader.
    pub fn new() -> Self {
        SolanaTokenHoldings
    }
}

#[async_trait]
impl ProtocolAdapter for SolanaTokenHoldings {
    fn protocol(&self) -> Protocol {
        Protocol::Wallet
    }

    fn source(&self) -> Source {
        Source::OnChain
    }

    fn capabilities(&self) -> &'static [Capability] {
        CAPABILITIES
    }

    fn supported_chains(&self) -> &'static [Chain] {
        &[Chain::Solana]
    }

    async fn read(
        &self,
        owner: &Wallet,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if chain != Chain::Solana {
            return Err(Error::Permanent {
                message: format!("Solana holdings only; got {chain:?}"),
            });
        }
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();

        let balances = get_token_balances(rpc, wallet).await?;
        let mut positions = Vec::with_capacity(balances.len());
        for b in balances {
            let raw = U256::from_str(&b.amount_raw).map_err(|e| Error::Integrity {
                message: format!("Solana balance not a number: {e}"),
            })?;
            positions.push(Position::Wallet(WalletBalance {
                amount: Amount {
                    amount: scaled(raw, b.decimals)?,
                    token: Token {
                        symbol: String::new(),
                        address: None,
                        decimals: b.decimals,
                    },
                    usd: None,
                },
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain: Chain::Solana,
            source: Source::OnChain,
            positions,
            provenance: Provenance {
                source: Source::OnChain,
                chain: Chain::Solana,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}
