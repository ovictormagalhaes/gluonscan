//! EVM idle ERC-20 balances via Alchemy's Data API, with full wallet hygiene applied and USD
//! attached from the bundled prices. Native coins are excluded (read by
//! [`EvmNativeBalance`](crate::EvmNativeBalance)).
//!
//! Unlike [`EvmWallet`](crate::EvmWallet) (a raw Moralis-shaped reader), [`AlchemyWallet`] is an
//! opinionated, batteries-included source: it drops disguised (Unicode-spoofed), implausible
//! (value > $1e10 or unit price > $1e12), unpriced, and sub-$1 dust leaves, and sanitizes the
//! surviving display strings — so a scam airdrop can neither inflate a total nor render a hidden
//! symbol. A non-hex balance from a succeeded response fails closed (`Integrity`).

use std::str::FromStr;

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use gluonscan_core::hygiene::{is_spoofed_token, sanitize_display};
use gluonscan_core::{
    Amount, Capability, Chain, Complete, Ctx, Detail, Error, Money, Position, Protocol,
    ProtocolAdapter, Provenance, Reading, Source, Staleness, Token, Wallet, WalletBalance,
};
use rust_decimal::Decimal;

/// Alchemy Data API base. The API key is carried in the URL PATH, not a header.
const ALCHEMY_DATA_API: &str = "https://api.g.alchemy.com";
const CAPABILITIES: &[Capability] = &[Capability::Positions];
/// The EVM chains the Data API covers (same set as the other EVM wallet readers).
const SUPPORTED_CHAINS: &[Chain] = &[
    Chain::Ethereum,
    Chain::Base,
    Chain::Arbitrum,
    Chain::Optimism,
    Chain::Polygon,
    Chain::Bnb,
];
/// Plausibility caps (a leaf whose value or unit price is orders of magnitude off is a scam /
/// mis-decimaled mint) and the dust floor. Policy defaults, matched to the reference app.
const MAX_WALLET_TOKEN_VALUE_USD: i64 = 10_000_000_000; // 1e10
const MAX_WALLET_TOKEN_PRICE_USD: i64 = 1_000_000_000_000; // 1e12
const WALLET_DUST_USD: i64 = 1;
/// Hard cap on pagination so a stuck `pageKey` cannot loop forever; exceeding it fails closed.
const ALCHEMY_MAX_PAGES: usize = 50;

/// Alchemy network slug for the chains the Data API covers.
fn alchemy_network(chain: Chain) -> Option<&'static str> {
    Some(match chain {
        Chain::Ethereum => "eth-mainnet",
        Chain::Base => "base-mainnet",
        Chain::Polygon => "matic-mainnet",
        Chain::Arbitrum => "arb-mainnet",
        Chain::Optimism => "opt-mainnet",
        Chain::Bnb => "bnb-mainnet",
        Chain::Solana => "solana-mainnet",
        _ => return None,
    })
}

/// Parse an Alchemy hex balance (`0x…`) to a [`U256`]. Empty-after-prefix is zero. A missing `0x`
/// prefix or non-hex body returns `None` (the caller fails closed).
fn parse_hex_balance(s: &str) -> Option<U256> {
    let body = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if body.is_empty() {
        return Some(U256::ZERO);
    }
    U256::from_str_radix(body, 16).ok()
}

/// The USD unit price from an entry's `tokenPrices` array (first `currency == "usd"`, case-insensitive).
fn alchemy_usd_price(entry: &serde_json::Value) -> Option<Decimal> {
    let prices = entry.get("tokenPrices").and_then(|p| p.as_array())?;
    for p in prices {
        let currency = p.get("currency").and_then(|c| c.as_str()).unwrap_or("");
        if currency.eq_ignore_ascii_case("usd") {
            let value = p.get("value").and_then(|v| v.as_str())?;
            return Decimal::from_str(value.trim()).ok();
        }
    }
    None
}

/// EVM idle ERC-20 balances via Alchemy's Data API (`/data/v1/{KEY}/assets/tokens/by-address`).
#[derive(Debug, Clone)]
pub struct AlchemyWallet {
    base: String,
    api_key: String,
}

impl AlchemyWallet {
    /// Construct with the Alchemy API key (carried in the request path).
    pub fn new(api_key: impl Into<String>) -> Self {
        AlchemyWallet {
            base: ALCHEMY_DATA_API.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Override the Data API base URL (e.g. a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    /// Fetch every token entry for `address` on `network`, following `pageKey` pagination.
    async fn fetch_entries(
        &self,
        cx: &Ctx,
        network: &str,
        address: &str,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let url = format!(
            "{}/data/v1/{}/assets/tokens/by-address",
            self.base.trim_end_matches('/'),
            self.api_key
        );
        let mut entries = Vec::new();
        let mut page_key: Option<String> = None;
        for _ in 0..ALCHEMY_MAX_PAGES {
            let mut body = serde_json::json!({
                "addresses": [{ "address": address, "networks": [network] }],
                "withMetadata": true,
                "withPrices": true,
                "includeNativeTokens": false,
                "includeErc20Tokens": true,
            });
            if let Some(pk) = &page_key {
                body["pageKey"] = serde_json::Value::String(pk.clone());
            }
            let raw = cx.http.post(&url, body.to_string(), &[]).await?;
            let json: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| Error::Integrity {
                    message: format!("Alchemy tokens response not JSON: {e}"),
                })?;
            let data = json.get("data").ok_or_else(|| Error::Integrity {
                message: "Alchemy tokens response missing data".into(),
            })?;
            if let Some(tokens) = data.get("tokens").and_then(|t| t.as_array()) {
                entries.extend(tokens.iter().cloned());
            }
            match data
                .get("pageKey")
                .and_then(|p| p.as_str())
                .filter(|s| !s.is_empty())
            {
                Some(pk) => page_key = Some(pk.to_string()),
                None => return Ok(entries),
            }
        }
        Err(Error::Transient {
            message: format!("Alchemy tokens exceeded {ALCHEMY_MAX_PAGES} pages for {address}"),
            retry_after: None,
        })
    }
}

/// One Alchemy token entry -> a clean [`Position::Wallet`], or `None` if a hygiene gate drops it.
/// `Err` only on data corruption (non-hex balance) so the caller fails closed.
fn entry_to_position(entry: &serde_json::Value) -> Result<Option<Position>, Error> {
    // An entry the API flagged with an error is dropped (not fatal).
    if entry
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return Ok(None);
    }
    // Native coins are read by EvmNativeBalance; a tokenAddress-less entry is skipped.
    let Some(token_address) = entry.get("tokenAddress").and_then(|a| a.as_str()) else {
        return Ok(None);
    };
    let meta = entry.get("tokenMetadata");
    let decimals = meta
        .and_then(|m| m.get("decimals"))
        .and_then(|d| d.as_u64())
        .unwrap_or(18) as u8;
    let symbol = meta
        .and_then(|m| m.get("symbol"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let name = meta.and_then(|m| m.get("name")).and_then(|s| s.as_str());

    let bal_hex = entry
        .get("tokenBalance")
        .and_then(|b| b.as_str())
        .unwrap_or("0x0");
    let raw = parse_hex_balance(bal_hex).ok_or_else(|| Error::Integrity {
        message: format!("Alchemy tokenBalance not hex: {bal_hex:?}"),
    })?;
    if raw == U256::ZERO {
        return Ok(None);
    }

    // Spoof: a disguise/control char in the display strings is a spoof signal — drop the leaf.
    let raw_symbol = if symbol.is_empty() { "UNKNOWN" } else { symbol };
    let raw_name = name.unwrap_or(raw_symbol);
    if is_spoofed_token(raw_name, raw_symbol) {
        return Ok(None);
    }
    let clean_symbol = sanitize_display(raw_symbol);
    let clean_name = sanitize_display(raw_name);
    let address = Address::from_str(token_address).ok();
    let token = Token::evm(clean_symbol, address, decimals).with_name(Some(clean_name));

    // An astronomical (scam) balance beyond Decimal's range drops the leaf, never the chain.
    let amount = match Amount::from_raw(token, raw) {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };

    // Price + value. Alchemy carries no verified flag, so every token is treated as unverified:
    // it must be priced and worth at least the dust floor to survive.
    let Some(price) = alchemy_usd_price(entry) else {
        return Ok(None);
    };
    if price <= Decimal::ZERO {
        return Ok(None);
    }
    let Some(value) = price.checked_mul(amount.amount) else {
        return Ok(None);
    };
    if value > Decimal::from(MAX_WALLET_TOKEN_VALUE_USD)
        || price > Decimal::from(MAX_WALLET_TOKEN_PRICE_USD)
    {
        return Ok(None);
    }
    if value < Decimal::from(WALLET_DUST_USD) {
        return Ok(None);
    }

    let amount = amount.with_usd(Some(Money::usd(value)));
    Ok(Some(Position::Wallet(
        WalletBalance::new(amount).with_verified_contract(Some(false)),
    )))
}

#[async_trait]
impl ProtocolAdapter for AlchemyWallet {
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
        let network = alchemy_network(chain).ok_or_else(|| Error::Permanent {
            message: format!("Alchemy wallet not configured for {chain:?}"),
        })?;
        let address = format!("{owner:#x}");
        let entries = self.fetch_entries(cx, network, &address).await?;
        let mut positions = Vec::new();
        for entry in &entries {
            if let Some(p) = entry_to_position(entry)? {
                positions.push(p);
            }
        }
        let reading = Reading::new(
            Protocol::Wallet,
            chain,
            Source::Api,
            positions,
            Provenance::new(Source::Api, chain, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
    }
}
