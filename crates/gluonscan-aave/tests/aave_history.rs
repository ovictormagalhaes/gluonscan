//! Contract test: Aave lending-event history from a mocked subgraph.

use std::collections::HashMap;
use std::sync::Arc;

use gluonscan_aave::AaveApi;
use gluonscan_core::{Address, Chain, Ctx, EventKind, ProtocolAdapter, TokenAddress, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const EVENTS: &str = r#"{"data":{
  "supplies":[{"id":"0xabc:1","timestamp":"1700000000","amount":"1500000","txHash":"0xdead","reserve":{"symbol":"USDC","name":"USD Coin","underlyingAsset":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","decimals":6}}],
  "redeemUnderlyings":[],
  "borrows":[{"id":"0xdef:1","timestamp":"1700000100","amount":"2000000000000000000","txHash":"0xbeef","reserve":{"symbol":"WETH","name":"Wrapped Ether","underlyingAsset":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","decimals":18}}],
  "repays":[]
}}"#;

#[tokio::test]
async fn reads_lending_events_from_subgraph() {
    let http = MockHttp::new().on(Match::body_contains("redeemUnderlyings"), EVENTS);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let mut subs = HashMap::new();
    subs.insert(
        Chain::Ethereum,
        "https://graph.example/aave-eth".to_string(),
    );

    let history = AaveApi::new()
        .with_subgraphs(subs)
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            None,
            None,
            &cx,
        )
        .await
        .expect("history")
        .into_inner();

    assert_eq!(history.events.len(), 2);
    // Sorted oldest-first: the supply (ts 1700000000) precedes the borrow (ts 1700000100).
    let supply = &history.events[0];
    assert_eq!(supply.kind, EventKind::Deposit);
    assert_eq!(supply.amount.token.symbol, "USDC");
    assert_eq!(
        supply.amount.amount,
        Decimal::from_str_exact("1.5").unwrap()
    );
    assert_eq!(supply.tx, "0xdead");
    assert!(matches!(
        supply.amount.token.address,
        Some(TokenAddress::Evm(_))
    ));

    let borrow = &history.events[1];
    assert_eq!(borrow.kind, EventKind::Borrow);
    assert_eq!(borrow.amount.token.symbol, "WETH");
    assert_eq!(borrow.amount.amount, Decimal::from_str_exact("2").unwrap());
    assert_eq!(borrow.tx, "0xbeef");
}

#[tokio::test]
async fn no_subgraph_configured_returns_empty_history() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let history = AaveApi::new()
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            None,
            None,
            &cx,
        )
        .await
        .expect("empty history")
        .into_inner();
    assert!(history.events.is_empty());
}
