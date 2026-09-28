//! `UpdateQuantity` with a zero `new_quantity` cancels the entire order.
//!
//! A zero-quantity maker cannot fill, so resting one published a price
//! level with no depth: it held `best_bid` / `best_ask`, made
//! `will_cross_market` reject a post-only at that price, and was later
//! dropped by a sweep with no trade and no cancel event — leaving the
//! `order_locations` entry behind, so `cancel_order` returned `Ok(None)`
//! and re-adding the id reported `DuplicateOrderId`.
//!
//! Zero is a removal, not a resize: it bypasses the projected-order
//! validator (a configured `min_order_size` does not veto it) and the
//! modify-aware risk check (no limit vetoes it either), and it cancels a
//! two-tranche order whole, hidden depth included. Being a real cancel, it
//! releases the per-account risk contribution and emits the level-change
//! event the old silent resize never sent.
//!
//! The last section pins the contrast the contract rests on: the removal
//! semantic is `UpdateQuantity`'s alone. `Replace` and
//! `UpdatePriceAndQuantity` with a zero quantity re-add through
//! validate-first, so on an iceberg or an auto-replenishing reserve they
//! leave the order resting on a zero visible tranche with live hidden
//! depth, on a non-replenishing reserve they are rejected with
//! `ZeroVisibleTranche`, and on a single-tranche maker they end the order
//! as a terminal `Filled` with nothing filled.

#[cfg(test)]
mod tests_update_quantity_zero {
    use orderbook_rs::orderbook::book_change_event::PriceLevelChangedEvent;
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::{
        DefaultOrderBook, OrderBook, OrderBookError, ReferencePriceSource, RiskConfig,
    };
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };
    use std::num::NonZeroU64;
    use std::sync::{Arc, Mutex};

    const PRICE: u128 = 100;
    const MAKER: u64 = 1;

    fn book_with_resting_ask(symbol: &str) -> OrderBook<()> {
        let mut book: OrderBook<()> = DefaultOrderBook::new(symbol);
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");
        book
    }

    fn update_to_zero(book: &OrderBook<()>) -> Option<Arc<OrderType<()>>> {
        book.update_order(OrderUpdate::UpdateQuantity {
            order_id: Id::from_u64(MAKER),
            new_quantity: Quantity::new(0),
        })
        .expect("update to zero succeeds")
    }

    /// The order leaves the book with a terminal cancel, and the level it
    /// was alone on is removed with it.
    #[test]
    fn update_to_zero_cancels_the_order_and_removes_the_level() {
        let book = book_with_resting_ask("UQZ1");

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));

        assert!(
            book.get_order(Id::from_u64(MAKER)).is_none(),
            "no ghost order"
        );
        assert_eq!(book.best_ask(), None, "no phantom level at zero depth");
        assert!(
            book.create_snapshot(usize::MAX)
                .expect("snapshot")
                .asks
                .is_empty(),
            "the empty level was removed"
        );
        assert_eq!(
            book.order_status(Id::from_u64(MAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            }),
            "a cancel event is recorded"
        );
    }

    /// The id is free again: the location entry no longer leaks.
    #[test]
    fn cancelled_id_is_reusable() {
        let book = book_with_resting_ask("UQZ2");
        update_to_zero(&book);

        assert!(
            book.cancel_order(Id::from_u64(MAKER))
                .expect("cancel of an absent order is not an error")
                .is_none(),
            "the order is already gone"
        );
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            4,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("the id is reusable");
        assert_eq!(book.best_ask(), Some(PRICE));
    }

    /// A post-only order at the vacated price no longer crosses nothing.
    #[test]
    fn post_only_at_the_vacated_price_is_admitted() {
        let book = book_with_resting_ask("UQZ3");
        update_to_zero(&book);

        book.add_post_only_order(Id::from_u64(2), PRICE, 5, Side::Buy, TimeInForce::Gtc, None)
            .expect("nothing to cross at the vacated price");
        assert_eq!(book.best_bid(), Some(PRICE));
    }

    /// The removal is atomic with the rest of the level: only the zeroed
    /// order goes.
    #[test]
    fn a_shared_level_keeps_its_other_makers() {
        let book = book_with_resting_ask("UQZ4");
        book.add_limit_order(
            Id::from_u64(2),
            PRICE,
            7,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed second maker");

        update_to_zero(&book);

        assert_eq!(book.best_ask(), Some(PRICE), "the level still has depth");
        assert_eq!(
            book.get_order(Id::from_u64(2))
                .expect("the other maker rests")
                .visible_quantity()
                .as_u64(),
            7
        );
    }

    /// Zeroing an absent order stays `Ok(None)`, as every other update
    /// variant reports a missing order.
    #[test]
    fn update_to_zero_on_an_absent_order_is_none() {
        let book = book_with_resting_ask("UQZ5");

        let result = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(99),
                new_quantity: Quantity::new(0),
            })
            .expect("absent order is not an error");
        assert!(result.is_none());
        assert_eq!(book.best_ask(), Some(PRICE), "the real maker is untouched");
    }

    /// The kill switch still gates it: this is a modify, not a cancel.
    #[test]
    fn update_to_zero_is_rejected_while_the_kill_switch_is_engaged() {
        let book = book_with_resting_ask("UQZ6");
        book.engage_kill_switch();

        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(MAKER),
                new_quantity: Quantity::new(0),
            })
            .expect_err("modifications are halted");
        assert!(
            matches!(err, OrderBookError::KillSwitchActive),
            "expected KillSwitchActive, got {err:?}"
        );
        assert!(
            book.get_order(Id::from_u64(MAKER)).is_some(),
            "the maker survives a rejected modify"
        );
    }

    fn assert_cancelled_whole(book: &OrderBook<()>) {
        assert!(
            book.get_order(Id::from_u64(MAKER)).is_none(),
            "the whole order is gone, hidden depth included"
        );
        assert_eq!(book.best_ask(), None, "no level survives on hidden depth");
        assert!(
            book.create_snapshot(usize::MAX)
                .expect("snapshot")
                .asks
                .is_empty(),
            "the empty level was removed"
        );
        assert_eq!(
            book.order_status(Id::from_u64(MAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            }),
            "a cancel event is recorded"
        );
    }

    /// `new_quantity` is the visible tranche for a two-tranche order, so a
    /// zero visible quantity says nothing about the total. Zero still
    /// cancels the entire order: it is a removal, not a resize, and the
    /// hidden depth goes with it rather than surviving as a 0-visible
    /// ghost pinning `best_ask`.
    #[test]
    fn iceberg_zeroed_with_hidden_depth_is_cancelled_whole() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ7");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_iceberg_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            50,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed iceberg");
        assert_eq!(
            book.get_order(Id::from_u64(MAKER))
                .expect("iceberg rests")
                .hidden_quantity()
                .as_u64(),
            50,
            "hidden depth is resting before the update"
        );

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_eq!(
            removed.hidden_quantity().as_u64(),
            50,
            "the order was removed intact, not resized to zero first"
        );
        assert_cancelled_whole(&book);
    }

    /// Same contract for a reserve order, whose hidden tranche is otherwise
    /// drawn down by replenishment rather than by `UpdateQuantity`.
    #[test]
    fn reserve_zeroed_with_hidden_depth_is_cancelled_whole() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ8");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_order(OrderType::ReserveOrder {
            id: Id::from_u64(MAKER),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(10),
            hidden_quantity: Quantity::new(40),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(5),
            replenish_amount: Some(NonZeroU64::new(10).expect("nonzero")),
            auto_replenish: true,
            extra_fields: (),
        })
        .expect("seed reserve order");

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_eq!(
            removed.hidden_quantity().as_u64(),
            40,
            "the order was removed intact, not resized to zero first"
        );
        assert_cancelled_whole(&book);
    }

    /// A size floor has no say over a removal: the zero branch runs before
    /// the projected-order validator, so a configured `min_order_size`
    /// cancels the order exactly as it would without one. The validator
    /// still guards every nonzero resize on the same book.
    #[test]
    fn min_order_size_does_not_veto_the_zero_update() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ9");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_min_order_size(5);
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");

        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(MAKER),
                new_quantity: Quantity::new(3),
            })
            .expect_err("a nonzero resize below the floor is validated");
        assert!(
            matches!(err, OrderBookError::OrderSizeOutOfRange { .. }),
            "expected OrderSizeOutOfRange, got {err:?}"
        );
        assert_eq!(
            book.get_order(Id::from_u64(MAKER))
                .expect("a rejected resize leaves the maker untouched")
                .visible_quantity()
                .as_u64(),
            10
        );

        let removed = update_to_zero(&book).expect("zero is a removal, not a resize");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_cancelled_whole(&book);
    }

    // ───────────────────────────────────────────────────────────────
    // The zero update runs no risk check either
    // ───────────────────────────────────────────────────────────────

    /// A risk limit has no more say over the removal than `min_order_size`
    /// does: the zero branch returns before `check_risk_modify_admission`.
    ///
    /// Derivation of the veto. The maker is admitted under a notional-only
    /// config, so the risk layer tracks it at `10 × 1_000 = 10_000`. A price
    /// band of 100 bps around a fixed reference of 100 is then installed,
    /// which admits a price `p` only while `|p - 100| × 10_000 <= 100 × 100`,
    /// that is `|p - 100| <= 1`. The maker rests at 1_000, so every modify
    /// that reaches the risk check is rejected on price alone: the projected
    /// notional `10_000 - 10_000 + 4 × 1_000 = 4_000` clears the 1_000_000
    /// ceiling, and the band does not.
    #[test]
    fn a_risk_limit_does_not_veto_the_zero_update() {
        const BANDED_PRICE: u128 = 1_000;

        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ10");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(1_000_000));
        book.add_limit_order(
            Id::from_u64(MAKER),
            BANDED_PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");
        book.set_risk_config(
            RiskConfig::new()
                .with_max_notional_per_account(1_000_000)
                .with_price_band_bps(100, ReferencePriceSource::FixedPrice(100)),
        );

        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(MAKER),
                new_quantity: Quantity::new(4),
            })
            .expect_err("a nonzero resize is risk-checked");
        assert!(
            matches!(err, OrderBookError::RiskPriceBand { .. }),
            "expected RiskPriceBand, got {err:?}"
        );
        assert_eq!(
            book.get_order(Id::from_u64(MAKER))
                .expect("a vetoed resize leaves the maker untouched")
                .visible_quantity()
                .as_u64(),
            10
        );

        let removed = update_to_zero(&book).expect("zero is a removal, not a resize");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert!(
            book.get_order(Id::from_u64(MAKER)).is_none(),
            "the limit that vetoes every resize does not veto the removal"
        );
        assert_eq!(
            book.order_status(Id::from_u64(MAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            }),
        );
    }

    /// The removal runs the same `cancel_order_with_reason` a user cancel
    /// runs, so it releases the maker's per-account risk contribution.
    ///
    /// Derivation of both verdicts against a 15_000 ceiling. While the maker
    /// rests, the account's resting notional is `10 × 1_000 = 10_000` and a
    /// second identical order is checked as `10_000 + 10 × 1_000 = 20_000`,
    /// above the ceiling, so it is rejected. After the zero update,
    /// `on_cancel` subtracts the tracked `remaining_qty × price = 10_000`,
    /// leaving 0, and the same submit is checked as `0 + 10_000 = 10_000`
    /// and rests.
    #[test]
    fn the_zero_update_releases_the_per_account_risk_contribution() {
        const NOTIONAL_PRICE: u128 = 1_000;
        const SECOND: u64 = 2;

        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ11");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(15_000));
        book.add_limit_order(
            Id::from_u64(MAKER),
            NOTIONAL_PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");

        fn submit_second(book: &OrderBook<()>) -> Result<Arc<OrderType<()>>, OrderBookError> {
            book.add_limit_order(
                Id::from_u64(SECOND),
                NOTIONAL_PRICE,
                10,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
        }

        let err = submit_second(&book).expect_err("20_000 breaches the 15_000 ceiling");
        assert!(
            matches!(err, OrderBookError::RiskMaxNotional { .. }),
            "expected RiskMaxNotional, got {err:?}"
        );

        update_to_zero(&book).expect("the maker is removed");

        submit_second(&book).expect("the released 10_000 makes room for the second order");
        assert_eq!(
            book.get_order(Id::from_u64(SECOND))
                .expect("the second order rests")
                .visible_quantity()
                .as_u64(),
            10,
        );
    }

    // ───────────────────────────────────────────────────────────────
    // Level-change eventing
    // ───────────────────────────────────────────────────────────────

    /// Collect every `PriceLevelChangedEvent` emitted from the moment the
    /// listener is installed.
    fn record_level_changes(book: &mut OrderBook<()>) -> Arc<Mutex<Vec<PriceLevelChangedEvent>>> {
        let events: Arc<Mutex<Vec<PriceLevelChangedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            if let Ok(mut recorded) = sink.lock() {
                recorded.push(event);
            }
        }));
        events
    }

    /// The removal reports the level's new depth exactly once, like any
    /// other cancel — the old resize emitted nothing at all. The listener is
    /// installed after the seeding admissions, so what it records is the
    /// update's alone.
    ///
    /// Derivation: `cancel_order_with_reason` emits one event carrying
    /// `price_level.visible_quantity()` read after the removal. The level
    /// holds `10 + 4` before the update and the zeroed maker is the 10, so
    /// the event carries 4.
    #[test]
    fn the_zero_update_emits_one_level_change_carrying_the_surviving_depth() {
        const NEIGHBOUR: u64 = 2;

        let mut book = book_with_resting_ask("UQZ12");
        book.add_limit_order(
            Id::from_u64(NEIGHBOUR),
            PRICE,
            4,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed a second maker on the same level");
        let events = record_level_changes(&mut book);

        update_to_zero(&book).expect("the maker is removed");

        let recorded = events.lock().expect("listener mutex");
        assert_eq!(
            recorded.len(),
            1,
            "exactly one level change, got {recorded:?}"
        );
        assert_eq!(recorded[0].side, Side::Sell);
        assert_eq!(recorded[0].price, PRICE);
        assert_eq!(
            recorded[0].quantity, 4,
            "the level's depth after the removal, not the removed quantity"
        );
    }

    /// Alone on its level, the removal reports zero depth — the signal a
    /// consumer needs to drop the level, which the old resize never sent.
    #[test]
    fn the_zero_update_emits_a_zero_depth_level_change_when_the_maker_was_alone() {
        let mut book = book_with_resting_ask("UQZ13");
        let events = record_level_changes(&mut book);

        update_to_zero(&book).expect("the maker is removed");

        let recorded = events.lock().expect("listener mutex");
        assert_eq!(
            recorded.len(),
            1,
            "exactly one level change, got {recorded:?}"
        );
        assert_eq!(recorded[0].side, Side::Sell);
        assert_eq!(recorded[0].price, PRICE);
        assert_eq!(recorded[0].quantity, 0, "the level is now empty");
    }

    // ───────────────────────────────────────────────────────────────
    // The contrast: a zero quantity on `Replace` /
    // `UpdatePriceAndQuantity` is not a removal
    // ───────────────────────────────────────────────────────────────

    /// One of the two cancel-then-add variants, driven to a zero quantity at
    /// the maker's own price so nothing but the quantity changes.
    type ZeroModify = fn(&OrderBook<()>) -> Result<Option<Arc<OrderType<()>>>, OrderBookError>;

    fn replace_to_zero(book: &OrderBook<()>) -> Result<Option<Arc<OrderType<()>>>, OrderBookError> {
        book.update_order(OrderUpdate::Replace {
            order_id: Id::from_u64(MAKER),
            price: Price::new(PRICE),
            quantity: Quantity::new(0),
            side: Side::Sell,
        })
    }

    fn update_price_and_quantity_to_zero(
        book: &OrderBook<()>,
    ) -> Result<Option<Arc<OrderType<()>>>, OrderBookError> {
        book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id: Id::from_u64(MAKER),
            new_price: Price::new(PRICE),
            new_quantity: Quantity::new(0),
        })
    }

    const ZERO_MODIFIES: [(&str, ZeroModify); 2] = [
        ("Replace", replace_to_zero),
        ("UpdatePriceAndQuantity", update_price_and_quantity_to_zero),
    ];

    /// A reserve ask at `PRICE` with the given tranches and replenishment
    /// policy. `replenish_amount` is 10 and the threshold 0 throughout, so
    /// an auto-replenishing reserve transfers `min(10, hidden)` per refresh.
    fn reserve_ask(id: u64, visible: u64, hidden: u64, auto_replenish: bool) -> OrderType<()> {
        OrderType::ReserveOrder {
            id: Id::from_u64(id),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: Some(NonZeroU64::new(10).expect("nonzero")),
            auto_replenish,
            extra_fields: (),
        }
    }

    /// Cross the resting ask with a buy for `quantity` and return the
    /// quantity actually printed, summed from the trades themselves rather
    /// than read off a status.
    fn hit_the_ask(book: &OrderBook<()>, quantity: u64) -> u64 {
        const TAKER: u64 = 9;
        let (_, trade) = book
            .add_limit_order_with_result(
                Id::from_u64(TAKER),
                PRICE,
                quantity,
                Side::Buy,
                TimeInForce::Gtc,
                None,
            )
            .expect("the taker is admitted");
        trade
            .map(|result| {
                result
                    .match_result
                    .trades()
                    .as_vec()
                    .iter()
                    .map(|print| print.quantity().as_u64())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// The maker's resting tranches, or `None` once it is gone.
    fn tranches(book: &OrderBook<()>) -> Option<(u64, u64)> {
        book.get_order(Id::from_u64(MAKER)).map(|order| {
            (
                order.visible_quantity().as_u64(),
                order.hidden_quantity().as_u64(),
            )
        })
    }

    /// Both cancel-then-add variants leave an iceberg resting on a zero
    /// visible tranche with its hidden depth live, and it still fills.
    ///
    /// Derivation. The projected order is `(0, 20)`, which
    /// `validate_order_shape` admits: `is_zero_visible_ghost` matches a
    /// `ReserveOrder { auto_replenish: false, .. }` only. The re-add finds no
    /// bids, so `remaining == total == 20`, the residual branch is skipped
    /// and the order rests exactly as projected, `(0, 20)`, status `Open`.
    /// Hit for 20: upstream's degenerate guard draws the whole hidden
    /// tranche into visible, so all 20 print and the maker leaves as
    /// `Filled { filled_quantity: 20 }`.
    #[test]
    fn a_zero_quantity_replace_leaves_an_iceberg_resting_and_fillable() {
        for (label, apply) in ZERO_MODIFIES {
            let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ14");
            book.set_order_state_tracker(OrderStateTracker::new());
            book.add_iceberg_order(
                Id::from_u64(MAKER),
                PRICE,
                10,
                20,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
            .expect("seed iceberg");

            let returned = apply(&book)
                .unwrap_or_else(|err| panic!("{label} with a zero quantity is admitted: {err:?}"))
                .expect("the re-added order is returned");
            assert_eq!(
                (
                    returned.visible_quantity().as_u64(),
                    returned.hidden_quantity().as_u64()
                ),
                (0, 20),
                "{label}: the visible tranche is zeroed, hidden is untouched"
            );
            assert_eq!(tranches(&book), Some((0, 20)), "{label}: it keeps resting");
            assert_eq!(
                book.order_status(Id::from_u64(MAKER)),
                Some(OrderStatus::Open),
                "{label}: no cancel, unlike UpdateQuantity"
            );

            assert_eq!(
                hit_the_ask(&book, 20),
                20,
                "{label}: the hidden tranche is live and fills"
            );
            assert_eq!(tranches(&book), None, "{label}: fully consumed");
            assert_eq!(
                book.order_status(Id::from_u64(MAKER)),
                Some(OrderStatus::Filled {
                    filled_quantity: 20
                }),
                "{label}: it left by filling, not by the update"
            );
        }
    }

    /// Same for an auto-replenishing reserve, which refreshes instead of
    /// drawing its whole hidden tranche in.
    ///
    /// Derivation. The projected `(0, 20)` reserve carries
    /// `auto_replenish: true`, so it is not the ghost shape and rests as
    /// projected. A buy for 10 makes `match_against` refresh
    /// `min(replenish_amount = 10, hidden = 20) = 10` into the visible
    /// tranche, leaving `(10, 10)`; the taker takes all 10, leaving
    /// `(0, 10)`; the visible tranche is then below `max(threshold = 0, 1)`
    /// with hidden left, so it refreshes once more by `min(10, 10) = 10` and
    /// the maker rests at `(10, 0)`. Ten print, and 10 of the original 20
    /// survive.
    #[test]
    fn a_zero_quantity_replace_leaves_an_auto_replenishing_reserve_resting_and_fillable() {
        for (label, apply) in ZERO_MODIFIES {
            let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ15");
            book.set_order_state_tracker(OrderStateTracker::new());
            book.add_order(reserve_ask(MAKER, 10, 20, true))
                .expect("seed auto-replenishing reserve");

            let returned = apply(&book)
                .unwrap_or_else(|err| panic!("{label} with a zero quantity is admitted: {err:?}"))
                .expect("the re-added order is returned");
            assert_eq!(
                (
                    returned.visible_quantity().as_u64(),
                    returned.hidden_quantity().as_u64()
                ),
                (0, 20),
                "{label}: the visible tranche is zeroed, hidden is untouched"
            );
            assert_eq!(tranches(&book), Some((0, 20)), "{label}: it keeps resting");
            assert_eq!(
                book.order_status(Id::from_u64(MAKER)),
                Some(OrderStatus::Open),
                "{label}: no cancel, unlike UpdateQuantity"
            );

            assert_eq!(
                hit_the_ask(&book, 10),
                10,
                "{label}: the reserve refreshes and fills"
            );
            assert_eq!(
                tranches(&book),
                Some((10, 0)),
                "{label}: refreshed twice, 10 of the original 20 left"
            );
        }
    }

    /// The one shape the validator does reject. Both variants project a
    /// zero-visible non-auto-replenishing reserve, which `pricelevel` would
    /// remove without a trade and strand, so the modify is refused and the
    /// original keeps resting at the tranches it had.
    ///
    /// Derivation: `is_zero_visible_ghost` matches `(visible == 0,
    /// hidden == 20, auto_replenish: false)` and reports
    /// `hidden_quantity: 20`, the hidden tranche as it would have been
    /// re-added.
    #[test]
    fn a_zero_quantity_replace_is_rejected_on_a_non_replenishing_reserve() {
        for (label, apply) in ZERO_MODIFIES {
            let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ16");
            book.set_order_state_tracker(OrderStateTracker::new());
            book.add_order(reserve_ask(MAKER, 10, 20, false))
                .expect("seed non-replenishing reserve");

            let err = apply(&book).expect_err("a zero visible tranche on this shape is refused");
            assert!(
                matches!(
                    err,
                    OrderBookError::ZeroVisibleTranche {
                        hidden_quantity: 20,
                        ..
                    }
                ),
                "{label}: expected ZeroVisibleTranche {{ hidden_quantity: 20 }}, got {err:?}"
            );
            assert_eq!(
                tranches(&book),
                Some((10, 20)),
                "{label}: a rejected modify leaves the original untouched"
            );
            assert_eq!(
                book.order_status(Id::from_u64(MAKER)),
                Some(OrderStatus::Open),
                "{label}: still open"
            );
        }
    }

    /// On a single-tranche maker the two cancel-then-add variants do remove
    /// the order — but as a terminal `Filled` with nothing filled, not as a
    /// cancel. Pinned, not endorsed: `UpdateQuantity` is the variant that
    /// removes an order honestly.
    ///
    /// Derivation. The projected `Standard` order carries quantity 0, which
    /// `validate_order_shape` admits on a book with no `min_order_size`. The
    /// original is cancelled, the re-add sweeps for 0 and comes back with
    /// `remaining_quantity == 0`, so `add_order_inner` takes its
    /// fully-matched branch: nothing rests and the status is
    /// `Filled { filled_quantity: original_qty }`, where `original_qty` is
    /// the projected order's total — zero.
    #[test]
    fn a_zero_quantity_replace_ends_a_standard_maker_with_a_zero_fill() {
        for (label, apply) in ZERO_MODIFIES {
            let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ17");
            book.set_order_state_tracker(OrderStateTracker::new());
            book.add_limit_order(
                Id::from_u64(MAKER),
                PRICE,
                10,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
            .expect("seed ask");

            let returned = apply(&book)
                .unwrap_or_else(|err| panic!("{label} with a zero quantity is admitted: {err:?}"))
                .expect("an order is returned");
            assert_eq!(
                (
                    returned.visible_quantity().as_u64(),
                    returned.hidden_quantity().as_u64()
                ),
                (0, 0),
                "{label}: the returned order holds nothing"
            );
            assert_eq!(tranches(&book), None, "{label}: it rests nowhere");
            assert_eq!(book.best_ask(), None, "{label}: no phantom level");
            assert_eq!(
                book.order_status(Id::from_u64(MAKER)),
                Some(OrderStatus::Filled { filled_quantity: 0 }),
                "{label}: terminal Filled with no fill, not Cancelled"
            );
        }
    }
}
