//! # gluonscan-sources
//!
//! Concrete [`PriceSource`] implementations. Each takes an injected [`Http`] client, so pricing is
//! testable offline against recorded contracts and shares the host's transport/limits.
//!
//! A missing price is [`Error::AbsentPrice`] — never a fabricated `0` or `1`.

use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::Address;
use async_trait::async_trait;
use gluonscan_core::{Chain, Error, Http, PriceSource};
use rust_decimal::Decimal;

const COINGECKO_API: &str = "https://api.coingecko.com";

/// CoinGecko price source: prices an EVM token by its contract address on a chain.
pub struct CoinGecko {
    http: Arc<dyn Http>,
    base: String,
}

impl CoinGecko {
    /// Construct with the default public API and an injected HTTP client.
    pub fn new(http: Arc<dyn Http>) -> Self {
        CoinGecko {
            http,
            base: COINGECKO_API.to_string(),
        }
    }

    /// Override the API base (a pro/proxy endpoint or a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    /// The CoinGecko asset-platform id for a chain.
    fn platform(chain: Chain) -> Option<&'static str> {
        Some(match chain {
            Chain::Ethereum => "ethereum",
            Chain::Base => "base",
            Chain::Polygon => "polygon-pos",
            Chain::Arbitrum => "arbitrum-one",
            Chain::Optimism => "optimistic-ethereum",
            Chain::Bnb => "binance-smart-chain",
            _ => return None,
        })
    }
}

#[async_trait]
impl PriceSource for CoinGecko {
    async fn price_usd(&self, chain: Chain, token: Address) -> Result<Decimal, Error> {
        let platform = CoinGecko::platform(chain).ok_or_else(|| Error::Permanent {
            message: format!("CoinGecko has no asset platform for {chain:?}"),
        })?;
        let addr = format!("{token:#x}");
        let url = format!(
            "{}/api/v3/simple/token_price/{platform}?contract_addresses={addr}&vs_currencies=usd",
            self.base
        );
        let raw = self.http.get(&url).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("CoinGecko response not JSON: {e}"),
        })?;
        json.pointer(&format!("/{addr}/usd"))
            .and_then(json_decimal)
            .ok_or(Error::AbsentPrice { asset: addr })
    }
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
