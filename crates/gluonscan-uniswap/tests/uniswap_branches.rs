//! Branch + fail-closed coverage for the Uniswap V3 adapter: malformed subgraph shapes, the
//! tick-range guard, the known collectedFees-corruption guard, and an exact-amount pin so a
//! Q64.96 scaling regression (e.g. amounts halved) breaks the build. Entirely offline.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Error, Position, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_uniswap::UniswapV3;
use rust_decimal::Decimal;

/// One in-range position at tick 0 (sqrtPrice = 2^96), range [-60, 60), WETH/USDC. `{fees0}` and
/// `{fees1}` and `{upper}` are the knobs the individual tests vary.
fn subgraph(fees0: &str, fees1: &str, upper: &str) -> String {
    format!(
        r#"{{"data":{{"positions":[{{
  "id":"12345","liquidity":"1000000000",
  "depositedToken0":"2.0","depositedToken1":"5000.0",
  "withdrawnToken0":"0.0","withdrawnToken1":"0.0",
  "collectedFeesToken0":"{fees0}","collectedFeesToken1":"{fees1}",
  "tickLower":{{"tickIdx":"-60"}},"tickUpper":{{"tickIdx":"{upper}"}},
  "pool":{{"tick":"0","sqrtPrice":"79228162514264337593543950336","feeTier":"3000",
    "token0":{{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"}},
    "token1":{{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}}}}]}}}}"#
    )
}

fn ctx_with(body: &'static str) -> Ctx {
    let http = MockHttp::new().on(Match::body_contains("positions"), body);
    Ctx::new(Arc::new(http), Arc::new(MockClock(0)))
}

async fn read(cx: &Ctx, detail: Detail) -> Result<gluonscan_core::Reading, Error> {
    UniswapV3::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Ethereum, detail, cx)
        .await
        .map(gluonscan_core::Complete::into_inner)
}

fn assert_integrity(err: Error) -> String {
    assert!(
        !err.is_retryable(),
        "integrity must not be retryable: {err:?}"
    );
    match err {
        Error::Integrity { message } => message,
        other => panic!("expected Error::Integrity, got {other:?}"),
    }
}

#[tokio::test]
async fn graphql_errors_array_fails_closed_as_integrity() {
    // `{"errors":[...],"data":null}` — data.positions is absent → not an array.
    let cx = ctx_with(r#"{"errors":[{"message":"boom"}],"data":null}"#);
    let msg = assert_integrity(read(&cx, Detail::Full).await.expect_err("must fail"));
    assert!(msg.contains("data.positions"), "wrong branch, got: {msg}");
}

#[tokio::test]
async fn positions_not_an_array_fails_closed_as_integrity() {
    // positions is an object, not an array.
    let cx = ctx_with(r#"{"data":{"positions":{"unexpected":"object"}}}"#);
    let msg = assert_integrity(read(&cx, Detail::Full).await.expect_err("must fail"));
    assert!(msg.contains("data.positions"), "wrong branch, got: {msg}");
}

#[tokio::test]
async fn empty_positions_is_ok_and_not_an_error() {
    let cx = ctx_with(r#"{"data":{"positions":[]}}"#);
    let reading = read(&cx, Detail::Full).await.expect("empty is valid");
    assert!(
        reading.positions.is_empty(),
        "expected no positions, got {}",
        reading.positions.len()
    );
}

#[tokio::test]
async fn out_of_range_tick_fails_closed_as_integrity() {
    // tickUpper 10_000_000 is well beyond MAX_TICK (887_272).
    let body: &'static str = Box::leak(subgraph("0.01", "25.0", "10000000").into_boxed_str());
    let cx = ctx_with(body);
    let msg = assert_integrity(read(&cx, Detail::Full).await.expect_err("must fail"));
    assert!(msg.contains("out of range"), "wrong branch, got: {msg}");
}

#[tokio::test]
async fn collected_fees_equal_nonzero_triggers_corruption_guard() {
    // token0 fees byte-identical to token1 fees, both non-zero → known subgraph corruption.
    let body: &'static str = Box::leak(subgraph("12.5", "12.5", "60").into_boxed_str());
    let cx = ctx_with(body);
    let msg = assert_integrity(read(&cx, Detail::Summary).await.expect_err("must fail"));
    assert!(
        msg.contains("collectedFees") && msg.contains("corruption"),
        "wrong branch, got: {msg}"
    );
}

#[tokio::test]
async fn collected_fees_equal_zero_does_not_trigger_guard() {
    // "0" == "0" is legitimate (a fresh position); must parse fine, not fail closed.
    let body: &'static str = Box::leak(subgraph("0", "0", "60").into_boxed_str());
    let cx = ctx_with(body);
    let reading = read(&cx, Detail::Summary)
        .await
        .expect("equal-zero fees are valid");
    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert_eq!(
        p.collected_fees[0].amount,
        Decimal::ZERO,
        "zero fees must survive the guard"
    );
    assert_eq!(p.collected_fees[1].amount, Decimal::ZERO);
}

#[tokio::test]
async fn principal_amounts_are_exact_at_tick_zero() {
    // Pinned from the Q64.96 math for L=1_000_000_000, sqrtPrice=2^96 (tick 0), range [-60, 60),
    // scaled by WETH(18)/USDC(6) decimals. A scaling regression (e.g. amounts halved) breaks this.
    let body: &'static str = Box::leak(subgraph("0.01", "25.0", "60").into_boxed_str());
    let cx = ctx_with(body);
    let reading = read(&cx, Detail::Summary).await.expect("read");
    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert_eq!(p.assets.len(), 2);
    assert_eq!(
        p.assets[0].amount,
        Decimal::from_str_exact(EXPECTED_AMOUNT0).unwrap(),
        "token0 principal drifted"
    );
    assert_eq!(
        p.assets[1].amount,
        Decimal::from_str_exact(EXPECTED_AMOUNT1).unwrap(),
        "token1 principal drifted"
    );
}

// Pinned from the produced Q64.96 output (WETH 18dp, USDC 6dp). The two raw base-unit figures are
// equal (~2_995_354, symmetric range around tick 0); only the decimal scaling differs.
const EXPECTED_AMOUNT0: &str = "0.000000000002995354";
const EXPECTED_AMOUNT1: &str = "2.995354";
