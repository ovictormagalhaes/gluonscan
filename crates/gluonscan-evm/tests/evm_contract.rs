//! Contract-based tests for the EVM helpers: ABI encode/decode plus `eth_call` / `eth_getBalance`
//! replayed over an injected [`MockChainProvider`].

use std::str::FromStr;

use alloy_primitives::{hex, Address, U256};
use gluonscan_core::{Chain, Error};
use gluonscan_evm::{
    decode_two_u256, decode_u256, encode_balance_of, encode_collect, encode_selector_with_address,
    eth_call, eth_get_balance,
};
use gluonscan_testing::{Match, MockChainProvider};

// A distinctive, non-zero address so any padding bug (wrong offset, truncation) is visible.
const ADDR: &str = "0x1111111111111111111111111111111111111111";

fn addr() -> Address {
    Address::from_str(ADDR).unwrap()
}

// --- Encoders: assert calldata bytes EXACTLY (4-byte selector + 32-byte-padded args). ---

#[test]
fn encode_balance_of_exact_calldata() {
    let data = encode_balance_of(addr());
    // selector 0x70a08231 + left-padded owner word (12 zero bytes + 20 address bytes).
    let expected = "70a082310000000000000000000000001111111111111111111111111111111111111111";
    assert_eq!(hex::encode(&data), expected);
    assert_eq!(data.len(), 4 + 32);
    assert_eq!(&data[0..4], &[0x70, 0xa0, 0x82, 0x31]);
    assert_eq!(&data[4..16], &[0u8; 12], "address word must be left-padded");
}

#[test]
fn encode_selector_with_address_exact_calldata() {
    let data = encode_selector_with_address([0xde, 0xad, 0xbe, 0xef], addr());
    let expected = "deadbeef0000000000000000000000001111111111111111111111111111111111111111";
    assert_eq!(hex::encode(&data), expected);
    assert_eq!(data.len(), 4 + 32);
}

#[test]
fn encode_balance_of_delegates_to_selector_helper() {
    assert_eq!(
        encode_balance_of(addr()),
        encode_selector_with_address([0x70, 0xa0, 0x82, 0x31], addr())
    );
}

#[test]
fn encode_collect_exact_calldata() {
    let data = encode_collect(U256::from(0x2au64), addr());
    let expected = concat!(
        "fc6f7865",                                                         // selector
        "000000000000000000000000000000000000000000000000000000000000002a", // tokenId = 42
        "0000000000000000000000001111111111111111111111111111111111111111", // recipient
        "00000000000000000000000000000000ffffffffffffffffffffffffffffffff", // amount0Max=u128::MAX
        "00000000000000000000000000000000ffffffffffffffffffffffffffffffff", // amount1Max=u128::MAX
    );
    assert_eq!(hex::encode(&data), expected);
    assert_eq!(data.len(), 4 + 32 * 4);
    assert_eq!(&data[0..4], &[0xfc, 0x6f, 0x78, 0x65]);
}

// --- decode_u256 / decode_two_u256: exact value on good input, Integrity on short buffer. ---

#[test]
fn decode_u256_known_value() {
    // 1.5 ETH in wei = 0x14d1120d7b160000, left-padded to a 32-byte word.
    let buf =
        hex::decode("00000000000000000000000000000000000000000000000014d1120d7b160000").unwrap();
    assert_eq!(buf.len(), 32);
    let got = decode_u256(&buf).unwrap();
    assert_eq!(got, U256::from(1_500_000_000_000_000_000u64));
}

#[test]
fn decode_u256_ignores_trailing_bytes() {
    // A 64-byte buffer still decodes only the first word.
    let buf = hex::decode(
        "000000000000000000000000000000000000000000000000000000000000002a\
         00000000000000000000000000000000000000000000000000000000deadbeef",
    )
    .unwrap();
    assert_eq!(decode_u256(&buf).unwrap(), U256::from(0x2au64));
}

#[test]
fn decode_u256_short_buffer_is_integrity() {
    let short = vec![0u8; 31];
    let err = decode_u256(&short).expect_err("31 bytes < 32 must fail");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[test]
fn decode_two_u256_known_values() {
    let buf = hex::decode(
        "00000000000000000000000000000000000000000000000014d1120d7b160000\
         0000000000000000000000000000000000000000000000000000000000000002",
    )
    .unwrap();
    assert_eq!(buf.len(), 64);
    let (a, b) = decode_two_u256(&buf).unwrap();
    assert_eq!(a, U256::from(1_500_000_000_000_000_000u64));
    assert_eq!(b, U256::from(2u64));
}

#[test]
fn decode_two_u256_short_buffer_is_integrity() {
    // 63 bytes: enough for one word but not two — must fail, not silently read one.
    let short = vec![0u8; 63];
    let err = decode_two_u256(&short).expect_err("63 bytes < 64 must fail");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}

// --- eth_call: response-shape handling over the mock transport. ---

#[tokio::test]
async fn eth_call_returns_decoded_bytes() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_call"),
        r#"{"jsonrpc":"2.0","id":1,"result":"0x1234abcd"}"#,
    );
    let out = eth_call(&rpc, Chain::Ethereum, None, addr(), vec![0x01])
        .await
        .expect("well-formed result");
    assert_eq!(out, vec![0x12, 0x34, 0xab, 0xcd]);
}

#[tokio::test]
async fn eth_call_includes_from_field_when_present() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), r#"{"result":"0x"}"#);
    let from = Address::from_str("0x2222222222222222222222222222222222222222").unwrap();
    let out = eth_call(&rpc, Chain::Base, Some(from), addr(), vec![])
        .await
        .expect("empty result decodes to empty bytes");
    assert!(out.is_empty());
    let (_, params) = rpc.calls().pop().expect("one call recorded");
    assert!(
        params.contains(r#""from":"0x2222222222222222222222222222222222222222""#),
        "from must be forwarded in params: {params}"
    );
}

#[tokio::test]
async fn eth_call_jsonrpc_error_is_provider() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_call"),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#,
    );
    let err = eth_call(&rpc, Chain::Ethereum, None, addr(), vec![])
        .await
        .expect_err("a JSON-RPC error must surface");
    assert!(
        matches!(err, Error::Provider(_)),
        "expected Provider, got {err:?}"
    );
}

#[tokio::test]
async fn eth_call_missing_result_is_integrity() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), r#"{"jsonrpc":"2.0","id":1}"#);
    let err = eth_call(&rpc, Chain::Ethereum, None, addr(), vec![])
        .await
        .expect_err("no `result` and no `error` must fail closed");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}

#[tokio::test]
async fn eth_call_non_hex_result_is_integrity() {
    let rpc = MockChainProvider::new().on(Match::method("eth_call"), r#"{"result":"0xZZZZ"}"#);
    let err = eth_call(&rpc, Chain::Ethereum, None, addr(), vec![])
        .await
        .expect_err("a non-hex result must fail, never a bogus reading");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}

#[tokio::test]
async fn eth_call_non_json_response_is_integrity() {
    let rpc =
        MockChainProvider::new().on(Match::method("eth_call"), "<html>gateway timeout</html>");
    let err = eth_call(&rpc, Chain::Ethereum, None, addr(), vec![])
        .await
        .expect_err("a non-JSON body must fail closed");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}

// --- eth_get_balance: happy path + malformed response. ---

#[tokio::test]
async fn eth_get_balance_known_value() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_getBalance"),
        r#"{"jsonrpc":"2.0","id":1,"result":"0x14d1120d7b160000"}"#,
    );
    let wei = eth_get_balance(&rpc, Chain::Ethereum, addr())
        .await
        .expect("well-formed quantity");
    assert_eq!(wei, U256::from(1_500_000_000_000_000_000u64));
}

#[tokio::test]
async fn eth_get_balance_error_is_provider() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_getBalance"),
        r#"{"error":{"code":-32000,"message":"boom"}}"#,
    );
    let err = eth_get_balance(&rpc, Chain::Ethereum, addr())
        .await
        .expect_err("a JSON-RPC error must surface");
    assert!(
        matches!(err, Error::Provider(_)),
        "expected Provider, got {err:?}"
    );
}

#[tokio::test]
async fn eth_get_balance_missing_result_is_integrity() {
    let rpc = MockChainProvider::new().on(
        Match::method("eth_getBalance"),
        r#"{"jsonrpc":"2.0","id":1}"#,
    );
    let err = eth_get_balance(&rpc, Chain::Ethereum, addr())
        .await
        .expect_err("missing result must fail closed");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}

#[tokio::test]
async fn eth_get_balance_non_hex_quantity_is_integrity() {
    let rpc =
        MockChainProvider::new().on(Match::method("eth_getBalance"), r#"{"result":"0xnothex"}"#);
    let err = eth_get_balance(&rpc, Chain::Ethereum, addr())
        .await
        .expect_err("a non-hex quantity must fail, never a bogus balance");
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
}
