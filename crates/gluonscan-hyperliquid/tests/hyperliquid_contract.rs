//! Contract-based tests for the Hyperliquid adapter, offline. The main reply is a real
//! `clearinghouseState` response for a live trader (0x8def…2dae, captured 2026-10-04): one open
//! short HYPE position with real entry/mark divergence, a non-null liquidation price, and funding.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Currency, Detail, MarginMode, PerpSide, Position, Protocol,
    ProtocolAdapter, Wallet,
};
use gluonscan_hyperliquid::Hyperliquid;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

// A live-shaped BTC long, 5x isolated, with a small positive PnL — the base for the TP/SL and
// margin-mode tests. positionValue is self-consistent with size (0.0006 * 83943 ≈ 50.3658).
const BTC_LONG_ISOLATED: &str = r#"{
  "marginSummary":{"accountValue":"9.93"},
  "assetPositions":[
    {"type":"oneWay","position":{
      "coin":"BTC","szi":"0.0006","leverage":{"type":"isolated","value":5},
      "entryPx":"83852.0","positionValue":"50.3658","unrealizedPnl":"0.05",
      "liquidationPx":"67966.51","marginUsed":"9.93"}}
  ],"time":1}"#;

// The position's own TP (Take Profit Market, 92500) and SL (Stop Market, 82000), exactly as
// Hyperliquid's `frontendOpenOrders` returns them — both flagged `isPositionTpsl`.
const BTC_TPSL_ORDERS: &str = r#"[
  {"coin":"BTC","isPositionTpsl":true,"isTrigger":true,"triggerPx":"92500.0",
   "orderType":"Take Profit Market","side":"A","reduceOnly":true,"sz":"0.0006"},
  {"coin":"BTC","isPositionTpsl":true,"isTrigger":true,"triggerPx":"82000.0",
   "orderType":"Stop Market","side":"A","reduceOnly":true,"sz":"0.0006"}
]"#;

async fn read_full(
    clearinghouse: &str,
    orders: Option<&str>,
) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    let mut http = MockHttp::new().on(Match::body_contains("clearinghouseState"), clearinghouse);
    if let Some(o) = orders {
        http = http.on(Match::body_contains("frontendOpenOrders"), o);
    }
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

fn only_perp(reading: &gluonscan_core::Reading) -> &gluonscan_core::PerpPosition {
    reading
        .positions
        .iter()
        .find_map(|pos| match pos {
            Position::Perp(p) => Some(p),
            _ => None,
        })
        .expect("expected a perp position")
}

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
    // leverage.type is "cross" in this fixture — the margin mode rides in verbatim.
    assert_eq!(p.margin_mode, Some(MarginMode::Cross));

    // Mark derived as positionValue / |szi| ≈ 90.169 — pinned to a tight range so the derivation is
    // exercised without asserting every repeating digit of the division.
    let mark = p.mark_price.expect("mark");
    assert!(
        mark > dec("90.16") && mark < dec("90.17"),
        "derived mark ~90.169, got {mark}"
    );

    // PnL is USD Money, verbatim from the API (signed). Funding is negated: Hyperliquid reports
    // cumFunding from the exchange's side (positive = paid by the account), while the model reads
    // negative = paid. This short received funding, so the model value is positive.
    let pnl = p.unrealized_pnl.as_ref().expect("pnl");
    assert_eq!(pnl.currency, Currency::Usd);
    assert_eq!(pnl.amount, dec("-2666521.2567420001"));
    let funding = p.funding.as_ref().expect("funding");
    assert_eq!(funding.currency, Currency::Usd);
    assert_eq!(funding.amount, dec("570146.735832"));

    // Collateral is the USDC margin (requirement) carried as a bare amount for detail. Its USD value
    // is intentionally unset: the margin is already encompassed by the account equity below, so
    // pricing it here would double-count the same capital.
    assert_eq!(p.collateral.len(), 1);
    let col = &p.collateral[0];
    assert_eq!(col.token.symbol, "USDC");
    assert_eq!(col.amount, dec("3146871.9509899998"));
    assert_eq!(col.usd, None);

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
async fn negative_account_value_fails_closed() {
    // A negative (underwater/bad-debt) equity is a present net-worth figure, not a balance. Silently
    // dropping it would overcount the account back up; it must fail closed.
    let response = r#"{"marginSummary":{"accountValue":"-12.5"},"assetPositions":[],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a negative accountValue must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn garbled_account_value_fails_closed() {
    // A present-but-unparseable accountValue is net-worth-bearing and must fail closed, not be
    // silently treated as absent/zero.
    let response =
        r#"{"marginSummary":{"accountValue":"not-a-number"},"assetPositions":[],"time":1}"#;
    let err = read(response)
        .await
        .expect_err("a garbled accountValue must fail closed");
    assert!(!err.is_retryable());
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
async fn funding_paid_by_account_maps_negative() {
    // Live shape (2026-10-09, ONDO long, positive funding rate): cumFunding.sinceOpen = +33.703202
    // while the account's userFunding `usdc` deltas sum to -33.703202. The long PAID funding, so
    // the model value (negative = paid) must be -33.703202.
    let response = r#"{"marginSummary":{"accountValue":"3380"},"assetPositions":[{"type":"oneWay","position":{
      "coin":"ONDO","szi":"10318.0","leverage":{"type":"cross","value":3},"entryPx":"0.9",
      "positionValue":"9500","unrealizedPnl":"214","marginUsed":"3166","liquidationPx":null,
      "cumFunding":{"allTime":"33.703202","sinceOpen":"33.703202","sinceChange":"2.515933"}}}],"time":1}"#;
    let reading = read(response).await.expect("read");
    let p = only_perp(&reading);
    assert_eq!(
        p.funding.as_ref().expect("funding").amount,
        dec("-33.703202")
    );
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

#[tokio::test]
async fn attaches_position_take_profit_and_stop_loss() {
    // At Detail::Full the position's own TP/SL trigger prices are attached from frontendOpenOrders.
    let reading = read_full(BTC_LONG_ISOLATED, Some(BTC_TPSL_ORDERS))
        .await
        .expect("read");
    let p = only_perp(&reading);
    assert_eq!(p.margin_mode, Some(MarginMode::Isolated));
    assert_eq!(p.take_profit_price, Some(dec("92500.0")));
    assert_eq!(p.stop_loss_price, Some(dec("82000.0")));
}

#[tokio::test]
async fn standalone_trigger_orders_do_not_attach_as_position_tpsl() {
    // A trigger order NOT flagged isPositionTpsl (a user's standalone stop) must never be read as
    // the position's TP/SL — otherwise an unrelated order would paint a false exit line.
    let orders = r#"[
      {"coin":"BTC","isPositionTpsl":false,"isTrigger":true,"triggerPx":"99999.0",
       "orderType":"Stop Market","side":"A","reduceOnly":false,"sz":"0.0006"}
    ]"#;
    let reading = read_full(BTC_LONG_ISOLATED, Some(orders))
        .await
        .expect("read");
    let p = only_perp(&reading);
    assert!(p.take_profit_price.is_none());
    assert!(p.stop_loss_price.is_none());
}

#[tokio::test]
async fn tpsl_fetch_failure_preserves_equity_and_leaves_levels_none() {
    // frontendOpenOrders is down (transient), clearinghouseState is fine. TP/SL are advisory, not
    // net-worth-bearing, so the read must still succeed with correct equity and the levels as None —
    // failing the whole account over a secondary detail would be strictly worse.
    let http = MockHttp::new()
        .on(
            Match::body_contains("clearinghouseState"),
            BTC_LONG_ISOLATED,
        )
        .on_transient(Match::body_contains("frontendOpenOrders"));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let reading = Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            Detail::Full,
            &cx,
        )
        .await
        .expect("a TP/SL fetch failure must not fail the account read")
        .into_inner();
    let p = only_perp(&reading);
    assert!(p.take_profit_price.is_none());
    assert!(p.stop_loss_price.is_none());
    // Equity is still emitted and correct.
    let equity = reading
        .positions
        .iter()
        .find_map(|pos| match pos {
            Position::Wallet(w) => Some(&w.amount),
            _ => None,
        })
        .expect("equity balance");
    assert_eq!(equity.usd.as_ref().map(|m| m.amount), Some(dec("9.93")));
}

#[tokio::test]
async fn malformed_trigger_price_is_skipped_not_fatal() {
    // A position-TP/SL order with a present-but-unparseable triggerPx contributes nothing (the level
    // stays None) rather than failing the read — an advisory line is never fabricated or fatal.
    let orders = r#"[
      {"coin":"BTC","isPositionTpsl":true,"isTrigger":true,"triggerPx":"soon",
       "orderType":"Take Profit Market","side":"A","reduceOnly":true,"sz":"0.0006"}
    ]"#;
    let reading = read_full(BTC_LONG_ISOLATED, Some(orders))
        .await
        .expect("read");
    let p = only_perp(&reading);
    assert!(p.take_profit_price.is_none());
    assert!(p.stop_loss_price.is_none());
}

#[tokio::test]
async fn tpsl_is_not_fetched_below_full_detail() {
    // At Summary the extra frontendOpenOrders call must not be made: TP/SL is Full-only detail.
    let http = MockHttp::new().on(
        Match::body_contains("clearinghouseState"),
        BTC_LONG_ISOLATED,
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let reading = Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            Detail::Summary,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    let p = only_perp(&reading);
    // Margin mode still rides in (same payload), but no trigger-order call was made.
    assert_eq!(p.margin_mode, Some(MarginMode::Isolated));
    assert!(p.take_profit_price.is_none());
    assert!(p.stop_loss_price.is_none());
}

#[tokio::test]
async fn unknown_margin_mode_maps_to_none() {
    // A margin mode Hyperliquid may add later is advisory, not net-worth-bearing: map an unrecognized
    // leverage.type to None rather than failing the equity read on a label.
    let response = r#"{"marginSummary":{"accountValue":"100.0"},"assetPositions":[
      {"type":"oneWay","position":{
        "coin":"ETH","szi":"1.0","leverage":{"type":"portfolio","value":5},"entryPx":"3000",
        "positionValue":"3100","unrealizedPnl":"100","marginUsed":"600","liquidationPx":"2000"}}
    ],"time":1}"#;
    let reading = read_full(response, None).await.expect("read");
    let p = only_perp(&reading);
    assert!(p.margin_mode.is_none());
}
