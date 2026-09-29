//! Contract-based test: Solana SPL token balances over the injected RPC (MockChainProvider).

use std::sync::Arc;

use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_wallet::SolanaWallet;
use rust_decimal::Decimal;

const WALLET: &str = "So11111111111111111111111111111111111111112";

const ACCOUNTS: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"value":[
  {"account":{"data":{"parsed":{"info":{"mint":"MintAAA","tokenAmount":{"amount":"5000000000","decimals":9}}}}}},
  {"account":{"data":{"parsed":{"info":{"mint":"MintBBB","tokenAmount":{"amount":"0","decimals":6}}}}}}
]}}"#;

#[tokio::test]
async fn reads_spl_balances_and_skips_zero() {
    let rpc = MockChainProvider::new().on(Match::method("getTokenAccountsByOwner"), ACCOUNTS);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = SolanaWallet::new()
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
    assert_eq!(reading.positions.len(), 1); // the zero-balance account is skipped

    let Position::Wallet(b) = &reading.positions[0] else {
        panic!("expected a wallet balance");
    };
    assert_eq!(b.amount.token.decimals, 9);
    assert_eq!(b.amount.amount, Decimal::from_str_exact("5").unwrap()); // 5e9 / 1e9
    assert!(b.amount.usd.is_none());
}

#[tokio::test]
async fn rejects_non_solana_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = SolanaWallet::new()
        .read(
            &Wallet::Bitcoin("bc1qxyz".into()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("a Solana wallet is required");
    assert!(!err.is_retryable());
}
