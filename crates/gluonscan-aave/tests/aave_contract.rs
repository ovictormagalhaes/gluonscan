//! Contract-based tests: the Aave adapter is exercised entirely offline against a recorded
//! request/response contract (no network, no keys) — the default test path, so fork PRs run it
//! without secrets.

use std::str::FromStr;
use std::sync::Arc;

use gluonscan_aave::AaveApi;
use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Source, Wallet,
};
use gluonscan_testing::{load_contracts, MockClock, MockHttp};
use rust_decimal::Decimal;

fn ctx() -> Ctx {
    let dir = format!("{}/tests/contracts", env!("CARGO_MANIFEST_DIR"));
    let http = MockHttp::from_contracts(load_contracts(&dir).expect("load contracts"));
    Ctx::new(Arc::new(http), Arc::new(MockClock(0)))
}

#[tokio::test]
async fn aave_ethereum_reads_from_contract() {
    let reading = AaveApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &ctx(),
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::AaveV3);
    assert_eq!(reading.chain, Chain::Ethereum);
    assert_eq!(reading.source, Source::Api);

    let Position::Lending(pos) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    assert_eq!(pos.supplied[0].token.symbol, "WETH");
    assert_eq!(pos.supplied[0].amount, Decimal::from_str("1.5").unwrap());
    assert_eq!(pos.borrowed[0].token.symbol, "USDC");
    assert_eq!(pos.health_factor, Some(Decimal::from_str("2.35").unwrap()));
}

#[tokio::test]
async fn reads_on_every_supported_chain() {
    let adapter = AaveApi::new();
    let cx = ctx();
    for &chain in adapter.supported_chains() {
        let reading = adapter
            .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
            .await
            .unwrap_or_else(|e| panic!("{chain:?} should read: {e}"))
            .into_inner();
        assert_eq!(reading.chain, chain);
        assert!(matches!(reading.positions[0], Position::Lending(_)));
    }
}

#[tokio::test]
async fn only_supported_chains_bind() {
    let adapter = AaveApi::new();
    let cx = ctx();
    let supported = adapter.supported_chains();
    for &chain in Chain::ALL {
        if supported.contains(&chain) {
            continue;
        }
        let err = adapter
            .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
            .await
            .expect_err("unsupported chain must error, not return a zero reading");
        // A configuration mismatch is permanent, never retryable.
        assert!(!err.is_retryable(), "{chain:?} should be a permanent error");
    }
}
