//! Collect provenance. A V3 `Collect` sends out everything the position is owed: its fees plus the
//! principal a `decreaseLiquidity` released. The receipt rebuild sums those `Collect` logs, so its
//! amounts include principal; a subgraph delta holds whatever the deployment's mapping computes.
//! Every collect event and every collected total says which ([`CollectBasis`]), so a consumer never
//! reads principal as fees.
//!
//! The fixture is a real Ethereum position (SPION/WETH, token id 999524) captured on 2026-11-10 from
//! the Uniswap V3 Ethereum subgraph (`position`, `positionSnapshots`) and `eth_getTransactionReceipt`,
//! with each receipt's logs kept for the position manager only. Its collected series is mirrored
//! (token1 reads token0's amount), so the adapter rebuilds the collects from the receipts. Tx
//! `0x336a…` decreased liquidity and collected in one transaction: 91.556866850008555367 SPION and
//! 0.02582884869063955 WETH went out, of which 32.281040390224433303 SPION and
//! 0.009518101693693813 WETH were principal.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, CollectBasis, Ctx, Detail, EventKind, History, HistoryEvent, LiquidityPosition,
    Position, ProtocolAdapter, Wallet,
};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_uniswap::UniswapV3;
use rust_decimal::Decimal;

const FIXTURE: &str = include_str!("fixtures/uniswap_eth_999524.json");
const DECREASE_AND_COLLECT_TX: &str =
    "0x336adcba947e3e0fc3419c62504a80e63046f5d958af09a70499a8152de039ea";
const COLLECT_ONLY_TX: &str = "0x85ef7c611e6895fd5fc14f2c1b1dcb5e4f0669f4199904e9ac18c944fdc1df8e";

fn dec(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("fixture is JSON")
}

fn word(hex: &str) -> String {
    format!("{hex:0>64}")
}

/// The real position's subgraph replies and receipts, plus a zero uncollected-fees `eth_call`.
fn real_cx() -> Ctx {
    let f = fixture();
    let positions = serde_json::json!({ "data": { "positions": [f["position"]] } }).to_string();
    let http = MockHttp::new()
        .on(
            Match::body_contains("positionSnapshots"),
            f["snapshots"].to_string(),
        )
        .on(Match::body_contains("positions"), positions);
    let mut rpc = MockChainProvider::new().on(
        Match::method("eth_call"),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{}{}"}}"#,
            word("0"),
            word("0")
        ),
    );
    for (tx, receipt) in f["receipts"].as_object().expect("receipts") {
        rpc = rpc.on(
            Match::all([
                Match::method("eth_getTransactionReceipt"),
                Match::body_contains(tx),
            ]),
            receipt.to_string(),
        );
    }
    Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc))
}

async fn history_with(adapter: UniswapV3, cx: &Ctx, id: &str) -> History {
    adapter
        .read_history(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Some(id),
            None,
            cx,
        )
        .await
        .expect("history")
        .into_inner()
}

async fn read_with(adapter: UniswapV3, cx: &Ctx, detail: Detail) -> LiquidityPosition {
    let reading = adapter
        .read(&Wallet::Evm(Address::ZERO), Chain::Ethereum, detail, cx)
        .await
        .expect("read")
        .into_inner();
    let Position::Liquidity(p) = reading.positions.into_iter().next().expect("one position") else {
        panic!("expected a liquidity position");
    };
    p
}

fn adapter() -> UniswapV3 {
    UniswapV3::new().with_subgraph(Chain::Ethereum, "https://subgraph.test/uniswap-v3")
}

fn collect<'a>(h: &'a History, tx: &str, symbol: &str) -> &'a HistoryEvent {
    h.events
        .iter()
        .find(|e| e.kind == EventKind::CollectFees && e.tx == tx && e.amount.token.symbol == symbol)
        .unwrap_or_else(|| panic!("a {symbol} collect in {tx}"))
}

fn released(e: &HistoryEvent) -> Decimal {
    e.released_principal
        .as_ref()
        .expect("a rebuilt collect knows its tx's released principal")
        .amount
}

#[tokio::test]
async fn a_rebuilt_collect_says_it_includes_the_principal_its_tx_released() {
    let h = history_with(adapter(), &real_cx(), "999524").await;
    assert!(!h.collected_fees_unavailable, "the mirror was rebuilt");

    let spion = collect(&h, DECREASE_AND_COLLECT_TX, "SPION");
    assert_eq!(spion.amount.amount, dec("91.556866850008555367"));
    assert_eq!(spion.collect_basis, CollectBasis::IncludesPrincipal);
    assert_eq!(released(spion), dec("32.281040390224433303"));
    assert_eq!(
        spion.amount.amount - released(spion),
        dec("59.275826459784122064"),
        "the fees are what the collect sent out less the principal its tx released"
    );

    let weth = collect(&h, DECREASE_AND_COLLECT_TX, "WETH");
    assert_eq!(weth.amount.amount, dec("0.02582884869063955"));
    assert_eq!(weth.collect_basis, CollectBasis::IncludesPrincipal);
    assert_eq!(released(weth), dec("0.009518101693693813"));
    assert_eq!(
        weth.released_principal.as_ref().unwrap().token,
        weth.amount.token,
        "the principal is in the event's own token"
    );
}

#[tokio::test]
async fn a_rebuilt_collect_whose_tx_decreased_nothing_released_no_principal() {
    let h = history_with(adapter(), &real_cx(), "999524").await;

    for symbol in ["SPION", "WETH"] {
        let e = collect(&h, COLLECT_ONLY_TX, symbol);
        assert_eq!(e.collect_basis, CollectBasis::IncludesPrincipal);
        assert_eq!(released(e), Decimal::ZERO, "{symbol}");
    }
}

#[tokio::test]
async fn a_rebuilt_stream_never_claims_fees_only() {
    let h = history_with(adapter(), &real_cx(), "999524").await;

    let collects: Vec<_> = h
        .events
        .iter()
        .filter(|e| e.kind == EventKind::CollectFees)
        .collect();
    assert_eq!(collects.len(), 6, "three collect txs, two tokens each");
    assert!(collects
        .iter()
        .all(|e| e.collect_basis == CollectBasis::IncludesPrincipal));
    assert!(h
        .events
        .iter()
        .filter(|e| e.kind != EventKind::CollectFees)
        .all(|e| e.released_principal.is_none()));
    assert_eq!(h.collected_fees_basis(), CollectBasis::IncludesPrincipal);
}

#[tokio::test]
async fn an_open_position_rebuilt_from_receipts_says_its_totals_include_principal() {
    let p = read_with(adapter(), &real_cx(), Detail::Full).await;

    assert!(!p.collected_fees_unavailable);
    let totals: Vec<_> = p
        .collected_fees
        .iter()
        .map(|a| (a.token.symbol.as_str(), a.amount))
        .collect();
    assert_eq!(
        totals,
        vec![
            ("SPION", dec("111.185674682988434603")),
            ("WETH", dec("0.123679750350794327")),
        ]
    );
    assert_eq!(p.collected_fees_basis, CollectBasis::IncludesPrincipal);
}

// Synthetic subgraph streams, not mirrored, so no receipt is read.

const POOL: &str = r#""pool":{"token0":{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"},"token1":{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}"#;

fn snapshot(ts: u32, wd1: &str, col0: &str, col1: &str, tx: &str) -> String {
    format!(
        r#"{{"timestamp":"{ts}","depositedToken0":"2.0","depositedToken1":"5000.0","withdrawnToken0":"0.0","withdrawnToken1":"{wd1}","collectedFeesToken0":"{col0}","collectedFeesToken1":"{col1}","transaction":{{"id":"{tx}"}},{POOL}}}"#
    )
}

/// Creation, a collect of 0.1 WETH + 30 USDC, then a partial withdraw of 3,000 USDC collected in the
/// same tx with 0.05 WETH + 20 USDC more (the subgraph delta: 3,020 USDC on a mapping that includes
/// principal, 20 on one that does not; this fixture's numbers are the latter's).
fn healthy_snapshots(wd1_after_withdraw: &str) -> String {
    format!(
        r#"{{"data":{{"positionSnapshots":[{},{},{}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0.0", "0xaaa"),
        snapshot(2000, "0.0", "0.1", "30.0", "0xbbb"),
        snapshot(3000, wd1_after_withdraw, "0.15", "50.0", "0xccc"),
    )
}

fn subgraph_cx(snapshots: String) -> Ctx {
    Ctx::new(
        Arc::new(MockHttp::new().on(Match::body_contains("positionSnapshots"), snapshots)),
        Arc::new(MockClock(0)),
    )
}

#[tokio::test]
async fn a_subgraph_stream_that_never_withdrew_is_fees_only() {
    let h = history_with(adapter(), &subgraph_cx(healthy_snapshots("0.0")), "12345").await;

    let collects: Vec<_> = h
        .events
        .iter()
        .filter(|e| e.kind == EventKind::CollectFees)
        .collect();
    assert_eq!(collects.len(), 4);
    assert!(collects
        .iter()
        .all(|e| e.collect_basis == CollectBasis::FeesOnly && e.released_principal.is_none()));
    assert_eq!(h.collected_fees_basis(), CollectBasis::FeesOnly);
}

#[tokio::test]
async fn an_undeclared_subgraph_delta_after_a_withdraw_is_unknown() {
    let h = history_with(
        adapter(),
        &subgraph_cx(healthy_snapshots("3000.0")),
        "12345",
    )
    .await;

    // Token0 was never withdrawn, so no principal can be in its collects on any mapping.
    assert_eq!(
        collect(&h, "0xccc", "WETH").collect_basis,
        CollectBasis::FeesOnly
    );
    assert_eq!(
        collect(&h, "0xbbb", "USDC").collect_basis,
        CollectBasis::FeesOnly
    );
    assert_eq!(
        collect(&h, "0xccc", "USDC").collect_basis,
        CollectBasis::Unknown,
        "whether the mapping subtracted the 3,000 USDC withdrawn is not knowable from the stream"
    );
    assert_eq!(h.collected_fees_basis(), CollectBasis::Unknown);
}

#[tokio::test]
async fn a_declared_fees_only_deployment_marks_its_deltas_fees_only() {
    let adapter = adapter().with_subgraph_collect_basis(Chain::Ethereum, CollectBasis::FeesOnly);
    let h = history_with(adapter, &subgraph_cx(healthy_snapshots("3000.0")), "12345").await;

    assert_eq!(
        collect(&h, "0xccc", "USDC").collect_basis,
        CollectBasis::FeesOnly
    );
    assert_eq!(h.collected_fees_basis(), CollectBasis::FeesOnly);
}

#[tokio::test]
async fn a_mixed_stream_is_marked_by_its_principal_including_collects() {
    let adapter =
        adapter().with_subgraph_collect_basis(Chain::Ethereum, CollectBasis::IncludesPrincipal);
    let h = history_with(adapter, &subgraph_cx(healthy_snapshots("3000.0")), "12345").await;

    assert_eq!(
        collect(&h, "0xbbb", "USDC").collect_basis,
        CollectBasis::FeesOnly
    );
    let usdc = collect(&h, "0xccc", "USDC");
    assert_eq!(usdc.collect_basis, CollectBasis::IncludesPrincipal);
    assert!(
        usdc.released_principal.is_none(),
        "a subgraph delta does not split out its principal"
    );
    assert_eq!(h.collected_fees_basis(), CollectBasis::IncludesPrincipal);
}

#[tokio::test]
async fn a_declaration_for_another_chain_does_not_apply() {
    let adapter = adapter().with_subgraph_collect_basis(Chain::Base, CollectBasis::FeesOnly);
    let h = history_with(adapter, &subgraph_cx(healthy_snapshots("3000.0")), "12345").await;

    assert_eq!(
        collect(&h, "0xccc", "USDC").collect_basis,
        CollectBasis::Unknown
    );
}

#[tokio::test]
async fn a_withheld_history_has_an_unknown_basis() {
    let mirrored = format!(
        r#"{{"data":{{"positionSnapshots":[{},{}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0.0", "0xaaa"),
        snapshot(2000, "0.0", "0.1", "0.1", "0xbbb"),
    );
    // No RPC: the mirrored collects cannot be rebuilt.
    let h = history_with(adapter(), &subgraph_cx(mirrored), "12345").await;

    assert!(h.collected_fees_unavailable);
    assert_eq!(h.collected_fees_basis(), CollectBasis::Unknown);
}

fn positions_body(withdrawn0: &str, withdrawn1: &str, col0: &str, col1: &str) -> String {
    format!(
        r#"{{"data":{{"positions":[{{
      "id":"12345","liquidity":"1000000000",
      "depositedToken0":"2.0","depositedToken1":"5000.0",
      "withdrawnToken0":"{withdrawn0}","withdrawnToken1":"{withdrawn1}",
      "collectedFeesToken0":"{col0}","collectedFeesToken1":"{col1}",
      "tickLower":{{"tickIdx":"-60"}},"tickUpper":{{"tickIdx":"60"}},
      "pool":{{"id":"0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640","tick":"0","sqrtPrice":"79228162514264337593543950336","feeTier":"3000",
        "token0":{{"id":"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2","symbol":"WETH","decimals":"18"}},
        "token1":{{"id":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","symbol":"USDC","decimals":"6"}}}}}}]}}}}"#
    )
}

fn positions_cx(body: String) -> Ctx {
    Ctx::new(
        Arc::new(MockHttp::new().on(Match::body_contains("positions"), body)),
        Arc::new(MockClock(0)),
    )
}

#[tokio::test]
async fn subgraph_totals_without_withdrawals_are_fees_only() {
    let cx = positions_cx(positions_body("0.0", "0.0", "0.15", "50.0"));
    let p = read_with(adapter(), &cx, Detail::Summary).await;

    assert_eq!(p.collected_fees.len(), 2);
    assert_eq!(p.collected_fees_basis, CollectBasis::FeesOnly);
}

#[tokio::test]
async fn undeclared_subgraph_totals_after_a_withdraw_are_unknown() {
    let cx = positions_cx(positions_body("0.0", "3000.0", "0.15", "50.0"));
    let p = read_with(adapter(), &cx, Detail::Summary).await;

    assert_eq!(p.collected_fees.len(), 2, "the totals are still reported");
    assert_eq!(
        p.collected_fees_basis,
        CollectBasis::Unknown,
        "a fees-only token0 does not make the pair fees-only"
    );
}

#[tokio::test]
async fn declared_subgraph_totals_carry_the_declared_basis() {
    let cx = positions_cx(positions_body("0.0", "3000.0", "0.15", "3050.0"));
    let adapter =
        adapter().with_subgraph_collect_basis(Chain::Ethereum, CollectBasis::IncludesPrincipal);
    let p = read_with(adapter, &cx, Detail::Summary).await;

    assert_eq!(p.collected_fees_basis, CollectBasis::IncludesPrincipal);

    let cx = positions_cx(positions_body("0.0", "3000.0", "0.15", "50.0"));
    let adapter =
        self::adapter().with_subgraph_collect_basis(Chain::Ethereum, CollectBasis::FeesOnly);
    let p = read_with(adapter, &cx, Detail::Summary).await;

    assert_eq!(p.collected_fees_basis, CollectBasis::FeesOnly);
}

#[tokio::test]
async fn withheld_totals_have_an_unknown_basis() {
    // Mirrored and closed: withheld without chain calls.
    let body = positions_body("2.0", "5000.0", "2.1", "2.1").replace("1000000000", "0");
    let p = read_with(adapter(), &positions_cx(body), Detail::Full).await;

    assert!(p.collected_fees_unavailable);
    assert_eq!(p.collected_fees_basis, CollectBasis::Unknown);
}

const MANAGER: &str = "0xC36442b4a4522E871399CD717aBDD847Ab11FE88";
const COLLECT_TOPIC: &str = "0x40d0efd1a53d60ecbf40971b9daf7dc90178c3aadc7aab1765632738fa8b8f01";
const DECREASE_TOPIC: &str = "0x26f6a048ee9138f2c0ce266f322cb99228e8d619ae2bff30c67f8dcf9d2377b4";

fn amounts_log(address: &str, topic: &str, token_id: u64, amount0: u128, amount1: u128) -> String {
    format!(
        r#"{{"address":"{address}","topics":["{topic}","0x{}"],"data":"0x{}{}{}"}}"#,
        word(&format!("{token_id:x}")),
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

// Only the position's own DecreaseLiquidity, from the position manager, counts as released
// principal: another token id's and another contract's same-signature logs are ignored.
#[tokio::test]
async fn released_principal_counts_only_the_positions_own_decreases() {
    let mirrored = format!(
        r#"{{"data":{{"positionSnapshots":[{},{}]}}}}"#,
        snapshot(1000, "0.0", "0.0", "0.0", "0xaaa"),
        snapshot(2000, "3000.0", "0.1", "0.1", "0xbbb"),
    );
    let on = |rpc: MockChainProvider, tx: &str, body: String| {
        rpc.on(
            Match::all([
                Match::method("eth_getTransactionReceipt"),
                Match::body_contains(tx),
            ]),
            body,
        )
    };
    let rpc = on(MockChainProvider::new(), "0xaaa", receipt(&[]));
    let rpc = on(
        rpc,
        "0xbbb",
        receipt(&[
            amounts_log(MANAGER, DECREASE_TOPIC, 12345, 0, 3_000_000_000),
            amounts_log(MANAGER, DECREASE_TOPIC, 999, 7, 7),
            amounts_log(
                "0x0000000000000000000000000000000000000001",
                DECREASE_TOPIC,
                12345,
                7,
                7,
            ),
            amounts_log(
                MANAGER,
                COLLECT_TOPIC,
                12345,
                100_000_000_000_000_000,
                3_020_000_000,
            ),
        ]),
    );
    let cx = subgraph_cx(mirrored).with_rpc(Arc::new(rpc));

    let h = history_with(adapter(), &cx, "12345").await;

    let usdc = collect(&h, "0xbbb", "USDC");
    assert_eq!(usdc.amount.amount, dec("3020"));
    assert_eq!(usdc.collect_basis, CollectBasis::IncludesPrincipal);
    assert_eq!(released(usdc), dec("3000"));
    assert_eq!(released(collect(&h, "0xbbb", "WETH")), Decimal::ZERO);
}
