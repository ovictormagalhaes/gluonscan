//! When a Raydium CLMM position was opened (`created_at`).
//!
//! The answer is the block time of the transaction that opened the position, found from one page of
//! the position NFT mint's signatures: the mint is created by the opening and not referenced by
//! liquidity changes or fee collects, so the oldest successful signature on it is the opening. The
//! read checks that transaction really is an `open_position*` of this position and never walks the
//! history further, so its cost (at most two calls, only on a `Detail::Full` read) is the same for a
//! position opened a minute ago and one with years of history. Anything it cannot confirm leaves
//! `created_at` empty; it is never a guess, and it never fails the read.
//!
//! The position and pool accounts come from `fixtures/position_out_of_range.json` (a mainnet
//! SOL/USDC position) with its liquidity set to zero, so the read needs no tick arrays.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use gluonscan_core::{
    Chain, ChainProvider, Ctx, Detail, Error, LiquidityPosition, Position, ProtocolAdapter, Wallet,
};
use gluonscan_raydium::RaydiumClmm;
use gluonscan_solana::{RAYDIUM_CLMM_PROGRAM, TOKEN_2022_PROGRAM};
use gluonscan_testing::{MockClock, MockHttp};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/position_out_of_range.json");
const WALLET: &str = "3s8ocnrPsHG84dGrvrR427sBKVMv9H3wAJKtb68mjatQ";
/// The page size the read asks for.
const PAGE: usize = 1000;
/// 2026-11-01T00:00:00Z.
const OPENED: i64 = 1_793_491_200;

fn disc(method: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{method}").as_bytes())[..8].to_vec()
}

fn ix(method: &str, accounts: &[&str]) -> Value {
    let mut data = disc(method);
    data.extend_from_slice(&[0; 16]);
    json!({
        "programId": RAYDIUM_CLMM_PROGRAM,
        "accounts": accounts,
        "data": bs58::encode(data).into_string(),
    })
}

fn tx(block_time: Option<i64>, top: Vec<Value>, inner: Vec<Value>) -> Value {
    json!({
        "blockTime": block_time,
        "meta": { "err": null, "innerInstructions": [{ "index": 0, "instructions": inner }] },
        "transaction": { "message": { "instructions": top } },
    })
}

fn sig(signature: &str, block_time: i64, failed: bool) -> Value {
    let err = if failed {
        json!({ "InstructionError": [0, "Custom"] })
    } else {
        Value::Null
    };
    json!({ "signature": signature, "blockTime": block_time, "err": err, "slot": 1 })
}

/// A Solana RPC serving one position NFT's accounts, the mint's signature history and its
/// transactions. Signatures are newest first and paged by `limit`/`before` the way a node does it.
/// Records every call.
struct Node {
    nft: String,
    position: String,
    accounts: HashMap<String, Vec<u8>>,
    signatures: Vec<Value>,
    transactions: HashMap<String, Value>,
    signatures_down: bool,
    /// Replaces the signature page with this `result`.
    signatures_reply: Option<Value>,
    calls: Mutex<Vec<(String, String)>>,
}

impl Node {
    fn new() -> Node {
        let fx: Value = serde_json::from_str(FIXTURE).unwrap();
        let decode = |key: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(fx["accounts"][key].as_str().unwrap())
                .unwrap()
        };
        let pda = fx["position_pda"].as_str().unwrap().to_string();
        let pool = fx["pool"].as_str().unwrap().to_string();
        let mut position = decode(&pda);
        position[81..97].copy_from_slice(&0u128.to_le_bytes());
        Node {
            nft: fx["nft_mint"].as_str().unwrap().to_string(),
            accounts: HashMap::from([(pda.clone(), position), (pool.clone(), decode(&pool))]),
            position: pda,
            signatures: Vec::new(),
            transactions: HashMap::new(),
            signatures_down: false,
            signatures_reply: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The opening transaction of this position, as the program records it.
    fn opening(&self, method: &str, block_time: Option<i64>) -> Value {
        tx(
            block_time,
            vec![ix(method, &[WALLET, WALLET, &self.nft, &self.position])],
            vec![],
        )
    }

    /// Prepend `n` newer successful transfers of the NFT (signatures `transfer-<i>`).
    fn with_transfers(mut self, n: usize) -> Node {
        let newer = (0..n).map(|i| sig(&format!("transfer-{i}"), OPENED + 1 + i as i64, false));
        let mut all: Vec<Value> = newer.rev().collect();
        all.append(&mut self.signatures);
        self.signatures = all;
        self
    }

    fn calls(&self, method: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn lookup_calls(&self) -> usize {
        self.calls("getSignaturesForAddress").len() + self.calls("getTransaction").len()
    }
}

/// A chain where the position was opened at `OPENED` by `open_position_v2`, with nothing after it.
fn opened() -> Node {
    let mut chain = Node::new();
    chain.signatures = vec![sig("open", OPENED, false)];
    let opening = chain.opening("open_position_v2", Some(OPENED));
    chain.transactions.insert("open".into(), opening);
    chain
}

#[async_trait]
impl ChainProvider for Node {
    async fn call(&self, _chain: Chain, method: &str, params: String) -> Result<String, Error> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        let args: Value = serde_json::from_str(&params).unwrap();
        let result = match method {
            "getTokenAccountsByOwner" if params.contains(TOKEN_2022_PROGRAM) => json!({ "value": [{
                "account": { "data": { "parsed": { "info": {
                    "mint": self.nft,
                    "tokenAmount": { "amount": "1", "decimals": 0 },
                } } } },
            }] }),
            "getTokenAccountsByOwner" => json!({ "value": [] }),
            "getAccountInfo" => {
                let data = self.accounts.get(args[0].as_str().unwrap());
                json!({ "value": data.map(|d| account(d)) })
            }
            "getMultipleAccounts" => {
                let values: Vec<Value> = args[0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| {
                        self.accounts
                            .get(k.as_str().unwrap())
                            .map_or(Value::Null, |d| account(d))
                    })
                    .collect();
                json!({ "context": { "slot": 1 }, "value": values })
            }
            "getSignaturesForAddress" => {
                if self.signatures_down {
                    return Err(Error::Transient {
                        message: "503".into(),
                        retry_after: None,
                    });
                }
                assert_eq!(args[0], self.nft.as_str(), "the lookup scans the NFT mint");
                if let Some(reply) = &self.signatures_reply {
                    return Ok(json!({ "jsonrpc": "2.0", "id": 1, "error": reply }).to_string());
                }
                let limit = args[1]["limit"].as_u64().unwrap() as usize;
                let start = match args[1].get("before").and_then(|b| b.as_str()) {
                    Some(before) => {
                        self.signatures
                            .iter()
                            .position(|s| s["signature"] == before)
                            .unwrap()
                            + 1
                    }
                    None => 0,
                };
                let end = (start + limit).min(self.signatures.len());
                json!(self.signatures[start..end])
            }
            "getTransaction" => self
                .transactions
                .get(args[0].as_str().unwrap())
                .cloned()
                .unwrap_or(Value::Null),
            other => panic!("unexpected RPC method {other}"),
        };
        Ok(json!({ "jsonrpc": "2.0", "id": 1, "result": result }).to_string())
    }
}

fn account(bytes: &[u8]) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({ "data": [b64, "base64"] })
}

async fn read(chain: Arc<Node>, detail: Detail) -> LiquidityPosition {
    let cx = Ctx::new(
        Arc::new(MockHttp::new()),
        Arc::new(MockClock(OPENED + 86_400)),
    )
    .with_rpc(chain);
    let reading = RaydiumClmm::new()
        .read(
            &Wallet::Solana(WALLET.to_string()),
            Chain::Solana,
            detail,
            &cx,
        )
        .await
        .expect("read")
        .into_inner();
    assert_eq!(reading.positions.len(), 1);
    match reading.positions.into_iter().next().unwrap() {
        Position::Liquidity(p) => p,
        other => panic!("expected a liquidity position, got {other:?}"),
    }
}

async fn created_at(chain: Node) -> (Option<i64>, Arc<Node>) {
    let chain = Arc::new(chain);
    let p = read(chain.clone(), Detail::Full).await;
    (p.created_at, chain)
}

#[tokio::test]
async fn new_position_is_created_at_its_opening_block_time() {
    let (at, chain) = created_at(opened()).await;
    assert_eq!(at, Some(OPENED));
    assert_eq!(chain.calls("getSignaturesForAddress").len(), 1);
    assert_eq!(chain.calls("getTransaction").len(), 1);
}

#[tokio::test]
async fn older_position_is_created_at_its_opening_not_its_latest_activity() {
    // Transfers after the opening, and a failed attempt to open before it.
    let mut chain = opened().with_transfers(40);
    chain.signatures.push(sig("failed-open", OPENED - 60, true));
    let (at, chain) = created_at(chain).await;
    assert_eq!(at, Some(OPENED));
    let fetched = chain.calls("getTransaction");
    assert_eq!(fetched.len(), 1, "only the opening is fetched: {fetched:?}");
    assert!(fetched[0].contains("\"open\""));
}

#[tokio::test]
async fn every_way_to_open_a_position_counts_including_through_a_cpi() {
    for method in [
        "open_position",
        "open_position_v2",
        "open_position_with_metadata",
        "open_position_with_token22_nft",
    ] {
        let mut chain = opened();
        let opening = chain.opening(method, Some(OPENED));
        chain.transactions.insert("open".into(), opening);
        assert_eq!(created_at(chain).await.0, Some(OPENED), "{method}");
    }
    // An aggregator or vault opening the position through a cross-program call.
    let mut chain = opened();
    let through_cpi = tx(
        Some(OPENED),
        vec![
            json!({ "programId": "Aggregator1111111111111111111111111111111111", "accounts": [], "data": "" }),
        ],
        vec![ix(
            "open_position_v2",
            &[WALLET, &chain.nft, &chain.position],
        )],
    );
    chain.transactions.insert("open".into(), through_cpi);
    assert_eq!(created_at(chain).await.0, Some(OPENED));
}

#[tokio::test]
async fn history_longer_than_one_page_is_not_walked_and_stays_unknown() {
    // 2,500 later transfers: the opening is on the third page. The read does not page to it.
    let (at, chain) = created_at(opened().with_transfers(2_500)).await;
    assert_eq!(at, None, "an opening the read did not see is never guessed");
    let pages = chain.calls("getSignaturesForAddress");
    assert_eq!(pages.len(), 1, "one page only: {pages:?}");
    assert!(!pages[0].contains("before"), "no pagination");
    assert!(chain.calls("getTransaction").is_empty());
}

#[tokio::test]
async fn lookup_cost_does_not_grow_with_the_history() {
    let mut costs = Vec::new();
    for later in [0, 1, 10, PAGE - 2, PAGE - 1, PAGE, 5 * PAGE] {
        let (at, chain) = created_at(opened().with_transfers(later)).await;
        let seen = later + 1 < PAGE;
        assert_eq!(at, seen.then_some(OPENED), "{later} later signatures");
        assert_eq!(chain.calls("getSignaturesForAddress").len(), 1, "{later}");
        costs.push(chain.lookup_calls());
    }
    assert_eq!(costs, [2, 2, 2, 2, 1, 1, 1]);
}

#[tokio::test]
async fn every_full_read_costs_the_same_bounded_lookup() {
    // The adapter keeps no state: each read looks the opening up again, at the same cost.
    let chain = Arc::new(opened().with_transfers(10));
    for n in 1..=3 {
        assert_eq!(
            read(chain.clone(), Detail::Full).await.created_at,
            Some(OPENED)
        );
        assert_eq!(chain.lookup_calls(), 2 * n);
    }
}

#[tokio::test]
async fn a_read_below_full_detail_does_not_look_the_opening_up() {
    for detail in [Detail::Presence, Detail::Summary] {
        let chain = Arc::new(opened());
        let p = read(chain.clone(), detail).await;
        assert_eq!(p.created_at, None, "{detail:?}");
        assert_eq!(chain.lookup_calls(), 0, "{detail:?}");
    }
}

#[tokio::test]
async fn unavailable_source_reads_the_position_without_created_at() {
    let mut down = opened();
    down.signatures_down = true;
    let down = Arc::new(down);
    let p = read(down.clone(), Detail::Full).await;
    assert_eq!(p.created_at, None, "the position is still read");
    assert_eq!(p.id.as_deref(), Some(down.nft.as_str()));
    assert!(down.calls("getTransaction").is_empty());

    // A reply that is not a signature list.
    let node = Node {
        signatures_reply: Some(json!({ "code": -32005, "message": "busy" })),
        ..opened()
    };
    assert_eq!(created_at(node).await.0, None);
}

#[tokio::test]
async fn opening_not_yet_finalized_or_unreadable_is_unknown() {
    // No finalized signature yet (the opening is only confirmed).
    let mut chain = Node::new();
    chain.signatures.clear();
    let (at, chain) = created_at(chain).await;
    assert_eq!(at, None);
    assert!(chain.calls("getTransaction").is_empty());

    // Only a failed attempt so far.
    let mut chain = Node::new();
    chain.signatures = vec![sig("failed-open", OPENED, true)];
    let (at, chain) = created_at(chain).await;
    assert_eq!(at, None);
    assert!(chain.calls("getTransaction").is_empty());

    // The node cannot return the transaction yet.
    let mut chain = opened();
    chain.transactions.clear();
    assert_eq!(created_at(chain).await.0, None);

    // The opening carries no block time.
    let mut chain = opened();
    let opening = chain.opening("open_position_v2", None);
    chain.transactions.insert("open".into(), opening);
    assert_eq!(created_at(chain).await.0, None);

    // The opening failed on-chain although its signature entry says otherwise.
    let mut chain = opened();
    let mut opening = chain.opening("open_position_v2", Some(OPENED));
    opening["meta"]["err"] = json!({ "InstructionError": [0, "Custom"] });
    chain.transactions.insert("open".into(), opening);
    assert_eq!(created_at(chain).await.0, None);
}

#[tokio::test]
async fn oldest_transaction_that_is_not_this_positions_opening_is_unknown() {
    let not_openings = |chain: &Node| {
        vec![
            // A transfer: this node's history does not reach back to the opening.
            tx(Some(OPENED), vec![], vec![]),
            // A liquidity change on the CLMM program, not an opening.
            tx(
                Some(OPENED),
                vec![ix(
                    "increase_liquidity_v2",
                    &[WALLET, &chain.nft, &chain.position],
                )],
                vec![],
            ),
            // An opening of another position that happens to list this mint.
            tx(
                Some(OPENED),
                vec![ix("open_position_v2", &[WALLET, &chain.nft, WALLET])],
                vec![],
            ),
            // An opening of this position on another program.
            {
                let mut other = chain.opening("open_position_v2", Some(OPENED));
                other["transaction"]["message"]["instructions"][0]["programId"] =
                    json!("Other111111111111111111111111111111111111111");
                other
            },
        ]
    };
    let count = not_openings(&Node::new()).len();
    for i in 0..count {
        let mut chain = opened();
        let tx = not_openings(&chain).swap_remove(i);
        chain.transactions.insert("open".into(), tx);
        assert_eq!(created_at(chain).await.0, None, "case {i}");
    }
}
