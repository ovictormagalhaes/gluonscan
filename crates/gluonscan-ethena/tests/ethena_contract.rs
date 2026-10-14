//! Contract-based tests for the Ethena adapter, entirely offline. The `eth_call` reply is a real
//! sUSDe `balanceOf` return word captured from Ethereum mainnet (2026-10-04): the sUSDe held by the
//! Aave V3 aEthsUSDe aToken, a large deterministic holder.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter, RateBasis, RateWindow,
    Source, Timestamp, TokenAddress, Wallet, U256,
};
use gluonscan_ethena::Ethena;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const SUSDE_ADDR: &str = "0x9d39a5de30e57443bff2a8307a4256c8797a3497";

// Real captured return word (32-byte uint256, hex).
const SUSDE_BAL: &str = "0x00000000000000000000000000000000000000000063221b757374ad02f1aba7"; // 119,844,723.127592257577921447 sUSDe
const ZERO_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

const RPC_ERROR: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#;

fn reply(result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
}

async fn read_reply(body: String) -> Result<gluonscan_core::Reading, Error> {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), body);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Ethena::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn attaches_the_ethena_yield_as_a_fraction() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), reply(SUSDE_BAL));
    // DeFiLlama's chart endpoint returns a percentage under the last data point's `apy`; carried as
    // a fraction. (Ethena's own app API is unreachable from a datacenter, hence DeFiLlama.)
    let http = MockHttp::new().on(
        Match::primary_contains("llama.fi"),
        r#"{"status":"success","data":[{"timestamp":"2026-10-04T00:00:00.000Z","apy":5.1,"apyBase":5.1},{"timestamp":"2026-10-05T00:00:00.000Z","apy":4.8492,"apyBase":4.8492}]}"#,
    );
    // Read 12h after the newest point (2026-10-05T00:00:00Z = 1791158400).
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(1_791_201_600))).with_rpc(Arc::new(rpc));
    let reading = Ethena::new()
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
    let epoch = p
        .rates()
        .get(RateBasis::Apy, RateWindow::Epoch { hours: Some(24) })
        .expect("yield attached");
    // 4.8492% -> 0.048492 fraction, dated at the chart point rather than at the read.
    assert_eq!(epoch.value, Decimal::from_str_exact("0.048492").unwrap());
    assert_eq!(epoch.as_of, Timestamp(1_791_158_400));
    assert_eq!(epoch.source, Source::Api);
    // Two daily points do not cover a week: no trailing rate, and no substitute.
    assert!(p
        .rates()
        .get(RateBasis::Apy, RateWindow::Trailing { hours: 168 })
        .is_none());
    assert_eq!(p.rates().len(), 1);
}

#[tokio::test]
async fn trailing_week_from_the_last_seven_daily_points() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), reply(SUSDE_BAL));
    // Real chart tail (2026-10-07..15): daily points stamped ~23:01Z, then today's intraday point.
    let http = MockHttp::new().on(
        Match::primary_contains("llama.fi"),
        r#"{"status":"success","data":[
          {"timestamp":"2026-10-07T23:01:37.586Z","apy":5.00367},
          {"timestamp":"2026-10-08T23:01:24.795Z","apy":5.05133},
          {"timestamp":"2026-10-09T23:01:47.211Z","apy":5.06817},
          {"timestamp":"2026-10-10T23:01:42.977Z","apy":4.86106},
          {"timestamp":"2026-10-11T23:01:23.683Z","apy":4.86848},
          {"timestamp":"2026-10-12T23:02:02.211Z","apy":4.86896},
          {"timestamp":"2026-10-13T23:01:24.945Z","apy":4.87052},
          {"timestamp":"2026-10-14T23:01:14.650Z","apy":4.88828},
          {"timestamp":"2026-10-15T15:01:21.499Z","apy":4.91502}]}"#,
    );
    // 2026-10-15T15:30:00Z.
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(1_792_078_200))).with_rpc(Arc::new(rpc));
    let reading = Ethena::new()
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
    let week = p
        .rates()
        .get(RateBasis::Apy, RateWindow::Trailing { hours: 168 })
        .expect("seven points within the span");
    // Mean of the newest 7 (10-09 .. 10-15): 34.34049 / 7 %.
    assert_eq!(
        week.value.round_dp(10),
        Decimal::from_str_exact("0.0490578429").unwrap()
    );
    // 2026-10-15T15:01:21Z, the newest point.
    assert_eq!(week.as_of, Timestamp(1_792_076_481));
    assert!(p.rates().iter().all(|r| r.basis == RateBasis::Apy));
}

#[tokio::test]
async fn returns_a_complete_stake_position() {
    let reading = read_reply(reply(SUSDE_BAL)).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Ethena);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);

    // sUSDe leg: pin identity + the exact decimal amount (catches a decimals/scaling or token swap).
    let susde = &p.staked[0];
    assert_eq!(susde.token.symbol, "sUSDe");
    assert_eq!(
        susde.token.address,
        Some(TokenAddress::Evm(SUSDE_ADDR.parse().unwrap()))
    );
    assert_eq!(
        susde.raw,
        U256::from_str_radix("119844723127592257577921447", 10).unwrap()
    );
    assert_eq!(susde.amount.to_string(), "119844723.127592257577921447");

    // Yield accrues in the share price, never a separate claimable.
    assert!(p.rewards.is_empty());

    // sUSDe is reported as a receipt so the idle-wallet list drops it (no double count).
    assert_eq!(
        reading.receipt_tokens,
        vec![SUSDE_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn no_balance_is_an_empty_reading_not_an_error() {
    let reading = read_reply(reply(ZERO_WORD)).await.expect("read");
    assert!(reading.positions.is_empty());
    assert!(reading.receipt_tokens.is_empty());
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let out = read_reply(RPC_ERROR.to_string()).await;
    assert!(
        out.is_err(),
        "an RPC error must fail closed, not produce a partial position"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed_permanently() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = Ethena::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("Ethena is Ethereum-only; other chains must fail closed");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error, not retryable"
    );
}

#[tokio::test]
async fn one_wei_susde_is_dust_not_a_position() {
    let one_wei = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let reading = read_reply(reply(one_wei)).await.expect("read");
    assert!(reading.positions.is_empty());
    assert_eq!(
        reading.receipt_tokens,
        vec![SUSDE_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn one_gwei_susde_is_kept() {
    let one_gwei = "0x000000000000000000000000000000000000000000000000000000003b9aca00";
    let reading = read_reply(reply(one_gwei)).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked[0].raw, U256::from(1_000_000_000u64));
}

// `convertToAssets(119,844,723.127592257577921447 sUSDe)` captured from mainnet at block
// 26,192,046 (2026-10-14): 150,169,017.352787340317755498 USDe.
const SUSDE_TO_USDE: &str = "0x0000000000000000000000000000000000000000007c3786be214701f029086a";
const USDE_ADDR: &str = "0x4c9edd5852cd905f086c759e8383e09bff1e68b3";

async fn read_with_rate(rate_reply: String) -> gluonscan_core::Reading {
    let rpc = MockChainProvider::new()
        .on(
            Match::all([Match::method("eth_call"), Match::body_contains("07a2d13a")]),
            rate_reply,
        )
        .on(Match::method("eth_call"), reply(SUSDE_BAL));
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Ethena::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner()
}

#[tokio::test]
async fn susde_carries_the_usde_it_redeems_for() {
    // Arrange
    let rate = reply(SUSDE_TO_USDE);

    // Act
    let reading = read_with_rate(rate).await;

    // Assert
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.underlying.len(), 1);
    let usde = p.underlying[0].as_ref().expect("sUSDe underlying");
    assert_eq!(usde.token.symbol, "USDe");
    assert_eq!(
        usde.token.address,
        Some(TokenAddress::Evm(USDE_ADDR.parse().unwrap()))
    );
    assert_eq!(usde.amount.to_string(), "150169017.352787340317755498");
}

#[tokio::test]
async fn a_failed_rate_read_leaves_the_underlying_unknown_and_keeps_the_position() {
    // Arrange
    let rate = RPC_ERROR.to_string();

    // Act
    let reading = read_with_rate(rate).await;

    // Assert
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.underlying, vec![None]);
}
