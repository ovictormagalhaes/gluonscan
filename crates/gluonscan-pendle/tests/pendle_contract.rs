//! Contract-based test: Pendle market catalog (MockHttp GET) + per-token `balanceOf`
//! (MockChainProvider, filtered by token address) — one token held, one at zero.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Timestamp, Wallet, YieldKind,
};
use gluonscan_pendle::PendleApi;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const CATALOG: &str = r#"{"markets":[{
  "expiry":1766620800,
  "impliedApy":"0.072",
  "pt":{"address":"0x1111111111111111111111111111111111111111","symbol":"PT-stETH-DEC25","decimals":18,"price":{"usd":"0.98"}},
  "yt":{"address":"0x2222222222222222222222222222222222222222","symbol":"YT-stETH-DEC25","decimals":18,"price":{"usd":"0.05"}}
}]}"#;

#[tokio::test]
async fn reads_held_pt_prices_it_and_skips_zero_yt() {
    let held = format!("0x{:0>64}", "de0b6b3a7640000"); // 1e18 raw = 1.0 at 18 decimals
    let zero = format!("0x{:0>64}", "0");
    let pt_reply = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{held}"}}"#);
    let yt_reply = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{zero}"}}"#);

    let http = MockHttp::new().on(Match::primary_contains("markets"), CATALOG);
    let rpc = MockChainProvider::new()
        .on(
            Match::all([
                Match::method("eth_call"),
                Match::body_contains("1111111111111111111111111111111111111111"),
            ]),
            pt_reply,
        )
        .on(
            Match::all([
                Match::method("eth_call"),
                Match::body_contains("2222222222222222222222222222222222222222"),
            ]),
            yt_reply,
        );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = PendleApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Pendle);
    assert_eq!(reading.positions.len(), 1); // the zero-balance YT is not a position

    let Position::Yield(y) = &reading.positions[0] else {
        panic!("expected a yield position");
    };
    assert_eq!(y.kind, YieldKind::PrincipalToken);
    assert_eq!(y.amount.token.symbol, "PT-stETH-DEC25");
    assert_eq!(
        y.amount.amount,
        Decimal::from_i128_with_scale(1_000_000_000_000_000_000, 18)
    );
    assert_eq!(y.expiry, Some(Timestamp(1766620800)));
    assert_eq!(y.apy, Some(Decimal::from_str_exact("0.072").unwrap()));
    let usd = y.amount.usd.as_ref().expect("priced from catalog");
    assert_eq!(usd.amount, Decimal::from_str_exact("0.98").unwrap());
}

#[tokio::test]
async fn unsupported_chain_errors() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = PendleApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("pendle is EVM-only");
    assert!(!err.is_retryable());
}
