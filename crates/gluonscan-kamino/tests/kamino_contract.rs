//! Contract-based test: a Kamino obligation joined with reserves/metrics + on-chain mint decimals.
//! Amounts and USD come from the newest point of the obligation's `metrics/history`; the account
//! `state` (collateral shares, debt and values frozen at the last transaction) only gates it.

use std::sync::Arc;

use base64::Engine;
use gluonscan_core::{
    Chain, Ctx, Detail, Error, EventKind, Position, Protocol, ProtocolAdapter, Wallet,
};
use gluonscan_kamino::KaminoApi;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const MAIN_MARKET: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";
const WALLET: &str = "So11111111111111111111111111111111111111112";

// The account `state` as the API returns it: 5 collateral shares of SOL and 200 USDC of debt, both
// frozen at the owner's last transaction (lastUpdate = NOW - 2 days). Unused slots are zeroed.
const NOW: i64 = 1_800_000_000;
const OBLIGATIONS: &str = r#"[{
  "obligationAddress":"ob1",
  "deposits":{},
  "borrows":{},
  "state":{
    "lastUpdate":{"timestamp":1799827200},
    "deposits":[{"depositReserve":"RES_SOL","depositedAmount":"5000000000","marketValueSf":"864691128455135232000"},
                {"depositReserve":"11111111111111111111111111111111","depositedAmount":"0","marketValueSf":"0"}],
    "borrows":[{"borrowReserve":"RES_USDC","borrowedAmountSf":"230584300921369395200000000","marketValueSf":"230584300921369395200"}]
  },
  "refreshedStats":{"borrowLiquidationLimit":"370","userTotalBorrowBorrowFactorAdjusted":"200","netAccountValue":"550"}
}]"#;

const RESERVES: &str = r#"[
  {"reserve":"RES_SOL","liquidityToken":"SOL","liquidityTokenMint":"MINT_SOL","maxLtv":"0.7","supplyApy":"0.045","borrowApy":"0.02"},
  {"reserve":"RES_USDC","liquidityToken":"USDC","liquidityTokenMint":"MINT_USDC","maxLtv":"0.8","supplyApy":"0.03","borrowApy":"0.089"}
]"#;

fn mint_account(decimals: u8) -> String {
    let mut data = vec![0u8; 82];
    data[44] = decimals; // SPL mint layout: decimals at byte 44
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"value":{{"data":["{b64}","base64"]}}}}}}"#)
}

// The newest hourly point (NOW - 30 min): the shares are worth 5.2 SOL with interest, the debt has
// grown to 200.45 USDC, and both carry today's USD value. Amounts carry fractions of a base unit and
// more fractional digits than `Decimal` holds, as the live API does.
const LIVE_TS: &str = "2027-01-15T07:30:00Z";
// Differs from the account's own stats (370 / 200) so the tests prove which one the health factor
// is derived from.
const LIVE_STATS: &str = r#""refreshedStats":{"borrowLiquidationLimit":"380","userTotalBorrowBorrowFactorAdjusted":"200"}"#;
const LIVE_BORROW: &str = r#"{"reserve":"RES_USDC","mintAddress":"MINT_USDC","amount":"200450000.76060026660765739627231855281","marketValueRefreshed":"200.450000000000000000000000000000001"}"#;
fn live_point(ts: &str) -> String {
    format!(
        r#"{{"history":[
  {{"timestamp":"2027-01-15T02:00:00Z","deposits":[{{"reserve":"RES_SOL","mintAddress":"MINT_SOL","amount":"5100000000","marketValueRefreshed":"700"}}],"borrows":[{{"reserve":"RES_USDC","mintAddress":"MINT_USDC","amount":"200400000","marketValueRefreshed":"200.4"}}]}},
  {{"timestamp":"{ts}",{LIVE_STATS},"deposits":[{{"reserve":"RES_SOL","mintAddress":"MINT_SOL","amount":"5200000000.42793513","marketValueRefreshed":"780.5"}}],"borrows":[{LIVE_BORROW}]}}
]}}"#
    )
}

fn ctx() -> Ctx {
    ctx_with(OBLIGATIONS, &live_point(LIVE_TS))
}

fn ctx_with(obligations: &str, live: &str) -> Ctx {
    let http = MockHttp::new()
        .on(Match::primary_contains("/metrics/history"), live)
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), obligations)
        .on(Match::primary_contains("kamino-market"), "[]"); // JLP + Altcoins: empty
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    Ctx::new(Arc::new(http), Arc::new(MockClock(NOW))).with_rpc(Arc::new(rpc))
}

async fn read_err(cx: &Ctx) -> Error {
    KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            cx,
        )
        .await
        .expect_err("read must fail closed")
}

#[tokio::test]
async fn reads_obligation_with_reserve_join() {
    let reading = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &ctx(),
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Kamino);
    assert_eq!(reading.positions.len(), 1);

    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    // HF = borrowLiquidationLimit / borrowFactorAdjustedDebt from the point: 380 / 200 = 1.9.
    assert_eq!(
        p.health_factor,
        Some(Decimal::from_str_exact("1.9").unwrap())
    );

    // Underlying SOL from the live point (5.2), not the 5 collateral shares stored on the account,
    // valued at the refreshed $780.50, not the $750 frozen in `marketValueSf`.
    assert_eq!(p.supplied.len(), 1, "zeroed account slots are not legs");
    let sol = &p.supplied[0];
    assert_eq!(sol.amount.token.symbol, "SOL");
    assert_eq!(sol.amount.token.decimals, 9);
    assert_eq!(sol.amount.amount, Decimal::from_str_exact("5.2").unwrap());
    assert_eq!(
        sol.amount.usd.as_ref().unwrap().amount,
        Decimal::from_str_exact("780.5").unwrap()
    );
    assert_eq!(sol.max_ltv, Some(Decimal::from_str_exact("0.7").unwrap()));
    assert_eq!(sol.apy, Some(Decimal::from_str_exact("0.045").unwrap()));
    assert!(sol.is_collateral);

    // Debt with interest accrued since the last transaction: 200.45, not the stored 200.
    let usdc = &p.borrowed[0];
    assert_eq!(usdc.amount.token.symbol, "USDC");
    assert_eq!(
        usdc.amount.amount,
        Decimal::from_str_exact("200.45").unwrap()
    );
    assert_eq!(
        usdc.amount.usd.as_ref().unwrap().amount,
        Decimal::from_str_exact("200.45").unwrap()
    );
    assert_eq!(usdc.apy, Some(Decimal::from_str_exact("0.089").unwrap()));
    // The obligation address is the stable identity of the position (one owner can hold several
    // obligations across markets), so it rides as the market id.
    assert_eq!(p.market_id.as_deref(), Some("ob1"));
    // The lending market holding the obligation rides alongside, so history can be addressed as
    // "obligation|market" without rescanning every market.
    assert_eq!(p.parent_market.as_deref(), Some(MAIN_MARKET));
}

#[tokio::test]
async fn obligation_without_address_fails_closed() {
    let no_address = OBLIGATIONS.replace(r#""obligationAddress":"ob1","#, "");
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), &no_address)
        .on(Match::primary_contains("kamino-market"), "[]");
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let err = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("an obligation without its address has no stable identity");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("obligationAddress")),
        "expected Integrity(obligationAddress), got {err:?}"
    );
}

// Same obligation as OBLIGATIONS but with the `refreshedStats` block removed, so neither
// `borrowLiquidationLimit` nor `userTotalBorrowBorrowFactorAdjusted` is present and no health factor
// can be derived — while a non-zero borrow leg is still present.
const OBLIGATIONS_DEBT_NO_HF: &str = r#"[{
  "obligationAddress":"ob1",
  "deposits":{},
  "borrows":{},
  "state":{
    "lastUpdate":{"timestamp":1799827200},
    "deposits":[{"depositReserve":"RES_SOL","depositedAmount":"5000000000","marketValueSf":"864691128455135232000"}],
    "borrows":[{"borrowReserve":"RES_USDC","borrowedAmountSf":"230584300921369395200000000","marketValueSf":"230584300921369395200"}]
  }
}]"#;

#[tokio::test]
async fn non_array_obligations_shape_fails_closed() {
    // A 200 with an object body (not the expected array) must fail closed, not read as "no
    // positions". The main market is queried first, so returning `{}` there triggers the error.
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains("/obligations"), "{}");
    let rpc = MockChainProvider::new();
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let err = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("non-array obligations must fail closed");

    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("non-array")),
        "expected Integrity(non-array), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn debt_without_health_factor_fails_closed() {
    // An obligation carrying a borrow leg but no derivable health factor must fail closed rather
    // than surface a debt position with an unknown liquidation risk.
    // Neither the account nor the point carries the stats a health factor is derived from.
    let live = live_point(LIVE_TS).replace(LIVE_STATS, r#""refreshedStats":{}"#);
    let http = MockHttp::new()
        .on(Match::primary_contains("/metrics/history"), &live)
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), OBLIGATIONS_DEBT_NO_HF)
        .on(Match::primary_contains("kamino-market"), "[]");
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(NOW))).with_rpc(Arc::new(rpc));

    let err = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("debt with no health factor must fail closed");

    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("health factor")),
        "expected Integrity(health factor), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn transient_obligations_transport_error_propagates() {
    // A transport failure (HTTP 429 stand-in) on the obligations request must propagate as a
    // retryable error, never be swallowed into an empty reading.
    let http = MockHttp::new()
        .on_transient(Match::primary_contains("/obligations"))
        .on(Match::primary_contains("/reserves/metrics"), RESERVES);
    let rpc = MockChainProvider::new();
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let err = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("transient transport failure must propagate");

    assert!(err.is_retryable(), "expected retryable error, got {err:?}");
}

#[tokio::test]
async fn rejects_non_solana_chain() {
    let err = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Ethereum,
            Detail::Full,
            &ctx(),
        )
        .await
        .expect_err("Kamino is Solana-only");
    assert!(!err.is_retryable());
}

// Two consecutive obligation snapshots; events are the per-mint deltas between them.
const HISTORY: &str = r#"{"history":[
  {"timestamp":"2026-01-01T00:00:00Z","deposits":[{"mintAddress":"MINT_SOL","amount":"5000000000"}],"borrows":[]},
  {"timestamp":"2026-01-02T00:00:00Z","deposits":[{"mintAddress":"MINT_SOL","amount":"7000000000"}],"borrows":[{"mintAddress":"MINT_USDC","amount":"100000000"}]}
]}"#;

#[tokio::test]
async fn reads_obligation_history_as_snapshot_deltas() {
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains("/metrics/history"), HISTORY);
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let history = KaminoApi::new()
        .read_history(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Some(&format!("ob1|{MAIN_MARKET}")),
            None,
            &cx,
        )
        .await
        .expect("read_history")
        .into_inner();

    assert_eq!(history.protocol, Protocol::Kamino);
    assert_eq!(
        history.events.len(),
        2,
        "one SOL deposit delta + one USDC borrow delta"
    );

    let sol = history
        .events
        .iter()
        .find(|e| e.amount.token.symbol == "SOL")
        .expect("SOL event");
    assert!(matches!(sol.kind, EventKind::Deposit));
    assert_eq!(sol.amount.amount, Decimal::from_str_exact("2").unwrap()); // 7 - 5 SOL

    let usdc = history
        .events
        .iter()
        .find(|e| e.amount.token.symbol == "USDC")
        .expect("USDC event");
    assert!(matches!(usdc.kind, EventKind::Borrow));
    assert_eq!(usdc.amount.amount, Decimal::from_str_exact("100").unwrap());
}

#[tokio::test]
async fn history_requires_a_position_selector() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = KaminoApi::new()
        .read_history(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            None,
            None,
            &cx,
        )
        .await
        .expect_err("Kamino history needs a selector");
    assert!(!err.is_retryable());
}

// A selector with no explicit market ("ob1" alone) resolves the market from the owner's obligation
// lists, then reads history exactly as the "ob1|market" form would.
#[tokio::test]
async fn resolves_market_from_owner_obligations_when_selector_omits_it() {
    let http = MockHttp::new()
        .on(
            Match::primary_contains(format!("{MAIN_MARKET}/users")),
            r#"[{"obligationAddress":"ob1"}]"#,
        )
        .on(Match::primary_contains("/users/"), "[]") // the other markets hold nothing
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains("/metrics/history"), HISTORY);
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let history = KaminoApi::new()
        .read_history(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Some("ob1"),
            None,
            &cx,
        )
        .await
        .expect("read_history with market resolution")
        .into_inner();

    assert_eq!(
        history.events.len(),
        2,
        "same deltas as the explicit-market form"
    );
}

#[tokio::test]
async fn stale_history_point_fails_retryable() {
    // Newest point 4.5h old: the API stopped refreshing this obligation, so there is no live reading.
    let err = read_err(&ctx_with(OBLIGATIONS, &live_point("2027-01-15T03:30:00Z"))).await;
    assert!(
        matches!(&err, Error::Transient { message, .. } if message.contains("old")),
        "expected Transient(old), got {err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn transaction_after_newest_point_fails_retryable() {
    // The owner transacted after the newest point (lastUpdate = NOW - 10 min), so the point misses
    // that deposit, withdraw, borrow or repay.
    let moved = OBLIGATIONS.replace("1799827200", "1799999400");
    let err = read_err(&ctx_with(&moved, &live_point(LIVE_TS))).await;
    assert!(
        matches!(&err, Error::Transient { message, .. } if message.contains("after its newest history point")),
        "expected Transient(after point), got {err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn reserve_mismatch_between_account_and_point_fails_closed() {
    // The point carries no debt while the account holds a USDC borrow: it is not this obligation.
    let live = live_point(LIVE_TS).replace(LIVE_BORROW, "");
    let err = read_err(&ctx_with(OBLIGATIONS, &live)).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("borrows differ")),
        "expected Integrity(borrows differ), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn obligation_without_history_points_fails_retryable() {
    let err = read_err(&ctx_with(OBLIGATIONS, r#"{"history":[]}"#)).await;
    assert!(
        matches!(&err, Error::Transient { message, .. } if message.contains("no history point")),
        "expected Transient(no point), got {err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn history_without_array_fails_closed() {
    let err = read_err(&ctx_with(OBLIGATIONS, "{}")).await;
    assert!(matches!(&err, Error::Integrity { .. }), "got {err:?}");
}

#[tokio::test]
async fn point_exactly_max_age_is_accepted_and_one_second_more_is_not() {
    // NOW is 08:00:00; a point at 04:00:00 is exactly MAX_POINT_AGE_SECS old.
    let at_limit = ctx_with(OBLIGATIONS, &live_point("2027-01-15T04:00:00Z"));
    let _live = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &at_limit,
        )
        .await
        .expect("a point exactly at the age limit is live");
    let err = read_err(&ctx_with(OBLIGATIONS, &live_point("2027-01-15T03:59:59Z"))).await;
    assert!(matches!(err, Error::Transient { .. }), "got {err:?}");
}

#[tokio::test]
async fn missing_last_update_fails_closed() {
    // Without the account's last update the "transacted after the point" check cannot run.
    let no_update = OBLIGATIONS.replace(r#""lastUpdate":{"timestamp":1799827200},"#, "");
    let err = read_err(&ctx_with(&no_update, &live_point(LIVE_TS))).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("lastUpdate")),
        "expected Integrity(lastUpdate), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn sub_unit_debt_is_skipped_on_both_sides() {
    // A partial repay left less than one base unit of USDC debt: below 2^60 in the account, "0.4"
    // in the point. Neither side counts it, so the read succeeds with no borrow leg.
    let dust_state = OBLIGATIONS.replace("230584300921369395200000000", "461168601842738790");
    let dust_live = live_point(LIVE_TS).replace(
        LIVE_BORROW,
        r#"{"reserve":"RES_USDC","mintAddress":"MINT_USDC","amount":"0.4","marketValueRefreshed":"4e-7"}"#,
    );
    let reading = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &ctx_with(&dust_state, &dust_live),
        )
        .await
        .expect("dust debt is not a mismatch")
        .into_inner();
    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert!(p.borrowed.is_empty());
    assert_eq!(p.supplied.len(), 1);
}

#[tokio::test]
async fn missing_leg_usd_fails_closed() {
    let live = live_point(LIVE_TS).replace(r#","marketValueRefreshed":"780.5""#, "");
    let err = read_err(&ctx_with(OBLIGATIONS, &live)).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("marketValueRefreshed")),
        "expected Integrity(marketValueRefreshed), got {err:?}"
    );
}

#[tokio::test]
async fn emptied_obligation_is_read_without_a_history_point() {
    // Every slot zeroed: the obligation is still listed but holds nothing. No history is fetched
    // (the mock has none to serve), and the position comes back empty.
    let empty = r#"[{
  "obligationAddress":"ob1",
  "state":{
    "lastUpdate":{"timestamp":1799827200},
    "deposits":[{"depositReserve":"11111111111111111111111111111111","depositedAmount":"0"}],
    "borrows":[{"borrowReserve":"11111111111111111111111111111111","borrowedAmountSf":"0"}]
  }
}]"#;
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), empty)
        .on(Match::primary_contains("kamino-market"), "[]");
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(NOW)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let reading = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("an emptied obligation reads without history")
        .into_inner();
    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert!(p.supplied.is_empty() && p.borrowed.is_empty());
}

#[tokio::test]
async fn non_base58_obligation_address_fails_closed() {
    let bad = OBLIGATIONS.replace(
        r#""obligationAddress":"ob1""#,
        r#""obligationAddress":"ob1/../x""#,
    );
    let err = read_err(&ctx_with(&bad, &live_point(LIVE_TS))).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("base58")),
        "expected Integrity(base58), got {err:?}"
    );
}

#[tokio::test]
async fn exponent_notation_usd_is_read() {
    // The API prints values under 1e-7 in exponent form; a real leg can carry one too.
    let live = live_point(LIVE_TS).replace(
        r#""marketValueRefreshed":"780.5""#,
        r#""marketValueRefreshed":"7.805e2""#,
    );
    let reading = KaminoApi::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &ctx_with(OBLIGATIONS, &live),
        )
        .await
        .expect("exponent USD parses")
        .into_inner();
    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert_eq!(
        p.supplied[0].amount.usd.as_ref().unwrap().amount,
        Decimal::from_str_exact("780.5").unwrap()
    );
}

#[tokio::test]
async fn negative_leg_usd_fails_closed() {
    let live = live_point(LIVE_TS).replace(
        r#""marketValueRefreshed":"780.5""#,
        r#""marketValueRefreshed":"-780.5""#,
    );
    let err = read_err(&ctx_with(OBLIGATIONS, &live)).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("marketValueRefreshed")),
        "expected Integrity(marketValueRefreshed), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn account_without_leg_arrays_fails_closed() {
    // A `state` missing its leg arrays is unreadable, not an emptied obligation.
    let no_borrows = OBLIGATIONS.replace(
        r#""borrows":[{"borrowReserve""#,
        r#""borrowz":[{"borrowReserve""#,
    );
    let err = read_err(&ctx_with(&no_borrows, &live_point(LIVE_TS))).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("state.borrows")),
        "expected Integrity(state.borrows), got {err:?}"
    );
    assert!(!err.is_retryable());
}
