//! Fee schedule implementation for OrderBook trading fees

use serde::{Deserialize, Serialize};

/// Denominator for basis-point fee math: 1 bps = 1 / 10_000 of the notional.
const BPS_DENOMINATOR: u128 = 10_000;

/// Fee computation would overflow its `u128` intermediate product.
///
/// Returned by [`FeeSchedule::calculate_fee`] when `notional × |bps|`
/// overflows `u128`. Since 0.14.0 the engine never clamps a fee: the trade
/// path validates every taker's worst-case notional against the configured
/// schedule before it touches the book and rejects an unrepresentable one
/// untouched with [`OrderBookError::FeeOverflow`](crate::OrderBookError::FeeOverflow)
/// (#244). Venues can also enforce
/// [`FeeSchedule::max_guaranteed_exact_notional`] at admission time so this
/// error is provably unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "fee overflow: notional {notional} × |{bps}| bps exceeds the u128 domain; max guaranteed-exact notional at this rate is {max_guaranteed_exact_notional}"
)]
pub struct FeeOverflow {
    /// The notional value (price × quantity) that was passed in.
    pub notional: u128,
    /// The signed fee rate in basis points that applied (maker or taker).
    pub bps: i32,
    /// Largest notional guaranteed exact at this rate (the
    /// multiplication-safety bound) — equal to
    /// [`FeeSchedule::max_guaranteed_exact_notional_for_bps`]`(bps)`.
    pub max_guaranteed_exact_notional: u128,
}

impl FeeOverflow {
    #[cold]
    fn new(notional: u128, bps: i32) -> Self {
        Self {
            notional,
            bps,
            max_guaranteed_exact_notional: FeeSchedule::max_guaranteed_exact_notional_for_bps(bps),
        }
    }
}

impl From<FeeOverflow> for crate::orderbook::error::OrderBookError {
    /// Carry the overflow's fields into
    /// [`OrderBookError::FeeOverflow`](crate::OrderBookError::FeeOverflow).
    #[cold]
    fn from(err: FeeOverflow) -> Self {
        Self::FeeOverflow {
            notional: err.notional,
            bps: err.bps,
            max_guaranteed_exact_notional: err.max_guaranteed_exact_notional,
        }
    }
}

/// Configurable fee schedule for maker and taker fees
///
/// Fees are expressed in basis points (bps), where 1 bps = 0.01% = 0.0001.
/// Negative values represent rebates (common for maker fees to provide liquidity).
///
/// # Examples
///
/// ```
/// use orderbook_rs::FeeSchedule;
///
/// // Standard fee schedule: 5 bps taker fee, 2 bps maker rebate
/// let schedule = FeeSchedule::new(-2, 5);
///
/// // Calculate fee for a $10,000 trade
/// let notional = 10_000_000; // $10,000 in cents/micro-units
/// let taker_fee = schedule.calculate_fee(notional, false)?;
/// assert_eq!(taker_fee, 5_000); // 5 bps of $10,000 = $5.00
/// # Ok::<(), orderbook_rs::FeeOverflow>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Maker fee in basis points (negative = rebate)
    ///
    /// Positive values charge makers, negative values provide rebates.
    /// Typical values range from -10 (rebate) to +10 (fee).
    pub maker_fee_bps: i32,

    /// Taker fee in basis points
    ///
    /// Always positive or zero. Typical values range from 0 to 50 bps.
    pub taker_fee_bps: i32,
}

impl FeeSchedule {
    /// Create a new fee schedule
    ///
    /// # Arguments
    ///
    /// * `maker_fee_bps` - Maker fee in basis points (negative for rebates)
    /// * `taker_fee_bps` - Taker fee in basis points (must be non-negative)
    ///
    /// # Errors
    ///
    /// Returns `OrderBookError::InvalidFeeRate` if taker fee is negative.
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::FeeSchedule;
    ///
    /// // Standard exchange fees
    /// let schedule = FeeSchedule::new(-2, 5);
    /// assert_eq!(schedule.maker_fee_bps, -2);
    /// assert_eq!(schedule.taker_fee_bps, 5);
    /// ```
    #[must_use = "FeeSchedule does nothing unless used"]
    pub fn new(maker_fee_bps: i32, taker_fee_bps: i32) -> Self {
        Self {
            maker_fee_bps,
            taker_fee_bps,
        }
    }

    /// Calculate the fee for a transaction
    ///
    /// Returns `sign(bps) × ⌊notional × |bps| / 10_000⌋`, exact, or
    /// [`FeeOverflow`] when the intermediate product does not fit `u128`.
    /// Since 0.14.0 this is fallible and never clamps (#244); before it
    /// saturated the product to a magnitude of `u128::MAX / 10_000`.
    ///
    /// # Arguments
    ///
    /// * `notional` - The notional value of the trade (price × quantity)
    /// * `is_maker` - true if this is a maker transaction, false for taker
    ///
    /// # Returns
    ///
    /// The fee amount. Positive values represent charges, negative values
    /// represent rebates.
    ///
    /// # Rounding
    ///
    /// The fee is `notional × bps / 10_000` computed with integer division, which
    /// **truncates toward zero** — i.e. it rounds toward `0`, not toward `−∞`. The
    /// magnitude is computed in the unsigned domain and the sign of `bps` is
    /// applied afterward, so the rounding is symmetric in magnitude for a taker
    /// fee (positive `bps`) and a maker rebate (negative `bps`): both drop the
    /// fractional part rather than rounding it. For example a `notional` of
    /// `15_003` yields `+7` at `+5` bps and `−3` at `−2` bps (each `floor` of the
    /// magnitude `7.5015` / `3.0006`, then signed). External fee reconciliation
    /// should therefore truncate-toward-zero, not round-half-up.
    ///
    /// # Errors
    ///
    /// Returns [`FeeOverflow`] if and only if `notional × |bps|` does not
    /// fit in `u128`, i.e. iff
    /// `notional > Self::max_guaranteed_exact_notional_for_bps(bps)`. The
    /// bound is multiplication safety, not a tight exactness frontier: at
    /// isolated notionals above it the exact fee would still be
    /// representable, and this rejects conservatively there. A zero-bps
    /// rate never errors (the fee is exactly `0` for any notional).
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::FeeSchedule;
    ///
    /// let schedule = FeeSchedule::new(-2, 5);
    ///
    /// // Taker fee: 5 bps on $10,000 = $5.00
    /// assert_eq!(schedule.calculate_fee(10_000_000, false)?, 5_000);
    ///
    /// // Maker rebate: -2 bps on $10,000 = -$2.00
    /// assert_eq!(schedule.calculate_fee(10_000_000, true)?, -2_000);
    ///
    /// // Fractional bps × notional truncates toward zero in both directions.
    /// assert_eq!(schedule.calculate_fee(15_003, false)?, 7); // floor(7.5015)
    /// assert_eq!(schedule.calculate_fee(15_003, true)?, -3); // -floor(3.0006)
    ///
    /// // Beyond the guaranteed-exact bound the computation refuses to clamp.
    /// assert!(schedule.calculate_fee(u128::MAX, false).is_err());
    /// # Ok::<(), orderbook_rs::FeeOverflow>(())
    /// ```
    #[inline]
    pub fn calculate_fee(&self, notional: u128, is_maker: bool) -> Result<i128, FeeOverflow> {
        let bps = if is_maker {
            self.maker_fee_bps
        } else {
            self.taker_fee_bps
        };
        // Compute the magnitude in the u128 domain: notional * |bps| / 10_000.
        // Doing this as u128 (not i128) avoids truncating a notional above
        // i128::MAX into a negative value. If the product fits in u128, the
        // post-division magnitude is at most u128::MAX / 10_000 < i128::MAX,
        // so the i128 conversion and the negation below cannot fail — mapping
        // them to the same error keeps exactness a checked guarantee rather
        // than a comment.
        let overflow = || FeeOverflow::new(notional, bps);
        let product = notional
            .checked_mul(u128::from(bps.unsigned_abs()))
            .ok_or_else(overflow)?;
        let magnitude = product
            .checked_div(BPS_DENOMINATOR)
            .and_then(|m| i128::try_from(m).ok())
            .ok_or_else(overflow)?;
        if bps < 0 {
            magnitude.checked_neg().ok_or_else(overflow)
        } else {
            Ok(magnitude)
        }
    }

    /// Calculate the fee for a transaction, or fail instead of clamping
    ///
    /// Identical to [`Self::calculate_fee`], which is fallible since
    /// 0.14.0 (#244); kept as a deprecated alias for callers that adopted
    /// the exact-fee API in 0.10.4.
    ///
    /// # Errors
    ///
    /// Same as [`Self::calculate_fee`].
    #[deprecated(
        since = "0.14.0",
        note = "`calculate_fee` is fallible and never clamps since 0.14.0; call it instead"
    )]
    #[inline]
    pub fn try_calculate_fee(&self, notional: u128, is_maker: bool) -> Result<i128, FeeOverflow> {
        self.calculate_fee(notional, is_maker)
    }

    /// Check that both legs of this schedule can price `notional` exactly
    ///
    /// `Ok` when [`Self::calculate_fee`] succeeds for the maker **and** the
    /// taker rate at `notional`; for any smaller notional it then succeeds
    /// too, and the sum of the fees of trades whose notionals add up to at
    /// most `notional` is representable as well. The trade path runs this on
    /// every taker's worst-case notional before it touches the book (#244).
    /// Two checked multiplications, no allocation.
    ///
    /// # Errors
    ///
    /// [`FeeOverflow`] for the first leg (taker, then maker) that would
    /// overflow.
    #[inline]
    pub fn check_notional(&self, notional: u128) -> Result<(), FeeOverflow> {
        for bps in [self.taker_fee_bps, self.maker_fee_bps] {
            if notional
                .checked_mul(u128::from(bps.unsigned_abs()))
                .is_none()
            {
                return Err(FeeOverflow::new(notional, bps));
            }
        }
        Ok(())
    }

    /// Largest notional whose fee at `bps` is guaranteed exact
    ///
    /// This is the **multiplication-safety bound** `u128::MAX / |bps|`
    /// (`u128::MAX` for a zero rate): at or below it the intermediate
    /// product `notional × |bps|` cannot overflow, so
    /// [`Self::calculate_fee`] returns the exact `⌊notional × |bps| /
    /// 10_000⌋` (signed). Callers can enforce it at admission time, making
    /// [`FeeOverflow`] provably unreachable.
    ///
    /// The guarantee is sufficient, not tight: above the bound the
    /// multiplication overflows, so [`Self::calculate_fee`] always rejects,
    /// even though at isolated notionals the exact fee would still fit
    /// (e.g. `1 << 127` at 2 bps).
    ///
    /// # Arguments
    ///
    /// * `bps` - Fee rate in basis points; only its magnitude matters
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::FeeSchedule;
    ///
    /// assert_eq!(
    ///     FeeSchedule::max_guaranteed_exact_notional_for_bps(5),
    ///     u128::MAX / 5
    /// );
    /// assert_eq!(
    ///     FeeSchedule::max_guaranteed_exact_notional_for_bps(-2),
    ///     u128::MAX / 2
    /// );
    /// assert_eq!(
    ///     FeeSchedule::max_guaranteed_exact_notional_for_bps(0),
    ///     u128::MAX
    /// );
    /// ```
    #[must_use]
    #[inline]
    pub const fn max_guaranteed_exact_notional_for_bps(bps: i32) -> u128 {
        // `u32 as u128` is lossless; `From` is not usable in a `const fn`.
        match u128::MAX.checked_div(bps.unsigned_abs() as u128) {
            Some(bound) => bound,
            // Zero rate: every notional prices exactly to `0`.
            None => u128::MAX,
        }
    }

    /// Largest notional guaranteed exact for both legs of this schedule
    ///
    /// The minimum of [`Self::max_guaranteed_exact_notional_for_bps`] over
    /// the maker and taker rates — a single venue-level admission bound: any
    /// notional at or below it produces exact maker **and** taker fees from
    /// [`Self::calculate_fee`]. Like the
    /// per-rate bound, it is a sufficient guarantee, not a tight exactness
    /// frontier.
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::FeeSchedule;
    ///
    /// let schedule = FeeSchedule::new(-2, 5);
    /// assert_eq!(schedule.max_guaranteed_exact_notional(), u128::MAX / 5);
    ///
    /// // A zero-fee schedule never saturates.
    /// assert_eq!(
    ///     FeeSchedule::zero_fee().max_guaranteed_exact_notional(),
    ///     u128::MAX
    /// );
    /// ```
    #[must_use]
    #[inline]
    pub const fn max_guaranteed_exact_notional(&self) -> u128 {
        let maker = Self::max_guaranteed_exact_notional_for_bps(self.maker_fee_bps);
        let taker = Self::max_guaranteed_exact_notional_for_bps(self.taker_fee_bps);
        if maker < taker { maker } else { taker }
    }

    /// Check if this fee schedule provides maker rebates
    ///
    /// # Returns
    ///
    /// true if maker_fee_bps is negative (rebate), false if positive or zero (fee)
    #[must_use]
    #[inline]
    pub fn has_maker_rebate(&self) -> bool {
        self.maker_fee_bps < 0
    }

    /// Check if this fee schedule has zero fees
    ///
    /// # Returns
    ///
    /// true if both maker and taker fees are zero
    #[must_use]
    #[inline]
    pub fn is_zero_fee(&self) -> bool {
        self.maker_fee_bps == 0 && self.taker_fee_bps == 0
    }

    /// Create a zero-fee schedule
    ///
    /// # Returns
    ///
    /// A FeeSchedule with zero fees for both makers and takers
    #[must_use]
    pub fn zero_fee() -> Self {
        Self::new(0, 0)
    }

    /// Create a fee schedule with only taker fees (common in some exchanges)
    ///
    /// # Arguments
    ///
    /// * `taker_fee_bps` - Taker fee in basis points
    ///
    /// # Returns
    ///
    /// A FeeSchedule with zero maker fee and specified taker fee
    #[must_use]
    pub fn taker_only(taker_fee_bps: i32) -> Self {
        Self::new(0, taker_fee_bps)
    }

    /// Create a fee schedule with maker rebates
    ///
    /// # Arguments
    ///
    /// * `maker_rebate_bps` - Maker rebate in basis points; its magnitude is
    ///   used and the maker rate is `-|maker_rebate_bps|`
    /// * `taker_fee_bps` - Taker fee in basis points
    ///
    /// `-|x|` is representable for every `i32`, including `i32::MIN` (whose
    /// `abs()` would overflow), so this never panics (#244).
    ///
    /// # Returns
    ///
    /// A FeeSchedule with negative maker fee (rebate) and specified taker fee
    #[must_use]
    pub fn with_maker_rebate(maker_rebate_bps: i32, taker_fee_bps: i32) -> Self {
        let maker_fee_bps = match maker_rebate_bps.checked_neg() {
            // A positive input: its negation.
            Some(negated) if negated < 0 => negated,
            // Zero, a negative input, or `i32::MIN`: already `-|x|`.
            _ => maker_rebate_bps,
        };
        Self::new(maker_fee_bps, taker_fee_bps)
    }
}

impl Default for FeeSchedule {
    fn default() -> Self {
        Self::zero_fee()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_schedule_creation() {
        let schedule = FeeSchedule::new(-2, 5);
        assert_eq!(schedule.maker_fee_bps, -2);
        assert_eq!(schedule.taker_fee_bps, 5);
    }

    #[test]
    fn test_zero_fee() {
        let schedule = FeeSchedule::zero_fee();
        assert!(schedule.is_zero_fee());
        assert_eq!(schedule.maker_fee_bps, 0);
        assert_eq!(schedule.taker_fee_bps, 0);
    }

    #[test]
    fn test_taker_only() {
        let schedule = FeeSchedule::taker_only(10);
        assert_eq!(schedule.maker_fee_bps, 0);
        assert_eq!(schedule.taker_fee_bps, 10);
    }

    #[test]
    fn test_maker_rebate() {
        let schedule = FeeSchedule::with_maker_rebate(3, 7);
        assert_eq!(schedule.maker_fee_bps, -3);
        assert_eq!(schedule.taker_fee_bps, 7);
        assert!(schedule.has_maker_rebate());
    }

    #[test]
    fn test_with_maker_rebate_i32_extremes_never_panic() {
        // `-(i32::MIN).abs()` used to overflow; `-|i32::MIN|` is i32::MIN.
        assert_eq!(
            FeeSchedule::with_maker_rebate(i32::MIN, 5).maker_fee_bps,
            i32::MIN
        );
        assert_eq!(
            FeeSchedule::with_maker_rebate(i32::MAX, 5).maker_fee_bps,
            -i32::MAX
        );
        assert_eq!(FeeSchedule::with_maker_rebate(-4, 5).maker_fee_bps, -4);
        assert_eq!(FeeSchedule::with_maker_rebate(0, 5).maker_fee_bps, 0);
        assert!(!FeeSchedule::with_maker_rebate(0, 5).has_maker_rebate());
    }

    #[test]
    fn test_calculate_fee_i32_min_rate() {
        let schedule = FeeSchedule::with_maker_rebate(i32::MIN, 0);
        // 10_000 × 2^31 / 10_000 = 2^31, negated.
        assert_eq!(schedule.calculate_fee(10_000, true), Ok(-2_147_483_648));
        assert!(schedule.calculate_fee(u128::MAX, true).is_err());
    }

    #[test]
    fn test_calculate_taker_fee() {
        let schedule = FeeSchedule::new(-2, 5);
        assert_eq!(schedule.calculate_fee(100_000_000, false), Ok(50_000));
    }

    #[test]
    fn test_calculate_maker_rebate() {
        let schedule = FeeSchedule::new(-2, 5);
        assert_eq!(schedule.calculate_fee(100_000_000, true), Ok(-20_000));
    }

    #[test]
    fn test_calculate_fee_notional_above_i128_max_is_exact_or_rejected() {
        // A notional above i128::MAX once cast to a negative i128. The
        // magnitude is computed in the u128 domain, so at 1 bps the fee is
        // exact and positive; at 5 bps the product overflows and is rejected
        // rather than clamped (#244).
        let notional = u128::MAX;
        let one_bps = FeeSchedule::new(-1, 1);
        let expected = i128::try_from(u128::MAX / 10_000).unwrap();
        assert_eq!(one_bps.calculate_fee(notional, false), Ok(expected));
        assert_eq!(one_bps.calculate_fee(notional, true), Ok(-expected));

        let five_bps = FeeSchedule::new(-2, 5);
        assert!(five_bps.calculate_fee(notional, false).is_err());
        assert!(five_bps.calculate_fee(notional, true).is_err());
    }

    #[test]
    fn test_calculate_fee_unchanged_for_realistic_inputs() {
        let schedule = FeeSchedule::new(-2, 5);
        assert_eq!(schedule.calculate_fee(100_000_000, false), Ok(50_000));
        assert_eq!(schedule.calculate_fee(100_000_000, true), Ok(-20_000));
        // Non-multiple-of-10_000 notional: floor(15_003 * 5 / 10_000) = 7.
        assert_eq!(schedule.calculate_fee(15_003, false), Ok(7));
        // Maker rebate truncates toward zero.
        assert_eq!(schedule.calculate_fee(15_003, true), Ok(-3));
    }

    #[test]
    fn test_zero_fee_calculation() {
        let schedule = FeeSchedule::zero_fee();
        assert_eq!(schedule.calculate_fee(100_000_000, true), Ok(0));
        assert_eq!(schedule.calculate_fee(100_000_000, false), Ok(0));
    }

    #[test]
    fn test_large_notional() {
        let schedule = FeeSchedule::new(1, 1);
        let notional = u128::MAX / 10_000 - 1;
        let fee = schedule.calculate_fee(notional, false).unwrap();
        assert!(fee > 0);
        assert!(fee < i128::MAX);
    }

    #[test]
    fn test_edge_cases() {
        let schedule = FeeSchedule::new(-10_000, 10_000);
        assert_eq!(schedule.calculate_fee(10_000, true), Ok(-10_000));
        assert_eq!(schedule.calculate_fee(10_000, false), Ok(10_000));
    }

    #[test]
    #[allow(deprecated)]
    fn test_try_calculate_fee_alias_matches_calculate_fee() {
        let schedule = FeeSchedule::new(-2, 5);
        for notional in [0u128, 1, 15_003, 100_000_000, u128::MAX / 5, u128::MAX] {
            for is_maker in [true, false] {
                assert_eq!(
                    schedule.try_calculate_fee(notional, is_maker),
                    schedule.calculate_fee(notional, is_maker)
                );
            }
        }
    }

    #[test]
    fn test_calculate_fee_overflow_returns_error_with_fields() {
        let schedule = FeeSchedule::new(-2, 5);
        assert_eq!(
            schedule.calculate_fee(u128::MAX, false),
            Err(FeeOverflow {
                notional: u128::MAX,
                bps: 5,
                max_guaranteed_exact_notional: u128::MAX / 5,
            })
        );
        // The maker leg reports the signed rate (-2), not its magnitude.
        assert_eq!(
            schedule.calculate_fee(u128::MAX, true),
            Err(FeeOverflow {
                notional: u128::MAX,
                bps: -2,
                max_guaranteed_exact_notional: u128::MAX / 2,
            })
        );
    }

    #[test]
    fn test_calculate_fee_ok_at_bound_err_above_bound() {
        let schedule = FeeSchedule::new(-2, 5);
        let taker_bound = FeeSchedule::max_guaranteed_exact_notional_for_bps(5);
        let maker_bound = FeeSchedule::max_guaranteed_exact_notional_for_bps(-2);

        assert!(schedule.calculate_fee(taker_bound, false).is_ok());
        assert!(schedule.calculate_fee(taker_bound + 1, false).is_err());
        assert!(schedule.calculate_fee(maker_bound, true).is_ok());
        assert!(schedule.calculate_fee(maker_bound + 1, true).is_err());
    }

    #[test]
    fn test_calculate_fee_zero_bps_never_overflows() {
        let schedule = FeeSchedule::zero_fee();
        assert_eq!(schedule.calculate_fee(u128::MAX, false), Ok(0));
        assert_eq!(schedule.calculate_fee(u128::MAX, true), Ok(0));
    }

    #[test]
    fn test_check_notional_both_legs() {
        let schedule = FeeSchedule::new(-7, 5);
        assert_eq!(schedule.check_notional(u128::MAX / 7), Ok(()));
        // The taker leg (5 bps) fits, the maker leg (-7 bps) does not.
        assert_eq!(
            schedule.check_notional(u128::MAX / 7 + 1),
            Err(FeeOverflow {
                notional: u128::MAX / 7 + 1,
                bps: -7,
                max_guaranteed_exact_notional: u128::MAX / 7,
            })
        );
        assert_eq!(
            schedule.check_notional(u128::MAX).map_err(|e| e.bps),
            Err(5)
        );
        assert_eq!(FeeSchedule::zero_fee().check_notional(u128::MAX), Ok(()));
    }

    #[test]
    fn test_max_guaranteed_exact_notional_for_bps_values() {
        assert_eq!(
            FeeSchedule::max_guaranteed_exact_notional_for_bps(0),
            u128::MAX
        );
        assert_eq!(
            FeeSchedule::max_guaranteed_exact_notional_for_bps(1),
            u128::MAX
        );
        assert_eq!(
            FeeSchedule::max_guaranteed_exact_notional_for_bps(5),
            u128::MAX / 5
        );
        assert_eq!(
            FeeSchedule::max_guaranteed_exact_notional_for_bps(-2),
            u128::MAX / 2
        );
        assert_eq!(
            FeeSchedule::max_guaranteed_exact_notional_for_bps(i32::MIN),
            u128::MAX / 2_147_483_648
        );
    }

    #[test]
    fn test_max_guaranteed_exact_notional_takes_min_of_both_legs() {
        assert_eq!(
            FeeSchedule::new(-2, 5).max_guaranteed_exact_notional(),
            u128::MAX / 5
        );
        assert_eq!(
            FeeSchedule::new(-7, 5).max_guaranteed_exact_notional(),
            u128::MAX / 7
        );
        assert_eq!(
            FeeSchedule::zero_fee().max_guaranteed_exact_notional(),
            u128::MAX
        );
    }

    #[test]
    fn test_calculate_fee_rejects_conservatively_above_bound() {
        // The bound is multiplication safety, not a tight exactness
        // frontier: at notional 2^127 with 2 bps the product 2^128 overflows
        // u128, so the fee is rejected although it would fit i128.
        let schedule = FeeSchedule::new(0, 2);
        let notional = 1u128 << 127;
        assert!(notional > FeeSchedule::max_guaranteed_exact_notional_for_bps(2));
        assert!(schedule.calculate_fee(notional, false).is_err());
    }

    #[test]
    fn test_fee_overflow_display_contains_values() {
        let schedule = FeeSchedule::new(0, 5);
        let Err(err) = schedule.calculate_fee(u128::MAX, false) else {
            panic!("expected overflow error");
        };
        let msg = err.to_string();
        assert!(msg.contains("fee overflow"), "message was: {msg}");
        assert!(msg.contains(&u128::MAX.to_string()), "message was: {msg}");
        assert!(msg.contains('5'), "message was: {msg}");
        assert!(
            msg.contains(&(u128::MAX / 5).to_string()),
            "message was: {msg}"
        );
    }

    #[test]
    fn test_serialization() {
        let schedule = FeeSchedule::new(-2, 5);
        let json = serde_json::to_string(&schedule).unwrap();
        let deserialized: FeeSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(schedule, deserialized);
    }

    #[test]
    fn test_default() {
        let schedule = FeeSchedule::default();
        assert!(schedule.is_zero_fee());
    }
}
