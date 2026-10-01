//! Contract test for the Alchemy Data API wallet source (MockHttp), covering the hygiene gates:
//! a clean token survives priced; spoofed / implausible / dust / unpriced leaves are dropped.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::AlchemyWallet;
use rust_decimal::Decimal;

// One good USDC leaf ($1,500), plus leaves that must each be dropped:
// - a right-to-left-override spoofed symbol,
// - a dust leaf (< $1),
// - an unpriced leaf (no usd price),
// - an implausible leaf (unit price > $1e12).
const RESPONSE: &str = r#"{
  "data": {
    "tokens": [
      {
        "tokenAddress": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "tokenBalance": "0x59682f00",
        "tokenMetadata": { "symbol": "USDC", "name": "USD Coin", "decimals": 6 },
        "tokenPrices": [ { "currency": "usd", "value": "1.0" } ]
      },
      {
        "tokenAddress": "0x1111111111111111111111111111111111111111",
        "tokenBalance": "0x0de0b6b3a7640000",
        "tokenMetadata": { "symbol": "US<RLO>DC", "name": "Spoofed", "decimals": 18 },
        "tokenPrices": [ { "currency": "usd", "value": "5.0" } ]
      },
      {
        "tokenAddress": "0x2222222222222222222222222222222222222222",
        "tokenBalance": "0x0de0b6b3a7640000",
        "tokenMetadata": { "symbol": "DUST", "name": "Dust", "decimals": 18 },
        "tokenPrices": [ { "currency": "usd", "value": "0.0001" } ]
      },
      {
        "tokenAddress": "0x3333333333333333333333333333333333333333",
        "tokenBalance": "0x0de0b6b3a7640000",
        "tokenMetadata": { "symbol": "NOPRICE", "name": "No Price", "decimals": 18 },
        "tokenPrices": []
      },
      {
        "tokenAddress": "0x4444444444444444444444444444444444444444",
        "tokenBalance": "0x0de0b6b3a7640000",
        "tokenMetadata": { "symbol": "SCAM", "name": "Scam", "decimals": 18 },
        "tokenPrices": [ { "currency": "usd", "value": "9999999999999" } ]
      }
    ]
  }
}"#;

#[tokio::test]
async fn reads_clean_tokens_and_drops_bad_leaves() {
    // Inject the real right-to-left override (U+202E) via escape; a raw codepoint in source is denied.
    let response = RESPONSE.replace("<RLO>", "\u{202e}");
    let http = MockHttp::new().on(Match::primary_contains("/assets/tokens/by-address"), response);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    let reading = AlchemyWallet::new("test-key")
        .with_base("https://alchemy.test")
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Wallet);
    assert_eq!(reading.positions.len(), 1, "only the clean USDC leaf survives");
    let Position::Wallet(b) = &reading.positions[0] else {
        panic!("expected a wallet balance");
    };
    assert_eq!(b.amount.token.symbol, "USDC");
    // 0x59682f00 = 1_500_000_000 raw / 10^6 = 1500 USDC.
    assert_eq!(b.amount.amount, Decimal::from_str_exact("1500").unwrap());
    // $1.0 * 1500 = $1500 attached from the bundled price.
    let usd = b.amount.usd.as_ref().expect("priced");
    assert_eq!(usd.amount, Decimal::from_str_exact("1500").unwrap());
}

#[tokio::test]
async fn non_hex_balance_fails_closed() {
    let bad = r#"{"data":{"tokens":[{"tokenAddress":"0xabc","tokenBalance":"not-hex","tokenMetadata":{"symbol":"X","name":"X","decimals":18},"tokenPrices":[{"currency":"usd","value":"1"}]}]}}"#;
    let http = MockHttp::new().on(Match::primary_contains("/assets/tokens/"), bad);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let err = AlchemyWallet::new("k")
        .with_base("https://alchemy.test")
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("non-hex balance is corruption");
    assert!(!err.is_retryable());
}
