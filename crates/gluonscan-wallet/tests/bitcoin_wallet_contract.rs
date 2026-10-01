//! Contract-based test: native BTC balance via a mempool.space-style explorer (MockHttp).

use std::sync::Arc;

use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::BitcoinWallet;
use rust_decimal::Decimal;

const ADDRESS: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

// funded 1.5 BTC, spent 0.5 BTC -> 1.0 BTC (1e8 sats).
const ADDR_STATS: &str = r#"{"address":"bc1q","chain_stats":{"funded_txo_sum":150000000,"spent_txo_sum":50000000},"mempool_stats":{}}"#;

#[tokio::test]
async fn reads_native_btc_balance() {
    let http = MockHttp::new().on(Match::primary_contains("/address/"), ADDR_STATS);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    let reading = BitcoinWallet::new()
        .read(
            &Wallet::Bitcoin(ADDRESS.to_string()),
            Chain::Bitcoin,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Native);
    assert_eq!(reading.positions.len(), 1);
    let Position::Wallet(btc) = &reading.positions[0] else {
        panic!("expected a wallet balance");
    };
    assert_eq!(btc.amount.token.symbol, "BTC");
    assert_eq!(btc.amount.token.decimals, 8);
    assert_eq!(btc.amount.amount, Decimal::from_str_exact("1").unwrap()); // 1e8 sats
    assert!(btc.amount.usd.is_none());
}

#[tokio::test]
async fn rejects_non_bitcoin_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = BitcoinWallet::new()
        .read(
            &Wallet::Solana("So1111".into()),
            Chain::Bitcoin,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("a Bitcoin wallet is required");
    assert!(!err.is_retryable());
}
