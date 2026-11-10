//! Uncollected fees of a Raydium CLMM position, read from real mainnet accounts.
//!
//! `token_fees_owed_0/1` on a `PersonalPositionState` only move when the position's liquidity
//! changes (`increase_liquidity` / `decrease_liquidity`), so a position nobody touched since its
//! last change has accrued fees the owed fields do not show. The claimable amount is
//! `owed + liquidity * (fee_growth_inside_now - fee_growth_inside_last) / 2^64`, with
//! `fee_growth_inside_now` built from the pool's `fee_growth_global` and the two ticks'
//! `fee_growth_outside`.
//!
//! Each fixture holds the position, pool and both tick array accounts of one SOL/USDC position in
//! pool `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv` (2026-11-10), all at the same slot. They
//! were captured as the post-state of a `simulateTransaction` of `decrease_liquidity_v2` with
//! `liquidity = 0` signed by the NFT holder (signature verification off): a zero-liquidity decrease
//! leaves the pool and tick arrays unchanged, and the program's own `DecreaseLiquidityEvent`
//! reported the `fee_amount_0/1` it would pay out. That is `claimable_fees_raw`; the position
//! account is the pre-simulation state.
//!
//! - `position_in_range.json`: in range, owed fields both 0, about 30.38 SOL and 2,628.88 USDC
//!   claimable. Its fee growth inside wraps past 2^128, so the subtraction must wrap.
//! - `position_out_of_range.json`: price below the range (both outside rules on their "else"
//!   side for the lower tick), non-zero owed fields plus fees accrued while it was in range.
//!
//! The read must take the position, pool and tick arrays from one `getMultipleAccounts` call (one
//! slot). The mock RPC serves separate `getAccountInfo` calls and that call from two views, so a
//! swap that lands between separate reads can be staged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use gluonscan_core::{
    Chain, ChainProvider, Ctx, Detail, Error, LiquidityPosition, Position, ProtocolAdapter, Wallet,
};
use gluonscan_math::get_sqrt_ratio_at_tick;
use gluonscan_raydium::RaydiumClmm;
use gluonscan_solana::TOKEN_2022_PROGRAM;
use gluonscan_testing::{MockClock, MockHttp};
use serde_json::{json, Value};

const IN_RANGE: &str = include_str!("fixtures/position_in_range.json");
const OUT_OF_RANGE: &str = include_str!("fixtures/position_out_of_range.json");
const WALLET: &str = "3s8ocnrPsHG84dGrvrR427sBKVMv9H3wAJKtb68mjatQ";

struct Fixture {
    json: Value,
    position: Vec<u8>,
    pool: Vec<u8>,
    tick_array_lower: (String, Vec<u8>),
    tick_array_upper: (String, Vec<u8>),
}

fn decode(b64: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("fixture account is base64")
}

fn load(raw: &str) -> Fixture {
    let json: Value = serde_json::from_str(raw).expect("fixture is JSON");
    let accounts = json["accounts"].as_object().expect("accounts");
    let pool_id = json["pool"].as_str().unwrap();
    let position_pda = json["position_pda"].as_str().unwrap();
    let mut tick_arrays: Vec<(String, Vec<u8>)> = accounts
        .iter()
        .filter(|(k, _)| k.as_str() != pool_id && k.as_str() != position_pda)
        .map(|(k, v)| (k.clone(), decode(v.as_str().unwrap())))
        .collect();
    let start = |d: &[u8]| i32::from_le_bytes(d[40..44].try_into().unwrap());
    tick_arrays.sort_by_key(|(_, d)| start(d));
    assert_eq!(tick_arrays.len(), 2, "two distinct tick arrays");
    let tick_array_upper = tick_arrays.pop().unwrap();
    let tick_array_lower = tick_arrays.pop().unwrap();
    Fixture {
        position: decode(accounts[position_pda].as_str().unwrap()),
        pool: decode(accounts[pool_id].as_str().unwrap()),
        json,
        tick_array_lower,
        tick_array_upper,
    }
}

impl Fixture {
    fn str(&self, key: &str) -> &str {
        self.json[key].as_str().unwrap()
    }

    fn pair(&self, key: &str) -> [u128; 2] {
        let a = self.json[key].as_array().unwrap();
        [0, 1].map(|i| a[i].as_str().unwrap().parse().unwrap())
    }
}

fn account_value(bytes: &[u8]) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({ "data": [b64, "base64"] })
}

/// Serves the wallet's position NFT and its Solana accounts from two views of the chain, the way a
/// live RPC can: each `getAccountInfo` answers from `single` (separate calls may land on separate
/// slots) and `getMultipleAccounts` answers every account of the call from `snapshot` (one slot).
/// Records every call.
struct Accounts {
    nft: String,
    single: HashMap<String, Vec<u8>>,
    snapshot: HashMap<String, Vec<u8>>,
    calls: Mutex<Vec<(String, String)>>,
}

impl Accounts {
    fn calls(&self, method: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .collect()
    }
}

#[async_trait]
impl ChainProvider for Accounts {
    async fn call(&self, _chain: Chain, method: &str, params: String) -> Result<String, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        let args: Value = serde_json::from_str(&params).expect("params are JSON");
        let result = match method {
            "getTokenAccountsByOwner" if params.contains(TOKEN_2022_PROGRAM) => json!({ "value": [{
                "account": { "data": { "parsed": { "info": {
                    "mint": self.nft,
                    "tokenAmount": { "amount": "1", "decimals": 0 },
                } } } },
            }] }),
            "getTokenAccountsByOwner" => json!({ "value": [] }),
            "getAccountInfo" => {
                let key = args[0].as_str().expect("one pubkey");
                json!({ "value": self.single.get(key).map(|b| account_value(b)) })
            }
            "getMultipleAccounts" => {
                let keys = args[0].as_array().expect("a pubkey list");
                let values: Vec<Value> = keys
                    .iter()
                    .map(|k| {
                        self.snapshot
                            .get(k.as_str().expect("pubkey"))
                            .map_or(Value::Null, |b| account_value(b))
                    })
                    .collect();
                json!({ "context": { "slot": 1 }, "value": values })
            }
            other => panic!("unexpected RPC method {other}"),
        };
        Ok(json!({ "jsonrpc": "2.0", "id": 1, "result": result }).to_string())
    }
}

/// Both views hold the fixture's accounts at its one slot; `None` leaves a tick array missing.
fn rpc(fx: &Fixture, lower: Option<&[u8]>, upper: Option<&[u8]>) -> Accounts {
    let mut accounts = HashMap::from([
        (fx.str("position_pda").to_string(), fx.position.clone()),
        (fx.str("pool").to_string(), fx.pool.clone()),
    ]);
    for (address, data) in [
        (&fx.tick_array_lower.0, lower),
        (&fx.tick_array_upper.0, upper),
    ] {
        if let Some(data) = data {
            accounts.insert(address.clone(), data.to_vec());
        }
    }
    Accounts {
        nft: fx.str("nft_mint").to_string(),
        single: accounts.clone(),
        snapshot: accounts,
        calls: Mutex::new(Vec::new()),
    }
}

fn full_rpc(fx: &Fixture) -> Accounts {
    rpc(
        fx,
        Some(&fx.tick_array_lower.1),
        Some(&fx.tick_array_upper.1),
    )
}

async fn read_with(provider: Arc<dyn ChainProvider>) -> Result<LiquidityPosition, Error> {
    let cx = Ctx::new(Arc::new(MockHttp::new()), Arc::new(MockClock(0))).with_rpc(provider);
    let reading = RaydiumClmm::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            Detail::Full,
            &cx,
        )
        .await?
        .into_inner();
    assert_eq!(reading.positions.len(), 1);
    match reading.positions.into_iter().next().unwrap() {
        Position::Liquidity(p) => Ok(p),
        other => panic!("expected a liquidity position, got {other:?}"),
    }
}

fn fees_raw(p: &LiquidityPosition) -> [u128; 2] {
    [0, 1].map(|i| p.uncollected_fees[i].raw.to::<u128>())
}

#[tokio::test]
async fn untouched_in_range_position_reports_fees_accrued_since_its_checkpoint() {
    let fx = load(IN_RANGE);
    assert_eq!(
        fx.pair("fees_owed_raw"),
        [0, 0],
        "owed fields alone read zero"
    );
    let p = read_with(Arc::new(full_rpc(&fx))).await.expect("read");
    assert!(p.in_range);
    assert_eq!(p.id.as_deref(), Some(fx.str("nft_mint")));
    // 30.379588775 SOL and 2,628.882180 USDC, exactly what the program pays on a fee collect.
    assert_eq!(fees_raw(&p), fx.pair("claimable_fees_raw"));
    assert_eq!(fees_raw(&p), [30_379_588_775, 2_628_882_180]);
}

#[tokio::test]
async fn out_of_range_position_adds_accrued_fees_to_its_owed_fields() {
    let fx = load(OUT_OF_RANGE);
    assert_eq!(fx.pair("fees_owed_raw"), [440_176, 100_122]);
    let p = read_with(Arc::new(full_rpc(&fx))).await.expect("read");
    assert!(!p.in_range);
    assert!(p.tick_current < p.tick_lower, "price sits below the range");
    assert_eq!(fees_raw(&p), fx.pair("claimable_fees_raw"));
    assert_eq!(fees_raw(&p), [7_532_479_703, 1_231_211_995]);
}

#[tokio::test]
async fn position_checkpointed_at_current_growth_reports_exactly_its_owed_fields() {
    let mut fx = load(OUT_OF_RANGE);
    // Move the checkpoint (fee_growth_inside_0/1_last_x64 at [97..129]) to the growth inside the
    // range as of the fixture's slot: nothing has accrued since, so the owed fields are the answer.
    let now = fx.pair("fee_growth_inside_now_x64");
    fx.position[97..113].copy_from_slice(&now[0].to_le_bytes());
    fx.position[113..129].copy_from_slice(&now[1].to_le_bytes());
    let p = read_with(Arc::new(full_rpc(&fx))).await.expect("read");
    assert_eq!(fees_raw(&p), fx.pair("fees_owed_raw"));
}

#[tokio::test]
async fn position_without_liquidity_reports_its_owed_fields_without_reading_ticks() {
    let mut fx = load(OUT_OF_RANGE);
    fx.position[81..97].copy_from_slice(&0u128.to_le_bytes());
    // Without liquidity nothing accrues; the ticks may even be uninitialized, so none are read.
    let accounts = Arc::new(rpc(&fx, None, None));
    let p = read_with(accounts.clone()).await.expect("read");
    assert_eq!(fees_raw(&p), fx.pair("fees_owed_raw"));
    let calls = accounts.calls.lock().unwrap().clone();
    for (_, params) in calls {
        assert!(!params.contains(&fx.tick_array_lower.0));
        assert!(!params.contains(&fx.tick_array_upper.0));
    }
}

#[tokio::test]
async fn missing_tick_array_fails_closed_instead_of_reporting_owed_fees_only() {
    let fx = load(IN_RANGE);
    let err = read_with(Arc::new(rpc(&fx, None, Some(&fx.tick_array_upper.1))))
        .await
        .expect_err("a missing tick array must not read as zero fees");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("tick array")),
        "expected Integrity(tick array), got {err:?}"
    );
}

#[tokio::test]
async fn tick_array_of_another_pool_fails_closed() {
    let fx = load(IN_RANGE);
    let mut upper = fx.tick_array_upper.1.clone();
    upper[8..40].copy_from_slice(&[7u8; 32]);
    let err = read_with(Arc::new(rpc(
        &fx,
        Some(&fx.tick_array_lower.1),
        Some(&upper),
    )))
    .await
    .expect_err("a tick array of another pool must be rejected");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("tick array")),
        "expected Integrity(tick array), got {err:?}"
    );
}

#[tokio::test]
async fn uninitialized_position_tick_fails_closed() {
    let fx = load(IN_RANGE);
    let tick_lower = fx.json["tick_lower"].as_i64().unwrap() as i32;
    let mut lower = fx.tick_array_lower.1.clone();
    let start = i32::from_le_bytes(lower[40..44].try_into().unwrap());
    // Tick spacing is 1 in this pool; TickState is 168 bytes from offset 44, liquidity_gross at +20.
    let at = 44 + (tick_lower - start) as usize * 168;
    lower[at + 20..at + 36].copy_from_slice(&0u128.to_le_bytes());
    let err = read_with(Arc::new(rpc(
        &fx,
        Some(&lower),
        Some(&fx.tick_array_upper.1),
    )))
    .await
    .expect_err("a position tick with no liquidity cannot be priced");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("tick")),
        "expected Integrity(tick), got {err:?}"
    );
}

/// Fails every read of one address with a transient transport error, delegating the rest.
struct FailingAddress {
    inner: Accounts,
    address: String,
}

#[async_trait]
impl ChainProvider for FailingAddress {
    async fn call(&self, chain: Chain, method: &str, params: String) -> Result<String, Error> {
        if params.contains(&self.address) {
            return Err(Error::Transient {
                message: "connection reset".into(),
                retry_after: None,
            });
        }
        self.inner.call(chain, method, params).await
    }
}

#[tokio::test]
async fn tick_array_transport_failure_propagates_as_retryable() {
    let fx = load(IN_RANGE);
    let provider = FailingAddress {
        address: fx.tick_array_upper.0.clone(),
        inner: full_rpc(&fx),
    };
    let err = read_with(Arc::new(provider))
        .await
        .expect_err("a failed tick array read must fail the read");
    assert!(
        err.is_retryable(),
        "expected a retryable error, got {err:?}"
    );
}

fn u128_at(data: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(data[at..at + 16].try_into().unwrap())
}

/// The pool and lower tick array after a swap that moves the price just below the in-range
/// position's lower tick, as the program leaves them: the crossed tick's fee growth outside flips
/// to `global - outside` and the pool's current tick and price move. No fees are charged, so the
/// growth inside the range, and with it the claimable amount, is unchanged.
fn after_crossing_the_lower_tick(fx: &Fixture) -> (Vec<u8>, Vec<u8>) {
    let tick_lower = fx.json["tick_lower"].as_i64().unwrap() as i32;
    let mut pool = fx.pool.clone();
    let mut lower = fx.tick_array_lower.1.clone();
    let start = i32::from_le_bytes(lower[40..44].try_into().unwrap());
    let spacing = i32::from(u16::from_le_bytes(pool[235..237].try_into().unwrap()));
    let tick = 44 + ((tick_lower - start) / spacing) as usize * 168;
    for (global_at, outside_at) in [(277, tick + 36), (293, tick + 52)] {
        let flipped = u128_at(&pool, global_at).wrapping_sub(u128_at(&lower, outside_at));
        lower[outside_at..outside_at + 16].copy_from_slice(&flipped.to_le_bytes());
    }
    let tick_current = tick_lower - 1;
    let sqrt_x64 = (get_sqrt_ratio_at_tick(tick_current) >> 32usize).to::<u128>();
    pool[253..269].copy_from_slice(&sqrt_x64.to_le_bytes());
    pool[269..273].copy_from_slice(&tick_current.to_le_bytes());
    (pool, lower)
}

#[tokio::test]
async fn swap_crossing_a_position_tick_between_reads_cannot_skew_the_fees() {
    let fx = load(IN_RANGE);
    let (pool_after, lower_after) = after_crossing_the_lower_tick(&fx);
    let mut accounts = full_rpc(&fx);
    // Separate reads straddle the swap: the pool from before it, the tick array from after it.
    // Combined, the crossed tick's flipped outside value meets the old current tick.
    accounts
        .single
        .insert(fx.tick_array_lower.0.clone(), lower_after.clone());
    accounts
        .snapshot
        .insert(fx.str("pool").to_string(), pool_after);
    accounts
        .snapshot
        .insert(fx.tick_array_lower.0.clone(), lower_after);
    let p = read_with(Arc::new(accounts))
        .await
        .expect("a consistent snapshot reads cleanly");
    assert_eq!(fees_raw(&p), fx.pair("claimable_fees_raw"));
    let tick_lower = fx.json["tick_lower"].as_i64().unwrap() as i32;
    assert_eq!(
        p.tick_current,
        tick_lower - 1,
        "current tick from the snapshot"
    );
    assert!(!p.in_range);
}

#[tokio::test]
async fn position_pool_and_tick_arrays_are_read_in_one_call() {
    let fx = load(IN_RANGE);
    let accounts = Arc::new(full_rpc(&fx));
    read_with(accounts.clone()).await.expect("read");
    let batches = accounts.calls("getMultipleAccounts");
    assert_eq!(
        batches.len(),
        1,
        "one getMultipleAccounts call: {batches:?}"
    );
    for address in [
        fx.str("position_pda"),
        fx.str("pool"),
        &fx.tick_array_lower.0,
        &fx.tick_array_upper.0,
    ] {
        assert!(batches[0].contains(address), "{address} in the snapshot");
    }
    for single in accounts.calls("getAccountInfo") {
        assert!(!single.contains(&fx.tick_array_lower.0));
        assert!(!single.contains(&fx.tick_array_upper.0));
    }
}

#[tokio::test]
async fn position_values_come_from_the_same_snapshot_as_the_pool() {
    let fx = load(IN_RANGE);
    let mut accounts = full_rpc(&fx);
    // A fee collect after discovery: the checkpoint moved to the current growth and the owed
    // fields were paid out. The discovery copy would count those fees a second time.
    let mut collected = fx.position.clone();
    let now = fx.pair("fee_growth_inside_now_x64");
    collected[97..113].copy_from_slice(&now[0].to_le_bytes());
    collected[113..129].copy_from_slice(&now[1].to_le_bytes());
    collected[129..145].copy_from_slice(&[0u8; 16]);
    accounts
        .snapshot
        .insert(fx.str("position_pda").to_string(), collected);
    let p = read_with(Arc::new(accounts)).await.expect("read");
    assert_eq!(fees_raw(&p), [0, 0]);
}

#[tokio::test]
async fn position_closed_between_reads_fails_as_retryable() {
    let fx = load(IN_RANGE);
    let mut accounts = full_rpc(&fx);
    accounts.snapshot.remove(fx.str("position_pda"));
    let err = read_with(Arc::new(accounts))
        .await
        .expect_err("a position gone from the snapshot must not be read from its older copy");
    assert!(
        err.is_retryable(),
        "expected a retryable error, got {err:?}"
    );
}

#[tokio::test]
async fn position_given_liquidity_between_reads_fails_as_retryable() {
    let fx = load(IN_RANGE);
    let mut accounts = full_rpc(&fx);
    let mut empty = fx.position.clone();
    empty[81..97].copy_from_slice(&0u128.to_le_bytes());
    accounts
        .single
        .insert(fx.str("position_pda").to_string(), empty);
    let err = read_with(Arc::new(accounts))
        .await
        .expect_err("liquidity added after discovery leaves the snapshot without its ticks");
    assert!(
        err.is_retryable(),
        "expected a retryable error, got {err:?}"
    );
}

#[tokio::test]
async fn pool_tick_spacing_that_differs_between_reads_fails_closed() {
    let fx = load(IN_RANGE);
    let mut accounts = full_rpc(&fx);
    let mut pool = fx.pool.clone();
    pool[235..237].copy_from_slice(&10u16.to_le_bytes());
    accounts.snapshot.insert(fx.str("pool").to_string(), pool);
    let err = read_with(Arc::new(accounts))
        .await
        .expect_err("tick arrays located with another spacing cannot be used");
    assert!(
        matches!(&err, Error::Integrity { message } if message.contains("tick spacing")),
        "expected Integrity(tick spacing), got {err:?}"
    );
}
