//! Contract-based tests: subgraph discovery (MockHttp) + on-chain `collect()` (MockChainProvider),
//! entirely offline.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_uniswap::UniswapV3;
use rust_decimal::Decimal;

const SUBGRAPH: &str = r#"{"data":{"positions":[{
  "id":"12345","liquidity":"1000000",
  "tickLower":{"tickIdx":"-60"},"tickUpper":{"tickIdx":"60"},
  "pool":{"tick":"0",
    "token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},
    "token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}]}}"#;

#[tokio::test]
async fn full_reads_range_and_uncollected_fees() {
    // collect() returns two uint256 words; 0xf4240 == 1_000_000 raw for each token.
    let rpc_result = format!("0x{:0>64}{:0>64}", "f4240", "f4240");
    let rpc_json = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{rpc_result}"}}"#);

    let http = MockHttp::new().on(Match::body_contains("positions"), SUBGRAPH);
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), rpc_json);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = UniswapV3::new()
        .read(Address::ZERO, Chain::Ethereum, Detail::Full, &cx)
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::UniswapV3);
    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert_eq!(p.in_range, Some(true)); // tick 0 within [-60, 60)
    assert_eq!(p.uncollected_fees.len(), 2);
    assert_eq!(p.uncollected_fees[0].token.symbol, "WETH");
    assert_eq!(
        p.uncollected_fees[0].amount,
        Decimal::from_i128_with_scale(1_000_000, 18)
    );
    assert_eq!(p.uncollected_fees[1].token.symbol, "USDC");
    assert_eq!(
        p.uncollected_fees[1].amount,
        Decimal::from_i128_with_scale(1_000_000, 6)
    );
}

#[tokio::test]
async fn summary_skips_fees_and_needs_no_rpc() {
    let http = MockHttp::new().on(Match::body_contains("positions"), SUBGRAPH);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))); // no RPC configured

    let reading = UniswapV3::new()
        .read(Address::ZERO, Chain::Ethereum, Detail::Summary, &cx)
        .await
        .expect("summary needs no rpc")
        .into_inner();

    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert!(p.uncollected_fees.is_empty());
    assert_eq!(p.in_range, Some(true));
}

#[tokio::test]
async fn unsupported_chain_errors() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = UniswapV3::new()
        .read(Address::ZERO, Chain::Bnb, Detail::Full, &cx)
        .await
        .expect_err("uniswap adapter not configured for BNB");
    assert!(!err.is_retryable());
}
