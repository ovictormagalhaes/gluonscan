//! Contract-based tests for the Lido adapter, entirely offline. The `eth_call` replies are real
//! `balanceOf` return words captured from Ethereum mainnet (2026-10-03): the stETH balance of the
//! wstETH contract (a large, deterministic holder) and the wstETH balance of the Aave V3 wstETH
//! aToken. A reply is routed to the right token by the `to` address in the request params.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter, RateBasis, RateWindow,
    Source, Timestamp, TokenAddress, Wallet, U256,
};
use gluonscan_lido::Lido;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const STETH_ADDR: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
const WSTETH_ADDR: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";

// Real captured return words (32-byte uint256, hex).
const STETH_BAL: &str = "0x00000000000000000000000000000000000000000003c747d65cb3717a72c5b1"; // 4,567,853.554182302240785841 stETH
const WSTETH_BAL: &str = "0x00000000000000000000000000000000000000000000a3c36de28b700a2a2b12"; // 773,350.769846533726284562 wstETH
const ZERO_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn ok(result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
}

const RPC_ERROR: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#;

fn eth_call_to(fragment: &str) -> Match {
    Match::all([Match::method("eth_call"), Match::body_contains(fragment)])
}

fn evm(addr: &str) -> Option<TokenAddress> {
    Some(TokenAddress::Evm(addr.parse().unwrap()))
}

async fn read_replies(steth: String, wsteth: String) -> Result<gluonscan_core::Reading, Error> {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), steth) // stETH `to`
        .on(eth_call_to("7f39c581f595b53c"), wsteth); // wstETH `to`
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

async fn read_with(steth: &str, wsteth: &str) -> Result<gluonscan_core::Reading, Error> {
    read_replies(ok(steth), ok(wsteth)).await
}

// Real `apr/last` and `apr/sma` responses (2026-10-15): one daily oracle report, and the 7 most
// recent reports with their simple mean.
const APR_LAST: &str = r#"{"data":{"timeUnix":1792066919,"apr":2.255},"meta":{"symbol":"stETH","address":"0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84","chainId":1}}"#;
const APR_SMA: &str = r#"{"data":{"aprs":[{"timeUnix":1791548555,"apr":2.238},{"timeUnix":1791635003,"apr":2.216},{"timeUnix":1791721319,"apr":2.203},{"timeUnix":1791807719,"apr":2.218},{"timeUnix":1791894191,"apr":2.245},{"timeUnix":1791980507,"apr":2.209},{"timeUnix":1792066919,"apr":2.255}],"smaApr":2.2262857142857144},"meta":{"symbol":"stETH","address":"0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84","chainId":1}}"#;

async fn read_rates(http: MockHttp) -> gluonscan_core::Rates {
    read_rates_at(http, 1_792_070_000).await
}

async fn read_rates_at(http: MockHttp, now: i64) -> gluonscan_core::Rates {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), ok(STETH_BAL))
        .on(eth_call_to("7f39c581f595b53c"), ok(WSTETH_BAL));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(now))).with_rpc(Arc::new(rpc));
    let reading = Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    p.rates().clone()
}

#[tokio::test]
async fn lido_rates_are_aprs_never_apys() {
    let rates = read_rates(
        MockHttp::new()
            .on(Match::primary_contains("apr/last"), APR_LAST)
            .on(Match::primary_contains("apr/sma"), APR_SMA),
    )
    .await;
    assert_eq!(rates.len(), 2);
    assert!(
        rates.iter().all(|r| r.basis == RateBasis::Apr),
        "Lido publishes APRs; they must never be labelled APY"
    );
    assert!(rates.iter().all(|r| r.source == Source::Api));

    // The last oracle report: 2.255% -> 0.02255, dated at the report, not at the read.
    let last = rates
        .get(RateBasis::Apr, RateWindow::Epoch { hours: Some(24) })
        .expect("last report apr");
    assert_eq!(last.value, Decimal::from_str_exact("0.02255").unwrap());
    assert_eq!(last.as_of, Timestamp(1_792_066_919));

    // The 7-report SMA, dated at the newest report.
    let sma = rates
        .get(RateBasis::Apr, RateWindow::Trailing { hours: 168 })
        .expect("7-day sma");
    assert_eq!(
        sma.value,
        Decimal::from_str_exact("0.022262857142857144").unwrap()
    );
    assert_eq!(sma.as_of, Timestamp(1_792_066_919));
}

#[tokio::test]
async fn sma_over_fewer_than_seven_reports_is_omitted() {
    let short = r#"{"data":{"aprs":[{"timeUnix":1791980507,"apr":2.209},{"timeUnix":1792066919,"apr":2.255}],"smaApr":2.232},"meta":{}}"#;
    let rates = read_rates(
        MockHttp::new()
            .on(Match::primary_contains("apr/last"), APR_LAST)
            .on(Match::primary_contains("apr/sma"), short),
    )
    .await;
    assert!(rates
        .get(RateBasis::Apr, RateWindow::Trailing { hours: 168 })
        .is_none());
    assert_eq!(rates.len(), 1, "the last-report APR is kept on its own");
}

#[tokio::test]
async fn sma_needs_a_newest_report_at_most_48h_old_and_not_in_the_future() {
    let newest = 1_792_066_919;
    let sma = |now| async move {
        read_rates_at(
            MockHttp::new()
                .on(Match::primary_contains("apr/last"), APR_LAST)
                .on(Match::primary_contains("apr/sma"), APR_SMA),
            now,
        )
        .await
        .get(RateBasis::Apr, RateWindow::Trailing { hours: 168 })
        .is_some()
    };
    assert!(sma(newest + 48 * 3600).await);
    assert!(!sma(newest + 48 * 3600 + 1).await);
    assert!(!sma(newest - 1).await);
}

#[tokio::test]
async fn attaches_the_lido_apr_as_a_fraction() {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), ok(STETH_BAL))
        .on(eth_call_to("7f39c581f595b53c"), ok(WSTETH_BAL));
    // The Lido API returns a percentage; the position carries it as a fraction.
    let http = MockHttp::new().on(
        Match::primary_contains("apr/last"),
        r#"{"data":{"timeUnix":1,"apr":2.216},"meta":{}}"#,
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let reading = Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    // The SMA request is unmocked and fails: only that rate is missing.
    assert_eq!(p.rates().len(), 1);
    let apr = p
        .rates()
        .get(RateBasis::Apr, RateWindow::Epoch { hours: Some(24) })
        .expect("apr attached")
        .value;
    assert_eq!(apr, Decimal::from_str_exact("0.02216").unwrap());
}

#[tokio::test]
async fn apr_fetch_failure_leaves_rates_empty_but_keeps_the_position() {
    // Best-effort: the APY is informational, so an API outage (here: unmocked GET) must never fail
    // the balance read — the position is still complete, just without an APY.
    let reading = read_with(STETH_BAL, WSTETH_BAL).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert!(p.rates().is_empty());
    assert_eq!(p.staked.len(), 2);
}

#[tokio::test]
async fn returns_a_complete_stake_position_for_both_tokens() {
    let reading = read_with(STETH_BAL, WSTETH_BAL).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Lido);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 2);

    // stETH leg: pin identity AND the exact decimal amount. Asserting the amount (not just a
    // nonzero raw) is what catches a wrong `decimals` scaling or a balance<->token mix-up.
    let steth = &p.staked[0];
    assert_eq!(steth.token.symbol, "stETH");
    assert_eq!(steth.token.address, evm(STETH_ADDR));
    assert_eq!(
        steth.raw,
        U256::from_str_radix("4567853554182302240785841", 10).unwrap()
    );
    assert_eq!(steth.amount.to_string(), "4567853.554182302240785841");

    // wstETH leg: pin the wstETH address value (a wrong constant would otherwise slip through) and
    // its own distinct amount.
    let wsteth = &p.staked[1];
    assert_eq!(wsteth.token.symbol, "wstETH");
    assert_eq!(wsteth.token.address, evm(WSTETH_ADDR));
    assert_eq!(
        wsteth.raw,
        U256::from_str_radix("773350769846533726284562", 10).unwrap()
    );
    assert_eq!(wsteth.amount.to_string(), "773350.769846533726284562");

    // Lido rewards auto-compound into the balance, never separately claimable.
    assert!(p.rewards.is_empty());

    // Both tokens are reported as receipts, in order, so the idle-wallet list drops them.
    let receipts: Vec<String> = reading
        .receipt_tokens
        .iter()
        .map(|a| format!("{a:#x}"))
        .collect();
    assert_eq!(
        receipts,
        vec![STETH_ADDR.to_string(), WSTETH_ADDR.to_string()]
    );
}

#[tokio::test]
async fn only_one_token_held_yields_one_leg() {
    let reading = read_with(STETH_BAL, ZERO_WORD).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.staked[0].token.symbol, "stETH");
    assert_eq!(p.staked[0].amount.to_string(), "4567853.554182302240785841");
    assert_eq!(
        reading.receipt_tokens,
        vec![STETH_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn no_balance_is_an_empty_reading_not_an_error() {
    let reading = read_with(ZERO_WORD, ZERO_WORD).await.expect("read");
    assert!(reading.positions.is_empty());
    assert!(reading.receipt_tokens.is_empty());
}

#[tokio::test]
async fn second_read_failing_fails_closed_no_partial_position() {
    // The stETH read succeeds but the wstETH read errors. A balance without its paired read is
    // partial data, so the whole reading must fail closed — never emit the stETH-only position.
    let out = read_replies(ok(STETH_BAL), RPC_ERROR.to_string()).await;
    assert!(
        out.is_err(),
        "a failed second read must fail the whole reading, not emit a partial one"
    );
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), RPC_ERROR);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let out = Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await;
    assert!(
        out.is_err(),
        "an RPC error must fail closed, not produce a partial position"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed_permanently() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = Lido::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("Lido is Ethereum-only; other chains must fail closed");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error, not retryable"
    );
}

const ONE_WEI_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const ONE_GWEI_WORD: &str = "0x000000000000000000000000000000000000000000000000000000003b9aca00";

// stETH is share-based: a full exit leaves 1 wei of rounding residue. That is no position, but the
// token stays a receipt so a consumer's wallet list does not surface it either.
#[tokio::test]
async fn one_wei_steth_is_dust_not_a_position() {
    let reading = read_with(ONE_WEI_WORD, ZERO_WORD).await.expect("read");
    assert!(reading.positions.is_empty());
    assert_eq!(
        reading.receipt_tokens,
        vec![STETH_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn dust_leg_is_dropped_and_real_leg_kept() {
    let reading = read_with(ONE_WEI_WORD, WSTETH_BAL).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.staked[0].token.symbol, "wstETH");
    assert_eq!(reading.receipt_tokens.len(), 2);
}

#[tokio::test]
async fn one_gwei_steth_is_kept() {
    let reading = read_with(ONE_GWEI_WORD, ZERO_WORD).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked[0].raw, U256::from(1_000_000_000u64));
}
