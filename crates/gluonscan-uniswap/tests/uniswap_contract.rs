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
        .with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
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
        .with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
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

// A chain the adapter is deployed on but with no subgraph URL configured must fail closed,
// never fall through to a placeholder endpoint.
#[tokio::test]
async fn supported_chain_without_subgraph_fails_closed() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = UniswapV3::new()
        .with_subgraph(Chain::Base, "https://subgraph.test/uniswap-v3")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Summary,
            &cx,
        )
        .await
        .expect_err("Ethereum has no subgraph configured");
    assert!(!err.is_retryable());
}

// One instance routes each chain to its own subgraph URL (the engine-sharing contract).
#[tokio::test]
async fn with_subgraphs_routes_per_chain() {
    use std::collections::HashMap;
    let mut subs = HashMap::new();
    subs.insert(Chain::Base, "https://subgraph.test/base".to_string());
    subs.insert(Chain::Arbitrum, "https://subgraph.test/arbitrum".to_string());
    let adapter = UniswapV3::new().with_subgraphs(subs);

    for chain in [Chain::Base, Chain::Arbitrum] {
        let http = MockHttp::new().on(Match::body_contains("positions"), SUBGRAPH);
        let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
        let reading = adapter
            .read(&Wallet::Evm(Address::ZERO), chain, Detail::Summary, &cx)
            .await
            .expect("configured chain reads")
            .into_inner();
        assert_eq!(reading.chain, chain);
    }
}

mod history {
    use super::*;
    use gluonscan_core::{EventKind, Timestamp};

    // Three consecutive snapshots of one WETH/USDC position. Deltas: a creation deposit (snap1),
    // a withdraw (snap2), and a distinct-amount fee collect (snap3).
    const SNAPSHOTS: &str = r#"{"data":{"positionSnapshots":[
      {"timestamp":"1000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"0.0","withdrawnToken1":"0.0","collectedFeesToken0":"0.0","collectedFeesToken1":"0.0","transaction":{"id":"0xaaa"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}},
      {"timestamp":"2000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"1.0","withdrawnToken1":"2500.0","collectedFeesToken0":"0.0","collectedFeesToken1":"0.0","transaction":{"id":"0xbbb"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}},
      {"timestamp":"3000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"1.0","withdrawnToken1":"2500.0","collectedFeesToken0":"0.1","collectedFeesToken1":"300.0","transaction":{"id":"0xccc"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}
    ]}}"#;

    fn ctx(body: &'static str) -> Ctx {
        let http = MockHttp::new().on(Match::body_contains("positionSnapshots"), body);
        Ctx::new(Arc::new(http), Arc::new(MockClock(0)))
    }

    async fn history(cx: &Ctx) -> gluonscan_core::History {
        UniswapV3::new()
            .with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
            .read_history(
                &Wallet::Evm(Address::ZERO),
                Chain::Ethereum,
                Some("12345"),
                None,
                cx,
            )
            .await
            .expect("read_history")
            .into_inner()
    }

    #[tokio::test]
    async fn snapshot_deltas_become_paired_events() {
        let h = history(&ctx(SNAPSHOTS)).await;
        assert_eq!(h.protocol, Protocol::UniswapV3);
        // creation deposit (2) + withdraw (2) + collect (2) = 6 paired single-token events.
        assert_eq!(h.events.len(), 6);

        let find = |kind: EventKind, sym: &str| {
            h.events
                .iter()
                .find(|e| e.kind == kind && e.amount.token.symbol == sym)
                .unwrap_or_else(|| panic!("missing {kind:?} {sym}"))
                .clone()
        };

        assert_eq!(
            find(EventKind::Deposit, "WETH").amount.amount,
            Decimal::from_str_exact("2").unwrap()
        );
        assert_eq!(
            find(EventKind::Withdraw, "USDC").amount.amount,
            Decimal::from_str_exact("2500").unwrap()
        );
        let collect_weth = find(EventKind::CollectFees, "WETH");
        assert_eq!(
            collect_weth.amount.amount,
            Decimal::from_str_exact("0.1").unwrap()
        );
        assert_eq!(collect_weth.tx, "0xccc");
        assert_eq!(collect_weth.at, Timestamp(3000));
    }

    #[tokio::test]
    async fn equal_collect_deltas_are_dropped_as_subgraph_corruption() {
        // snap3 reports identical collectedFees on both sides (the known V3 indexing bug) → no
        // collect events; only the creation deposit + withdraw survive.
        const CORRUPT: &str = r#"{"data":{"positionSnapshots":[
          {"timestamp":"1000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"0.0","withdrawnToken1":"0.0","collectedFeesToken0":"0.0","collectedFeesToken1":"0.0","transaction":{"id":"0xaaa"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}},
          {"timestamp":"2000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"1.0","withdrawnToken1":"2500.0","collectedFeesToken0":"0.0","collectedFeesToken1":"0.0","transaction":{"id":"0xbbb"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}},
          {"timestamp":"3000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"1.0","withdrawnToken1":"2500.0","collectedFeesToken0":"949.0","collectedFeesToken1":"949.0","transaction":{"id":"0xccc"},"pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}
        ]}}"#;
        let h = history(&ctx(CORRUPT)).await;
        assert!(
            h.events.iter().all(|e| e.kind != EventKind::CollectFees),
            "equal-delta collect must be dropped"
        );
        assert_eq!(h.events.len(), 4); // deposit pair + withdraw pair
    }

    #[tokio::test]
    async fn history_requires_a_position_selector() {
        let err = UniswapV3::new()
            .with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
            .read_history(&Wallet::Evm(Address::ZERO), Chain::Ethereum, None, None, &ctx(SNAPSHOTS))
            .await
            .expect_err("history needs a position id");
        assert!(!err.is_retryable());
    }
}
