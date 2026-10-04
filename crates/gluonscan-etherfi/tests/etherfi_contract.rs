//! Contract-based tests for the ether.fi adapter, entirely offline. The `eth_call` replies are real
//! `balanceOf` return words captured from Ethereum mainnet (2026-10-04): the weETH balance of a
//! large holder and the eETH balance of the weETH wrapper contract (a deterministic holder, since
//! every wrapped eETH is custodied there). A reply is routed to the right token by the `to` address.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter, TokenAddress, Wallet,
    U256,
};
use gluonscan_etherfi::EtherFi;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};

const WEETH_ADDR: &str = "0xcd5fe23c85820f7b72d0926fc9b05b43e359b7ee";
const EETH_ADDR: &str = "0x35fa164735182de50811e8e2e824cfb9b6118ac2";

// Real captured return words (32-byte uint256, hex).
const WEETH_BAL: &str = "0x000000000000000000000000000000000000000000000a6de8290a1c1674d3d0"; // 49,251.088846137389798352 weETH
const EETH_BAL: &str = "0x00000000000000000000000000000000000000000001d5fca37ba8766a350bc4"; // 2,219,450.240166915271363524 eETH
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

async fn read_replies(weeth: String, eeth: String) -> Result<gluonscan_core::Reading, Error> {
    let rpc = MockChainProvider::new()
        .on(eth_call_to("cd5fe23c85820f7b"), weeth) // weETH `to`
        .on(eth_call_to("35fa164735182de5"), eeth); // eETH `to`
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    EtherFi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

async fn read_with(weeth: &str, eeth: &str) -> Result<gluonscan_core::Reading, Error> {
    read_replies(ok(weeth), ok(eeth)).await
}

#[tokio::test]
async fn returns_a_complete_stake_position_for_both_tokens() {
    let reading = read_with(WEETH_BAL, EETH_BAL).await.expect("read");

    assert_eq!(reading.protocol, Protocol::EtherFi);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 2);

    // weETH leg: pin identity + the exact decimal amount (catches a decimals/scaling or token swap).
    let weeth = &p.staked[0];
    assert_eq!(weeth.token.symbol, "weETH");
    assert_eq!(weeth.token.address, evm(WEETH_ADDR));
    assert_eq!(
        weeth.raw,
        U256::from_str_radix("49251088846137389798352", 10).unwrap()
    );
    assert_eq!(weeth.amount.to_string(), "49251.088846137389798352");

    // eETH leg: pin the eETH address value + its own distinct amount.
    let eeth = &p.staked[1];
    assert_eq!(eeth.token.symbol, "eETH");
    assert_eq!(eeth.token.address, evm(EETH_ADDR));
    assert_eq!(
        eeth.raw,
        U256::from_str_radix("2219450240166915271363524", 10).unwrap()
    );
    assert_eq!(eeth.amount.to_string(), "2219450.240166915271363524");

    // Restaking yield is in the balance / rate; points & airdrops are off-chain Merkle → no rewards.
    assert!(p.rewards.is_empty());

    // Both tokens reported as receipts, in order, so the idle-wallet list drops them.
    let receipts: Vec<String> = reading
        .receipt_tokens
        .iter()
        .map(|a| format!("{a:#x}"))
        .collect();
    assert_eq!(
        receipts,
        vec![WEETH_ADDR.to_string(), EETH_ADDR.to_string()]
    );
}

#[tokio::test]
async fn only_one_token_held_yields_one_leg() {
    let reading = read_with(WEETH_BAL, ZERO_WORD).await.expect("read");
    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);
    assert_eq!(p.staked[0].token.symbol, "weETH");
    assert_eq!(p.staked[0].amount.to_string(), "49251.088846137389798352");
    assert_eq!(
        reading.receipt_tokens,
        vec![WEETH_ADDR.parse::<Address>().unwrap()]
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
    // weETH read ok, eETH read errors: a balance without its paired read is partial data, so the
    // whole reading must fail closed — never emit the weETH-only position.
    let out = read_replies(ok(WEETH_BAL), RPC_ERROR.to_string()).await;
    assert!(
        out.is_err(),
        "a failed second read must fail the whole reading, not emit a partial one"
    );
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), RPC_ERROR);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let out = EtherFi::new()
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
    let err = EtherFi::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("ether.fi is Ethereum-only; other chains must fail closed");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error, not retryable"
    );
}
