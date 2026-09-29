//! RateLimitedHttp: forwards to the inner client and throttles once the window is full.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gluonscan::{Http, RateLimitedHttp};
use gluonscan_testing::{Match, MockHttp};

#[tokio::test]
async fn forwards_to_inner() {
    let http = RateLimitedHttp::per_minute(Arc::new(MockHttp::new().on(Match::Any, "ok")), 30);
    assert_eq!(http.get("https://x", &[]).await.unwrap(), "ok");
}

#[tokio::test]
async fn throttles_once_the_window_is_full() {
    // 2 requests per 200ms; the third must wait for the window to slide.
    let http = RateLimitedHttp::new(
        Arc::new(MockHttp::new().on(Match::Any, "ok")),
        2,
        Duration::from_millis(200),
    );
    let start = Instant::now();
    http.get("u", &[]).await.unwrap();
    http.get("u", &[]).await.unwrap();
    http.get("u", &[]).await.unwrap();
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "third request should have been delayed"
    );
}
