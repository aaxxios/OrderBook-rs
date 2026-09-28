//! Functional-style iterators for order book analysis
//!
//! This module provides efficient, lazy iterators for analyzing order book depth
//! and structure without unnecessary allocations. All iterators support standard
//! iterator combinators and can short-circuit early.
//!
//! # Error surfacing (0.14.0, #245)
//!
//! Every iterator yields `Result<LevelInfo, OrderBookError>`. A level whose
//! `visible + hidden` total does not fit `u64` (a
//! [`PriceLevelError`](pricelevel::PriceLevelError) from
//! `PriceLevel::total_quantity`) or a cumulative depth that overflows `u64`
//! is yielded **once** as `Err(..)`; the iterator is then exhausted (every
//! later `next()` returns `None`), so a failed level is never silently read
//! as an empty one and no depth is ever reported past it. Collect with
//! `collect::<Result<Vec<_>, _>>()` or use `?` per item.

use super::error::OrderBookError;
use crossbeam_skiplist::SkipMap;
use crossbeam_skiplist::map::Iter;
use either::Either;
use pricelevel::{PriceLevel, Side};
use std::iter::Rev;
use std::sync::Arc;

/// Direction-erased iterator over price levels in a [`SkipMap`].
///
/// Wraps either a reverse (highest-to-lowest) or forward (lowest-to-highest) iterator:
/// bids ([`Side::Buy`]) iterate in descending price order, while asks ([`Side::Sell`])
/// iterate in ascending price order.
type PriceLevelIter<'a> =
    Either<Rev<Iter<'a, u128, Arc<PriceLevel>>>, Iter<'a, u128, Arc<PriceLevel>>>;

/// Total resting quantity (`visible + hidden`) of a price level, in quantity
/// units, with the level's own overflow surfaced as a typed error.
///
/// The single analytics entry point for a level's depth (#245): every
/// book analytic and depth iterator reads a level through this helper
/// instead of `total_quantity().unwrap_or(0)`, which used to read an
/// overflowed level as an empty one.
///
/// Advisory read: it sums two independent atomic counters (see
/// `pricelevel`'s `PriceLevel::total_quantity`).
///
/// # Errors
///
/// Returns [`OrderBookError::PriceLevelError`] when the level's
/// `visible + hidden` total overflows `u64` (for example a limit order and
/// an iceberg at one price whose combined depth exceeds `u64::MAX`).
#[inline]
pub(crate) fn level_total(level: &PriceLevel) -> Result<u64, OrderBookError> {
    Ok(level.total_quantity()?)
}

/// Checked `u64` running-depth accumulation shared by the depth iterators
/// and the book's depth analytics (#245).
///
/// # Errors
///
/// Returns [`OrderBookError::ArithmeticOverflow`] naming `operation` when
/// `acc + quantity` does not fit `u64`.
#[inline]
pub(crate) fn checked_depth_add(
    acc: u64,
    quantity: u64,
    operation: &'static str,
) -> Result<u64, OrderBookError> {
    acc.checked_add(quantity)
        .ok_or_else(|| analytics_overflow(operation))
}

/// Checked `acc + price * quantity` in `u128` for the notional aggregates
/// (VWAP, market impact, simulated fills, weighted depth) (#245).
///
/// # Errors
///
/// Returns [`OrderBookError::ArithmeticOverflow`] naming `operation` when
/// the product or the sum does not fit `u128`.
#[inline]
pub(crate) fn checked_notional_add(
    acc: u128,
    price: u128,
    quantity: u64,
    operation: &'static str,
) -> Result<u128, OrderBookError> {
    price
        .checked_mul(u128::from(quantity))
        .and_then(|notional| acc.checked_add(notional))
        .ok_or_else(|| analytics_overflow(operation))
}

/// Cold constructor for [`OrderBookError::ArithmeticOverflow`], kept off the
/// analytics loops' fast path.
#[cold]
#[inline(never)]
#[must_use]
pub(crate) fn analytics_overflow(operation: &'static str) -> OrderBookError {
    OrderBookError::ArithmeticOverflow { operation }
}

/// Information about a price level including price, quantity, and cumulative depth
#[derive(Debug, Clone)]
pub struct LevelInfo {
    /// The price of this level (in price units)
    pub price: u128,

    /// Total quantity at this price level (in units)
    pub quantity: u64,

    /// Cumulative depth up to and including this level (in units)
    pub cumulative_depth: u64,
}

/// Iterator over price levels with cumulative depth tracking
///
/// Iterates through price levels in price-priority order (best to worst),
/// maintaining cumulative depth as it goes. This is useful for analyzing
/// market depth distribution and finding liquidity thresholds.
///
/// Yields `Result<LevelInfo, OrderBookError>`; the first error ends the
/// iteration (see the [module docs](self)).
pub struct LevelsWithCumulativeDepth<'a> {
    iter: PriceLevelIter<'a>,
    cumulative_depth: u64,
    finished: bool,
}

impl<'a> LevelsWithCumulativeDepth<'a> {
    /// Creates a new iterator over levels with cumulative depth
    ///
    /// Crate-private since 0.13.0 (#228): the parameter is a reference to
    /// the book's live level map, and no public API hands one out. Obtain
    /// the iterator from
    /// [`OrderBook::levels_with_cumulative_depth`](crate::OrderBook::levels_with_cumulative_depth).
    ///
    /// # Arguments
    /// - `price_levels`: Reference to the SkipMap of price levels
    /// - `side`: Side to iterate (Buy for bids, Sell for asks)
    pub(crate) fn new(price_levels: &'a SkipMap<u128, Arc<PriceLevel>>, side: Side) -> Self {
        let iter = match side {
            Side::Buy => Either::Left(price_levels.iter().rev()), // Highest to lowest
            Side::Sell => Either::Right(price_levels.iter()),     // Lowest to highest
        };

        Self {
            iter,
            cumulative_depth: 0,
            finished: false,
        }
    }
}

impl<'a> Iterator for LevelsWithCumulativeDepth<'a> {
    type Item = Result<LevelInfo, OrderBookError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let entry = self.iter.next()?;
        let price = *entry.key();
        let step = level_total(entry.value()).and_then(|quantity| {
            checked_depth_add(self.cumulative_depth, quantity, "cumulative depth")
                .map(|cumulative| (quantity, cumulative))
        });
        match step {
            Ok((quantity, cumulative_depth)) => {
                self.cumulative_depth = cumulative_depth;
                Some(Ok(LevelInfo {
                    price,
                    quantity,
                    cumulative_depth,
                }))
            }
            Err(err) => {
                self.finished = true;
                Some(Err(err))
            }
        }
    }
}

impl std::iter::FusedIterator for LevelsWithCumulativeDepth<'_> {}

/// Iterator over price levels until a target depth is reached
///
/// Stops automatically when the cumulative depth reaches or exceeds the target.
/// Useful for analyzing how many levels are needed to fill a specific quantity.
///
/// Yields `Result<LevelInfo, OrderBookError>`; the first error ends the
/// iteration (see the [module docs](self)).
pub struct LevelsUntilDepth<'a> {
    iter: PriceLevelIter<'a>,
    target_depth: u64,
    cumulative_depth: u64,
    finished: bool,
}

impl<'a> LevelsUntilDepth<'a> {
    /// Creates a new iterator that stops at target depth
    ///
    /// Crate-private since 0.13.0 (#228): the parameter is a reference to
    /// the book's live level map, and no public API hands one out. Obtain
    /// the iterator from
    /// [`OrderBook::levels_until_depth`](crate::OrderBook::levels_until_depth).
    ///
    /// # Arguments
    /// - `price_levels`: Reference to the SkipMap of price levels
    /// - `side`: Side to iterate (Buy for bids, Sell for asks)
    /// - `target_depth`: Target cumulative depth (in units)
    pub(crate) fn new(
        price_levels: &'a SkipMap<u128, Arc<PriceLevel>>,
        side: Side,
        target_depth: u64,
    ) -> Self {
        let iter = match side {
            Side::Buy => Either::Left(price_levels.iter().rev()),
            Side::Sell => Either::Right(price_levels.iter()),
        };

        Self {
            iter,
            target_depth,
            cumulative_depth: 0,
            finished: false,
        }
    }
}

impl<'a> Iterator for LevelsUntilDepth<'a> {
    type Item = Result<LevelInfo, OrderBookError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        let entry = self.iter.next()?;
        let price = *entry.key();
        let step = level_total(entry.value()).and_then(|quantity| {
            checked_depth_add(self.cumulative_depth, quantity, "cumulative depth")
                .map(|cumulative| (quantity, cumulative))
        });
        match step {
            Ok((quantity, cumulative_depth)) => {
                self.cumulative_depth = cumulative_depth;
                // Check if we've reached target depth
                if cumulative_depth >= self.target_depth {
                    self.finished = true;
                }
                Some(Ok(LevelInfo {
                    price,
                    quantity,
                    cumulative_depth,
                }))
            }
            Err(err) => {
                self.finished = true;
                Some(Err(err))
            }
        }
    }
}

impl std::iter::FusedIterator for LevelsUntilDepth<'_> {}

/// Iterator over price levels within a specific price range
///
/// Only yields levels where the price falls within [min_price, max_price] inclusive.
/// Useful for analyzing liquidity in specific price bands.
///
/// Yields `Result<LevelInfo, OrderBookError>`; the first error ends the
/// iteration (see the [module docs](self)).
pub struct LevelsInRange<'a> {
    iter: PriceLevelIter<'a>,
    side: Side,
    min_price: u128,
    max_price: u128,
    finished: bool,
}

impl<'a> LevelsInRange<'a> {
    /// Creates a new iterator over levels in a price range
    ///
    /// Crate-private since 0.13.0 (#228): the parameter is a reference to
    /// the book's live level map, and no public API hands one out. Obtain
    /// the iterator from
    /// [`OrderBook::levels_in_range`](crate::OrderBook::levels_in_range).
    ///
    /// # Arguments
    /// - `price_levels`: Reference to the SkipMap of price levels
    /// - `side`: Side to iterate (Buy for bids, Sell for asks)
    /// - `min_price`: Minimum price (inclusive, in price units)
    /// - `max_price`: Maximum price (inclusive, in price units)
    pub(crate) fn new(
        price_levels: &'a SkipMap<u128, Arc<PriceLevel>>,
        side: Side,
        min_price: u128,
        max_price: u128,
    ) -> Self {
        let iter = match side {
            Side::Buy => Either::Left(price_levels.iter().rev()),
            Side::Sell => Either::Right(price_levels.iter()),
        };

        Self {
            iter,
            side,
            min_price,
            max_price,
            finished: false,
        }
    }
}

impl<'a> Iterator for LevelsInRange<'a> {
    type Item = Result<LevelInfo, OrderBookError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        for entry in self.iter.by_ref() {
            let price = *entry.key();

            // Ordered early exit: the underlying SkipMap iteration is sorted, so
            // once a price passes the FAR edge of the band no later entry can be
            // in range. Buy iterates descending (high→low): a price below the
            // band ends it. Sell iterates ascending (low→high): a price above the
            // band ends it.
            let past_far_edge = match self.side {
                Side::Buy => price < self.min_price,
                Side::Sell => price > self.max_price,
            };
            if past_far_edge {
                self.finished = true;
                return None;
            }

            // Check if price is within range.
            if price >= self.min_price && price <= self.max_price {
                return match level_total(entry.value()) {
                    Ok(quantity) => Some(Ok(LevelInfo {
                        price,
                        quantity,
                        cumulative_depth: 0, // Not tracked in range iterator
                    })),
                    Err(err) => {
                        self.finished = true;
                        Some(Err(err))
                    }
                };
            }
            // Otherwise we are still on the NEAR side of the band (Buy: above
            // max; Sell: below min) — keep scanning toward it.
        }

        self.finished = true;
        None
    }
}

impl std::iter::FusedIterator for LevelsInRange<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map(prices: impl IntoIterator<Item = u128>) -> SkipMap<u128, Arc<PriceLevel>> {
        let map = SkipMap::new();
        for p in prices {
            map.insert(p, Arc::new(PriceLevel::new(p)));
        }
        map
    }

    #[test]
    fn test_levels_in_range_terminates_at_far_edge_sell() {
        // Wide ascending book 1..=1000, narrow band [10, 12] near the low end.
        let map = make_map(1..=1000u128);
        let mut it = LevelsInRange::new(&map, Side::Sell, 10, 12);
        let prices: Vec<u128> = (&mut it).map(|l| l.expect("level").price).collect();
        assert_eq!(prices, vec![10, 11, 12], "only in-band levels are yielded");
        assert!(
            it.finished,
            "iterator marks itself finished at the far edge"
        );
        // Early-exit proof: the underlying iterator was NOT drained to the end —
        // entries past the far edge (13..=1000) remain. A non-short-circuiting
        // scan would have consumed all of them.
        assert!(
            it.iter.next().is_some(),
            "iteration must stop at the far edge, leaving later entries unconsumed"
        );
    }

    #[test]
    fn test_levels_in_range_terminates_at_far_edge_buy() {
        // Buy iterates descending; narrow band [988, 990] near the high end.
        let map = make_map(1..=1000u128);
        let mut it = LevelsInRange::new(&map, Side::Buy, 988, 990);
        let prices: Vec<u128> = (&mut it).map(|l| l.expect("level").price).collect();
        assert_eq!(prices, vec![990, 989, 988], "descending in-band yield");
        assert!(it.finished);
        assert!(
            it.iter.next().is_some(),
            "iteration must stop below the band, leaving lower entries unconsumed"
        );
    }

    #[test]
    fn test_levels_in_range_empty_when_band_outside_book() {
        let map = make_map(1..=10u128);
        let got: Vec<u128> = LevelsInRange::new(&map, Side::Sell, 100, 200)
            .map(|l| l.expect("level").price)
            .collect();
        assert!(got.is_empty());
    }
}
