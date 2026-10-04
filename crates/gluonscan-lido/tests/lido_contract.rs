//! Contract-based tests for the Lido adapter, entirely offline. The `eth_call` replies are real
//! `balanceOf` return words captured from Ethereum mainnet (2026-10-03): the stETH balance of the
//! wstETH contract (a large, deterministic holder) and the wstETH balance of the Aave V3 wstETH
//! aToken. A reply is routed to the right token by the `to` address in the request params.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_lido::Lido;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};

// Real captured return words (32-byte uint256, hex).
const STETH_BAL: &str = "0x00000000000000000000000000000000000000000003c747d65cb3717a72c5b1"; // 4_567_853.55 stETH
const WSTETH_BAL: &str = "0x00000000000000000000000000000000000000000000a3c36de28b700a2a2b12"; // ~773 wstETH
const ZERO_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn reply(result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
}

fn eth_call_to(fragment: &str) -> Match {
    Match::all([Match::method("eth_call"), Match::body_contains(fragment)])
}

async fn read_with(steth: &str, wsteth: &str) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), reply(steth)) // stETH `to`
        .on(eth_call_to("7f39c581f595b53c"), reply(wsteth)); // wstETH `to`
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Lido::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full, &cx)
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn returns_a_complete_stake_position_for_both_tokens() {
    let reading = read_with(STETH_BAL, WSTETH_BAL).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Lido);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };

    // Completeness: both staked legs carry a token with an address and a positive amount.
    assert_eq!(p.staked.len(), 2);
    assert_eq!(p.staked[0].token.symbol, "stETH");
    assert_eq!(p.staked[1].token.symbol, "wstETH");
    for leg in &p.staked {
        assert!(leg.token.address.is_some(), "staked leg must carry its token address");
        assert!(!leg.raw.is_zero(), "staked amount must be positive");
    }

    // Lido rewards are auto-compounded, never separately claimable.
    assert!(p.rewards.is_empty());

    // Both tokens are reported as receipts so the idle-wallet list drops them (no double count).
    assert_eq!(reading.receipt_tokens.len(), 2);
}

#[tokio::test]
async fn only_one_token_held_yields_one_leg() {
    let reading = read_with(STETH_BAL, ZERO_WORD).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.staked[0].token.symbol, "stETH");
    assert_eq!(reading.receipt_tokens, vec![
        "0xae7ab96520de3a18e5e111b5eaab095312d7fe84".parse::<Address>().unwrap()
    ]);
}

#[tokio::test]
async fn no_balance_is_an_empty_reading_not_an_error() {
    let reading = read_with(ZERO_WORD, ZERO_WORD).await.expect("read");
    assert!(reading.positions.is_empty());
    assert!(reading.receipt_tokens.is_empty());
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_call"),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#,
    );
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let err = Lido::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Ethereum, Detail::Full, &cx)
        .await;
    assert!(err.is_err(), "an RPC error must fail closed, not produce a partial position");
}

#[tokio::test]
async fn unsupported_chain_fails_closed() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = Lido::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await;
    assert!(err.is_err(), "Lido is Ethereum-only; other chains must fail closed");
}
