//! Protection collar for elected stop orders (#302).
//!
//! Without a collar an elected trailing stop executes as an unpriced
//! immediate-or-cancel market order: in a thin book one print can elect a
//! chain of stops that sweeps one side down to its last level. A
//! [`StopProtection`] bounds that walk much like CME protection points: the
//! elected stop executes as an immediate-or-cancel **limit** order at its
//! stop price moved by the collar against it,
//!
//! - sell stop: limit at `stop - collar`;
//! - buy stop: limit at `stop + collar`,
//!
//! where `stop` is the stop's (trailed) stop price at election. Whatever does
//! not fill within that band is cancelled: an elected stop never leaves
//! liquidity on the book.
//!
//! **Unlike CME**, the remainder is cancelled, not rested at the limit: a
//! stop whose band is exhausted is consumed and leaves its position
//! **unprotected** (terminal state `Cancelled { StopProtectionBand }` when
//! liquidity remained beyond the limit). A gap of more than one collar
//! through the stop price therefore always consumes the stop with zero
//! fill. The collar bounds each child's price, not the cascade: a ladder of
//! stops spaced one collar apart still walks the book `k × collar` in one
//! call (there is no cascade depth limit or velocity pause).
//!
//! The collar is an absolute offset in price units (the same `u128` units
//! as order prices). It is configured per book with
//! [`OrderBook::set_stop_protection`](crate::OrderBook::set_stop_protection)
//! and travels in the snapshot package and in
//! [`ReplayBookConfig`](crate::orderbook::sequencer::ReplayBookConfig).
//! The type is available with and without the `special_orders` feature, so
//! snapshots and replay configurations carry it identically in every
//! build; it only has an effect where trailing stops exist
//! (`special_orders`).

use crate::orderbook::error::OrderBookError;
use pricelevel::{Price, Side};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU128;

/// The lowest representable price: a sell collar limit below it is
/// unbounded.
const LOWEST_PRICE: u128 = u128::MIN;

/// The highest representable price: a buy collar limit above it is
/// unbounded.
const HIGHEST_PRICE: u128 = u128::MAX;

/// Protection collar for elected stop orders (#302).
///
/// An elected stop executes as an immediate-or-cancel limit order at its
/// stop price moved by [`collar`](Self::collar) price units against it (see
/// [`limit_price`](Self::limit_price)); any remainder is cancelled (unlike
/// CME protection points, which rest it). The collar is never zero: "no
/// protection" is `None` on the book, not a zero collar.
///
/// # A collar wider than the price: no protection on that side
///
/// A **sell** stop whose collar reaches or exceeds its stop price
/// (`collar >= stop`) gets a limit of `0` (exactly `0` at equality,
/// clamped beyond): every bid is within its band, so the collar does
/// **not** protect it at all. The same holds for a **buy** stop whose
/// `stop + collar` reaches or exceeds `u128::MAX` (limit `u128::MAX`). This
/// is silent by design (no log per
/// election, which is a hot path): size the collar well below the stop
/// prices it protects. A stop trailing down towards the collar is covered
/// by the same rule, since the limit is computed from the trailed stop
/// price at election.
///
/// Serialized as `{"collar": <u128>}`; a zero collar is refused on
/// deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StopProtection {
    /// Distance from the stop price to the child's limit, in price units.
    collar: NonZeroU128,
}

impl StopProtection {
    /// A protection with a collar of `collar` price units.
    #[inline]
    #[must_use]
    pub const fn new(collar: NonZeroU128) -> Self {
        Self { collar }
    }

    /// A protection with a collar of `collar` price units.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::InvalidStopProtection`] when `collar` is zero.
    #[inline]
    pub fn try_new(collar: u128) -> Result<Self, OrderBookError> {
        NonZeroU128::new(collar)
            .map(Self::new)
            .ok_or_else(zero_collar)
    }

    /// The collar, in price units.
    #[inline]
    #[must_use]
    pub const fn collar(self) -> u128 {
        self.collar.get()
    }

    /// The limit price of the order a stop on `side` with stop price
    /// `stop_price` executes as when it is elected: `stop - collar` for a
    /// sell stop, `stop + collar` for a buy stop.
    ///
    /// When the band reaches or passes the representable bound (a sell
    /// collar `>=` the stop price, a buy `stop + collar >= u128::MAX`), the
    /// limit is the bound itself (`0`, `u128::MAX`): every price on that
    /// side of the stop is within the band, so the collar does not restrict
    /// the order there.
    #[inline]
    #[must_use]
    pub fn limit_price(self, side: Side, stop_price: Price) -> Price {
        Price::new(self.limit_for(side, stop_price.as_u128()))
    }

    /// [`Self::limit_price`] on raw price units (the matching path's unit).
    #[inline]
    #[must_use]
    pub(crate) fn limit_for(self, side: Side, stop: u128) -> u128 {
        // A band reaching or passing the representable bound is unbounded
        // on that side: its limit is the bound itself (reached exactly at
        // equality, clamped beyond), which every price satisfies.
        let (limit, unbounded) = match side {
            Side::Sell => (stop.checked_sub(self.collar()), LOWEST_PRICE),
            Side::Buy => (stop.checked_add(self.collar()), HIGHEST_PRICE),
        };
        match limit {
            Some(limit) => limit,
            None => unbounded,
        }
    }

    /// Checks the collar against the book's tick size: with a tick size the
    /// collar must be a multiple of it, so a collar limit derived from a
    /// tick-aligned stop price is tick-aligned too.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::InvalidTickSize`] (carrying the collar as `price`)
    /// when it is not a multiple of a non-zero `tick_size`.
    #[inline]
    pub fn check_tick_size(self, tick_size: Option<u128>) -> Result<(), OrderBookError> {
        match tick_size {
            Some(tick) if tick > 0 && !self.collar().is_multiple_of(tick) => {
                Err(misaligned_collar(self.collar(), tick))
            }
            _ => Ok(()),
        }
    }
}

impl From<NonZeroU128> for StopProtection {
    #[inline]
    fn from(collar: NonZeroU128) -> Self {
        Self::new(collar)
    }
}

/// The error for a zero collar.
#[cold]
#[inline(never)]
fn zero_collar() -> OrderBookError {
    OrderBookError::InvalidStopProtection {
        collar: 0,
        reason: "collar is zero; leave the protection unset (None) for no collar",
    }
}

/// The error for a collar that is not a multiple of the tick size.
#[cold]
#[inline(never)]
fn misaligned_collar(collar: u128, tick_size: u128) -> OrderBookError {
    OrderBookError::InvalidTickSize {
        price: collar,
        tick_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stop_protection_zero_collar_is_rejected() {
        assert!(matches!(
            StopProtection::try_new(0),
            Err(OrderBookError::InvalidStopProtection { collar: 0, .. })
        ));
        let protection = StopProtection::try_new(5).expect("non-zero");
        assert_eq!(protection.collar(), 5);
        assert_eq!(
            StopProtection::from(NonZeroU128::new(5).expect("non-zero")),
            protection
        );
    }

    #[test]
    fn test_stop_protection_limit_moves_against_the_stop() {
        let protection = StopProtection::try_new(3).expect("non-zero");
        assert_eq!(protection.limit_for(Side::Sell, 95), 92);
        assert_eq!(protection.limit_for(Side::Buy, 95), 98);
        assert_eq!(
            protection.limit_price(Side::Sell, Price::new(95)),
            Price::new(92)
        );
        // Exactly at the bounds.
        assert_eq!(protection.limit_for(Side::Sell, 3), 0);
        assert_eq!(protection.limit_for(Side::Buy, u128::MAX - 3), u128::MAX);
    }

    #[test]
    fn test_stop_protection_band_past_the_price_bounds_is_unbounded() {
        let protection = StopProtection::try_new(10).expect("non-zero");
        assert_eq!(protection.limit_for(Side::Sell, 4), 0, "sell underflow");
        assert_eq!(
            protection.limit_for(Side::Buy, u128::MAX - 4),
            u128::MAX,
            "buy overflow"
        );
        let widest = StopProtection::try_new(u128::MAX).expect("non-zero");
        assert_eq!(widest.limit_for(Side::Sell, u128::MAX), 0);
        assert_eq!(widest.limit_for(Side::Buy, 1), u128::MAX);
    }

    #[test]
    fn test_stop_protection_tick_alignment() {
        let protection = StopProtection::try_new(10).expect("non-zero");
        assert!(protection.check_tick_size(None).is_ok());
        assert!(protection.check_tick_size(Some(5)).is_ok());
        assert!(protection.check_tick_size(Some(10)).is_ok());
        assert!(
            protection.check_tick_size(Some(0)).is_ok(),
            "a zero tick disables validation, as for order prices"
        );
        assert!(matches!(
            protection.check_tick_size(Some(4)),
            Err(OrderBookError::InvalidTickSize {
                price: 10,
                tick_size: 4
            })
        ));
        assert!(protection.check_tick_size(Some(20)).is_err());
    }

    #[test]
    fn test_stop_protection_serde_round_trip_and_zero_refused() {
        let protection = StopProtection::try_new(25).expect("non-zero");
        let json = serde_json::to_string(&protection).expect("serialize");
        assert_eq!(json, r#"{"collar":25}"#);
        let back: StopProtection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, protection);
        assert!(serde_json::from_str::<StopProtection>(r#"{"collar":0}"#).is_err());
        let wide = StopProtection::try_new(u128::MAX).expect("non-zero");
        let json = serde_json::to_string(&wide).expect("serialize");
        assert_eq!(
            serde_json::from_str::<StopProtection>(&json).expect("u128 round trip"),
            wide
        );
    }
}
