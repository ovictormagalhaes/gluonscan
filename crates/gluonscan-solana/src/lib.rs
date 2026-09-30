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
/// The Token-2022 program id. Newer position NFTs (e.g. Raydium CLMM) mint under this program, so
/// discovery must query it alongside the classic SPL Token program or it silently misses them.
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
/// Raydium's concentrated-liquidity (CLMM) program id.
pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
/// The Metaplex Token Metadata program id.
pub const METAPLEX_METADATA_PROGRAM: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

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

/// List the wallet's NFT mints (`amount == 1`, `decimals == 0`) across both the classic SPL Token
/// program and Token-2022 — the discovery step for position NFTs. Querying only one program silently
/// misses positions minted under the other.
pub async fn get_token_accounts_by_owner(
    rpc: &dyn ChainProvider,
    owner: &str,
) -> Result<Vec<String>, Error> {
    let mut mints = Vec::new();
    for program in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
        for mint in nft_mints_for_program(rpc, owner, program).await? {
            // A mint is globally unique to one token program, so any repeat is a duplicate read.
            if !mints.contains(&mint) {
                mints.push(mint);
            }
        }
    }
    Ok(mints)
}

async fn nft_mints_for_program(
    rpc: &dyn ChainProvider,
    owner: &str,
    program: &str,
) -> Result<Vec<String>, Error> {
    let params = format!(r#"["{owner}",{{"programId":"{program}"}},{{"encoding":"jsonParsed"}}]"#);
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

/// Read an SPL mint's `decimals` (byte 44 of the mint account layout).
pub async fn get_mint_decimals(rpc: &dyn ChainProvider, mint: &str) -> Result<u8, Error> {
    let data = get_account_info(rpc, mint)
        .await?
        .ok_or_else(|| Error::Integrity {
            message: format!("mint account {mint} not found"),
        })?;
    data.get(44).copied().ok_or_else(|| Error::Integrity {
        message: format!("mint account {mint} too short for decimals"),
    })
}

/// Read an account's native SOL balance in lamports (`getBalance`).
pub async fn get_native_balance(rpc: &dyn ChainProvider, owner: &str) -> Result<u64, Error> {
    let params = format!(r#"["{owner}"]"#);
    let raw = rpc.call(Chain::Solana, "getBalance", params).await?;
    let json: serde_json::Value = serde_json::from_str(&raw).map_err(|e| Error::Integrity {
        message: format!("getBalance response not JSON: {e}"),
    })?;
    json.pointer("/result/value")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| Error::Integrity {
            message: "getBalance response missing result.value".into(),
        })
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

/// The Metaplex metadata PDA (base58) for a mint: `["metadata", program, mint]` under the program.
pub fn metadata_pda(mint: &str) -> Result<String, Error> {
    let program = pubkey_bytes(METAPLEX_METADATA_PROGRAM)?;
    let mint_bytes = pubkey_bytes(mint)?;
    let (pda, _bump) = find_program_address(&[b"metadata", &program, &mint_bytes], &program);
    Ok(pubkey_str(&pda))
}

/// Fields decoded from a Metaplex Token Metadata account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NftMetadata {
    /// The on-chain name, trailing padding trimmed, when present.
    pub name: Option<String>,
    /// The verified collection mint (base58), only when the NFT declares one and it is verified.
    pub collection: Option<String>,
}

fn read_u32(data: &[u8], off: &mut usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let bytes: [u8; 4] = data.get(*off..end)?.try_into().ok()?;
    *off = end;
    Some(u32::from_le_bytes(bytes))
}

fn read_borsh_string(data: &[u8], off: &mut usize) -> Option<String> {
    let len = read_u32(data, off)? as usize;
    let end = off.checked_add(len)?;
    let bytes = data.get(*off..end)?;
    *off = end;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Decode the name and verified collection from a Metaplex metadata account.
///
/// Bounds-checked and fail-safe: the fixed-offset name is read first, and any later field it
/// cannot walk cleanly (a newer/variant layout) leaves that field `None` rather than guessing —
/// a wrong collection key would be worse than an absent one.
pub fn decode_metadata(data: &[u8]) -> NftMetadata {
    let mut md = NftMetadata::default();
    // key(1) + update_authority(32) + mint(32) = 65, then data.name/symbol/uri as borsh strings.
    let mut off = 65usize;
    let name = match read_borsh_string(data, &mut off) {
        Some(s) => s,
        None => return md,
    };
    let name = name.trim_end_matches('\0').trim();
    if !name.is_empty() {
        md.name = Some(name.to_string());
    }
    if read_borsh_string(data, &mut off).is_none() {
        return md;
    }
    if read_borsh_string(data, &mut off).is_none() {
        return md;
    }
    // seller_fee_basis_points: u16
    off = match off.checked_add(2) {
        Some(o) => o,
        None => return md,
    };
    // creators: Option<Vec<Creator>>, each Creator = pubkey(32) + verified(1) + share(1) = 34.
    match data.get(off) {
        Some(0) => off += 1,
        Some(1) => {
            off += 1;
            let count = match read_u32(data, &mut off) {
                Some(c) => c as usize,
                None => return md,
            };
            off = match count.checked_mul(34).and_then(|n| off.checked_add(n)) {
                Some(o) if o <= data.len() => o,
                _ => return md,
            };
        }
        _ => return md,
    }
    // primary_sale_happened(1) + is_mutable(1)
    off = match off.checked_add(2) {
        Some(o) => o,
        None => return md,
    };
    // edition_nonce: Option<u8>, then token_standard: Option<u8>
    for _ in 0..2 {
        match data.get(off) {
            Some(0) => off += 1,
            Some(1) => off += 2,
            _ => return md,
        }
    }
    // collection: Option<Collection { verified: bool, key: pubkey }>
    if data.get(off) == Some(&1) && data.get(off + 1) == Some(&1) {
        if let Some(key) = data.get(off + 2..off + 34) {
            if let Ok(arr) = <[u8; 32]>::try_from(key) {
                md.collection = Some(pubkey_str(&arr));
            }
        }
    }
    md
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

    fn borsh_string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn decodes_name_and_verified_collection() {
        let collection = [9u8; 32];
        let mut data = Vec::new();
        data.push(4); // key
        data.extend_from_slice(&[1u8; 32]); // update_authority
        data.extend_from_slice(&[2u8; 32]); // mint
        borsh_string(&mut data, "Mad Lad #1234\0\0"); // name (padded)
        borsh_string(&mut data, "MAD"); // symbol
        borsh_string(&mut data, "https://example.com/1234.json"); // uri
        data.extend_from_slice(&500u16.to_le_bytes()); // seller_fee_basis_points
        data.push(0); // creators: None
        data.push(1); // primary_sale_happened
        data.push(1); // is_mutable
        data.push(1); // edition_nonce: Some
        data.push(255);
        data.push(1); // token_standard: Some
        data.push(0);
        data.push(1); // collection: Some
        data.push(1); // verified
        data.extend_from_slice(&collection); // key

        let md = decode_metadata(&data);
        assert_eq!(md.name.as_deref(), Some("Mad Lad #1234"));
        assert_eq!(md.collection, Some(pubkey_str(&collection)));
    }

    #[test]
    fn unverified_collection_is_dropped() {
        let mut data = Vec::new();
        data.push(4);
        data.extend_from_slice(&[1u8; 32]);
        data.extend_from_slice(&[2u8; 32]);
        borsh_string(&mut data, "Solo NFT");
        borsh_string(&mut data, "SOLO");
        borsh_string(&mut data, "u");
        data.extend_from_slice(&0u16.to_le_bytes());
        data.push(0); // creators None
        data.push(0); // primary_sale
        data.push(1); // is_mutable
        data.push(0); // edition_nonce None
        data.push(0); // token_standard None
        data.push(1); // collection Some
        data.push(0); // NOT verified
        data.extend_from_slice(&[9u8; 32]);

        let md = decode_metadata(&data);
        assert_eq!(md.name.as_deref(), Some("Solo NFT"));
        assert_eq!(md.collection, None); // unverified collection is never trusted
    }

    #[test]
    fn truncated_data_yields_name_only_no_panic() {
        let mut data = Vec::new();
        data.push(4);
        data.extend_from_slice(&[1u8; 32]);
        data.extend_from_slice(&[2u8; 32]);
        borsh_string(&mut data, "Half Decoded");
        // Truncated right after the name.
        let md = decode_metadata(&data);
        assert_eq!(md.name.as_deref(), Some("Half Decoded"));
        assert_eq!(md.collection, None);
    }
}
