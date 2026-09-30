//! Fail-closed tests for the Aave adapter: a GraphQL error, a malformed `userSupplies` entry, and a
//! transport failure must each surface an error (Integrity / retryable) — never a silent empty
//! reading that a user could mistake for "no positions".

use std::sync::Arc;

use gluonscan_aave::AaveApi;
use gluonscan_core::{Address, Chain, Ctx, Detail, Error, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};

fn ctx(http: MockHttp) -> Ctx {
    Ctx::new(Arc::new(http), Arc::new(MockClock(0)))
}

async fn read_err(http: MockHttp) -> Error {
    AaveApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Summary,
            &ctx(http),
        )
        .await
        .expect_err("read must fail closed, not return an empty reading")
}

#[tokio::test]
async fn graphql_errors_array_fails_closed() {
    // A GraphQL error accompanied by `data: null` must not be treated as "no positions".
    let http = MockHttp::new().on(
        Match::body_contains("userSupplies"),
        r#"{"errors":[{"message":"boom"}],"data":null}"#,
    );
    let err = read_err(http).await;
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn malformed_user_supplies_entry_fails_closed() {
    // `userSupplies` is present but an element is malformed (missing `currency`): the read must fail
    // closed rather than parse it into a partial/empty supply.
    let http = MockHttp::new().on(
        Match::body_contains("userSupplies"),
        r#"{"data":{"userSupplies":[{"balance":{"amount":{"value":"1.5"}}}],"userBorrows":[],"userMarketState":null}}"#,
    );
    let err = read_err(http).await;
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn non_array_user_supplies_fails_closed() {
    // `userSupplies` present but the wrong JSON type (an object, not an array) is a malformed
    // response — it must fail closed, not be read as "no positions".
    let http = MockHttp::new().on(
        Match::body_contains("userSupplies"),
        r#"{"data":{"userSupplies":{"bad":"shape"},"userBorrows":[],"userMarketState":null}}"#,
    );
    let err = read_err(http).await;
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("not an array")),
        "expected Integrity(not an array), got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn transport_failure_propagates_retryable() {
    // A transient transport failure must propagate as retryable, never swallowed into an empty read.
    let http = MockHttp::new().on_transient(Match::body_contains("userSupplies"));
    let err = read_err(http).await;
    assert!(
        err.is_retryable(),
        "expected a retryable transport error, got {err:?}"
    );
}
