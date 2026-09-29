//! Contract-based test: Kamino obligations across markets (MockHttp GET). One market holds an
//! obligation; the others return empty. Also checks the Solana-wallet requirement.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_kamino::KaminoApi;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const MAIN_MARKET: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";
const WALLET: &str = "So11111111111111111111111111111111111111112";

const OBLIGATIONS: &str = r#"[{
  "obligationAddress":"obl1",
  "deposits":[{"symbol":"SOL","decimals":9,"amount":"5.0","usdValue":"750.00"}],
  "borrows":[{"symbol":"USDC","decimals":6,"amount":"200.0","usdValue":"200.00"}],
  "refreshedStats":{"netAccountValue":"550.00","healthFactor":"1.85"}
}]"#;

fn ctx() -> Ctx {
    let http = MockHttp::new()
        .on(Match::primary_contains(MAIN_MARKET), OBLIGATIONS)
        .on(Match::primary_contains("kamino-market"), "[]"); // JLP + Altcoins: empty
    Ctx::new(Arc::new(http), Arc::new(MockClock(0)))
}

#[tokio::test]
async fn reads_obligation_across_markets() {
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
    assert_eq!(reading.chain, Chain::Solana);
    assert_eq!(reading.positions.len(), 1); // only the Main market holds a position

    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert_eq!(p.supplied[0].token.symbol, "SOL");
    assert_eq!(
        p.supplied[0].amount,
        Decimal::from_str_exact("5.0").unwrap()
    );
    assert_eq!(p.borrowed[0].token.symbol, "USDC");
    assert_eq!(
        p.health_factor,
        Some(Decimal::from_str_exact("1.85").unwrap())
    );
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

#[tokio::test]
async fn rejects_evm_wallet() {
    let err = KaminoApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Solana,
            Detail::Full,
            &ctx(),
        )
        .await
        .expect_err("a Solana wallet is required");
    assert!(!err.is_retryable());
}
