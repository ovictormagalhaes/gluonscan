//! Contract-based tests for the Lido adapter, entirely offline. The `eth_call` replies are real
//! `balanceOf` return words captured from Ethereum mainnet (2026-10-03): the stETH balance of the
//! wstETH contract (a large, deterministic holder) and the wstETH balance of the Aave V3 wstETH
//! aToken. A reply is routed to the right token by the `to` address in the request params.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter, TokenAddress, Wallet,
    U256,
};
use gluonscan_lido::Lido;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const STETH_ADDR: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
const WSTETH_ADDR: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";

// Real captured return words (32-byte uint256, hex).
const STETH_BAL: &str = "0x00000000000000000000000000000000000000000003c747d65cb3717a72c5b1"; // 4,567,853.554182302240785841 stETH
const WSTETH_BAL: &str = "0x00000000000000000000000000000000000000000000a3c36de28b700a2a2b12"; // 773,350.769846533726284562 wstETH
const ZERO_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

fn ok(result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
}

const RPC_ERROR: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#;

fn eth_call_to(fragment: &str) -> Match {
    Match::all([Match::method("eth_call"), Match::body_contains(fragment)])
}

fn evm(addr: &str) -> Option<TokenAddress> {
    Some(TokenAddress::Evm(addr.parse().unwrap()))
}

async fn read_replies(steth: String, wsteth: String) -> Result<gluonscan_core::Reading, Error> {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), steth) // stETH `to`
        .on(eth_call_to("7f39c581f595b53c"), wsteth); // wstETH `to`
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

async fn read_with(steth: &str, wsteth: &str) -> Result<gluonscan_core::Reading, Error> {
    read_replies(ok(steth), ok(wsteth)).await
}

#[tokio::test]
async fn attaches_the_lido_apr_as_a_fraction() {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("ae7ab96520de3a18"), ok(STETH_BAL))
        .on(eth_call_to("7f39c581f595b53c"), ok(WSTETH_BAL));
    // The Lido API returns a percentage; the position carries it as a fraction.
    let http = MockHttp::new().on(
        Match::primary_contains("eth-api.lido.fi"),
        r#"{"data":{"timeUnix":1,"apr":2.216},"meta":{}}"#,
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let reading = Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    let apy = p.apy.expect("apr attached");
    assert!(
        apy > Decimal::from_str_exact("0.0221").unwrap()
            && apy < Decimal::from_str_exact("0.0222").unwrap(),
        "2.216% -> ~0.02216 fraction, got {apy}"
    );
}

#[tokio::test]
async fn apr_fetch_failure_leaves_apy_none_but_keeps_the_position() {
    // Best-effort: the APY is informational, so an API outage (here: unmocked GET) must never fail
    // the balance read — the position is still complete, just without an APY.
    let reading = read_with(STETH_BAL, WSTETH_BAL).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert!(p.apy.is_none());
    assert_eq!(p.staked.len(), 2);
}

#[tokio::test]
async fn returns_a_complete_stake_position_for_both_tokens() {
    let reading = read_with(STETH_BAL, WSTETH_BAL).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Lido);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 2);

    // stETH leg: pin identity AND the exact decimal amount. Asserting the amount (not just a
    // nonzero raw) is what catches a wrong `decimals` scaling or a balance<->token mix-up.
    let steth = &p.staked[0];
    assert_eq!(steth.token.symbol, "stETH");
    assert_eq!(steth.token.address, evm(STETH_ADDR));
    assert_eq!(
        steth.raw,
        U256::from_str_radix("4567853554182302240785841", 10).unwrap()
    );
    assert_eq!(steth.amount.to_string(), "4567853.554182302240785841");

    // wstETH leg: pin the wstETH address value (a wrong constant would otherwise slip through) and
    // its own distinct amount.
    let wsteth = &p.staked[1];
    assert_eq!(wsteth.token.symbol, "wstETH");
    assert_eq!(wsteth.token.address, evm(WSTETH_ADDR));
    assert_eq!(
        wsteth.raw,
        U256::from_str_radix("773350769846533726284562", 10).unwrap()
    );
    assert_eq!(wsteth.amount.to_string(), "773350.769846533726284562");

    // Lido rewards auto-compound into the balance, never separately claimable.
    assert!(p.rewards.is_empty());

    // Both tokens are reported as receipts, in order, so the idle-wallet list drops them.
    let receipts: Vec<String> = reading
        .receipt_tokens
        .iter()
        .map(|a| format!("{a:#x}"))
        .collect();
    assert_eq!(
        receipts,
        vec![STETH_ADDR.to_string(), WSTETH_ADDR.to_string()]
    );
}

#[tokio::test]
async fn only_one_token_held_yields_one_leg() {
    let reading = read_with(STETH_BAL, ZERO_WORD).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.staked[0].token.symbol, "stETH");
    assert_eq!(p.staked[0].amount.to_string(), "4567853.554182302240785841");
    assert_eq!(
        reading.receipt_tokens,
        vec![STETH_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn no_balance_is_an_empty_reading_not_an_error() {
    let reading = read_with(ZERO_WORD, ZERO_WORD).await.expect("read");
    assert!(reading.positions.is_empty());
    assert!(reading.receipt_tokens.is_empty());
}

#[tokio::test]
async fn second_read_failing_fails_closed_no_partial_position() {
    // The stETH read succeeds but the wstETH read errors. A balance without its paired read is
    // partial data, so the whole reading must fail closed — never emit the stETH-only position.
    let out = read_replies(ok(STETH_BAL), RPC_ERROR.to_string()).await;
    assert!(
        out.is_err(),
        "a failed second read must fail the whole reading, not emit a partial one"
    );
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), RPC_ERROR);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let out = Lido::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await;
    assert!(
        out.is_err(),
        "an RPC error must fail closed, not produce a partial position"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed_permanently() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = Lido::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("Lido is Ethereum-only; other chains must fail closed");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error, not retryable"
    );
}
