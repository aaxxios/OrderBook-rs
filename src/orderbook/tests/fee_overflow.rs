//! #244: fee and notional arithmetic on the trade path is checked, and a
//! taker whose worst-case notional cannot be priced exactly is rejected
//! **before** it touches the book, identically on every submission API and
//! whether or not a trade listener is installed.

#[cfg(test)]
mod tests {
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::trade::TradeResult;
    use crate::{FeeSchedule, OrderBook, OrderBookError};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, PriceLevelError, Quantity, Side, TimeInForce,
        TimestampMs,
    };
    use std::sync::{Arc, Mutex};

    /// Resting ask: `ASK_QTY @ ASK_PRICE`. A buy of `ASK_QTY` reaches the
    /// notional `u128::MAX / 4`, which fits `u128` but not `× 5` bps.
    const ASK_PRICE: u128 = u128::MAX / 8;
    const ASK_QTY: u64 = 2;
    /// Resting bid: `BID_QTY @ BID_PRICE`, same notional for a sell.
    const BID_PRICE: u128 = u128::MAX / 16;
    const BID_QTY: u64 = 4;
    const ASK_ID: u64 = 1;
    const BID_ID: u64 = 2;
    const TAKER: u64 = 100;

    type Trades = Arc<Mutex<Vec<TradeResult>>>;

    fn book(schedule: Option<FeeSchedule>, listener: bool) -> (OrderBook<()>, Trades) {
        let mut book = OrderBook::<()>::new("FEE");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_fee_schedule(schedule);
        let trades: Trades = Arc::new(Mutex::new(Vec::new()));
        if listener {
            let sink = Arc::clone(&trades);
            book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
                sink.lock().expect("trade sink").push(tr.clone());
            }));
        }
        book.add_limit_order(
            Id::from_u64(ASK_ID),
            ASK_PRICE,
            ASK_QTY,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");
        book.add_limit_order(
            Id::from_u64(BID_ID),
            BID_PRICE,
            BID_QTY,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed bid");
        (book, trades)
    }

    fn limit(id: u64, price: u128, quantity: u64, side: Side) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(id),
            price: Price::new(price),
            quantity: Quantity::new(quantity),
            side,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// Every entry point a taker can use, driven for `side` with the
    /// quantity (or amount) that reaches the overflowing notional.
    #[derive(Debug, Clone, Copy)]
    enum Api {
        AddOrder,
        AddOrderWithResult,
        AddOrderWithCommitted,
        SubmitMarket,
        SubmitMarketWithUser,
        SubmitMarketWithCommitted,
        SubmitByAmount,
        SubmitByAmountWithUser,
        SubmitByAmountWithCommitted,
        MatchMarket,
        MatchMarketWithUser,
        MatchByAmount,
        MatchByAmountWithUser,
        MatchLimit,
        MatchLimitWithUser,
        MatchOrder,
        MatchOrderWithUser,
    }

    const ALL_APIS: [Api; 17] = [
        Api::AddOrder,
        Api::AddOrderWithResult,
        Api::AddOrderWithCommitted,
        Api::SubmitMarket,
        Api::SubmitMarketWithUser,
        Api::SubmitMarketWithCommitted,
        Api::SubmitByAmount,
        Api::SubmitByAmountWithUser,
        Api::SubmitByAmountWithCommitted,
        Api::MatchMarket,
        Api::MatchMarketWithUser,
        Api::MatchByAmount,
        Api::MatchByAmountWithUser,
        Api::MatchLimit,
        Api::MatchLimitWithUser,
        Api::MatchOrder,
        Api::MatchOrderWithUser,
    ];

    // tests may panic: rules/global_rules.md § Testing
    #[allow(clippy::panic_in_result_fn)]
    fn drive(book: &OrderBook<()>, api: Api, side: Side) -> Result<(), OrderBookError> {
        let id = Id::from_u64(TAKER);
        let (price, quantity) = match side {
            Side::Buy => (ASK_PRICE, ASK_QTY),
            Side::Sell => (BID_PRICE, BID_QTY),
        };
        // price × quantity == u128::MAX / 4 (rounded down) on both sides.
        let amount = price
            .checked_mul(u128::from(quantity))
            .expect("fixture notional fits");
        let user = Hash32::zero();
        let committed = |failure: crate::SubmitFailure| {
            assert!(failure.committed.is_none(), "{api:?}: nothing committed");
            failure.error
        };
        match api {
            Api::AddOrder => book
                .add_order(limit(TAKER, price, quantity, side))
                .map(drop),
            Api::AddOrderWithResult => book
                .add_order_with_result(limit(TAKER, price, quantity, side))
                .map(drop),
            Api::AddOrderWithCommitted => book
                .add_order_with_committed(limit(TAKER, price, quantity, side))
                .map(drop)
                .map_err(committed),
            Api::SubmitMarket => book.submit_market_order(id, quantity, side).map(drop),
            Api::SubmitMarketWithUser => book
                .submit_market_order_with_user(id, quantity, side, user)
                .map(drop),
            Api::SubmitMarketWithCommitted => book
                .submit_market_order_with_committed(id, quantity, side)
                .map(drop)
                .map_err(committed),
            Api::SubmitByAmount => book
                .submit_market_order_by_amount(id, amount, side)
                .map(drop),
            Api::SubmitByAmountWithUser => book
                .submit_market_order_by_amount_with_user(id, amount, side, user)
                .map(drop),
            Api::SubmitByAmountWithCommitted => book
                .submit_market_order_by_amount_with_committed(id, amount, side)
                .map(drop)
                .map_err(committed),
            Api::MatchMarket => book.match_market_order(id, quantity, side).map(drop),
            Api::MatchMarketWithUser => book
                .match_market_order_with_user(id, quantity, side, user)
                .map(drop),
            Api::MatchByAmount => book
                .match_market_order_by_amount(id, amount, side)
                .map(drop),
            Api::MatchByAmountWithUser => book
                .match_market_order_by_amount_with_user(id, amount, side, user)
                .map(drop),
            Api::MatchLimit => book.match_limit_order(id, quantity, side, price).map(drop),
            Api::MatchLimitWithUser => book
                .match_limit_order_with_user(id, quantity, side, price, user)
                .map(drop),
            // The raw family: one market, one limit sweep.
            Api::MatchOrder => book.match_order(id, side, quantity, None).map(drop),
            Api::MatchOrderWithUser => book
                .match_order_with_user(id, side, quantity, Some(price), user)
                .map(drop),
        }
    }

    fn assert_untouched(book: &OrderBook<()>, trades: &Trades, context: &str) {
        assert_eq!(book.best_ask(), Some(ASK_PRICE), "{context}: ask intact");
        assert_eq!(book.best_bid(), Some(BID_PRICE), "{context}: bid intact");
        let ask = book.get_order(Id::from_u64(ASK_ID)).expect("ask rests");
        assert_eq!(ask.visible_quantity().as_u64(), ASK_QTY, "{context}");
        let bid = book.get_order(Id::from_u64(BID_ID)).expect("bid rests");
        assert_eq!(bid.visible_quantity().as_u64(), BID_QTY, "{context}");
        assert!(book.get_order(Id::from_u64(TAKER)).is_none(), "{context}");
        assert!(
            trades.lock().expect("trade sink").is_empty(),
            "{context}: no trade emitted"
        );
    }

    #[test]
    fn test_fee_overflow_rejected_untouched_on_every_api_with_and_without_listener() {
        for listener in [true, false] {
            for side in [Side::Buy, Side::Sell] {
                for api in ALL_APIS {
                    let context = format!("{api:?} {side:?} listener={listener}");
                    let (book, trades) = book(Some(FeeSchedule::new(-2, 5)), listener);
                    let err = drive(&book, api, side).expect_err(&context);
                    match err {
                        OrderBookError::FeeOverflow {
                            notional,
                            bps,
                            max_guaranteed_exact_notional,
                        } => {
                            assert_eq!(bps, 5, "{context}: taker leg binds");
                            assert!(notional > max_guaranteed_exact_notional, "{context}");
                            assert_eq!(max_guaranteed_exact_notional, u128::MAX / 5);
                        }
                        other => panic!("{context}: expected FeeOverflow, got {other:?}"),
                    }
                    assert_eq!(
                        book.order_status(Id::from_u64(TAKER)),
                        Some(OrderStatus::Rejected {
                            reason: RejectReason::FeeOverflow
                        }),
                        "{context}"
                    );
                    assert_untouched(&book, &trades, &context);
                }
            }
        }
    }

    #[test]
    fn test_fee_overflow_modify_rejected_before_original_is_cancelled() {
        for listener in [true, false] {
            let (book, trades) = book(Some(FeeSchedule::new(-2, 5)), listener);
            // Re-price the resting bid up through the ask: its re-add would
            // sweep BID_QTY at ASK_PRICE, a notional × 5 bps overflows.
            let err = book
                .update_order(OrderUpdate::UpdatePrice {
                    order_id: Id::from_u64(BID_ID),
                    new_price: Price::new(ASK_PRICE),
                })
                .expect_err("modify must be refused");
            assert!(
                matches!(err, OrderBookError::FeeOverflow { .. }),
                "got {err:?}"
            );
            assert_untouched(&book, &trades, "update_order");
        }
    }

    #[test]
    fn test_notional_overflow_rejected_untouched_without_fees() {
        for schedule in [None, Some(FeeSchedule::zero_fee())] {
            for listener in [true, false] {
                let mut book = OrderBook::<()>::new("NOTIONAL");
                book.set_order_state_tracker(OrderStateTracker::new());
                book.set_fee_schedule(schedule);
                let trades: Trades = Arc::new(Mutex::new(Vec::new()));
                if listener {
                    let sink = Arc::clone(&trades);
                    book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
                        sink.lock().expect("trade sink").push(tr.clone());
                    }));
                }
                let price = u128::MAX / 2;
                book.add_limit_order(
                    Id::from_u64(ASK_ID),
                    price,
                    3,
                    Side::Sell,
                    TimeInForce::Gtc,
                    None,
                )
                .expect("seed ask");

                let err = book
                    .submit_market_order(Id::from_u64(TAKER), 3, Side::Buy)
                    .expect_err("market buy");
                assert!(
                    matches!(err, OrderBookError::NotionalOverflow { price: p, quantity: 3 } if p == price),
                    "got {err:?}"
                );
                assert_eq!(
                    book.order_status(Id::from_u64(TAKER)),
                    Some(OrderStatus::Rejected {
                        reason: RejectReason::NotionalOverflow
                    })
                );
                let err = book
                    .add_order(limit(TAKER + 1, price, 3, Side::Buy))
                    .expect_err("limit buy");
                assert!(
                    matches!(err, OrderBookError::NotionalOverflow { .. }),
                    "got {err:?}"
                );
                assert_eq!(book.best_ask(), Some(price));
                assert!(trades.lock().expect("trade sink").is_empty());

                // A single unit still fits: price × 1 is representable.
                book.submit_market_order(Id::from_u64(TAKER + 2), 1, Side::Buy)
                    .expect("one unit trades");
                assert_eq!(trades.lock().expect("sink").len(), usize::from(listener));
            }
        }
    }

    #[test]
    fn test_fee_overflow_non_crossing_order_is_admitted() {
        let (book, trades) = book(Some(FeeSchedule::new(-2, 5)), true);
        // A buy below the best ask never trades: its notional is irrelevant.
        book.add_order(limit(TAKER, ASK_PRICE - 1, ASK_QTY * 1_000, Side::Buy))
            .expect("non-crossing order rests");
        assert!(trades.lock().expect("sink").is_empty());
    }

    #[test]
    fn test_fee_overflow_limit_bound_uses_the_book_when_the_limit_alone_overflows() {
        // A buy limit far above the book: limit × quantity overflows, but the
        // highest resting ask × quantity prices exactly, so it trades.
        let mut book = OrderBook::<()>::new("LIM");
        book.set_fee_schedule(Some(FeeSchedule::new(-2, 5)));
        let trades: Trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        book.add_limit_order(
            Id::from_u64(1),
            1_000,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");
        book.add_order(limit(TAKER, u128::MAX / 2, 10, Side::Buy))
            .expect("crossing buy trades at 1_000");
        let trades = trades.lock().expect("sink");
        assert_eq!(trades.len(), 1);
        // notional 10_000 → maker -2, taker 5.
        assert_eq!(trades[0].total_maker_fees, -2);
        assert_eq!(trades[0].total_taker_fees, 5);
        assert_eq!(trades[0].quote_notional, 10_000);
    }

    #[test]
    fn test_maker_rebate_i32_min_trades_with_exact_fees() {
        let mut book = OrderBook::<()>::new("MIN");
        book.set_fee_schedule(Some(FeeSchedule::with_maker_rebate(i32::MIN, 5)));
        let trades: Trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        book.add_limit_order(Id::from_u64(1), 100, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("seed ask");
        book.submit_market_order(Id::from_u64(TAKER), 10, Side::Buy)
            .expect("trade");
        let trades = trades.lock().expect("sink");
        assert_eq!(trades.len(), 1);
        // notional 1_000: maker -floor(1_000 × 2^31 / 10_000), taker 0.
        assert_eq!(trades[0].total_maker_fees, -214_748_364);
        assert_eq!(trades[0].total_taker_fees, 0);
        assert_eq!(trades[0].total_fees(), Ok(-214_748_364));
    }

    /// The per-level backstop: a sweep that bypasses the preflight (as a
    /// maker admitted concurrently under the shared gate would make it)
    /// aborts before the level whose notional cannot be priced, keeps the
    /// prefix, and publishes it with exact fees.
    #[test]
    fn test_sweep_backstop_aborts_before_an_unpriceable_level() {
        let mut book = OrderBook::<()>::new("BACKSTOP");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_fee_schedule(Some(FeeSchedule::new(-2, 5)));
        let trades: Trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        book.add_limit_order(Id::from_u64(1), 100, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("cheap ask");
        book.add_limit_order(
            Id::from_u64(2),
            ASK_PRICE,
            ASK_QTY,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("expensive ask");

        // The public entry point refuses the taker untouched…
        let err = book
            .match_market_order(Id::from_u64(TAKER), 1 + ASK_QTY, Side::Buy)
            .expect_err("preflight");
        assert!(matches!(err, OrderBookError::FeeOverflow { .. }));
        assert_eq!(book.best_ask(), Some(100));

        // …and the sweep itself, entered without the preflight, stops at the
        // expensive level with the cheap one committed.
        let outcome = book
            .match_order_with_user_outcome(
                Id::from_u64(TAKER + 1),
                Side::Buy,
                1 + ASK_QTY,
                None,
                Hash32::zero(),
                pricelevel::TakerKind::Standard,
                crate::orderbook::matching::SweepReservation::NONE,
                0,
            )
            .expect("sweep outcome");
        let aborted = outcome.aborted.clone().expect("aborted");
        match &aborted {
            OrderBookError::MatchAborted {
                executed_quantity,
                trade_count,
                source,
                ..
            } => {
                assert_eq!(*executed_quantity, 1);
                assert_eq!(*trade_count, 1);
                assert!(
                    matches!(source.as_ref(), PriceLevelError::InvalidOperation { .. }),
                    "got {source:?}"
                );
            }
            other => panic!("expected MatchAborted, got {other:?}"),
        }
        assert_eq!(
            book.order_status(Id::from_u64(TAKER + 1)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::MatchAborted
            })
        );
        assert_eq!(book.best_ask(), Some(ASK_PRICE), "expensive level intact");
        let _ = book.publish_match_outcome(outcome, false);
        let trades = trades.lock().expect("sink");
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].quote_notional, 100);
        assert_eq!(book.match_fold_failures(), 0);
    }

    #[test]
    fn test_normal_fee_book_unchanged_on_every_api() {
        for api in ALL_APIS {
            let mut book = OrderBook::<()>::new("NORMAL");
            book.set_fee_schedule(Some(FeeSchedule::new(-2, 5)));
            let trades: Trades = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&trades);
            book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
                sink.lock().expect("trade sink").push(tr.clone());
            }));
            book.add_limit_order(
                Id::from_u64(1),
                1_000,
                10,
                Side::Sell,
                TimeInForce::Gtc,
                None,
            )
            .expect("seed ask");
            let id = Id::from_u64(TAKER);
            let user = Hash32::zero();
            let res: Result<(), OrderBookError> = match api {
                Api::AddOrder => book.add_order(limit(TAKER, 1_000, 10, Side::Buy)).map(drop),
                Api::AddOrderWithResult => book
                    .add_order_with_result(limit(TAKER, 1_000, 10, Side::Buy))
                    .map(drop),
                Api::AddOrderWithCommitted => book
                    .add_order_with_committed(limit(TAKER, 1_000, 10, Side::Buy))
                    .map(drop)
                    .map_err(|f| f.error),
                Api::SubmitMarket => book.submit_market_order(id, 10, Side::Buy).map(drop),
                Api::SubmitMarketWithUser => book
                    .submit_market_order_with_user(id, 10, Side::Buy, user)
                    .map(drop),
                Api::SubmitMarketWithCommitted => book
                    .submit_market_order_with_committed(id, 10, Side::Buy)
                    .map(drop)
                    .map_err(|f| f.error),
                Api::SubmitByAmount => book
                    .submit_market_order_by_amount(id, 10_000, Side::Buy)
                    .map(drop),
                Api::SubmitByAmountWithUser => book
                    .submit_market_order_by_amount_with_user(id, 10_000, Side::Buy, user)
                    .map(drop),
                Api::SubmitByAmountWithCommitted => book
                    .submit_market_order_by_amount_with_committed(id, 10_000, Side::Buy)
                    .map(drop)
                    .map_err(|f| f.error),
                Api::MatchMarket => book.match_market_order(id, 10, Side::Buy).map(drop),
                Api::MatchMarketWithUser => book
                    .match_market_order_with_user(id, 10, Side::Buy, user)
                    .map(drop),
                Api::MatchByAmount => book
                    .match_market_order_by_amount(id, 10_000, Side::Buy)
                    .map(drop),
                Api::MatchByAmountWithUser => book
                    .match_market_order_by_amount_with_user(id, 10_000, Side::Buy, user)
                    .map(drop),
                Api::MatchLimit => book.match_limit_order(id, 10, Side::Buy, 1_000).map(drop),
                Api::MatchLimitWithUser => book
                    .match_limit_order_with_user(id, 10, Side::Buy, 1_000, user)
                    .map(drop),
                // The raw family publishes nothing; it must still match.
                Api::MatchOrder => book.match_order(id, Side::Buy, 10, None).map(drop),
                Api::MatchOrderWithUser => book
                    .match_order_with_user(id, Side::Buy, 10, Some(1_000), user)
                    .map(drop),
            };
            res.unwrap_or_else(|err| panic!("{api:?}: {err:?}"));
            assert_eq!(book.best_ask(), None, "{api:?}: ask consumed");
            let trades = trades.lock().expect("sink");
            if matches!(api, Api::MatchOrder | Api::MatchOrderWithUser) {
                assert!(trades.is_empty(), "{api:?}: raw family does not publish");
            } else {
                assert_eq!(trades.len(), 1, "{api:?}");
                assert_eq!(trades[0].total_maker_fees, -2, "{api:?}");
                assert_eq!(trades[0].total_taker_fees, 5, "{api:?}");
                assert_eq!(trades[0].quote_notional, 10_000, "{api:?}");
            }
        }
    }

    /// A book with a normal ask and one absurd far ask behind it.
    fn book_with_far_ask() -> (OrderBook<()>, Trades) {
        let mut book = OrderBook::<()>::new("FAR");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_fee_schedule(Some(FeeSchedule::new(-2, 5)));
        let trades: Trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |tr: &TradeResult| {
            sink.lock().expect("trade sink").push(tr.clone());
        }));
        book.add_limit_order(Id::from_u64(1), 100, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("normal ask");
        // 10^37 × 11 × 5 bps overflows u128.
        book.add_limit_order(
            Id::from_u64(2),
            FAR_ASK,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("absurd ask");
        (book, trades)
    }

    const FAR_ASK: u128 = 10_u128.pow(37);

    /// Review P2-01: the bound is the worst **reachable** ask, so a maker
    /// resting an absurd price far behind the touch cannot make ordinary
    /// buys fail with `FeeOverflow`.
    #[test]
    fn test_far_absurd_ask_does_not_block_a_buy_that_cannot_reach_it() {
        let (book, trades) = book_with_far_ask();
        book.submit_market_order(Id::from_u64(TAKER), 4, Side::Buy)
            .expect("market buy within the best level");
        book.match_market_order(Id::from_u64(TAKER + 1), 3, Side::Buy)
            .expect("match_market_order within the best level");
        // A limit far above the book fails the limit fast path and walks.
        book.add_order(limit(TAKER + 2, u128::MAX / 2, 3, Side::Buy))
            .expect("limit buy within the best level");
        assert_eq!(trades.lock().expect("sink").len(), 3);
        assert_eq!(book.best_ask(), Some(FAR_ASK));
        assert_eq!(
            book.get_order(Id::from_u64(2))
                .expect("far ask rests")
                .visible_quantity()
                .as_u64(),
            10
        );
    }

    #[test]
    fn test_buy_that_reaches_the_absurd_ask_is_rejected_untouched() {
        let (book, trades) = book_with_far_ask();
        let err = book
            .submit_market_order(Id::from_u64(TAKER), 11, Side::Buy)
            .expect_err("reaches the far ask");
        match err {
            OrderBookError::FeeOverflow { notional, bps, .. } => {
                assert_eq!(notional, FAR_ASK * 11);
                assert_eq!(bps, 5);
            }
            other => panic!("expected FeeOverflow, got {other:?}"),
        }
        let err = book
            .add_order(limit(TAKER + 1, u128::MAX / 2, 11, Side::Buy))
            .expect_err("limit buy reaching the far ask");
        assert!(
            matches!(err, OrderBookError::FeeOverflow { .. }),
            "got {err:?}"
        );
        assert_eq!(book.best_ask(), Some(100), "best level untouched");
        assert_eq!(
            book.get_order(Id::from_u64(1))
                .expect("best ask rests")
                .visible_quantity()
                .as_u64(),
            10
        );
        assert!(trades.lock().expect("sink").is_empty());
        // A limit below the far ask caps the walk: it trades.
        book.add_order(limit(TAKER + 2, FAR_ASK - 1, 11, Side::Buy))
            .expect("limit capped below the far ask");
        assert_eq!(trades.lock().expect("sink").len(), 1);
    }

    #[test]
    fn test_preflight_returns_the_highest_verified_price() {
        let (book, _trades) = book_with_far_ask();
        let verified = |side, quantity, limit| {
            book.check_trade_arithmetic(side, quantity, limit)
                .expect("priceable")
        };
        assert_eq!(verified(Side::Buy, 4, None), 100);
        // A limit that passes is the bound.
        assert_eq!(verified(Side::Buy, 4, Some(1_000)), 1_000);
        assert_eq!(verified(Side::Buy, 4, Some(99)), 0, "does not cross");
        assert_eq!(verified(Side::Sell, 4, None), 0, "no bids");
        // Covering the best level exactly never reaches the next one.
        assert_eq!(verified(Side::Buy, 10, None), 100);
    }

    /// Copilot review on #280, extended by #247: the re-add of a
    /// validate-first modify takes the whole pre-cancel verdict as its
    /// admission (the arithmetic preflight included), so it cannot fail with
    /// `FeeOverflow` after the original is gone; a worse maker admitted
    /// concurrently is left to the sweep's backstop, which aborts before
    /// touching the level.
    #[test]
    fn test_modify_re_add_does_not_rerun_the_arithmetic_preflight() {
        use crate::orderbook::matching::ShapeVerdict;
        use crate::orderbook::modifications::Admission;

        let (book, _trades) = book(Some(FeeSchedule::new(-2, 5)), false);
        let crossing = limit(TAKER, ASK_PRICE, ASK_QTY, Side::Buy);
        assert!(matches!(
            book.validate_order_shape(&crossing),
            Err(OrderBookError::FeeOverflow { .. })
        ));
        let verdict = ShapeVerdict {
            fok: None,
            arithmetic_verified_price: 42,
        };
        let failure = book
            .add_order_inner(
                crossing,
                false,
                false,
                Admission::ReAdd {
                    verdict,
                    prior_filled: 0,
                },
            )
            .expect_err("the backstop refuses the unpriceable level");
        assert!(
            matches!(
                failure.into_submit().error,
                OrderBookError::MatchAborted {
                    executed_quantity: 0,
                    ..
                }
            ),
            "not FeeOverflow: the preflight is not re-run"
        );
        assert!(
            book.get_order(Id::from_u64(ASK_ID)).is_some(),
            "ask untouched"
        );
    }
}
