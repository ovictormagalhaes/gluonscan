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
    let weth = &pos.supplied[0];
    assert_eq!(weth.amount.token.symbol, "WETH");
    assert_eq!(weth.amount.amount, Decimal::from_str("1.5").unwrap());
    assert_eq!(
        weth.liquidation_threshold,
        Some(Decimal::from_str("0.83").unwrap())
    );
    assert_eq!(weth.max_ltv, Some(Decimal::from_str("0.805").unwrap()));
    assert!(weth.is_collateral && weth.can_be_collateral);
    assert_eq!(weth.apy, Some(Decimal::from_str("0.021").unwrap()));

    let usdc = &pos.borrowed[0];
    assert_eq!(usdc.amount.token.symbol, "USDC");
    assert_eq!(usdc.borrow_factor, Some(Decimal::from_str("1").unwrap()));
    assert_eq!(usdc.apy, Some(Decimal::from_str("0.055").unwrap()));
    assert_eq!(pos.health_factor, Some(Decimal::from_str("2.35").unwrap()));
}

// Completeness guard (mirror of the Uniswap one): every supplied/borrowed leg must carry a token
// ADDRESS (not just a symbol), Full detail must populate the risk params the consumer's HF /
// liquidation math reads, and a position carrying debt must expose a health factor. A consumer
// resolves per-token prices by address, so an address-less leg silently breaks downstream.
#[tokio::test]
async fn lending_legs_carry_addresses_risk_params_and_health_factor() {
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

    let Position::Lending(pos) = &reading.positions[0] else {
        panic!("expected a lending position");
    };

    assert!(!pos.supplied.is_empty(), "fixture has a supplied leg");
    for s in &pos.supplied {
        assert!(
            !s.amount.token.symbol.is_empty(),
            "supplied symbol required"
        );
        let addr = s
            .amount
            .token
            .address
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default();
        assert!(
            !addr.is_empty(),
            "supplied leg must carry a token address, got {:?}",
            s.amount.token.address
        );
        assert!(
            s.liquidation_threshold.is_some(),
            "Full detail must populate liquidation_threshold"
        );
        assert!(s.max_ltv.is_some(), "Full detail must populate max_ltv");
        assert!(s.apy.is_some(), "supplied apy required");
    }
    for b in &pos.borrowed {
        let addr = b
            .amount
            .token
            .address
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default();
        assert!(!addr.is_empty(), "borrowed leg must carry a token address");
        assert_eq!(
            b.borrow_factor,
            Some(Decimal::from_str("1").unwrap()),
            "Aave V3 pins borrow_factor to 1.0"
        );
        assert!(b.apy.is_some(), "borrowed apy required");
    }
    if !pos.borrowed.is_empty() {
        assert!(
            pos.health_factor.is_some(),
            "a position with debt must expose a health factor"
        );
    }
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

// The position carries the chain's Aave V3 Pool address as its market id: a stable identity that
// does not move when the account's asset mix or a reserve's risk parameters change, so consumers can
// key persisted series on it.
#[tokio::test]
async fn position_carries_the_pool_address_as_market_id() {
    let adapter = AaveApi::new();
    let cx = ctx();
    let expected = [
        (
            Chain::Ethereum,
            "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2",
        ),
        (Chain::Base, "0xa238dd80c259a72e81d7e4664a9801593f98d1c5"),
        (
            Chain::Arbitrum,
            "0x794a61358d6845594f94dc1db02a252b5b4814ad",
        ),
        (
            Chain::Optimism,
            "0x794a61358d6845594f94dc1db02a252b5b4814ad",
        ),
        (Chain::Polygon, "0x794a61358d6845594f94dc1db02a252b5b4814ad"),
        (Chain::Bnb, "0x6807dc923806fe8fd134338eabca509979a7e0cb"),
    ];
    for (chain, pool) in expected {
        let reading = adapter
            .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
            .await
            .unwrap_or_else(|e| panic!("{chain:?} should read: {e}"))
            .into_inner();
        let Position::Lending(pos) = &reading.positions[0] else {
            panic!("expected a lending position");
        };
        assert_eq!(pos.market_id.as_deref(), Some(pool), "{chain:?}");
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
