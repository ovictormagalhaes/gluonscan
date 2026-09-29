//! # gluonscan-math
//!
//! Exact-integer DeFi math. No floats — deriving a price from a tick in floating point is a bug.
//!
//! Currently: Uniswap V3 Q64.96 tick math ([`get_sqrt_ratio_at_tick`]), a direct port of the
//! canonical `TickMath.getSqrtRatioAtTick`, checked against reference vectors (see tests).

use alloy_primitives::U256;

/// Minimum usable tick.
pub const MIN_TICK: i32 = -887_272;
/// Maximum usable tick.
pub const MAX_TICK: i32 = 887_272;

fn hx(s: &str) -> U256 {
    U256::from_str_radix(s, 16).expect("valid hex constant")
}

/// The sqrt price (as a Q64.96 fixed-point, i.e. `sqrt(1.0001^tick) * 2^96`) at a given tick.
///
/// Panics if `tick` is outside `[MIN_TICK, MAX_TICK]`.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> U256 {
    assert!(
        (MIN_TICK..=MAX_TICK).contains(&tick),
        "tick out of range: {tick}"
    );
    let abs = tick.unsigned_abs() as u64;

    let mut ratio = if abs & 0x1 != 0 {
        hx("fffcb933bd6fad37aa2d162d1a594001")
    } else {
        hx("100000000000000000000000000000000") // 2^128
    };

    macro_rules! apply {
        ($bit:expr, $c:expr) => {
            if abs & $bit != 0 {
                ratio = (ratio * hx($c)) >> 128;
            }
        };
    }
    apply!(0x2, "fff97272373d413259a46990580e213a");
    apply!(0x4, "fff2e50f5f656932ef12357cf3c7fdcc");
    apply!(0x8, "ffe5caca7e10e4e61c3624eaa0941cd0");
    apply!(0x10, "ffcb9843d60f6159c9db58835c926644");
    apply!(0x20, "ff973b41fa98c081472e6896dfb254c0");
    apply!(0x40, "ff2ea16466c96a3843ec78b326b52861");
    apply!(0x80, "fe5dee046a99a2a811c461f1969c3053");
    apply!(0x100, "fcbe86c7900a88aedcffc83b479aa3a4");
    apply!(0x200, "f987a7253ac413176f2b074cf7815e54");
    apply!(0x400, "f3392b0822b70005940c7a398e4b70f3");
    apply!(0x800, "e7159475a2c29b7443b29c7fa6e889d9");
    apply!(0x1000, "d097f3bdfd2022b8845ad8f792aa5825");
    apply!(0x2000, "a9f746462d870fdf8a65dc1f90e061e5");
    apply!(0x4000, "70d869a156d2a1b890bb3df62baf32f7");
    apply!(0x8000, "31be135f97d08fd981231505542fcfa6");
    apply!(0x10000, "9aa508b5b7a84e1c677de54f3e99bc9");
    apply!(0x20000, "5d6af8dedb81196699c329225ee604");
    apply!(0x40000, "2216e584f5fa1ea926041bedfe98");
    apply!(0x80000, "48a170391f7dc42444e8fa2");

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // sqrtPriceX96 = (ratio >> 32), rounded up if there is any remainder in the low 32 bits.
    let shifted: U256 = ratio >> 32;
    let low_mask: U256 = (U256::from(1u64) << 32) - U256::from(1u64);
    let low: U256 = ratio & low_mask;
    if low.is_zero() {
        shifted
    } else {
        shifted + U256::from(1u64)
    }
}

/// Whether the current tick lies within `[lower, upper)` — i.e. the position is in range.
pub fn is_in_range(current: i32, lower: i32, upper: i32) -> bool {
    current >= lower && current < upper
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference vectors from the canonical Uniswap V3 `TickMath`. If the port drifts, these break.
    #[test]
    fn tick_zero_is_two_pow_96() {
        assert_eq!(
            get_sqrt_ratio_at_tick(0),
            U256::from_str_radix("79228162514264337593543950336", 10).unwrap()
        );
    }

    #[test]
    fn min_tick_is_min_sqrt_ratio() {
        assert_eq!(
            get_sqrt_ratio_at_tick(MIN_TICK),
            U256::from(4_295_128_739u64)
        );
    }

    #[test]
    fn max_tick_is_max_sqrt_ratio() {
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK),
            U256::from_str_radix("1461446703485210103287273052203988822378723970342", 10).unwrap()
        );
    }

    #[test]
    fn monotonic_around_zero() {
        assert!(get_sqrt_ratio_at_tick(-1) < get_sqrt_ratio_at_tick(0));
        assert!(get_sqrt_ratio_at_tick(0) < get_sqrt_ratio_at_tick(1));
    }

    #[test]
    fn range_check() {
        assert!(is_in_range(0, -10, 10));
        assert!(!is_in_range(10, -10, 10));
        assert!(!is_in_range(-11, -10, 10));
    }
}
