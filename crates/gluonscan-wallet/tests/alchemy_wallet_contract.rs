//! Contract test for the Alchemy Data API wallet source (MockHttp), covering the hygiene gates:
//! a clean token survives priced; spoofed and implausible leaves are dropped; dust and unpriced
//! leaves are reported flagged so a consumer that held them before can keep them.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet, WalletBalance,
    WalletGate,
};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::AlchemyWallet;
use rust_decimal::Decimal;

// One good USDC leaf ($1,500), two leaves that must be dropped (a right-to-left-override spoofed
// symbol, an implausible unit price > $1e12) and two that are kept flagged (a dust leaf < $1, an
// unpriced leaf).
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

fn balance<'a>(positions: &'a [Position], symbol: &str) -> Option<&'a WalletBalance> {
    positions.iter().find_map(|p| match p {
        Position::Wallet(b) if b.amount.token.symbol == symbol => Some(b),
        _ => None,
    })
}

#[tokio::test]
async fn reads_clean_tokens_flags_gated_leaves_and_drops_bad_ones() {
    // Arrange: inject the real right-to-left override (U+202E) via escape; a raw codepoint in
    // source is denied.
    let response = RESPONSE.replace("<RLO>", "\u{202e}");
    let http = MockHttp::new().on(
        Match::primary_contains("/assets/tokens/by-address"),
        response,
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    // Act
    let reading = AlchemyWallet::new("test-key")
        .with_base("https://alchemy.test")
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect("read")
        .into_inner();

    // Assert
    assert_eq!(reading.protocol, Protocol::Wallet);
    assert_eq!(reading.positions.len(), 3, "USDC, DUST and NOPRICE");
    let usdc = balance(&reading.positions, "USDC").expect("clean leaf");
    assert_eq!(usdc.gated, None);
    // 0x59682f00 = 1_500_000_000 raw / 10^6 = 1500 USDC, at $1.0 = $1500.
    assert_eq!(usdc.amount.amount, Decimal::from_str_exact("1500").unwrap());
    let usd = usdc.amount.usd.as_ref().expect("priced");
    assert_eq!(usd.amount, Decimal::from_str_exact("1500").unwrap());

    let dust = balance(&reading.positions, "DUST").expect("dust kept, flagged");
    assert_eq!(dust.gated, Some(WalletGate::BelowDust));
    assert_eq!(
        dust.amount.usd.as_ref().map(|m| m.amount),
        Some(Decimal::from_str_exact("0.0001").unwrap())
    );

    let unpriced = balance(&reading.positions, "NOPRICE").expect("unpriced kept, flagged");
    assert_eq!(unpriced.gated, Some(WalletGate::Unpriced));
    assert!(
        unpriced.amount.usd.is_none(),
        "an unpriced leaf is never valued"
    );

    assert!(balance(&reading.positions, "SCAM").is_none());
    assert!(reading
        .positions
        .iter()
        .all(|p| matches!(p, Position::Wallet(b) if !b.amount.token.symbol.contains('\u{202e}'))));
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
