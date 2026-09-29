//! Contract-based test: Raydium CLMM read over Solana RPC (MockChainProvider). Account bytes are
//! built here to match the adapter's slice layout, so decoder and fixture stay self-consistent.

use std::sync::Arc;

use base64::Engine;
use gluonscan_core::{Chain, Ctx, Detail, Position, Protocol, ProtocolAdapter, Wallet};
use gluonscan_raydium::RaydiumClmm;
use gluonscan_solana::{find_program_address, pubkey_bytes, pubkey_str, RAYDIUM_CLMM_PROGRAM};
use gluonscan_testing::{Match, MockChainProvider, MockClock, MockHttp};
use rust_decimal::Decimal;

const NFT_MINT: &str = "So11111111111111111111111111111111111111112";

fn account_response(bytes: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"value":{{"data":["{b64}","base64"]}}}}}}"#)
}

fn token_accounts_response(mint: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"value":[{{"account":{{"data":{{"parsed":{{"info":{{"mint":"{mint}","tokenAmount":{{"amount":"1","decimals":0}}}}}}}}}}}}]}}}}"#
    )
}

fn position_account(
    pool: &[u8; 32],
    lower: i32,
    upper: i32,
    liq: u128,
    f0: u64,
    f1: u64,
) -> Vec<u8> {
    let mut b = vec![0u8; 113];
    b[41..73].copy_from_slice(pool);
    b[73..77].copy_from_slice(&lower.to_le_bytes());
    b[77..81].copy_from_slice(&upper.to_le_bytes());
    b[81..97].copy_from_slice(&liq.to_le_bytes());
    b[97..105].copy_from_slice(&f0.to_le_bytes());
    b[105..113].copy_from_slice(&f1.to_le_bytes());
    b
}

fn pool_account(tick: i32, sqrt_x64: u128, mint0: &[u8; 32], mint1: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![0u8; 92];
    b[8..12].copy_from_slice(&tick.to_le_bytes());
    b[12..28].copy_from_slice(&sqrt_x64.to_le_bytes());
    b[28..60].copy_from_slice(mint0);
    b[60..92].copy_from_slice(mint1);
    b
}

fn mint_account(decimals: u8) -> Vec<u8> {
    let mut b = vec![0u8; 45];
    b[44] = decimals;
    b
}

#[tokio::test]
async fn reads_a_clmm_position_from_chain() {
    let program = pubkey_bytes(RAYDIUM_CLMM_PROGRAM).unwrap();
    let nft = pubkey_bytes(NFT_MINT).unwrap();
    let (pda, _) = find_program_address(&[b"position", &nft], &program);
    let pda_str = pubkey_str(&pda);

    let pool = [1u8; 32];
    let mint0 = [2u8; 32];
    let mint1 = [3u8; 32];
    let (pool_str, mint0_str, mint1_str) =
        (pubkey_str(&pool), pubkey_str(&mint0), pubkey_str(&mint1));

    // sqrt price Q64.64 at tick 0 = 2^64; shifted to Q64.96 it equals get_sqrt_ratio_at_tick(0).
    let sqrt_x64: u128 = 1u128 << 64;

    let rpc = MockChainProvider::new()
        .on(
            Match::method("getTokenAccountsByOwner"),
            token_accounts_response(NFT_MINT),
        )
        .on(
            Match::all([
                Match::method("getAccountInfo"),
                Match::body_contains(&pda_str),
            ]),
            account_response(&position_account(
                &pool,
                -60,
                60,
                1_000_000_000,
                1_000_000,
                2_000_000,
            )),
        )
        .on(
            Match::all([
                Match::method("getAccountInfo"),
                Match::body_contains(&pool_str),
            ]),
            account_response(&pool_account(0, sqrt_x64, &mint0, &mint1)),
        )
        .on(
            Match::all([
                Match::method("getAccountInfo"),
                Match::body_contains(&mint0_str),
            ]),
            account_response(&mint_account(9)),
        )
        .on(
            Match::all([
                Match::method("getAccountInfo"),
                Match::body_contains(&mint1_str),
            ]),
            account_response(&mint_account(6)),
        );
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(Arc::new(rpc));

    let reading = RaydiumClmm::new()
        .read(
            &Wallet::Solana(NFT_MINT.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();

    assert_eq!(reading.protocol, Protocol::Raydium);
    assert_eq!(reading.positions.len(), 1);
    let Position::Liquidity(p) = &reading.positions[0] else {
        panic!("expected a liquidity position");
    };
    assert_eq!((p.tick_lower, p.tick_upper, p.tick_current), (-60, 60, 0));
    assert!(p.in_range);
    assert_eq!(p.token0.decimals, 9);
    assert_eq!(p.token1.decimals, 6);
    assert!(p.assets[0].amount > Decimal::ZERO);
    assert!(p.assets[1].amount > Decimal::ZERO);
    assert_eq!(
        p.uncollected_fees[0].amount,
        Decimal::from_i128_with_scale(1_000_000, 9)
    );
    assert_eq!(
        p.uncollected_fees[1].amount,
        Decimal::from_i128_with_scale(2_000_000, 6)
    );
}

#[tokio::test]
async fn rejects_evm_wallet() {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0)))
        .with_rpc(Arc::new(MockChainProvider::new()));
    let err = RaydiumClmm::new()
        .read(
            &Wallet::Evm(gluonscan_core::Address::ZERO),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await
        .expect_err("a Solana wallet is required");
    assert!(!err.is_retryable());
}
