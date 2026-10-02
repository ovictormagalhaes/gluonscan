//! Contract test for the Alchemy Solana wallet source (MockHttp): native SOL is kept, a mint the
//! token endpoint did not price is priced via the secondary `/prices/v1` lookup, and dust is dropped.

use std::sync::Arc;

use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::AlchemySolanaWallet;
use rust_decimal::Decimal;

const WALLET: &str = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";

// Native SOL (priced inline), a USDC leaf (priced inline), and an unpriced mint that the secondary
// price lookup will value at $0 -> dropped as dust.
const TOKENS: &str = r#"{"data":{"tokens":[
  {"tokenBalance":"0x77359400","tokenMetadata":{"symbol":"SOL","name":"Solana","decimals":9},"tokenPrices":[{"currency":"usd","value":"200.0"}]},
  {"tokenAddress":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","tokenBalance":"0x3b9aca00","tokenMetadata":{"symbol":"USDC","name":"USD Coin","decimals":6},"tokenPrices":[{"currency":"usd","value":"1.0"}]},
  {"tokenAddress":"ScamMint111111111111111111111111111111111111","tokenBalance":"0x0de0b6b3a7640000","tokenMetadata":{"symbol":"SCAM","name":"Scam","decimals":9},"tokenPrices":[]}
]}}"#;

// Secondary price lookup returns no USD price for the scam mint.
const NO_PRICE: &str = r#"{"data":[{"prices":[]}]}"#;

#[tokio::test]
async fn keeps_sol_and_usdc_prices_scam_via_lookup_and_drops_it() {
    let http = MockHttp::new()
        .on(Match::primary_contains("/assets/tokens/by-address"), TOKENS)
        .on(Match::primary_contains("/prices/v1/"), NO_PRICE);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

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

    assert_eq!(reading.protocol, Protocol::Wallet);
    // SOL (2 * $200 = $400) and USDC (1000 * $1 = $1000) survive; the unpriced scam mint is dropped.
    assert_eq!(reading.positions.len(), 2);
    let symbols: Vec<&str> = reading
        .positions
        .iter()
        .map(|p| {
            let Position::Wallet(b) = p else {
                panic!("wallet")
            };
            b.amount.token.symbol.as_str()
        })
        .collect();
    assert!(symbols.contains(&"SOL"));
    assert!(symbols.contains(&"USDC"));

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
