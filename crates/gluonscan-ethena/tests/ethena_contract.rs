//! Contract-based tests for the Ethena adapter, entirely offline. The `eth_call` reply is a real
//! sUSDe `balanceOf` return word captured from Ethereum mainnet (2026-10-04): the sUSDe held by the
//! Aave V3 aEthsUSDe aToken, a large deterministic holder.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Error, Position, Protocol, ProtocolAdapter, TokenAddress, Wallet,
    U256,
};
use gluonscan_ethena::Ethena;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const SUSDE_ADDR: &str = "0x9d39a5de30e57443bff2a8307a4256c8797a3497";

// Real captured return word (32-byte uint256, hex).
const SUSDE_BAL: &str = "0x00000000000000000000000000000000000000000063221b757374ad02f1aba7"; // 119,844,723.127592257577921447 sUSDe
const ZERO_WORD: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

const RPC_ERROR: &str = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#;

fn reply(result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
}

async fn read_reply(body: String) -> Result<gluonscan_core::Reading, Error> {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), body);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    Ethena::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn attaches_the_ethena_yield_as_a_fraction() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), reply(SUSDE_BAL));
    // DeFiLlama's chart endpoint returns a percentage under the last data point's `apy`; carried as
    // a fraction. (Ethena's own app API is unreachable from a datacenter, hence DeFiLlama.)
    let http = MockHttp::new().on(
        Match::primary_contains("llama.fi"),
        r#"{"status":"success","data":[{"timestamp":"2026-10-04T00:00:00.000Z","apy":5.1,"apyBase":5.1},{"timestamp":"2026-10-05T00:00:00.000Z","apy":4.8492,"apyBase":4.8492}]}"#,
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    let reading = Ethena::new()
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
    let apy = p.apy.expect("yield attached");
    // 4.8492% -> ~0.048492 fraction.
    assert!(
        apy > Decimal::from_str_exact("0.048").unwrap()
            && apy < Decimal::from_str_exact("0.049").unwrap(),
        "got {apy}"
    );
}

#[tokio::test]
async fn returns_a_complete_stake_position() {
    let reading = read_reply(reply(SUSDE_BAL)).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Ethena);
    assert_eq!(reading.positions.len(), 1);

    let Position::Stake(p) = &reading.positions[0] else {
        panic!("expected a stake position");
    };
    assert_eq!(p.staked.len(), 1);

    // sUSDe leg: pin identity + the exact decimal amount (catches a decimals/scaling or token swap).
    let susde = &p.staked[0];
    assert_eq!(susde.token.symbol, "sUSDe");
    assert_eq!(
        susde.token.address,
        Some(TokenAddress::Evm(SUSDE_ADDR.parse().unwrap()))
    );
    assert_eq!(
        susde.raw,
        U256::from_str_radix("119844723127592257577921447", 10).unwrap()
    );
    assert_eq!(susde.amount.to_string(), "119844723.127592257577921447");

    // Yield accrues in the share price, never a separate claimable.
    assert!(p.rewards.is_empty());

    // sUSDe is reported as a receipt so the idle-wallet list drops it (no double count).
    assert_eq!(
        reading.receipt_tokens,
        vec![SUSDE_ADDR.parse::<Address>().unwrap()]
    );
}

#[tokio::test]
async fn no_balance_is_an_empty_reading_not_an_error() {
    let reading = read_reply(reply(ZERO_WORD)).await.expect("read");
    assert!(reading.positions.is_empty());
    assert!(reading.receipt_tokens.is_empty());
}

#[tokio::test]
async fn rpc_error_fails_closed() {
    let out = read_reply(RPC_ERROR.to_string()).await;
    assert!(
        out.is_err(),
        "an RPC error must fail closed, not produce a partial position"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed_permanently() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = Ethena::new()
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("Ethena is Ethereum-only; other chains must fail closed");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error, not retryable"
    );
}
