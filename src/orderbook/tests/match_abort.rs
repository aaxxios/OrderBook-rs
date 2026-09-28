//! #240: a price level that fails mid-sweep stops the sweep, the committed
//! prefix is published through every stream exactly like a partial fill, the
//! remainder never rests, and a fill-or-kill taker is either filled in full
//! or rejected untouched.
//!
//! The failure is forced deterministically by restoring the book's trade-id
//! `UuidGenerator` (serde, `{"namespace", "counter"}`) a few ids short of its
//! exhaustion sentinel: pricelevel 0.10 reserves one trade id per maker step
//! and reports the refused reservation through `MatchResult::error()` while
//! keeping the steps it already committed.

#[cfg(test)]
mod tests {
    use crate::orderbook::book_change_event::PriceLevelChangedEvent;
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::risk::RiskConfig;
    use crate::orderbook::sequencer::{CommittedPrefix, SequencerResult};
    use crate::orderbook::trade::TradeResult;
    use crate::{OrderBook, OrderBookError};
    use pricelevel::{
        CapacityResource, Hash32, Id, OrderType, Price, PriceLevelError, Quantity, Side,
        TimeInForce, TimestampMs, UuidGenerator,
    };
    use std::sync::{Arc, Mutex};

    const MAKER_A: u64 = 1;
    const MAKER_B: u64 = 2;
    const MAKER_C: u64 = 3;
    const MAKER_D: u64 = 4;
    const TAKER: u64 = 100;

    fn maker_user() -> Hash32 {
        Hash32::new([7; 32])
    }

    fn taker_user() -> Hash32 {
        Hash32::new([9; 32])
    }

    /// A generator that can still mint exactly `remaining` trade ids.
    fn generator_with_remaining(remaining: u64) -> UuidGenerator {
        let counter = u64::MAX.checked_sub(remaining).expect("remaining ids");
        let json = format!(
            r#"{{"namespace":"6ba7b810-9dad-11d1-80b4-00c04fd430c8","counter":{counter}}}"#
        );
        serde_json::from_str(&json).expect("restore generator")
    }

    struct Streams {
        trades: Arc<Mutex<Vec<TradeResult>>>,
        levels: Arc<Mutex<Vec<PriceLevelChangedEvent>>>,
    }

    /// Asks: A 5@100, B 5@101, C 5@101, D 5@102 (all one maker account),
    /// risk + state tracking + both listeners installed, and a trade-id
    /// generator restored with `remaining` ids left. With `remaining == 2`
    /// a buy sweep trades A and B and fails on C's trade id.
    fn aborting_book(remaining: u64) -> (OrderBook<()>, Streams) {
        let mut book = OrderBook::<()>::new("ABRT");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(1_000));
        let trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        let levels = Arc::new(Mutex::new(Vec::new()));
        let level_sink = Arc::clone(&levels);
        book.price_level_changed_listener = Some(Arc::new(move |ev: PriceLevelChangedEvent| {
            level_sink.lock().expect("level sink").push(ev);
        }));
        for (id, price) in [
            (MAKER_A, 100),
            (MAKER_B, 101),
            (MAKER_C, 101),
            (MAKER_D, 102),
        ] {
            book.add_limit_order_with_user(
                Id::from_u64(id),
                price,
                5,
                Side::Sell,
                TimeInForce::Gtc,
                maker_user(),
                None,
            )
            .expect("seed maker");
        }
        book.transaction_id_generator = generator_with_remaining(remaining);
        levels.lock().expect("level sink").clear();
        (book, Streams { trades, levels })
    }

    fn assert_aborted(err: &OrderBookError, executed: u64, trades: usize) {
        match err {
            OrderBookError::MatchAborted {
                order_id,
                executed_quantity,
                trade_count,
                source,
            } => {
                assert_eq!(*order_id, Id::from_u64(TAKER));
                assert_eq!(*executed_quantity, executed);
                assert_eq!(*trade_count, trades);
                assert!(
                    matches!(
                        source,
                        PriceLevelError::CapacityExceeded {
                            resource: CapacityResource::IdSequence,
                            ..
                        }
                    ),
                    "unexpected source {source:?}"
                );
            }
            other => panic!("expected MatchAborted, got {other:?}"),
        }
        assert_eq!(RejectReason::from(err), RejectReason::MatchAborted);
    }

    /// The committed prefix of the canonical scenario: A and B, 5 each.
    fn assert_prefix_is_a_and_b(tr: &TradeResult) {
        let trades = tr.match_result.trades().as_vec();
        let fills: Vec<(Id, u128, u64)> = trades
            .iter()
            .map(|t| {
                (
                    t.maker_order_id(),
                    t.price().as_u128(),
                    t.quantity().as_u64(),
                )
            })
            .collect();
        assert_eq!(
            fills,
            vec![
                (Id::from_u64(MAKER_A), 100, 5),
                (Id::from_u64(MAKER_B), 101, 5)
            ]
        );
    }

    /// Everything after the failed level is untouched, the failed level
    /// keeps its unconsumed maker, the consumed makers are gone from every
    /// index, the taker never rests, and state / risk agree on the prefix.
    fn assert_book_after_prefix(book: &OrderBook<()>, taker_filled: u64) {
        assert_eq!(book.best_bid(), None, "the remainder must never rest");
        assert!(book.get_order(Id::from_u64(TAKER)).is_none());
        assert_eq!(book.best_ask(), Some(101), "level 100 was drained");
        let at_101 = book.get_orders_at_price(101, Side::Sell);
        assert_eq!(at_101.len(), 1);
        assert_eq!(at_101[0].id(), Id::from_u64(MAKER_C));
        assert_eq!(at_101[0].visible_quantity().as_u64(), 5);
        let at_102 = book.get_orders_at_price(102, Side::Sell);
        assert_eq!(at_102.len(), 1, "no trade at a worse level");
        assert_eq!(at_102[0].visible_quantity().as_u64(), 5);
        assert!(book.get_order(Id::from_u64(MAKER_A)).is_none());
        assert!(book.get_order(Id::from_u64(MAKER_B)).is_none());

        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: taker_filled,
                reason: CancelReason::MatchAborted,
            })
        );
        for maker in [MAKER_A, MAKER_B] {
            assert_eq!(
                book.order_status(Id::from_u64(maker)),
                Some(OrderStatus::Filled { filled_quantity: 5 })
            );
        }
        for maker in [MAKER_C, MAKER_D] {
            assert_eq!(
                book.order_status(Id::from_u64(maker)),
                Some(OrderStatus::Open)
            );
        }

        // Risk saw exactly the prefix: A and B left, C and D still rest.
        for maker in [MAKER_A, MAKER_B] {
            assert!(!book.risk_state.orders.contains_key(&Id::from_u64(maker)));
        }
        for maker in [MAKER_C, MAKER_D] {
            assert!(book.risk_state.orders.contains_key(&Id::from_u64(maker)));
        }
        let notional = book
            .risk_state
            .counters
            .get(&maker_user())
            .map(|c| c.resting_notional.load())
            .expect("maker counters");
        assert_eq!(notional, 5 * 101 + 5 * 102);
    }

    fn assert_level_events_only_touched_prefix(streams: &Streams) {
        let prices: Vec<u128> = streams
            .levels
            .lock()
            .expect("level sink")
            .iter()
            .map(|ev| ev.price)
            .collect();
        assert_eq!(
            prices,
            vec![100, 101],
            "only the traded levels report changes"
        );
    }

    #[test]
    fn test_add_order_gtc_sweep_aborts_at_failed_level_and_publishes_prefix() {
        let (book, streams) = aborting_book(2);
        let err = book
            .add_limit_order_with_user(
                Id::from_u64(TAKER),
                102,
                20,
                Side::Buy,
                TimeInForce::Gtc,
                taker_user(),
                None,
            )
            .expect_err("sweep must abort");
        assert_aborted(&err, 10, 2);

        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1, "one TradeResult for the prefix");
        assert_prefix_is_a_and_b(&published[0]);
        drop(published);
        assert_level_events_only_touched_prefix(&streams);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_add_order_ioc_abort_is_match_aborted_not_insufficient_liquidity() {
        let (book, streams) = aborting_book(2);
        let err = book
            .add_limit_order_with_user(
                Id::from_u64(TAKER),
                102,
                20,
                Side::Buy,
                TimeInForce::Ioc,
                taker_user(),
                None,
            )
            .expect_err("sweep must abort");
        assert_aborted(&err, 10, 2);
        assert_eq!(streams.trades.lock().expect("trade sink").len(), 1);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_add_order_with_committed_returns_the_published_prefix() {
        let (book, streams) = aborting_book(2);
        let order = OrderType::Standard {
            id: Id::from_u64(TAKER),
            price: Price::new(102),
            quantity: Quantity::new(20),
            side: Side::Buy,
            time_in_force: TimeInForce::Gtc,
            user_id: taker_user(),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        };
        let failure = book
            .add_order_with_committed(order)
            .expect_err("sweep must abort");
        assert_aborted(&failure.error, 10, 2);
        let committed = failure.committed.as_ref().expect("committed prefix");
        assert_prefix_is_a_and_b(committed);
        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].engine_seq, committed.engine_seq,
            "the caller gets the very TradeResult the listener saw"
        );
        drop(published);

        // The journal records the prefix, and it round-trips.
        let recorded = SequencerResult::from_submit_failure(&failure).expect("record");
        let SequencerResult::MatchAborted {
            code, committed, ..
        } = &recorded
        else {
            panic!("expected MatchAborted, got {recorded:?}");
        };
        assert_eq!(*code, RejectReason::MatchAborted);
        assert_eq!(committed.executed_quantity, 10);
        assert_eq!(committed.trades.len(), 2);
        assert_eq!(committed.trades[0].maker_order_id, Id::from_u64(MAKER_A));
        assert_eq!(committed.trades[1].price, Price::new(101));

        let json = serde_json::to_string(&recorded).expect("encode json");
        let back: SequencerResult = serde_json::from_str(&json).expect("decode json");
        match back {
            SequencerResult::MatchAborted {
                committed: decoded, ..
            } => assert_eq!(&decoded, committed),
            other => panic!("json round trip changed the variant: {other:?}"),
        }
    }

    #[test]
    fn test_submit_market_order_abort_publishes_prefix_and_returns_error() {
        let (book, streams) = aborting_book(2);
        let err = book
            .submit_market_order(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("sweep must abort");
        assert_aborted(&err, 10, 2);
        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_prefix_is_a_and_b(&published[0]);
        drop(published);
        assert_level_events_only_touched_prefix(&streams);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_submit_market_order_with_committed_carries_prefix() {
        let (book, _streams) = aborting_book(2);
        let failure = book
            .submit_market_order_with_committed(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("sweep must abort");
        assert_aborted(&failure.error, 10, 2);
        assert_prefix_is_a_and_b(failure.committed.as_ref().expect("prefix"));
    }

    #[test]
    fn test_submit_market_order_by_amount_abort_reports_base_quantity_prefix() {
        let (book, streams) = aborting_book(2);
        let failure = book
            .submit_market_order_by_amount_with_committed(Id::from_u64(TAKER), 10_000, Side::Buy)
            .expect_err("sweep must abort");
        assert_aborted(&failure.error, 10, 2);
        let committed = failure.committed.as_ref().expect("prefix");
        assert_prefix_is_a_and_b(committed);
        assert_eq!(
            committed.match_result.remaining_quantity().as_u64(),
            0,
            "the notional sentinel never leaks"
        );
        assert_eq!(streams.trades.lock().expect("trade sink").len(), 1);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_match_limit_order_abort_publishes_prefix() {
        let (book, streams) = aborting_book(2);
        let err = book
            .match_limit_order(Id::from_u64(TAKER), 20, Side::Buy, 102)
            .expect_err("sweep must abort");
        assert_aborted(&err, 10, 2);
        assert_eq!(streams.trades.lock().expect("trade sink").len(), 1);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_raw_match_order_abort_keeps_book_consistent() {
        let (book, streams) = aborting_book(2);
        let err = book
            .match_order(Id::from_u64(TAKER), Side::Buy, 20, Some(102))
            .expect_err("sweep must abort");
        assert_aborted(&err, 10, 2);
        // The raw entry point publishes no trades, as for a normal match,
        // but the level stream, risk, state and indices all saw the prefix.
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert_level_events_only_touched_prefix(&streams);
        assert_book_after_prefix(&book, 10);
    }

    #[test]
    fn test_abort_on_first_level_commits_nothing() {
        let (book, streams) = aborting_book(0);
        let err = book
            .add_limit_order_with_user(
                Id::from_u64(TAKER),
                102,
                20,
                Side::Buy,
                TimeInForce::Gtc,
                taker_user(),
                None,
            )
            .expect_err("sweep must abort");
        assert_aborted(&err, 0, 0);
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert!(streams.levels.lock().expect("level sink").is_empty());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), Some(100));
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MatchAborted,
            })
        );
        let failure = SequencerResult::from(&err);
        assert!(matches!(
            failure,
            SequencerResult::RejectedWithCode {
                code: RejectReason::MatchAborted,
                may_have_mutated: true,
                ..
            }
        ));
    }

    #[test]
    fn test_fok_preflight_rejects_untouched_when_trade_ids_are_short() {
        let (book, streams) = aborting_book(2);
        // 15 units need three trades (A, B, C); only two ids are left.
        let err = book
            .add_limit_order_with_user(
                Id::from_u64(TAKER),
                102,
                15,
                Side::Buy,
                TimeInForce::Fok,
                taker_user(),
                None,
            )
            .expect_err("preflight must kill");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(PriceLevelError::CapacityExceeded {
                    resource: CapacityResource::IdSequence,
                    additional: 3,
                })
            ),
            "unexpected error {err:?}"
        );
        assert_eq!(RejectReason::from(&err), RejectReason::CapacityExceeded);
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::CapacityExceeded,
            })
        );
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert!(streams.levels.lock().expect("level sink").is_empty());
        assert_eq!(book.transaction_id_generator.remaining(), 2);
        for maker in [MAKER_A, MAKER_B, MAKER_C, MAKER_D] {
            assert_eq!(
                book.order_status(Id::from_u64(maker)),
                Some(OrderStatus::Open)
            );
            assert!(book.get_order(Id::from_u64(maker)).is_some());
        }
        assert_eq!(book.best_bid(), None);
    }

    /// The preflight bound is conservative: at a level without hidden depth
    /// it counts `min(makers, quantity taken)` trades, so 10 units over A
    /// (level 100) and B / C (level 101) need three ids of headroom although
    /// the sweep only mints two.
    #[test]
    fn test_fok_within_trade_id_headroom_fills_in_full() {
        let (book, streams) = aborting_book(3);
        book.add_limit_order_with_user(
            Id::from_u64(TAKER),
            101,
            10,
            Side::Buy,
            TimeInForce::Fok,
            taker_user(),
            None,
        )
        .expect("two trades fit the two remaining ids");
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Filled {
                filled_quantity: 10
            })
        );
        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_prefix_is_a_and_b(&published[0]);
        assert_eq!(book.transaction_id_generator.remaining(), 1);
    }

    #[test]
    fn test_committed_prefix_same_fills_ignores_trade_ids_only() {
        let (book, _streams) = aborting_book(2);
        let failure = book
            .submit_market_order_with_committed(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("sweep must abort");
        let prefix = CommittedPrefix::try_from_match_result(
            &failure.committed.as_ref().expect("prefix").match_result,
        )
        .expect("record prefix");
        let mut other_ids = prefix.clone();
        other_ids.trades[0].trade_id = Id::from_u64(999);
        assert!(prefix.same_fills(&other_ids));
        let mut other_qty = prefix.clone();
        other_qty.trades[1].quantity = Quantity::new(4);
        assert!(!prefix.same_fills(&other_qty));
        let mut shorter = prefix.clone();
        shorter.trades.pop();
        assert!(!prefix.same_fills(&shorter));
    }

    #[test]
    fn test_from_submit_failure_rejects_inconsistent_prefix() {
        let (book, _streams) = aborting_book(2);
        let mut failure = book
            .submit_market_order_with_committed(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("sweep must abort");
        failure.committed = None;
        assert!(matches!(
            SequencerResult::from_submit_failure(&failure),
            Err(OrderBookError::InvalidOperation { .. })
        ));
    }

    #[cfg(feature = "bincode")]
    #[test]
    fn test_match_aborted_sequencer_result_bincode_round_trip() {
        let (book, _streams) = aborting_book(2);
        let failure = book
            .submit_market_order_with_committed(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("sweep must abort");
        let recorded = SequencerResult::from_submit_failure(&failure).expect("record");
        let cfg = bincode::config::standard();
        let bytes = bincode::serde::encode_to_vec(&recorded, cfg).expect("encode");
        let (decoded, used) =
            bincode::serde::decode_from_slice::<SequencerResult, _>(&bytes, cfg).expect("decode");
        assert_eq!(used, bytes.len());
        match (recorded, decoded) {
            (
                SequencerResult::MatchAborted {
                    reason: r1,
                    code: c1,
                    committed: p1,
                },
                SequencerResult::MatchAborted {
                    reason: r2,
                    code: c2,
                    committed: p2,
                },
            ) => {
                assert_eq!(r1, r2);
                assert_eq!(c1, c2);
                assert_eq!(p1, p2);
            }
            other => panic!("bincode round trip changed the variant: {other:?}"),
        }
    }
}
