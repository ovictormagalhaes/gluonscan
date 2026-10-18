//! Contract test: Aave lending-event history from a mocked subgraph.

use std::collections::HashMap;
use std::sync::Arc;

use gluonscan_aave::{AaveApi, HISTORY_PAGE_SIZE};
use gluonscan_core::{
    Address, Chain, Ctx, Error, EventKind, History, ProtocolAdapter, TokenAddress, Wallet,
};
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

const SUBGRAPH: &str = "https://graph.example/aave-eth";

fn event(id: &str, ts: i64, symbol: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "timestamp": ts.to_string(),
        "amount": "1000000",
        "txHash": format!("0x{id}"),
        "reserve": {"symbol": symbol, "name": symbol, "underlyingAsset": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "decimals": 6}
    })
}

/// Supply `i` (zero-padded so id order is supply order), one second apart.
fn supply(i: usize) -> serde_json::Value {
    event(&format!("s:{i:05}"), 1_700_000_000 + i as i64, "USDC")
}

/// One full page of supplies (ids `s:00000` .. `s:00999`) plus a repay newer than every supply.
fn first_page() -> String {
    let supplies: Vec<_> = (0..HISTORY_PAGE_SIZE).map(supply).collect();
    serde_json::json!({"data": {
        "supplies": supplies,
        "redeemUnderlyings": [],
        "borrows": [],
        "repays": [event("r:1", 1_800_000_000, "USDC")],
    }})
    .to_string()
}

/// The request for the page after the first full supplies page: only supplies are still open.
fn second_page_request() -> Match {
    Match::body_contains(format!(r#"id_gt:\"s:{:05}\""#, HISTORY_PAGE_SIZE - 1))
}

async fn read(http: MockHttp) -> (Arc<MockHttp>, Result<History, Error>) {
    let http = Arc::new(http);
    let cx = Ctx::new(http.clone(), Arc::new(MockClock(0)));
    let mut subs = HashMap::new();
    subs.insert(Chain::Ethereum, SUBGRAPH.to_string());
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
        .map(|h| h.into_inner());
    (http, history)
}

#[tokio::test]
async fn pages_each_kind_until_a_short_page() {
    // A full page of supplies and one newer repay: a consumer checkpointing on the repay would lose
    // every supply past the first page if it were not read now.
    let second = serde_json::json!({"data": {"supplies": [supply(HISTORY_PAGE_SIZE)]}}).to_string();
    let (http, history) = read(
        MockHttp::new()
            .on(second_page_request(), second)
            .on(Match::body_contains("repays"), first_page()),
    )
    .await;
    let history = history.expect("every page is read");

    assert_eq!(history.events.len(), HISTORY_PAGE_SIZE + 2);
    assert!(history.events.windows(2).all(|w| w[0].at <= w[1].at));
    let (repay, supplies) = history.events.split_last().unwrap();
    assert_eq!(repay.kind, EventKind::Repay);
    assert!(supplies.iter().all(|e| e.kind == EventKind::Deposit));
    assert_eq!(
        supplies.last().unwrap().tx,
        format!("0xs:{:05}", HISTORY_PAGE_SIZE),
        "the supply on the second page is returned, before the newer repay"
    );

    // Two requests; the second asks only for the kind that filled its page.
    let calls = http.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.contains(r#"id_gt:\"\""#));
    assert!(calls[0].1.contains(&format!("first:{HISTORY_PAGE_SIZE}")));
    let second_query = &calls[1].1;
    assert!(second_query.contains("supplies("));
    for closed in ["repays(", "borrows(", "redeemUnderlyings("] {
        assert!(!second_query.contains(closed), "{closed} was exhausted");
    }
}

#[tokio::test]
async fn a_failed_page_fails_the_history() {
    // Transport failure on the second page: retryable, never the first page alone.
    let (_, history) = read(
        MockHttp::new()
            .on_transient(second_page_request())
            .on(Match::body_contains("repays"), first_page()),
    )
    .await;
    let err = history.expect_err("a failed page fails the read");
    assert!(err.is_retryable(), "got {err:?}");

    // GraphQL errors on the second page.
    let (_, history) = read(
        MockHttp::new()
            .on(
                second_page_request(),
                r#"{"data":null,"errors":[{"message":"indexing error"}]}"#,
            )
            .on(Match::body_contains("repays"), first_page()),
    )
    .await;
    let err = history.expect_err("a page with errors fails the read");
    assert!(matches!(err, Error::Integrity { .. }), "got {err:?}");

    // A second page that does not carry the requested kind.
    let (_, history) = read(
        MockHttp::new()
            .on(second_page_request(), r#"{"data":{}}"#)
            .on(Match::body_contains("repays"), first_page()),
    )
    .await;
    let err = history.expect_err("a page without its kind fails the read");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("supplies")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_requested_kind_missing_from_the_response_fails_closed() {
    let mut no_repays: serde_json::Value = serde_json::from_str(EVENTS).unwrap();
    no_repays["data"].as_object_mut().unwrap().remove("repays");
    let no_repays = no_repays.to_string();
    let (_, history) = read(MockHttp::new().on(Match::body_contains("repays"), no_repays)).await;
    let err = history.expect_err("an absent kind is not an empty one");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("repays")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_page_that_repeats_events_fails_instead_of_looping() {
    // A subgraph ignoring the cursor serves the same full page forever.
    let (http, history) =
        read(MockHttp::new().on(Match::body_contains("supplies"), first_page())).await;
    let err = history.expect_err("a cursor that does not advance fails the read");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("repeated")),
        "got {err:?}"
    );
    assert_eq!(http.calls().len(), 2);
}
