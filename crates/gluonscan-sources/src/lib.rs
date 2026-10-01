//! # gluonscan-sources
//!
//! Concrete [`PriceSource`] implementations. Each takes an injected [`Http`] client, so pricing is
//! testable offline against recorded contracts and shares the host's transport/limits.
//!
//! A missing price is [`Error::AbsentPrice`] — never a fabricated `0` or `1`.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use gluonscan_core::{Asset, Chain, Error, Http, PriceSource};
use rust_decimal::Decimal;

const COINGECKO_API: &str = "https://api.coingecko.com";
/// CoinGecko's CloudFront edge 403s requests without a `User-Agent`, and the `Http` port does not
/// mandate one, so this source always sends its own.
const DEFAULT_USER_AGENT: &str = concat!(
    "gluonscan-sources/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ovictormagalhaes/gluonscan)"
);

/// CoinGecko price source: prices a native coin (by coin id), an EVM token (by contract address),
/// or a Solana SPL mint (by mint address) on a chain. Always sends a `User-Agent`; an optional
/// demo API key is sent as `x-cg-demo-api-key`.
pub struct CoinGecko {
    http: Arc<dyn Http>,
    base: String,
    user_agent: String,
    api_key: Option<String>,
}

impl CoinGecko {
    /// Construct with the default public API and an injected HTTP client.
    pub fn new(http: Arc<dyn Http>) -> Self {
        CoinGecko {
            http,
            base: COINGECKO_API.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            api_key: None,
        }
    }

    /// Override the API base (a pro/proxy endpoint or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    /// Override the `User-Agent` sent on every request.
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Send a demo API key (`x-cg-demo-api-key`) on every request.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    fn headers(&self) -> Vec<(&str, &str)> {
        let mut headers = vec![("User-Agent", self.user_agent.as_str())];
        if let Some(key) = &self.api_key {
            headers.push(("x-cg-demo-api-key", key.as_str()));
        }
        headers
    }

    /// The CoinGecko asset-platform id for a chain (for token-by-contract pricing).
    fn platform(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Ethereum => "ethereum",
            Chain::Base => "base",
            Chain::Polygon => "polygon-pos",
            Chain::Arbitrum => "arbitrum-one",
            Chain::Optimism => "optimistic-ethereum",
            Chain::Bnb => "binance-smart-chain",
            Chain::Monad => "monad",
            Chain::Hyperliquid => "hyperevm",
            Chain::Solana => "solana",
            _ => return None,
        })
    }

    /// The CoinGecko coin id for a chain's native coin.
    fn native_coin_id(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Bitcoin => "bitcoin",
            Chain::Ethereum | Chain::Base | Chain::Arbitrum | Chain::Optimism => "ethereum",
            Chain::Polygon => "matic-network",
            Chain::Bnb => "binancecoin",
            Chain::Monad => "monad",
            Chain::Hyperliquid => "hyperliquid",
            Chain::Solana => "solana",
            _ => return None,
        })
    }

    async fn token_price(&self, platform: &str, key: &str) -> Result<Decimal, Error> {
        let url = format!(
            "{}/api/v3/simple/token_price/{platform}?contract_addresses={key}&vs_currencies=usd",
            self.base
        );
        let raw = self.http.get(&url, &self.headers()).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("CoinGecko response not JSON: {e}"),
        })?;
        // The response keys the price under the queried address; CoinGecko lowercases EVM
        // addresses but preserves case-sensitive Solana mints, so read the single returned entry
        // rather than matching the key back.
        single_entry_usd(&json).ok_or(Error::AbsentPrice {
            asset: key.to_string(),
        })
    }

    /// Batch several contract addresses / SPL mints on one platform into a single call. Returns a
    /// map keyed by the address CoinGecko echoes (EVM lowercased, SPL mint case-preserved); an
    /// address with no price is simply absent from the map.
    async fn batch_token_prices(
        &self,
        platform: &str,
        keys: &[String],
    ) -> Result<HashMap<String, Decimal>, Error> {
        let csv = keys.join(",");
        let url = format!(
            "{}/api/v3/simple/token_price/{platform}?contract_addresses={csv}&vs_currencies=usd",
            self.base
        );
        let raw = self.http.get(&url, &self.headers()).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("CoinGecko response not JSON: {e}"),
        })?;
        let mut out = HashMap::new();
        if let Some(obj) = json.as_object() {
            for (addr, v) in obj {
                if let Some(price) = v.get("usd").and_then(json_decimal) {
                    out.insert(addr.clone(), price);
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl PriceSource for CoinGecko {
    async fn price_usd(&self, chain: Chain, asset: Asset) -> Result<Decimal, Error> {
        match asset {
            Asset::Native => {
                let id = CoinGecko::native_coin_id(chain).ok_or_else(|| Error::Permanent {
                    message: format!("CoinGecko has no native coin id for {chain:?}"),
                })?;
                let url = format!(
                    "{}/api/v3/simple/price?ids={id}&vs_currencies=usd",
                    self.base
                );
                let raw = self.http.get(&url, &self.headers()).await?;
                let json: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                        message: format!("CoinGecko response not JSON: {e}"),
                    })?;
                json.pointer(&format!("/{id}/usd"))
                    .and_then(json_decimal)
                    .ok_or(Error::AbsentPrice {
                        asset: id.to_string(),
                    })
            }
            Asset::Token(token) => {
                let platform = CoinGecko::platform(chain).ok_or_else(|| Error::Permanent {
                    message: format!("CoinGecko has no asset platform for {chain:?}"),
                })?;
                self.token_price(platform, &format!("{token:#x}")).await
            }
            Asset::Mint(mint) => self.token_price("solana", &mint).await,
            other => Err(Error::Permanent {
                message: format!("CoinGecko cannot price asset kind {other:?}"),
            }),
        }
    }

    /// Batched pricing: one call for all contract/mint addresses on the chain's platform, one call
    /// for the native coin if requested. Results are returned per asset in input order.
    async fn prices_usd(&self, chain: Chain, assets: &[Asset]) -> Vec<Result<Decimal, Error>> {
        let contract_keys: Vec<String> = assets
            .iter()
            .filter_map(|a| match a {
                Asset::Token(t) => Some(format!("{t:#x}")),
                Asset::Mint(m) => Some(m.clone()),
                _ => None,
            })
            .collect();

        let contract_prices: HashMap<String, Decimal> = if contract_keys.is_empty() {
            HashMap::new()
        } else if let Some(platform) = CoinGecko::platform(chain) {
            self.batch_token_prices(platform, &contract_keys)
                .await
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        // EVM addresses come back lowercased; mints keep case. Match exact first, then lowercased.
        let lowered: HashMap<String, Decimal> = contract_prices
            .iter()
            .map(|(k, v)| (k.to_lowercase(), *v))
            .collect();

        // One native lookup for the whole batch (a failure drops native to absent, not an error).
        let native_price: Option<Decimal> = if assets.iter().any(|a| matches!(a, Asset::Native)) {
            self.price_usd(chain, Asset::Native).await.ok()
        } else {
            None
        };

        assets
            .iter()
            .map(|asset| match asset {
                Asset::Native => native_price.ok_or(Error::AbsentPrice {
                    asset: "native".to_string(),
                }),
                Asset::Token(t) => {
                    let key = format!("{t:#x}");
                    contract_prices
                        .get(&key)
                        .or_else(|| lowered.get(&key.to_lowercase()))
                        .copied()
                        .ok_or(Error::AbsentPrice { asset: key })
                }
                Asset::Mint(m) => contract_prices
                    .get(m)
                    .or_else(|| lowered.get(&m.to_lowercase()))
                    .copied()
                    .ok_or(Error::AbsentPrice { asset: m.clone() }),
                other => Err(Error::Permanent {
                    message: format!("CoinGecko cannot price asset kind {other:?}"),
                }),
            })
            .collect()
    }
}

const CMC_API: &str = "https://pro-api.coinmarketcap.com";

/// CoinMarketCap price source. Prices an EVM token by resolving its contract address to a CMC id
/// (`/v1/cryptocurrency/info`) and then quoting it, and a native coin by its symbol
/// (`/v2/cryptocurrency/quotes/latest`). Uses the `X-CMC_PRO_API_KEY` header — the reason the
/// [`Http`] port carries headers. Solana SPL mints are not resolvable here; use [`CoinGecko`].
pub struct CoinMarketCap {
    http: Arc<dyn Http>,
    base: String,
    api_key: String,
}

impl CoinMarketCap {
    /// Construct with an injected HTTP client and a CMC Pro API key.
    pub fn new(http: Arc<dyn Http>, api_key: impl Into<String>) -> Self {
        CoinMarketCap {
            http,
            base: CMC_API.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Override the API base (a proxy or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    /// The CMC ticker symbol for a chain's native coin.
    fn native_symbol(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Bitcoin => "BTC",
            Chain::Ethereum | Chain::Base | Chain::Arbitrum | Chain::Optimism => "ETH",
            Chain::Bnb => "BNB",
            Chain::Solana => "SOL",
            _ => return None,
        })
    }

    async fn quote_by_id(&self, id: u64, headers: &[(&str, &str)]) -> Result<Decimal, Error> {
        let url = format!(
            "{}/v2/cryptocurrency/quotes/latest?id={id}&convert=USD",
            self.base
        );
        let raw = self.http.get(&url, headers).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("CMC quote response not JSON: {e}"),
        })?;
        json.pointer(&format!("/data/{id}/quote/USD/price"))
            .and_then(json_decimal)
            .ok_or(Error::AbsentPrice {
                asset: id.to_string(),
            })
    }
}

#[async_trait]
impl PriceSource for CoinMarketCap {
    async fn price_usd(&self, chain: Chain, asset: Asset) -> Result<Decimal, Error> {
        let headers = [("X-CMC_PRO_API_KEY", self.api_key.as_str())];
        match asset {
            Asset::Native => {
                let symbol =
                    CoinMarketCap::native_symbol(chain).ok_or_else(|| Error::Permanent {
                        message: format!("CoinMarketCap has no native symbol for {chain:?}"),
                    })?;
                let url = format!(
                    "{}/v2/cryptocurrency/quotes/latest?symbol={symbol}&convert=USD",
                    self.base
                );
                let raw = self.http.get(&url, &headers).await?;
                let json: serde_json::Value =
                    serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                        message: format!("CMC quote response not JSON: {e}"),
                    })?;
                json.pointer(&format!("/data/{symbol}/0/quote/USD/price"))
                    .and_then(json_decimal)
                    .ok_or(Error::AbsentPrice {
                        asset: symbol.to_string(),
                    })
            }
            Asset::Token(token) => {
                let addr = format!("{token:#x}");
                // 1. Resolve the contract address to a CMC id.
                let info_url = format!("{}/v1/cryptocurrency/info?address={addr}", self.base);
                let info_raw = self.http.get(&info_url, &headers).await?;
                let info: serde_json::Value =
                    serde_json::from_str(&info_raw).map_err(|e| Error::Integrity {
                        message: format!("CMC info response not JSON: {e}"),
                    })?;
                let id = info
                    .pointer("/data")
                    .and_then(|d| d.as_object())
                    .and_then(|o| o.values().next())
                    .and_then(|entry| entry.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("id"))
                    .and_then(|i| i.as_u64())
                    .ok_or(Error::AbsentPrice { asset: addr })?;
                // 2. Quote it in USD.
                self.quote_by_id(id, &headers).await
            }
            Asset::Mint(mint) => Err(Error::Permanent {
                message: format!(
                    "CoinMarketCap cannot price Solana SPL mint {mint}; use CoinGecko"
                ),
            }),
            other => Err(Error::Permanent {
                message: format!("CoinMarketCap cannot price asset kind {other:?}"),
            }),
        }
    }
}

/// Read the `usd` price from the single entry of a CoinGecko token-price map, regardless of how
/// the queried address key is cased in the response.
fn single_entry_usd(json: &serde_json::Value) -> Option<Decimal> {
    json.as_object()?
        .values()
        .next()?
        .get("usd")
        .and_then(json_decimal)
}

/// Read a decimal from a JSON string or number literal (never via `f64`).
fn json_decimal(v: &serde_json::Value) -> Option<Decimal> {
    if let Some(s) = v.as_str() {
        Decimal::from_str(s).ok()
    } else if v.is_number() {
        Decimal::from_str(&v.to_string()).ok()
    } else {
        None
    }
}
