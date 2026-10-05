//! Contract-based tests for the Hyperliquid adapter, offline. The main reply is a real
//! `clearinghouseState` response for a live trader (0x8def…2dae, captured 2026-10-04): one open
//! short HYPE position with real entry/mark divergence, a non-null liquidation price, and funding.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Currency, Detail, PerpSide, Position, Protocol, ProtocolAdapter, Wallet,
};
use gluonscan_hyperliquid::Hyperliquid;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const RESPONSE: &str = r#"{
  "marginSummary":{"accountValue":"3161724.061735","totalNtlPos":"9440615.8529700004","totalRawUsd":"12602339.9147050008","totalMarginUsed":"3146871.9509899998"},
  "withdrawable":"14852.110745",
  "assetPositions":[
    {"type":"oneWay","position":{
      "coin":"HYPE","szi":"-104699.13","leverage":{"type":"cross","value":3},
      "entryPx":"64.7005","positionValue":"9440615.8529700004","unrealizedPnl":"-2666521.2567420001",
      "returnOnEquity":"-1.1809052349","liquidationPx":"120.0648965588","marginUsed":"3146871.9509899998",
      "maxLeverage":10,"cumFunding":{"allTime":"-178823.929272","sinceOpen":"-570146.735832","sinceChange":"-40257.879461"}}}
  ],
  "time":1791118611580}"#;

fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn read(response: &str) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    read_on(response, Chain::Hyperliquid).await
}

async fn read_on(
    response: &str,
    chain: Chain,
) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    let http = MockHttp::new().on(Match::body_contains("clearinghouseState"), response);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn maps_the_live_short_position() {
    let reading = read(RESPONSE).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Hyperliquid);
    // One open perp, plus the account's margin equity emitted once as a USDC wallet balance.
    assert_eq!(reading.positions.len(), 2);

    let p = reading
        .positions
        .iter()
        .find_map(|pos| match pos {
            Position::Perp(p) => Some(p),
            _ => None,
        })
        .expect("expected a perp position");

    assert_eq!(p.market, "HYPE");
    assert_eq!(p.side, PerpSide::Short); // szi is negative
    assert_eq!(p.size, dec("104699.13")); // |szi|
    assert_eq!(p.entry_price, Some(dec("64.7005")));
    assert_eq!(p.leverage, Some(dec("3")));
    assert_eq!(p.liquidation_price, Some(dec("120.0648965588")));

    // Mark derived as positionValue / |szi| ≈ 90.169 — pinned to a tight range so the derivation is
    // exercised without asserting every repeating digit of the division.
    let mark = p.mark_price.expect("mark");
    assert!(
        mark > dec("90.16") && mark < dec("90.17"),
        "derived mark ~90.169, got {mark}"
    );

    // PnL and funding are USD Money, verbatim from the API (signed).
    let pnl = p.unrealized_pnl.as_ref().expect("pnl");
    assert_eq!(pnl.currency, Currency::Usd);
    assert_eq!(pnl.amount, dec("-2666521.2567420001"));
    let funding = p.funding.as_ref().expect("funding");
    assert_eq!(funding.currency, Currency::Usd);
    assert_eq!(funding.amount, dec("-570146.735832"));

    // Collateral is the USDC margin with its USD value attached.
    assert_eq!(p.collateral.len(), 1);
    let col = &p.collateral[0];
    assert_eq!(col.token.symbol, "USDC");
    assert_eq!(col.amount, dec("3146871.9509899998"));
    assert_eq!(
        col.usd.as_ref().map(|m| m.amount),
        Some(dec("3146871.9509899998"))
    );

    // The account's margin equity (marginSummary.accountValue) is emitted once as a USDC balance —
    // the portfolio figure, independent of the margin backing any single position.
    let equity = reading
        .positions
        .iter()
        .find_map(|pos| match pos {
            Position::Wallet(w) => Some(&w.amount),
            _ => None,
        })
        .expect("expected the account equity balance");
    assert_eq!(equity.token.symbol, "USDC");
    assert_eq!(equity.amount, dec("3161724.061735"));
    assert_eq!(
        equity.usd.as_ref().map(|m| m.amount),
        Some(dec("3161724.061735"))
    );
}

#[tokio::test]
async fn cash_only_account_reports_equity_with_no_perp() {
    // A deposited account with no open positions still has portfolio value: the equity balance must
    // be emitted even though there is no perp, or the account would read as $0.
    let response = r#"{"marginSummary":{"accountValue":"1250.5"},"assetPositions":[],"time":1}"#;
    let reading = read(response).await.expect("read");
    assert_eq!(reading.positions.len(), 1);
    let Position::Wallet(w) = &reading.positions[0] else {
        panic!("expected the account equity balance");
    };
    assert_eq!(w.amount.token.symbol, "USDC");
    assert_eq!(w.amount.amount, dec("1250.5"));
    assert_eq!(w.amount.usd.as_ref().map(|m| m.amount), Some(dec("1250.5")));
}

#[tokio::test]
async fn missing_account_value_fails_closed() {
    // An open position with no marginSummary.accountValue would undercount net worth (equity lost);
    // the net-worth-bearing figure must fail closed, not be silently dropped.
    let response = r#"{"assetPositions":[{"type":"oneWay","position":{
      "coin":"ETH","szi":"1.0","leverage":{"type":"cross","value":5},"entryPx":"3000",
      "positionValue":"3100","unrealizedPnl":"100","marginUsed":"600","liquidationPx":"2000"}}],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a response missing marginSummary.accountValue must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn no_positions_is_a_successful_empty_reading() {
    let response = r#"{"marginSummary":{"accountValue":"0.0"},"assetPositions":[],"time":1}"#;
    let reading = read(response).await.expect("read");
    assert!(reading.positions.is_empty());
}

#[tokio::test]
async fn malformed_payload_fails_closed() {
    // A response without `assetPositions` (e.g. an error body) must fail closed, not read as empty.
    let response = r#"{"error":"something went wrong"}"#;
    let err = read(response)
        .await
        .expect_err("a malformed payload must fail closed");
    assert!(
        !err.is_retryable(),
        "a malformed payload is not a transient error"
    );
}

#[tokio::test]
async fn missing_required_field_fails_closed() {
    // A position missing `positionValue` (needed to derive the mark) must fail closed, never emit a
    // perp with a fabricated mark.
    let response = r#"{"assetPositions":[{"type":"oneWay","position":{
      "coin":"ETH","szi":"1.0","leverage":{"type":"cross","value":5},"entryPx":"3000",
      "unrealizedPnl":"10","marginUsed":"600"}}],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a position missing positionValue must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn funding_present_but_malformed_fails_closed() {
    // cumFunding.sinceOpen is present but not a parseable decimal string — a real USD figure in an
    // unexpected shape must fail closed, not be silently dropped to "no funding".
    let response = r#"{"assetPositions":[{"type":"oneWay","position":{
      "coin":"ETH","szi":"1.0","leverage":{"type":"cross","value":5},"entryPx":"3000",
      "positionValue":"3100","unrealizedPnl":"100","marginUsed":"600","liquidationPx":"2000",
      "cumFunding":{"sinceOpen":"not-a-number"}}}],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a present-but-malformed funding value must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn zero_size_fails_closed() {
    // A zero size would make the mark division degenerate; the guard must fail closed.
    let response = r#"{"assetPositions":[{"type":"oneWay","position":{
      "coin":"ETH","szi":"0","leverage":{"type":"cross","value":5},"entryPx":"3000",
      "positionValue":"0","unrealizedPnl":"0","marginUsed":"0"}}],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a zero-size position must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn unparseable_liquidation_price_fails_closed() {
    // A present-but-garbage liquidationPx fails closed (vs a null one, which is a legitimate None).
    let response = r#"{"assetPositions":[{"type":"oneWay","position":{
      "coin":"ETH","szi":"1.0","leverage":{"type":"cross","value":5},"entryPx":"3000",
      "positionValue":"3100","unrealizedPnl":"100","marginUsed":"600","liquidationPx":"soon"}}],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a present-but-unparseable liquidation price must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn transient_http_failure_is_retryable() {
    let http = MockHttp::new().on_transient(Match::body_contains("clearinghouseState"));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let err = Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("a transient HTTP failure must surface");
    assert!(
        err.is_retryable(),
        "a transport failure must stay retryable"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed() {
    let err = read_on(RESPONSE, Chain::Ethereum)
        .await
        .expect_err("Hyperliquid is only on its own chain");
    assert!(!err.is_retryable());
}
