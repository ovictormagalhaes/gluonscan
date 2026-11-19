//! Wallet signature verification: did the holder of `address` sign `message`?
//!
//! The ownership proof behind a wallet login, without an Ethereum or Solana SDK. Each family sits
//! behind its own feature, so a consumer compiles only the curve it needs:
//!
//! - `signature-evm`: EIP-191 `personal_sign`. The signer is recovered with secp256k1 (`k256`)
//!   from the 65-byte `r || s || v` signature and compared with `address` byte for byte, so case
//!   (and an EIP-55 checksum) does not matter.
//! - `signature-solana`: ed25519 over the message bytes, with a base58 public key and signature,
//!   checked with `verify_strict` (canonical `s`, no small-order key or `R`).
//!
//! The accepted forms are those of the widely used reference implementations (ethers-core's
//! `Address`/`Signature` parsing and `Signature::recover`; `solana-pubkey`/`solana-signature`
//! parsing and `Signature::verify`), and the tests pin them with vectors those implementations
//! produced:
//!
//! - An EVM address is 40 hex digits in any case, with an optional leading `0x`; ASCII spaces,
//!   tabs, CR and LF between the digits are ignored.
//! - An EVM signature is 65 bytes of hex with up to two leading `0x` (lowercase only). The recovery
//!   byte `v` may be `27`/`28`, `0`/`1` or an EIP-155 value, and a high-`s` signature is accepted
//!   in its low-`s` form, as both are the same proof.
//! - A Solana address is 32 bytes and a signature 64 bytes, both base58.
//!
//! `Ok(true)` means the signature proves `address` signed `message`. `Ok(false)` means the inputs
//! are well formed but the proof does not hold: another signer, another message, or a signature
//! that recovers or verifies to nothing (for Solana, also an address that is not a curve point). An
//! input that cannot be parsed is an `Err`. No input panics. Where the reference would accept a
//! form no signer produces (an ed25519 `R` in a non-canonical encoding), this rejects it.

use crate::Ecosystem;

/// Why a signature could not be checked at all.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// The address is not in the family's format.
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    /// The signature is not in the family's format.
    #[error("invalid signature: {0}")]
    InvalidSignature(String),
    /// The family is not supported, or its feature is not enabled.
    #[error("unsupported chain family: {0:?}")]
    UnsupportedFamily(Ecosystem),
}

/// Whether `signature` proves that the holder of `address` signed `message`, under `family`'s
/// scheme (see the [module docs](self)).
pub fn verify_wallet_signature(
    family: Ecosystem,
    address: &str,
    message: &str,
    signature: &str,
) -> Result<bool, SignatureError> {
    match family {
        #[cfg(feature = "signature-evm")]
        Ecosystem::Evm => verify_evm(address, message, signature),
        #[cfg(feature = "signature-solana")]
        Ecosystem::Solana => verify_solana(address, message, signature),
        other => {
            let _ = (address, message, signature);
            Err(SignatureError::UnsupportedFamily(other))
        }
    }
}

/// Whether the EIP-191 `personal_sign` signature `signature` over `message` recovers `address`.
#[cfg(feature = "signature-evm")]
pub fn verify_evm(address: &str, message: &str, signature: &str) -> Result<bool, SignatureError> {
    let claimed = evm_address(address)?;
    // The reference strips one `0x` and its hex decoder strips another, so `0x0x…` is accepted.
    let digits = signature.strip_prefix("0x").unwrap_or(signature);
    let digits = digits.strip_prefix("0x").unwrap_or(digits);
    let bytes = hex::decode(digits).map_err(|e| SignatureError::InvalidSignature(e.to_string()))?;
    let bytes: [u8; 65] = bytes.try_into().map_err(|b: Vec<u8>| {
        SignatureError::InvalidSignature(format!("expected 65 bytes, got {}", b.len()))
    })?;
    Ok(recover_evm_signer(message, &bytes) == Some(claimed))
}

/// 40 hex digits, optionally after one `0x`, ignoring ASCII whitespace between them.
#[cfg(feature = "signature-evm")]
fn evm_address(address: &str) -> Result<[u8; 20], SignatureError> {
    let digits = address.strip_prefix("0x").unwrap_or(address);
    let mut nibbles = Vec::with_capacity(40);
    for c in digits.chars() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {}
            _ => match c.to_digit(16) {
                Some(n) if nibbles.len() < 40 => nibbles.push(n as u8),
                _ => {
                    return Err(SignatureError::InvalidAddress(format!(
                        "{address:?} is not 40 hex digits"
                    )))
                }
            },
        }
    }
    if nibbles.len() != 40 {
        return Err(SignatureError::InvalidAddress(format!(
            "{address:?} is not 40 hex digits"
        )));
    }
    let mut out = [0u8; 20];
    for (byte, [high, low]) in out.iter_mut().zip(nibbles.as_chunks::<2>().0) {
        *byte = high << 4 | low;
    }
    Ok(out)
}

/// The address that signed `message` (EIP-191) with `r || s || v`, if the signature recovers one.
#[cfg(feature = "signature-evm")]
fn recover_evm_signer(message: &str, sig: &[u8; 65]) -> Option<[u8; 20]> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let v = sig[64];
    // Legacy 27/28 (and the bare 0..=3 it encodes), or an EIP-155 `v` whose parity is the id.
    let id = match v {
        0..=26 => v % 4,
        27..=34 => (v - 27) % 4,
        _ => (v - 1) % 2,
    };
    let mut recovery = RecoveryId::from_byte(id)?;
    let r: [u8; 32] = sig[..32].try_into().ok()?;
    let s: [u8; 32] = sig[32..64].try_into().ok()?;
    let mut signature = Signature::from_scalars(r, s).ok()?;
    // A high-s signature is the same proof as its low-s twin with the other recovery parity.
    let low = signature.normalize_s();
    if low != signature {
        signature = low;
        recovery = RecoveryId::from_byte(recovery.to_byte() ^ 1)?;
    }
    let key =
        VerifyingKey::recover_from_prehash(&eip191_hash(message), &signature, recovery).ok()?;
    let point = key.to_sec1_point(false);
    let hash = alloy_primitives::keccak256(&point.as_bytes()[1..]);
    hash[12..].try_into().ok()
}

/// `keccak256("\x19Ethereum Signed Message:\n" || len(message) || message)`.
#[cfg(feature = "signature-evm")]
fn eip191_hash(message: &str) -> [u8; 32] {
    let mut bytes = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
    bytes.extend_from_slice(message.as_bytes());
    alloy_primitives::keccak256(&bytes).0
}

/// Whether the base58 ed25519 `signature` over `message` verifies, strictly, under the base58
/// public key `address`.
#[cfg(feature = "signature-solana")]
pub fn verify_solana(
    address: &str,
    message: &str,
    signature: &str,
) -> Result<bool, SignatureError> {
    let key: [u8; 32] = base58_exact(address, 44).ok_or_else(|| {
        SignatureError::InvalidAddress(format!("{address:?} is not a base58 32-byte key"))
    })?;
    let sig: [u8; 64] = base58_exact(signature, 88).ok_or_else(|| {
        SignatureError::InvalidSignature(format!("{signature:?} is not a base58 64-byte signature"))
    })?;
    // 32 bytes that are not a curve point (a program address, say) can never have signed.
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(&key) else {
        return Ok(false);
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig);
    Ok(key.verify_strict(message.as_bytes(), &sig).is_ok())
}

/// `N` bytes from a base58 string of at most `max_len` characters (the longest `N` bytes encode
/// to), or `None`.
#[cfg(feature = "signature-solana")]
fn base58_exact<const N: usize>(s: &str, max_len: usize) -> Option<[u8; N]> {
    if s.len() > max_len {
        return None;
    }
    bs58::decode(s).into_vec().ok()?.try_into().ok()
}
