//! The facade's on-chain Morpho market check: routed through the engine's RPC, failing closed
//! when there is no RPC to ask.

use std::sync::Arc;

use gluonscan::{Chain, Gluonscan, Wallet};
use gluonscan_testing::{Match, MockChainProvider};

const MARKET: &str = "0x5b347b3dcfed096f09040cd30a174ae354ecc0a35c996493b8fa490d6d3e79d7";
const OWNER: &str = "0x000000000000000000000000000000000000dEaD";
// `position(0x5b347b…79d7, 0x…dead)` on Base at block 52,603,743: 1e9 supply shares.
const HELD: &str = r#"{"jsonrpc":"2.0","id":1,"result":"0x000000000000000000000000000000000000000000000000000000003b9aca0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}"#;

fn owner() -> Wallet {
    Wallet::Evm(OWNER.parse().unwrap())
}

#[tokio::test]
async fn asks_the_morpho_blue_contract_through_the_engine_rpc() {
    // Arrange
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), HELD);
    let engine = Gluonscan::builder().rpc(Arc::new(rpc)).build();

    // Act
    let held = engine
        .morpho_holds_market(Chain::Base, MARKET, &owner())
        .await;

    // Assert
    assert_eq!(held.ok(), Some(true));
}

#[tokio::test]
async fn without_an_rpc_the_check_fails_instead_of_answering() {
    // Arrange
    let engine = Gluonscan::builder().build();

    // Act
    let held = engine
        .morpho_holds_market(Chain::Base, MARKET, &owner())
        .await;

    // Assert
    assert!(held.is_err(), "{held:?}");
}
