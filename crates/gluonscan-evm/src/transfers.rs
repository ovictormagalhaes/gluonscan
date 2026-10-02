//! Cost-basis chain reads over the injected [`ChainProvider`]: a wallet's inbound ERC-20 transfers
//! (`alchemy_getAssetTransfers`), the ERC-20 `Transfer` logs of a transaction receipt, and the latest
//! block number. The RPC URL carries the provider key — no API key is handled here. The consumer owns
//! the cost-basis math (pairing the inbound leg with what was paid, pricing, averaging).

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

/// The ERC-20 `Transfer` logs of transaction `tx` (empty if the tx is unknown or has none).
pub async fn receipt_transfers(
    rpc: &dyn ChainProvider,
    chain: Chain,
    tx: &str,
) -> Result<Vec<ReceiptTransfer>, Error> {
    let params = serde_json::json!([tx]).to_string();
    let raw = rpc.call(chain, "eth_getTransactionReceipt", params).await?;
    let json = parse_json(&raw, "getTransactionReceipt")?;
    let res = rpc_result(&json, "getTransactionReceipt")?;
    let Some(logs) = res.get("logs").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for log in logs {
        let topics: Vec<&str> = log
            .get("topics")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if topics.len() < 3 || topics[0] != TRANSFER_TOPIC {
            continue;
        }
        out.push(ReceiptTransfer {
            token: log
                .get("address")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase(),
            from: topic_address(topics[1]),
            to: topic_address(topics[2]),
            value_raw: log
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("0x0")
                .to_string(),
        });
    }
    Ok(out)
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
