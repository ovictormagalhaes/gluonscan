//! Offline tests for the account balances by account mode, the spot tokens, each position's opening
//! date, and the fill history. The unified-account replies are live-shaped from a real account
//! (0xf699…2b27, captured 2026-11-20): one isolated BTC short, with the spot USDC `hold` equal to the
//! perp `accountValue` and the spot `total` holding both the margin and the free USDC.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use gluonscan_core::{
    Address, Amount, Chain, Ctx, Detail, Error, EventKind, History, Http, PerpSide, Position,
    ProtocolAdapter, Reading, Timestamp, Wallet,
};
use gluonscan_hyperliquid::Hyperliquid;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const USER: &str = "0x0000000000000000000000000000000000000000";

const BTC_SHORT: &str = r#"{
  "marginSummary":{"accountValue":"32.89368","totalNtlPos":"59.39976","totalRawUsd":"92.29344","totalMarginUsed":"32.89368"},
  "assetPositions":[{"type":"oneWay","position":{
    "coin":"BTC","szi":"-0.00098","leverage":{"type":"isolated","value":2,"rawUsd":"92.29344"},
    "entryPx":"62796.0","positionValue":"59.39976","unrealizedPnl":"2.14116","liquidationPx":"93014.3008314437",
    "marginUsed":"32.89368","cumFunding":{"allTime":"0.009244","sinceOpen":"-0.011013","sinceChange":"-0.011013"}}}],
  "time":1795187769723}"#;

const SPOT_UNIFIED: &str = r#"{"balances":[
  {"coin":"USDC","token":0,"total":"53.983371","hold":"32.89368","entryNtl":"0.0"}]}"#;

const FLAT_PERPS: &str = r#"{"marginSummary":{"accountValue":"0.0"},"assetPositions":[],"time":1}"#;

// Two USDC pairs whose contexts are deliberately out of the universe's order, plus a context with no
// pair: prices must be joined by name, never by position.
const SPOT_MARKET: &str = r#"[
  {"tokens":[
     {"name":"USDC","index":0,"weiDecimals":8,"szDecimals":8},
     {"name":"HYPE","index":150,"weiDecimals":8,"szDecimals":2},
     {"name":"MAX","index":734,"weiDecimals":6,"szDecimals":0},
     {"name":"LONE","index":900,"weiDecimals":5,"szDecimals":0}],
   "universe":[
     {"name":"@107","tokens":[150,0],"index":107},
     {"name":"@591","tokens":[734,0],"index":591},
     {"name":"@999","tokens":[900,235],"index":999}]},
  [{"coin":"@591","markPx":"0.5"},{"coin":"@3","markPx":"9.0"},{"coin":"@107","markPx":"61.774"}]
]"#;

fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}

fn http(clearinghouse: &str, spot: &str, mode: &str) -> MockHttp {
    MockHttp::new()
        .on(
            Match::body_contains("\"clearinghouseState\""),
            clearinghouse,
        )
        .on(Match::body_contains("spotClearinghouseState"), spot)
        .on(Match::body_contains("userAbstraction"), mode)
        .on(Match::body_contains("spotMetaAndAssetCtxs"), SPOT_MARKET)
}

fn defaults(http: MockHttp) -> MockHttp {
    http.on(Match::body_contains("frontendOpenOrders"), "[]")
        .on(Match::body_contains("\"userFills\""), "[]")
}

async fn read_with(http: MockHttp, detail: Detail) -> Result<Reading, Error> {
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(&Wallet::Evm(Address::ZERO), Chain::Hyperliquid, detail, &cx)
        .await
        .map(|c| c.into_inner())
}

async fn read(clearinghouse: &str, spot: &str, mode: &str) -> Result<Reading, Error> {
    read_with(defaults(http(clearinghouse, spot, mode)), Detail::Full).await
}

fn balances(reading: &Reading) -> Vec<&Amount> {
    reading
        .positions
        .iter()
        .filter_map(|p| match p {
            Position::Wallet(w) => Some(&w.amount),
            _ => None,
        })
        .collect()
}

fn balance<'a>(reading: &'a Reading, symbol: &str) -> &'a Amount {
    balances(reading)
        .into_iter()
        .find(|a| a.token.symbol == symbol)
        .unwrap_or_else(|| panic!("expected a {symbol} balance"))
}

fn usd(a: &Amount) -> Option<Decimal> {
    a.usd.as_ref().map(|m| m.amount)
}

#[tokio::test]
async fn unified_account_counts_the_spot_usdc_once() {
    // The spot total already holds the perp margin and its PnL; adding accountValue would count the
    // 32.89 twice (86.87 instead of 53.98).
    let reading = read(BTC_SHORT, SPOT_UNIFIED, r#""unifiedAccount""#)
        .await
        .expect("read");
    let usdc = balances(&reading);
    assert_eq!(usdc.len(), 1, "USDC is emitted once");
    assert_eq!(usdc[0].token.symbol, "USDC");
    assert_eq!(usdc[0].amount, dec("53.983371"));
    assert_eq!(usd(usdc[0]), Some(dec("53.983371")));
    assert!(reading
        .positions
        .iter()
        .any(|p| matches!(p, Position::Perp(p) if p.market == "BTC")));
}

#[tokio::test]
async fn unified_account_without_a_position_still_reports_its_usdc() {
    let spot =
        r#"{"balances":[{"coin":"USDC","token":0,"total":"21.09","hold":"0.0","entryNtl":"0.0"}]}"#;
    let reading = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
        .await
        .expect("read");
    assert_eq!(reading.positions.len(), 1);
    assert_eq!(usd(balance(&reading, "USDC")), Some(dec("21.09")));
}

#[tokio::test]
async fn separate_account_sums_spot_and_perp_usdc() {
    for mode in [r#""default""#, r#""disabled""#, r#""dexAbstraction""#] {
        let perps = r#"{"marginSummary":{"accountValue":"100.5"},"assetPositions":[],"time":1}"#;
        let spot = r#"{"balances":[{"coin":"USDC","token":0,"total":"50.25","hold":"0.0"}]}"#;
        let reading = read(perps, spot, mode).await.expect("read");
        let usdc = balances(&reading);
        assert_eq!(usdc.len(), 1, "{mode}: USDC is emitted once");
        assert_eq!(usd(usdc[0]), Some(dec("150.75")), "{mode}");
    }
}

#[tokio::test]
async fn other_spot_tokens_are_priced_at_their_usdc_mark() {
    let spot = r#"{"balances":[
      {"coin":"USDC","token":0,"total":"1.0","hold":"0.0"},
      {"coin":"HYPE","token":150,"total":"2.5","hold":"0.0"},
      {"coin":"MAX","token":734,"total":"10","hold":"0.0"}]}"#;
    let reading = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
        .await
        .expect("read");
    let hype = balance(&reading, "HYPE");
    assert_eq!(hype.amount, dec("2.5"));
    assert_eq!(hype.token.decimals, 8);
    assert_eq!(usd(hype), Some(dec("154.435")));
    let max = balance(&reading, "MAX");
    assert_eq!(max.token.decimals, 6);
    assert_eq!(usd(max), Some(dec("5.0")));
}

#[tokio::test]
async fn a_spot_token_without_a_usdc_pair_is_left_unpriced() {
    // LONE trades only against another quote, and an unlisted balance has no token index: both are
    // reported with no USD value rather than a guessed one.
    let spot = r#"{"balances":[
      {"coin":"LONE","token":900,"total":"3","hold":"0.0"},
      {"coin":"+1230","total":"4","hold":"0.0"}]}"#;
    let reading = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
        .await
        .expect("read");
    assert_eq!(usd(balance(&reading, "LONE")), None);
    assert_eq!(balance(&reading, "LONE").token.decimals, 5);
    assert_eq!(usd(balance(&reading, "+1230")), None);
    assert_eq!(balance(&reading, "+1230").amount, dec("4"));
    assert_eq!(balance(&reading, "+1230").token.decimals, 18);
}

#[tokio::test]
async fn a_listed_token_missing_from_the_metadata_is_unpriced_at_full_precision() {
    let spot = r#"{"balances":[{"coin":"NEW","token":555,"total":"1.5"}]}"#;
    let reading = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
        .await
        .expect("read");
    assert_eq!(usd(balance(&reading, "NEW")), None);
    assert_eq!(balance(&reading, "NEW").token.decimals, 18);
}

async fn read_holding_hype(market: &str) -> Result<Reading, Error> {
    let http = defaults(
        MockHttp::new()
            .on(Match::body_contains("\"clearinghouseState\""), FLAT_PERPS)
            .on(
                Match::body_contains("spotClearinghouseState"),
                r#"{"balances":[{"coin":"HYPE","token":150,"total":"2"}]}"#,
            )
            .on(
                Match::body_contains("userAbstraction"),
                r#""unifiedAccount""#,
            )
            .on(
                Match::body_contains("spotMetaAndAssetCtxs"),
                market.to_string(),
            ),
    );
    read_with(http, Detail::Full).await
}

const HYPE_META: &str = r#"{"tokens":[{"name":"HYPE","index":150,"weiDecimals":8},{"name":"MAX","index":734,"weiDecimals":6}],
  "universe":[{"name":"@107","tokens":[150,0],"index":107},{"name":"@591","tokens":[734,0],"index":591}]}"#;

#[tokio::test]
async fn a_held_token_with_no_market_or_a_zero_mark_is_unpriced() {
    for ctxs in ["[]", r#"[{"coin":"@107","markPx":"0.0"}]"#] {
        let reading = read_holding_hype(&format!("[{HYPE_META},{ctxs}]"))
            .await
            .expect("read");
        assert_eq!(usd(balance(&reading, "HYPE")), None, "{ctxs}");
    }
}

#[tokio::test]
async fn a_broken_mark_of_a_token_not_held_does_not_fail_the_read() {
    let ctxs = r#"[{"coin":"@107","markPx":"60.0"},{"coin":"@591","markPx":null}]"#;
    let reading = read_holding_hype(&format!("[{HYPE_META},{ctxs}]"))
        .await
        .expect("MAX is not held");
    assert_eq!(usd(balance(&reading, "HYPE")), Some(dec("120.0")));
}

#[tokio::test]
async fn a_malformed_spot_market_or_held_mark_fails_closed() {
    let bad_meta = [
        "{}".to_string(),
        r#"[{"tokens":[]},[]]"#.to_string(),
        r#"[{"tokens":[{"name":"HYPE"}],"universe":[]},[]]"#.to_string(),
        format!(r#"[{HYPE_META},{{}}]"#),
        format!(r#"[{HYPE_META},[{{"coin":"@107","markPx":"-1"}}]]"#),
    ];
    for market in bad_meta {
        let err = read_holding_hype(&market)
            .await
            .expect_err("a price that cannot be read is not guessed");
        assert!(!err.is_retryable(), "{market}");
    }
}

#[tokio::test]
async fn usdc_only_accounts_do_not_fetch_the_spot_market() {
    let http = defaults(
        MockHttp::new()
            .on(Match::body_contains("\"clearinghouseState\""), BTC_SHORT)
            .on(Match::body_contains("spotClearinghouseState"), SPOT_UNIFIED)
            .on(
                Match::body_contains("userAbstraction"),
                r#""unifiedAccount""#,
            ),
    );
    read_with(http, Detail::Full)
        .await
        .expect("no spotMetaAndAssetCtxs call is needed for USDC alone");
}

#[tokio::test]
async fn an_empty_account_skips_the_mode_lookup() {
    let http = MockHttp::new()
        .on(Match::body_contains("\"clearinghouseState\""), FLAT_PERPS)
        .on(
            Match::body_contains("spotClearinghouseState"),
            r#"{"balances":[]}"#,
        );
    let reading = read_with(http, Detail::Full)
        .await
        .expect("an empty account reads as empty without further calls");
    assert!(reading.positions.is_empty());
}

#[tokio::test]
async fn portfolio_margin_and_unknown_modes_fail_closed() {
    let err = read(BTC_SHORT, SPOT_UNIFIED, r#""portfolioMargin""#)
        .await
        .expect_err("portfolio margin is not modeled");
    assert!(matches!(err, Error::Permanent { .. }), "{err:?}");
    for mode in [r#""somethingNew""#, "{}"] {
        let err = read(BTC_SHORT, SPOT_UNIFIED, mode)
            .await
            .expect_err("an unknown account mode must not be guessed");
        assert!(matches!(err, Error::Integrity { .. }), "{mode}: {err:?}");
    }
}

// Answers `spotClearinghouseState` from a queue, one reply per call, and everything else from
// `rest`: a balance that changes between two reads of the same read.
struct ChangingSpot {
    spot: Mutex<VecDeque<&'static str>>,
    rest: MockHttp,
}

#[async_trait]
impl Http for ChangingSpot {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<String, Error> {
        if body.contains("spotClearinghouseState") {
            return Ok(self
                .spot
                .lock()
                .unwrap()
                .pop_front()
                .expect("a spot reply")
                .to_string());
        }
        self.rest.post(url, body, headers).await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, Error> {
        self.rest.get(url, headers).await
    }
}

#[tokio::test]
async fn usdc_moving_between_the_reads_of_a_separate_account_is_retried() {
    // 50 USDC moved from perps to spot between the spot and the perp reads: the perp equity no
    // longer holds it and the first spot read did not yet. Summing would drop it.
    let perps = r#"{"marginSummary":{"accountValue":"100"},"assetPositions":[],"time":1}"#;
    let before = r#"{"balances":[{"coin":"USDC","token":0,"total":"10"}]}"#;
    let after = r#"{"balances":[{"coin":"USDC","token":0,"total":"60"}]}"#;
    let http = ChangingSpot {
        spot: Mutex::new(VecDeque::from([before, after])),
        rest: MockHttp::new()
            .on(Match::body_contains("\"clearinghouseState\""), perps)
            .on(Match::body_contains("userAbstraction"), r#""default""#),
    };
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
        .expect_err("a sum across a transfer is not one balance");
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn a_unified_account_reads_spot_once() {
    let http = Arc::new(defaults(http(
        BTC_SHORT,
        SPOT_UNIFIED,
        r#""unifiedAccount""#,
    )));
    let cx = Ctx::new(http.clone(), Arc::new(MockClock(0)));
    let _reading = Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    let spot_reads = http
        .calls()
        .iter()
        .filter(|(_, body)| body.contains("spotClearinghouseState"))
        .count();
    assert_eq!(spot_reads, 1, "one balance, one read");
}

#[tokio::test]
async fn a_null_token_index_is_an_unlisted_balance() {
    let spot = r#"{"balances":[{"coin":"+7","token":null,"total":"2"}]}"#;
    let reading = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
        .await
        .expect("read");
    assert_eq!(usd(balance(&reading, "+7")), None);
}

#[tokio::test]
async fn malformed_spot_balances_fail_closed() {
    for spot in [
        r#"{"error":"boom"}"#,
        r#"{"balances":[{"coin":"HYPE","token":"abc","total":"1"}]}"#,
        r#"{"balances":[{"coin":"USDC","token":0,"total":"-1"}]}"#,
        r#"{"balances":[{"coin":"USDC","token":0,"total":"lots"}]}"#,
        r#"{"balances":[{"token":0,"total":"1"}]}"#,
    ] {
        let err = read(FLAT_PERPS, spot, r#""unifiedAccount""#)
            .await
            .expect_err("a net-worth-bearing balance must not be guessed");
        assert!(!err.is_retryable(), "{spot}");
    }
}

#[tokio::test]
async fn a_malformed_spot_mark_fails_closed() {
    let market = r#"[{"tokens":[{"name":"HYPE","index":150,"weiDecimals":8}],
      "universe":[{"name":"@107","tokens":[150,0],"index":107}]},
      [{"coin":"@107","markPx":"n/a"}]]"#;
    let http = defaults(
        MockHttp::new()
            .on(Match::body_contains("\"clearinghouseState\""), FLAT_PERPS)
            .on(
                Match::body_contains("spotClearinghouseState"),
                r#"{"balances":[{"coin":"HYPE","token":150,"total":"1"}]}"#,
            )
            .on(
                Match::body_contains("userAbstraction"),
                r#""unifiedAccount""#,
            )
            .on(Match::body_contains("spotMetaAndAssetCtxs"), market),
    );
    let err = read_with(http, Detail::Full)
        .await
        .expect_err("a present but unparseable price must not be guessed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_spot_read_failure_fails_the_read_retryably() {
    let http = MockHttp::new()
        .on(Match::body_contains("\"clearinghouseState\""), BTC_SHORT)
        .on_transient(Match::body_contains("spotClearinghouseState"));
    let err = read_with(http, Detail::Full)
        .await
        .expect_err("the spot balance is net-worth-bearing");
    assert!(err.is_retryable());
}

// The account's recent fills, newest first as `userFills` returns them: the current short opened on
// 2026-11-19, after an earlier long that was opened and closed.
const RECENT_FILLS: &str = r#"[
  {"coin":"BTC","px":"62796.0","sz":"0.00098","side":"A","time":1795536506394,"startPosition":"0.0",
   "dir":"Open Short","closedPnl":"0.0","hash":"0xaa","oid":1,"crossed":true,"fee":"0.027693","tid":30,"feeToken":"USDC"},
  {"coin":"BTC","px":"82962.0","sz":"0.00014","side":"A","time":1791902326268,"startPosition":"0.00014",
   "dir":"Close Long","closedPnl":"0.00028","hash":"0xbb","oid":2,"crossed":true,"fee":"0.005226","tid":20,"feeToken":"USDC"},
  {"coin":"BTC","px":"82960.0","sz":"0.00014","side":"B","time":1791902315752,"startPosition":"0.0",
   "dir":"Open Long","closedPnl":"0.0","hash":"0xcc","oid":3,"crossed":true,"fee":"0.005226","tid":10,"feeToken":"USDC"}
]"#;

fn only_perp(reading: &Reading) -> &gluonscan_core::PerpPosition {
    reading
        .positions
        .iter()
        .find_map(|p| match p {
            Position::Perp(p) => Some(p),
            _ => None,
        })
        .expect("a perp position")
}

// `fills` answers `userFills` (registered first, so it wins); the rest is the unified BTC short.
async fn read_with_fills(fills: MockHttp) -> Reading {
    let http = fills
        .on(Match::body_contains("frontendOpenOrders"), "[]")
        .on(Match::body_contains("\"clearinghouseState\""), BTC_SHORT)
        .on(Match::body_contains("spotClearinghouseState"), SPOT_UNIFIED)
        .on(
            Match::body_contains("userAbstraction"),
            r#""unifiedAccount""#,
        );
    read_with(http, Detail::Full).await.expect("read")
}

#[tokio::test]
async fn the_position_is_dated_by_the_fill_that_opened_it() {
    let reading =
        read_with_fills(MockHttp::new().on(Match::body_contains("\"userFills\""), RECENT_FILLS))
            .await;
    assert_eq!(
        only_perp(&reading).opened_at,
        Some(Timestamp(1_795_536_506))
    );
}

#[tokio::test]
async fn a_flip_dates_the_position_it_opened() {
    // One sell of 0.003 from a 0.001 long closes the long and opens a 0.002 short.
    let fills = r#"[
      {"coin":"BTC","px":"60000.0","sz":"0.003","side":"A","time":1795000000999,"startPosition":"0.001",
       "dir":"Long > Short","closedPnl":"1.0","hash":"0x01","oid":1,"crossed":true,"fee":"0.01","tid":5,"feeToken":"USDC"},
      {"coin":"BTC","px":"59000.0","sz":"0.001","side":"B","time":1794000000000,"startPosition":"0.0",
       "dir":"Open Long","closedPnl":"0.0","hash":"0x02","oid":2,"crossed":true,"fee":"0.01","tid":4,"feeToken":"USDC"}
    ]"#;
    let reading =
        read_with_fills(MockHttp::new().on(Match::body_contains("\"userFills\""), fills)).await;
    assert_eq!(
        only_perp(&reading).opened_at,
        Some(Timestamp(1_795_000_000))
    );
}

#[tokio::test]
async fn the_latest_opening_wins_over_an_older_closed_one_on_the_same_side() {
    let fills = r#"[
      {"coin":"BTC","px":"62000.0","sz":"0.001","side":"A","time":1795500000000,"startPosition":"0.0",
       "dir":"Open Short","closedPnl":"0.0","hash":"0x05","oid":3,"crossed":true,"fee":"0.01","tid":52,"feeToken":"USDC"},
      {"coin":"BTC","px":"61000.0","sz":"0.001","side":"B","time":1795400000000,"startPosition":"-0.001",
       "dir":"Close Short","closedPnl":"1.0","hash":"0x06","oid":2,"crossed":true,"fee":"0.01","tid":51,"feeToken":"USDC"},
      {"coin":"BTC","px":"63000.0","sz":"0.001","side":"A","time":1795300000000,"startPosition":"0.0",
       "dir":"Open Short","closedPnl":"0.0","hash":"0x07","oid":1,"crossed":true,"fee":"0.01","tid":50,"feeToken":"USDC"}
    ]"#;
    let reading =
        read_with_fills(MockHttp::new().on(Match::body_contains("\"userFills\""), fills)).await;
    assert_eq!(
        only_perp(&reading).opened_at,
        Some(Timestamp(1_795_500_000))
    );
}

#[tokio::test]
async fn adding_to_a_position_does_not_move_its_opening_date() {
    let fills = r#"[
      {"coin":"BTC","px":"61000.0","sz":"0.0004","side":"A","time":1795600000000,"startPosition":"-0.00058",
       "dir":"Open Short","closedPnl":"0.0","hash":"0x03","oid":1,"crossed":true,"fee":"0.01","tid":40,"feeToken":"USDC"},
      {"coin":"BTC","px":"62796.0","sz":"0.00058","side":"A","time":1795536506394,"startPosition":"0.0",
       "dir":"Open Short","closedPnl":"0.0","hash":"0xaa","oid":2,"crossed":true,"fee":"0.01","tid":30,"feeToken":"USDC"}
    ]"#;
    let reading =
        read_with_fills(MockHttp::new().on(Match::body_contains("\"userFills\""), fills)).await;
    assert_eq!(
        only_perp(&reading).opened_at,
        Some(Timestamp(1_795_536_506))
    );
}

#[tokio::test]
async fn no_opening_fill_in_the_window_leaves_the_date_unknown() {
    // Only an add to the short is in the window: the opening is older than the fills served.
    let fills = r#"[
      {"coin":"BTC","px":"61000.0","sz":"0.0004","side":"A","time":1795600000000,"startPosition":"-0.00058",
       "dir":"Open Short","closedPnl":"0.0","hash":"0x03","oid":1,"crossed":true,"fee":"0.01","tid":40,"feeToken":"USDC"},
      {"coin":"ETH","px":"2500.0","sz":"1","side":"A","time":1795700000000,"startPosition":"0.0",
       "dir":"Open Short","closedPnl":"0.0","hash":"0x04","oid":2,"crossed":true,"fee":"0.01","tid":41,"feeToken":"USDC"}
    ]"#;
    let reading =
        read_with_fills(MockHttp::new().on(Match::body_contains("\"userFills\""), fills)).await;
    assert_eq!(only_perp(&reading).opened_at, None);
}

#[tokio::test]
async fn a_fills_failure_leaves_the_date_unknown_and_keeps_the_read() {
    let reading =
        read_with_fills(MockHttp::new().on_transient(Match::body_contains("\"userFills\""))).await;
    assert_eq!(only_perp(&reading).opened_at, None);
    assert_eq!(usd(balance(&reading, "USDC")), Some(dec("53.983371")));
}

#[tokio::test]
async fn fills_are_not_fetched_below_full_detail() {
    let http = Arc::new(http(BTC_SHORT, SPOT_UNIFIED, r#""unifiedAccount""#));
    let cx = Ctx::new(http.clone(), Arc::new(MockClock(0)));
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
    assert_eq!(only_perp(&reading).opened_at, None);
    assert!(http
        .calls()
        .iter()
        .all(|(_, body)| !body.contains("\"userFills\"") && !body.contains("frontendOpenOrders")));
}

fn fill(coin: &str, side: &str, sz: &str, start: &str, time: i64, tid: u64) -> String {
    format!(
        r#"{{"coin":"{coin}","px":"100.0","sz":"{sz}","side":"{side}","time":{time},"startPosition":"{start}",
        "dir":"x","closedPnl":"0.5","hash":"0x{tid:x}","oid":1,"crossed":true,"fee":"0.02","tid":{tid},"feeToken":"USDC"}}"#
    )
}

async fn history(
    http: MockHttp,
    position: Option<&str>,
    since: Option<Timestamp>,
) -> Result<History, Error> {
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            position,
            since,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn history_classifies_each_perp_fill() {
    let liquidation = format!(
        r#"{{"coin":"ETH","px":"2000.0","sz":"1","side":"B","time":9000,"startPosition":"-1",
        "dir":"Close Short","closedPnl":"-300.0","hash":"0x9","oid":1,"crossed":true,"fee":"1.0","tid":9,"feeToken":"USDC",
        "liquidation":{{"liquidatedUser":"{USER}","markPx":"2000.0","method":"market"}}}}"#
    );
    let page = format!(
        "[{}]",
        [
            fill("BTC", "B", "1", "0.0", 1000, 1),
            fill("BTC", "B", "0.5", "1", 2000, 2),
            fill("BTC", "A", "0.5", "1.5", 3000, 3),
            fill("BTC", "A", "3", "1", 4000, 4),
            fill("BTC", "B", "2", "-2", 5000, 5),
            fill("@107", "B", "2", "0.0", 6000, 6),
            fill("xyz:TSLA", "B", "2", "0.0", 7000, 7),
            liquidation,
        ]
        .join(",")
    );
    let h = history(
        MockHttp::new().on(Match::body_contains("userFillsByTime"), page),
        None,
        None,
    )
    .await
    .expect("history");
    let kinds: Vec<(EventKind, PerpSide)> = h
        .events
        .iter()
        .map(|e| (e.kind, e.perp.as_ref().expect("perp detail").side))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (EventKind::PerpOpen, PerpSide::Long),
            (EventKind::PerpOpen, PerpSide::Long),
            (EventKind::PerpClose, PerpSide::Long),
            (EventKind::PerpFlip, PerpSide::Short),
            (EventKind::PerpClose, PerpSide::Short),
            (EventKind::Liquidation, PerpSide::Short),
        ],
        "spot and builder-dex fills are not perp events of this account"
    );
    let first = &h.events[0];
    assert_eq!(first.at, Timestamp(1));
    assert_eq!(first.tx, "0x1");
    assert_eq!(first.amount.amount, dec("1"));
    let detail = first.perp.as_ref().unwrap();
    assert_eq!(detail.market, "BTC");
    assert_eq!(detail.price, dec("100.0"));
    assert_eq!(detail.fill_id, "1");
    assert_eq!(
        detail.realized_pnl.as_ref().map(|m| m.amount),
        Some(dec("0.5"))
    );
    assert_eq!(detail.fee.as_ref().map(|m| m.amount), Some(dec("0.02")));
    assert!(!h.earlier_events_unavailable);
}

#[tokio::test]
async fn history_classifies_the_short_side_and_orders_fills_in_time() {
    let liquidator = r#"{"coin":"ETH","px":"100.0","sz":"1","side":"B","time":5000,"startPosition":"2",
        "dir":"Open Long","closedPnl":"0.0","hash":"0xe","oid":1,"crossed":true,"fee":"0.02","tid":14,"feeToken":"USDC",
        "liquidation":{"liquidatedUser":"0x00000000000000000000000000000000000000ff","markPx":"100.0","method":"market"}}"#;
    // Out of order on purpose, with two fills in one millisecond whose ids differ in length.
    let page = format!(
        "[{}]",
        [
            fill("SOL", "B", "1", "1", 6000, 100),
            fill("ETH", "B", "0.5", "-2", 3000, 12),
            fill("ETH", "A", "1", "0.0", 1000, 10),
            liquidator.to_string(),
            fill("SOL", "B", "1", "0.0", 6000, 99),
            fill("ETH", "B", "3.5", "-1.5", 4000, 13),
            fill("ETH", "A", "1", "-1", 2000, 11),
        ]
        .join(",")
    );
    let h = history(
        MockHttp::new().on(Match::body_contains("userFillsByTime"), page),
        None,
        None,
    )
    .await
    .expect("history");
    let got: Vec<(EventKind, PerpSide, &str)> = h
        .events
        .iter()
        .map(|e| {
            let p = e.perp.as_ref().unwrap();
            (e.kind, p.side, p.fill_id.as_str())
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (EventKind::PerpOpen, PerpSide::Short, "10"),
            (EventKind::PerpOpen, PerpSide::Short, "11"),
            (EventKind::PerpClose, PerpSide::Short, "12"),
            (EventKind::PerpFlip, PerpSide::Long, "13"),
            (EventKind::PerpOpen, PerpSide::Long, "14"),
            (EventKind::PerpOpen, PerpSide::Long, "99"),
            (EventKind::PerpOpen, PerpSide::Long, "100"),
        ],
        "a fill this account took as liquidator is its own trade, not its liquidation"
    );
}

#[tokio::test]
async fn history_filters_by_market_and_starts_after_since() {
    let page = format!(
        "[{},{}]",
        fill("BTC", "B", "1", "0.0", 5000, 1),
        fill("ETH", "B", "1", "0.0", 6000, 2)
    );
    let h = history(
        MockHttp::new().on(Match::body_contains(r#""startTime":5000"#), page),
        Some("ETH"),
        Some(Timestamp(4)),
    )
    .await
    .expect("`since` 4s asks from 5000 ms");
    assert_eq!(h.events.len(), 1);
    assert_eq!(h.events[0].perp.as_ref().unwrap().market, "ETH");
}

#[tokio::test]
async fn a_fee_in_another_token_has_no_usd_value() {
    let page = r#"[{"coin":"BTC","px":"100.0","sz":"1","side":"B","time":1000,"startPosition":"0.0","dir":"Open Long",
      "closedPnl":"0.0","hash":"0x1","oid":1,"crossed":true,"fee":"0.3","tid":1,"feeToken":" HYPE"}]"#;
    let h = history(
        MockHttp::new().on(Match::body_contains("userFillsByTime"), page),
        None,
        None,
    )
    .await
    .expect("history");
    assert_eq!(h.events[0].perp.as_ref().unwrap().fee, None);
}

#[tokio::test]
async fn a_malformed_fill_fails_the_history_closed() {
    let page = r#"[{"coin":"BTC","px":"100.0","sz":"1","side":"?","time":1000,"startPosition":"0.0",
      "closedPnl":"0.0","hash":"0x1","fee":"0.3","tid":1,"feeToken":"USDC"}]"#;
    let err = history(
        MockHttp::new().on(Match::body_contains("userFillsByTime"), page),
        None,
        None,
    )
    .await
    .expect_err("a fill with an unknown side cannot be classified");
    assert!(!err.is_retryable());
}

// `count` fills, one per millisecond from 1 ms, served the way Hyperliquid pages them: each request
// returns up to 2000 fills from its inclusive `startTime`, oldest first.
fn paged(count: i64) -> MockHttp {
    let fills: Vec<String> = (1..=count)
        .map(|t| fill("BTC", "B", "1", &(t - 1).to_string(), t, t as u64))
        .collect();
    let mut http = MockHttp::new();
    let mut start = 0i64;
    loop {
        let from = (start.max(1) - 1) as usize;
        let page: Vec<String> = fills.iter().skip(from).take(2000).cloned().collect();
        let full = page.len() == 2000;
        let last = (from + page.len()) as i64;
        http = http.on(
            Match::body_contains(format!(r#""startTime":{start}"#)),
            format!("[{}]", page.join(",")),
        );
        if !full {
            return http;
        }
        start = last;
    }
}

#[tokio::test]
async fn history_pages_through_full_pages_without_duplicates() {
    let h = history(paged(4500), None, None).await.expect("history");
    assert_eq!(h.events.len(), 4500);
    assert!(h.events.windows(2).all(|w| w[0].at <= w[1].at));
    assert!(!h.earlier_events_unavailable);
}

#[tokio::test]
async fn history_reaching_the_retention_limit_says_older_fills_may_be_missing() {
    let h = history(paged(10_000), None, None).await.expect("history");
    assert_eq!(h.events.len(), 10_000);
    assert!(h.earlier_events_unavailable);
}

#[tokio::test]
async fn history_whose_pages_never_end_fails_rather_than_drop_the_newest_fills() {
    let http = Arc::new(paged(14_000));
    let cx = Ctx::new(http.clone(), Arc::new(MockClock(0)));
    let err = Hyperliquid::new()
        .with_endpoint("https://hl.test/info")
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Hyperliquid,
            None,
            None,
            &cx,
        )
        .await
        .expect_err("every page came back full");
    assert!(err.is_retryable(), "{err:?}");
    assert_eq!(http.calls().len(), 6);
}

#[tokio::test]
async fn a_full_page_within_one_millisecond_fails_closed() {
    let page: Vec<String> = (0..2000)
        .map(|i| fill("BTC", "B", "1", "0.0", 0, i))
        .collect();
    let err = history(
        MockHttp::new().on(
            Match::body_contains("userFillsByTime"),
            format!("[{}]", page.join(",")),
        ),
        None,
        None,
    )
    .await
    .expect_err("paging past this page would skip fills");
    assert!(!err.is_retryable());
}
