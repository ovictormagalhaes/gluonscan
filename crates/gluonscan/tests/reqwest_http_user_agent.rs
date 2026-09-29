//! Regression: some provider edges (CoinGecko's CloudFront) 403 requests with no `User-Agent`, and
//! `reqwest` sends none by default. Every request from `ReqwestHttp` must carry one — on GET and
//! POST alike. The `header_exists` matcher only serves 200 when the header is present; without it
//! the request falls through to 404 and the `unwrap` below fails.

use gluonscan::{Http, ReqwestHttp};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn get_and_post_carry_a_user_agent() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/g"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/p"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let http = ReqwestHttp::new();

    let got = http
        .get(&format!("{}/g", server.uri()), &[])
        .await
        .expect("GET carries a User-Agent");
    assert_eq!(got, "ok");

    let posted = http
        .post(&format!("{}/p", server.uri()), "{}".to_string(), &[])
        .await
        .expect("POST carries a User-Agent");
    assert_eq!(posted, "ok");
}
