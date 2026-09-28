//! #230: an aggressive reserve's residual follows `auto_replenish`, exactly
//! as a resting maker's visible tranche does.
//!
//! The residual-resting helper behind `OrderQuantity::set_total_remaining`
//! used to refresh an emptied visible tranche from `replenish_amount` alone,
//! ignoring `auto_replenish`, falling back to zero without an explicit
//! amount, and never honouring `replenish_threshold`. `pricelevel` refreshes
//! a resting maker with `auto_replenish` on whenever the surviving visible
//! tranche falls below `max(replenish_threshold, 1)`, adding
//! `min(replenish_amount.unwrap_or(80), hidden)`; with the flag off it
//! removes the depleted maker and strands its hidden tranche.
//!
//! Both paths now obey the same rule:
//!
//! - `auto_replenish = true`: a visible tranche below the threshold (an
//!   emptied one always is) grows by `min(amount.unwrap_or(80), hidden)` and
//!   the residual rests.
//! - `auto_replenish = false`: nothing is transferred; an emptied visible
//!   tranche means the residual does **not** rest and its hidden remainder
//!   is discarded.
//!
//! Admission closes the matching hole for the one shape that cannot
//! execute: a **non-auto-replenishing** reserve submitted with a zero
//! visible tranche behind hidden quantity is rejected with
//! `OrderBookError::ZeroVisibleTranche`, on `add_order` and on every modify
//! projection the validator sees. An iceberg and an auto-replenishing
//! reserve in the same shape execute rather than vanishing, so both stay
//! admissible. Single-tranche kinds are out of scope, and so is a `(0, 0)`
//! order, which strands nothing. `UpdateQuantity` with a zero quantity is a
//! removal taken before the validator ever runs (#223), so the rule never
//! sees it.
//!
//! Every case asserts the accounting rule
//! `submitted = executed + resting (visible + hidden) + discarded` with
//! independently measured terms: `executed` is summed from the returned
//! `TradeResult`'s trades and cross-checked against the contra depth the
//! book actually lost and against the tracked status, the resting tranches
//! come from `get_order`, and the expected discard is a literal per case.
//! Discarded quantity is never counted as executed. The engine-side
//! measurement of discarded quantity lives in `tests/metrics/`, which is the
//! test binary that installs a `metrics` recorder.
//!
//! The execution asymmetry between the aggressive and the resting side is
//! documented here, not fixed: an aggressive two-tranche order sweeps with
//! its **total** quantity, so it can execute far past its visible tranche.
//!
//! The last section covers the validate-first consequence: because the three
//! cancel-then-add modify variants re-add the order as a taker, a re-price
//! that would exhaust a non-auto reserve's visible tranche is rejected with
//! `OrderBookError::ReserveResidualWouldBeDiscarded` **before** the original
//! is cancelled, so a re-price of such a reserve cannot destroy the order it
//! modifies: the gate is exclusive for the whole operation, so the dry run
//! is exact. That is the scope of the guarantee, not a claim about every
//! possible modification failure.

#[cfg(test)]
mod tests_reserve_residual_policy {
    use orderbook_rs::orderbook::order_state::{OrderStateTracker, OrderStatus};
    use orderbook_rs::{OrderBook, OrderBookError};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };
    use std::num::NonZeroU64;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    const PRICE: u128 = 100;
    /// The fixture taker's visible tranche.
    const VISIBLE: u64 = 10;
    /// The fixture taker's hidden tranche.
    const HIDDEN: u64 = 20;
    /// What the fixture taker submits: `VISIBLE + HIDDEN`.
    const SUBMITTED: u64 = VISIBLE + HIDDEN;

    /// A reserve BUY at `PRICE` with the fixture tranches and the given
    /// replenishment policy.
    fn reserve_buy(
        id: Id,
        threshold: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(VISIBLE),
            hidden_quantity: Quantity::new(HIDDEN),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(threshold),
            replenish_amount: replenish_amount.and_then(NonZeroU64::new),
            auto_replenish,
            extra_fields: (),
        }
    }

    /// An iceberg BUY at `price` with the given tranches.
    fn iceberg_buy(id: Id, price: u128, visible: u64, hidden: u64) -> OrderType<()> {
        OrderType::IcebergOrder {
            id,
            price: Price::new(price),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// A plain book (no lot size) with lifecycle tracking on.
    fn tracked_book(symbol: &str) -> OrderBook<()> {
        let mut book: OrderBook<()> = OrderBook::new(symbol);
        book.set_order_state_tracker(OrderStateTracker::new());
        book
    }

    /// Seed `depth` units of resting SELL depth at `price` as a single
    /// order, so the depth it still holds after a sweep can be read back.
    fn seed_contra(book: &OrderBook<()>, price: u128, depth: u64) -> Id {
        let contra_id = new_id();
        let seeded =
            book.add_limit_order(contra_id, price, depth, Side::Sell, TimeInForce::Gtc, None);
        assert!(
            seeded.is_ok(),
            "seeding {depth} units of contra depth must succeed: {seeded:?}"
        );
        contra_id
    }

    /// Contra depth still resting, in quantity units. Zero once the sweep
    /// has consumed the order entirely.
    fn remaining_contra(book: &OrderBook<()>, contra_id: Id) -> u64 {
        book.get_order(contra_id)
            .map(|order| order.visible_quantity().as_u64())
            .unwrap_or(0)
    }

    /// Everything one aggressive submit can be measured on, each term read
    /// from an independent source so the accounting assertion is falsifiable.
    struct Submitted {
        /// Summed from the returned `TradeResult`'s trades.
        executed: u64,
        /// Contra depth the book lost, read off the contra order.
        contra_consumed: u64,
        /// Filled quantity the lifecycle tracker recorded.
        tracked_filled: u64,
        /// Whether the tracked status is terminal (`Filled`).
        tracked_terminal: bool,
        /// Resting tranches, or `(0, 0)` when nothing rests.
        resting: (u64, u64),
        /// Tranches of the `OrderType` handle `add_order_with_result`
        /// returned.
        returned: (u64, u64),
        /// Total the returned handle reports.
        returned_total: u64,
    }

    /// Submit a reserve taker against `depth` units of contra depth and
    /// measure the outcome.
    fn submit_reserve_taker(
        book: &OrderBook<()>,
        depth: u64,
        threshold: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> Submitted {
        let contra_id = seed_contra(book, PRICE, depth);
        let taker_id = new_id();
        let submitted = book.add_order_with_result(reserve_buy(
            taker_id,
            threshold,
            replenish_amount,
            auto_replenish,
        ));

        let (returned, executed) = match submitted {
            Ok((order, Some(trade))) => {
                // Sum the trade prints themselves rather than reading
                // `executed_quantity`, so the measurement does not share a
                // source with the status or the resting tranches.
                let executed: u64 = trade
                    .match_result
                    .trades()
                    .as_vec()
                    .iter()
                    .map(|print| print.quantity().as_u64())
                    .sum();
                (order, executed)
            }
            other => panic!("expected a trade from the aggressive reserve, got {other:?}"),
        };

        let (tracked_filled, tracked_terminal) = match book.order_status(taker_id) {
            Some(OrderStatus::Filled { filled_quantity }) => (filled_quantity, true),
            Some(OrderStatus::PartiallyFilled {
                filled_quantity, ..
            }) => (filled_quantity, false),
            other => panic!("expected a filled or partially-filled status, got {other:?}"),
        };

        Submitted {
            executed,
            contra_consumed: depth - remaining_contra(book, contra_id),
            tracked_filled,
            tracked_terminal,
            resting: book
                .get_order(taker_id)
                .map(|order| {
                    (
                        order.visible_quantity().as_u64(),
                        order.hidden_quantity().as_u64(),
                    )
                })
                .unwrap_or((0, 0)),
            returned: (
                returned.visible_quantity().as_u64(),
                returned.hidden_quantity().as_u64(),
            ),
            returned_total: returned.visible_quantity().as_u64()
                + returned.hidden_quantity().as_u64(),
        }
    }

    /// Cross-check the three independent measurements of executed quantity
    /// and close the accounting rule against a per-case expected discard.
    ///
    /// `expected_discarded` is a literal, never derived from the other
    /// terms, so the identity below can actually fail.
    fn assert_accounted(measured: &Submitted, expected_executed: u64, expected_discarded: u64) {
        assert_eq!(
            measured.executed, expected_executed,
            "executed quantity, summed from the emitted trades"
        );
        assert_eq!(
            measured.contra_consumed, measured.executed,
            "the contra side must lose exactly what the taker executed"
        );
        assert_eq!(
            measured.tracked_filled, measured.executed,
            "the tracked filled quantity must equal the executed quantity"
        );
        let (visible, hidden) = measured.resting;
        assert_eq!(
            measured.executed + visible + hidden + expected_discarded,
            SUBMITTED,
            "accounting: executed {} + visible {visible} + hidden {hidden} + discarded \
             {expected_discarded} != submitted {SUBMITTED}",
            measured.executed
        );
    }

    // ----------------------------------------------------------------
    // 1. `auto_replenish = false`: the residual ends when its visible
    //    tranche is exhausted
    // ----------------------------------------------------------------

    /// The visible tranche survives the fill (5 of 10 consumed), so there is
    /// nothing to refresh and the residual rests as 5 / 20: the same outcome
    /// before and after #230.
    #[test]
    fn test_add_order_reserve_residual_without_auto_partial_visible_rests_unrefreshed() {
        let book = tracked_book("RSV-NOAUTO-5");
        let measured = submit_reserve_taker(&book, 5, 0, None, false);

        assert_accounted(&measured, 5, 0);
        assert_eq!(measured.resting, (5, 20), "the residual rests unrefreshed");
        assert_eq!(
            measured.returned,
            (5, 20),
            "a resting submit returns the resting residual"
        );
        assert!(
            !measured.tracked_terminal,
            "a resting partially-filled taker is not terminal"
        );
        assert_eq!(book.best_bid(), Some(PRICE), "the residual rests as a bid");
    }

    /// The fill empties the visible tranche exactly. Without automatic
    /// replenishment nothing is drawn from hidden, so the residual has no
    /// displayable quantity and the order ends, mirroring `pricelevel`'s
    /// removal of a depleted non-auto maker. The 20 hidden units are
    /// discarded and never counted as executed.
    #[test]
    fn test_add_order_reserve_residual_without_auto_exhausted_visible_ends_order() {
        let book = tracked_book("RSV-NOAUTO-10");
        let measured = submit_reserve_taker(&book, 10, 0, None, false);

        assert_accounted(&measured, 10, 20);
        assert_eq!(measured.resting, (0, 0), "nothing rests");
        assert!(measured.tracked_terminal, "the ended order is terminal");
        assert_eq!(
            measured.returned,
            (0, 0),
            "a discarded order is returned holding nothing"
        );
        assert_eq!(
            measured.returned_total, 0,
            "the returned handle must not report the discarded hidden tranche"
        );
        assert!(book.best_bid().is_none(), "no bid level remains");
        assert!(
            book.best_ask().is_none(),
            "the contra depth is fully consumed"
        );
    }

    /// An explicit `replenish_amount` is dead configuration without
    /// automatic replenishment: the outcome is identical to the `None` case
    /// above. Before #230 this order rested as 10 / 10.
    #[test]
    fn test_add_order_reserve_residual_without_auto_explicit_amount_ends_order() {
        let book = tracked_book("RSV-NOAUTO-AMT");
        let measured = submit_reserve_taker(&book, 10, 0, Some(10), false);

        assert_accounted(&measured, 10, 20);
        assert_eq!(
            measured.resting,
            (0, 0),
            "the explicit amount no longer refreshes a non-auto residual"
        );
        assert!(measured.tracked_terminal, "the ended order is terminal");
        assert_eq!(measured.returned_total, 0, "returned holding nothing");
        assert!(book.best_bid().is_none(), "no bid level remains");
    }

    /// The taker sweeps with its **total** of 30, so it executes 15, well
    /// past its visible tranche of 10, drawing the extra 5 from hidden. The
    /// remaining 15 hidden units have no visible tranche to sit behind and
    /// are discarded.
    #[test]
    fn test_add_order_reserve_residual_without_auto_sweeps_past_visible_ends_order() {
        let book = tracked_book("RSV-NOAUTO-15");
        let measured = submit_reserve_taker(&book, 15, 0, None, false);

        assert_accounted(&measured, 15, 15);
        assert_eq!(measured.resting, (0, 0), "nothing rests");
        assert!(measured.tracked_terminal, "the ended order is terminal");
        assert_eq!(measured.returned_total, 0, "returned holding nothing");
        assert!(book.best_bid().is_none(), "no bid level remains");
    }

    /// Enough contra depth to match the whole submitted total: the fully
    /// matched branch runs, nothing is left to rest and nothing is dropped.
    #[test]
    fn test_add_order_reserve_taker_fully_matched_records_submitted_total() {
        let book = tracked_book("RSV-NOAUTO-30");
        let measured = submit_reserve_taker(&book, SUBMITTED, 0, None, false);

        assert_accounted(&measured, SUBMITTED, 0);
        assert_eq!(measured.resting, (0, 0), "a filled taker rests nothing");
        assert!(measured.tracked_terminal, "a filled order is terminal");
        assert_eq!(
            measured.returned,
            (VISIBLE, HIDDEN),
            "a fully matched submit returns the order as submitted"
        );
        assert!(book.best_bid().is_none(), "no bid level remains");
        assert!(
            book.best_ask().is_none(),
            "the contra depth is fully consumed"
        );
    }

    // ----------------------------------------------------------------
    // 2. `auto_replenish = true`: the residual is refreshed and rests
    // ----------------------------------------------------------------

    /// Without an explicit amount the refresh uses `pricelevel`'s default
    /// capped by hidden: `min(80, 20) == 20`, so the residual rests fully
    /// visible as 20 / 0, displaying twice what the order first showed.
    #[test]
    fn test_add_order_reserve_residual_with_auto_default_amount_rests_refreshed() {
        let book = tracked_book("RSV-AUTO-10");
        let measured = submit_reserve_taker(&book, 10, 0, None, true);

        assert_accounted(&measured, 10, 0);
        assert_eq!(
            measured.resting,
            (20, 0),
            "the emptied visible tranche grows by min(80, 20)"
        );
        assert_eq!(
            measured.returned,
            (20, 0),
            "the resting residual is returned"
        );
        assert!(
            !measured.tracked_terminal,
            "a resting residual is not terminal"
        );
        assert_eq!(book.best_bid(), Some(PRICE), "the residual rests as a bid");
    }

    /// A fill that leaves the visible tranche at or above the threshold
    /// never refreshes. With the threshold at zero the engine reads it as 1,
    /// so a surviving tranche of 5 is above it.
    #[test]
    fn test_add_order_reserve_residual_with_auto_partial_visible_rests_unrefreshed() {
        let book = tracked_book("RSV-AUTO-5");
        let measured = submit_reserve_taker(&book, 5, 0, None, true);

        assert_accounted(&measured, 5, 0);
        assert_eq!(
            measured.resting,
            (5, 20),
            "a surviving visible tranche above the threshold is not refreshed"
        );
    }

    /// An explicit amount is honoured over the default: `min(10, 20) == 10`
    /// moves from hidden, so the residual of 20 rests as 10 / 10.
    #[test]
    fn test_add_order_reserve_residual_with_auto_explicit_amount_rests_refreshed() {
        let book = tracked_book("RSV-AUTO-AMT-10");
        let measured = submit_reserve_taker(&book, 10, 0, Some(10), true);

        assert_accounted(&measured, 10, 0);
        assert_eq!(
            measured.resting,
            (10, 10),
            "the residual is refreshed with the explicit amount"
        );
    }

    /// The same explicit amount after a deeper sweep: 15 executed leaves a
    /// residual of 15, all of it hidden, and `min(10, 15) == 10` moves into
    /// the visible tranche.
    #[test]
    fn test_add_order_reserve_residual_with_auto_explicit_amount_after_deep_sweep_rests_refreshed()
    {
        let book = tracked_book("RSV-AUTO-AMT-15");
        let measured = submit_reserve_taker(&book, 15, 0, Some(10), true);

        assert_accounted(&measured, 15, 0);
        assert_eq!(
            measured.resting,
            (10, 5),
            "min(10, 15) refreshes the visible tranche out of the hidden residual"
        );
    }

    // ----------------------------------------------------------------
    // 3. `replenish_threshold`: the below-threshold refresh arm
    // ----------------------------------------------------------------

    /// A fill of 8 leaves 2 visible out of 10, below the threshold of 5, so
    /// the below-threshold arm fires even though the tranche is not empty:
    /// `min(replenish_amount = 10, hidden = 20) == 10` moves across, giving
    /// `2 + 10 = 12` visible and `20 - 10 = 10` hidden. Before #230 the
    /// helper only ever refreshed an emptied tranche, so this residual
    /// rested 2 / 20 while the identical resting maker would have shown
    /// 12 / 10.
    #[test]
    fn test_add_order_reserve_residual_below_threshold_rests_refreshed() {
        let book = tracked_book("RSV-THRESHOLD-8");
        let measured = submit_reserve_taker(&book, 8, 5, Some(10), true);

        assert_accounted(&measured, 8, 0);
        assert_eq!(
            measured.resting,
            (12, 10),
            "a surviving tranche below the threshold grows by min(10, 20)"
        );
        assert_eq!(
            measured.returned,
            (12, 10),
            "the resting residual is returned"
        );
        assert_eq!(book.best_bid(), Some(PRICE), "the residual rests as a bid");
    }

    /// The same 8-unit fill without automatic replenishment: the threshold
    /// is never consulted, nothing moves, and the residual rests 2 / 20. The
    /// discard guard does not fire because the visible tranche is positive.
    #[test]
    fn test_add_order_reserve_residual_below_threshold_without_auto_rests_unrefreshed() {
        let book = tracked_book("RSV-THRESHOLD-NOAUTO");
        let measured = submit_reserve_taker(&book, 8, 5, Some(10), false);

        assert_accounted(&measured, 8, 0);
        assert_eq!(
            measured.resting,
            (2, 20),
            "without automatic replenishment nothing is transferred"
        );
        assert!(
            !measured.tracked_terminal,
            "a positive visible tranche keeps the residual resting"
        );
    }

    /// A fill of 3 leaves 7 visible, at or above the threshold of 5, so the
    /// arm does not fire and hidden is untouched.
    #[test]
    fn test_add_order_reserve_residual_at_or_above_threshold_rests_unrefreshed() {
        let book = tracked_book("RSV-THRESHOLD-3");
        let measured = submit_reserve_taker(&book, 3, 5, Some(10), true);

        assert_accounted(&measured, 3, 0);
        assert_eq!(
            measured.resting,
            (7, 20),
            "a tranche at or above the threshold is not refreshed"
        );
    }

    // ----------------------------------------------------------------
    // 4. The execution asymmetry #230 documents but does not change
    // ----------------------------------------------------------------

    /// The same `{10 visible, 20 hidden, no amount, no auto-replenishment}`
    /// reserve reaches two different executed quantities against the same 20
    /// units of contra liquidity, depending on which side of the trade it is:
    ///
    /// - **Aggressive**: `add_order` sweeps with the order's TOTAL (30), so
    ///   it executes all 20 available units and then ends; the residual of
    ///   10 has no visible tranche and is discarded.
    /// - **Resting**: `pricelevel` only ever exposes the maker's visible
    ///   tranche, so an aggressive SELL of 20 executes just 10 against it,
    ///   removes the depleted maker and strands its 20 hidden units. The
    ///   crossing sell rests its own remainder of 10.
    ///
    /// #230 did not touch this: it only made the aggressive side's residual
    /// follow `auto_replenish` the way the resting side already did.
    #[test]
    fn test_add_order_reserve_aggressive_and_resting_execution_differ() {
        // Aggressive: 20 units of contra depth, swept with the total of 30.
        let aggressive_book = tracked_book("RSV-AGGRESSIVE");
        let measured = submit_reserve_taker(&aggressive_book, 20, 0, None, false);

        assert_accounted(&measured, 20, 10);
        assert_eq!(measured.resting, (0, 0), "nothing rests");
        assert!(measured.tracked_terminal, "the ended order is terminal");
        assert!(aggressive_book.best_bid().is_none(), "no bid level remains");

        // Resting: the identical order as a maker, hit by an aggressive SELL
        // of the same 20 units.
        let maker_book = tracked_book("RSV-RESTING");
        let maker_id = new_id();
        let rested = maker_book.add_order(reserve_buy(maker_id, 0, None, false));
        assert!(rested.is_ok(), "seeding the maker must succeed: {rested:?}");

        let swept = maker_book.add_limit_order_with_result(
            new_id(),
            PRICE,
            20,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let maker_executed: u64 = match swept {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade from the crossing sell, got {other:?}"),
        };

        assert_eq!(
            maker_executed, 10,
            "a resting reserve only ever exposes its visible tranche"
        );
        assert!(
            maker_book.get_order(maker_id).is_none(),
            "the depleted non-replenishing maker leaves the book"
        );
        assert_eq!(
            maker_book.order_status(maker_id),
            Some(OrderStatus::Filled {
                filled_quantity: 10
            }),
            "the removed maker records only its executed visible tranche"
        );
        assert!(
            maker_book.best_bid().is_none(),
            "the emptied bid level is removed with the maker"
        );
        assert_eq!(
            maker_book.best_ask(),
            Some(PRICE),
            "the crossing sell rests its unfilled remainder of 10"
        );

        // The headline numbers, side by side: 20 executed aggressively,
        // 10 executed while resting, against the same contra quantity.
        assert!(
            measured.executed > maker_executed,
            "the aggressive side executes more than the resting side: {} vs {maker_executed}",
            measured.executed
        );
    }

    // ----------------------------------------------------------------
    // 5. A two-tranche order must display a positive visible tranche
    // ----------------------------------------------------------------

    /// Assert the operation failed with `ZeroVisibleTranche` on `hidden`.
    fn assert_zero_visible<V: std::fmt::Debug>(
        result: Result<V, OrderBookError>,
        expected_id: Id,
        expected_hidden: u64,
        context: &str,
    ) {
        match result {
            Err(OrderBookError::ZeroVisibleTranche {
                order_id,
                hidden_quantity,
            }) => {
                assert_eq!(order_id, expected_id, "{context}: order id reported");
                assert_eq!(
                    hidden_quantity, expected_hidden,
                    "{context}: hidden tranche reported"
                );
            }
            other => panic!("{context}: expected ZeroVisibleTranche, got {other:?}"),
        }
    }

    /// A reserve submitted with no visible tranche is a ghost: it would show
    /// nothing on its level and be removed with its hidden tranche stranded
    /// by the first taker to reach it. Rejected at admission.
    #[test]
    fn test_add_order_reserve_zero_visible_tranche_rejects() {
        let book = tracked_book("ZERO-VIS-RSV");
        let order_id = new_id();
        let mut order = reserve_buy(order_id, 0, None, false);
        if let OrderType::ReserveOrder {
            visible_quantity, ..
        } = &mut order
        {
            *visible_quantity = Quantity::new(0);
        }

        assert_zero_visible(book.add_order(order), order_id, HIDDEN, "reserve admission");
        assert!(book.get_order(order_id).is_none(), "a ghost must not rest");
        assert!(book.best_bid().is_none(), "no level may be created");
    }

    /// The identical **iceberg** shape is admitted, because it is not a
    /// ghost: `pricelevel`'s degenerate guard draws the whole hidden tranche
    /// into visible on match, so the order executes instead of vanishing.
    /// Asserted by hitting it: a sell of 20 fills all 20.
    #[test]
    fn test_add_order_iceberg_zero_visible_tranche_admits_and_executes() {
        let book = tracked_book("ZERO-VIS-ICE");
        let order_id = new_id();

        let rested = book.add_order(iceberg_buy(order_id, PRICE, 0, HIDDEN));
        assert!(
            rested.is_ok(),
            "a zero-visible iceberg is executable, not a ghost: {rested:?}"
        );

        let swept = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            HIDDEN,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let executed: u64 = match swept {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade against the iceberg, got {other:?}"),
        };
        assert_eq!(
            executed, HIDDEN,
            "the hidden tranche is drawn into visible and executes in full"
        );
    }

    /// An **auto-replenishing** reserve with no visible tranche is admitted
    /// too: it refreshes `min(amount_or_default, hidden)` and re-queues, so
    /// it executes rather than being removed with its hidden stranded.
    #[test]
    fn test_add_order_auto_reserve_zero_visible_tranche_admits_and_executes() {
        let book = tracked_book("ZERO-VIS-AUTO");
        let order_id = new_id();
        let mut order = reserve_buy(order_id, 0, Some(10), true);
        if let OrderType::ReserveOrder {
            visible_quantity, ..
        } = &mut order
        {
            *visible_quantity = Quantity::new(0);
        }

        let rested = book.add_order(order);
        assert!(
            rested.is_ok(),
            "a zero-visible auto reserve is executable: {rested:?}"
        );

        let swept = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let executed: u64 = match swept {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade against the auto reserve, got {other:?}"),
        };
        assert_eq!(
            executed, 10,
            "the refresh makes the tranche matchable and it executes"
        );
    }

    /// The zero-visible shapes as **aggressive takers** that partially fill
    /// and rest. Both are admitted, so `set_total_remaining` has to produce
    /// something sane for them; these assert what it actually produces.
    #[test]
    fn test_add_order_zero_visible_iceberg_taker_partial_fill_rests_display_zero() {
        let book = tracked_book("ZERO-VIS-ICE-TAKER");
        let contra_id = seed_contra(&book, PRICE, 5);
        let taker_id = new_id();

        let submitted = book.add_order_with_result(iceberg_buy(taker_id, PRICE, 0, HIDDEN));
        let executed: u64 = match submitted {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade from the aggressive iceberg, got {other:?}"),
        };
        assert_eq!(
            executed, 5,
            "the sweep uses the total, not the display size"
        );
        assert_eq!(remaining_contra(&book, contra_id), 0, "contra consumed");

        // `set_total_remaining` keeps the submitted display size, which is 0
        // here, so the whole residual stays hidden. That is not a ghost: the
        // upstream degenerate guard draws it into visible when a taker hits
        // the level, which the next assertion exercises.
        match book.get_order(taker_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (0, 15),
                "display 0 leaves the residual entirely hidden"
            ),
            None => panic!("the iceberg residual must rest"),
        }

        let swept = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            15,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let drained: u64 = match swept {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade against the rested iceberg, got {other:?}"),
        };
        assert_eq!(
            drained, 15,
            "the rested residual is matchable: hidden is drawn into visible"
        );
    }

    /// The auto-replenishing reserve with no visible tranche, crossing 5 of
    /// its 30: the reduction leaves 0 visible, the threshold arm fires and
    /// `min(10, hidden)` refreshes it, so the residual rests displayable.
    #[test]
    fn test_add_order_zero_visible_auto_reserve_taker_partial_fill_rests_refreshed() {
        let book = tracked_book("ZERO-VIS-AUTO-TAKER");
        let contra_id = seed_contra(&book, PRICE, 5);
        let taker_id = new_id();
        let mut order = reserve_buy(taker_id, 0, Some(10), true);
        if let OrderType::ReserveOrder {
            visible_quantity, ..
        } = &mut order
        {
            *visible_quantity = Quantity::new(0);
        }

        let submitted = book.add_order_with_result(order);
        let executed: u64 = match submitted {
            Ok((_, Some(trade))) => trade
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum(),
            other => panic!("expected a trade from the aggressive reserve, got {other:?}"),
        };
        assert_eq!(executed, 5, "the sweep uses the total of 20");
        assert_eq!(remaining_contra(&book, contra_id), 0, "contra consumed");

        match book.get_order(taker_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (10, 5),
                "the emptied tranche is refreshed with min(10, 15)"
            ),
            None => panic!("the auto reserve residual must rest"),
        }
    }

    /// Single-tranche kinds are outside the rule: a standard order carries
    /// no hidden quantity to strand, so a zero quantity is not this error's
    /// business.
    #[test]
    fn test_add_order_standard_zero_quantity_not_rejected_by_zero_visible_rule() {
        let book = tracked_book("ZERO-VIS-STD");
        let order_id = new_id();

        let result = book.add_limit_order(order_id, PRICE, 0, Side::Buy, TimeInForce::Gtc, None);

        assert!(
            !matches!(result, Err(OrderBookError::ZeroVisibleTranche { .. })),
            "the two-tranche rule must not reach single-tranche kinds: {result:?}"
        );
    }

    /// The two cancel-then-add arms project the updated order through the
    /// same validator, and since #221 the quantity sets the **visible**
    /// tranche, so a zero would drive a healthy resting reserve into the
    /// ghost shape. Both reject it and leave the original untouched. The
    /// third quantity-carrying arm, `UpdateQuantity`, removes the order
    /// instead (#223) and is exercised at the end.
    #[test]
    fn test_update_order_non_auto_reserve_zero_quantity_rejects_and_preserves_original() {
        {
            let kind = "reserve";
            let book = tracked_book("ZERO-VIS-MODIFY");
            let order_id = new_id();
            let rested = book.add_order(reserve_buy(order_id, 0, None, false));
            assert!(rested.is_ok(), "{kind}: seeding must succeed: {rested:?}");

            let updates = [
                (
                    "UpdatePriceAndQuantity",
                    OrderUpdate::UpdatePriceAndQuantity {
                        order_id,
                        new_price: Price::new(PRICE - 1),
                        new_quantity: Quantity::new(0),
                    },
                ),
                (
                    "Replace",
                    OrderUpdate::Replace {
                        order_id,
                        price: Price::new(PRICE - 1),
                        quantity: Quantity::new(0),
                        side: Side::Buy,
                    },
                ),
            ];

            for (label, update) in updates {
                assert_zero_visible(
                    book.update_order(update),
                    order_id,
                    HIDDEN,
                    &format!("{kind} / {label}"),
                );
                match book.get_order(order_id) {
                    Some(order) => {
                        assert_eq!(
                            order.price().as_u128(),
                            PRICE,
                            "{kind} / {label}: price unchanged"
                        );
                        assert_eq!(
                            (
                                order.visible_quantity().as_u64(),
                                order.hidden_quantity().as_u64()
                            ),
                            (VISIBLE, HIDDEN),
                            "{kind} / {label}: tranches unchanged"
                        );
                    }
                    None => panic!("{kind} / {label}: a rejected modify must not cancel"),
                }
                assert_eq!(
                    book.best_bid(),
                    Some(PRICE),
                    "{kind} / {label}: the level must survive"
                );
            }

            // `UpdateQuantity` with a zero quantity is a removal (#223): it
            // runs before the projected-shape validator, so the
            // `ZeroVisibleTranche` rule never sees it and the whole order
            // is cancelled, hidden depth included.
            let removed = book
                .update_order(OrderUpdate::UpdateQuantity {
                    order_id,
                    new_quantity: Quantity::new(0),
                })
                .unwrap_or_else(|e| panic!("{kind} / UpdateQuantity: zero is a removal: {e:?}"))
                .unwrap_or_else(|| {
                    panic!("{kind} / UpdateQuantity: the cancelled order is returned")
                });
            assert_eq!(removed.id(), order_id);
            assert!(
                book.get_order(order_id).is_none(),
                "{kind} / UpdateQuantity: the whole order is cancelled"
            );
            assert_eq!(
                book.best_bid(),
                None,
                "{kind} / UpdateQuantity: the level goes with it"
            );
        }
    }

    /// The iceberg arm of the cancel-then-add variants keeps its pre-#230
    /// outcome: a zero quantity is **accepted**, leaving 0 visible / 20
    /// hidden, because that shape executes rather than vanishing (the hidden
    /// tranche is drawn into visible on match). The rule narrowed to the
    /// non-auto reserve alone. (`UpdateQuantity` with zero is a removal on
    /// every kind since #223, so it is exercised through
    /// `UpdatePriceAndQuantity` here.)
    #[test]
    fn test_update_order_iceberg_zero_quantity_accepts_and_leaves_hidden_intact() {
        let book = tracked_book("ZERO-VIS-MODIFY-ICE");
        let order_id = new_id();
        let rested = book.add_order(iceberg_buy(order_id, PRICE, VISIBLE, HIDDEN));
        assert!(rested.is_ok(), "seeding must succeed: {rested:?}");

        let updated = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(PRICE),
            new_quantity: Quantity::new(0),
        });
        assert!(
            updated.is_ok(),
            "a zero-quantity iceberg update stays accepted: {updated:?}"
        );

        match book.get_order(order_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (0, HIDDEN),
                "the visible tranche is emptied and hidden is untouched"
            ),
            None => panic!("the iceberg must still rest"),
        }
    }

    // ----------------------------------------------------------------
    // 6. Validate-first: a modify may not destroy the order it modifies
    // ----------------------------------------------------------------

    /// Price the fixture maker rests at, below the market.
    const REST_PRICE: u128 = 100;
    /// Price the modify re-prices it to, crossing the ask side.
    const CROSS_PRICE: u128 = 110;

    /// The three cancel-then-add variants, each projecting the same
    /// `10 visible / 20 hidden` order onto `CROSS_PRICE`. The quantity
    /// argument sets the **visible** tranche for the two-tranche kinds
    /// (#221), so passing `VISIBLE` leaves the projected shape unchanged.
    fn crossing_updates(order_id: Id) -> [(&'static str, OrderUpdate); 3] {
        [
            (
                "UpdatePrice",
                OrderUpdate::UpdatePrice {
                    order_id,
                    new_price: Price::new(CROSS_PRICE),
                },
            ),
            (
                "UpdatePriceAndQuantity",
                OrderUpdate::UpdatePriceAndQuantity {
                    order_id,
                    new_price: Price::new(CROSS_PRICE),
                    new_quantity: Quantity::new(VISIBLE),
                },
            ),
            (
                "Replace",
                OrderUpdate::Replace {
                    order_id,
                    price: Price::new(CROSS_PRICE),
                    quantity: Quantity::new(VISIBLE),
                    side: Side::Buy,
                },
            ),
        ]
    }

    /// A book holding `ask_depth` units of SELL depth at `CROSS_PRICE` and
    /// the given BUY maker resting at `REST_PRICE`. Returns the contra id.
    fn book_with_maker(symbol: &str, ask_depth: u64, maker: OrderType<()>) -> (OrderBook<()>, Id) {
        let book = tracked_book(symbol);
        let contra_id = seed_contra(&book, CROSS_PRICE, ask_depth);
        let rested = book.add_order(maker);
        assert!(rested.is_ok(), "seeding the maker must succeed: {rested:?}");
        (book, contra_id)
    }

    /// The order id carried by a crossing update.
    fn update_order_id(update: &OrderUpdate) -> Id {
        match update {
            OrderUpdate::UpdatePrice { order_id, .. }
            | OrderUpdate::UpdatePriceAndQuantity { order_id, .. }
            | OrderUpdate::Replace { order_id, .. } => *order_id,
            other => panic!("unexpected update variant: {other:?}"),
        }
    }

    /// The maker is still resting untouched at its original price and shape,
    /// and the contra depth was never swept.
    fn assert_modify_had_no_effect(book: &OrderBook<()>, maker_id: Id, contra_id: Id, depth: u64) {
        match book.get_order(maker_id) {
            Some(order) => {
                assert_eq!(
                    order.price().as_u128(),
                    REST_PRICE,
                    "a rejected modify must leave the original at its old price"
                );
                assert_eq!(order.visible_quantity().as_u64(), VISIBLE, "visible intact");
                assert_eq!(order.hidden_quantity().as_u64(), HIDDEN, "hidden intact");
            }
            None => panic!("a rejected modify must never cancel the original"),
        }
        assert_eq!(
            book.best_bid(),
            Some(REST_PRICE),
            "the original's level must survive"
        );
        assert_eq!(
            remaining_contra(book, contra_id),
            depth,
            "the contra depth must be untouched: validation precedes matching"
        );
        assert!(
            book.order_status(maker_id).is_none()
                || book.order_status(maker_id) == Some(OrderStatus::Open),
            "a rejected modify records no terminal transition for the original"
        );
    }

    /// A non-auto-replenishing reserve re-priced into depth that would
    /// exhaust its visible tranche is rejected before anything is cancelled:
    /// the re-add's residual would not rest and the hidden remainder would be
    /// discarded, silently destroying the order the caller asked to modify.
    /// All three cancel-then-add variants take the same pre-check.
    #[test]
    fn test_update_order_reserve_without_auto_crossing_visible_depth_rejects_and_preserves_original()
     {
        for depth in [10, 15] {
            for (label, update) in crossing_updates(new_id()) {
                let maker_id = update_order_id(&update);
                let (book, contra_id) = book_with_maker(
                    "RSV-MODIFY-REJECT",
                    depth,
                    reserve_buy(maker_id, 0, None, false),
                );

                match book.update_order(update) {
                    Err(OrderBookError::ReserveResidualWouldBeDiscarded {
                        order_id,
                        visible_quantity,
                        crossable_quantity,
                        hidden_quantity,
                        discarded_quantity,
                    }) => {
                        assert_eq!(order_id, maker_id, "{label}/{depth}: order id reported");
                        assert_eq!(
                            visible_quantity, VISIBLE,
                            "{label}/{depth}: projected visible tranche reported"
                        );
                        assert_eq!(
                            crossable_quantity, depth,
                            "{label}/{depth}: crossable depth reported"
                        );
                        assert_eq!(
                            hidden_quantity, HIDDEN,
                            "{label}/{depth}: projected hidden tranche reported"
                        );
                        // What would actually be destroyed: the residual the
                        // re-add would leave unmatched, `total - crossable`.
                        // Depth 10 abandons all 20 hidden; depth 15 draws 5
                        // out of hidden first and abandons 15.
                        assert_eq!(
                            discarded_quantity,
                            SUBMITTED - depth,
                            "{label}/{depth}: destroyed quantity reported"
                        );
                    }
                    other => panic!(
                        "{label}/{depth}: expected ReserveResidualWouldBeDiscarded, got {other:?}"
                    ),
                }

                assert_modify_had_no_effect(&book, maker_id, contra_id, depth);
            }
        }
    }

    /// Crossing into depth *smaller* than the visible tranche is allowed:
    /// the sweep leaves a positive visible tranche, so the residual rests
    /// normally and nothing is discarded.
    #[test]
    fn test_update_order_reserve_without_auto_shallow_crossing_depth_executes_and_rests() {
        for (label, update) in crossing_updates(new_id()) {
            let maker_id = update_order_id(&update);
            let (book, _contra_id) = book_with_maker(
                "RSV-MODIFY-SHALLOW",
                5,
                reserve_buy(maker_id, 0, None, false),
            );

            let result = book.update_order(update);
            assert!(
                result.is_ok(),
                "{label}: shallow re-price rejected: {result:?}"
            );

            match book.get_order(maker_id) {
                Some(order) => {
                    assert_eq!(
                        order.price().as_u128(),
                        CROSS_PRICE,
                        "{label}: the residual rests at the new price"
                    );
                    assert_eq!(
                        (
                            order.visible_quantity().as_u64(),
                            order.hidden_quantity().as_u64()
                        ),
                        (5, HIDDEN),
                        "{label}: 5 executed out of the visible tranche, hidden untouched"
                    );
                }
                None => panic!("{label}: the re-priced order must rest"),
            }
            assert_eq!(
                book.best_bid(),
                Some(CROSS_PRICE),
                "{label}: the residual rests as a bid at the new price"
            );
            assert!(
                book.best_ask().is_none(),
                "{label}: the ask depth is consumed"
            );
        }
    }

    /// A projected **full** fill is allowed through: the sweep consumes the
    /// whole order, so nothing is discarded and the pre-check must not
    /// refuse it. This case is outside the rejection band
    /// `visible <= crossable < total`.
    #[test]
    fn test_update_order_reserve_without_auto_full_crossing_depth_fills_completely() {
        for (label, update) in crossing_updates(new_id()) {
            let maker_id = update_order_id(&update);
            let (book, _contra_id) = book_with_maker(
                "RSV-MODIFY-FULL",
                SUBMITTED,
                reserve_buy(maker_id, 0, None, false),
            );

            let result = book.update_order(update);
            assert!(
                result.is_ok(),
                "{label}: full-fill re-price rejected: {result:?}"
            );

            assert!(
                book.get_order(maker_id).is_none(),
                "{label}: a fully filled order rests nothing"
            );
            assert_eq!(
                book.order_status(maker_id),
                Some(OrderStatus::Filled {
                    filled_quantity: SUBMITTED
                }),
                "{label}: the whole order executed, nothing discarded"
            );
            assert!(book.best_bid().is_none(), "{label}: no bid level remains");
            assert!(
                book.best_ask().is_none(),
                "{label}: the ask depth is consumed"
            );
        }
    }

    /// A re-price that crosses nothing is always allowed: no fill means the
    /// tranches are never rewritten, the residual guard never fires, and the
    /// order simply rests at its new price.
    #[test]
    fn test_update_order_reserve_without_auto_non_crossing_reprice_rests_unchanged() {
        let maker_id = new_id();
        // Ask depth sits at CROSS_PRICE; re-pricing the bid down crosses
        // nothing.
        let (book, contra_id) = book_with_maker(
            "RSV-MODIFY-NOCROSS",
            10,
            reserve_buy(maker_id, 0, None, false),
        );
        let new_price = REST_PRICE - 1;

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: maker_id,
            new_price: Price::new(new_price),
        });
        assert!(
            result.is_ok(),
            "a non-crossing re-price must be allowed: {result:?}"
        );

        match book.get_order(maker_id) {
            Some(order) => {
                assert_eq!(order.price().as_u128(), new_price, "re-priced");
                assert_eq!(
                    (
                        order.visible_quantity().as_u64(),
                        order.hidden_quantity().as_u64()
                    ),
                    (VISIBLE, HIDDEN),
                    "no fill means no tranche rewrite"
                );
            }
            None => panic!("a non-crossing re-price must leave the order resting"),
        }
        assert_eq!(
            remaining_contra(&book, contra_id),
            10,
            "the ask depth is untouched"
        );
    }

    /// An auto-replenishing reserve is never touched by the pre-check: its
    /// residual refreshes from hidden and rests, so the modify is safe.
    #[test]
    fn test_update_order_reserve_with_auto_crossing_depth_rests_refreshed() {
        for (label, update) in crossing_updates(new_id()) {
            let maker_id = update_order_id(&update);
            let (book, _contra_id) = book_with_maker(
                "RSV-MODIFY-AUTO",
                10,
                reserve_buy(maker_id, 0, Some(10), true),
            );

            let result = book.update_order(update);
            assert!(
                result.is_ok(),
                "{label}: auto-replenishing re-price rejected: {result:?}"
            );

            match book.get_order(maker_id) {
                Some(order) => {
                    assert_eq!(
                        order.price().as_u128(),
                        CROSS_PRICE,
                        "{label}: the residual rests at the new price"
                    );
                    assert_eq!(
                        (
                            order.visible_quantity().as_u64(),
                            order.hidden_quantity().as_u64()
                        ),
                        (10, 10),
                        "{label}: the emptied visible tranche grows by min(10, 20)"
                    );
                }
                None => panic!("{label}: an auto-replenishing residual must rest"),
            }
        }
    }

    /// An iceberg is outside the rule entirely: admission guarantees a
    /// positive display size, so its residual always keeps
    /// `min(display, remaining) > 0` visible and a crossing re-price rests
    /// as before.
    #[test]
    fn test_update_order_iceberg_crossing_depth_rests_unaffected() {
        for (label, update) in crossing_updates(new_id()) {
            let maker_id = update_order_id(&update);
            let (book, _contra_id) = book_with_maker(
                "ICE-MODIFY",
                10,
                iceberg_buy(maker_id, REST_PRICE, VISIBLE, HIDDEN),
            );

            let result = book.update_order(update);
            assert!(
                result.is_ok(),
                "{label}: iceberg re-price rejected: {result:?}"
            );

            match book.get_order(maker_id) {
                Some(order) => assert_eq!(
                    (
                        order.visible_quantity().as_u64(),
                        order.hidden_quantity().as_u64()
                    ),
                    (10, 10),
                    "{label}: the iceberg residual of 20 rests with one display tranche visible"
                ),
                None => panic!("{label}: the iceberg residual must rest"),
            }
        }
    }
}
