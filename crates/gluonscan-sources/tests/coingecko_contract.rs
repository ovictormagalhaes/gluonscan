//! Contract-based test: CoinGecko pricing over a mock HTTP transport, offline.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Error, PriceSource};
use gluonscan_sources::CoinGecko;
use gluonscan_testing::{Match, MockHttp};
use rust_decimal::Decimal;

#[tokio::test]
async fn prices_a_token_by_contract() {
    let addr: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .unwrap();
    let key = format!("{addr:#x}");
    let body = format!(r#"{{"{key}":{{"usd":1500.5}}}}"#);

    let http = MockHttp::new().on(Match::primary_contains("token_price"), body);
    let price = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Ethereum, addr)
        .await
        .expect("price");
    assert_eq!(price, Decimal::from_str_exact("1500.5").unwrap());
}

#[tokio::test]
async fn missing_price_is_absent_not_zero() {
    let http = MockHttp::new().on(Match::primary_contains("token_price"), "{}");
    let err = CoinGecko::new(Arc::new(http))
        .price_usd(Chain::Ethereum, Address::ZERO)
        .await
        .expect_err("missing price must error");
    assert!(matches!(err, Error::AbsentPrice { .. }));
}

#[tokio::test]
async fn unsupported_chain_errors() {
    let err = CoinGecko::new(Arc::new(MockHttp::new()))
        .price_usd(Chain::Solana, Address::ZERO)
        .await
        .expect_err("CoinGecko has no platform for Solana");
    assert!(!err.is_retryable());
}
