//! The accounting asset a held PT/YT carries, read from a recorded Ethereum market catalog
//! (`fixtures/markets_ethereum.json`, three real markets): PT-sUSDE-26NOV2026 redeems to USDe (not
//! the sUSDe it wraps), PT-SY pufETH-26NOV2026 to native ETH (Pendle's zero address), and
//! PT-reUSD-10DEC2026 had its `accountingAsset` removed to stand for a catalog entry without one.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, ProtocolAdapter, TokenAddress, Wallet, YieldKind,
    YieldPosition,
};
use gluonscan_pendle::PendleApi;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};

const CATALOG: &str = include_str!("fixtures/markets_ethereum.json");

const PT_SUSDE: &str = "b195b618ea52b77cb2a58846f452f59f8dfa9390";
const YT_SUSDE: &str = "89e6e5f7c3a60e7d6347f054051a29a272f4ce44";
const PT_PUFETH: &str = "a32877eee252d78d5e4fa00285d4cc2066d15bac";
const PT_REUSD: &str = "ecfafdc7741323a945a163ed068b5a3c43483957";

fn held() -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:0>64}"}}"#,
        "de0b6b3a7640000"
    )
}

async fn read_holding(catalog: &str, held_tokens: &[&str]) -> Vec<YieldPosition> {
    let http = MockHttp::new().on(Match::primary_contains("markets"), catalog);
    let mut rpc = MockChainProvider::new();
    for token in held_tokens {
        rpc = rpc.on(
            Match::all([Match::method("eth_call"), Match::body_contains(*token)]),
            held(),
        );
    }
    // Every other balance, and the vePENDLE lock, reads zero.
    let rpc = rpc.on(
        Match::method("eth_call"),
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{}"}}"#,
            "0".repeat(128)
        ),
    );
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));
    PendleApi::new()
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner()
        .positions
        .into_iter()
        .filter_map(|p| match p {
            Position::Yield(y) => Some(y),
            _ => None,
        })
        .collect()
}

fn holding<'a>(positions: &'a [YieldPosition], address: &str) -> &'a YieldPosition {
    positions
        .iter()
        .find(|y| {
            y.amount
                .token
                .address
                .as_ref()
                .is_some_and(|a| a.to_string() == format!("0x{address}"))
        })
        .expect("held position")
}

#[tokio::test]
async fn a_pt_carries_the_asset_it_redeems_to_not_the_one_it_wraps() {
    // Arrange
    let catalog = CATALOG;

    // Act
    let positions = read_holding(catalog, &[PT_SUSDE, YT_SUSDE]).await;

    // Assert
    for (token, kind) in [
        (PT_SUSDE, YieldKind::PrincipalToken),
        (YT_SUSDE, YieldKind::YieldToken),
    ] {
        let y = holding(&positions, token);
        assert_eq!(y.kind, kind);
        let asset = y.accounting_asset.as_ref().expect("accounting asset");
        assert_eq!(asset.symbol, "USDe");
        assert_eq!(asset.decimals, 18);
        assert_eq!(
            asset.address.as_ref().map(ToString::to_string).as_deref(),
            Some("0x4c9edd5852cd905f086c759e8383e09bff1e68b3")
        );
    }
}

#[tokio::test]
async fn an_eth_accounted_pt_redeems_to_the_native_coin() {
    // Arrange
    let catalog = CATALOG;

    // Act
    let positions = read_holding(catalog, &[PT_PUFETH]).await;

    // Assert
    let asset = holding(&positions, PT_PUFETH)
        .accounting_asset
        .as_ref()
        .expect("accounting asset");
    assert_eq!(asset.symbol, "ETH");
    assert_eq!(asset.decimals, 18);
    assert_eq!(asset.address, None, "native, never an ERC-20 at 0x0");
}

#[tokio::test]
async fn a_market_without_an_accounting_asset_names_none() {
    // Arrange
    let catalog = CATALOG;

    // Act
    let positions = read_holding(catalog, &[PT_REUSD]).await;

    // Assert
    let y = holding(&positions, PT_REUSD);
    assert_eq!(y.accounting_asset, None, "never the underlying asset");
}

#[tokio::test]
async fn an_unparseable_accounting_asset_names_none() {
    // Arrange
    let mut catalog: serde_json::Value = serde_json::from_str(CATALOG).unwrap();
    let markets = catalog["results"].as_array_mut().unwrap();
    markets[0]["accountingAsset"]["address"] = "0xnot-an-address".into();
    markets[1]["accountingAsset"]
        .as_object_mut()
        .unwrap()
        .remove("decimals");
    let catalog = catalog.to_string();

    // Act
    let positions = read_holding(&catalog, &[PT_SUSDE, PT_PUFETH]).await;

    // Assert
    assert_eq!(holding(&positions, PT_SUSDE).accounting_asset, None);
    assert_eq!(holding(&positions, PT_PUFETH).accounting_asset, None);
    assert!(matches!(
        holding(&positions, PT_SUSDE).amount.token.address,
        Some(TokenAddress::Evm(_))
    ));
}
