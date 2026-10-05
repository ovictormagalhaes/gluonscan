//! Contract-based tests for the Morpho adapter, offline. The main GraphQL reply is the real
//! `userByAddress` response for a live Base borrower (0xD3d7900a…307Fc, captured 2026-10-03): one
//! live cbBTC/USDC borrow plus one zeroed historical market that must be filtered out.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, TokenAddress, Wallet, U256,
};
use gluonscan_morpho::Morpho;
use gluonscan_testing::{Match, MockClock, MockHttp};
use rust_decimal::Decimal;

const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";

const RESPONSE: &str = r#"{"data":{"userByAddress":{"marketPositions":[
  {"healthFactor":null,"state":{"supplyAssets":0,"borrowAssets":0,"collateral":0},
   "market":{"marketId":"0xd4a903dc6d949519060c7707f9604fdc9772c046e05c2e3a8fce0bd7196e4109","lltv":"625000000000000000",
     "loanAsset":{"symbol":"USDC","address":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","decimals":6},
     "collateralAsset":{"symbol":"cbXRP","address":"0xcb585250f852C6c6bf90434AB21A00f02833a4af","decimals":6},
     "state":{"supplyApy":0.05565724904836262,"borrowApy":0.06202242631331779}}},
  {"healthFactor":1.6665932441569424,"state":{"supplyAssets":0,"borrowAssets":5149558401067,"collateral":11775745194},
   "market":{"marketId":"0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836","lltv":"860000000000000000",
     "loanAsset":{"symbol":"USDC","address":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","decimals":6},
     "collateralAsset":{"symbol":"cbBTC","address":"0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf","decimals":8},
     "state":{"supplyApy":0.04403805578960049,"borrowApy":0.049024557476286146}}}
]}}}"#;

fn evm(addr: &str) -> Option<TokenAddress> {
    Some(TokenAddress::Evm(addr.parse().unwrap()))
}

fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn read(response: &str) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    read_on(response, Chain::Base).await
}

async fn read_on(
    response: &str,
    chain: Chain,
) -> Result<gluonscan_core::Reading, gluonscan_core::Error> {
    let http = MockHttp::new().on(Match::body_contains("userByAddress"), response);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    Morpho::new()
        .with_endpoint("https://morpho.test/graphql")
        .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
        .await
        .map(|c| c.into_inner())
}

#[tokio::test]
async fn maps_the_live_borrow_and_drops_the_zeroed_market() {
    let reading = read(RESPONSE).await.expect("read");

    assert_eq!(reading.protocol, Protocol::Morpho);
    // The zeroed historical market is filtered; only the live borrow survives.
    assert_eq!(reading.positions.len(), 1);

    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };

    // Collateral leg: cbBTC (address + 8-decimal amount pinned so a decimals or token mix-up is
    // caught), flagged collateral, LLTV 0.86 as both threshold and max LTV.
    assert_eq!(p.supplied.len(), 1);
    let collateral = &p.supplied[0];
    assert_eq!(collateral.amount.token.symbol, "cbBTC");
    assert_eq!(collateral.amount.token.address, evm(CBBTC));
    assert_eq!(collateral.amount.raw, U256::from(11_775_745_194u64));
    assert_eq!(collateral.amount.amount.to_string(), "117.75745194"); // 11775745194 / 1e8
    assert!(collateral.is_collateral);
    assert_eq!(collateral.liquidation_threshold, Some(dec("0.86")));
    assert_eq!(collateral.max_ltv, Some(dec("0.86")));
    // Morpho Blue collateral earns no yield — an accurate zero, not a fabricated one.
    assert_eq!(collateral.apy, Some(Decimal::ZERO));

    // Borrow leg: USDC (address + 6-decimal amount pinned), borrow factor 1 (Morpho Blue has none),
    // APY carried through as a fraction (~4.9%, not 490%).
    assert_eq!(p.borrowed.len(), 1);
    let debt = &p.borrowed[0];
    assert_eq!(debt.amount.token.symbol, "USDC");
    assert_eq!(debt.amount.token.address, evm(USDC));
    assert_eq!(debt.amount.raw, U256::from(5_149_558_401_067u64));
    assert_eq!(debt.amount.amount.to_string(), "5149558.401067"); // 5149558401067 / 1e6
    assert_eq!(debt.borrow_factor, Some(Decimal::ONE));
    let apy = debt.apy.expect("borrow apy");
    assert!(
        apy > dec("0.048") && apy < dec("0.05"),
        "borrow apy ~4.9% fraction, got {apy}"
    );

    // Health factor present (never hidden for debt) and the real ~1.667 value.
    let hf = p.health_factor.expect("health factor");
    assert!(
        hf > dec("1.66") && hf < dec("1.67"),
        "health factor ~1.667, got {hf}"
    );

    // The market's unique on-chain id is carried through: it is the only stable identity that keeps
    // two isolated markets sharing the same (collateral, loan, LLTV) from colliding downstream.
    assert_eq!(
        p.market_id.as_deref(),
        Some("0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836")
    );
}

#[tokio::test]
async fn large_18_decimal_amount_stays_lossless() {
    // Morpho serializes any BigInt above 2^53 as a JSON STRING (verified against the live API), so
    // a 1e26-wei (100M of an 18-decimal token) amount arrives quoted and is parsed verbatim — no
    // f64 anywhere. Also asserts the supply leg carries the market's supplyApy (0.03 fraction).
    let response = r#"{"data":{"userByAddress":{"marketPositions":[
      {"healthFactor":null,"state":{"supplyAssets":"100000000000000000000000000","borrowAssets":0,"collateral":0},
       "market":{"marketId":"0xabc","lltv":"860000000000000000",
         "loanAsset":{"symbol":"DAI","address":"0x6B175474E89094C44Da98b954EedeAC495271d0F","decimals":18},
         "collateralAsset":null,
         "state":{"supplyApy":0.03,"borrowApy":0.05}}}
    ]}}}"#;
    let reading = read(response).await.expect("read");
    let Position::Lending(p) = &reading.positions[0] else {
        panic!("expected a lending position");
    };
    // Supply-only: one loan-supply leg, no collateral flag, HF stays None (no debt).
    assert_eq!(p.supplied.len(), 1);
    let supply = &p.supplied[0];
    assert!(!supply.is_collateral);
    assert_eq!(
        supply.amount.raw,
        U256::from_str_radix("100000000000000000000000000", 10).unwrap(),
        "big-int amount must not be truncated to f64"
    );
    assert_eq!(supply.apy, Some(dec("0.03")));
    assert!(p.borrowed.is_empty());
    assert!(p.health_factor.is_none());
}

#[tokio::test]
async fn empty_wallet_is_a_successful_empty_reading() {
    let response = r#"{"data":{"userByAddress":{"marketPositions":[]}}}"#;
    let reading = read(response).await.expect("read");
    assert!(reading.positions.is_empty());
}

#[tokio::test]
async fn never_seen_wallet_null_user_is_empty_not_error() {
    let response = r#"{"data":{"userByAddress":null}}"#;
    let reading = read(response).await.expect("read");
    assert!(reading.positions.is_empty());
}

#[tokio::test]
async fn collateral_without_its_asset_fails_closed() {
    // A position with collateral > 0 but a null collateralAsset cannot be denominated (no
    // decimals/address). Emitting it would be partial/degraded data, so the read must fail closed.
    let response = r#"{"data":{"userByAddress":{"marketPositions":[
      {"healthFactor":1.5,"state":{"supplyAssets":0,"borrowAssets":1000000,"collateral":11775745194},
       "market":{"marketId":"0xabc","lltv":"860000000000000000",
         "loanAsset":{"symbol":"USDC","address":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","decimals":6},
         "collateralAsset":null,
         "state":{"supplyApy":0.03,"borrowApy":0.05}}}
    ]}}}"#;
    let err = read(response)
        .await
        .expect_err("collateral with no collateral asset must fail closed");
    assert!(
        !err.is_retryable(),
        "a missing collateral asset is not a transient error"
    );
}

#[tokio::test]
async fn debt_without_health_factor_fails_closed() {
    // A borrow with no health factor would hide liquidation risk — the read must fail closed. This
    // is the guard that no other test exercised; without it a deleted check would pass silently.
    let response = r#"{"data":{"userByAddress":{"marketPositions":[
      {"healthFactor":null,"state":{"supplyAssets":0,"borrowAssets":5149558401067,"collateral":11775745194},
       "market":{"marketId":"0xabc","lltv":"860000000000000000",
         "loanAsset":{"symbol":"USDC","address":"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913","decimals":6},
         "collateralAsset":{"symbol":"cbBTC","address":"0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf","decimals":8},
         "state":{"supplyApy":0.044,"borrowApy":0.049}}}
    ]}}}"#;
    let err = read(response)
        .await
        .expect_err("debt without a health factor must fail closed");
    assert!(
        !err.is_retryable(),
        "a missing health factor on a debt position is not transient"
    );
}

#[tokio::test]
async fn malformed_token_address_fails_closed() {
    // A garbage loan-asset address can't be priced — fail closed, never an address-less token.
    let response = r#"{"data":{"userByAddress":{"marketPositions":[
      {"healthFactor":null,"state":{"supplyAssets":1000000,"borrowAssets":0,"collateral":0},
       "market":{"marketId":"0xabc","lltv":"860000000000000000",
         "loanAsset":{"symbol":"USDC","address":"not-an-address","decimals":6},
         "collateralAsset":null,
         "state":{"supplyApy":0.03,"borrowApy":0.05}}}
    ]}}}"#;
    let err = read(response)
        .await
        .expect_err("a malformed token address must fail closed");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn transient_http_failure_is_retryable() {
    // The other half of the fail-closed contract: a transport-level failure (timeout/5xx) must
    // surface as retryable so the caller retries next cycle, not skip permanently.
    let http = MockHttp::new().on_transient(Match::body_contains("userByAddress"));
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));
    let err = Morpho::new()
        .with_endpoint("https://morpho.test/graphql")
        .read(&Wallet::Evm(Address::ZERO), Chain::Base, Detail::Full, &cx)
        .await
        .expect_err("a transient HTTP failure must surface");
    assert!(
        err.is_retryable(),
        "a transport failure must stay retryable"
    );
}

#[tokio::test]
async fn graphql_error_fails_closed() {
    let response =
        r#"{"errors":[{"message":"invalid address","status":"BAD_USER_INPUT"}],"data":null}"#;
    let err = read(response)
        .await
        .expect_err("a GraphQL error must fail closed");
    assert!(
        !err.is_retryable(),
        "a BAD_USER_INPUT GraphQL error is not retryable"
    );
}

#[tokio::test]
async fn unsupported_chain_fails_closed() {
    let err = read_on(RESPONSE, Chain::Arbitrum)
        .await
        .expect_err("Morpho v1 is Ethereum + Base only");
    assert!(
        !err.is_retryable(),
        "an unsupported chain is a permanent config error"
    );
}
