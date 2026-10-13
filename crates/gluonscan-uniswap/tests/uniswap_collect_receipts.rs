//! The subgraph mirrors `collectedFeesToken0` into `collectedFeesToken1` for many positions. With an
//! RPC configured, the adapter rebuilds the true collect amounts from the `Collect` logs in the
//! receipts of the position's snapshot txs. Offline via MockHttp + MockChainProvider.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, EventKind, Position, ProtocolAdapter, Timestamp, Wallet,
};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_uniswap::UniswapV3;
use rust_decimal::Decimal;

const MANAGER: &str = "0xC36442b4a4522E871399CD717aBDD847Ab11FE88";
const COLLECT_TOPIC: &str = "0x40d0efd1a53d60ecbf40971b9daf7dc90178c3aadc7aab1765632738fa8b8f01";
const POOL: &str = r#""pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}"#;

fn snapshot(ts: u32, col0: &str, col1: &str, tx: &str) -> String {
    format!(
        r#"{{"timestamp":"{ts}","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"0.0","withdrawnToken1":"0.0","collectedFeesToken0":"{col0}","collectedFeesToken1":"{col1}","transaction":{{"id":"{tx}"}},{POOL}}}"#
    )
}

// Creation (0xaaa), then two collects whose cumulative token1 series mirrors token0.
fn mirrored_snapshots() -> String {
    format!(
        r#"{{"data":{{"positionSnapshots":[{},{},{}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0xaaa"),
        snapshot(2000, "0.1", "0.1", "0xbbb"),
        snapshot(3000, "0.2", "0.2", "0xccc"),
    )
}

fn word(hex: &str) -> String {
    format!("{hex:0>64}")
}

fn token_topic(id: u64) -> String {
    format!("0x{}", word(&format!("{id:x}")))
}

fn collect_log(address: &str, token_id: u64, amount0: u128, amount1: u128) -> String {
    format!(
        r#"{{"address":"{address}","topics":["{COLLECT_TOPIC}","{}"],"data":"0x{}{}{}"}}"#,
        token_topic(token_id),
        word("1"),
        word(&format!("{amount0:x}")),
        word(&format!("{amount1:x}")),
    )
}

fn receipt(logs: &[String]) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"logs":[{}]}}}}"#,
        logs.join(",")
    )
}

fn on_receipt(rpc: MockChainProvider, tx: &str, body: String) -> MockChainProvider {
    rpc.on(
        Match::all([
            Match::method("eth_getTransactionReceipt"),
            Match::body_contains(tx),
        ]),
        body,
    )
}

// 0.1 WETH + 300 USDC, then 0.1 WETH + 50 USDC. Decoys: another token id's collect and the same
// event signature from another contract.
fn receipts() -> MockChainProvider {
    let rpc = on_receipt(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on_receipt(
        rpc,
        "0xbbb",
        receipt(&[
            collect_log(MANAGER, 12345, 100_000_000_000_000_000, 300_000_000),
            collect_log(MANAGER, 999, 7, 7),
        ]),
    );
    on_receipt(
        rpc,
        "0xccc",
        receipt(&[
            collect_log("0x0000000000000000000000000000000000000001", 12345, 1, 1),
            collect_log(MANAGER, 12345, 100_000_000_000_000_000, 50_000_000),
        ]),
    )
}

fn adapter() -> UniswapV3 {
    UniswapV3::new().with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
}

fn dec(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}

async fn history(
    cx: &Ctx,
    since: Option<Timestamp>,
) -> Result<gluonscan_core::History, gluonscan_core::Error> {
    adapter()
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Some("12345"),
            since,
            cx,
        )
        .await
        .map(|c| c.into_inner())
}

fn snapshots_http(body: String) -> MockHttp {
    MockHttp::new().on(Match::body_contains("positionSnapshots"), body)
}

#[tokio::test]
async fn mirrored_history_is_rebuilt_from_collect_receipts() {
    let cx = Ctx::new(
        Arc::new(snapshots_http(mirrored_snapshots())),
        Arc::new(MockClock(0)),
    )
    .with_rpc(Arc::new(receipts()));

    let h = history(&cx, None).await.expect("history");

    assert!(!h.collected_fees_unavailable, "rebuilt collects are known");
    let collects: Vec<_> = h
        .events
        .iter()
        .filter(|e| e.kind == EventKind::CollectFees)
        .map(|e| {
            (
                e.tx.as_str(),
                e.at,
                e.amount.token.symbol.as_str(),
                e.amount.amount,
            )
        })
        .collect();
    assert_eq!(
        collects,
        vec![
            ("0xbbb", Timestamp(2000), "WETH", dec("0.1")),
            ("0xbbb", Timestamp(2000), "USDC", dec("300")),
            ("0xccc", Timestamp(3000), "WETH", dec("0.1")),
            ("0xccc", Timestamp(3000), "USDC", dec("50")),
        ]
    );
    assert!(
        h.events.iter().any(|e| e.kind == EventKind::Deposit),
        "deposits from the snapshot deltas are kept"
    );
}

#[tokio::test]
async fn an_incremental_sync_does_not_refetch_the_baseline_receipt() {
    let body = format!(
        r#"{{"data":{{"positionSnapshots":[{},{}]}}}}"#,
        snapshot(2000, "0.1", "0.1", "0xbbb"),
        snapshot(3000, "0.2", "0.2", "0xccc"),
    );
    let rpc = Arc::new(receipts());
    let cx = Ctx::new(Arc::new(snapshots_http(body)), Arc::new(MockClock(0))).with_rpc(rpc.clone());

    let h = history(&cx, Some(Timestamp(2500))).await.expect("history");

    let txs: Vec<_> = h
        .events
        .iter()
        .filter(|e| e.kind == EventKind::CollectFees)
        .map(|e| e.tx.as_str())
        .collect();
    assert_eq!(txs, vec!["0xccc", "0xccc"]);
    assert!(
        rpc.calls()
            .iter()
            .all(|(_, params)| !params.contains("0xbbb")),
        "the baseline collect is already stored"
    );
}

// A receipt the node does not return must fail the history: reading it as "no collect" would
// persist a partial collect stream.
#[tokio::test]
async fn a_missing_receipt_fails_the_history() {
    let rpc = on_receipt(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on_receipt(
        rpc,
        "0xbbb",
        r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string(),
    );
    let rpc = on_receipt(rpc, "0xccc", receipt(&[]));
    let cx = Ctx::new(
        Arc::new(snapshots_http(mirrored_snapshots())),
        Arc::new(MockClock(0)),
    )
    .with_rpc(Arc::new(rpc));

    assert!(history(&cx, None).await.is_err());
}

// A Collect sends out everything owed, principal released by decreaseLiquidity included, exactly
// like the subgraph's own collected totals (healthy positions match the receipt sums to the wei).
// The withdraw and the collect of one exit tx are both kept; the consumer nets them.
#[tokio::test]
async fn an_exit_keeps_its_withdraw_and_a_collect_carrying_the_principal() {
    let exit = format!(
        r#"{{"timestamp":"2000","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"2.0","withdrawnToken1":"5000.0","collectedFeesToken0":"2.1","collectedFeesToken1":"2.1","transaction":{{"id":"0xbbb"}},{POOL}}}"#
    );
    let body = format!(
        r#"{{"data":{{"positionSnapshots":[{},{exit}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0xaaa"),
    );
    let rpc = on_receipt(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on_receipt(
        rpc,
        "0xbbb",
        receipt(&[
            collect_log(MANAGER, 12345, 2_000_000_000_000_000_000, 5_000_000_000),
            collect_log(MANAGER, 12345, 100_000_000_000_000_000, 30_000_000),
        ]),
    );
    let cx =
        Ctx::new(Arc::new(snapshots_http(body)), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let h = history(&cx, None).await.expect("history");

    let of = |kind: EventKind, symbol: &str| {
        h.events
            .iter()
            .filter(|e| e.kind == kind && e.amount.token.symbol == symbol)
            .map(|e| e.amount.amount)
            .sum::<Decimal>()
    };
    assert_eq!(of(EventKind::Withdraw, "USDC"), dec("5000"));
    assert_eq!(
        of(EventKind::CollectFees, "USDC"),
        dec("5030"),
        "two Collects in one tx are summed, principal included"
    );
    assert_eq!(of(EventKind::CollectFees, "WETH"), dec("2.1"));
}

// A snapshot whose totals cannot be read would leave its tx's receipt unread, so the collects stay
// withheld rather than rebuilt without it (deposits and withdraws are still returned).
#[tokio::test]
async fn a_malformed_snapshot_keeps_the_collects_withheld() {
    let broken = format!(
        r#"{{"timestamp":"2000","depositedToken0":"oops","depositedToken1":"5000.0","withdrawnToken0":"0.0","withdrawnToken1":"0.0","collectedFeesToken0":"0.1","collectedFeesToken1":"0.1","transaction":{{"id":"0xbbb"}},{POOL}}}"#
    );
    let body = format!(
        r#"{{"data":{{"positionSnapshots":[{},{broken},{}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0xaaa"),
        snapshot(3000, "0.2", "0.2", "0xccc"),
    );
    let cx = Ctx::new(Arc::new(snapshots_http(body)), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(receipts()));

    let h = history(&cx, None).await.expect("history");
    assert!(h.collected_fees_unavailable);
    assert!(h.events.iter().all(|e| e.kind != EventKind::CollectFees));
    assert!(h.events.iter().any(|e| e.kind == EventKind::Deposit));
}

// Two txs in one block collapse into one snapshot, so a collect can lack a snapshot tx. Token0's
// collected series is intact on a mirrored stream: receipts that do not reproduce it keep the
// collects withheld.
#[tokio::test]
async fn receipts_that_miss_a_collect_keep_the_history_withheld() {
    let rpc = on_receipt(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on_receipt(
        rpc,
        "0xbbb",
        receipt(&[collect_log(
            MANAGER,
            12345,
            100_000_000_000_000_000,
            300_000_000,
        )]),
    );
    let rpc = on_receipt(rpc, "0xccc", receipt(&[]));
    let cx = Ctx::new(
        Arc::new(snapshots_http(mirrored_snapshots())),
        Arc::new(MockClock(0)),
    )
    .with_rpc(Arc::new(rpc));

    let h = history(&cx, None).await.expect("history");
    assert!(h.collected_fees_unavailable);
    assert!(h.events.iter().all(|e| e.kind != EventKind::CollectFees));
}

#[tokio::test]
async fn an_open_read_whose_receipts_miss_a_collect_stays_unavailable() {
    let rpc = on_receipt(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on_receipt(
        rpc,
        "0xbbb",
        receipt(&[collect_log(
            MANAGER,
            12345,
            100_000_000_000_000_000,
            300_000_000,
        )]),
    );
    let rpc = uncollected(on_receipt(rpc, "0xccc", receipt(&[])));
    let cx =
        Ctx::new(Arc::new(read_http("1000000000")), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let p = read_one(&cx).await;
    assert!(p.collected_fees_unavailable);
    assert!(p.collected_fees.is_empty());
}

#[tokio::test]
async fn without_an_rpc_the_mirrored_collects_stay_withheld() {
    let cx = Ctx::new(
        Arc::new(snapshots_http(mirrored_snapshots())),
        Arc::new(MockClock(0)),
    );
    let h = history(&cx, None).await.expect("history");
    assert!(h.collected_fees_unavailable);
    assert!(h.events.iter().all(|e| e.kind != EventKind::CollectFees));
}

fn positions_body(liquidity: &str) -> String {
    format!(
        r#"{{"data":{{"positions":[{{
      "id":"12345","liquidity":"{liquidity}",
      "depositedToken0":"2.0","depositedToken1":"5000.0",
      "withdrawnToken0":"0.0","withdrawnToken1":"0.0",
      "collectedFeesToken0":"0.2","collectedFeesToken1":"0.2",
      "tickLower":{{"tickIdx":"-60"}},"tickUpper":{{"tickIdx":"60"}},
      "pool":{{"id":"0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640","tick":"0","sqrtPrice":"79228162514264337593543950336","feeTier":"3000",
        "token0":{{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"}},
        "token1":{{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}}}}]}}}}"#
    )
}

fn read_http(liquidity: &str) -> MockHttp {
    MockHttp::new()
        .on(
            Match::body_contains("positionSnapshots"),
            mirrored_snapshots(),
        )
        .on(Match::body_contains("positions"), positions_body(liquidity))
}

fn uncollected(rpc: MockChainProvider) -> MockChainProvider {
    let out = format!("0x{}{}", word("f4240"), word("f4240"));
    rpc.on(
        Match::method("eth_call"),
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{out}"}}"#),
    )
}

async fn read_one(cx: &Ctx) -> gluonscan_core::LiquidityPosition {
    let reading = adapter()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            cx,
        )
        .await
        .expect("read")
        .into_inner();
    let Position::Liquidity(p) = reading.positions.into_iter().next().expect("one position") else {
        panic!("expected a liquidity position");
    };
    p
}

#[tokio::test]
async fn an_open_position_reads_its_collected_totals_from_receipts() {
    let cx = Ctx::new(Arc::new(read_http("1000000000")), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(uncollected(receipts())));

    let p = read_one(&cx).await;

    assert!(!p.collected_fees_unavailable);
    let totals: Vec<_> = p
        .collected_fees
        .iter()
        .map(|a| (a.token.symbol.as_str(), a.amount))
        .collect();
    assert_eq!(totals, vec![("WETH", dec("0.2")), ("USDC", dec("350"))]);
}

#[tokio::test]
async fn a_failed_receipt_leaves_only_that_position_unavailable() {
    let rpc = uncollected(on_receipt(
        MockChainProvider::new(),
        "0xaaa",
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":429,"message":"rate limited"}}"#.to_string(),
    ));
    let cx =
        Ctx::new(Arc::new(read_http("1000000000")), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let p = read_one(&cx).await;

    assert!(p.collected_fees_unavailable);
    assert!(p.collected_fees.is_empty());
    assert!(
        !p.uncollected_fees.is_empty(),
        "the rest of the position is kept"
    );
}

#[tokio::test]
async fn a_closed_position_stays_unavailable_without_chain_calls() {
    let rpc = Arc::new(receipts());
    let cx = Ctx::new(Arc::new(read_http("0")), Arc::new(MockClock(0))).with_rpc(rpc.clone());

    let p = read_one(&cx).await;

    assert!(p.collected_fees_unavailable);
    assert!(
        rpc.calls().is_empty(),
        "closed positions cost no receipt reads"
    );
}
