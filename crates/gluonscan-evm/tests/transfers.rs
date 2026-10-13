//! Contract tests for the cost-basis chain reads: parse `alchemy_getAssetTransfers`, the ERC-20
//! `Transfer` logs of a receipt, and `eth_blockNumber`. Offline via MockChainProvider.

use gluonscan_core::Chain;
use gluonscan_evm::transfers::{asset_transfers_in, latest_block, receipt_logs, receipt_transfers};
use gluonscan_testing::{Match, MockChainProvider};

const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

#[tokio::test]
async fn asset_transfers_parse_amount_decimals_and_timestamp() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"transfers":[
      {"hash":"0xabc","blockNum":"0x10","metadata":{"blockTimestamp":"2026-01-02T03:04:05.000Z"},
       "rawContract":{"value":"0x64","decimal":"0x12"}}
    ]}}"#;
    let rpc = MockChainProvider::new().on(Match::method("alchemy_getAssetTransfers"), body);
    let out = asset_transfers_in(&rpc, Chain::Ethereum, "0xwallet", "0xtoken", Some(5))
        .await
        .expect("transfers");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].tx, "0xabc");
    assert_eq!(out[0].block, 16);
    assert_eq!(out[0].value_raw, "0x64");
    assert_eq!(out[0].token_decimals, 18);
    // 2026-01-02T03:04:05Z
    assert_eq!(out[0].block_timestamp, 1_767_323_045);
}

#[tokio::test]
async fn receipt_transfers_decode_erc20_logs_and_skip_non_transfer() {
    let from = format!(
        "0x{}{}",
        "0".repeat(24),
        "1111111111111111111111111111111111111111"
    );
    let to = format!(
        "0x{}{}",
        "0".repeat(24),
        "2222222222222222222222222222222222222222"
    );
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"logs":[
          {{"address":"0xToKeN","topics":["{TRANSFER_TOPIC}","{from}","{to}"],"data":"0xff"}},
          {{"address":"0xother","topics":["0xdeadbeef"],"data":"0x01"}}
        ]}}}}"#
    );
    let rpc = MockChainProvider::new().on(Match::method("eth_getTransactionReceipt"), body);
    let out = receipt_transfers(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");
    assert_eq!(out.len(), 1, "only the ERC-20 Transfer log is kept");
    assert_eq!(out[0].token, "0xtoken"); // lowercased
    assert_eq!(out[0].from, "0x1111111111111111111111111111111111111111");
    assert_eq!(out[0].to, "0x2222222222222222222222222222222222222222");
    assert_eq!(out[0].value_raw, "0xff");
}

#[tokio::test]
async fn receipt_logs_keep_every_log_lowercased() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"logs":[
      {"address":"0xC36442b4a4522E871399CD717aBDD847Ab11FE88","topics":["0xABCD","0x01"],"data":"0xFF"},
      {"address":"0xother","topics":[],"data":"0x"}
    ]}}"#;
    let rpc = MockChainProvider::new().on(Match::method("eth_getTransactionReceipt"), body);
    let out = receipt_logs(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].address, "0xc36442b4a4522e871399cd717abdd847ab11fe88");
    assert_eq!(
        out[0].topics,
        vec!["0xabcd".to_string(), "0x01".to_string()]
    );
    assert_eq!(out[0].data, "0xff");
    assert!(out[1].topics.is_empty());
}

// A tx the node does not know returns `result: null`; reading that as "no logs" would silently drop
// whatever the caller is rebuilding from them.
#[tokio::test]
async fn missing_receipt_is_an_error_not_an_empty_list() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_getTransactionReceipt"),
        r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
    );
    let err = receipt_logs(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect_err("missing receipt");
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn latest_block_parses_hex() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_blockNumber"),
        r#"{"jsonrpc":"2.0","id":1,"result":"0x1a"}"#,
    );
    let n = latest_block(&rpc, Chain::Ethereum).await.expect("block");
    assert_eq!(n, 26);
}

#[tokio::test]
async fn rpc_error_is_retryable() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_blockNumber"),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#,
    );
    let err = latest_block(&rpc, Chain::Ethereum)
        .await
        .expect_err("rpc error");
    assert!(err.is_retryable());
}
