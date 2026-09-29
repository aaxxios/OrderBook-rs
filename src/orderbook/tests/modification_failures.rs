//! #247: modification paths never swallow a mutation error.
//!
//! - The `OrderUpdate::Cancel` arm of `update_order` is the same removal as
//!   `cancel_order`: a level failure is propagated and the book's indices
//!   never disagree with the level.
//! - A cancel-then-add modify whose re-add fails after the original was
//!   cancelled restores the original (`ModifyRolledBack`), or reports the
//!   order gone with consistent indices (`ModifyOrderLost`).
//! - A remainder the book cannot rest after the sweep ends the taker in a
//!   terminal state.
//! - A self-trade-prevention maker cancel the level fails aborts the sweep
//!   with the #240 semantics instead of carrying on.
//!
//! Failures are forced through the test-only `rest_fault_hook` (level
//! admission) and `cancel_fault_hook` (level removal): pricelevel 0.10 has
//! no public way to make a level fail either on demand.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::{CancelFault, OrderBook};
    use crate::orderbook::modifications::{Admission, ModifyPhase, OrderQuantity, ReAddQuantity};
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::risk::RiskConfig;
    use crate::orderbook::sequencer::{
        InMemoryJournal, Journal, ReplayEngine, ReplayError, SequencerCommand, SequencerEvent,
        SequencerResult,
    };
    use crate::orderbook::stp::STPMode;
    use crate::{Clock, OrderBookError, StubClock};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, PriceLevelError, Quantity, Side, TimeInForce,
        TimestampMs,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SYMBOL: &str = "MODFAIL";

    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    fn id(raw: u64) -> Id {
        Id::from_u64(raw)
    }

    fn injected() -> PriceLevelError {
        PriceLevelError::InvalidOperation {
            message: "injected level failure".to_string(),
        }
    }

    /// Book with generous risk limits and order-state tracking.
    fn tracked_book() -> OrderBook<()> {
        let mut book = OrderBook::<()>::new(SYMBOL);
        book.set_order_state_tracker(OrderStateTracker::with_capacity(1_000));
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(1_000));
        book
    }

    fn rest(book: &OrderBook<()>, raw: u64, price: u128, qty: u64, side: Side, owner: Hash32) {
        book.add_limit_order_with_user(id(raw), price, qty, side, TimeInForce::Gtc, owner, None)
            .expect("rest order");
    }

    /// Fails every level admission of `raw` whose zero-based attempt index
    /// is in `failing`, counting attempts in the returned counter.
    fn fail_admissions(book: &mut OrderBook<()>, raw: u64, failing: fn(usize) -> bool) {
        let attempts = Arc::new(AtomicUsize::new(0));
        let target = id(raw);
        book.rest_fault_hook = Some(Arc::new(move |order_id| {
            if order_id != target {
                return None;
            }
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            failing(attempt).then(injected)
        }));
    }

    fn fail_cancels(book: &mut OrderBook<()>, raw: u64, fault: fn() -> CancelFault) {
        let target = id(raw);
        book.cancel_fault_hook = Some(Arc::new(move |order_id| (order_id == target).then(fault)));
    }

    /// Every index agrees that `raw` rests: location, user index, risk.
    fn assert_indexed(book: &OrderBook<()>, raw: u64, owner: Hash32) {
        let order_id = id(raw);
        assert!(book.get_order(order_id).is_some(), "{order_id} rests");
        assert!(book.order_locations.contains_key(&order_id), "located");
        assert!(
            book.user_orders
                .get(&owner)
                .is_some_and(|ids| ids.contains(&order_id)),
            "in the user index"
        );
        assert!(book.risk_state.orders.contains_key(&order_id), "in risk");
    }

    /// No index holds `raw` any more: no orphan anywhere.
    fn assert_gone(book: &OrderBook<()>, raw: u64) {
        let order_id = id(raw);
        assert!(book.get_order(order_id).is_none(), "{order_id} gone");
        assert!(!book.order_locations.contains_key(&order_id), "unlocated");
        assert!(
            !book
                .user_orders
                .iter()
                .any(|entry| entry.value().contains(&order_id)),
            "not in any user index"
        );
        assert!(!book.risk_state.orders.contains_key(&order_id), "released");
    }

    fn open_count(book: &OrderBook<()>, owner: Hash32) -> u64 {
        book.risk_state
            .counters
            .get(&owner)
            .map(|c| c.open_count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn ask_ids_in_queue_order(book: &OrderBook<()>, price: u128) -> Vec<Id> {
        book.asks
            .get(&price)
            .expect("ask level")
            .value()
            .snapshot_by_insertion_seq()
            .expect("queue view")
            .iter()
            .map(|order| order.id())
            .collect()
    }

    // --- OrderUpdate::Cancel ------------------------------------------------

    #[test]
    fn test_update_cancel_success_records_state_and_releases_risk() {
        let book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        assert_eq!(open_count(&book, user(1)), 1);

        let cancelled = book
            .update_order(OrderUpdate::Cancel { order_id: id(1) })
            .expect("cancel");
        assert_eq!(cancelled.map(|order| order.id()), Some(id(1)));
        assert_gone(&book, 1);
        assert_eq!(open_count(&book, user(1)), 0, "risk released");
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            })
        );
        assert!(book.asks.is_empty(), "empty level removed");
    }

    #[test]
    fn test_update_cancel_refused_by_level_propagates_and_keeps_the_order() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        fail_cancels(&mut book, 1, || CancelFault::Refuse(injected()));

        let result = book.update_order(OrderUpdate::Cancel { order_id: id(1) });
        assert!(matches!(result, Err(OrderBookError::PriceLevelError(_))));
        assert_indexed(&book, 1, user(1));
        assert_eq!(book.order_status(id(1)), Some(OrderStatus::Open));
    }

    #[test]
    fn test_update_cancel_removed_then_failed_completes_the_removal() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        fail_cancels(&mut book, 1, || CancelFault::RemoveThenFail(injected()));

        let result = book.update_order(OrderUpdate::Cancel { order_id: id(1) });
        assert!(matches!(
            result,
            Err(OrderBookError::OrderRemovedWithLevelFault { order_id, .. }) if order_id == id(1)
        ));
        assert_gone(&book, 1);
        assert!(matches!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                reason: CancelReason::UserRequested,
                ..
            })
        ));
    }

    // --- cancel-then-add re-add failures ----------------------------------

    #[test]
    fn test_failed_readd_restores_the_original_at_the_back_of_its_level() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        rest(&book, 2, 100, 5, Side::Sell, user(2));
        let before = book.get_order(id(1)).expect("original");
        // The re-add (attempt 0) fails; the restore (attempt 1) succeeds.
        fail_admissions(&mut book, 1, |attempt| attempt == 0);

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(101),
        });
        assert!(matches!(
            &result,
            Err(OrderBookError::ModifyRolledBack { order_id, source })
                if *order_id == id(1)
                    && matches!(source.as_ref(), OrderBookError::PriceLevelError(_))
        ));

        // Same id, price, quantity and timestamp; indexed exactly once.
        let restored = book.get_order(id(1)).expect("restored");
        assert_eq!(restored.price(), before.price());
        assert_eq!(restored.total_quantity().ok(), Some(5));
        assert_eq!(restored.timestamp(), before.timestamp());
        assert_indexed(&book, 1, user(1));
        assert_eq!(open_count(&book, user(1)), 1, "one risk contribution");
        assert_eq!(book.order_status(id(1)), Some(OrderStatus::Open));
        // Time priority is lost: it now queues behind order 2.
        assert_eq!(ask_ids_in_queue_order(&book, 100), vec![id(2), id(1)]);
        // No phantom level at the refused price.
        assert!(book.asks.get(&101).is_none());
        assert_eq!(book.best_ask(), Some(100));
    }

    #[test]
    fn test_failed_readd_restores_a_partially_filled_original() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 10, Side::Sell, user(1));
        rest(&book, 9, 100, 4, Side::Buy, user(9));
        let prior = book.order_status(id(1));
        assert!(prior.is_some(), "tracked before the modify");
        fail_admissions(&mut book, 1, |attempt| attempt == 0);

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(105),
        });
        assert!(matches!(
            result,
            Err(OrderBookError::ModifyRolledBack { .. })
        ));
        let restored = book.get_order(id(1)).expect("restored");
        assert_eq!(
            restored.total_quantity().ok(),
            Some(6),
            "remaining quantity"
        );
        assert_eq!(book.order_status(id(1)), prior, "prior state restored");
        assert_indexed(&book, 1, user(1));
    }

    #[test]
    fn test_failed_readd_and_failed_restore_report_the_order_lost() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        fail_admissions(&mut book, 1, |_| true);

        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id: id(1),
            new_price: Price::new(101),
            new_quantity: Quantity::new(7),
        });
        assert!(matches!(
            &result,
            Err(OrderBookError::ModifyOrderLost {
                order_id,
                executed_quantity: 0,
                restore_error: Some(_),
                ..
            }) if *order_id == id(1)
        ));
        assert_gone(&book, 1);
        assert_eq!(open_count(&book, user(1)), 0);
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::RestFailed,
            })
        );
        assert!(book.asks.is_empty(), "no phantom level at either price");
    }

    #[test]
    fn test_readd_that_traded_then_failed_to_rest_reports_the_order_lost() {
        let mut book = tracked_book();
        rest(&book, 2, 99, 3, Side::Buy, user(2));
        rest(&book, 1, 100, 10, Side::Sell, user(1));
        fail_admissions(&mut book, 1, |_| true);

        // Re-priced to 99 it crosses the bid for 3; the remaining 7 cannot
        // rest, and the original cannot be restored because it traded.
        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(99),
        });
        assert!(matches!(
            &result,
            Err(OrderBookError::ModifyOrderLost {
                executed_quantity: 3,
                restore_error: None,
                ..
            })
        ));
        assert_gone(&book, 1);
        assert_gone(&book, 2);
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 3,
                reason: CancelReason::RestFailed,
            })
        );
        assert_eq!(
            book.order_status(id(2)),
            Some(OrderStatus::Filled { filled_quantity: 3 })
        );
        assert!(book.asks.is_empty() && book.bids.is_empty());
    }

    #[test]
    fn test_replace_readd_failure_rolls_back() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        fail_admissions(&mut book, 1, |attempt| attempt == 0);

        let result = book.update_order(OrderUpdate::Replace {
            order_id: id(1),
            price: Price::new(102),
            quantity: Quantity::new(8),
            side: Side::Sell,
        });
        assert!(matches!(
            result,
            Err(OrderBookError::ModifyRolledBack { .. })
        ));
        let restored = book.get_order(id(1)).expect("restored");
        assert_eq!(restored.price(), Price::new(100));
        assert_eq!(restored.total_quantity().ok(), Some(5));
        assert_indexed(&book, 1, user(1));
    }

    #[test]
    fn test_readd_as_fill_or_kill_is_rejected_before_the_cancel() {
        let book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        let fok = OrderType::Standard {
            id: id(1),
            price: Price::new(101),
            quantity: Quantity::new(5),
            side: Side::Sell,
            time_in_force: TimeInForce::Fok,
            user_id: user(1),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        };
        let snapshot = book.get_order(id(1)).expect("resting");
        let result = book.cancel_then_readd(id(1), &snapshot, fok, ReAddQuantity::Explicit);
        assert!(matches!(
            result,
            Err(OrderBookError::InvalidOperation { .. })
        ));
        assert_indexed(&book, 1, user(1));
        assert_eq!(book.order_status(id(1)), Some(OrderStatus::Open));
    }

    // --- submit remainder that cannot rest -----------------------------------

    #[test]
    fn test_submit_remainder_rest_failure_after_trades_is_terminal() {
        let mut book = tracked_book();
        rest(&book, 2, 100, 3, Side::Sell, user(2));
        fail_admissions(&mut book, 1, |_| true);

        let result = book.add_limit_order_with_user(
            id(1),
            100,
            10,
            Side::Buy,
            TimeInForce::Gtc,
            user(1),
            None,
        );
        assert!(matches!(result, Err(OrderBookError::PriceLevelError(_))));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 3,
                reason: CancelReason::RestFailed,
            })
        );
        assert_gone(&book, 1);
        assert!(book.bids.is_empty(), "no phantom bid level");
    }

    #[test]
    fn test_submit_rest_failure_without_trades_is_rejected() {
        let mut book = tracked_book();
        fail_admissions(&mut book, 1, |_| true);

        let result = book.add_limit_order_with_user(
            id(1),
            100,
            10,
            Side::Buy,
            TimeInForce::Gtc,
            user(1),
            None,
        );
        assert!(matches!(result, Err(OrderBookError::PriceLevelError(_))));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::Other(0),
            })
        );
        assert_gone(&book, 1);
        assert!(book.bids.is_empty());
    }

    // --- STP maker cancel failures ------------------------------------------

    fn stp_book(mode: STPMode) -> OrderBook<()> {
        let mut book = tracked_book();
        book.set_stp_mode(mode);
        book
    }

    #[test]
    fn test_stp_cancel_maker_refused_aborts_the_sweep() {
        let mut book = stp_book(STPMode::CancelMaker);
        rest(&book, 12, 99, 2, Side::Sell, user(2));
        rest(&book, 10, 100, 5, Side::Sell, user(1));
        rest(&book, 11, 100, 5, Side::Sell, user(2));
        fail_cancels(&mut book, 10, || CancelFault::Refuse(injected()));

        let result = book.add_limit_order_with_user(
            id(20),
            100,
            6,
            Side::Buy,
            TimeInForce::Gtc,
            user(1),
            None,
        );
        assert!(matches!(
            result,
            Err(OrderBookError::MatchAborted {
                executed_quantity: 2,
                trade_count: 1,
                ..
            })
        ));
        // The refused maker still rests and is tracked; the level behind the
        // failure was not touched.
        assert_indexed(&book, 10, user(1));
        assert_eq!(book.order_status(id(10)), Some(OrderStatus::Open));
        assert_indexed(&book, 11, user(2));
        assert_eq!(
            book.order_status(id(20)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 2,
                reason: CancelReason::MatchAborted,
            })
        );
        assert_gone(&book, 20);
    }

    #[test]
    fn test_stp_cancel_maker_removed_then_failed_completes_and_aborts() {
        let mut book = stp_book(STPMode::CancelMaker);
        rest(&book, 10, 100, 5, Side::Sell, user(1));
        rest(&book, 11, 100, 5, Side::Sell, user(2));
        fail_cancels(&mut book, 10, || CancelFault::RemoveThenFail(injected()));

        let result = book.add_limit_order_with_user(
            id(20),
            100,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            user(1),
            None,
        );
        assert!(matches!(
            result,
            Err(OrderBookError::MatchAborted {
                executed_quantity: 0,
                ..
            })
        ));
        assert_gone(&book, 10);
        assert_eq!(
            book.order_status(id(10)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            })
        );
        assert_indexed(&book, 11, user(2));
        assert_gone(&book, 20);
    }

    #[test]
    fn test_stp_cancel_both_maker_refused_aborts_the_sweep() {
        let mut book = stp_book(STPMode::CancelBoth);
        rest(&book, 10, 100, 5, Side::Sell, user(1));
        fail_cancels(&mut book, 10, || CancelFault::Refuse(injected()));

        let result = book.add_limit_order_with_user(
            id(20),
            100,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            user(1),
            None,
        );
        assert!(matches!(result, Err(OrderBookError::MatchAborted { .. })));
        assert_indexed(&book, 10, user(1));
        assert_eq!(book.order_status(id(10)), Some(OrderStatus::Open));
        assert_gone(&book, 20);
    }

    // --- concurrent mutations inside the modify (review of #285) ------------

    fn standard(raw: u64, price: u128, qty: u64, side: Side, owner: Hash32) -> OrderType<()> {
        OrderType::Standard {
            id: id(raw),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            time_in_force: TimeInForce::Gtc,
            user_id: owner,
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    /// Runs `action` once, the first time a cancel-then-add modify reaches
    /// `phase`. The action admits orders through the ungated inner path:
    /// the modify already holds the submit gate.
    fn on_modify(book: &mut OrderBook<()>, phase: ModifyPhase, action: fn(&OrderBook<()>)) {
        let fired = Arc::new(AtomicUsize::new(0));
        book.modify_interleave_hook = Some(Arc::new(move |book, _, at| {
            if at == phase && fired.fetch_add(1, Ordering::SeqCst) == 0 {
                action(book);
            }
        }));
    }

    fn admit(book: &OrderBook<()>, order: OrderType<()>) {
        book.add_order_inner(order, false, false, Admission::Submit)
            .map(|_| ())
            .map_err(|failure| failure.into_submit().error)
            .expect("concurrent admission");
    }

    /// MUST 1: an opposite order that lands between the cancel and the
    /// restore at the original's price must not let the engine rest the
    /// original into a locked book.
    #[test]
    fn test_restore_that_would_lock_the_book_is_refused() {
        let mut book = tracked_book();
        let bid = OrderType::PostOnly {
            id: id(1),
            price: Price::new(100),
            quantity: Quantity::new(5),
            side: Side::Buy,
            user_id: user(1),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        };
        book.add_order(bid).expect("post-only bid rests");
        // After the cancel a sell rests at 100: the post-only re-add at 101
        // now crosses (no trade), and restoring the bid at 100 would lock.
        on_modify(&mut book, ModifyPhase::AfterCancel, |book| {
            admit(book, standard(2, 100, 5, Side::Sell, user(2)));
        });

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(101),
        });
        assert!(matches!(
            &result,
            Err(OrderBookError::ModifyOrderLost {
                executed_quantity: 0,
                source,
                restore_error: Some(restore),
                ..
            }) if matches!(source.as_ref(), OrderBookError::PriceCrossing { .. })
                && matches!(restore.as_ref(), OrderBookError::PriceCrossing { .. })
        ));
        assert_gone(&book, 1);
        assert!(book.bids.is_empty(), "nothing rests at the locking price");
        assert_eq!(book.best_ask(), Some(100));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::RestFailed,
            })
        );
    }

    /// MUST 3: a taker fills 4 of 10 between the modify's read and its
    /// cancel. `UpdatePrice` re-adds the cancelled remainder (6), never the
    /// 10 it read, and the state counts the raced fill.
    #[test]
    fn test_update_price_readds_the_remainder_after_a_concurrent_fill() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 10, Side::Sell, user(1));
        on_modify(&mut book, ModifyPhase::BeforeCancel, |book| {
            admit(book, standard(9, 100, 4, Side::Buy, user(9)));
        });

        let moved = book
            .update_order(OrderUpdate::UpdatePrice {
                order_id: id(1),
                new_price: Price::new(101),
            })
            .expect("re-priced")
            .expect("order found");
        assert_eq!(moved.total_quantity().ok(), Some(6), "no quantity created");
        assert_eq!(moved.price(), Price::new(101));
        assert_eq!(
            book.asks.get(&101).map(|l| l.value().visible_quantity()),
            Some(6)
        );
        assert!(book.asks.get(&100).is_none());
        assert_indexed(&book, 1, user(1));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::PartiallyFilled {
                original_quantity: 10,
                filled_quantity: 4,
            })
        );
    }

    /// MUST 3: with an explicit new quantity the modify is not applied to
    /// an order that changed under it; the remainder is restored.
    #[test]
    fn test_explicit_quantity_modify_after_a_concurrent_fill_rolls_back() {
        for update in [
            OrderUpdate::Replace {
                order_id: id(1),
                price: Price::new(101),
                quantity: Quantity::new(8),
                side: Side::Sell,
            },
            OrderUpdate::UpdatePriceAndQuantity {
                order_id: id(1),
                new_price: Price::new(101),
                new_quantity: Quantity::new(8),
            },
        ] {
            let mut book = tracked_book();
            rest(&book, 1, 100, 10, Side::Sell, user(1));
            on_modify(&mut book, ModifyPhase::BeforeCancel, |book| {
                admit(book, standard(9, 100, 4, Side::Buy, user(9)));
            });

            let result = book.update_order(update);
            assert!(matches!(
                &result,
                Err(OrderBookError::ModifyRolledBack { source, .. })
                    if matches!(
                        source.as_ref(),
                        OrderBookError::OrderChangedDuringModify {
                            read_quantity: 10,
                            cancelled_quantity: 6,
                            ..
                        }
                    )
            ));
            let restored = book.get_order(id(1)).expect("restored");
            assert_eq!(restored.price(), Price::new(100));
            assert_eq!(
                restored.total_quantity().ok(),
                Some(6),
                "no quantity created"
            );
            assert!(book.asks.get(&101).is_none());
            assert_indexed(&book, 1, user(1));
        }
    }

    /// MUST 2: a lost order's terminal `filled_quantity` counts the
    /// original's fills (4 of 10 as a taker) plus the re-add's (3).
    #[test]
    fn test_lost_order_terminal_state_is_cumulative() {
        let mut book = tracked_book();
        rest(&book, 9, 100, 4, Side::Buy, user(9));
        rest(&book, 1, 100, 10, Side::Sell, user(1));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::PartiallyFilled {
                original_quantity: 10,
                filled_quantity: 4,
            })
        );
        rest(&book, 2, 99, 3, Side::Buy, user(2));
        fail_admissions(&mut book, 1, |_| true);

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(99),
        });
        assert!(matches!(
            &result,
            Err(OrderBookError::ModifyOrderLost {
                executed_quantity: 3,
                restore_error: None,
                ..
            })
        ));
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 7,
                reason: CancelReason::RestFailed,
            })
        );
        assert_gone(&book, 1);
    }

    /// A successful re-price keeps the original's fill history instead of
    /// resetting it to `Open`.
    #[test]
    fn test_readd_keeps_the_fill_history() {
        let book = tracked_book();
        rest(&book, 9, 100, 4, Side::Buy, user(9));
        rest(&book, 1, 100, 10, Side::Sell, user(1));
        book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(105),
        })
        .expect("re-priced");
        assert_eq!(
            book.order_status(id(1)),
            Some(OrderStatus::PartiallyFilled {
                original_quantity: 10,
                filled_quantity: 4,
            })
        );
    }

    /// SHOULD: a rolled-back modify records no `Rejected` state for the
    /// failed re-add. Since #288 an order's resting state is recorded
    /// before its level admits it (so a concurrent sweep's `Filled` can
    /// never precede it), so a re-add the level refuses leaves its `Open`
    /// in the history ahead of the restore's.
    #[test]
    fn test_rollback_records_no_rejection_for_the_readd() {
        let mut book = tracked_book();
        rest(&book, 1, 100, 5, Side::Sell, user(1));
        fail_admissions(&mut book, 1, |attempt| attempt == 0);

        let result = book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(101),
        });
        assert!(matches!(
            result,
            Err(OrderBookError::ModifyRolledBack { .. })
        ));
        let history: Vec<OrderStatus> = book
            .order_state_tracker
            .as_ref()
            .and_then(|t| t.get_history(id(1)))
            .expect("history")
            .into_iter()
            .map(|(_, status)| status)
            .collect();
        assert_eq!(
            history,
            vec![
                OrderStatus::Open,
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::UserRequested,
                },
                // The re-add, recorded before the level refused it.
                OrderStatus::Open,
                // The restore.
                OrderStatus::Open,
            ]
        );
    }

    // --- replay ------------------------------------------------------------------

    /// A journaled `ModifyRolledBack` changed the live book (the original
    /// moved to the back of its level), so replay re-executes it instead of
    /// skipping it. The injected failure does not reproduce on the replay
    /// book, so the update succeeds there and replay stops loudly with
    /// `OutcomeMismatch` instead of silently diverging.
    #[test]
    fn test_rolled_back_modify_is_reexecuted_on_replay() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut live = OrderBook::<()>::with_clock(SYMBOL, Arc::clone(&clock));
        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        let append = |seq: u64, command: SequencerCommand<()>, result: SequencerResult| {
            journal
                .append(&SequencerEvent {
                    sequence_num: seq,
                    timestamp_ns: seq,
                    command,
                    result,
                })
                .expect("append");
        };

        let maker = OrderType::Standard {
            id: id(1),
            price: Price::new(100),
            quantity: Quantity::new(5),
            side: Side::Sell,
            time_in_force: TimeInForce::Gtc,
            user_id: user(1),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        };
        live.add_order(maker).expect("rest");
        append(
            0,
            SequencerCommand::AddOrder(maker),
            SequencerResult::OrderAdded { order_id: id(1) },
        );

        fail_admissions(&mut live, 1, |attempt| attempt == 0);
        let update = OrderUpdate::UpdatePrice {
            order_id: id(1),
            new_price: Price::new(101),
        };
        let err = live.update_order(update).expect_err("rolled back");
        let recorded = SequencerResult::from(&err);
        assert!(matches!(
            recorded,
            SequencerResult::RejectedWithCode {
                code: RejectReason::ModifyRolledBack,
                may_have_mutated: true,
                ..
            }
        ));
        append(1, SequencerCommand::UpdateOrder(update), recorded);

        let replayed = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            clock,
            &Default::default(),
        );
        assert!(matches!(
            replayed,
            Err(ReplayError::OutcomeMismatch {
                sequence_num: 1,
                recorded: RejectReason::ModifyRolledBack,
                actual: None,
            })
        ));
    }
}
