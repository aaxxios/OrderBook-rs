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
                        source.as_ref(),
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
        // PriceLevel#219: the failed level was absorbed with its error; the
        // published prefix is rebuilt without it.
        assert!(committed.match_result.error().is_none());
        assert_eq!(committed.match_result.remaining_quantity().as_u64(), 10);
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
        // The raw match entry point has no trade-id pre-check, so an
        // exhausted generator aborts it at the first level.
        let (book, streams) = aborting_book(0);
        let err = book
            .match_order(Id::from_u64(TAKER), Side::Buy, 20, Some(102))
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

    fn assert_rejected_untouched_for_ids(
        book: &OrderBook<()>,
        streams: &Streams,
        err: &OrderBookError,
    ) {
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(PriceLevelError::CapacityExceeded {
                    resource: CapacityResource::IdSequence,
                    additional: 1,
                })
            ),
            "unexpected error {err:?}"
        );
        assert_eq!(RejectReason::from(err), RejectReason::CapacityExceeded);
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert!(streams.levels.lock().expect("level sink").is_empty());
        for maker in [MAKER_A, MAKER_B, MAKER_C, MAKER_D] {
            assert!(book.get_order(Id::from_u64(maker)).is_some());
        }
        assert!(book.trade_ids_exhausted(), "exhaustion is latched");
        assert_eq!(book.match_aborts(), 0, "nothing was aborted");
    }

    #[test]
    fn test_exhausted_generator_rejects_crossing_add_untouched() {
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
            .expect_err("no trade id left");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::CapacityExceeded,
            })
        );
        assert_eq!(book.best_bid(), None);

        // Non-crossing and post-only orders never mint a trade id.
        book.add_limit_order_with_user(
            Id::from_u64(200),
            90,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            taker_user(),
            None,
        )
        .expect("a non-crossing order rests");
        assert_eq!(book.best_bid(), Some(90));
    }

    #[test]
    fn test_exhausted_generator_rejects_market_orders_untouched() {
        let (book, streams) = aborting_book(0);
        let err = book
            .submit_market_order(Id::from_u64(TAKER), 20, Side::Buy)
            .expect_err("no trade id left");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
        let err = book
            .submit_market_order_by_amount(Id::from_u64(TAKER + 1), 10_000, Side::Buy)
            .expect_err("no trade id left");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
        let err = book
            .match_limit_order(Id::from_u64(TAKER + 2), 20, Side::Buy, 102)
            .expect_err("no trade id left");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
    }

    #[test]
    fn test_exhausted_generator_lets_a_non_crossing_limit_match_through() {
        let (book, streams) = aborting_book(0);
        // Best ask is 100: a buy limited at 99 does not cross, so it needs
        // no trade id and must not be rejected.
        let result = book
            .match_limit_order(Id::from_u64(TAKER), 20, Side::Buy, 99)
            .expect("a non-crossing limit is not refused");
        assert!(result.trades().as_vec().is_empty());
        assert_eq!(result.remaining_quantity().as_u64(), 20);
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert_eq!(book.order_status(Id::from_u64(TAKER)), None);
        assert!(!book.trade_ids_exhausted(), "nothing tried to mint an id");

        // The same limit at the best ask crosses and is refused untouched.
        let err = book
            .match_limit_order(Id::from_u64(TAKER + 1), 20, Side::Buy, 100)
            .expect_err("a crossing limit needs a trade id");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
    }

    #[test]
    fn test_exhausted_generator_keeps_the_original_on_a_crossing_modify() {
        let (book, streams) = aborting_book(0);
        book.add_limit_order_with_user(
            Id::from_u64(300),
            90,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            taker_user(),
            None,
        )
        .expect("rest the original");
        streams.levels.lock().expect("level sink").clear();
        let err = book
            .update_order(pricelevel::OrderUpdate::UpdatePrice {
                order_id: Id::from_u64(300),
                new_price: Price::new(101),
            })
            .expect_err("the re-add could not mint a trade id");
        assert_rejected_untouched_for_ids(&book, &streams, &err);
        let original = book
            .get_order(Id::from_u64(300))
            .expect("original still rests");
        assert_eq!(original.price(), Price::new(90));
        assert_eq!(book.best_bid(), Some(90));
    }

    #[test]
    fn test_abort_counters_and_latch() {
        let (mut book, _streams) = aborting_book(2);
        assert_eq!(book.match_aborts(), 0);
        assert!(!book.trade_ids_exhausted());
        let _ = book.submit_market_order(Id::from_u64(TAKER), 20, Side::Buy);
        assert_eq!(book.match_aborts(), 1);
        assert_eq!(book.match_fold_failures(), 0, "the fold never failed");
        assert!(book.trade_ids_exhausted(), "the IdSequence abort latches");
        book.set_trade_id_namespace(uuid::Uuid::nil());
        assert!(!book.trade_ids_exhausted(), "a fresh generator clears it");
    }

    /// #240: boxing `MatchAborted::source` keeps `OrderBookError` from
    /// widening every `Result<_, OrderBookError>`.
    #[test]
    fn test_order_book_error_stays_within_96_bytes() {
        assert!(std::mem::size_of::<OrderBookError>() <= 96);
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

    /// With headroom to spare the FOK fills in full and mints exactly the
    /// ids it traded.
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

    /// #293 (PriceLevel#218): the trade-id check counts the trades the
    /// sweep will actually mint, not an upper bound. 10 units over A (level
    /// 100) and B / C (level 101) trade twice, so two ids are enough; before
    /// #293 the bound `min(makers, quantity)` asked for three and killed it.
    #[test]
    fn test_fok_trade_id_check_is_exact() {
        let (book, streams) = aborting_book(2);
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
        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_prefix_is_a_and_b(&published[0]);
        drop(published);
        assert_eq!(book.transaction_id_generator.remaining(), 0);
        assert_eq!(book.match_aborts(), 0);
    }

    /// A FOK the preflight refuses at a later level: nothing traded,
    /// nothing moved, every maker still rests, the taker is `Rejected`.
    fn assert_fok_rejected_untouched(book: &OrderBook<()>, streams: &Streams, remaining: u64) {
        assert!(streams.trades.lock().expect("trade sink").is_empty());
        assert!(streams.levels.lock().expect("level sink").is_empty());
        assert_eq!(book.transaction_id_generator.remaining(), remaining);
        for maker in [MAKER_A, MAKER_B, MAKER_C, MAKER_D] {
            assert_eq!(
                book.order_status(Id::from_u64(maker)),
                Some(OrderStatus::Open)
            );
            assert!(book.get_order(Id::from_u64(maker)).is_some());
        }
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), Some(100));
        assert_eq!(book.match_aborts(), 0, "nothing was aborted");
    }

    /// #293 (PriceLevel#217): a poisoned level refuses every match, so a
    /// FOK that would have to trade there after level 100 is rejected
    /// before level 100 is touched, instead of aborting mid-sweep.
    ///
    /// pricelevel only poisons a level when code panics inside it, which
    /// this crate cannot provoke, so the preflight's test-only fault hook
    /// stands in for `PriceLevel::is_poisoned` at level 101.
    #[test]
    fn test_fok_preflight_rejects_a_poisoned_later_level_untouched() {
        let (mut book, streams) = aborting_book(100);
        book.fok_level_fault_hook = Some(Arc::new(|price: u128| {
            (price == 101).then(|| PriceLevelError::InvalidOperation {
                message: "price level poisoned (test)".to_string(),
            })
        }));
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
            .expect_err("preflight must reject");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(PriceLevelError::InvalidOperation { .. })
            ),
            "unexpected error {err:?}"
        );
        let reason = RejectReason::from(&err);
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Rejected { reason })
        );
        assert_fok_rejected_untouched(&book, &streams, 100);
    }

    /// #293 (PriceLevel#218): a level whose FIFO sequence counter has no
    /// headroom for the sweep's replenishments refuses the match; the FOK
    /// is rejected untouched with `CounterExhausted` instead of aborting
    /// after level 100 committed.
    ///
    /// Exhausting a real level counter takes about `2^64` operations and a
    /// restored book restarts its counters, so the preflight's test-only
    /// fault hook stands in for `MatchRequirements::check` at level 101.
    #[test]
    fn test_fok_preflight_rejects_exhausted_level_counter_untouched() {
        let (mut book, streams) = aborting_book(100);
        book.fok_level_fault_hook = Some(Arc::new(|price: u128| {
            (price == 101).then_some(PriceLevelError::CounterExhausted {
                counter: pricelevel::ExhaustedCounter::QueueSequence,
            })
        }));
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
            .expect_err("preflight must reject");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(PriceLevelError::CounterExhausted {
                    counter: pricelevel::ExhaustedCounter::QueueSequence,
                })
            ),
            "unexpected error {err:?}"
        );
        assert_eq!(RejectReason::from(&err), RejectReason::CounterExhausted);
        assert_eq!(
            book.order_status(Id::from_u64(TAKER)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::CounterExhausted,
            })
        );
        assert_fok_rejected_untouched(&book, &streams, 100);
    }

    /// The fault hook only answers for the level it names: a FOK that
    /// fills at level 100 alone never reaches level 101 and fills.
    #[test]
    fn test_fok_preflight_ignores_levels_it_does_not_reach() {
        let (mut book, streams) = aborting_book(100);
        book.fok_level_fault_hook = Some(Arc::new(|price: u128| {
            (price == 101).then_some(PriceLevelError::CounterExhausted {
                counter: pricelevel::ExhaustedCounter::QueueSequence,
            })
        }));
        book.add_limit_order_with_user(
            Id::from_u64(TAKER),
            102,
            5,
            Side::Buy,
            TimeInForce::Fok,
            taker_user(),
            None,
        )
        .expect("level 100 fills the FOK");
        assert_eq!(streams.trades.lock().expect("trade sink").len(), 1);
        assert_eq!(book.best_ask(), Some(101));
    }

    /// #293 (PriceLevel#217): a level that refuses the match with an error
    /// and no trades — the shape a poisoned level now reports — stops the
    /// sweep with the earlier levels' prefix instead of letting it walk on
    /// to a worse price. With one id left, A trades at 100 and B's first
    /// step at 101 fails before trading.
    #[test]
    fn test_sweep_stops_at_a_level_that_refuses_without_trading() {
        let (book, streams) = aborting_book(1);
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
        assert_aborted(&err, 5, 1);
        let published = streams.trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        let trades = published[0].match_result.trades().as_vec();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].maker_order_id(), Id::from_u64(MAKER_A));
        assert!(published[0].match_result.error().is_none());
        drop(published);
        for maker in [MAKER_B, MAKER_C, MAKER_D] {
            assert_eq!(
                book.order_status(Id::from_u64(maker)),
                Some(OrderStatus::Open)
            );
        }
        assert_eq!(book.get_orders_at_price(102, Side::Sell).len(), 1);
        assert_eq!(book.best_bid(), None, "the remainder must never rest");
    }

    /// #293 (PriceLevel#219): the first level is absorbed by adopting its
    /// own buffers, error included when it failed mid-match. The published
    /// prefix is rebuilt without the error slot, like every other abort.
    #[test]
    fn test_first_level_abort_prefix_is_published_without_error_slot() {
        let mut book = OrderBook::<()>::new("ABRT1");
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        for id in [MAKER_A, MAKER_B] {
            book.add_limit_order_with_user(
                Id::from_u64(id),
                100,
                5,
                Side::Sell,
                TimeInForce::Gtc,
                maker_user(),
                None,
            )
            .expect("seed maker");
        }
        book.transaction_id_generator = generator_with_remaining(1);
        let failure = book
            .submit_market_order_with_committed(Id::from_u64(TAKER), 10, Side::Buy)
            .expect_err("sweep must abort");
        assert_aborted(&failure.error, 5, 1);
        let committed = failure.committed.as_ref().expect("committed prefix");
        assert!(committed.match_result.error().is_none());
        assert_eq!(committed.match_result.trades().len(), 1);
        assert_eq!(committed.match_result.remaining_quantity().as_u64(), 5);
        assert_eq!(
            book.order_status(Id::from_u64(MAKER_B)),
            Some(OrderStatus::Open)
        );
        let published = trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].engine_seq, committed.engine_seq);
    }

    /// #293 (PriceLevel#218): an iceberg that replenishes trades more often
    /// than it has makers. The FOK preflight counts those trades exactly,
    /// both for the trade-id check (one id short is rejected untouched) and
    /// for the result reservation (enough ids fill in full).
    #[test]
    fn test_fok_counts_replenishment_trades_exactly() {
        let build = |remaining: u64| {
            let mut book = OrderBook::<()>::new("ICE");
            book.add_iceberg_order(
                Id::from_u64(MAKER_A),
                100,
                2,
                6,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
            .expect("seed iceberg");
            book.add_limit_order(
                Id::from_u64(MAKER_B),
                101,
                2,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
            .expect("seed maker");
            book.transaction_id_generator = generator_with_remaining(remaining);
            book
        };
        // 10 units: the iceberg trades 2 + 2 + 2 + 2 (four trades), then B.
        let short = build(4);
        let err = short
            .add_limit_order(
                Id::from_u64(TAKER),
                101,
                10,
                Side::Buy,
                TimeInForce::Fok,
                None,
            )
            .expect_err("one id short");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(PriceLevelError::CapacityExceeded {
                    resource: CapacityResource::IdSequence,
                    additional: 5,
                })
            ),
            "unexpected error {err:?}"
        );
        assert_eq!(short.best_ask(), Some(100));

        let mut exact = build(5);
        let trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        exact.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        exact
            .add_limit_order(
                Id::from_u64(TAKER),
                101,
                10,
                Side::Buy,
                TimeInForce::Fok,
                None,
            )
            .expect("five ids fill the FOK");
        let published = trades.lock().expect("trade sink");
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].match_result.trades().len(), 5);
        assert!(published[0].match_result.is_complete());
        assert_eq!(exact.transaction_id_generator.remaining(), 0);
        assert_eq!(exact.match_fold_failures(), 0);
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
