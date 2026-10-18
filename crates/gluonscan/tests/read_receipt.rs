//! The facade's receipt read: routed through the engine's RPC, failing closed when there is no RPC
//! to ask.

use std::sync::Arc;

use gluonscan::{Address, Chain, Gluonscan, U256};
use gluonscan_testing::{Match, MockChainProvider};

const WALLET: &str = "0x1111111111111111111111111111111111111111";

#[tokio::test]
async fn reads_the_receipt_through_the_engine_rpc() {
    // Arrange
    let rpc = MockChainProvider::new()
        .on(
            Match::method("eth_getTransactionReceipt"),
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":{{"status":"0x1","from":"{WALLET}","to":null,"logs":[]}}}}"#
            ),
        )
        .on(
            Match::method("eth_getTransactionByHash"),
            r#"{"jsonrpc":"2.0","id":1,"result":{"hash":"0xtx","value":"0x2a"}}"#,
        );
    let engine = Gluonscan::builder().rpc(Arc::new(rpc)).build();

    // Act
    let read = engine
        .read_receipt(Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");

    // Assert
    assert!(read.succeeded);
    assert_eq!(read.from, WALLET.parse::<Address>().unwrap());
    assert_eq!(read.value_wei, U256::from(42u64));
    assert!(read.transfers.is_empty());
}

#[tokio::test]
async fn without_an_rpc_the_read_fails() {
    // Arrange
    let engine = Gluonscan::builder().build();

    // Act
    let read = engine.read_receipt(Chain::Ethereum, "0xtx").await;

    // Assert
    assert!(read.is_err(), "{read:?}");
}
