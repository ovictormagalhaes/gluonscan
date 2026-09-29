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
async fn returns_collectibles_flags_spam_records_protocol_receipts() {
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
    // The real NFT and the spam NFT are BOTH returned (spam flagged, not dropped); only the Uniswap
    // position NFT is excluded — and recorded as a receipt token so the consumer can dedup.
    assert_eq!(reading.positions.len(), 2);

    let ape = reading
        .positions
        .iter()
        .find_map(|p| match p {
            Position::Nft(n) if n.token_id == "7211" => Some(n),
            _ => None,
        })
        .expect("the real collectible");
    assert_eq!(ape.collection, "0xBC4CA0EdA7647A8aB7C2061c2E118A18a936f13D");
    assert_eq!(ape.name.as_deref(), Some("Bored Ape"));
    assert_eq!(ape.possible_spam, Some(false));
    assert!(ape.floor_price.is_none());

    let spam = reading
        .positions
        .iter()
        .find_map(|p| match p {
            Position::Nft(n) if n.token_id == "1" => Some(n),
            _ => None,
        })
        .expect("the spam NFT is returned, not dropped");
    assert_eq!(spam.possible_spam, Some(true));

    // The Uniswap V3 NonfungiblePositionManager is recorded as a receipt token, not a collectible.
    assert_eq!(reading.receipt_tokens.len(), 1);
    assert_eq!(
        format!("{:#x}", reading.receipt_tokens[0]),
        "0xc36442b4a4522e871399cd717abdd847ab11fe88"
    );
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
