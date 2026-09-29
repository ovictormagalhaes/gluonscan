//! Contract-based test: EVM token holdings over a mock HTTP transport (header-authenticated).

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_holdings::EvmTokenHoldings;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const ERC20: &str = r#"[
  {"token_address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":6,"balance":"1500000000"},
  {"token_address":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":18,"balance":"2000000000000000000"}
]"#;

#[tokio::test]
async fn reads_idle_token_balances() {
    let http = MockHttp::new().on(Match::primary_contains("/erc20"), ERC20);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    let reading = EvmTokenHoldings::new("test-key")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Wallet);
    assert_eq!(reading.positions.len(), 2);

    let Position::Wallet(usdc) = &reading.positions[0] else {
        panic!("expected a holding");
    };
    assert_eq!(usdc.amount.token.symbol, "USDC");
    assert_eq!(usdc.amount.amount, Decimal::from_str_exact("1500").unwrap());
    assert!(usdc.amount.usd.is_none()); // pricing is a separate operation

    let Position::Wallet(weth) = &reading.positions[1] else {
        panic!("expected a holding");
    };
    assert_eq!(weth.amount.amount, Decimal::from_str_exact("2").unwrap());
}

#[tokio::test]
async fn rejects_non_evm_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = EvmTokenHoldings::new("k")
        .read(
            &Wallet::Bitcoin("bc1qxyz".into()),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("an EVM wallet is required");
    assert!(!err.is_retryable());
}
