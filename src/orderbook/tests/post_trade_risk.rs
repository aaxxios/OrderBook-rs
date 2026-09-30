//! #291: a taker whose residual the risk layer refuses after it traded.
//!
//! The pre-trade risk check admits the whole order before the sweep, so
//! the residual's reservation only fails on a state the check could not
//! see (concurrent admissions on the account, an unrepresentable counter).
//! That failure follows real trades, so it is reported as
//! `OrderBookError::RiskRejectedAfterTrades` (code 22, may have mutated)
//! instead of the pre-trade risk codes replay skips, and replay reproduces
//! it without a `RiskConfig`: it re-runs the deterministic sweep and
//! refuses the residual as the journal recorded.
//!
//! The refusal is forced through the test-only `rest_risk_fault_hook`; the
//! repricer id-reuse window through `reprice_interleave_hook`.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::risk::RiskConfig;
    use crate::orderbook::sequencer::{
        InMemoryJournal, Journal, ReplayEngine, ReplayError, SequencerCommand, SequencerEvent,
        SequencerResult, snapshots_match,
    };
    use crate::orderbook::trade::SubmitFailure;
    use crate::{Clock, OrderBookError, StubClock};
    use pricelevel::TimestampMs;
    use pricelevel::{Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SYMBOL: &str = "POSTRISK";

    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    fn id(raw: u64) -> Id {
        Id::from_u64(raw)
    }

    fn limit(raw: u64, price: u128, qty: u64, side: Side, owner: Hash32) -> OrderType<()> {
        OrderType::Standard {
            id: id(raw),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            time_in_force: TimeInForce::Gtc,
            user_id: owner,
            timestamp: TimestampMs::new(raw),
            extra_fields: (),
        }
    }

    /// The refusal a concurrent admission on the same account produces.
    fn refusal(account: Hash32) -> OrderBookError {
        OrderBookError::RiskMaxNotional {
            account,
            current: u128::MAX,
            attempted: 1,
            limit: u128::MAX,
        }
    }

    /// Book with risk limits and order-state tracking.
    fn risk_book(clock: Arc<dyn Clock>) -> OrderBook<()> {
        let mut book = OrderBook::<()>::with_clock(SYMBOL, clock);
        book.set_order_state_tracker(OrderStateTracker::with_capacity(1_000));
        book.set_risk_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(1_000)
                .with_max_notional_per_account(1_000_000_000),
        );
        book
    }

    /// Refuses the risk reservation of `raw` once `armed` is set.
    fn refuse_residual(book: &mut OrderBook<()>, raw: u64, armed: Arc<AtomicBool>) {
        let target = id(raw);
        book.rest_risk_fault_hook = Some(Arc::new(move |order_id| {
            (order_id == target && armed.load(Ordering::SeqCst)).then(|| refusal(user(2)))
        }));
    }

    fn append(
        journal: &InMemoryJournal<()>,
        seq: u64,
        command: SequencerCommand<()>,
        result: SequencerResult,
    ) {
        journal
            .append(&SequencerEvent {
                sequence_num: seq,
                timestamp_ns: seq,
                command,
                result,
            })
            .expect("append");
    }

    /// Live: two asks of 5 at 100 and 101, a buy of 20 at 101 sweeps both
    /// and its residual of 10 is refused by the risk layer. Returns the
    /// book, the journal (recorded as a sequencer does) and the taker's
    /// error.
    fn post_trade_refusal_fixture(
        clock: Arc<dyn Clock>,
    ) -> (OrderBook<()>, InMemoryJournal<()>, OrderBookError) {
        let mut live = risk_book(clock);
        let armed = Arc::new(AtomicBool::new(false));
        refuse_residual(&mut live, 10, Arc::clone(&armed));
        let journal: InMemoryJournal<()> = InMemoryJournal::new();

        let makers = [
            limit(1, 100, 5, Side::Sell, user(1)),
            limit(2, 101, 5, Side::Sell, user(1)),
            // Deeper liquidity the taker's limit does not reach.
            limit(3, 102, 5, Side::Sell, user(1)),
        ];
        for (seq, maker) in (0u64..).zip(makers) {
            live.add_order(maker).expect("rest maker");
            append(
                &journal,
                seq,
                SequencerCommand::AddOrder(maker),
                SequencerResult::OrderAdded {
                    order_id: maker.id(),
                },
            );
        }

        armed.store(true, Ordering::SeqCst);
        let taker = limit(10, 101, 20, Side::Buy, user(2));
        let err = live.add_order(taker).expect_err("residual refused");
        append(
            &journal,
            3,
            SequencerCommand::AddOrder(taker),
            SequencerResult::from(&err),
        );
        armed.store(false, Ordering::SeqCst);

        // A later command that depends on the book the refusal left behind.
        let follow_up = limit(11, 102, 2, Side::Buy, user(3));
        live.add_order(follow_up).expect("follow-up");
        append(
            &journal,
            4,
            SequencerCommand::AddOrder(follow_up),
            SequencerResult::OrderAdded {
                order_id: follow_up.id(),
            },
        );
        (live, journal, err)
    }

    #[test]
    fn test_post_trade_risk_refusal_reports_the_trades_and_ends_the_taker() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let (live, _journal, err) = post_trade_refusal_fixture(clock);

        match &err {
            OrderBookError::RiskRejectedAfterTrades {
                order_id,
                executed_quantity,
                source,
            } => {
                assert_eq!(*order_id, id(10));
                assert_eq!(*executed_quantity, 10);
                assert!(matches!(**source, OrderBookError::RiskMaxNotional { .. }));
            }
            other => panic!("expected RiskRejectedAfterTrades, got {other:?}"),
        }
        assert_eq!(
            RejectReason::from(&err),
            RejectReason::RiskRejectedAfterTrades
        );
        // The first taker's residual never rested: no bid besides the
        // follow-up, no location, and a terminal state with its fills.
        assert!(live.get_order(id(10)).is_none());
        assert!(!live.order_locations.contains_key(&id(10)));
        assert_eq!(
            live.order_status(id(10)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 10,
                reason: CancelReason::RestFailed,
            })
        );
        // Makers 1 and 2 were consumed; the follow-up then traded with 3.
        assert!(live.get_order(id(1)).is_none());
        assert!(live.get_order(id(2)).is_none());
        assert_eq!(live.best_bid(), None);
        assert_eq!(live.best_ask(), Some(102));
    }

    /// A risk refusal of an order that did not trade keeps the pre-trade
    /// risk error and its "never mutated" classification.
    #[test]
    fn test_untraded_risk_refusal_keeps_the_risk_error() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut book = risk_book(clock);
        refuse_residual(&mut book, 10, Arc::new(AtomicBool::new(true)));

        let err = book
            .add_order(limit(10, 90, 5, Side::Buy, user(2)))
            .expect_err("refused");
        assert!(matches!(err, OrderBookError::RiskMaxNotional { .. }));
        assert!(matches!(
            SequencerResult::from(&err),
            SequencerResult::RejectedWithCode {
                code: RejectReason::RiskMaxNotional,
                may_have_mutated: false,
                ..
            }
        ));
        assert!(book.get_order(id(10)).is_none());
        assert_eq!(book.best_bid(), None);
    }

    /// Both journal builders record the post-trade refusal as a rejection
    /// that may have mutated, under its own code.
    #[test]
    fn test_post_trade_risk_refusal_is_journaled_as_mutating() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let (_live, _journal, err) = post_trade_refusal_fixture(Arc::clone(&clock));
        for recorded in [
            SequencerResult::from(&err),
            SequencerResult::from_submit_failure(&SubmitFailure::new(err.clone())).expect("record"),
        ] {
            assert!(
                matches!(
                    recorded,
                    SequencerResult::RejectedWithCode {
                        code: RejectReason::RiskRejectedAfterTrades,
                        may_have_mutated: true,
                        stp_mode: None,
                        ..
                    }
                ),
                "{recorded:?}"
            );
        }

        // `add_order_with_committed` hands the trades back with the error.
        let mut book = risk_book(clock);
        refuse_residual(&mut book, 10, Arc::new(AtomicBool::new(true)));
        book.add_order(limit(1, 100, 5, Side::Sell, user(1)))
            .expect("rest maker");
        let failure = book
            .add_order_with_committed(limit(10, 100, 8, Side::Buy, user(2)))
            .expect_err("refused");
        assert!(matches!(
            failure.error,
            OrderBookError::RiskRejectedAfterTrades {
                executed_quantity: 5,
                ..
            }
        ));
        let committed = failure.committed.as_ref().expect("committed trades");
        assert_eq!(committed.match_result.trades().as_vec().len(), 1);
    }

    /// Replay re-executes the journaled refusal without a `RiskConfig`: the
    /// sweep reproduces the live trades, the residual is refused as
    /// recorded, and the replayed book matches the live one.
    #[test]
    fn test_replay_of_post_trade_risk_refusal_matches_live_book() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let (live, journal, _err) = post_trade_refusal_fixture(Arc::clone(&clock));

        let (replayed, last) = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            clock,
            &Default::default(),
        )
        .expect("replay");
        assert_eq!(last, 4);
        let replayed_snapshot = replayed.create_snapshot(usize::MAX).expect("snapshot");
        let live_snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        assert!(
            snapshots_match(&replayed_snapshot, &live_snapshot),
            "replay must reproduce the trades of the refused taker"
        );
        assert!(replayed.get_order(id(1)).is_none());
        assert!(replayed.get_order(id(10)).is_none());
        assert_eq!(replayed.best_ask(), Some(102));
    }

    /// A journal whose post-trade refusal the replay book cannot reproduce
    /// (here: the makers it traded with are missing) stops replay with a
    /// typed error instead of diverging silently.
    #[test]
    fn test_replay_of_unreproducible_post_trade_refusal_is_a_mismatch() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let (_live, _full, err) = post_trade_refusal_fixture(Arc::clone(&clock));
        let taker = SequencerCommand::AddOrder(limit(10, 101, 20, Side::Buy, user(2)));

        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        append(&journal, 0, taker, SequencerResult::from(&err));
        let replayed = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            clock,
            &Default::default(),
        );
        assert!(
            matches!(
                replayed,
                Err(ReplayError::OutcomeMismatch {
                    sequence_num: 0,
                    recorded: RejectReason::RiskRejectedAfterTrades,
                    actual: Some(OrderBookError::InvalidOperation { .. }),
                })
            ),
            "{:?}",
            replayed.as_ref().err()
        );
    }

    /// A replay whose sweep fills the whole order has no residual to
    /// refuse: the recorded refusal cannot be reproduced and replay stops.
    #[test]
    fn test_replay_of_post_trade_refusal_that_fills_completely_is_a_mismatch() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let (_live, _journal, err) = post_trade_refusal_fixture(Arc::clone(&clock));
        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        let maker = limit(1, 100, 50, Side::Sell, user(1));
        append(
            &journal,
            0,
            SequencerCommand::AddOrder(maker),
            SequencerResult::OrderAdded { order_id: id(1) },
        );
        append(
            &journal,
            1,
            SequencerCommand::AddOrder(limit(10, 101, 20, Side::Buy, user(2))),
            SequencerResult::from(&err),
        );
        let replayed = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            clock,
            &Default::default(),
        );
        assert!(
            matches!(
                replayed,
                Err(ReplayError::OutcomeMismatch {
                    sequence_num: 1,
                    recorded: RejectReason::RiskRejectedAfterTrades,
                    actual: None,
                })
            ),
            "{:?}",
            replayed.as_ref().err()
        );
    }

    /// A modify whose re-add traded and then had its residual refused by
    /// the risk layer loses the order; the source says why.
    #[test]
    fn test_modify_readd_refused_after_trades_reports_the_risk_source() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut book = risk_book(clock);
        let armed = Arc::new(AtomicBool::new(false));
        refuse_residual(&mut book, 10, Arc::clone(&armed));
        book.add_order(limit(1, 101, 3, Side::Sell, user(1)))
            .expect("rest maker");
        book.add_order(limit(10, 100, 8, Side::Buy, user(2)))
            .expect("rest bid");

        armed.store(true, Ordering::SeqCst);
        let err = book
            .update_order(OrderUpdate::UpdatePrice {
                order_id: id(10),
                new_price: Price::new(101),
            })
            .expect_err("re-add refused");
        match err {
            OrderBookError::ModifyOrderLost {
                order_id,
                executed_quantity,
                source,
                restore_error: None,
            } => {
                assert_eq!(order_id, id(10));
                assert_eq!(executed_quantity, 3);
                assert!(matches!(
                    *source,
                    OrderBookError::RiskRejectedAfterTrades {
                        executed_quantity: 3,
                        ..
                    }
                ));
            }
            other => panic!("expected ModifyOrderLost, got {other:?}"),
        }
        assert!(book.get_order(id(10)).is_none());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
    }

    #[cfg(feature = "special_orders")]
    mod repricer_id_reuse {
        use super::{id, user};
        use crate::orderbook::book::OrderBook;
        use crate::orderbook::repricing::RepricingOperations;
        use pricelevel::{
            Id, OrderType, PegReferenceType, Price, Quantity, Side, TimeInForce, TimestampMs,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        fn pegged(raw: u64, price: u128, qty: u64) -> OrderType<()> {
            OrderType::PeggedOrder {
                id: id(raw),
                price: Price::new(price),
                quantity: Quantity::new(qty),
                side: Side::Sell,
                user_id: user(1),
                timestamp: TimestampMs::new(raw),
                time_in_force: TimeInForce::Gtc,
                reference_price_offset: 0,
                reference_price_type: PegReferenceType::BestAsk,
                extra_fields: (),
            }
        }

        /// Rests `order`, then fills it completely with a taker: the sweep
        /// drains it without unregistering it, which leaves the stale
        /// registration a repricing pass cleans up.
        fn rest_then_fill(book: &OrderBook<()>, order: OrderType<()>, qty: u64) {
            let order_id = order.id();
            book.add_order(order).expect("rest special order");
            book.submit_market_order(Id::from_u64(900), qty, Side::Buy)
                .expect("fill it");
            assert!(book.get_order(order_id).is_none());
        }

        /// Rests `replacement` (same id) from inside the repricer's window
        /// between `get_order == None` and the unregistration, once.
        fn reuse_id_in_window(book: &mut OrderBook<()>, replacement: OrderType<()>) {
            let fired = Arc::new(AtomicBool::new(false));
            let target = replacement.id();
            book.reprice_interleave_hook = Some(Arc::new(move |book, order_id| {
                if order_id == target && !fired.swap(true, Ordering::SeqCst) {
                    book.add_order(replacement).expect("reuse the id");
                }
            }));
        }

        #[test]
        fn test_pegged_id_reused_in_repricer_window_keeps_its_registration() {
            let mut book = OrderBook::<()>::new("REPRICE_ABA");
            rest_then_fill(&book, pegged(5, 100, 4), 4);
            assert_eq!(book.pegged_order_count(), 1, "stale registration");

            reuse_id_in_window(&mut book, pegged(5, 110, 7));
            book.reprice_pegged_orders().expect("reprice");

            // The new order owns the id and rests; its registration survived.
            assert!(book.get_order(id(5)).is_some());
            assert_eq!(book.pegged_order_ids(), vec![id(5)]);
        }

        // #286: trailing stops are no longer tracked by the repricer (they
        // are pending off-book stops the book owns), so only the pegged
        // registration has a repricer id-reuse window.

        /// Without a reuse the stale registration is still released.
        #[test]
        fn test_stale_registrations_are_released_when_the_id_is_unowned() {
            let book = OrderBook::<()>::new("REPRICE_ABA");
            rest_then_fill(&book, pegged(5, 100, 4), 4);
            assert_eq!(book.pegged_order_count(), 1);

            let result = book.reprice_special_orders().expect("reprice");
            assert!(result.failed_orders.is_empty());
            assert_eq!(book.pegged_order_count(), 0);
            assert_eq!(result.trailing_stops_repriced, 0);
        }
    }
}
