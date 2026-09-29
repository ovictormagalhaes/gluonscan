//! Contract-based test: EVM wallet NFTs over a mock HTTP transport (header-authenticated).
//! Spam NFTs and protocol-position contracts (a Uniswap V3 LP) are excluded.

use std::sync::Arc;

use gluonscan_core::{Address, Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_testing::{Match, MockClock, MockHttp};
use gluonscan_wallet::EvmNfts;

// One real collectible, one spam item, and one Uniswap V3 position NFT (must be skipped: it is
// read as a liquidity position by the Uniswap adapter, not counted here as an NFT).
const NFTS: &str = r#"{"result":[
  {"token_address":"0xBC4CA0EdA7647A8aB7C2061c2E118A18a936f13D","token_id":"7211","name":"Bored Ape","possible_spam":false},
  {"token_address":"0x0000000000000000000000000000000000000bad","token_id":"1","name":"Free Airdrop","possible_spam":true},
  {"token_address":"0xC36442b4a4522E871399CD717aBDD847Ab11FE88","token_id":"999999","name":"Uniswap V3 Positions NFT-V1","possible_spam":false}
],"cursor":null}"#;

#[tokio::test]
async fn reads_collectibles_skipping_spam_and_protocol_positions() {
    let http = MockHttp::new().on(Match::primary_contains("/nft"), NFTS);
    let cx = Ctx::new(Arc::new(http), Arc::new(MockClock(0)));

    let reading = EvmNfts::new("test-key")
        .read(
            &Wallet::Evm(Address::ZERO),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Nfts);
    assert_eq!(reading.positions.len(), 1); // spam + Uniswap position NFT are excluded

    let Position::Nft(nft) = &reading.positions[0] else {
        panic!("expected an NFT position");
    };
    assert_eq!(nft.collection, "0xBC4CA0EdA7647A8aB7C2061c2E118A18a936f13D");
    assert_eq!(nft.token_id, "7211");
    assert_eq!(nft.name.as_deref(), Some("Bored Ape"));
    assert!(nft.floor_price.is_none()); // pricing is a separate operation
}

#[tokio::test]
async fn rejects_non_evm_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)));
    let err = EvmNfts::new("k")
        .read(
            &Wallet::Bitcoin("bc1qxyz".into()),
            Chain::Ethereum,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("an EVM wallet is required");
    assert!(!err.is_retryable());
}
