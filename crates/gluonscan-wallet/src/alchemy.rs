//! Wallet-token sources backed by Alchemy's Data API, with full wallet hygiene applied and USD
//! attached from the bundled prices. [`AlchemyWallet`] reads EVM ERC-20 balances; [`AlchemySolanaWallet`]
//! reads SPL balances (including native SOL). Both are opinionated, batteries-included sources: they
//! drop disguised (Unicode-spoofed), implausible (value > $1e10 or unit price > $1e12), unpriced, and
//! sub-$1 dust leaves, and sanitize the surviving display strings — so a scam airdrop can neither
//! inflate a total nor render a hidden symbol. A non-hex balance from a succeeded response fails
//! closed (`Integrity`); an astronomical balance beyond `Decimal` range drops that leaf, not the chain.
//!
//! Unlike [`EvmWallet`](crate::EvmWallet) / [`SolanaWallet`](crate::SolanaWallet) (raw readers that
//! leave prices `None` and apply no hygiene), these carry the Data API's bundled price. The Solana
//! source additionally does a per-mint secondary price lookup (`/prices/v1`) for mints the token
//! endpoint did not price — matching the reference app — and fails closed if a lookup errors.

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
/// Wrapped-SOL mint, used as the identity for a native SOL balance (which has no token address).
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const CAPABILITIES: &[Capability] = &[Capability::Positions];
/// The EVM chains the Data API covers (same set as the other EVM wallet readers).
const EVM_CHAINS: &[Chain] = &[
    Chain::Ethereum,
    Chain::Base,
    Chain::Arbitrum,
    Chain::Optimism,
    Chain::Polygon,
    Chain::Bnb,
];
const SOLANA_CHAINS: &[Chain] = &[Chain::Solana];
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

/// The USD unit price from a `tokenPrices`/`prices` array (first `currency == "usd"`, case-insensitive).
fn usd_price(prices: &serde_json::Value) -> Option<Decimal> {
    let arr = prices.as_array()?;
    for p in arr {
        let currency = p.get("currency").and_then(|c| c.as_str()).unwrap_or("");
        if currency.eq_ignore_ascii_case("usd") {
            let value = p.get("value").and_then(|v| v.as_str())?;
            return Decimal::from_str(value.trim()).ok();
        }
    }
    None
}

/// POST the Data API tokens-by-address endpoint and page through `pageKey`, returning every token
/// entry. `include_native` controls whether the chain's native coin is returned as an entry.
async fn fetch_token_entries(
    cx: &Ctx,
    url: &str,
    network: &str,
    address: &str,
    include_native: bool,
) -> Result<Vec<serde_json::Value>, Error> {
    let mut entries = Vec::new();
    let mut page_key: Option<String> = None;
    for _ in 0..ALCHEMY_MAX_PAGES {
        let mut body = serde_json::json!({
            "addresses": [{ "address": address, "networks": [network] }],
            "withMetadata": true,
            "withPrices": true,
            "includeNativeTokens": include_native,
            "includeErc20Tokens": true,
        });
        if let Some(pk) = &page_key {
            body["pageKey"] = serde_json::Value::String(pk.clone());
        }
        let raw = cx.http.post(url, body.to_string(), &[]).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
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

/// Whether `entry` was flagged with an error by the API (drop that leaf, not fatal).
fn entry_errored(entry: &serde_json::Value) -> bool {
    entry
        .get("error")
        .and_then(|e| e.as_str())
        .is_some_and(|s| !s.is_empty())
}

/// Apply the value/plausibility/dust gates to a priced amount and build the wallet position, or
/// `None` if a gate drops it. `price` must be `> 0`.
fn priced_position(
    token: Token,
    raw: U256,
    amount_human: Decimal,
    price: Decimal,
) -> Option<Position> {
    if price <= Decimal::ZERO {
        return None;
    }
    let value = price.checked_mul(amount_human)?;
    if value > Decimal::from(MAX_WALLET_TOKEN_VALUE_USD)
        || price > Decimal::from(MAX_WALLET_TOKEN_PRICE_USD)
        || value < Decimal::from(WALLET_DUST_USD)
    {
        return None;
    }
    let amount = Amount::from_raw(token, raw)
        .ok()?
        .with_usd(Some(Money::usd(value)));
    Some(Position::Wallet(
        WalletBalance::new(amount).with_verified_contract(Some(false)),
    ))
}

// ---- EVM ----------------------------------------------------------------------------------------

/// EVM idle ERC-20 balances via Alchemy's Data API. Native coins are excluded (read by
/// [`EvmNativeBalance`](crate::EvmNativeBalance)).
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
}

/// One EVM Alchemy token entry -> a clean [`Position::Wallet`], or `None` if a hygiene gate drops it.
/// `Err` only on data corruption (non-hex balance) so the caller fails closed.
fn evm_entry_to_position(entry: &serde_json::Value) -> Result<Option<Position>, Error> {
    if entry_errored(entry) {
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

    let raw_symbol = if symbol.is_empty() { "UNKNOWN" } else { symbol };
    let raw_name = name.unwrap_or(raw_symbol);
    if is_spoofed_token(raw_name, raw_symbol) {
        return Ok(None);
    }
    let token = Token::evm(
        sanitize_display(raw_symbol),
        Address::from_str(token_address).ok(),
        decimals,
    )
    .with_name(Some(sanitize_display(raw_name)));

    // An astronomical (scam) balance beyond Decimal's range drops the leaf, never the chain.
    let Ok(amount) = Amount::from_raw(token.clone(), raw) else {
        return Ok(None);
    };
    // Alchemy carries no verified flag, so every token is treated as unverified: it must be priced.
    let Some(price) = usd_price(entry.get("tokenPrices").unwrap_or(&serde_json::Value::Null))
    else {
        return Ok(None);
    };
    Ok(priced_position(token, raw, amount.amount, price))
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
        EVM_CHAINS
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
        let url = format!(
            "{}/data/v1/{}/assets/tokens/by-address",
            self.base.trim_end_matches('/'),
            self.api_key
        );
        let entries = fetch_token_entries(cx, &url, network, &format!("{owner:#x}"), false).await?;
        let mut positions = Vec::new();
        for entry in &entries {
            if let Some(p) = evm_entry_to_position(entry)? {
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

// ---- Solana -------------------------------------------------------------------------------------

/// A Solana wallet-token survivor of the pre-pricing pass, awaiting its price.
struct SolSurvivor {
    token: Token,
    raw: U256,
    amount: Decimal,
    mint: String,
    price: Option<Decimal>,
}

/// SPL balances (including native SOL) via Alchemy's Data API, with a per-mint secondary price
/// lookup for mints the token endpoint did not price.
#[derive(Debug, Clone)]
pub struct AlchemySolanaWallet {
    base: String,
    api_key: String,
}

impl AlchemySolanaWallet {
    /// Construct with the Alchemy API key (carried in the request path).
    pub fn new(api_key: impl Into<String>) -> Self {
        AlchemySolanaWallet {
            base: ALCHEMY_DATA_API.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Override the Data API base URL (e.g. a test double).
    pub fn with_base(mut self, url: impl Into<String>) -> Self {
        self.base = url.into();
        self
    }

    /// Secondary price lookup for a single mint (`/prices/v1/{KEY}/tokens/by-address`). `Ok(None)`
    /// means no USD price; `Err` fails closed so the sweep never persists a guessed-unpriced token.
    async fn fetch_price(&self, cx: &Ctx, mint: &str) -> Result<Option<Decimal>, Error> {
        let url = format!(
            "{}/prices/v1/{}/tokens/by-address",
            self.base.trim_end_matches('/'),
            self.api_key
        );
        let body = serde_json::json!({
            "addresses": [{ "network": "solana-mainnet", "address": mint }]
        });
        let raw = cx.http.post(&url, body.to_string(), &[]).await?;
        let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
            message: format!("Alchemy price response not JSON: {e}"),
        })?;
        let prices = json
            .pointer("/data/0/prices")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Ok(usd_price(&prices))
    }
}

#[async_trait]
impl ProtocolAdapter for AlchemySolanaWallet {
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
        SOLANA_CHAINS
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
                message: format!("Alchemy Solana wallet only; got {chain:?}"),
            });
        }
        let owner = owner.solana()?;
        let url = format!(
            "{}/data/v1/{}/assets/tokens/by-address",
            self.base.trim_end_matches('/'),
            self.api_key
        );
        // Native SOL stays in the token list (includeNativeTokens: true).
        let entries = fetch_token_entries(cx, &url, "solana-mainnet", owner, true).await?;

        // Pass 1: parse, drop spoofed / zero / out-of-range before any price lookup.
        let mut survivors: Vec<SolSurvivor> = Vec::new();
        for entry in &entries {
            if entry_errored(entry) {
                continue;
            }
            let addr = entry.get("tokenAddress").and_then(|a| a.as_str());
            let is_native = addr.is_none();
            let mint = addr.unwrap_or(SOL_MINT).to_string();
            let meta = entry.get("tokenMetadata");
            let decimals = match meta
                .and_then(|m| m.get("decimals"))
                .and_then(|d| d.as_u64())
            {
                Some(d) => d as u8,
                None if is_native => 9,
                None => {
                    return Err(Error::Integrity {
                        message: format!("Alchemy SPL entry {mint} missing decimals"),
                    })
                }
            };
            let symbol = meta
                .and_then(|m| m.get("symbol"))
                .and_then(|s| s.as_str())
                .unwrap_or(if is_native { "SOL" } else { "" });
            let name = meta.and_then(|m| m.get("name")).and_then(|s| s.as_str());

            let bal_hex = entry
                .get("tokenBalance")
                .and_then(|b| b.as_str())
                .unwrap_or("0x0");
            let raw = parse_hex_balance(bal_hex).ok_or_else(|| Error::Integrity {
                message: format!("Alchemy tokenBalance not hex: {bal_hex:?}"),
            })?;
            if raw == U256::ZERO {
                continue;
            }

            let raw_symbol = if symbol.is_empty() { "UNKNOWN" } else { symbol };
            let raw_name = name.unwrap_or(raw_symbol);
            if is_spoofed_token(raw_name, raw_symbol) {
                continue;
            }
            let token = Token::solana(sanitize_display(raw_symbol), Some(mint.clone()), decimals)
                .with_name(Some(sanitize_display(raw_name)));
            let Ok(amount) = Amount::from_raw(token.clone(), raw) else {
                continue; // astronomical (scam) balance beyond Decimal range: drop leaf
            };
            let price = usd_price(entry.get("tokenPrices").unwrap_or(&serde_json::Value::Null));
            survivors.push(SolSurvivor {
                token,
                raw,
                amount: amount.amount,
                mint,
                price,
            });
        }

        // Secondary per-mint price lookup for survivors the token endpoint did not price. Sequential
        // (a bounded-concurrency variant is a perf follow-up); any lookup error fails the read closed.
        for s in survivors.iter_mut() {
            if s.price.is_none() {
                s.price = self.fetch_price(cx, &s.mint).await?;
            }
        }

        // Pass 2: apply price, plausibility and dust; an unpriced survivor is dropped.
        let mut positions = Vec::new();
        for s in survivors {
            let Some(price) = s.price else { continue };
            if let Some(p) = priced_position(s.token, s.raw, s.amount, price) {
                positions.push(p);
            }
        }

        let reading = Reading::new(
            Protocol::Wallet,
            Chain::Solana,
            Source::Api,
            positions,
            Provenance::new(Source::Api, Chain::Solana, cx.clock.now(), Staleness::Live),
        );
        Ok(Complete::new(reading))
    }
}
