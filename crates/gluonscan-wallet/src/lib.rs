//! # gluonscan-wallet
//!
//! Idle wallet contents — [`Protocol::Wallet`]. [`EvmWallet`] reads a wallet's ERC-20 balances
//! (tokens not deployed in any protocol) from a Moralis-style API (header-authenticated),
//! [`SolanaWallet`] reads SPL balances over the injected RPC, and [`BitcoinWallet`] reads the
//! native BTC balance from an explorer. Each idle balance normalizes into a [`Position::Wallet`].
//! Prices are left `None` — pricing is a separate operation.
//!
//! (NFTs are a follow-up reader in this crate.)

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

/// EVM idle ERC-20 balances via a Moralis-style API (header-authenticated).
#[derive(Debug, Clone)]
pub struct EvmWallet {
    base: String,
    api_key: String,
}

impl EvmWallet {
    /// Construct with an API key (and the default Moralis base).
    pub fn new(api_key: impl Into<String>) -> Self {
        EvmWallet {
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
impl ProtocolAdapter for EvmWallet {
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
        let slug = EvmWallet::chain_slug(chain).ok_or_else(|| Error::Permanent {
            message: format!("wallet balances not configured for {chain:?}"),
        })?;

        let url = format!("{}/{owner:#x}/erc20?chain={slug}", self.base);
        let headers = [("X-API-Key", self.api_key.as_str())];
        let raw = cx.http.get(&url, &headers).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("wallet balances response not JSON: {e}"),
        })?;
        let tokens = json.as_array().ok_or_else(|| Error::Integrity {
            message: "wallet balances response was not an array".into(),
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
                        message: format!("wallet balance `{symbol}` missing decimals"),
                    })? as u8;
            let raw_balance =
                t.get("balance")
                    .and_then(|b| b.as_str())
                    .ok_or_else(|| Error::Integrity {
                        message: format!("wallet balance `{symbol}` missing balance"),
                    })?;
            let raw_u256 = U256::from_str(raw_balance).map_err(|e| Error::Integrity {
                message: format!("wallet balance `{symbol}` not a number: {e}"),
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
pub struct SolanaWallet;

impl SolanaWallet {
    /// Construct the reader.
    pub fn new() -> Self {
        SolanaWallet
    }
}

#[async_trait]
impl ProtocolAdapter for SolanaWallet {
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
                message: format!("Solana wallet only; got {chain:?}"),
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

const MEMPOOL_API: &str = "https://mempool.space/api";

/// Native BTC balance via a mempool.space-style explorer API.
#[derive(Debug, Clone)]
pub struct BitcoinWallet {
    base: String,
}

impl BitcoinWallet {
    /// Construct with the default public explorer.
    pub fn new() -> Self {
        BitcoinWallet {
            base: MEMPOOL_API.to_string(),
        }
    }

    /// Override the explorer base (a proxy or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }
}

impl Default for BitcoinWallet {
    fn default() -> Self {
        BitcoinWallet::new()
    }
}

#[async_trait]
impl ProtocolAdapter for BitcoinWallet {
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
        &[Chain::Bitcoin]
    }

    async fn read(
        &self,
        owner: &Wallet,
        chain: Chain,
        _detail: Detail,
        cx: &Ctx,
    ) -> Result<Complete<Reading>, Error> {
        if chain != Chain::Bitcoin {
            return Err(Error::Permanent {
                message: format!("Bitcoin wallet only; got {chain:?}"),
            });
        }
        let address = owner.bitcoin()?;

        let url = format!("{}/address/{address}", self.base);
        let raw = cx.http.get(&url, &[]).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Bitcoin address response not JSON: {e}"),
        })?;
        let funded = json
            .pointer("/chain_stats/funded_txo_sum")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| Error::Integrity {
                message: "Bitcoin response missing chain_stats.funded_txo_sum".into(),
            })?;
        let spent = json
            .pointer("/chain_stats/spent_txo_sum")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| Error::Integrity {
                message: "Bitcoin response missing chain_stats.spent_txo_sum".into(),
            })?;
        let sats = funded.saturating_sub(spent);

        let mut positions = Vec::new();
        if sats > 0 {
            positions.push(Position::Wallet(WalletBalance {
                amount: Amount {
                    amount: scaled(U256::from(sats), 8)?,
                    token: Token {
                        symbol: "BTC".to_string(),
                        address: None,
                        decimals: 8,
                    },
                    usd: None,
                },
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain: Chain::Bitcoin,
            source: Source::Api,
            positions,
            provenance: Provenance {
                source: Source::Api,
                chain: Chain::Bitcoin,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}
