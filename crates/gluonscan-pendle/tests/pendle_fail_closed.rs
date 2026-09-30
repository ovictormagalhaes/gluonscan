//! Fail-closed tests for the Pendle adapter catalog: a response missing `results` and a transport
//! failure must each surface an error — never a silent empty catalog that would drop held positions.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Error, ProtocolAdapter, Wallet};
use gluonscan_pendle::PendleApi;
use gluonscan_testing::{Match, MockClock, MockHttp};

async fn read_err(http: MockHttp) -> Error {
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    PendleApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("read must fail closed, not return an empty catalog")
}

#[tokio::test]
async fn catalog_missing_results_fails_closed() {
    // `results` is absent (only `total`): the walk must fail closed rather than treat it as an empty
    // page and silently drop every held position.
    let http = MockHttp::new().on(Match::primary_contains("markets"), r#"{"total":0}"#);
    let err = read_err(http).await;
    assert!(
        matches!(err, Error::Integrity { .. }),
        "expected Integrity, got {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn catalog_transport_failure_propagates_retryable() {
    // A transient catalog fetch failure must propagate as retryable, never swallowed into an empty
    // catalog.
    let http = MockHttp::new().on_transient(Match::primary_contains("markets"));
    let err = read_err(http).await;
    assert!(
        err.is_retryable(),
        "expected a retryable transport error, got {err:?}"
    );
}
