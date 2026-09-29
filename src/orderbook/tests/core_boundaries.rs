//! #294: core boundary gaps found by the final audit.
//!
//! - an unwind under the **shared** side of the submit gate (a panicking
//!   `Clock`) engages the kill switch and latches `submit_gate_poisoned`,
//!   like one under the exclusive side;
//! - a sweep whose drain unwinds (a panicking `Clock` while recording a
//!   maker's `Filled` state) leaves no ghost location or user-index entry
//!   for the makers it consumed;
//! - a standalone `OrderStateTracker` whose listener panics still queues
//!   the terminal id for eviction;
//! - a rest path that unwinds between the location claim and the level's
//!   admission withdraws everything it published (location, user index,
//!   risk reservation, resting state, an empty level it created).

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects, clippy::manual_assert)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::clock::Clock;
    use crate::orderbook::order_state::{OrderStateTracker, OrderStatus};
    use crate::orderbook::risk::RiskConfig;
    use crate::{OrderBookError, current_time_millis};
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;

    /// A clock that panics while armed, otherwise counts up.
    #[derive(Debug, Default)]
    struct ArmedClock {
        armed: AtomicBool,
        ticks: AtomicU64,
    }

    impl Clock for ArmedClock {
        fn now_millis(&self) -> TimestampMs {
            if self.armed.load(Ordering::SeqCst) {
                panic!("injected Clock panic");
            }
            TimestampMs::new(self.ticks.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    fn limit(id: u64, price: u128, qty: u64, side: Side, user: u8) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(id),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            time_in_force: TimeInForce::Gtc,
            user_id: Hash32::new([user; 32]),
            timestamp: TimestampMs::new(current_time_millis()),
            extra_fields: (),
        }
    }

    /// A book whose order-state tracker reads `clock`.
    fn book_with_clock(clock: &Arc<ArmedClock>) -> OrderBook<()> {
        let mut book = OrderBook::<()>::new("B294");
        book.set_order_state_tracker(OrderStateTracker::with_clock(
            Arc::clone(clock) as Arc<dyn Clock>
        ));
        book
    }

    /// A `Clock` panicking under the shared gate (an ordinary GTC add on an
    /// `STPMode::None` book) used to leave no trace: a read guard never
    /// poisons. The unwinding guard now engages the kill switch and latches
    /// the flag; new flow is rejected, cancels still drain the book.
    #[test]
    fn test_shared_gate_clock_panic_engages_kill_switch() {
        let clock = Arc::new(ArmedClock::default());
        let book = Arc::new(book_with_clock(&clock));
        book.add_order(limit(1, 90, 5, Side::Buy, 1))
            .expect("resting bid");
        assert!(
            !book.submit_needs_exclusive_gate(false, Hash32::new([1; 32]), false, false),
            "the panicking submit runs under the shared side"
        );

        clock.armed.store(true, Ordering::SeqCst);
        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 80, 1, Side::Buy, 1));
        })
        .join();
        assert!(joined.is_err(), "the Clock panic propagated");
        clock.armed.store(false, Ordering::SeqCst);

        assert!(book.is_kill_switch_engaged());
        assert!(book.submit_gate_poisoned());
        assert!(
            !book.submit_gate.is_poisoned(),
            "the shared side never poisons"
        );

        let err = book
            .add_order(limit(3, 85, 1, Side::Buy, 1))
            .expect_err("new flow rejected");
        assert!(matches!(err, OrderBookError::KillSwitchActive), "{err:?}");
        let err = book
            .submit_market_order(Id::from_u64(4), 1, Side::Sell)
            .expect_err("market flow rejected");
        assert!(matches!(err, OrderBookError::KillSwitchActive), "{err:?}");
        assert!(
            book.cancel_order(Id::from_u64(1))
                .expect("cancel runs")
                .is_some()
        );

        // An operator release resumes flow; the latch stays.
        book.release_kill_switch();
        book.add_order(limit(5, 70, 1, Side::Buy, 1))
            .expect("flow resumes");
        assert!(book.submit_gate_poisoned());
    }

    /// A clean submit neither engages the kill switch nor latches.
    #[test]
    fn test_shared_gate_without_panic_does_not_latch() {
        let clock = Arc::new(ArmedClock::default());
        let book = book_with_clock(&clock);
        book.add_order(limit(1, 100, 5, Side::Sell, 1))
            .expect("maker");
        book.add_order(limit(2, 100, 5, Side::Buy, 2))
            .expect("taker");
        assert!(!book.is_kill_switch_engaged());
        assert!(!book.submit_gate_poisoned());
    }

    /// The drain records each filled maker's `Filled` state (reading the
    /// tracker's `Clock`) before releasing its location. A `Clock` panic
    /// on the first maker used to leave every consumed maker located and
    /// user-indexed although no level holds it (ghost locations). The
    /// release guard now releases them while the drain unwinds.
    #[test]
    fn test_drain_unwind_leaves_no_ghost_location() {
        let clock = Arc::new(ArmedClock::default());
        let mut book = book_with_clock(&clock);
        // Arm the clock once the sweep reaches the level: the next clock
        // read is the drain's first `Filled`.
        let arm = Arc::clone(&clock);
        book.level_interleave_hook = Some(Arc::new(move |price: u128| {
            if price == 100 {
                arm.armed.store(true, Ordering::SeqCst);
            }
        }));
        let book = Arc::new(book);
        for id in 1..=3 {
            book.add_order(limit(id, 100, 5, Side::Sell, 1))
                .expect("maker");
        }
        book.add_order(limit(10, 110, 5, Side::Sell, 1))
            .expect("untouched maker");

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(4, 100, 15, Side::Buy, 2));
        })
        .join();
        assert!(joined.is_err(), "the Clock panic propagated");
        clock.armed.store(false, Ordering::SeqCst);

        for id in 1..=3 {
            let id = Id::from_u64(id);
            assert!(
                !book.order_locations.contains_key(&id),
                "maker {id} left a ghost location"
            );
            assert!(
                !book
                    .user_orders
                    .iter()
                    .any(|entry| entry.value().contains(&id)),
                "maker {id} left a ghost user-index entry"
            );
            assert!(book.get_order(id).is_none());
        }
        assert!(book.asks.get(&100).is_none(), "the emptied level is gone");
        assert!(!book.order_locations.contains_key(&Id::from_u64(4)));
        // The untouched maker keeps its indices.
        assert!(book.order_locations.contains_key(&Id::from_u64(10)));
        assert!(book.get_order(Id::from_u64(10)).is_some());
        // The unwind went through the (shared) gate: kill switch engaged.
        assert!(book.is_kill_switch_engaged());
        assert!(book.submit_gate_poisoned());
    }

    /// The drain's normal path is unchanged: every filled maker is
    /// released and recorded `Filled`, in order.
    #[test]
    fn test_drain_releases_every_filled_maker() {
        let clock = Arc::new(ArmedClock::default());
        let book = book_with_clock(&clock);
        for id in 1..=3 {
            book.add_order(limit(id, 100, 5, Side::Sell, 1))
                .expect("maker");
        }
        book.add_order(limit(4, 100, 15, Side::Buy, 2))
            .expect("taker");
        let tracker = book.order_state_tracker.as_ref().expect("tracker");
        for id in 1..=3 {
            let id = Id::from_u64(id);
            assert!(!book.order_locations.contains_key(&id));
            assert!(matches!(
                tracker.get(id),
                Some(OrderStatus::Filled { filled_quantity: 5 })
            ));
        }
        assert!(book.user_orders.is_empty());
        assert!(!book.submit_gate_poisoned());
    }

    /// A standalone tracker's listener used to run before the terminal id
    /// was queued for eviction, so a panicking listener left that id
    /// retained forever. It is now queued first and evicted on schedule.
    #[test]
    fn test_standalone_tracker_listener_panic_still_evicts() {
        let mut tracker = OrderStateTracker::with_capacity(1);
        tracker.set_listener(Arc::new(|id: Id, _old: &OrderStatus, new: &OrderStatus| {
            if id == Id::from_u64(1) && new.is_terminal() {
                panic!("injected listener panic");
            }
        }));
        let tracker = Arc::new(tracker);
        tracker.transition(Id::from_u64(1), OrderStatus::Open);

        let panicking = Arc::clone(&tracker);
        let joined = thread::spawn(move || {
            panicking.transition(Id::from_u64(1), OrderStatus::Filled { filled_quantity: 1 });
        })
        .join();
        assert!(joined.is_err(), "the listener panic propagated");
        assert!(
            tracker.get(Id::from_u64(1)).is_some(),
            "retained within capacity"
        );

        // A second terminal id exceeds the capacity of 1: the first one,
        // queued before its listener panicked, is evicted.
        tracker.transition(Id::from_u64(2), OrderStatus::Filled { filled_quantity: 1 });
        assert!(tracker.get(Id::from_u64(1)).is_none(), "evicted");
        assert!(tracker.get(Id::from_u64(2)).is_some());
    }

    /// A `Clock` panic while the rest path records the resting state (the
    /// location and user entry already claimed, the risk reservation
    /// taken) used to leave them all behind for an order no level holds.
    #[test]
    fn test_rest_clock_panic_withdraws_claim() {
        let clock = Arc::new(ArmedClock::default());
        let mut book = book_with_clock(&clock);
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(2));
        let book = Arc::new(book);
        book.add_order(limit(1, 90, 5, Side::Buy, 1))
            .expect("resting bid");

        clock.armed.store(true, Ordering::SeqCst);
        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 80, 1, Side::Buy, 1));
        })
        .join();
        assert!(joined.is_err(), "the Clock panic propagated");
        clock.armed.store(false, Ordering::SeqCst);

        let id = Id::from_u64(2);
        assert!(!book.order_locations.contains_key(&id), "ghost location");
        assert!(
            !book
                .user_orders
                .iter()
                .any(|entry| entry.value().contains(&id)),
            "ghost user-index entry"
        );
        let tracker = book.order_state_tracker.as_ref().expect("tracker");
        assert!(tracker.get(id).is_none(), "no state was recorded");
        assert!(book.bids.get(&80).is_none(), "no level");
        assert!(book.is_kill_switch_engaged());

        // The reservation was released: the account's second slot is free
        // again, and the third is still refused.
        book.release_kill_switch();
        book.add_order(limit(3, 85, 1, Side::Buy, 1))
            .expect("second open order fits");
        let err = book
            .add_order(limit(4, 84, 1, Side::Buy, 1))
            .expect_err("third open order refused");
        assert!(
            matches!(err, OrderBookError::RiskMaxOpenOrders { .. }),
            "{err:?}"
        );
    }

    /// A panic inside the level admission (after the resting state was
    /// recorded and the level created) withdraws the state and removes the
    /// empty level too.
    #[test]
    fn test_rest_admission_panic_withdraws_state_and_level() {
        let clock = Arc::new(ArmedClock::default());
        let mut book = book_with_clock(&clock);
        book.rest_fault_hook = Some(Arc::new(|id: Id| {
            if id == Id::from_u64(2) {
                panic!("injected admission panic");
            }
            None
        }));
        let book = Arc::new(book);
        book.add_order(limit(1, 90, 5, Side::Buy, 1))
            .expect("resting bid");

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 80, 1, Side::Buy, 1));
        })
        .join();
        assert!(joined.is_err(), "the admission panic propagated");

        let id = Id::from_u64(2);
        assert!(!book.order_locations.contains_key(&id), "ghost location");
        assert!(
            !book
                .user_orders
                .iter()
                .any(|entry| entry.value().contains(&id)),
            "ghost user-index entry"
        );
        let tracker = book.order_state_tracker.as_ref().expect("tracker");
        assert!(tracker.get(id).is_none(), "resting state withdrawn");
        assert!(book.bids.get(&80).is_none(), "empty level removed");
        assert_eq!(book.best_bid(), Some(90));
        // The earlier order is untouched.
        assert!(book.order_locations.contains_key(&Id::from_u64(1)));
        assert!(matches!(
            tracker.get(Id::from_u64(1)),
            Some(OrderStatus::Open)
        ));
        assert!(book.is_kill_switch_engaged());
    }

    /// `withdraw_last_transition` restores the previous status, forgets an
    /// order whose only transition it removes, and ignores a mismatch.
    #[test]
    fn test_withdraw_last_transition() {
        let tracker = OrderStateTracker::new();
        let id = Id::from_u64(7);
        let partial = OrderStatus::PartiallyFilled {
            original_quantity: 10,
            filled_quantity: 4,
        };
        tracker.transition(id, OrderStatus::Open);
        tracker.transition(id, partial.clone());

        tracker.withdraw_last_transition(id, &OrderStatus::Open);
        assert_eq!(tracker.get(id), Some(partial.clone()), "mismatch ignored");

        tracker.withdraw_last_transition(id, &partial);
        assert_eq!(tracker.get(id), Some(OrderStatus::Open));
        assert_eq!(tracker.get_history(id).map(|h| h.len()), Some(1));

        tracker.withdraw_last_transition(id, &OrderStatus::Open);
        assert!(tracker.get(id).is_none(), "only transition removed");
        tracker.withdraw_last_transition(id, &OrderStatus::Open);
    }

    fn panicking_commit_hook() {
        panic!("injected panic in the gate guard's commit phase");
    }

    /// PR #297 review: the gate guard's commit phase runs under the held
    /// gate inside the guard's drop and can run caller code (`tracing`). A
    /// panic there does not run the drop again and a shared guard does not
    /// poison; the commit sentinel engages the kill switch and latches.
    #[test]
    fn test_commit_phase_panic_under_shared_gate_latches() {
        let mut book = OrderBook::<()>::new("B294C");
        // A price-level listener: every resting add commits a level event.
        book.set_price_level_listener(Arc::new(
            |_: crate::orderbook::book_change_event::PriceLevelChangedEvent| {},
        ));
        let book = Arc::new(book);
        book.add_order(limit(1, 90, 5, Side::Buy, 1))
            .expect("resting bid");
        assert!(!book.is_kill_switch_engaged());

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            crate::orderbook::emission::commit_seam::set(Some(panicking_commit_hook));
            let _ = panicking.add_order(limit(2, 100, 5, Side::Sell, 2));
        })
        .join();
        assert!(joined.is_err(), "the commit-phase panic propagated");

        assert!(book.is_kill_switch_engaged());
        assert!(book.submit_gate_poisoned());
        assert!(!book.submit_gate.is_poisoned(), "shared side");
        let err = book
            .add_order(limit(3, 85, 1, Side::Buy, 1))
            .expect_err("new flow rejected");
        assert!(matches!(err, OrderBookError::KillSwitchActive), "{err:?}");
        assert!(
            book.cancel_order(Id::from_u64(1))
                .expect("cancel runs")
                .is_some()
        );
    }
}
