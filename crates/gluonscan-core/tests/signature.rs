//! Wallet signature verification (`gluonscan_core::signature`).
//!
//! `fixtures/signature_vectors.json` holds inputs with the outcome the reference implementations
//! gave for each: ethers-core 2.0.14 with const-hex 1.18.1 (`Address::from_str`,
//! `Signature::from_str`, `Signature::recover` over the message; an outer `0x` stripped first) for
//! EVM, and solana-pubkey 2.4.0 / solana-signature 2.3.0
//! (`Pubkey::from_str`, `Signature::from_str`, `Signature::verify`) for Solana. Their failures map
//! as: unparseable address → `InvalidAddress`, unparseable signature → `InvalidSignature`, a
//! signature that does not recover or verify to the address → `Ok(false)`. The other tests sign
//! fresh messages here.
//!
//! Needs both families: `cargo test -p gluonscan-core --features signature` (a workspace test
//! enables them through the facade's default `full`).
#![cfg(all(feature = "signature-evm", feature = "signature-solana"))]

use ed25519_dalek::Signer as _;
use gluonscan_core::signature::{
    verify_evm, verify_solana, verify_wallet_signature, SignatureError,
};
use gluonscan_core::Ecosystem;
use k256::ecdsa::SigningKey;
use proptest::prelude::*;
use serde_json::Value;

const VECTORS: &str = include_str!("fixtures/signature_vectors.json");

fn outcome(result: Result<bool, SignatureError>) -> &'static str {
    match result {
        Ok(true) => "verified",
        Ok(false) => "not_verified",
        Err(SignatureError::InvalidAddress(_)) => "invalid_address",
        Err(SignatureError::InvalidSignature(_)) => "invalid_signature",
        Err(other) => panic!("unexpected {other:?}"),
    }
}

fn parity(family: Ecosystem, key: &str) {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    let vectors = vectors[key].as_array().unwrap();
    assert!(vectors.len() > 40, "{key}: the fixture holds its vectors");
    let mismatches: Vec<String> = vectors
        .iter()
        .filter_map(|v| {
            let s = |k: &str| v[k].as_str().unwrap();
            let got = outcome(verify_wallet_signature(
                family,
                s("address"),
                s("message"),
                s("signature"),
            ));
            (got != s("outcome"))
                .then(|| format!("{}: expected {}, got {got}", s("name"), s("outcome")))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[test]
fn evm_matches_the_reference_on_every_vector() {
    parity(Ecosystem::Evm, "evm");
}

#[test]
fn solana_matches_the_reference_on_every_vector() {
    parity(Ecosystem::Solana, "solana");
}

#[test]
fn every_outcome_is_covered_by_the_vectors() {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    for key in ["evm", "solana"] {
        for expected in [
            "verified",
            "not_verified",
            "invalid_address",
            "invalid_signature",
        ] {
            assert!(
                vectors[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["outcome"] == expected),
                "{key} has a {expected} vector"
            );
        }
    }
}

// ---- EVM, signed here ----------------------------------------------------------------------

struct EvmKey(SigningKey);

impl EvmKey {
    /// `seed` 1..=254: `[0xff; 32]` is above the curve order and not a key.
    fn new(seed: u8) -> EvmKey {
        EvmKey(SigningKey::from_slice(&[seed; 32]).unwrap())
    }

    fn address(&self) -> String {
        let point = self.0.verifying_key().to_sec1_point(false);
        let hash = alloy_primitives::keccak256(&point.as_bytes()[1..]);
        format!("0x{}", hex::encode(&hash[12..]))
    }

    /// `personal_sign`: `r || s || v` with `v` = 27/28.
    fn sign(&self, message: &str) -> [u8; 65] {
        let mut prefixed = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
        prefixed.extend_from_slice(message.as_bytes());
        let hash = alloy_primitives::keccak256(&prefixed);
        let (sig, id) = self.0.sign_prehash_recoverable(hash.as_slice());
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = 27 + id.to_byte();
        out
    }
}

fn hx(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

#[test]
fn evm_personal_sign_verifies_in_every_accepted_form() {
    let key = EvmKey::new(1);
    let message = "Sign in to example.org\nNonce: 8f1c";
    let sig = key.sign(message);
    let address = key.address();
    assert_eq!(verify_evm(&address, message, &hx(&sig)), Ok(true));
    assert_eq!(
        verify_evm(&address, message, &hex::encode(sig)),
        Ok(true),
        "without 0x"
    );
    let mut bare_v = sig;
    bare_v[64] -= 27;
    assert_eq!(
        verify_evm(&address, message, &hx(&bare_v)),
        Ok(true),
        "v 0/1"
    );
    assert_eq!(
        verify_evm(
            &address.to_uppercase().replace("0X", "0x"),
            message,
            &hx(&sig)
        ),
        Ok(true),
        "address case does not matter"
    );
    assert_eq!(
        verify_wallet_signature(Ecosystem::Evm, &address, message, &hx(&sig)),
        Ok(true)
    );
}

#[test]
fn evm_wrong_address_or_altered_message_does_not_verify() {
    let key = EvmKey::new(1);
    let sig = hx(&key.sign("hello"));
    assert_eq!(
        verify_evm(&EvmKey::new(2).address(), "hello", &sig),
        Ok(false)
    );
    assert_eq!(verify_evm(&key.address(), "hello!", &sig), Ok(false));
    assert_eq!(verify_evm(&key.address(), "Hello", &sig), Ok(false));
}

#[test]
fn evm_malformed_input_is_an_error() {
    let key = EvmKey::new(1);
    let sig = hx(&key.sign("hello"));
    for bad in [
        "",
        "0x",
        "0xdeadbeef",
        "zz",
        &sig[..sig.len() - 1],
        &format!("{sig}00"),
    ] {
        assert!(
            matches!(
                verify_evm(&key.address(), "hello", bad),
                Err(SignatureError::InvalidSignature(_))
            ),
            "{bad:?}"
        );
    }
    for bad in [
        "",
        "0x",
        "0x1234",
        "not an address",
        &format!("{}0", key.address()),
    ] {
        assert!(
            matches!(
                verify_evm(bad, "hello", &sig),
                Err(SignatureError::InvalidAddress(_))
            ),
            "{bad:?}"
        );
    }
}

// ---- Solana, signed here -------------------------------------------------------------------

fn sol_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn b58(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

#[test]
fn solana_signature_verifies() {
    let key = sol_key(7);
    let message = "Sign in to example.org\nNonce: 8f1c";
    let sig = b58(&key.sign(message.as_bytes()).to_bytes());
    let address = b58(key.verifying_key().as_bytes());
    assert_eq!(verify_solana(&address, message, &sig), Ok(true));
    assert_eq!(
        verify_wallet_signature(Ecosystem::Solana, &address, message, &sig),
        Ok(true)
    );
}

#[test]
fn solana_wrong_address_or_altered_message_does_not_verify() {
    let key = sol_key(7);
    let sig = b58(&key.sign(b"hello").to_bytes());
    let other = b58(sol_key(8).verifying_key().as_bytes());
    assert_eq!(verify_solana(&other, "hello", &sig), Ok(false));
    let address = b58(key.verifying_key().as_bytes());
    assert_eq!(verify_solana(&address, "hello!", &sig), Ok(false));
}

#[test]
fn solana_malformed_input_is_an_error() {
    let key = sol_key(7);
    let address = b58(key.verifying_key().as_bytes());
    let sig = key.sign(b"hello").to_bytes();
    for bad in [
        String::new(),
        b58(&sig[..63]),
        "0OIl".into(),
        hex::encode(sig),
    ] {
        assert!(
            matches!(
                verify_solana(&address, "hello", &bad),
                Err(SignatureError::InvalidSignature(_))
            ),
            "{bad:?}"
        );
    }
    for bad in [String::new(), b58(&[1; 31]), "0OIl".into(), "1".repeat(45)] {
        assert!(
            matches!(
                verify_solana(&bad, "hello", &b58(&sig)),
                Err(SignatureError::InvalidAddress(_))
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn a_family_without_a_scheme_is_unsupported() {
    assert_eq!(
        verify_wallet_signature(Ecosystem::Bitcoin, "bc1q", "m", "s"),
        Err(SignatureError::UnsupportedFamily(Ecosystem::Bitcoin))
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_message_round_trips_and_any_change_breaks_it(message in ".{0,200}", seed in 1u8..=254) {
        let evm = EvmKey::new(seed);
        let sig = hx(&evm.sign(&message));
        prop_assert_eq!(verify_evm(&evm.address(), &message, &sig), Ok(true));
        prop_assert_eq!(verify_evm(&evm.address(), &format!("{message} "), &sig), Ok(false));

        let sol = sol_key(seed);
        let address = b58(sol.verifying_key().as_bytes());
        let sig = b58(&sol.sign(message.as_bytes()).to_bytes());
        prop_assert_eq!(verify_solana(&address, &message, &sig), Ok(true));
        prop_assert_eq!(verify_solana(&address, &format!("{message} "), &sig), Ok(false));
    }

    #[test]
    fn arbitrary_input_never_panics(address in ".{0,100}", message in ".{0,50}", signature in ".{0,200}") {
        for family in [Ecosystem::Evm, Ecosystem::Solana, Ecosystem::Bitcoin] {
            let _ = verify_wallet_signature(family, &address, &message, &signature);
        }
    }

    #[test]
    fn arbitrary_well_formed_bytes_never_panic(address in any::<[u8; 32]>(), sig in prop::collection::vec(any::<u8>(), 65)) {
        let _ = verify_evm(&hx(&address[..20]), "m", &hx(&sig));
        let _ = verify_solana(&b58(&address), "m", &b58(&sig[..64]));
    }
}
