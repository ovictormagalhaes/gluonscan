//! Contract-based test: a Kamino obligation joined with reserves/metrics + on-chain mint decimals.
//! USD comes from the scaled-fraction `marketValueSf` (/ 2^60); amount from raw / 10^decimals.

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

// Per-asset legs live under the decoded on-chain `state`; the top-level deposits/borrows are empty
// aggregation maps. marketValueSf = usd * 2^60 ($750, $200). Deposits carry a plain base-unit
// `depositedAmount`; borrows carry `borrowedAmountSf` = base-units * 2^60 (200000000 * 2^60).
const OBLIGATIONS: &str = r#"[{
  "obligationAddress":"obl1",
  "deposits":{},
  "borrows":{},
  "state":{
    "deposits":[{"depositReserve":"RES_SOL","depositedAmount":"5000000000","marketValueSf":"864691128455135232000"}],
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

fn ctx() -> Ctx {
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), OBLIGATIONS)
        .on(Match::primary_contains("kamino-market"), "[]"); // JLP + Altcoins: empty
    let rpc = MockChainProvider::new()
        .on(Match::body_contains("MINT_SOL"), mint_account(9))
        .on(Match::body_contains("MINT_USDC"), mint_account(6));
    Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc))
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
    // HF = borrowLiquidationLimit / borrowFactorAdjustedDebt = 370 / 200 = 1.85.
    assert_eq!(
        p.health_factor,
        Some(Decimal::from_str_exact("1.85").unwrap())
    );

    let sol = &p.supplied[0];
    assert_eq!(sol.amount.token.symbol, "SOL");
    assert_eq!(sol.amount.token.decimals, 9);
    assert_eq!(sol.amount.amount, Decimal::from_str_exact("5").unwrap());
    assert_eq!(
        sol.amount.usd.as_ref().unwrap().amount,
        Decimal::from_str_exact("750").unwrap()
    );
    assert_eq!(sol.max_ltv, Some(Decimal::from_str_exact("0.7").unwrap()));
    assert_eq!(sol.apy, Some(Decimal::from_str_exact("0.045").unwrap()));
    assert!(sol.is_collateral);

    let usdc = &p.borrowed[0];
    assert_eq!(usdc.amount.token.symbol, "USDC");
    assert_eq!(usdc.amount.amount, Decimal::from_str_exact("200").unwrap());
    assert_eq!(
        usdc.amount.usd.as_ref().unwrap().amount,
        Decimal::from_str_exact("200").unwrap()
    );
    assert_eq!(usdc.apy, Some(Decimal::from_str_exact("0.089").unwrap()));
}

// Same obligation as OBLIGATIONS but with the `refreshedStats` block removed, so neither
// `borrowLiquidationLimit` nor `userTotalBorrowBorrowFactorAdjusted` is present and no health factor
// can be derived — while a non-zero borrow leg is still present.
const OBLIGATIONS_DEBT_NO_HF: &str = r#"[{
  "obligationAddress":"obl1",
  "deposits":{},
  "borrows":{},
  "state":{
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
    let http = MockHttp::new()
        .on(Match::primary_contains("/reserves/metrics"), RESERVES)
        .on(Match::primary_contains(MAIN_MARKET), OBLIGATIONS_DEBT_NO_HF)
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
            Some(&format!("obl1|{MAIN_MARKET}")),
            None,
            &cx,
        )
        .await
        .expect("read_history")
        .into_inner();

    assert_eq!(history.protocol, Protocol::Kamino);
    assert_eq!(history.events.len(), 2, "one SOL deposit delta + one USDC borrow delta");

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
        .read_history(&Wallet::Solana(WALLET.to_string()), Chain::Solana, None, None, &cx)
        .await
        .expect_err("Kamino history needs obligation|market");
    assert!(!err.is_retryable());
}
