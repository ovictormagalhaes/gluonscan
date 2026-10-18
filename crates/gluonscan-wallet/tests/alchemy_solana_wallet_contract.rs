//! Contract test for the Alchemy Solana wallet source (MockHttp): native SOL is kept, a mint the
//! token endpoint did not price goes to the secondary `/prices/v1` lookup, and a mint that stays
//! unpriced is reported flagged (no USD), never valued and never silently dropped.

use std::sync::Arc;

use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet, WalletGate};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::AlchemySolanaWallet;
use rust_decimal::Decimal;

const WALLET: &str = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";

// Native SOL (priced inline), a USDC leaf (priced inline), and a mint the secondary price lookup
// cannot price either.
const TOKENS: &str = r#"{"data":{"tokens":[
  {"tokenBalance":"0x77359400","tokenMetadata":{"symbol":"SOL","name":"Solana","decimals":9},"tokenPrices":[{"currency":"usd","value":"200.0"}]},
  {"tokenAddress":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","tokenBalance":"0x3b9aca00","tokenMetadata":{"symbol":"USDC","name":"USD Coin","decimals":6},"tokenPrices":[{"currency":"usd","value":"1.0"}]},
  {"tokenAddress":"ScamMint111111111111111111111111111111111111","tokenBalance":"0x0de0b6b3a7640000","tokenMetadata":{"symbol":"SCAM","name":"Scam","decimals":9},"tokenPrices":[]}
]}}"#;

// Secondary price lookup returns no USD price for the scam mint.
const NO_PRICE: &str = r#"{"data":[{"prices":[]}]}"#;

#[tokio::test]
async fn keeps_priced_leaves_and_flags_the_mint_no_lookup_can_price() {
    // Arrange
    let http = MockHttp::new()
        .on(Match::primary_contains("/assets/tokens/by-address"), TOKENS)
        .on(Match::primary_contains("/prices/v1/"), NO_PRICE);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    // Act
    let reading = AlchemySolanaWallet::new("key")
        .with_base("https://alchemy.test")
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    // Assert: SOL ($400) and USDC ($1,000) are ungated; SCAM is reported unpriced, with no USD.
    assert_eq!(reading.protocol, Protocol::Wallet);
    assert_eq!(reading.positions.len(), 3);
    let leaves: Vec<(&str, Option<WalletGate>, bool)> = reading
        .positions
        .iter()
        .map(|p| {
            let Position::Wallet(b) = p else {
                panic!("wallet")
            };
            (
                b.amount.token.symbol.as_str(),
                b.gated,
                b.amount.usd.is_some(),
            )
        })
        .collect();
    assert!(leaves.contains(&("SOL", None, true)));
    assert!(leaves.contains(&("USDC", None, true)));
    assert!(leaves.contains(&("SCAM", Some(WalletGate::Unpriced), false)));

    for p in &reading.positions {
        let Position::Wallet(b) = p else {
            panic!("wallet")
        };
        if b.amount.token.symbol == "SOL" {
            // 0x77359400 = 2_000_000_000 lamports / 1e9 = 2 SOL.
            assert_eq!(b.amount.amount, Decimal::from_str_exact("2").unwrap());
            assert_eq!(
                b.amount.usd.as_ref().unwrap().amount,
                Decimal::from_str_exact("400").unwrap()
            );
        }
    }
}

#[tokio::test]
async fn rejects_non_solana_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = AlchemySolanaWallet::new("k")
        .with_base("https://alchemy.test")
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Base,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("solana only");
    assert!(!err.is_retryable());
}
