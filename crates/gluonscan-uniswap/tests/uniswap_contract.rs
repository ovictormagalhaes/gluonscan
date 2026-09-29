//! Contract-based tests: subgraph discovery (MockHttp) + on-chain `collect()` (MockChainProvider),
//! entirely offline. Verifies the adapter returns the COMPLETE position resource.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_uniswap::UniswapV3;
use rust_decimal::Decimal;

// sqrtPrice = 2^96 → tick 0, in range for [-60, 60).
const SUBGRAPH: &str = r#"{"data":{"positions":[{
  "id":"12345","liquidity":"1000000000",
  "depositedToken0":"2.0","depositedToken1":"5000.0",
  "withdrawnToken0":"0.0","withdrawnToken1":"0.0",
  "collectedFeesToken0":"0.01","collectedFeesToken1":"25.0",
  "tickLower":{"tickIdx":"-60"},"tickUpper":{"tickIdx":"60"},
  "pool":{"tick":"0","sqrtPrice":"79228162514264337593543950336","feeTier":"3000",
    "token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},
    "token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}]}}"#;

#[tokio::test]
async fn full_returns_the_complete_position() {
    let rpc_result = format!("0x{:0>64}{:0>64}", "f4240", "f4240"); // 1_000_000 raw each
    let rpc_json = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{rpc_result}"}}"#);

    let http = MockHttp::new().on(Match::body_contains("positions"), SUBGRAPH);
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), rpc_json);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = UniswapV3::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::UniswapV3);
    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };

    // range + metadata
    assert_eq!(p.token0.symbol, "WETH");
    assert_eq!(p.token1.symbol, "USDC");
    assert_eq!(p.fee_tier_bps, Some(3000));
    assert_eq!((p.tick_lower, p.tick_upper, p.tick_current), (-60, 60, 0));
    assert!(p.in_range);
    assert_eq!(p.status, gluonscan_core::PositionStatus::Active); // has liquidity

    // principal amounts (from the Q64.96 math) — in range, so both sides are held
    assert_eq!(p.assets.len(), 2);
    assert!(p.assets[0].amount > Decimal::ZERO);
    assert!(p.assets[1].amount > Decimal::ZERO);

    // lifetime totals straight from the subgraph
    assert_eq!(
        p.deposited[0].amount,
        Decimal::from_str_exact("2.0").unwrap()
    );
    assert_eq!(
        p.deposited[1].amount,
        Decimal::from_str_exact("5000.0").unwrap()
    );
    assert_eq!(
        p.collected_fees[1].amount,
        Decimal::from_str_exact("25.0").unwrap()
    );

    // uncollected fees from collect()
    assert_eq!(p.uncollected_fees.len(), 2);
    assert_eq!(
        p.uncollected_fees[1].amount,
        Decimal::from_i128_with_scale(1_000_000, 6)
    );
}

#[tokio::test]
async fn summary_has_principal_but_no_uncollected_fees_and_needs_no_rpc() {
    let http = MockHttp::new().on(Match::body_contains("positions"), SUBGRAPH);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))); // no RPC configured

    let reading = UniswapV3::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Summary,
            &cx,
        )
        .await
        .expect("summary needs no rpc")
        .into_inner();

    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert!(p.uncollected_fees.is_empty()); // fees are on-chain, gated to Full
    assert!(p.assets[0].amount > Decimal::ZERO); // principal still computed, no network
    assert_eq!(
        p.deposited[1].amount,
        Decimal::from_str_exact("5000.0").unwrap()
    );
}

#[tokio::test]
async fn unsupported_chain_errors() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = UniswapV3::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Bnb, Detail::Full, &cx)
        .await
        .expect_err("uniswap adapter not configured for BNB");
    assert!(!err.is_retryable());
}
