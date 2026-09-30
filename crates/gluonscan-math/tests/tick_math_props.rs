//! Property tests for the exact-integer Uniswap V3 tick math.
//!
//! These pin the invariants a floating-point port would silently violate: the sqrt price is
//! strictly increasing in tick, liquidity value sits on the correct side of the price band, and
//! amounts never shrink as liquidity grows.
//!
//! Note: `mul_div` is a private helper in the crate, so it cannot be exercised from this
//! integration-test crate. It is covered indirectly through `get_amounts_for_liquidity`.

use alloy_primitives::U256;
use gluonscan_math::{get_amounts_for_liquidity, get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn sqrt_ratio_is_strictly_increasing_in_tick(
        a in MIN_TICK..=MAX_TICK,
        b in MIN_TICK..=MAX_TICK,
    ) {
        prop_assume!(a != b);
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        prop_assert!(
            get_sqrt_ratio_at_tick(lo) < get_sqrt_ratio_at_tick(hi),
            "sqrt ratio must strictly increase: tick {lo} vs {hi}"
        );
    }

    #[test]
    fn amounts_sit_on_the_correct_side_of_the_band(
        t1 in MIN_TICK..=MAX_TICK,
        t2 in MIN_TICK..=MAX_TICK,
        tc in MIN_TICK..=MAX_TICK,
        liquidity in any::<u128>(),
    ) {
        let (tick_lower, tick_upper) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
        let sqrt_lower = get_sqrt_ratio_at_tick(tick_lower);
        let sqrt_upper = get_sqrt_ratio_at_tick(tick_upper);
        let sqrt_price = get_sqrt_ratio_at_tick(tc);
        let (amount0, amount1) =
            get_amounts_for_liquidity(sqrt_price, sqrt_lower, sqrt_upper, U256::from(liquidity));

        if sqrt_price <= sqrt_lower {
            prop_assert_eq!(amount1, U256::ZERO, "below range: all value must be token0");
        } else if sqrt_price >= sqrt_upper {
            prop_assert_eq!(amount0, U256::ZERO, "above range: all value must be token1");
        }
        // Strictly in range: both legs may be non-zero (no zero guarantee at tiny liquidity).
    }

    #[test]
    fn amounts_are_monotonic_in_liquidity(
        t1 in MIN_TICK..=MAX_TICK,
        t2 in MIN_TICK..=MAX_TICK,
        tc in MIN_TICK..=MAX_TICK,
        la in any::<u128>(),
        lb in any::<u128>(),
    ) {
        let (tick_lower, tick_upper) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
        let (l_small, l_large) = if la <= lb { (la, lb) } else { (lb, la) };
        let sqrt_lower = get_sqrt_ratio_at_tick(tick_lower);
        let sqrt_upper = get_sqrt_ratio_at_tick(tick_upper);
        let sqrt_price = get_sqrt_ratio_at_tick(tc);

        let (small0, small1) =
            get_amounts_for_liquidity(sqrt_price, sqrt_lower, sqrt_upper, U256::from(l_small));
        let (large0, large1) =
            get_amounts_for_liquidity(sqrt_price, sqrt_lower, sqrt_upper, U256::from(l_large));

        prop_assert!(large0 >= small0, "amount0 must not shrink as liquidity grows");
        prop_assert!(large1 >= small1, "amount1 must not shrink as liquidity grows");
    }
}
