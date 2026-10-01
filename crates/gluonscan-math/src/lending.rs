//! Pure health-factor and liquidation-price math for lending positions.
//!
//! The health factor a protocol reports is
//!   `HF = Σ(collateral_usd × liquidationThreshold) / Σ(debt_usd × borrowFactor)`.
//! Given the per-asset risk params (governance-set; they move on a weeks timescale) plus fresh unit
//! prices, this recomputes the HF and the per-collateral liquidation price with no I/O — the shared
//! source of truth for an on-read reprice and a client-facing liquidation simulator, so both produce
//! identical numbers.
//!
//! Each leg carries `amount` + `price_usd` (not a standalone `value_usd`), so the USD value is always
//! `amount × price_usd`: vary the price and the value follows structurally, and the reported
//! liquidation price can never drift from the value it was derived from. All math is [`Decimal`]
//! (never `as f64` on raw amounts); the HF is returned as [`Decimal`] so callers own any lossy
//! conversion at the edge.

use rust_decimal::Decimal;

/// A collateral leg: token amount, current unit price, and the effective (eMode/elevation-aware)
/// liquidation threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct CollateralLeg {
    /// Token amount held as collateral.
    pub amount: Decimal,
    /// Current unit price in USD.
    pub price_usd: Decimal,
    /// Effective liquidation threshold (fraction in `[0,1]`).
    pub liquidation_threshold: Decimal,
}

impl CollateralLeg {
    /// USD value of the leg (`amount × price_usd`).
    pub fn value_usd(&self) -> Decimal {
        self.amount * self.price_usd
    }
    fn contribution(&self) -> Decimal {
        self.value_usd() * self.liquidation_threshold
    }
}

/// A debt leg: token amount, current unit price, and the borrow factor (≥ 1; Aave has no borrow
/// factor, so callers pin it to 1).
#[derive(Debug, Clone, PartialEq)]
pub struct DebtLeg {
    /// Token amount borrowed.
    pub amount: Decimal,
    /// Current unit price in USD.
    pub price_usd: Decimal,
    /// Borrow factor (≥ 1); pin to 1 where the protocol has none.
    pub borrow_factor: Decimal,
}

impl DebtLeg {
    /// USD value of the leg (`amount × price_usd`).
    pub fn value_usd(&self) -> Decimal {
        self.amount * self.price_usd
    }
}

/// `Σ(debt_usd × borrowFactor)` — the borrow-factor-adjusted debt.
pub fn adjusted_debt(debts: &[DebtLeg]) -> Decimal {
    debts.iter().map(|d| d.value_usd() * d.borrow_factor).sum()
}

/// `Σ(collateral_usd × liquidationThreshold)` — the liquidation limit.
pub fn liquidation_limit(collaterals: &[CollateralLeg]) -> Decimal {
    collaterals.iter().map(|c| c.contribution()).sum()
}

/// The health factor. `None` when there is no (adjusted) debt — a position with no borrow has no
/// liquidation risk, matching how the adapters treat it.
pub fn health_factor(collaterals: &[CollateralLeg], debts: &[DebtLeg]) -> Option<Decimal> {
    let debt = adjusted_debt(debts);
    if debt <= Decimal::ZERO {
        return None;
    }
    Some(liquidation_limit(collaterals) / debt)
}

/// Where a single collateral triggers liquidation, holding every other leg and the debt fixed.
#[derive(Debug, Clone, PartialEq)]
pub enum LiquidationPoint {
    /// The collateral's price would have to fall to `price_usd` (a `drop_fraction` of its current
    /// price, in `[0,1)`) for HF to reach 1.
    Price {
        /// The price at which HF reaches 1.
        price_usd: Decimal,
        /// How far the price must fall, as a fraction of the current price.
        drop_fraction: Decimal,
    },
    /// This collateral alone cannot trigger liquidation: even at price 0 the rest of the collateral
    /// still covers the debt.
    Safe,
    /// The position is already at/under HF=1: this asset's price would have to *rise* to restore
    /// health. There is no downside liquidation price.
    Underwater,
    /// No liquidation contribution (zero value or zero threshold) — undefined.
    Undefined,
}

/// Solve `HF = 1` for one collateral's price with the other legs and the debt held fixed.
/// `total_limit` is `Σ(value × threshold)` over all collateral (passed in so the batch form computes
/// it once).
fn liquidation_point_for(
    target: &CollateralLeg,
    total_limit: Decimal,
    debt: Decimal,
) -> LiquidationPoint {
    let contribution = target.contribution();
    if contribution <= Decimal::ZERO {
        return LiquidationPoint::Undefined;
    }
    // others = Σ_{i≠idx}(value×thr) = total − this leg's contribution.
    let others = total_limit - contribution;
    // price multiplier x such that value_idx·x·thr_idx + others = debt.
    let x = (debt - others) / contribution;
    if x <= Decimal::ZERO {
        return LiquidationPoint::Safe;
    }
    if x >= Decimal::ONE {
        // x ≥ 1 ⟺ HF ≤ 1 already: price must rise, not fall.
        return LiquidationPoint::Underwater;
    }
    LiquidationPoint::Price {
        price_usd: target.price_usd * x,
        drop_fraction: Decimal::ONE - x,
    }
}

/// Liquidation point for a single collateral by index.
pub fn liquidation_price(
    collaterals: &[CollateralLeg],
    debts: &[DebtLeg],
    idx: usize,
) -> LiquidationPoint {
    match collaterals.get(idx) {
        Some(target) => {
            liquidation_point_for(target, liquidation_limit(collaterals), adjusted_debt(debts))
        }
        None => LiquidationPoint::Undefined,
    }
}

/// Liquidation point for every collateral, aligned to input order. Computes the total liquidation
/// limit and adjusted debt once (O(n)), so a simulator can render all per-collateral liquidation
/// prices without O(n²) recomputation.
pub fn liquidation_prices(
    collaterals: &[CollateralLeg],
    debts: &[DebtLeg],
) -> Vec<LiquidationPoint> {
    let total_limit = liquidation_limit(collaterals);
    let debt = adjusted_debt(debts);
    collaterals
        .iter()
        .map(|c| liquidation_point_for(c, total_limit, debt))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn coll(amount: Decimal, price: Decimal, thr: Decimal) -> CollateralLeg {
        CollateralLeg {
            amount,
            price_usd: price,
            liquidation_threshold: thr,
        }
    }
    fn debt(amount: Decimal) -> DebtLeg {
        DebtLeg {
            amount,
            price_usd: dec!(1),
            borrow_factor: dec!(1),
        }
    }
    fn approx(a: Decimal, b: Decimal, eps: Decimal) {
        assert!((a - b).abs() < eps, "expected {a} ≈ {b}");
    }

    // Live Aave Base wallet: cbBTC + WETH collateral, USDC debt. Protocol-reported HF was 2.1700.
    #[test]
    fn aave_health_factor_matches_protocol() {
        let cols = vec![
            coll(dec!(0.4488771), dec!(83837.5), dec!(0.78)),
            coll(dec!(5.7531835), dec!(2660.55), dec!(0.83)),
        ];
        let hf = health_factor(&cols, &[debt(dec!(19381.28))]).unwrap();
        approx(hf, dec!(2.1700), dec!(0.001));
    }

    // Live Kamino Main Market obligation: SOL + cbBTC collateral, USDC debt. Protocol HF was 2.2305.
    #[test]
    fn kamino_health_factor_matches_protocol() {
        let cols = vec![
            coll(dec!(125.685), dec!(115.06), dec!(0.75)),
            coll(dec!(0.098516), dec!(83936.2), dec!(0.80)),
        ];
        let hf = health_factor(&cols, &[debt(dec!(7828.56))]).unwrap();
        approx(hf, dec!(2.2305), dec!(0.001));
    }

    #[test]
    fn no_debt_has_no_health_factor() {
        let cols = vec![coll(dec!(1000), dec!(1), dec!(0.8))];
        assert_eq!(health_factor(&cols, &[]), None);
    }

    // cbBTC liquidates at ~$19,070 (a ~77% drop from $83,837.50), WETH held fixed.
    #[test]
    fn aave_cbbtc_liquidation_price() {
        let cols = vec![
            coll(dec!(0.4488771), dec!(83837.5), dec!(0.78)),
            coll(dec!(5.7531835), dec!(2660.55), dec!(0.83)),
        ];
        match liquidation_price(&cols, &[debt(dec!(19381.28))], 0) {
            LiquidationPoint::Price {
                price_usd,
                drop_fraction,
            } => {
                approx(price_usd, dec!(19070), dec!(60));
                approx(drop_fraction, dec!(0.773), dec!(0.003));
            }
            other => panic!("expected a price, got {other:?}"),
        }
    }

    // WETH alone cannot liquidate: cbBTC's liquidation limit already exceeds the debt.
    #[test]
    fn aave_weth_alone_is_safe() {
        let cols = vec![
            coll(dec!(0.4488771), dec!(83837.5), dec!(0.78)),
            coll(dec!(5.7531835), dec!(2660.55), dec!(0.83)),
        ];
        assert_eq!(
            liquidation_price(&cols, &[debt(dec!(19381.28))], 1),
            LiquidationPoint::Safe
        );
    }

    // SOL liquidates at ~$12.87 (a ~89% drop from $115.06), cbBTC held fixed.
    #[test]
    fn kamino_sol_liquidation_price() {
        let cols = vec![
            coll(dec!(125.685), dec!(115.06), dec!(0.75)),
            coll(dec!(0.098516), dec!(83936.2), dec!(0.80)),
        ];
        match liquidation_price(&cols, &[debt(dec!(7828.56))], 0) {
            LiquidationPoint::Price {
                price_usd,
                drop_fraction,
            } => {
                approx(price_usd, dec!(12.87), dec!(0.1));
                approx(drop_fraction, dec!(0.888), dec!(0.003));
            }
            other => panic!("expected a price, got {other:?}"),
        }
    }

    // An already-underwater position (HF < 1): the target's price would have to RISE, so there is no
    // downside liquidation price.
    #[test]
    fn underwater_position_has_no_downside_liquidation_price() {
        // 1000 collateral @ thr 0.8 → limit 800; debt 900 → HF 0.888 < 1.
        let cols = vec![coll(dec!(1000), dec!(1), dec!(0.8))];
        assert_eq!(
            liquidation_price(&cols, &[debt(dec!(900))], 0),
            LiquidationPoint::Underwater
        );
    }

    #[test]
    fn borrow_factor_raises_effective_debt() {
        let cols = vec![coll(dec!(1000), dec!(1), dec!(0.8))];
        // debt 400 with borrow factor 2 => adjusted debt 800 => HF = 800/800 = 1.0
        let hf = health_factor(
            &cols,
            &[DebtLeg {
                amount: dec!(400),
                price_usd: dec!(1),
                borrow_factor: dec!(2),
            }],
        )
        .unwrap();
        approx(hf, dec!(1.0), dec!(0.0001));
    }

    #[test]
    fn zero_threshold_collateral_is_undefined_target() {
        let cols = vec![coll(dec!(1000), dec!(1), dec!(0))];
        assert_eq!(
            liquidation_price(&cols, &[debt(dec!(100))], 0),
            LiquidationPoint::Undefined
        );
    }

    #[test]
    fn batch_liquidation_prices_align_to_input_order() {
        let cols = vec![
            coll(dec!(0.4488771), dec!(83837.5), dec!(0.78)),
            coll(dec!(5.7531835), dec!(2660.55), dec!(0.83)),
        ];
        let pts = liquidation_prices(&cols, &[debt(dec!(19381.28))]);
        assert_eq!(pts.len(), 2);
        assert!(matches!(pts[0], LiquidationPoint::Price { .. }));
        assert_eq!(pts[1], LiquidationPoint::Safe);
        assert_eq!(pts[0], liquidation_price(&cols, &[debt(dec!(19381.28))], 0));
    }
}
