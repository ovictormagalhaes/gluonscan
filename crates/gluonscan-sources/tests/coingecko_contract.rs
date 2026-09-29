//! Contract-based test: CoinGecko / CoinMarketCap pricing over a mock HTTP transport, offline.

use std::sync::Arc;

use gluonscan_core::{Address, Asset, Chain, Error, PriceSource};
use gluonscan_sources::{CoinGecko, CoinMarketCap};
use gluonscan_testing::{Match, MockHttp};
use rust_decimal::Decimal;

const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
const USDC_SOL_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

#[tokio::test]
async fn coinmarketcap_resolves_address_then_prices() {
    let addr: Address = WETH.parse().unwrap();
    let key = format!("{addr:#x}");
    let info = format!(r#"{{"data":{{"{key}":[{{"id":1027}}]}}}}"#);
    let quote = r#"{"data":{"1027":{"quote":{"USD":{"price":1500.5}}}}}"#;

    let http = MockHttp::new()
        .on(Match::primary_contains("cryptocurrency/info"), info)
        .on(Match::primary_contains("quotes/latest"), quote);
    let price = CoinMarketCap::new(Arc::new(http), "test-key")
        .price_usd(Chain::Ethereum, Asset::Token(addr))
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("1500.5").unwrap());
}

#[tokio::test]
async fn coinmarketcap_unknown_token_is_absent() {
    let http = MockHttp::new().on(
        Match::primary_contains("cryptocurrency/info"),
        r#"{"data":{}}"#,
    );
    let err = CoinMarketCap::new(Arc::new(http), "test-key")
        .price_usd(Chain::Ethereum, Asset::Token(Address::ZERO))
        .await
        .expect_err("unknown token must error");
    assert!(matches!(err, Error::AbsentPrice { .. }));
}

#[tokio::test]
async fn coinmarketcap_prices_native_by_symbol() {
    let quote = r#"{"data":{"BTC":[{"quote":{"USD":{"price":65000.0}}}]}}"#;
    let http = MockHttp::new().on(Match::primary_contains("quotes/latest"), quote);
    let price = CoinMarketCap::new(Arc::new(http), "test-key")
        .price_usd(Chain::Bitcoin, Asset::Native)
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("65000").unwrap());
}

#[tokio::test]
async fn coinmarketcap_cannot_price_spl_mint() {
    let err = CoinMarketCap::new(Arc::new(MockHttp::new()), "test-key")
        .price_usd(Chain::Solana, Asset::Mint(USDC_SOL_MINT.to_string()))
        .await
        .expect_err("CMC cannot resolve SPL mints");
    assert!(!err.is_retryable()); // Permanent, not a fabricated price
}

#[tokio::test]
async fn coingecko_prices_a_token_by_contract() {
    let addr: Address = WETH.parse().unwrap();
    let key = format!("{addr:#x}");
    let body = format!(r#"{{"{key}":{{"usd":1500.5}}}}"#);

    let http = MockHttp::new().on(Match::primary_contains("token_price"), body);
    let price = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Ethereum, Asset::Token(addr))
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("1500.5").unwrap());
}

#[tokio::test]
async fn coingecko_prices_native_by_coin_id() {
    let http = MockHttp::new().on(
        Match::primary_contains("simple/price?ids"),
        r#"{"bitcoin":{"usd":65000}}"#,
    );
    let price = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Bitcoin, Asset::Native)
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("65000").unwrap());
}

#[tokio::test]
async fn coingecko_prices_spl_mint_case_preserved() {
    // CoinGecko preserves the case-sensitive base58 mint in the response key; the reader must
    // read the single entry rather than lowercasing the key back.
    let body = format!(r#"{{"{USDC_SOL_MINT}":{{"usd":1.0}}}}"#);
    let http = MockHttp::new().on(Match::primary_contains("token_price"), body);
    let price = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Solana, Asset::Mint(USDC_SOL_MINT.to_string()))
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("1").unwrap());
}

#[tokio::test]
async fn missing_price_is_absent_not_zero() {
    let http = MockHttp::new().on(Match::primary_contains("token_price"), "{}");
    let err = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Ethereum, Asset::Token(Address::ZERO))
        .await
        .expect_err("missing price must error");
    assert!(matches!(err, Error::AbsentPrice { .. }));
}

#[tokio::test]
async fn token_price_on_a_native_only_chain_errors() {
    // Bitcoin has no token platform — an EVM-token key there is a permanent configuration error.
    let err = CoinGecko::new(Arc::new(MockHttp::new()))
        .price_usd(Chain::Bitcoin, Asset::Token(Address::ZERO))
        .await
        .expect_err("no token platform for Bitcoin");
    assert!(!err.is_retryable());
}
