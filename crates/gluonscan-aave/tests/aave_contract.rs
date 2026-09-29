//! Contract-based test: the Aave adapter is exercised entirely offline against a recorded
//! request/response contract (no network, no keys). This is the default test path — fork PRs run
//! it with no secrets.

use std::str::FromStr;
use std::sync::Arc;

use gluonscan_aave::AaveApi;
use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Source};
use gluonscan_testing::{load_contracts, MockClock, MockHttp};
use rust_decimal::Decimal;

#[tokio::test]
async fn aave_ethereum_reads_from_contract() {
    let dir = format!("{}/tests/contracts", env!("CARGO_MANIFEST_DIR"));
    let http = MockHttp::from_contracts(load_contracts(&dir).expect("load contracts"));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    let adapter = AaveApi::new();
    assert!(adapter.supported_chains().contains(&Chain::Ethereum));
    assert_eq!(adapter.source(), Source::Api);

    let reading = adapter
        .read(Address::ZERO, Chain::Ethereum, Detail::Full, &cx)
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::AaveV3);
    assert_eq!(reading.chain, Chain::Ethereum);

    let Position::Lending(pos) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert_eq!(pos.supplied.len(), 1);
    assert_eq!(pos.supplied[0].token.symbol, "WETH");
    assert_eq!(pos.supplied[0].amount, Decimal::from_str("1.5").unwrap());
    assert_eq!(pos.borrowed.len(), 1);
    assert_eq!(pos.borrowed[0].token.symbol, "USDC");
    assert_eq!(pos.health_factor, Some(Decimal::from_str("2.35").unwrap()));
}

#[tokio::test]
async fn unsupported_chain_is_a_config_error_not_a_zero() {
    let http = MockHttp::new(); // no contracts; must not even be called
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let err = AaveApi::new()
        .read(Address::ZERO, Chain::Solana, Detail::Full, &cx)
        .await
        .expect_err("Aave on Solana must error");
    assert!(!err.is_retryable());
}
