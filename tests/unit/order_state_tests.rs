//! Integration tests for order state machine tracking through the OrderBook API.

#[cfg(test)]
mod tests_order_state {
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::{DefaultOrderBook, OrderBook};
    use pricelevel::{Hash32, Id, Side, TimeInForce};
    use std::sync::{Arc, Mutex};

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    /// Create a book with state tracking enabled.
    fn book_with_tracker(symbol: &str) -> OrderBook<()> {
        let mut book = OrderBook::new(symbol);
        book.set_order_state_tracker(OrderStateTracker::new());
        book
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Open → Filled lifecycle
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn add_order_no_match_tracks_open() {
        let book = book_with_tracker("TEST");
        let id = new_id();
        let result = book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None);
        assert!(result.is_ok());

        let status = book.order_status(id);
        assert_eq!(status, Some(OrderStatus::Open));
    }

    #[test]
    fn add_order_fully_matched_tracks_filled() {
        let book = book_with_tracker("TEST");

        // Place a resting ask
        let ask_id = new_id();
        book.add_limit_order(ask_id, 100, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");
        assert_eq!(book.order_status(ask_id), Some(OrderStatus::Open));

        // Place an aggressive buy that fully matches the ask
        let bid_id = new_id();
        book.add_limit_order(bid_id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add bid");

        // The incoming buy should be Filled (fully matched immediately)
        assert_eq!(
            book.order_status(bid_id),
            Some(OrderStatus::Filled {
                filled_quantity: 10
            })
        );

        // The resting ask should be Filled (consumed by matching)
        let ask_status = book.order_status(ask_id);
        assert!(
            matches!(ask_status, Some(OrderStatus::Filled { .. })),
            "resting ask should be Filled, got: {ask_status:?}"
        );
    }

    #[test]
    fn add_order_partial_match_tracks_partially_filled() {
        let book = book_with_tracker("TEST");

        // Place a small resting ask
        let ask_id = new_id();
        book.add_limit_order(ask_id, 100, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");

        // Place a larger buy that partially matches
        let bid_id = new_id();
        book.add_limit_order(bid_id, 100, 15, Side::Buy, TimeInForce::Gtc, None)
            .expect("add bid");

        // The buy rests with remaining quantity, should be PartiallyFilled
        let bid_status = book.order_status(bid_id);
        assert!(
            matches!(
                bid_status,
                Some(OrderStatus::PartiallyFilled {
                    original_quantity: 15,
                    filled_quantity: 5,
                })
            ),
            "bid should be PartiallyFilled, got: {bid_status:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Rejection tracking
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn rejected_order_tick_size() {
        let mut book = book_with_tracker("TEST");
        book.set_tick_size(100);

        let id = new_id();
        let result = book.add_limit_order(id, 150, 10, Side::Buy, TimeInForce::Gtc, None);
        assert!(result.is_err());

        let status = book.order_status(id);
        assert!(
            matches!(status, Some(OrderStatus::Rejected { .. })),
            "should be Rejected, got: {status:?}"
        );
    }

    #[test]
    fn rejected_order_post_only_crossing() {
        use pricelevel::{OrderType, Price, Quantity, TimestampMs};

        let book = book_with_tracker("TEST");

        // Place a resting ask at 100
        book.add_limit_order(new_id(), 100, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");

        // Post-only buy at 100 would cross → rejected
        let id = new_id();
        let post_only = OrderType::PostOnly {
            id,
            price: Price::new(100),
            quantity: Quantity::new(5),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        };
        let result = book.add_order(post_only);
        assert!(result.is_err());

        let status = book.order_status(id);
        assert!(
            matches!(status, Some(OrderStatus::Rejected { .. })),
            "post-only crossing should be Rejected, got: {status:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Cancel tracking
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn cancel_order_tracks_user_requested() {
        let book = book_with_tracker("TEST");
        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");

        let result = book.cancel_order(id);
        assert!(result.is_ok());

        let status = book.order_status(id);
        assert_eq!(
            status,
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            })
        );
    }

    #[test]
    fn cancel_partially_filled_preserves_filled_quantity() {
        let book = book_with_tracker("TEST");

        // Resting ask
        book.add_limit_order(new_id(), 100, 3, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");

        // Buy partially matches
        let bid_id = new_id();
        book.add_limit_order(bid_id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add bid");

        // Cancel the remaining resting buy
        book.cancel_order(bid_id).expect("cancel");

        let status = book.order_status(bid_id);
        assert_eq!(
            status,
            Some(OrderStatus::Cancelled {
                filled_quantity: 3,
                reason: CancelReason::UserRequested,
            })
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Mass cancel tracking
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn mass_cancel_all_tracks_correct_reason() {
        let book = book_with_tracker("TEST");
        let id1 = new_id();
        let id2 = new_id();
        book.add_limit_order(id1, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id2, 200, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("add");

        let _ = book.cancel_all_orders();

        assert_eq!(
            book.order_status(id1),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelAll,
            })
        );
        assert_eq!(
            book.order_status(id2),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelAll,
            })
        );
    }

    #[test]
    fn mass_cancel_by_side_tracks_correct_reason() {
        let book = book_with_tracker("TEST");
        let bid_id = new_id();
        let ask_id = new_id();
        book.add_limit_order(bid_id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(ask_id, 200, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("add");

        let _ = book.cancel_orders_by_side(Side::Buy);

        assert_eq!(
            book.order_status(bid_id),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelBySide,
            })
        );
        // Ask should still be Open
        assert_eq!(book.order_status(ask_id), Some(OrderStatus::Open));
    }

    #[test]
    fn mass_cancel_by_price_range_tracks_correct_reason() {
        let book = book_with_tracker("TEST");
        let id1 = new_id();
        let id2 = new_id();
        let id3 = new_id();
        book.add_limit_order(id1, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id2, 200, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id3, 300, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");

        let _ = book.cancel_orders_by_price_range(Side::Buy, 100, 200);

        assert_eq!(
            book.order_status(id1),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelByPriceRange,
            })
        );
        assert_eq!(
            book.order_status(id2),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelByPriceRange,
            })
        );
        // id3 at 300 should still be Open
        assert_eq!(book.order_status(id3), Some(OrderStatus::Open));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // IOC / FOK tracking
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn fok_insufficient_liquidity_tracks_cancelled() {
        let book = book_with_tracker("TEST");

        // No liquidity at all
        let id = new_id();
        let result = book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Fok, None);
        assert!(result.is_err());

        let status = book.order_status(id);
        assert_eq!(
            status,
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::InsufficientLiquidity,
            })
        );
    }

    #[test]
    fn ioc_partial_fill_tracks_cancelled_with_filled_qty() {
        let book = book_with_tracker("TEST");

        // Small resting ask
        book.add_limit_order(new_id(), 100, 3, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");

        // IOC buy for 10 — only 3 can fill, rest is cancelled
        let id = new_id();
        let result = book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Ioc, None);
        assert!(result.is_err()); // IOC returns Err when not fully filled

        let status = book.order_status(id);
        assert_eq!(
            status,
            Some(OrderStatus::Cancelled {
                filled_quantity: 3,
                reason: CancelReason::InsufficientLiquidity,
            })
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // No tracker configured — zero overhead
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn no_tracker_returns_none_and_no_panic() {
        let book = DefaultOrderBook::new("TEST");
        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");

        // No tracker → order_status always returns None
        assert!(book.order_status(id).is_none());
        assert!(book.order_state_tracker().is_none());

        // Operations should work without panicking
        book.cancel_order(id).expect("cancel");
        assert!(book.order_status(id).is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Listener fires on transitions
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn listener_fires_on_every_transition() {
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let transitions_clone = Arc::clone(&transitions);

        let mut tracker = OrderStateTracker::new();
        tracker.set_listener(Arc::new(move |id, old, new| {
            if let Ok(mut t) = transitions_clone.lock() {
                t.push((id, old.clone(), new.clone()));
            }
        }));

        let mut book: OrderBook<()> = OrderBook::new("TEST");
        book.set_order_state_tracker(tracker);

        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.cancel_order(id).expect("cancel");

        let t = transitions.lock().expect("lock");
        assert_eq!(t.len(), 2, "expected 2 transitions, got {}", t.len());

        // First: Open
        assert_eq!(t[0].0, id);
        assert_eq!(t[0].2, OrderStatus::Open);

        // Second: Cancelled
        assert_eq!(t[1].0, id);
        assert!(matches!(t[1].2, OrderStatus::Cancelled { .. }));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // order_status query for unknown orders
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn order_status_unknown_returns_none() {
        let book = book_with_tracker("TEST");
        assert!(book.order_status(new_id()).is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // STP tracking (requires STP-enabled book)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn stp_cancel_taker_tracks_cancelled() {
        use orderbook_rs::orderbook::stp::STPMode;

        let mut book = OrderBook::<()>::with_stp_mode("STP", STPMode::CancelTaker);
        book.set_order_state_tracker(OrderStateTracker::new());

        let user = Hash32::from([1u8; 32]);

        // Resting ask from user
        let ask_id = new_id();
        book.add_limit_order_with_user(ask_id, 100, 10, Side::Sell, TimeInForce::Gtc, user, None)
            .expect("add ask");

        // Aggressive buy from same user → STP cancels taker
        let bid_id = new_id();
        let result =
            book.add_limit_order_with_user(bid_id, 100, 5, Side::Buy, TimeInForce::Gtc, user, None);
        assert!(result.is_err()); // SelfTradePrevented

        let bid_status = book.order_status(bid_id);
        assert_eq!(
            bid_status,
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            })
        );

        // The resting ask should still be Open
        assert_eq!(book.order_status(ask_id), Some(OrderStatus::Open));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // get_order_history
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn get_order_history_returns_transitions_in_order() {
        let book = book_with_tracker("TEST");

        // Place a small resting ask
        book.add_limit_order(new_id(), 100, 3, Side::Sell, TimeInForce::Gtc, None)
            .expect("add ask");

        // Buy partially matches, then rests
        let bid_id = new_id();
        book.add_limit_order(bid_id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add bid");

        // Cancel the resting remainder
        book.cancel_order(bid_id).expect("cancel");

        let history = book.get_order_history(bid_id);
        assert!(history.is_some());
        let history = history.expect("history exists");

        // Should have 2 entries: PartiallyFilled then Cancelled
        assert_eq!(
            history.len(),
            2,
            "expected 2 transitions, got {}",
            history.len()
        );

        // First transition: PartiallyFilled
        assert!(
            matches!(history[0].1, OrderStatus::PartiallyFilled { .. }),
            "first should be PartiallyFilled, got: {:?}",
            history[0].1
        );

        // Second transition: Cancelled
        assert!(
            matches!(history[1].1, OrderStatus::Cancelled { .. }),
            "second should be Cancelled, got: {:?}",
            history[1].1
        );

        // Timestamps should be monotonic
        assert!(history[1].0 >= history[0].0);
    }

    #[test]
    fn get_order_history_returns_none_for_unknown() {
        let book = book_with_tracker("TEST");
        assert!(book.get_order_history(new_id()).is_none());
    }

    #[test]
    fn get_order_history_no_tracker_returns_none() {
        let book = DefaultOrderBook::new("TEST");
        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        assert!(book.get_order_history(id).is_none());
    }

    #[test]
    fn get_order_history_single_transition_open() {
        let book = book_with_tracker("TEST");
        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");

        let history = book.get_order_history(id);
        assert!(history.is_some());
        let history = history.expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].1, OrderStatus::Open);
        assert!(history[0].0 > 0, "timestamp should be non-zero");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // active_order_count / terminal_order_count
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn active_and_terminal_counts() {
        let book = book_with_tracker("TEST");

        assert_eq!(book.active_order_count(), 0);
        assert_eq!(book.terminal_order_count(), 0);

        // Add 3 resting orders
        let id1 = new_id();
        let id2 = new_id();
        let id3 = new_id();
        book.add_limit_order(id1, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id2, 99, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id3, 200, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("add");

        assert_eq!(book.active_order_count(), 3);
        assert_eq!(book.terminal_order_count(), 0);

        // Cancel one
        book.cancel_order(id1).expect("cancel");

        assert_eq!(book.active_order_count(), 2);
        assert_eq!(book.terminal_order_count(), 1);

        // Fill one via aggressive order
        let aggressive_id = new_id();
        book.add_limit_order(aggressive_id, 200, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("aggressive buy");

        // id3 (ask at 200) should be Filled, aggressive_id should be Filled
        // id2 still Open
        assert_eq!(book.active_order_count(), 1);
        assert!(book.terminal_order_count() >= 3);
    }

    #[test]
    fn counts_zero_without_tracker() {
        let book = DefaultOrderBook::new("TEST");
        assert_eq!(book.active_order_count(), 0);
        assert_eq!(book.terminal_order_count(), 0);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // purge_terminal_states
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn purge_terminal_states_removes_old_entries() {
        let book = book_with_tracker("TEST");

        // Add and cancel orders
        let id1 = new_id();
        let id2 = new_id();
        book.add_limit_order(id1, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(id2, 200, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("add");
        book.cancel_order(id1).expect("cancel");
        book.cancel_order(id2).expect("cancel");

        assert_eq!(book.terminal_order_count(), 2);

        // Purge with zero duration → should remove all terminal entries
        // (they were created in the past, any non-zero cutoff should catch them)
        let purged = book.purge_terminal_states(std::time::Duration::from_secs(0));
        assert_eq!(purged, 2);
        assert_eq!(book.terminal_order_count(), 0);

        // Status should now be None (purged)
        assert!(book.order_status(id1).is_none());
        assert!(book.order_status(id2).is_none());
    }

    #[test]
    fn purge_terminal_states_keeps_recent_entries() {
        let book = book_with_tracker("TEST");

        let id = new_id();
        book.add_limit_order(id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.cancel_order(id).expect("cancel");

        // Purge with very long duration → nothing should be removed
        let purged = book.purge_terminal_states(std::time::Duration::from_secs(3600));
        assert_eq!(purged, 0);
        assert!(book.order_status(id).is_some());
    }

    #[test]
    fn purge_terminal_states_does_not_affect_active() {
        let book = book_with_tracker("TEST");

        let active_id = new_id();
        let terminal_id = new_id();
        book.add_limit_order(active_id, 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("add");
        book.add_limit_order(terminal_id, 200, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("add");
        book.cancel_order(terminal_id).expect("cancel");

        // Purge all terminal
        let purged = book.purge_terminal_states(std::time::Duration::from_secs(0));
        assert_eq!(purged, 1);

        // Active order should still be tracked
        assert_eq!(book.order_status(active_id), Some(OrderStatus::Open));
        assert_eq!(book.active_order_count(), 1);
    }

    #[test]
    fn purge_terminal_states_zero_without_tracker() {
        let book = DefaultOrderBook::new("TEST");
        assert_eq!(
            book.purge_terminal_states(std::time::Duration::from_secs(0)),
            0
        );
    }
}
