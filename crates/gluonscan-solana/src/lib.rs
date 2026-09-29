//! # gluonscan-solana
//!
//! Minimal Solana on-chain helpers over the injected [`ChainProvider`] port: base58 pubkeys,
//! program-derived addresses ([`find_program_address`]), and the two RPC reads the adapters need
//! ([`get_token_accounts_by_owner`], [`get_account_info`]). No RPC client is owned here — the
//! transport is injected, so tests replay recorded RPC contracts.

use base64::Engine;
use curve25519_dalek::edwards::CompressedEdwardsY;
use gluonscan_core::{Chain, ChainProvider, Error};
use sha2::{Digest, Sha256};

/// The SPL Token program id.
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// Raydium's concentrated-liquidity (CLMM) program id.
pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

/// Decode a base58 pubkey into its 32 bytes.
pub fn pubkey_bytes(s: &str) -> Result<[u8; 32], Error> {
    let v = bs58::decode(s).into_vec().map_err(|e| Error::Permanent {
        message: format!("invalid base58 pubkey: {e}"),
    })?;
    v.try_into().map_err(|_| Error::Permanent {
        message: "pubkey is not 32 bytes".into(),
    })
}

/// Encode 32 bytes as a base58 pubkey.
pub fn pubkey_str(bytes: &[u8; 32]) -> String {
    bs58::encode(bytes).into_string()
}

fn is_on_curve(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY::from_slice(bytes)
        .ok()
        .and_then(|c| c.decompress())
        .is_some()
}

/// Find the program-derived address for `seeds` under `program_id`, returning it with the bump.
///
/// Ports Solana's `find_program_address`: `sha256(seeds || bump || program_id ||
/// "ProgramDerivedAddress")`, decrementing the bump until the result is off the ed25519 curve.
pub fn find_program_address(seeds: &[&[u8]], program_id: &[u8; 32]) -> ([u8; 32], u8) {
    for bump in (0..=u8::MAX).rev() {
        let mut hasher = Sha256::new();
        for seed in seeds {
            hasher.update(seed);
        }
        hasher.update([bump]);
        hasher.update(program_id);
        hasher.update(b"ProgramDerivedAddress");
        let hash: [u8; 32] = hasher.finalize().into();
        if !is_on_curve(&hash) {
            return (hash, bump);
        }
    }
    // Astronomically unlikely (every bump on-curve); a panic here is a genuine invariant break.
    panic!("no off-curve program address found");
}

/// List the wallet's token accounts (SPL Token program) and return the mints it holds as an NFT
/// (`amount == 1`, `decimals == 0`) — the discovery step for position NFTs.
pub async fn get_token_accounts_by_owner(
    rpc: &dyn ChainProvider,
    owner: &str,
) -> Result<Vec<String>, Error> {
    let params =
        format!(r#"["{owner}",{{"programId":"{TOKEN_PROGRAM}"}},{{"encoding":"jsonParsed"}}]"#);
    let raw = rpc
        .call(Chain::Solana, "getTokenAccountsByOwner", params)
        .await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("getTokenAccountsByOwner response not JSON: {e}"),
    })?;
    let accounts = json
        .pointer("/result/value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Integrity {
            message: "getTokenAccountsByOwner missing result.value".into(),
        })?;

    let mut mints = Vec::new();
    for acc in accounts {
        let info = acc.pointer("/account/data/parsed/info");
        let amount = info
            .and_then(|i| i.pointer("/tokenAmount/amount"))
            .and_then(|a| a.as_str());
        let decimals = info
            .and_then(|i| i.pointer("/tokenAmount/decimals"))
            .and_then(|d| d.as_u64());
        if amount == Some("1") && decimals == Some(0) {
            if let Some(mint) = info.and_then(|i| i.get("mint")).and_then(|m| m.as_str()) {
                mints.push(mint.to_string());
            }
        }
    }
    Ok(mints)
}

/// Fetch and base64-decode an account's data. Returns `None` when the account does not exist
/// (`result.value == null`), so callers can treat a missing account as "not present" rather than
/// an error.
pub async fn get_account_info(
    rpc: &dyn ChainProvider,
    pubkey: &str,
) -> Result<Option<Vec<u8>>, Error> {
    let params = format!(r#"["{pubkey}",{{"encoding":"base64"}}]"#);
    let raw = rpc.call(Chain::Solana, "getAccountInfo", params).await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("getAccountInfo response not JSON: {e}"),
    })?;
    match json.pointer("/result/value") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => {
            let data = value
                .pointer("/data/0")
                .and_then(|d| d.as_str())
                .ok_or_else(|| Error::Integrity {
                    message: format!("account {pubkey} has no base64 data"),
                })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| Error::Integrity {
                    message: format!("account {pubkey} data not base64: {e}"),
                })?;
            Ok(Some(bytes))
        }
    }
}

/// A raw SPL token balance held by a wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalance {
    /// The token mint (base58).
    pub mint: String,
    /// The raw (base-unit) amount, as a decimal string.
    pub amount_raw: String,
    /// The mint's decimals.
    pub decimals: u8,
}

/// List the wallet's non-zero SPL token balances (fungible + non-fungible alike).
pub async fn get_token_balances(
    rpc: &dyn ChainProvider,
    owner: &str,
) -> Result<Vec<TokenBalance>, Error> {
    let params =
        format!(r#"["{owner}",{{"programId":"{TOKEN_PROGRAM}"}},{{"encoding":"jsonParsed"}}]"#);
    let raw = rpc
        .call(Chain::Solana, "getTokenAccountsByOwner", params)
        .await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("getTokenAccountsByOwner response not JSON: {e}"),
    })?;
    let accounts = json
        .pointer("/result/value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Integrity {
            message: "getTokenAccountsByOwner missing result.value".into(),
        })?;

    let mut out = Vec::new();
    for acc in accounts {
        let info = acc.pointer("/account/data/parsed/info");
        let amount = info
            .and_then(|i| i.pointer("/tokenAmount/amount"))
            .and_then(|a| a.as_str());
        let decimals = info
            .and_then(|i| i.pointer("/tokenAmount/decimals"))
            .and_then(|d| d.as_u64());
        let mint = info.and_then(|i| i.get("mint")).and_then(|m| m.as_str());
        if let (Some(amount), Some(decimals), Some(mint)) = (amount, decimals, mint) {
            if amount != "0" {
                out.push(TokenBalance {
                    mint: mint.to_string(),
                    amount_raw: amount.to_string(),
                    decimals: decimals as u8,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_address_is_deterministic_and_off_curve() {
        let program = pubkey_bytes(RAYDIUM_CLMM_PROGRAM).unwrap();
        let mint = [7u8; 32];
        let (a, bump_a) = find_program_address(&[b"position", &mint], &program);
        let (b, bump_b) = find_program_address(&[b"position", &mint], &program);
        assert_eq!(a, b);
        assert_eq!(bump_a, bump_b);
        assert!(!is_on_curve(&a));
    }

    #[test]
    fn pubkey_roundtrips() {
        let bytes = pubkey_bytes(RAYDIUM_CLMM_PROGRAM).unwrap();
        assert_eq!(pubkey_str(&bytes), RAYDIUM_CLMM_PROGRAM);
    }
}
