//! Property and guard tests for the base-unit <-> Decimal money conversions.
//!
//! Data integrity is non-negotiable: these conversions must round-trip exactly on the
//! representable range and fail closed (`Error::Integrity`, never a panic or a fabricated value)
//! everywhere else.

use gluonscan_core::{scaled, to_raw, Error, U256};
use proptest::prelude::*;

// 2^96 - 1: the largest integer rust_decimal's 96-bit mantissa can hold. Since `scaled(raw, d)`
// produces a Decimal whose mantissa is exactly `raw` (value `raw * 10^-d`, scale `d`), this is the
// largest raw base-unit value `scaled` can represent regardless of `decimals`.
const MAX_DECIMAL_MANTISSA: u128 = 79_228_162_514_264_337_593_543_950_335;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn scaled_to_raw_is_exact_round_trip(bits in any::<u128>(), decimals in 0u8..=28) {
        let raw_u128 = bits % (MAX_DECIMAL_MANTISSA + 1);
        let raw = U256::from(raw_u128);
        let dec = scaled(raw, decimals).expect("value within Decimal range must scale");
        let back = to_raw(dec, decimals).expect("a non-negative decimal must convert back");
        prop_assert_eq!(back, raw);
    }
}

#[test]
fn scaled_rejects_more_than_28_decimals() {
    let err = scaled(U256::from(1u64), 29).unwrap_err();
    assert!(
        matches!(err, Error::Integrity { .. }),
        "29 decimals must be rejected with Integrity, got {err:?}"
    );
}

#[test]
fn scaled_rejects_value_beyond_decimal_range() {
    // u128::MAX (~3.4e38) with 0 decimals overflows Decimal's ~7.9e28 mantissa: fail closed,
    // never a wrapped/truncated value.
    let err = scaled(U256::from(u128::MAX), 0).unwrap_err();
    assert!(
        matches!(err, Error::Integrity { .. }),
        "a value beyond Decimal range must be rejected with Integrity, got {err:?}"
    );
}

#[test]
fn to_raw_rejects_negative_amount() {
    let negative = -scaled(U256::from(1u64), 6).unwrap();
    let err = to_raw(negative, 6).unwrap_err();
    assert!(
        matches!(err, Error::Integrity { .. }),
        "a negative amount has no base-unit representation, got {err:?}"
    );
}
