//! Contract tests for the cost-basis chain reads: parse `alchemy_getAssetTransfers`, a transaction's
//! receipt (sender, native value, ERC-20 `Transfer` logs with their tokens' decimals), and
//! `eth_blockNumber`. Offline via MockChainProvider.

use gluonscan_core::{Address, Chain, U256};
use gluonscan_evm::transfers::{asset_transfers_in, latest_block, read_receipt, receipt_logs};
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

/// A 32-byte `eth_call` answer of `hex` (a hex number, so `word("12")` is 18).
fn word(hex: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"0x{hex:0>64}"}}"#)
}

fn rpc_error(error: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"error":{error}}}"#)
}

fn topic(address: &str) -> String {
    format!("0x{}{address}", "0".repeat(24))
}

const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const PT: &str = "b195b618ea52b77cb2a58846f452f59f8dfa9390";
const REVERTS: &str = "3333333333333333333333333333333333333333";
const HUGE_DECIMALS: &str = "4444444444444444444444444444444444444444";
const NO_CODE: &str = "5555555555555555555555555555555555555555";
const MAX_DECIMALS: &str = "6666666666666666666666666666666666666666";
const CODE3_REVERT: &str = "7777777777777777777777777777777777777777";
const DATA_REVERT: &str = "8888888888888888888888888888888888888888";
const BAD_OPCODE: &str = "9999999999999999999999999999999999999999";
const WALLET: &str = "1111111111111111111111111111111111111111";
const ROUTER: &str = "2222222222222222222222222222222222222222";

/// A receipt sent by `WALLET` to `ROUTER` with `(token, from, to)` Transfer logs.
fn receipt(logs: &[(&str, &str, &str)]) -> String {
    let logs: Vec<String> = logs
        .iter()
        .map(|(token, from, to)| {
            format!(
                r#"{{"address":"0x{token}","topics":["{TRANSFER_TOPIC}","{}","{}"],"data":"0x01"}}"#,
                topic(from),
                topic(to)
            )
        })
        .collect();
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"from":"0x{WALLET}","to":"0x{ROUTER}","logs":[{}]}}}}"#,
        logs.join(",")
    )
}

/// The transaction itself, carrying `value_hex` wei.
fn transaction(value_hex: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"hash":"0xtx","from":"0x{WALLET}","value":"{value_hex}"}}}}"#
    )
}

fn decimals_of(token: &str) -> Match {
    Match::all([
        Match::method("eth_call"),
        Match::body_contains(token),
        Match::body_contains("313ce567"),
    ])
}

fn node(receipt_reply: impl Into<String>, tx_reply: impl Into<String>) -> MockChainProvider {
    MockChainProvider::new()
        .on(Match::method("eth_getTransactionReceipt"), receipt_reply)
        .on(Match::method("eth_getTransactionByHash"), tx_reply)
}

#[tokio::test]
async fn read_receipt_decodes_transfer_logs_and_skips_others() {
    // Arrange
    let from = topic(WALLET);
    let to = topic(ROUTER);
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"from":"0x{WALLET}","to":null,"logs":[
          {{"address":"0xToKeN","topics":["{TRANSFER_TOPIC}","{from}","{to}"],"data":"0xff"}},
          {{"address":"0xother","topics":["0xdeadbeef"],"data":"0x01"}}
        ]}}}}"#
    );
    let rpc = node(body, transaction("0x0"));

    // Act
    let out = read_receipt(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");

    // Assert
    assert_eq!(out.from, format!("0x{WALLET}").parse::<Address>().unwrap());
    assert_eq!(out.to, None, "a contract creation calls no address");
    assert_eq!(out.value_wei, U256::ZERO);
    assert_eq!(
        out.transfers.len(),
        1,
        "only the ERC-20 Transfer log is kept"
    );
    let t = &out.transfers[0];
    assert_eq!(t.token, "0xtoken"); // lowercased
    assert_eq!(t.from, format!("0x{WALLET}"));
    assert_eq!(t.to, format!("0x{ROUTER}"));
    assert_eq!(t.value_raw, "0xff");
    assert_eq!(
        t.token_decimals, None,
        "a log address that is not an address"
    );
}

#[tokio::test]
async fn a_native_coin_payment_is_read_from_the_transaction_value() {
    // Arrange: the wallet sent 1.5 ETH and only received the PT; the router wrapped the ETH, so no
    // ERC-20 leg leaves the wallet.
    let rpc = node(
        receipt(&[(PT, ROUTER, WALLET)]),
        transaction("0x14d1120d7b160000"),
    )
    .on(decimals_of(PT), word("12"));

    // Act
    let out = read_receipt(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");

    // Assert
    assert_eq!(out.from, format!("0x{WALLET}").parse::<Address>().unwrap());
    assert_eq!(
        out.to,
        Some(format!("0x{ROUTER}").parse::<Address>().unwrap())
    );
    assert_eq!(out.value_wei, U256::from(1_500_000_000_000_000_000u128));
    assert!(out
        .transfers
        .iter()
        .all(|t| t.from != format!("0x{WALLET}")));
}

#[tokio::test]
async fn read_receipt_reads_each_tokens_decimals_once() {
    // Arrange
    let rpc = node(
        receipt(&[
            (USDC, WALLET, ROUTER),
            (USDC, ROUTER, ROUTER),
            (PT, ROUTER, WALLET),
            (MAX_DECIMALS, ROUTER, WALLET),
        ]),
        transaction("0x0"),
    )
    .on(decimals_of(USDC), word("6"))
    // 0x12 = 18.
    .on(decimals_of(PT), word("12"))
    .on(decimals_of(MAX_DECIMALS), word("ff"));

    // Act
    let out = read_receipt(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");

    // Assert
    let decimals: Vec<Option<u8>> = out.transfers.iter().map(|t| t.token_decimals).collect();
    assert_eq!(decimals, vec![Some(6), Some(6), Some(18), Some(255)]);
    assert_eq!(out.transfers[2].to, format!("0x{WALLET}"));
    let decimal_calls = rpc
        .calls()
        .into_iter()
        .filter(|(m, _)| m == "eth_call")
        .count();
    assert_eq!(decimal_calls, 3, "one decimals() read per distinct token");
}

#[tokio::test]
async fn a_token_without_a_usable_decimals_answer_has_none() {
    // Arrange
    let tokens = [
        REVERTS,
        HUGE_DECIMALS,
        NO_CODE,
        CODE3_REVERT,
        DATA_REVERT,
        BAD_OPCODE,
    ];
    let logs: Vec<(&str, &str, &str)> = tokens.iter().map(|t| (*t, WALLET, ROUTER)).collect();
    let rpc = node(receipt(&logs), transaction("0x0"))
        .on(
            decimals_of(REVERTS),
            rpc_error(r#"{"code":-32000,"message":"execution reverted"}"#),
        )
        // 0x100 = 256, past u8.
        .on(decimals_of(HUGE_DECIMALS), word("100"))
        .on(
            decimals_of(NO_CODE),
            r#"{"jsonrpc":"2.0","id":1,"result":"0x"}"#,
        )
        .on(
            decimals_of(CODE3_REVERT),
            rpc_error(r#"{"code":3,"message":"execution failed","data":"0x08c379a0"}"#),
        )
        .on(
            decimals_of(DATA_REVERT),
            rpc_error(r#"{"code":-32015,"message":"VM execution error.","data":"Reverted 0x"}"#),
        )
        .on(
            decimals_of(BAD_OPCODE),
            rpc_error(r#"{"code":-32000,"message":"invalid opcode: INVALID"}"#),
        );

    // Act
    let out = read_receipt(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect("receipt");

    // Assert
    let decimals: Vec<Option<u8>> = out.transfers.iter().map(|t| t.token_decimals).collect();
    assert_eq!(decimals, vec![None; tokens.len()]);
}

#[tokio::test]
async fn a_node_that_does_not_answer_decimals_fails_the_read_retryably() {
    // Arrange
    let rpc = node(receipt(&[(USDC, WALLET, ROUTER)]), transaction("0x0")).on(
        decimals_of(USDC),
        rpc_error(r#"{"code":429,"message":"rate limited"}"#),
    );

    // Act
    let err = read_receipt(&rpc, Chain::Ethereum, "0xtx")
        .await
        .expect_err("decimals unanswered");

    // Assert
    assert!(err.is_retryable(), "{err:?}");
}

// A receipt or transaction the node has not seen yet is `result: null`; reading it as "no transfers"
// would make a purchase look like a plain transfer in, and a caller would move past it for good.
#[tokio::test]
async fn an_unseen_receipt_or_transaction_fails_the_read_retryably() {
    // Arrange
    let null = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
    let cases = [
        ("receipt", node(null, transaction("0x0"))),
        ("transaction", node(receipt(&[]), null)),
    ];

    for (label, rpc) in cases {
        // Act
        let err = read_receipt(&rpc, Chain::Ethereum, "0xtx")
            .await
            .expect_err(label);

        // Assert
        assert!(err.is_retryable(), "{label}: {err:?}");
    }
}

#[tokio::test]
async fn a_receipt_without_a_sender_or_logs_is_an_integrity_error() {
    // Arrange
    let cases = [
        (
            "no from",
            r#"{"jsonrpc":"2.0","id":1,"result":{"to":null,"logs":[]}}"#,
        ),
        (
            "no logs",
            r#"{"jsonrpc":"2.0","id":1,"result":{"from":"0x1111111111111111111111111111111111111111"}}"#,
        ),
    ];

    for (label, body) in cases {
        // Act
        let err = read_receipt(&node(body, transaction("0x0")), Chain::Ethereum, "0xtx")
            .await
            .expect_err(label);

        // Assert
        assert!(!err.is_retryable(), "{label}: {err:?}");
    }
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
