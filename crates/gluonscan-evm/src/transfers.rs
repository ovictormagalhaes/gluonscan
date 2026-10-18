//! Cost-basis chain reads over the injected [`ChainProvider`]: a wallet's inbound ERC-20 transfers
//! (`alchemy_getAssetTransfers`), a transaction's sender, native value and ERC-20 `Transfer` logs
//! ([`read_receipt`]), and the latest block number. The RPC URL carries the provider key — no API key
//! is handled here. The consumer owns the cost-basis math (pairing the inbound leg with what was paid,
//! pricing, averaging).
//!
//! [`read_receipt`] reads each distinct token's `decimals()` at the latest block. A token that gives
//! no usable answer (no code, a revert, an invalid opcode, a return that is not one word or exceeds
//! `u8`, or a log address that is not an address) has `None`. Any other failure of one token's read
//! fails the whole read, retryably: a node that keeps failing that way means the receipt never
//! resolves, which the caller should bound rather than read as "nothing was paid".

use std::collections::HashMap;
use std::str::FromStr;

use alloy_primitives::{hex, Address, U256};
use chrono::DateTime;
use gluonscan_core::{Chain, ChainProvider, Error};

/// `keccak256("Transfer(address,address,uint256)")` — the ERC-20 Transfer log topic0.
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// An inbound ERC-20 transfer to the queried wallet (a purchase leg of a position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetTransfer {
    /// Transaction hash.
    pub tx: String,
    /// Block number.
    pub block: u64,
    /// Block timestamp, unix seconds (0 when the indexer omits it).
    pub block_timestamp: i64,
    /// Raw transferred amount as hex (`0x…`), in the token's base units.
    pub value_raw: String,
    /// The token's decimals as reported by the indexer.
    pub token_decimals: u8,
}

/// One ERC-20 `Transfer` log decoded from a transaction receipt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptTransfer {
    /// The ERC-20 contract address (lowercased `0x…`).
    pub token: String,
    /// Sender (lowercased `0x…`).
    pub from: String,
    /// Recipient (lowercased `0x…`).
    pub to: String,
    /// Raw amount as hex (`0x…`).
    pub value_raw: String,
    /// The token's `decimals()` at the latest block, or `None` when the contract gives no usable
    /// answer (no code at the address, a revert, an invalid opcode, a return that is not one word or
    /// does not fit a `u8`) or the log's address is not an address. Never guessed.
    pub token_decimals: Option<u8>,
}

/// A mined transaction as a cost-basis read sees it: who sent it, the native coin it carried and
/// its ERC-20 `Transfer` logs. A purchase paid in the native coin has no outbound ERC-20 leg from
/// the wallet (the router wraps it, and WETH emits `Deposit`, not `Transfer`), so `from` and
/// `value_wei` are what tell it apart from a plain transfer in.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptRead {
    /// The transaction's sender, from the receipt.
    pub from: Address,
    /// The called address, from the receipt; `None` for a contract creation.
    pub to: Option<Address>,
    /// The native coin the transaction carried, in wei (`eth_getTransactionByHash`).
    pub value_wei: U256,
    /// The ERC-20 `Transfer` logs, in log order.
    pub transfers: Vec<ReceiptTransfer>,
}

/// Extract `result`, mapping an RPC `error` to a retryable [`Error::Transient`] and a missing result
/// to [`Error::Integrity`].
fn rpc_result<'a>(v: &'a serde_json::Value, ctx: &str) -> Result<&'a serde_json::Value, Error> {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        return Err(Error::Transient {
            message: format!("{ctx} RPC error: {e}"),
            retry_after: None,
        });
    }
    v.get("result").ok_or_else(|| Error::Integrity {
        message: format!("{ctx} response missing result"),
    })
}

fn parse_json(raw: &str, ctx: &str) -> Result<serde_json::Value, Error> {
    serde_json::from_str(raw).map_err(|e| Error::Integrity {
        message: format!("{ctx} response not JSON: {e}"),
    })
}

/// The 20-byte address packed in a 32-byte indexed log topic (lowercased `0x…`).
fn topic_address(topic: &str) -> String {
    topic
        .trim_start_matches("0x")
        .get(24..)
        .map(|s| format!("0x{s}"))
        .unwrap_or_default()
        .to_lowercase()
}

/// Inbound ERC-20 transfers of `token` to `wallet` at/after `from_block`, oldest first.
pub async fn asset_transfers_in(
    rpc: &dyn ChainProvider,
    chain: Chain,
    wallet: &str,
    token: &str,
    from_block: Option<u64>,
) -> Result<Vec<AssetTransfer>, Error> {
    let from_block_hex = from_block
        .map(|b| format!("0x{b:x}"))
        .unwrap_or_else(|| "0x0".to_string());
    let params = serde_json::json!([{
        "fromBlock": from_block_hex,
        "toBlock": "latest",
        "toAddress": wallet,
        "category": ["erc20"],
        "contractAddresses": [token],
        "withMetadata": true,
        "excludeZeroValue": true,
        "order": "asc"
    }])
    .to_string();
    let raw = rpc.call(chain, "alchemy_getAssetTransfers", params).await?;
    let json = parse_json(&raw, "getAssetTransfers")?;
    let res = rpc_result(&json, "getAssetTransfers")?;
    let Some(arr) = res.get("transfers").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let Some(tx) = t.get("hash").and_then(|v| v.as_str()) else {
            continue;
        };
        let block = t
            .get("blockNum")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        let block_timestamp = t
            .pointer("/metadata/blockTimestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let token_decimals = t
            .pointer("/rawContract/decimal")
            .and_then(|v| v.as_str())
            .and_then(|s| u8::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            .unwrap_or(18);
        let value_raw = t
            .pointer("/rawContract/value")
            .and_then(|v| v.as_str())
            .unwrap_or("0x0")
            .to_string();
        out.push(AssetTransfer {
            tx: tx.to_string(),
            block,
            block_timestamp,
            value_raw,
            token_decimals,
        });
    }
    Ok(out)
}

/// ERC-20 `decimals()` calldata.
const DECIMALS_CALL: &str = "0x313ce567";

/// Whether a JSON-RPC `error` from an `eth_call` is the contract's own answer (a revert, code 3 or
/// named in the message or `data`, or an invalid opcode) rather than the node failing to answer.
fn is_contract_failure(error: &serde_json::Value) -> bool {
    if error.get("code").and_then(|c| c.as_i64()) == Some(3) {
        return true;
    }
    let text = |k: &str| {
        error
            .get(k)
            .map(|v| match v.as_str() {
                Some(s) => s.to_lowercase(),
                None => v.to_string().to_lowercase(),
            })
            .unwrap_or_default()
    };
    [text("message"), text("data")]
        .iter()
        .any(|t| t.contains("revert") || t.contains("invalid opcode"))
}

/// `token`'s `decimals()` at the latest block: `None` on a definitive non-answer, an error when the
/// node did not answer, so the caller retries rather than reading the token as one without decimals.
async fn token_decimals(
    rpc: &dyn ChainProvider,
    chain: Chain,
    token: &str,
) -> Result<Option<u8>, Error> {
    if Address::from_str(token).is_err() {
        return Ok(None);
    }
    let params = serde_json::json!([{ "to": token, "data": DECIMALS_CALL }, "latest"]).to_string();
    let raw = rpc.call(chain, "eth_call", params).await?;
    let json = parse_json(&raw, "decimals")?;
    if let Some(e) = json.get("error").filter(|e| !e.is_null()) {
        if is_contract_failure(e) {
            return Ok(None);
        }
        return Err(Error::Transient {
            message: format!("decimals of {token} RPC error: {e}"),
            retry_after: None,
        });
    }
    let out = rpc_result(&json, "decimals")?
        .as_str()
        .and_then(|r| hex::decode(r.trim_start_matches("0x")).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("decimals of {token} result not hex"),
        })?;
    if out.len() != 32 {
        return Ok(None);
    }
    let decimals = U256::from_be_slice(&out);
    Ok((decimals <= U256::from(u8::MAX)).then(|| decimals.to::<u8>()))
}

/// The `result` of a by-hash read, where `null` means the node has not seen the transaction yet
/// (a load-balanced node lagging the indexer that surfaced it): retryable, never "nothing there".
fn mined<'a>(
    json: &'a serde_json::Value,
    ctx: &str,
    tx: &str,
) -> Result<&'a serde_json::Value, Error> {
    let res = rpc_result(json, ctx)?;
    if res.is_null() {
        return Err(Error::Transient {
            message: format!("{ctx}: no result for tx {tx}"),
            retry_after: None,
        });
    }
    Ok(res)
}

/// An optional address field of a receipt: absent or `null` is `None`, anything unparseable an error.
fn address_field(v: &serde_json::Value, key: &str, tx: &str) -> Result<Option<Address>, Error> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(a) => a
            .as_str()
            .and_then(|s| Address::from_str(s).ok())
            .map(Some)
            .ok_or_else(|| Error::Integrity {
                message: format!("receipt of {tx} has an unparseable `{key}`"),
            }),
    }
}

/// Transaction `tx`'s sender, native value and ERC-20 `Transfer` logs, each with its token's
/// decimals, read once per distinct token. A receipt or transaction the node does not return is a
/// retryable error, never an empty read.
pub async fn read_receipt(
    rpc: &dyn ChainProvider,
    chain: Chain,
    tx: &str,
) -> Result<ReceiptRead, Error> {
    let params = serde_json::json!([tx]).to_string();
    let raw = rpc
        .call(chain, "eth_getTransactionReceipt", params.clone())
        .await?;
    let json = parse_json(&raw, "getTransactionReceipt")?;
    let receipt = mined(&json, "getTransactionReceipt", tx)?;
    let from = address_field(receipt, "from", tx)?.ok_or_else(|| Error::Integrity {
        message: format!("receipt of {tx} has no `from`"),
    })?;
    let to = address_field(receipt, "to", tx)?;
    let logs = receipt
        .get("logs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Integrity {
            message: format!("receipt of {tx} has no `logs`"),
        })?;

    let raw = rpc.call(chain, "eth_getTransactionByHash", params).await?;
    let json = parse_json(&raw, "getTransactionByHash")?;
    let value_wei = mined(&json, "getTransactionByHash", tx)?
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .ok_or_else(|| Error::Integrity {
            message: format!("transaction {tx} has no parseable `value`"),
        })?;

    let mut transfers = Vec::new();
    let mut decimals: HashMap<String, Option<u8>> = HashMap::new();
    for log in logs {
        let topics: Vec<&str> = log
            .get("topics")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if topics.len() < 3 || topics[0] != TRANSFER_TOPIC {
            continue;
        }
        let token = log
            .get("address")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let token_decimals = match decimals.get(&token) {
            Some(d) => *d,
            None => {
                let d = token_decimals(rpc, chain, &token).await?;
                decimals.insert(token.clone(), d);
                d
            }
        };
        transfers.push(ReceiptTransfer {
            token,
            from: topic_address(topics[1]),
            to: topic_address(topics[2]),
            value_raw: log
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("0x0")
                .to_string(),
            token_decimals,
        });
    }
    Ok(ReceiptRead {
        from,
        to,
        value_wei,
        transfers,
    })
}

/// One log of a mined transaction receipt, as returned by the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptLog {
    /// Emitting contract (lowercased `0x…`).
    pub address: String,
    /// Indexed topics (lowercased `0x…` 32-byte words), topic0 first.
    pub topics: Vec<String>,
    /// Non-indexed data (`0x…`).
    pub data: String,
}

/// Every log of transaction `tx`. As in [`read_receipt`], a receipt the node does not return is an
/// error, never an empty list: a caller rebuilding amounts from these logs must not read a
/// missing receipt as a tx that emitted nothing. It is retryable, since a load-balanced node lagging
/// behind the indexer that surfaced the tx is the usual cause.
pub async fn receipt_logs(
    rpc: &dyn ChainProvider,
    chain: Chain,
    tx: &str,
) -> Result<Vec<ReceiptLog>, Error> {
    let params = serde_json::json!([tx]).to_string();
    let raw = rpc.call(chain, "eth_getTransactionReceipt", params).await?;
    let json = parse_json(&raw, "getTransactionReceipt")?;
    let res = rpc_result(&json, "getTransactionReceipt")?;
    let logs = res
        .get("logs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Transient {
            message: format!("no receipt for tx {tx}"),
            retry_after: None,
        })?;
    logs.iter()
        .map(|log| {
            let field = |k: &str| {
                log.get(k)
                    .and_then(|v| v.as_str())
                    .map(str::to_lowercase)
                    .ok_or_else(|| Error::Integrity {
                        message: format!("receipt log of {tx} missing `{k}`"),
                    })
            };
            let topics = log
                .get("topics")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Error::Integrity {
                    message: format!("receipt log of {tx} missing `topics`"),
                })?
                .iter()
                .map(|t| {
                    t.as_str()
                        .map(str::to_lowercase)
                        .ok_or_else(|| Error::Integrity {
                            message: format!("receipt log of {tx} has a non-string topic"),
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ReceiptLog {
                address: field("address")?,
                topics,
                data: field("data")?,
            })
        })
        .collect()
}

/// The latest block number on `chain`.
pub async fn latest_block(rpc: &dyn ChainProvider, chain: Chain) -> Result<u64, Error> {
    let raw = rpc.call(chain, "eth_blockNumber", "[]".to_string()).await?;
    let json = parse_json(&raw, "eth_blockNumber")?;
    let res = rpc_result(&json, "eth_blockNumber")?;
    let s = res.as_str().ok_or_else(|| Error::Integrity {
        message: "eth_blockNumber result not a string".into(),
    })?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| Error::Integrity {
        message: format!("eth_blockNumber not hex: {e}"),
    })
}
