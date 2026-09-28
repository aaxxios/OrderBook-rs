//! #226: reserve orders are lot-size validated per tranche and on their
//! replenishment transfer.
//!
//! - Admission: a reserve is checked exactly like an iceberg on its visible
//!   and hidden tranches (a 15 / 5 split on a lot-10 book used to pass on
//!   its total of 20) and, additionally, on the capped quantity that
//!   replenishment moves from hidden into the visible tranche.
//!   The transfer check applies only while `auto_replenish` is on, the
//!   single flag that decides whether anything is ever transferred (#230).
//! - The validate-first modify path projects the updated order through the
//!   same validator, so a misaligned `UpdateQuantity` /
//!   `UpdatePriceAndQuantity` / `Replace` is rejected and the original
//!   order rests untouched.
//! - Replenishment behaviour is exercised here only for its lot alignment:
//!   every refreshed maker and every rested residual stays tranche-aligned
//!   and conserves quantity, and a reserve that never replenishes leaves the
//!   book — stranding its hidden tranche — when its visible one is depleted.
//!   The residual policy itself (what an exhausted non-auto reserve residual
//!   does) lives in `reserve_residual_policy_tests`.

#[cfg(test)]
mod tests_reserve_lot_size {
    use orderbook_rs::{OrderBook, OrderBookError};
    use pricelevel::{
        DEFAULT_RESERVE_REPLENISH_AMOUNT, Hash32, Id, OrderType, OrderUpdate, Price, Quantity,
        Side, TimeInForce, TimestampMs,
    };
    use std::num::NonZeroU64;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    const PRICE: u128 = 100;

    /// Build a reserve buy order with the given tranches and replenishment
    /// policy. `replenish_amount` of `None` (or `Some(0)`) leaves the order
    /// without an explicit amount.
    fn reserve_buy(
        id: Id,
        visible: u64,
        hidden: u64,
        threshold: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
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

    /// Assert the operation failed with `InvalidLotSize` on `quantity`.
    fn assert_invalid_lot<V: std::fmt::Debug>(
        result: Result<V, OrderBookError>,
        quantity: u64,
        lot: u64,
    ) {
        match result {
            Err(OrderBookError::InvalidLotSize {
                quantity: reported,
                lot_size,
            }) => {
                assert_eq!(reported, quantity, "offending quantity reported");
                assert_eq!(lot_size, lot, "configured lot size reported");
            }
            other => panic!("expected InvalidLotSize {{ quantity: {quantity} }}, got {other:?}"),
        }
    }

    /// Read the resting tranches of `order_id`, or `(0, 0)` when the order
    /// is no longer on the book.
    fn resting_tranches(book: &OrderBook<()>, order_id: Id) -> (u64, u64) {
        match book.get_order(order_id) {
            Some(order) => (
                order.visible_quantity().as_u64(),
                order.hidden_quantity().as_u64(),
            ),
            None => (0, 0),
        }
    }

    /// Both tranches of the resting order must stay whole multiples of the
    /// book's lot size after any replenishment.
    fn assert_tranches_aligned(visible: u64, hidden: u64, lot: u64) {
        assert_eq!(
            visible % lot,
            0,
            "visible tranche {visible} not lot-aligned"
        );
        assert_eq!(hidden % lot, 0, "hidden tranche {hidden} not lot-aligned");
    }

    /// `executed + resting visible + resting hidden == submitted total`.
    fn assert_conserved(executed: u64, visible: u64, hidden: u64, submitted: u64) {
        assert_eq!(
            executed + visible + hidden,
            submitted,
            "quantity conservation: executed {executed} + visible {visible} + hidden {hidden} != submitted {submitted}"
        );
    }

    // ----------------------------------------------------------------
    // 1. Admission
    // ----------------------------------------------------------------

    /// The issue's repro: 15 visible / 5 hidden totals 20 — a multiple of
    /// the lot — but the visible tranche is not, so the reserve is rejected
    /// on the tranche just like an iceberg would be.
    #[test]
    fn test_add_order_reserve_misaligned_tranche_rejects() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT", 10);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 15, 5, 0, None, false));

        assert_invalid_lot(result, 15, 10);
        assert!(
            book.get_order(order_id).is_none(),
            "a rejected reserve must not rest"
        );
        assert!(book.best_bid().is_none(), "no level may be created");
    }

    /// The identical iceberg split is rejected on the same quantity — the
    /// two kinds share their visible / hidden rule.
    #[test]
    fn test_add_iceberg_order_misaligned_tranche_rejects() {
        let book: OrderBook<()> = OrderBook::with_lot_size("ICEBERG-LOT", 10);
        let order_id = new_id();

        let result =
            book.add_iceberg_order(order_id, PRICE, 15, 5, Side::Buy, TimeInForce::Gtc, None);

        assert_invalid_lot(result, 15, 10);
        assert!(
            book.get_order(order_id).is_none(),
            "a rejected iceberg must not rest"
        );
    }

    /// Both tranches are aligned, but replenishment would move 7 units into
    /// the visible tranche — an aggressive fill of 10 would leave the order
    /// resting as 7 / 13. Rejected on the transfer.
    #[test]
    fn test_add_order_reserve_misaligned_replenish_amount_with_auto_rejects() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT", 10);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 10, 20, 0, Some(7), true));

        assert_invalid_lot(result, 7, 10);
        assert!(
            book.get_order(order_id).is_none(),
            "a rejected reserve must not rest"
        );
    }

    /// The identical shape is admitted without `auto_replenish` (#230): no
    /// transfer ever happens on either path — a depleted visible tranche
    /// removes the resting maker and ends the aggressive residual — so the
    /// misaligned 7 can never be displayed and is not validated.
    #[test]
    fn test_add_order_reserve_misaligned_replenish_amount_without_auto_accepts() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT", 10);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 10, 20, 0, Some(7), false));

        assert!(
            result.is_ok(),
            "non-replenishing reserve rejected on a dead amount: {result:?}"
        );
        assert_eq!(resting_tranches(&book, order_id), (10, 20));
    }

    /// Without an explicit amount, an auto-replenishing reserve transfers
    /// `pricelevel`'s default capped by hidden: `min(80, 50) == 50`, a
    /// multiple of 25.
    #[test]
    fn test_add_order_reserve_default_replenish_capped_by_hidden_accepts() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT-25", 25);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 25, 50, 0, None, true));

        assert!(
            result.is_ok(),
            "capped default transfer rejected: {result:?}"
        );
        assert_eq!(resting_tranches(&book, order_id), (25, 50));
    }

    /// With more hidden depth than the default amount the transfer is not
    /// capped, and 80 is not a multiple of 25.
    #[test]
    fn test_add_order_reserve_default_replenish_uncapped_rejects() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT-25", 25);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 25, 100, 0, None, true));

        assert_invalid_lot(result, DEFAULT_RESERVE_REPLENISH_AMOUNT.get(), 25);
        assert!(book.get_order(order_id).is_none());
    }

    /// The very same shape rests fine without `auto_replenish`: with no
    /// explicit amount nothing is ever transferred.
    #[test]
    fn test_add_order_reserve_without_auto_replenish_accepts() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-LOT-25", 25);
        let order_id = new_id();

        let result = book.add_order(reserve_buy(order_id, 25, 100, 0, None, false));

        assert!(
            result.is_ok(),
            "non-replenishing reserve rejected: {result:?}"
        );
        assert_eq!(resting_tranches(&book, order_id), (25, 100));
    }

    // ----------------------------------------------------------------
    // 2. Validate-first modification
    // ----------------------------------------------------------------

    /// A lot-10 book holding an aligned 10 / 20 reserve buy at `PRICE`.
    fn book_with_resting_reserve(order_id: Id) -> OrderBook<()> {
        let mut book: OrderBook<()> = OrderBook::new("RESERVE-MODIFY");
        book.set_lot_size(10);
        let added = book.add_order(reserve_buy(order_id, 10, 20, 0, Some(10), true));
        assert!(added.is_ok(), "seeding the reserve must succeed: {added:?}");
        book
    }

    /// The resting reserve is untouched: same price, same tranches.
    fn assert_original_intact(book: &OrderBook<()>, order_id: Id) {
        match book.get_order(order_id) {
            Some(order) => {
                assert_eq!(order.price().as_u128(), PRICE, "price unchanged");
                assert_eq!(order.visible_quantity().as_u64(), 10, "visible unchanged");
                assert_eq!(order.hidden_quantity().as_u64(), 20, "hidden unchanged");
            }
            None => panic!("a rejected update must leave the original resting"),
        }
        assert_eq!(book.best_bid(), Some(PRICE), "the level must survive");
    }

    /// `UpdatePriceAndQuantity` projects the new visible tranche through the
    /// validator before cancelling anything.
    #[test]
    fn test_update_order_reserve_price_and_quantity_misaligned_rejects() {
        let order_id = new_id();
        let book = book_with_resting_reserve(order_id);

        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(PRICE),
            new_quantity: Quantity::new(15),
        });

        assert_invalid_lot(result, 15, 10);
        assert_original_intact(&book, order_id);
    }

    /// `UpdateQuantity` takes the same validate-first path.
    #[test]
    fn test_update_order_reserve_quantity_misaligned_rejects() {
        let order_id = new_id();
        let book = book_with_resting_reserve(order_id);

        let result = book.update_order(OrderUpdate::UpdateQuantity {
            order_id,
            new_quantity: Quantity::new(15),
        });

        assert_invalid_lot(result, 15, 10);
        assert_original_intact(&book, order_id);
    }

    /// `Replace` rewrites the visible tranche and keeps the reserve shape,
    /// so it is rejected on the same quantity.
    #[test]
    fn test_update_order_reserve_replace_misaligned_rejects() {
        let order_id = new_id();
        let book = book_with_resting_reserve(order_id);

        let result = book.update_order(OrderUpdate::Replace {
            order_id,
            price: Price::new(PRICE),
            quantity: Quantity::new(15),
            side: Side::Buy,
        });

        assert_invalid_lot(result, 15, 10);
        assert_original_intact(&book, order_id);
    }

    /// An aligned update still applies: `new_quantity` sets the visible
    /// tranche (#221) and hidden is untouched, so the order rests 30 / 20.
    #[test]
    fn test_update_order_reserve_aligned_quantity_applies() {
        let order_id = new_id();
        let book = book_with_resting_reserve(order_id);

        let result = book.update_order(OrderUpdate::UpdateQuantity {
            order_id,
            new_quantity: Quantity::new(30),
        });

        assert!(result.is_ok(), "aligned update rejected: {result:?}");
        assert_eq!(resting_tranches(&book, order_id), (30, 20));
        assert_eq!(book.best_bid(), Some(PRICE));
    }

    // ----------------------------------------------------------------
    // 3. Replenishment behaviour (unchanged, and lot-aligned throughout)
    // ----------------------------------------------------------------

    /// Maker depletion: an aggressive contra order that consumes the whole
    /// visible tranche makes `pricelevel` refresh it with
    /// `min(replenish_amount, hidden) == 10`, so the maker rests 10 / 10 —
    /// both tranches aligned, nothing manufactured.
    #[test]
    fn test_reserve_maker_depletion_replenishes_aligned_tranches() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-FILL", 10);
        let maker_id = new_id();

        let added = book.add_order(reserve_buy(maker_id, 10, 20, 0, Some(10), true));
        assert!(added.is_ok(), "seeding the maker must succeed: {added:?}");

        let taken = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let executed = match taken {
            Ok((_, Some(trade))) => match trade.match_result.executed_quantity() {
                Ok(quantity) => quantity.as_u64(),
                Err(error) => panic!("executed quantity unavailable: {error}"),
            },
            other => panic!("expected a trade from the crossing sell, got {other:?}"),
        };
        assert_eq!(executed, 10, "the visible tranche is fully consumed");

        let (visible, hidden) = resting_tranches(&book, maker_id);
        assert_eq!(
            (visible, hidden),
            (10, 10),
            "the emptied visible tranche is refreshed from hidden"
        );
        assert_tranches_aligned(visible, hidden, 10);
        assert_conserved(executed, visible, hidden, 30);
    }

    /// Partial refresh below a non-aligned threshold: consuming 10 of a 20
    /// visible tranche leaves 10 < 15, which triggers the refresh. The
    /// threshold is never transferred, so its misalignment is harmless —
    /// the maker rests 20 / 30, still aligned.
    #[test]
    fn test_reserve_maker_below_threshold_replenishes_aligned_tranches() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-THRESHOLD", 10);
        let maker_id = new_id();

        let added = book.add_order(reserve_buy(maker_id, 20, 40, 15, Some(10), true));
        assert!(
            added.is_ok(),
            "a misaligned threshold must not block admission: {added:?}"
        );

        let taken = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let executed = match taken {
            Ok((_, Some(trade))) => match trade.match_result.executed_quantity() {
                Ok(quantity) => quantity.as_u64(),
                Err(error) => panic!("executed quantity unavailable: {error}"),
            },
            other => panic!("expected a trade from the crossing sell, got {other:?}"),
        };
        assert_eq!(executed, 10, "the contra order is fully filled");

        let (visible, hidden) = resting_tranches(&book, maker_id);
        assert_eq!(
            (visible, hidden),
            (20, 30),
            "10 left visible plus a 10 refresh, drawn from hidden"
        );
        assert_tranches_aligned(visible, hidden, 10);
        assert_conserved(executed, visible, hidden, 60);
    }

    /// The upstream half of the `auto_replenish = false` exemption: with
    /// automatic replenishment off, `pricelevel`'s `match_against`
    /// returns `(consumed, None, 0, remaining)` once the visible tranche is
    /// fully consumed, so the level **removes** the maker instead of
    /// refreshing it — the hidden tranche is stranded and dropped. Nothing
    /// is ever transferred, which is exactly why admission skips the
    /// transfer check for this policy: the book can never end up displaying
    /// a non-aligned visible tranche. (The local half — an exhausted
    /// non-auto residual ending instead of resting — is pinned by
    /// `reserve_residual_policy_tests`.)
    #[test]
    fn test_reserve_maker_without_auto_replenish_leaves_book_on_depletion() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-NO-AUTO", 25);
        let maker_id = new_id();

        // Admitted with no transfer check: 25 / 100 with no explicit amount
        // and no auto-replenishment. The default amount (80) would not be a
        // multiple of 25, so this order only rests because the exemption
        // applies.
        let added = book.add_order(reserve_buy(maker_id, 25, 100, 0, None, false));
        assert!(added.is_ok(), "seeding the maker must succeed: {added:?}");
        assert_eq!(resting_tranches(&book, maker_id), (25, 100));

        let taken = book.add_limit_order_with_result(
            new_id(),
            PRICE,
            25,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
        let executed = match taken {
            Ok((_, Some(trade))) => match trade.match_result.executed_quantity() {
                Ok(quantity) => quantity.as_u64(),
                Err(error) => panic!("executed quantity unavailable: {error}"),
            },
            other => panic!("expected a trade from the crossing sell, got {other:?}"),
        };
        assert_eq!(executed, 25, "only the visible tranche is executable");

        assert!(
            book.get_order(maker_id).is_none(),
            "a non-replenishing reserve leaves the book when its visible tranche is depleted"
        );
        assert!(
            book.best_bid().is_none(),
            "the emptied level is removed with the maker"
        );
        assert!(
            book.best_ask().is_none(),
            "the crossing sell is fully filled and never rests"
        );

        // Conservation does not hold across this step, and that is the
        // upstream contract rather than a regression: the 100 hidden units
        // are stranded by the removal, so `executed + resting == 25` out of
        // the 125 submitted.
        let (visible, hidden) = resting_tranches(&book, maker_id);
        assert_eq!((visible, hidden), (0, 0), "nothing rests");
        assert_conserved(executed, visible, hidden, 25);
    }

    /// Aggressive residual resting: an **auto-replenishing** reserve taker
    /// that fills its whole visible tranche rests its residual through
    /// `set_total_remaining`, whose helper refreshes the emptied visible
    /// tranche with `min(replenish_amount, hidden)` — the same transfer
    /// admission validated. The residual rests 10 / 10, lot-aligned and
    /// conserved.
    ///
    /// With automatic replenishment off the residual does not rest at all
    /// (#230); that policy is covered by `reserve_residual_policy_tests`.
    #[test]
    fn test_reserve_taker_residual_rests_aligned_tranches() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-TAKER", 10);
        let contra = book.add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None);
        assert!(
            contra.is_ok(),
            "seeding contra depth must succeed: {contra:?}"
        );

        let taker_id = new_id();
        let submitted =
            book.add_order_with_result(reserve_buy(taker_id, 10, 20, 0, Some(10), true));
        let executed = match submitted {
            Ok((_, Some(trade))) => match trade.match_result.executed_quantity() {
                Ok(quantity) => quantity.as_u64(),
                Err(error) => panic!("executed quantity unavailable: {error}"),
            },
            other => panic!("expected a trade from the aggressive reserve, got {other:?}"),
        };
        assert_eq!(executed, 10, "all available contra depth is taken");

        let (visible, hidden) = resting_tranches(&book, taker_id);
        assert_eq!(
            (visible, hidden),
            (10, 10),
            "the residual of 20 rests with the visible tranche refreshed from hidden"
        );
        assert_tranches_aligned(visible, hidden, 10);
        assert_conserved(executed, visible, hidden, 30);
        assert_eq!(book.best_bid(), Some(PRICE), "the residual rests as a bid");
        assert!(book.best_ask().is_none(), "the contra depth is exhausted");
    }

    /// The same taker with a misaligned replenish amount never reaches the
    /// book: admission rejects it, so the 7 / 13 residual it would have
    /// rested cannot exist.
    #[test]
    fn test_reserve_taker_misaligned_replenish_amount_rejects_before_matching() {
        let book: OrderBook<()> = OrderBook::with_lot_size("RESERVE-TAKER", 10);
        let contra_id = new_id();
        let contra = book.add_limit_order(contra_id, PRICE, 10, Side::Sell, TimeInForce::Gtc, None);
        assert!(
            contra.is_ok(),
            "seeding contra depth must succeed: {contra:?}"
        );

        let taker_id = new_id();
        let result = book.add_order(reserve_buy(taker_id, 10, 20, 0, Some(7), true));

        assert_invalid_lot(result, 7, 10);
        assert!(
            book.get_order(taker_id).is_none(),
            "the rejected taker must not rest"
        );
        assert_eq!(
            resting_tranches(&book, contra_id),
            (10, 0),
            "the contra maker must be untouched — validation precedes matching"
        );
        assert!(book.best_bid().is_none(), "no residual bid level");
    }
}
