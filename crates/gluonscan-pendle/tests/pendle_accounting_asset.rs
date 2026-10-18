//! The accounting asset a held PT/YT carries, read from recorded Pendle market catalogs.
//!
//! `fixtures/markets_ethereum.json` is page 1 of `/v1/1/markets` (2026-10-18) trimmed to three
//! markets, with `total` edited from 503 to 3:
//! - PT-sUSDE-26NOV2026 is accounted in USDe (not the sUSDe it wraps);
//! - PT-SY pufETH-26NOV2026 in native ETH (zero address, base type `NATIVE`);
//! - PT-reUSD-10DEC2026 had its `accountingAsset` removed, to stand for an entry without one.
//!
//! `fixtures/markets_arbitrum.json` is page 1 of `/v1/42161/markets` (2026-10-18) trimmed to three
//! markets, with `total` edited from 92 to 3 and each `isActive` set to `true` (all three had
//! matured, and the adapter skips inactive markets):
//! - PT-wstETH-25JUN2026 is accounted in stETH on chain 1 (Ethereum's address);
//! - PT-rsETH-25APR2024 in native ETH on chain 1;
//! - PT-rETH-25JUN2026 in native ETH on chain 42161.
//!
//! Every other edit is made in the test that needs it, on the market selected by its PT address.

use std::sync::Arc;

use gluonscan_core::{
    Address, Chain, Ctx, Detail, Position, ProtocolAdapter, TokenAddress, Wallet, YieldKind,
    YieldPosition,
};
use gluonscan_pendle::PendleApi;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};

const ETHEREUM: &str = include_str!("fixtures/markets_ethereum.json");
const ARBITRUM: &str = include_str!("fixtures/markets_arbitrum.json");

const PT_SUSDE: &str = "b195b618ea52b77cb2a58846f452f59f8dfa9390";
const YT_SUSDE: &str = "89e6e5f7c3a60e7d6347f054051a29a272f4ce44";
const PT_PUFETH: &str = "a32877eee252d78d5e4fa00285d4cc2066d15bac";
const PT_REUSD: &str = "ecfafdc7741323a945a163ed068b5a3c43483957";
const PT_ARB_WSTETH: &str = "71fbf40651e9d4278a74586afc99f307f369ce9a";
const PT_ARB_RSETH: &str = "8d164e0c662c9e199baac9e97b6a8664c75700ea";
const PT_ARB_RETH: &str = "3362c1265a0522f321253708c9fb176f2274fa8d";

fn held() -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:0>64}"}}"#,
        "de0b6b3a7640000"
    )
}

async fn read_holding(chain: Chain, catalog: &str, held_tokens: &[&str]) -> Vec<YieldPosition> {
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
        .read(&Wallet::Evm(Address::ZERO), chain, Detail::Full, &cx)
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

/// An edit of one market's `accountingAsset`.
type Edit = fn(&mut serde_json::Value);

/// The catalog with `edit` applied to the accounting asset of the market whose PT is `pt`.
fn edited(catalog: &str, pt: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut json: serde_json::Value = serde_json::from_str(catalog).unwrap();
    let market = json["results"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|m| m["pt"]["address"] == format!("0x{pt}"))
        .expect("market in fixture");
    edit(&mut market["accountingAsset"]);
    json.to_string()
}

#[tokio::test]
async fn a_pt_carries_the_unit_it_is_worth_one_of_not_the_one_it_wraps() {
    // Arrange
    let catalog = ETHEREUM;

    // Act
    let positions = read_holding(Chain::Ethereum, catalog, &[PT_SUSDE, YT_SUSDE]).await;

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
async fn an_eth_accounted_pt_is_worth_the_native_coin() {
    // Arrange
    let cases = [
        (Chain::Ethereum, ETHEREUM, PT_PUFETH),
        (Chain::Arbitrum, ARBITRUM, PT_ARB_RETH),
    ];

    for (chain, catalog, pt) in cases {
        // Act
        let positions = read_holding(chain, catalog, &[pt]).await;

        // Assert
        let asset = holding(&positions, pt)
            .accounting_asset
            .as_ref()
            .expect("accounting asset");
        assert_eq!(asset.symbol, "ETH");
        assert_eq!(asset.decimals, 18);
        assert_eq!(asset.address, None, "native, never an ERC-20 at 0x0");
    }
}

#[tokio::test]
async fn an_accounting_asset_on_another_chain_is_none() {
    // Arrange
    let catalog = ARBITRUM;

    // Act
    let positions = read_holding(Chain::Arbitrum, catalog, &[PT_ARB_WSTETH, PT_ARB_RSETH]).await;

    // Assert
    assert_eq!(
        holding(&positions, PT_ARB_WSTETH).accounting_asset,
        None,
        "Ethereum's stETH is not a token on Arbitrum"
    );
    assert_eq!(
        holding(&positions, PT_ARB_RSETH).accounting_asset,
        None,
        "Ethereum's native coin is not Arbitrum's"
    );
}

#[tokio::test]
async fn a_market_without_an_accounting_asset_names_none() {
    // Arrange
    let catalog = ETHEREUM;

    // Act
    let positions = read_holding(Chain::Ethereum, catalog, &[PT_REUSD]).await;

    // Assert
    let y = holding(&positions, PT_REUSD);
    assert_eq!(y.accounting_asset, None, "never the underlying asset");
}

#[tokio::test]
async fn an_incomplete_or_ambiguous_accounting_asset_names_none() {
    // Arrange
    let cases: [(&str, &str, Edit); 7] = [
        ("unparseable address", PT_SUSDE, |a| {
            a["address"] = "0xnot-an-address".into()
        }),
        ("no decimals", PT_SUSDE, |a| {
            a.as_object_mut().unwrap().remove("decimals");
        }),
        ("empty symbol", PT_SUSDE, |a| a["symbol"] = "".into()),
        ("whitespace symbol", PT_SUSDE, |a| a["symbol"] = "  ".into()),
        ("no symbol", PT_SUSDE, |a| {
            a.as_object_mut().unwrap().remove("symbol");
        }),
        ("no chain id", PT_SUSDE, |a| {
            a.as_object_mut().unwrap().remove("chainId");
        }),
        ("zero address that is not NATIVE", PT_PUFETH, |a| {
            a["baseType"] = "GENERIC".into()
        }),
    ];

    for (label, pt, edit) in cases {
        let catalog = edited(ETHEREUM, pt, edit);

        // Act
        let positions = read_holding(Chain::Ethereum, &catalog, &[pt]).await;

        // Assert
        let y = holding(&positions, pt);
        assert_eq!(y.accounting_asset, None, "{label}");
        assert!(
            matches!(y.amount.token.address, Some(TokenAddress::Evm(_))),
            "{label}: the position itself still reads"
        );
    }
}
