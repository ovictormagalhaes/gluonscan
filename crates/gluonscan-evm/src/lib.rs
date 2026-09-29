//! # gluonscan-evm
//!
//! Minimal EVM on-chain helpers built on the injected [`ChainProvider`] port: a `latest`-block
//! `eth_call`, plus the tiny bit of ABI encode/decode the adapters need. No RPC client is owned
//! here — the transport is injected, so tests replay recorded RPC contracts.

use alloy_primitives::{hex, Address, U256};
use gluonscan_core::{Chain, ChainProvider, Error};

/// Perform an `eth_call` to `to` with calldata `data` at the latest block, returning the raw
/// returned bytes.
pub async fn eth_call(
    rpc: &dyn ChainProvider,
    chain: Chain,
    from: Option<Address>,
    to: Address,
    data: Vec<u8>,
) -> Result<Vec<u8>, Error> {
    // `from` matters for calls guarded by msg.sender (e.g. Uniswap's collect() authorization).
    let from_field = match from {
        Some(f) => format!(r#""from":"{f:#x}","#),
        None => String::new(),
    };
    let params = format!(
        r#"[{{{from_field}"to":"{to:#x}","data":"0x{}"}},"latest"]"#,
        hex::encode(&data)
    );
    let raw = rpc.call(chain, "eth_call", params).await?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("eth_call response not JSON: {e}"),
    })?;
    if let Some(err) = v.get("error") {
        return Err(Error::Provider(format!("eth_call error: {err}").into()));
    }
    let result = v
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| Error::Integrity {
            message: "eth_call response missing `result`".into(),
        })?;
    hex::decode(result.trim_start_matches("0x")).map_err(|e| Error::Integrity {
        message: format!("eth_call result not hex: {e}"),
    })
}

/// Read an account's native coin balance via `eth_getBalance` at the latest block, returning wei.
pub async fn eth_get_balance(
    rpc: &dyn ChainProvider,
    chain: Chain,
    address: Address,
) -> Result<U256, Error> {
    let params = format!(r#"["{address:#x}","latest"]"#);
    let raw = rpc.call(chain, "eth_getBalance", params).await?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("eth_getBalance response not JSON: {e}"),
    })?;
    if let Some(err) = v.get("error") {
        return Err(Error::Provider(
            format!("eth_getBalance error: {err}").into(),
        ));
    }
    let result = v
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| Error::Integrity {
            message: "eth_getBalance response missing `result`".into(),
        })?;
    U256::from_str_radix(result.trim_start_matches("0x"), 16).map_err(|e| Error::Integrity {
        message: format!("eth_getBalance result not a hex quantity: {e}"),
    })
}

/// Encode a call to the NonfungiblePositionManager `collect((tokenId, recipient, u128::MAX,
/// u128::MAX))` — a static simulation that yields the position's uncollected fees.
pub fn encode_collect(token_id: U256, recipient: Address) -> Vec<u8> {
    let mut data = vec![0xfc, 0x6f, 0x78, 0x65]; // selector 0xfc6f7865
    data.extend_from_slice(&token_id.to_be_bytes::<32>());
    let mut recipient_word = [0u8; 32];
    recipient_word[12..].copy_from_slice(recipient.as_slice());
    data.extend_from_slice(&recipient_word);
    let max = U256::from(u128::MAX);
    data.extend_from_slice(&max.to_be_bytes::<32>());
    data.extend_from_slice(&max.to_be_bytes::<32>());
    data
}

/// Encode an ERC-20 `balanceOf(owner)` call.
pub fn encode_balance_of(owner: Address) -> Vec<u8> {
    let mut data = vec![0x70, 0xa0, 0x82, 0x31]; // selector 0x70a08231
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(owner.as_slice());
    data.extend_from_slice(&word);
    data
}

/// Decode a single ABI `uint256` word from a return buffer.
pub fn decode_u256(out: &[u8]) -> Result<U256, Error> {
    if out.len() < 32 {
        return Err(Error::Integrity {
            message: format!("expected >=32 bytes, got {}", out.len()),
        });
    }
    Ok(U256::from_be_slice(&out[0..32]))
}

/// Decode two ABI `uint256` words from a return buffer.
pub fn decode_two_u256(out: &[u8]) -> Result<(U256, U256), Error> {
    if out.len() < 64 {
        return Err(Error::Integrity {
            message: format!("expected >=64 bytes, got {}", out.len()),
        });
    }
    Ok((
        U256::from_be_slice(&out[0..32]),
        U256::from_be_slice(&out[32..64]),
    ))
}
