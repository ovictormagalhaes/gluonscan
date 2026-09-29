//! Contract-based test: Solana wallet NFTs over the injected RPC (MockChainProvider). NFT mints
//! are discovered by owner; the Metaplex metadata account is decoded for name + collection.

use std::sync::Arc;

use base64::Engine;
use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_solana::pubkey_str;
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use gluonscan_wallet::SolanaNfts;

const OWNER: &str = "So11111111111111111111111111111111111111112";
const NFT_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const COLLECTION_KEY: [u8; 32] = [9u8; 32];

// One NFT (amount 1, decimals 0) and one fungible token (must be filtered out by discovery).
const ACCOUNTS: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"value":[
  {"account":{"data":{"parsed":{"info":{"mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","tokenAmount":{"amount":"1","decimals":0}}}}}},
  {"account":{"data":{"parsed":{"info":{"mint":"MintFungible","tokenAmount":{"amount":"500","decimals":6}}}}}}
]}}"#;

fn borsh_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn account_info_with_metadata() -> String {
    let mut data = Vec::new();
    data.push(4); // key
    data.extend_from_slice(&[1u8; 32]); // update_authority
    data.extend_from_slice(&[2u8; 32]); // mint
    borsh_string(&mut data, "Mad Lad #42");
    borsh_string(&mut data, "MAD");
    borsh_string(&mut data, "https://example.com/42.json");
    data.extend_from_slice(&500u16.to_le_bytes()); // seller_fee_basis_points
    data.push(0); // creators: None
    data.push(1); // primary_sale_happened
    data.push(1); // is_mutable
    data.push(0); // edition_nonce: None
    data.push(1); // token_standard: Some
    data.push(0);
    data.push(1); // collection: Some
    data.push(1); // verified
    data.extend_from_slice(&COLLECTION_KEY);

    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"value":{{"data":["{b64}","base64"]}}}}}}"#)
}

#[tokio::test]
async fn reads_nft_with_decoded_name_and_collection() {
    let rpc = MockChainProvider::new()
        .on(Match::method("getTokenAccountsByOwner"), ACCOUNTS)
        .on(
            Match::method("getAccountInfo"),
            account_info_with_metadata(),
        );
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = SolanaNfts::new()
        .read(
            &Wallet::Solana(OWNER.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Nfts);
    assert_eq!(reading.positions.len(), 1); // the fungible token is filtered out

    let Position::Nft(nft) = &reading.positions[0] else {
        panic!("expected an NFT position");
    };
    assert_eq!(nft.token_id, NFT_MINT);
    assert_eq!(nft.name.as_deref(), Some("Mad Lad #42"));
    assert_eq!(nft.collection, pubkey_str(&COLLECTION_KEY));
    assert!(nft.floor_price.is_none());
}

#[tokio::test]
async fn missing_metadata_falls_back_to_mint() {
    let no_account = r#"{"jsonrpc":"2.0","id":1,"result":{"value":null}}"#;
    let rpc = MockChainProvider::new()
        .on(Match::method("getTokenAccountsByOwner"), ACCOUNTS)
        .on(Match::method("getAccountInfo"), no_account);
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = SolanaNfts::new()
        .read(
            &Wallet::Solana(OWNER.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    let Position::Nft(nft) = &reading.positions[0] else {
        panic!("expected an NFT position");
    };
    assert_eq!(nft.token_id, NFT_MINT);
    assert_eq!(nft.collection, NFT_MINT); // no verified collection -> mint is the fallback
    assert!(nft.name.is_none());
}

#[tokio::test]
async fn rejects_non_solana_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = SolanaNfts::new()
        .read(
            &Wallet::Bitcoin("bc1qxyz".into()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("a Solana wallet is required");
    assert!(!err.is_retryable());
}
