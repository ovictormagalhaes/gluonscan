//! Contract-based tests: native coin balances (EVM `eth_getBalance`, Solana `getBalance`).

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_wallet::{EvmNativeBalance, SolanaNativeBalance};
use rust_decimal::Decimal;

// 1.5 ETH in wei = 0x14d1120d7b160000.
const ETH_BALANCE: &str = r#"{"jsonrpc":"2.0","id":1,"result":"0x14d1120d7b160000"}"#;
// 2 SOL in lamports = 2_000_000_000.
const SOL_BALANCE: &str =
    r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":2000000000}}"#;

#[tokio::test]
async fn reads_evm_native_balance() {
    let rpc = MockChainProvider::new().on(Match::method("eth_getBalance"), ETH_BALANCE);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = EvmNativeBalance::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Wallet);
    assert_eq!(reading.positions.len(), 1);
    let Position::Wallet(b) = &reading.positions[0] else {
        panic!("expected a wallet balance");
    };
    assert_eq!(b.amount.token.symbol, "ETH");
    assert_eq!(b.amount.token.decimals, 18);
    assert!(b.amount.token.address.is_none());
    assert_eq!(b.amount.amount, Decimal::from_str_exact("1.5").unwrap());
    assert!(b.amount.usd.is_none());
}

#[tokio::test]
async fn reads_solana_native_balance() {
    let rpc = MockChainProvider::new().on(Match::method("getBalance"), SOL_BALANCE);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = SolanaNativeBalance::new()
        .read(
            &Wallet::Solana("So1111".into()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    let Position::Wallet(b) = &reading.positions[0] else {
        panic!("expected a wallet balance");
    };
    assert_eq!(b.amount.token.symbol, "SOL");
    assert_eq!(b.amount.token.decimals, 9);
    assert_eq!(b.amount.amount, Decimal::from_str_exact("2").unwrap());
}

#[tokio::test]
async fn evm_native_rejects_unmapped_chain() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = EvmNativeBalance::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Monad, Detail::Full, &cx)
        .await
        .expect_err("Monad has no native symbol mapped");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn evm_native_rejects_non_evm_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = EvmNativeBalance::new()
        .read(
            &Wallet::Bitcoin("bc1q".into()),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("an EVM wallet is required");
    assert!(!err.is_retryable());
}
