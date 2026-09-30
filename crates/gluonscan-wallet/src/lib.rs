//! # gluonscan-wallet
//!
//! Idle wallet contents — [`Protocol::Wallet`]. [`EvmWallet`] reads a wallet's ERC-20 balances
//! (tokens not deployed in any protocol) from a Moralis-style API (header-authenticated),
//! [`SolanaWallet`] reads SPL balances over the injected RPC, and [`BitcoinWallet`] reads the
//! native BTC balance from an explorer. Each idle balance normalizes into a [`Position::Wallet`].
//! Prices are left `None` — pricing is a separate operation.
//!
//! [`EvmNfts`] and [`SolanaNfts`] read collectible NFTs the wallet holds ([`Protocol::Nfts`]).
//! The EVM reader skips contracts that are protocol positions (e.g. a Uniswap V3 LP is read as a
//! [`Position::Liquidity`], not an NFT) so nothing is double-counted; the Solana reader discovers
//! NFT mints by owner and decodes their Metaplex name/collection.

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::{
    scaled, Amount, Capability, Chain, Complete, Ctx, Detail, Error, NftPosition, Position,
    Protocol, ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet,
    WalletBalance,
};
use gluonscan_evm::eth_get_balance;
use gluonscan_solana::{
    decode_metadata, get_account_info, get_native_balance, get_token_accounts_by_owner,
    get_token_balances, metadata_pda,
};

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

/// Moralis chain slug for the EVM chains this crate's readers support.
fn moralis_chain_slug(chain: Chain) -> Option<&'static str> {
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

/// The native coin symbol for an EVM chain.
fn evm_native_symbol(chain: Chain) -> Option<&'static str> {
    Some(match chain {
        Chain::Ethereum | Chain::Base | Chain::Arbitrum | Chain::Optimism => "ETH",
        Chain::Bnb => "BNB",
        Chain::Polygon => "POL",
        _ => return None,
    })
}

/// Contracts whose NFTs are protocol positions (read by the protocol adapters), not collectibles.
/// The generic NFT reader skips these so a Uniswap position is never double-counted as an NFT.
/// Uniswap V3 NonfungiblePositionManager (lowercased): `0xc3644…` on Ethereum/Arbitrum,
/// `0x03a52…` on Base.
const PROTOCOL_NFT_CONTRACTS: &[&str] = &[
    "0xc36442b4a4522e871399cd717abdd847ab11fe88",
    "0x03a520b32c04bf3beef7beb72e919cf822ed34f1",
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
        let slug = moralis_chain_slug(chain).ok_or_else(|| Error::Permanent {
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
            let decimals = t
                .get("decimals")
                .and_then(|d| d.as_u64())
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| Error::Integrity {
                    message: format!("wallet balance `{symbol}` missing or out-of-range decimals"),
                })?;
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
            let name = t.get("name").and_then(|n| n.as_str()).map(str::to_string);

            positions.push(Position::Wallet(WalletBalance {
                amount: Amount {
                    raw: raw_u256,
                    amount: scaled(raw_u256, decimals)?,
                    token: Token {
                        symbol,
                        name,
                        address,
                        decimals,
                    },
                    usd: None,
                },
                // Passed through, not acted on — the consumer decides what to hide.
                possible_spam: t.get("possible_spam").and_then(|v| v.as_bool()),
                verified_contract: t.get("verified_contract").and_then(|v| v.as_bool()),
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain,
            source: Source::Api,
            positions,
            receipt_tokens: Vec::new(),
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
                    raw,
                    amount: scaled(raw, b.decimals)?,
                    token: Token {
                        symbol: String::new(),
                        name: None,
                        address: None,
                        decimals: b.decimals,
                    },
                    usd: None,
                },
                possible_spam: None,
                verified_contract: None,
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain: Chain::Solana,
            source: Source::OnChain,
            positions,
            receipt_tokens: Vec::new(),
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
                    raw: U256::from(sats),
                    amount: scaled(U256::from(sats), 8)?,
                    token: Token {
                        symbol: "BTC".to_string(),
                        name: Some("Bitcoin".to_string()),
                        address: None,
                        decimals: 8,
                    },
                    usd: None,
                },
                possible_spam: None,
                verified_contract: None,
            }));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain: Chain::Bitcoin,
            source: Source::Api,
            positions,
            receipt_tokens: Vec::new(),
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

/// EVM wallet NFTs (collectibles) via a Moralis-style API — [`Protocol::Nfts`].
///
/// This reads NFTs the wallet *holds*, distinct from NFTs that are protocol positions (a Uniswap
/// V3 LP is an ERC-721, but it is read as a [`Position::Liquidity`] by the Uniswap adapter). To
/// avoid double-counting, a known set of protocol-position contracts (e.g. the Uniswap V3
/// NonfungiblePositionManager) is skipped, and NFTs the indexer flags as spam are dropped. Floor
/// prices are left `None` — pricing is a separate operation.
#[derive(Debug, Clone)]
pub struct EvmNfts {
    base: String,
    api_key: String,
}

impl EvmNfts {
    /// Construct with an API key (and the default Moralis base).
    pub fn new(api_key: impl Into<String>) -> Self {
        EvmNfts {
            base: MORALIS_API.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Override the API base (a proxy or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }
}

#[async_trait]
impl ProtocolAdapter for EvmNfts {
    fn protocol(&self) -> Protocol {
        Protocol::Nfts
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
        let slug = moralis_chain_slug(chain).ok_or_else(|| Error::Permanent {
            message: format!("wallet NFTs not configured for {chain:?}"),
        })?;

        let url = format!("{}/{owner:#x}/nft?chain={slug}", self.base);
        let headers = [("X-API-Key", self.api_key.as_str())];
        let raw = cx.http.get(&url, &headers).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("NFT response not JSON: {e}"),
        })?;
        let items = json
            .get("result")
            .and_then(|r| r.as_array())
            .ok_or_else(|| Error::Integrity {
                message: "NFT response missing result array".into(),
            })?;

        let mut positions = Vec::with_capacity(items.len());
        // Contracts that are protocol positions (a Uniswap LP NFT), not collectibles: recorded as
        // receipt tokens and skipped as collectibles so the consumer can dedup, never dropped
        // silently. Spam is flagged, not dropped — the consumer decides.
        let mut receipt_tokens = Vec::new();
        for it in items {
            let collection = it
                .get("token_address")
                .and_then(|a| a.as_str())
                .ok_or_else(|| Error::Integrity {
                    message: "NFT missing token_address".into(),
                })?;
            if PROTOCOL_NFT_CONTRACTS.contains(&collection.to_lowercase().as_str()) {
                if let Ok(addr) = Address::from_str(collection) {
                    receipt_tokens.push(addr);
                }
                continue;
            }
            let token_id = it
                .get("token_id")
                .and_then(|t| t.as_str())
                .ok_or_else(|| Error::Integrity {
                    message: "NFT missing token_id".into(),
                })?
                .to_string();
            let name = it
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            positions.push(Position::Nft(NftPosition {
                collection: collection.to_string(),
                token_id,
                name,
                floor_price: None,
                possible_spam: it.get("possible_spam").and_then(|s| s.as_bool()),
            }));
        }

        let reading = Reading {
            protocol: Protocol::Nfts,
            chain,
            source: Source::Api,
            positions,
            receipt_tokens,
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

/// Solana wallet NFTs (collectibles) via the injected on-chain transport — [`Protocol::Nfts`].
///
/// Discovery is by owner: the SPL token accounts holding exactly one indivisible unit
/// (`amount == 1`, `decimals == 0`) are the NFT mints. For each, the Metaplex metadata account is
/// read to decode the name and verified collection (fail-safe: an undecodable field is left
/// `None`, never guessed). The mint is the token id; the collection falls back to the mint when no
/// verified collection is declared. Floor prices are left `None` — pricing is a separate operation.
#[derive(Debug, Default, Clone)]
pub struct SolanaNfts;

impl SolanaNfts {
    /// Construct the reader.
    pub fn new() -> Self {
        SolanaNfts
    }
}

#[async_trait]
impl ProtocolAdapter for SolanaNfts {
    fn protocol(&self) -> Protocol {
        Protocol::Nfts
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
                message: format!("Solana NFTs only; got {chain:?}"),
            });
        }
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();

        let mints = get_token_accounts_by_owner(rpc, wallet).await?;
        let mut positions = Vec::with_capacity(mints.len());
        for mint in mints {
            let pda = metadata_pda(&mint)?;
            let metadata = match get_account_info(rpc, &pda).await? {
                Some(data) => decode_metadata(&data),
                None => Default::default(),
            };
            positions.push(Position::Nft(NftPosition {
                collection: metadata.collection.unwrap_or_else(|| mint.clone()),
                token_id: mint,
                name: metadata.name,
                floor_price: None,
                possible_spam: None,
            }));
        }

        let reading = Reading {
            protocol: Protocol::Nfts,
            chain: Chain::Solana,
            source: Source::OnChain,
            positions,
            receipt_tokens: Vec::new(),
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

/// EVM native coin balance (ETH / BNB / POL) via the injected on-chain transport — a
/// [`Position::Wallet`] with a `None` token address (native coins have no contract).
#[derive(Debug, Default, Clone)]
pub struct EvmNativeBalance;

impl EvmNativeBalance {
    /// Construct the reader.
    pub fn new() -> Self {
        EvmNativeBalance
    }
}

#[async_trait]
impl ProtocolAdapter for EvmNativeBalance {
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
        let symbol = evm_native_symbol(chain).ok_or_else(|| Error::Permanent {
            message: format!("no native coin symbol for {chain:?}"),
        })?;
        let rpc = cx.rpc()?.as_ref();
        let wei = eth_get_balance(rpc, chain, owner).await?;

        let mut positions = Vec::new();
        if wei > U256::ZERO {
            positions.push(Position::Wallet(WalletBalance::new(Amount::from_raw(
                Token::new(symbol, None, 18),
                wei,
            )?)));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain,
            source: Source::OnChain,
            positions,
            receipt_tokens: Vec::new(),
            provenance: Provenance {
                source: Source::OnChain,
                chain,
                block: None,
                at: cx.clock.now(),
                staleness: Staleness::Live,
            },
        };
        Ok(Complete::new(reading))
    }
}

/// Native SOL balance via the injected on-chain transport — a [`Position::Wallet`].
#[derive(Debug, Default, Clone)]
pub struct SolanaNativeBalance;

impl SolanaNativeBalance {
    /// Construct the reader.
    pub fn new() -> Self {
        SolanaNativeBalance
    }
}

#[async_trait]
impl ProtocolAdapter for SolanaNativeBalance {
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
                message: format!("Solana native balance only; got {chain:?}"),
            });
        }
        let wallet = owner.solana()?;
        let rpc = cx.rpc()?.as_ref();
        let lamports = get_native_balance(rpc, wallet).await?;

        let mut positions = Vec::new();
        if lamports > 0 {
            positions.push(Position::Wallet(WalletBalance::new(Amount::from_raw(
                Token::new("SOL", None, 9),
                U256::from(lamports),
            )?)));
        }

        let reading = Reading {
            protocol: Protocol::Wallet,
            chain: Chain::Solana,
            source: Source::OnChain,
            positions,
            receipt_tokens: Vec::new(),
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
