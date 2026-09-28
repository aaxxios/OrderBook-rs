//! Analytics overflow and level-error surfacing (#245).
//!
//! Every book analytic returns `Result<_, OrderBookError>`: a level whose
//! `visible + hidden` total overflows `u64` surfaces as
//! `OrderBookError::PriceLevelError`, and a `u64` depth sum or `u128`
//! notional that overflows surfaces as `OrderBookError::ArithmeticOverflow`
//! (never a panic, a wrapped value or a clamped one).

#[cfg(test)]
mod tests {
    use crate::orderbook::book::MAX_DEPTH_DISTRIBUTION_BINS;
    use crate::{OrderBook, OrderBookError};
    use pricelevel::{Id, Side, TimeInForce};

    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    fn is_level_error<T: std::fmt::Debug>(result: &Result<T, OrderBookError>) -> bool {
        matches!(result, Err(OrderBookError::PriceLevelError(_)))
    }

    fn is_overflow<T: std::fmt::Debug>(result: &Result<T, OrderBookError>) -> bool {
        matches!(result, Err(OrderBookError::ArithmeticOverflow { .. }))
    }

    /// A real trigger for a level-total overflow: a limit order and an
    /// iceberg at one price whose visible counters sum to `u64::MAX` while
    /// the iceberg also carries a hidden tranche, so `visible + hidden`
    /// exceeds `u64` although each counter fits.
    fn book_with_overflowed_level(side: Side) -> OrderBook<()> {
        let book = OrderBook::<()>::new("OVF");
        book.add_limit_order(new_id(), 100, u64::MAX - 1, side, TimeInForce::Gtc, None)
            .expect("limit order fits the visible counter");
        book.add_iceberg_order(new_id(), 100, 1, 1, side, TimeInForce::Gtc, None)
            .expect("iceberg fits both counters");
        book
    }

    /// Two levels on one side whose depths each fit `u64` but whose sum
    /// does not.
    fn book_with_u64_depth_overflow(side: Side) -> OrderBook<()> {
        let book = OrderBook::<()>::new("OVF");
        let (near, far) = match side {
            Side::Buy => (100, 99),
            Side::Sell => (100, 101),
        };
        book.add_limit_order(new_id(), near, u64::MAX, side, TimeInForce::Gtc, None)
            .expect("u64::MAX fits one level");
        book.add_limit_order(new_id(), far, u64::MAX, side, TimeInForce::Gtc, None)
            .expect("u64::MAX fits one level");
        book
    }

    // ---- level_total error propagation --------------------------------

    #[test]
    fn test_level_total_overflow_is_a_real_book_state() {
        let book = book_with_overflowed_level(Side::Buy);
        assert!(is_level_error(
            &book.total_quantity_at_price(100, Side::Buy)
        ));
        assert_eq!(
            book.total_quantity_at_price(101, Side::Buy)
                .expect("absent level is not an error"),
            None
        );
    }

    #[test]
    fn test_level_total_overflow_propagates_through_every_bid_analytic() {
        let book = book_with_overflowed_level(Side::Buy);
        // A healthy ask so two-sided analytics reach the bid level.
        book.add_limit_order(new_id(), 101, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");

        assert!(is_level_error(&book.price_at_depth(1, Side::Buy)));
        assert!(is_level_error(
            &book.cumulative_depth_to_target(1, Side::Buy)
        ));
        assert!(is_level_error(&book.total_depth_at_levels(1, Side::Buy)));
        assert!(is_level_error(&book.price_at_depth_adjusted(
            1,
            1,
            Side::Buy
        )));
        assert!(is_level_error(&book.liquidity_in_range(0, 200, Side::Buy)));
        assert!(is_level_error(&book.vwap(1, Side::Sell)));
        assert!(is_level_error(&book.market_impact(1, Side::Sell)));
        assert!(is_level_error(&book.simulate_market_order(1, Side::Sell)));
        assert!(is_level_error(&book.micro_price()));
        assert!(is_level_error(&book.order_book_imbalance(5)));
        assert!(is_level_error(&book.buy_sell_pressure()));
        assert!(is_level_error(&book.depth_statistics(Side::Buy, 0)));
        assert!(is_level_error(&book.is_thin_book(1, 5)));
        assert!(is_level_error(&book.depth_distribution(Side::Buy, 4)));
        assert!(is_level_error(&book.get_volume_by_price()));
        assert!(is_level_error(&book.find_level(Side::Buy, |_| true)));

        // The healthy ask side is unaffected.
        assert_eq!(book.total_depth_at_levels(5, Side::Sell).expect("asks"), 5);
        assert_eq!(book.vwap(5, Side::Buy).expect("asks"), Some(101.0));
    }

    #[test]
    fn test_level_total_overflow_propagates_through_every_ask_analytic() {
        let book = book_with_overflowed_level(Side::Sell);
        assert!(is_level_error(&book.price_at_depth(1, Side::Sell)));
        assert!(is_level_error(&book.total_depth_at_levels(1, Side::Sell)));
        assert!(is_level_error(&book.liquidity_in_range(0, 200, Side::Sell)));
        assert!(is_level_error(&book.vwap(1, Side::Buy)));
        assert!(is_level_error(&book.market_impact(1, Side::Buy)));
        assert!(is_level_error(&book.simulate_market_order(1, Side::Buy)));
        assert!(is_level_error(&book.buy_sell_pressure()));
        assert!(is_level_error(&book.depth_statistics(Side::Sell, 0)));
        assert!(is_level_error(&book.depth_distribution(Side::Sell, 1)));
    }

    #[test]
    fn test_level_total_overflow_fails_enriched_snapshot_metrics() {
        let book = book_with_overflowed_level(Side::Buy);
        let result = book.enriched_snapshot(10);
        assert!(
            result.is_err(),
            "the overflowed level must not be read as empty depth: {result:?}"
        );
    }

    // ---- iterator error surfacing --------------------------------------

    #[test]
    fn test_iterators_yield_level_error_once_then_stop() {
        let book = book_with_overflowed_level(Side::Buy);
        // A healthy level behind the failed one must never be reached.
        book.add_limit_order(new_id(), 90, 7, Side::Buy, TimeInForce::Gtc, None)
            .expect("deeper bid");

        let mut it = book.levels_with_cumulative_depth(Side::Buy);
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::PriceLevelError(_)))
        ));
        assert!(it.next().is_none(), "exhausted after the error");
        assert!(it.next().is_none(), "fused");

        let mut it = book.levels_until_depth(u64::MAX, Side::Buy);
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::PriceLevelError(_)))
        ));
        assert!(it.next().is_none());

        let mut it = book.levels_in_range(0, 200, Side::Buy);
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::PriceLevelError(_)))
        ));
        assert!(it.next().is_none());

        // Collecting surfaces the error instead of a truncated vector.
        let collected: Result<Vec<_>, _> = book.levels_with_cumulative_depth(Side::Buy).collect();
        assert!(collected.is_err());
    }

    #[test]
    fn test_iterators_yield_healthy_levels_before_the_failed_one() {
        let book = book_with_overflowed_level(Side::Buy);
        book.add_limit_order(new_id(), 110, 3, Side::Buy, TimeInForce::Gtc, None)
            .expect("better bid");

        let mut it = book.levels_with_cumulative_depth(Side::Buy);
        let first = it.next().expect("some").expect("healthy best level");
        assert_eq!(
            (first.price, first.quantity, first.cumulative_depth),
            (110, 3, 3)
        );
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::PriceLevelError(_)))
        ));
        assert!(it.next().is_none());

        // Range iterator yields the healthy in-band level, then the error.
        let mut it = book.levels_in_range(100, 110, Side::Buy);
        assert_eq!(it.next().expect("some").expect("healthy").price, 110);
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::PriceLevelError(_)))
        ));
        assert!(it.next().is_none());

        // A range that excludes the failed level never reads it.
        let only_healthy: Vec<_> = book
            .levels_in_range(105, 200, Side::Buy)
            .collect::<Result<_, _>>()
            .expect("failed level out of band");
        assert_eq!(only_healthy.len(), 1);
    }

    #[test]
    fn test_iterators_cumulative_depth_overflow_is_reported() {
        let book = book_with_u64_depth_overflow(Side::Buy);
        let mut it = book.levels_with_cumulative_depth(Side::Buy);
        let first = it.next().expect("some").expect("first level fits");
        assert_eq!(first.cumulative_depth, u64::MAX);
        assert!(matches!(
            it.next(),
            Some(Err(OrderBookError::ArithmeticOverflow { .. }))
        ));
        assert!(it.next().is_none());

        // The until-depth iterator stops at the first level when that
        // already reaches the target: the overflow is never computed.
        let reached: Vec<_> = book
            .levels_until_depth(u64::MAX, Side::Buy)
            .collect::<Result<_, _>>()
            .expect("target reached before overflow");
        assert_eq!(reached.len(), 1);

        // The range iterator does not accumulate, so it cannot overflow.
        let levels: Vec<_> = book
            .levels_in_range(0, 200, Side::Buy)
            .collect::<Result<_, _>>()
            .expect("no accumulation");
        assert_eq!(levels.len(), 2);
    }

    // ---- u64 depth aggregates --------------------------------------------

    #[test]
    fn test_u64_depth_sums_return_overflow_error() {
        for side in [Side::Buy, Side::Sell] {
            let book = book_with_u64_depth_overflow(side);
            assert!(is_overflow(&book.total_depth_at_levels(2, side)));
            assert!(is_overflow(&book.liquidity_in_range(0, 200, side)));
            assert!(is_overflow(&book.buy_sell_pressure()));
            assert!(is_overflow(&book.depth_statistics(side, 0)));
            assert!(is_overflow(&book.is_thin_book(1, 2)));
            assert!(is_overflow(&book.order_book_imbalance(2)));
            // A single bin collects both levels: its volume overflows.
            assert!(is_overflow(&book.depth_distribution(side, 1)));
            // The cumulative walk past the first level overflows before a
            // target beyond u64::MAX could be compared.
            assert!(is_overflow(&book.find_level(side, |_| false)));

            // Taking only the first level fits exactly.
            assert_eq!(
                book.total_depth_at_levels(1, side).expect("one level"),
                u64::MAX
            );
            assert!(
                book.price_at_depth(u64::MAX, side)
                    .expect("reached")
                    .is_some()
            );
            // Two bins keep the levels apart, so each volume fits.
            let bins = book.depth_distribution(side, 2).expect("two bins");
            assert_eq!(bins.iter().map(|b| b.level_count).sum::<usize>(), 2);
        }
    }

    #[test]
    fn test_market_impact_available_depth_overflow_is_reported() {
        // Impact scans the whole side for `total_quantity_available`.
        let book = book_with_u64_depth_overflow(Side::Sell);
        assert!(is_overflow(&book.market_impact(1, Side::Buy)));
        // VWAP / simulation stop once filled, so they never sum the tail.
        assert_eq!(book.vwap(1, Side::Buy).expect("fits"), Some(100.0));
        let sim = book.simulate_market_order(1, Side::Buy).expect("fits");
        assert_eq!(sim.total_filled, 1);
    }

    #[test]
    fn test_imbalance_bid_plus_ask_overflow_is_reported() {
        let book = OrderBook::<()>::new("OVF");
        book.add_limit_order(new_id(), 100, u64::MAX, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid");
        book.add_limit_order(new_id(), 101, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert!(is_overflow(&book.order_book_imbalance(1)));
        assert!(is_overflow(&book.micro_price()));
    }

    // ---- u128 notional aggregates -----------------------------------------

    #[test]
    fn test_u128_notional_product_overflow_is_reported() {
        let book = OrderBook::<()>::new("OVF");
        book.add_limit_order(new_id(), u128::MAX, 2, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask at u128::MAX");

        assert!(is_overflow(&book.vwap(2, Side::Buy)));
        assert!(is_overflow(&book.market_impact(2, Side::Buy)));
        assert!(is_overflow(&book.simulate_market_order(2, Side::Buy)));
        assert!(is_overflow(&book.depth_statistics(Side::Sell, 0)));

        // One unit at u128::MAX is exactly representable.
        assert_eq!(
            book.vwap(1, Side::Buy).expect("fits"),
            Some(u128::MAX as f64)
        );
        let sim = book.simulate_market_order(1, Side::Buy).expect("fits");
        assert_eq!(sim.total_cost().expect("fits"), u128::MAX);
    }

    #[test]
    fn test_u128_notional_sum_overflow_is_reported() {
        let book = OrderBook::<()>::new("OVF");
        book.add_limit_order(
            new_id(),
            u128::MAX - 1,
            1,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("ask");
        book.add_limit_order(new_id(), u128::MAX, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");

        // Each product fits; the running sum does not.
        assert!(is_overflow(&book.vwap(2, Side::Buy)));
        assert!(is_overflow(&book.market_impact(2, Side::Buy)));
        assert!(is_overflow(&book.simulate_market_order(2, Side::Buy)));
        assert!(is_overflow(&book.depth_statistics(Side::Sell, 0)));
        assert!(is_overflow(&book.depth_statistics(Side::Sell, 2)));
        // Restricting to the best level fits.
        let stats = book.depth_statistics(Side::Sell, 1).expect("one level");
        assert_eq!(stats.total_volume, 1);
        assert!(is_overflow(&book.enriched_snapshot(10)));
    }

    // ---- depth_distribution bounds --------------------------------------

    #[test]
    fn test_depth_distribution_huge_bins_are_capped() {
        let book = OrderBook::<()>::new("DIST");
        for price in 90..=100u128 {
            book.add_limit_order(new_id(), price, 10, Side::Buy, TimeInForce::Gtc, None)
                .expect("bid");
        }
        let bins = book
            .depth_distribution(Side::Buy, usize::MAX)
            .expect("capped request");
        assert_eq!(bins.len(), MAX_DEPTH_DISTRIBUTION_BINS);
        assert_eq!(bins.iter().map(|b| b.volume).sum::<u64>(), 110);
        assert_eq!(bins.iter().map(|b| b.level_count).sum::<usize>(), 11);

        let exact = book
            .depth_distribution(Side::Buy, MAX_DEPTH_DISTRIBUTION_BINS)
            .expect("at the cap");
        assert_eq!(exact, bins, "a request above the cap equals the cap");
    }

    #[test]
    fn test_depth_distribution_extreme_prices() {
        // A level at u128::MAX: the last bin's exclusive bound
        // `u128::MAX + 1` is not representable.
        let book = OrderBook::<()>::new("DIST");
        book.add_limit_order(new_id(), u128::MAX, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert!(is_overflow(&book.depth_distribution(Side::Sell, 1)));
        assert!(is_overflow(&book.depth_distribution(Side::Sell, 3)));

        // Just below the top of the domain everything fits.
        let book = OrderBook::<()>::new("DIST");
        book.add_limit_order(
            new_id(),
            u128::MAX - 1,
            4,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("ask");
        book.add_limit_order(new_id(), 1, 6, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        let bins = book.depth_distribution(Side::Sell, 2).expect("fits");
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].min_price, 1);
        assert_eq!(bins[1].max_price, u128::MAX);
        assert_eq!(bins.iter().map(|b| b.volume).sum::<u64>(), 10);
        for bin in &bins {
            assert!(bin.width().is_ok());
        }
    }

    // ---- integer midpoint -----------------------------------------------

    #[test]
    fn test_integer_mid_price_rounds_down_and_is_exact() {
        let book = OrderBook::<()>::new("MID");
        assert_eq!(book.integer_mid_price(), None);
        book.add_limit_order(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid");
        assert_eq!(book.integer_mid_price(), None, "one-sided book");
        book.add_limit_order(new_id(), 101, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert_eq!(book.integer_mid_price(), Some(100), "odd sum rounds down");

        let book = OrderBook::<()>::new("MID");
        book.add_limit_order(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid");
        book.add_limit_order(new_id(), 104, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert_eq!(book.integer_mid_price(), Some(102), "even sum is exact");

        // Above 2^53 the f64 mid loses precision; the integer mid does not.
        let base = 1u128 << 60;
        let book = OrderBook::<()>::new("MID");
        book.add_limit_order(new_id(), base + 1, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid");
        book.add_limit_order(new_id(), base + 3, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert_eq!(book.integer_mid_price(), Some(base + 2));
    }

    #[test]
    fn test_integer_mid_price_at_u128_max_does_not_overflow() {
        let book = OrderBook::<()>::new("MID");
        book.add_limit_order(
            new_id(),
            u128::MAX - 1,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        )
        .expect("bid");
        book.add_limit_order(new_id(), u128::MAX, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask");
        assert_eq!(book.integer_mid_price(), Some(u128::MAX - 1));
    }
}
