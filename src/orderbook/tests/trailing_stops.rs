//! #286: trailing stops are pending off-book stops driven by the last trade
//! price.
//!
//! With `special_orders`: a pending stop is never liquidity; its watermark
//! follows the last trade and its stop price trails it; a last trade at or
//! through the stop executes it as an IOC market order (remainder
//! cancelled); stops cascade deterministically in admission order; cancel,
//! modify, every mass cancel scope, expiry and `get_order` cover them; risk
//! is reserved at admission and released on trigger or cancel; snapshot v5
//! round-trips them (v4 packages still restore) and replay reproduces the
//! triggers. Without the feature a trailing stop is rejected untouched with
//! `StopOrdersUnsupported` (code 23).

#[cfg(all(test, feature = "special_orders"))]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::fees::FeeSchedule;
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::risk::RiskConfig;
    use crate::orderbook::sequencer::{
        InMemoryJournal, Journal, ReplayBookConfig, ReplayEngine, SequencerCommand, SequencerEvent,
        SequencerResult, snapshots_match,
    };
    use crate::orderbook::snapshot::OrderBookSnapshotPackage;
    use crate::orderbook::stp::STPMode;
    use crate::orderbook::trade::TradeResult;
    use crate::{Clock, OrderBookError, StubClock};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };
    use std::sync::{Arc, Barrier, Mutex};
    use uuid::Uuid;

    const SYMBOL: &str = "STOP/USD";

    fn id(raw: u64) -> Id {
        Id::from_u64(raw)
    }

    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    fn namespace() -> Uuid {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, b"trailing-stops-286")
    }

    fn limit(raw: u64, price: u128, qty: u64, side: Side, owner: Hash32) -> OrderType<()> {
        OrderType::Standard {
            id: id(raw),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            user_id: owner,
            timestamp: TimestampMs::new(raw),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stop_tif(
        raw: u64,
        side: Side,
        stop: u128,
        watermark: u128,
        trail: u64,
        qty: u64,
        owner: Hash32,
        tif: TimeInForce,
    ) -> OrderType<()> {
        OrderType::TrailingStop {
            id: id(raw),
            price: Price::new(stop),
            quantity: Quantity::new(qty),
            side,
            user_id: owner,
            timestamp: TimestampMs::new(raw),
            time_in_force: tif,
            trail_amount: Quantity::new(trail),
            last_reference_price: Price::new(watermark),
            extra_fields: (),
        }
    }

    fn stop(
        raw: u64,
        side: Side,
        stop: u128,
        watermark: u128,
        trail: u64,
        qty: u64,
    ) -> OrderType<()> {
        stop_tif(
            raw,
            side,
            stop,
            watermark,
            trail,
            qty,
            user(9),
            TimeInForce::Gtc,
        )
    }

    /// A deterministic book with order-state tracking.
    fn new_book() -> OrderBook<()> {
        let clock = Arc::new(StubClock::starting_at(0)) as Arc<dyn Clock>;
        let mut book = OrderBook::with_clock_and_namespace(SYMBOL, clock, namespace());
        book.set_order_state_tracker(OrderStateTracker::with_capacity(10_000));
        book
    }

    type Trades = Arc<Mutex<Vec<String>>>;

    /// Installs a trade listener logging `taker [maker@price x qty, ...]`.
    fn record_trades(book: &mut OrderBook<()>) -> Trades {
        let log: Trades = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            let makers: Vec<String> = result
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|t| {
                    format!(
                        "{}@{}x{}",
                        t.maker_order_id(),
                        t.price().as_u128(),
                        t.quantity().as_u64()
                    )
                })
                .collect();
            sink.lock().expect("log").push(format!(
                "{} [{}] taker_fees={}",
                result.match_result.order_id(),
                makers.join(","),
                result.total_taker_fees
            ));
        }));
        log
    }

    fn taken(log: &Trades) -> Vec<String> {
        log.lock().expect("log").clone()
    }

    fn stop_price(book: &OrderBook<()>, raw: u64) -> (u128, u128) {
        match book.get_order(id(raw)).as_deref() {
            Some(OrderType::TrailingStop {
                price,
                last_reference_price,
                ..
            }) => (price.as_u128(), last_reference_price.as_u128()),
            other => panic!("expected pending stop {raw}, got {other:?}"),
        }
    }

    // ---- invisibility ---------------------------------------------------

    #[test]
    fn test_pending_stop_is_not_liquidity() {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 110, 10, Side::Sell, user(1)))
            .expect("ask");
        let admitted = book
            .add_order(stop(50, Side::Sell, 95, 100, 5, 3))
            .expect("pending stop");
        assert_eq!(admitted.id(), id(50));

        assert_eq!(book.best_bid(), Some(90));
        assert_eq!(book.best_ask(), Some(110));
        assert_eq!(book.order_count_at_price(95, Side::Sell), None);
        assert!(book.get_orders_at_price(95, Side::Sell).is_empty());
        assert_eq!(book.get_all_orders().len(), 2, "levels hold two orders");
        let snapshot = book.create_snapshot(usize::MAX).expect("snapshot");
        assert_eq!(snapshot.asks.len(), 1);
        assert_eq!(snapshot.bids.len(), 1);
        assert_eq!(snapshot.pending_stops.len(), 1);
        assert_eq!(snapshot.last_trade_price, None);
        let enriched = book.enriched_snapshot(10).expect("enriched");
        assert_eq!(enriched.ask_depth_total, 10, "the stop adds no depth");
        assert!(!book.order_locations.contains_key(&id(50)));

        assert_eq!(book.trailing_stop_count(), 1);
        assert_eq!(book.trailing_stop_ids(), vec![id(50)]);
        assert_eq!(book.order_status(id(50)), Some(OrderStatus::Open));
        assert_eq!(stop_price(&book, 50), (95, 100));

        // A buy at the stop price rests: there is nothing there to match
        // (the pre-0.14 resting stop would have filled it).
        book.add_order(limit(3, 95, 1, Side::Buy, user(2)))
            .expect("rests");
        assert_eq!(book.best_bid(), Some(95));
        assert_eq!(book.last_trade_price(), None, "no trade");
        assert_eq!(book.trailing_stop_count(), 1);
    }

    // ---- trailing -------------------------------------------------------

    #[test]
    fn test_sell_stop_trails_rising_last_trades_on_an_uncrossed_book() {
        let book = new_book();
        book.add_order(limit(1, 90, 100, Side::Buy, user(1)))
            .expect("bid");
        for (raw, price) in [(2, 101), (3, 102), (4, 104)] {
            book.add_order(limit(raw, price, 1, Side::Sell, user(1)))
                .expect("ask");
        }
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 3))
            .expect("pending stop");

        for (taker, expected) in [(10, (96, 101)), (11, (97, 102)), (12, (99, 104))] {
            book.submit_market_order(id(taker), 1, Side::Buy)
                .expect("buy");
            assert_eq!(stop_price(&book, 50), expected);
        }
        // A lower last trade never loosens the stop.
        book.add_order(limit(5, 103, 1, Side::Sell, user(1)))
            .expect("ask 103");
        book.submit_market_order(id(13), 1, Side::Buy)
            .expect("buy at 103");
        assert_eq!(book.last_trade_price(), Some(103));
        assert_eq!(stop_price(&book, 50), (99, 104));
        assert_eq!(book.order_status(id(50)), Some(OrderStatus::Open));
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(100));
    }

    #[test]
    fn test_buy_stop_trails_falling_last_trades() {
        let book = new_book();
        book.add_order(limit(1, 200, 100, Side::Sell, user(1)))
            .expect("ask");
        for (raw, price) in [(2, 99), (3, 98), (4, 96)] {
            book.add_order(limit(raw, price, 1, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(stop(50, Side::Buy, 105, 100, 5, 3))
            .expect("pending stop");
        for (taker, expected) in [(10, (104, 99)), (11, (103, 98)), (12, (101, 96))] {
            book.submit_market_order(id(taker), 1, Side::Sell)
                .expect("sell");
            assert_eq!(stop_price(&book, 50), expected);
        }
    }

    // ---- trigger --------------------------------------------------------

    #[test]
    fn test_elected_stop_executes_as_ioc_market_order_and_cancels_the_remainder() {
        let mut book = new_book();
        book.set_fee_schedule(Some(FeeSchedule::new(0, 100)));
        let trades = record_trades(&mut book);
        book.add_order(limit(1, 94, 2, Side::Buy, user(1)))
            .expect("bid 94");
        book.add_order(limit(2, 93, 2, Side::Buy, user(1)))
            .expect("bid 93");
        book.add_order(limit(3, 95, 1, Side::Buy, user(1)))
            .expect("bid 95");
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 6))
            .expect("pending stop");

        // A trade at 95 elects the stop (95 <= 95): its market sell of 6
        // takes 2 @ 94 and 2 @ 93, and the remaining 2 are cancelled.
        book.submit_market_order(id(4), 1, Side::Sell)
            .expect("trade at 95");
        let child = book.stop_trigger_order_id(id(50));
        let log = taken(&trades);
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[0].starts_with(&format!("{} [{}@95x1]", id(4), id(3))));
        assert!(
            log[1].starts_with(&format!("{child} [{}@94x2,{}@93x2]", id(1), id(2))),
            "{log:?}"
        );
        assert!(
            !log[1].ends_with("taker_fees=0"),
            "the market order pays fees"
        );
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 4,
                reason: CancelReason::InsufficientLiquidity,
            })
        );
        assert!(book.get_order(id(50)).is_none());
        assert_eq!(book.trailing_stop_count(), 0);
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.last_trade_price(), Some(93));
    }

    #[test]
    fn test_elected_stop_filled_completely_ends_filled() {
        let book = new_book();
        book.add_order(limit(1, 94, 10, Side::Buy, user(1)))
            .expect("bid 94");
        book.add_order(limit(3, 95, 1, Side::Buy, user(1)))
            .expect("bid 95");
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 6))
            .expect("pending stop");
        book.submit_market_order(id(4), 1, Side::Sell)
            .expect("trade at 95");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 6 })
        );
        assert_eq!(book.visible_quantity_at_price(94, Side::Buy), Some(4));
    }

    #[test]
    fn test_stop_crossed_by_the_last_trade_triggers_at_admission() {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 100, 1, Side::Sell, user(1)))
            .expect("ask 100");
        book.add_order(limit(3, 101, 10, Side::Sell, user(1)))
            .expect("ask 101");
        book.submit_market_order(id(4), 1, Side::Buy)
            .expect("trade at 100");

        // Sell stop at 100 with last trade 100: elected on admission.
        book.add_order(stop(50, Side::Sell, 100, 100, 5, 2))
            .expect("admitted then triggered");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 2 })
        );
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(8));
        assert_eq!(book.last_trade_price(), Some(90));

        // Buy stop at 90 with last trade 90: elected on admission, buys 3
        // at 101.
        book.add_order(stop(51, Side::Buy, 90, 90, 5, 3))
            .expect("admitted then triggered");
        assert_eq!(
            book.order_status(id(51)),
            Some(OrderStatus::Filled { filled_quantity: 3 })
        );
        assert_eq!(book.visible_quantity_at_price(101, Side::Sell), Some(7));
        assert_eq!(book.trailing_stop_count(), 0);
    }

    #[test]
    fn test_stop_without_a_last_trade_stays_pending_until_the_first_trade() {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(stop(50, Side::Sell, 120, 100, 5, 2))
            .expect("pending stop above any price");
        assert_eq!(book.trailing_stop_count(), 1, "no trade yet: pending");
        book.submit_market_order(id(2), 1, Side::Sell)
            .expect("first trade at 90");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 2 })
        );
    }

    // ---- cascade --------------------------------------------------------

    #[test]
    fn test_cascade_of_three_stops_runs_in_order_until_quiescent() {
        let mut book = new_book();
        let trades = record_trades(&mut book);
        for (raw, price, qty) in [(1, 99, 1), (2, 97, 1), (3, 95, 1), (4, 90, 100)] {
            book.add_order(limit(raw, price, qty, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(limit(5, 100, 1, Side::Sell, user(1)))
            .expect("ask 100");
        book.submit_market_order(id(6), 1, Side::Buy)
            .expect("trade at 100");
        for (raw, stop_px, trail) in [(51, 98, 2), (52, 96, 4), (53, 94, 6), (54, 80, 20)] {
            book.add_order(stop(raw, Side::Sell, stop_px, 100, trail, 1))
                .expect("pending stop");
        }

        // 99, 97 -> elects 51 (97 <= 98); its sale at 95 elects 52; 52's
        // sale at 90 elects 53, which also sells at 90. 54 (stop 80) stays.
        book.submit_market_order(id(7), 2, Side::Sell)
            .expect("sweep 99 and 97");
        let takers: Vec<String> = taken(&trades)
            .iter()
            .map(|line| line.split(' ').next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            takers,
            vec![
                id(6).to_string(),
                id(7).to_string(),
                book.stop_trigger_order_id(id(51)).to_string(),
                book.stop_trigger_order_id(id(52)).to_string(),
                book.stop_trigger_order_id(id(53)).to_string(),
            ]
        );
        for raw in [51, 52, 53] {
            assert_eq!(
                book.order_status(id(raw)),
                Some(OrderStatus::Filled { filled_quantity: 1 }),
                "stop {raw}"
            );
        }
        assert_eq!(book.trailing_stop_ids(), vec![id(54)]);
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(98));
    }

    #[test]
    fn test_stops_elected_by_one_price_execute_in_admission_order() {
        let mut book = new_book();
        let trades = record_trades(&mut book);
        book.add_order(limit(1, 96, 1, Side::Buy, user(1)))
            .expect("bid 96");
        book.add_order(limit(2, 90, 100, Side::Buy, user(1)))
            .expect("bid 90");
        book.add_order(limit(3, 200, 100, Side::Sell, user(1)))
            .expect("ask 200");
        // Admitted first with the lower stop, then a higher sell stop and a
        // buy stop; one trade at 96 elects all three.
        book.add_order(stop(60, Side::Sell, 97, 100, 3, 1))
            .expect("first");
        book.add_order(stop(61, Side::Sell, 98, 100, 2, 1))
            .expect("second");
        book.add_order(stop(62, Side::Buy, 96, 80, 16, 1))
            .expect("third");
        book.submit_market_order(id(4), 1, Side::Sell)
            .expect("trade at 96");
        let takers: Vec<String> = taken(&trades)
            .iter()
            .map(|line| line.split(' ').next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            takers,
            vec![
                id(4).to_string(),
                book.stop_trigger_order_id(id(60)).to_string(),
                book.stop_trigger_order_id(id(61)).to_string(),
                book.stop_trigger_order_id(id(62)).to_string(),
            ],
            "time priority, sell and buy alike"
        );
        assert_eq!(book.trailing_stop_count(), 0);
    }

    // ---- cancel / modify / mass cancel / expiry -------------------------

    fn book_with_stops() -> OrderBook<()> {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 110, 10, Side::Sell, user(2)))
            .expect("ask");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            95,
            100,
            5,
            3,
            user(1),
            TimeInForce::Gtc,
        ))
        .expect("stop 50");
        book.add_order(stop_tif(
            51,
            Side::Buy,
            105,
            100,
            5,
            3,
            user(2),
            TimeInForce::Gtc,
        ))
        .expect("stop 51");
        book.add_order(stop_tif(
            52,
            Side::Sell,
            85,
            100,
            15,
            3,
            user(2),
            TimeInForce::Gtc,
        ))
        .expect("stop 52");
        book
    }

    #[test]
    fn test_cancel_and_update_cancel_remove_a_pending_stop() {
        let book = book_with_stops();
        let cancelled = book.cancel_order(id(50)).expect("cancel");
        assert_eq!(cancelled.map(|order| order.id()), Some(id(50)));
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            })
        );
        assert!(book.cancel_order(id(50)).expect("again").is_none());
        let via_update = book
            .update_order(OrderUpdate::Cancel { order_id: id(51) })
            .expect("update cancel");
        assert!(via_update.is_some());
        let via_zero = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: id(52),
                new_quantity: Quantity::new(0),
            })
            .expect("zero quantity");
        assert!(via_zero.is_some());
        assert_eq!(book.trailing_stop_count(), 0);
        // The ids are free again.
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 3))
            .expect("id reusable");
    }

    #[test]
    fn test_update_order_modifies_a_pending_stop() {
        let book = book_with_stops();
        let resized = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: id(50),
                new_quantity: Quantity::new(7),
            })
            .expect("resize")
            .expect("found");
        assert_eq!(resized.visible_quantity().as_u64(), 7);
        book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(50),
            new_price: Price::new(93),
        })
        .expect("reprice")
        .expect("found");
        assert_eq!(stop_price(&book, 50), (93, 100));
        assert!(matches!(
            book.update_order(OrderUpdate::UpdatePrice {
                order_id: id(50),
                new_price: Price::new(93),
            }),
            Err(OrderBookError::InvalidOperation { .. })
        ));
        book.update_order(OrderUpdate::Replace {
            order_id: id(50),
            price: Price::new(120),
            quantity: Quantity::new(2),
            side: Side::Buy,
        })
        .expect("replace")
        .expect("found");
        match book.get_order(id(50)).as_deref() {
            Some(OrderType::TrailingStop {
                side,
                price,
                quantity,
                ..
            }) => {
                assert_eq!(*side, Side::Buy);
                assert_eq!(price.as_u128(), 120);
                assert_eq!(quantity.as_u64(), 2);
            }
            other => panic!("expected pending stop, got {other:?}"),
        }
        assert_eq!(book.order_status(id(50)), Some(OrderStatus::Open));
        // No level changed.
        assert_eq!(book.best_bid(), Some(90));
        assert_eq!(book.best_ask(), Some(110));
    }

    #[test]
    fn test_modify_through_the_last_trade_elects_the_stop() {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 100, 1, Side::Sell, user(1)))
            .expect("ask");
        book.submit_market_order(id(3), 1, Side::Buy)
            .expect("trade at 100");
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 2))
            .expect("pending");
        book.update_order(OrderUpdate::UpdatePrice {
            order_id: id(50),
            new_price: Price::new(100),
        })
        .expect("modify");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 2 })
        );
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(8));
    }

    #[test]
    fn test_mass_cancel_scopes_cover_pending_stops_after_resting_orders() {
        let book = book_with_stops();
        let by_side = book.cancel_orders_by_side(Side::Sell);
        assert_eq!(by_side.cancelled_order_ids(), &[id(2), id(50), id(52)]);
        assert_eq!(
            book.order_status(id(52)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelBySide,
            })
        );

        let book = book_with_stops();
        let by_user = book.cancel_orders_by_user(user(2));
        assert_eq!(by_user.cancelled_order_ids(), &[id(2), id(51), id(52)]);
        assert_eq!(book.trailing_stop_ids(), vec![id(50)]);

        let book = book_with_stops();
        let by_range = book.cancel_orders_by_price_range(Side::Sell, 80, 95);
        assert_eq!(by_range.cancelled_order_ids(), &[id(50), id(52)]);
        let by_range = book.cancel_orders_by_price_range(Side::Buy, 100, 110);
        assert_eq!(by_range.cancelled_order_ids(), &[id(51)]);
        assert_eq!(book.trailing_stop_count(), 0);

        // A user with only pending stops.
        let book = new_book();
        book.add_order(stop_tif(
            70,
            Side::Sell,
            95,
            100,
            5,
            1,
            user(7),
            TimeInForce::Gtc,
        ))
        .expect("stop");
        assert_eq!(
            book.cancel_orders_by_user(user(7)).cancelled_order_ids(),
            &[id(70)]
        );

        let book = book_with_stops();
        let all = book.cancel_all_orders();
        assert_eq!(
            all.cancelled_order_ids(),
            &[id(1), id(2), id(50), id(51), id(52)]
        );
        assert_eq!(
            book.order_status(id(51)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::MassCancelAll,
            })
        );
        assert_eq!(book.trailing_stop_count(), 0);
        assert!(book.get_order(id(51)).is_none());
        // Fresh admissions after the bulk clear work.
        book.add_order(stop(80, Side::Sell, 95, 100, 5, 1))
            .expect("admitted after cancel_all");
        assert_eq!(book.trailing_stop_ids(), vec![id(80)]);
    }

    #[test]
    fn test_expiry_evicts_expired_pending_stops() {
        let book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            95,
            100,
            5,
            1,
            user(1),
            TimeInForce::Gtd(1_000),
        ))
        .expect("gtd stop");
        book.add_order(stop(51, Side::Sell, 95, 100, 5, 1))
            .expect("gtc stop");
        assert!(
            book.evict_expired_orders(TimestampMs::new(999))
                .expect("sweep")
                .is_empty()
        );
        let evicted = book
            .evict_expired_orders(TimestampMs::new(1_000))
            .expect("sweep");
        assert_eq!(evicted.evicted_order_ids(), &[id(50)]);
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::TimeInForceExpired,
            })
        );
        assert_eq!(book.trailing_stop_ids(), vec![id(51)]);
    }

    // ---- admission ------------------------------------------------------

    #[test]
    fn test_admission_checks_reject_untouched() {
        let mut book = new_book();
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(stop(50, Side::Sell, 95, 100, 5, 1))
            .expect("stop");
        assert!(matches!(
            book.add_order(stop(1, Side::Sell, 95, 100, 5, 1)),
            Err(OrderBookError::DuplicateOrderId { .. })
        ));
        assert!(matches!(
            book.add_order(stop(50, Side::Sell, 94, 100, 5, 1)),
            Err(OrderBookError::DuplicateOrderId { .. })
        ));
        assert!(matches!(
            book.add_order(limit(50, 80, 1, Side::Buy, user(1))),
            Err(OrderBookError::DuplicateOrderId { .. })
        ));
        for tif in [TimeInForce::Ioc, TimeInForce::Fok] {
            let err = book
                .add_order(stop_tif(60, Side::Sell, 95, 100, 5, 1, user(1), tif))
                .expect_err("immediate stops refused");
            assert!(
                matches!(err, OrderBookError::InvalidOperation { .. }),
                "{err:?}"
            );
        }
        book.set_tick_size(5);
        assert!(matches!(
            book.add_order(stop(61, Side::Sell, 95, 100, 3, 1)),
            Err(OrderBookError::InvalidTickSize {
                price: 3,
                tick_size: 5
            })
        ));
        assert!(matches!(
            book.add_order(stop(62, Side::Sell, 97, 100, 5, 1)),
            Err(OrderBookError::InvalidTickSize { price: 97, .. })
        ));
        book.engage_kill_switch();
        assert!(matches!(
            book.add_order(stop(63, Side::Sell, 95, 100, 5, 1)),
            Err(OrderBookError::KillSwitchActive)
        ));
        assert_eq!(
            book.order_status(id(63)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::KillSwitchActive
            })
        );
        assert_eq!(book.trailing_stop_ids(), vec![id(50)]);
    }

    #[test]
    fn test_stp_book_requires_a_user_on_stops() {
        let book = new_book();
        let mut book = book;
        book.set_stp_mode(STPMode::CancelTaker);
        assert!(matches!(
            book.add_order(stop_tif(
                50,
                Side::Sell,
                95,
                100,
                5,
                1,
                Hash32::zero(),
                TimeInForce::Gtc
            )),
            Err(OrderBookError::MissingUserId { .. })
        ));
    }

    // ---- risk -----------------------------------------------------------

    fn open_orders(book: &OrderBook<()>, account: Hash32) -> u64 {
        book.risk_state.counters.get(&account).map_or(0, |c| {
            c.open_count.load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    fn notional(book: &OrderBook<()>, account: Hash32) -> u128 {
        book.risk_state
            .counters
            .get(&account)
            .map_or(0, |c| c.resting_notional.load())
    }

    #[test]
    fn test_pending_stop_reserves_risk_until_trigger_or_cancel() {
        let mut book = new_book();
        book.set_risk_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(2)
                .with_max_notional_per_account(1_000),
        );
        let trader = user(3);
        book.add_order(limit(1, 90, 10, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 101, 1, Side::Sell, user(6)))
            .expect("ask");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            95,
            100,
            5,
            5,
            trader,
            TimeInForce::Gtc,
        ))
        .expect("stop");
        assert_eq!(open_orders(&book, trader), 1);
        assert_eq!(notional(&book, trader), 475, "5 x 95 at the stop price");

        // The limits apply with the stop counted.
        book.add_order(limit(3, 80, 6, Side::Buy, trader))
            .expect("second order");
        assert!(matches!(
            book.add_order(limit(4, 70, 1, Side::Buy, trader)),
            Err(OrderBookError::RiskMaxOpenOrders { .. })
        ));
        assert!(matches!(
            book.add_order(stop_tif(
                51,
                Side::Sell,
                90,
                100,
                5,
                5,
                user(4),
                TimeInForce::Gtc
            ))
            .and_then(|_| book.add_order(stop_tif(
                52,
                Side::Sell,
                90,
                100,
                5,
                12,
                user(4),
                TimeInForce::Gtc
            ))),
            Err(OrderBookError::RiskMaxNotional { .. })
        ));

        // A trail re-books the notional at the new stop price.
        book.submit_market_order(id(5), 1, Side::Buy)
            .expect("trade at 101");
        assert_eq!(stop_price(&book, 50), (96, 101));
        assert_eq!(notional(&book, trader), 5 * 96 + 80 * 6);

        // Cancel releases it (and the other account's stop is withdrawn).
        book.cancel_order(id(51)).expect("cancel other");
        book.cancel_order(id(50)).expect("cancel");
        assert_eq!(open_orders(&book, trader), 1);
        assert_eq!(notional(&book, trader), 480);

        // Trigger releases it too.
        book.add_order(stop_tif(
            53,
            Side::Sell,
            95,
            101,
            5,
            5,
            trader,
            TimeInForce::Gtc,
        ))
        .expect("stop");
        assert_eq!(open_orders(&book, trader), 2);
        book.submit_market_order(id(6), 1, Side::Sell)
            .expect("trade at 90");
        assert_eq!(
            book.order_status(id(53)),
            Some(OrderStatus::Filled { filled_quantity: 5 })
        );
        assert_eq!(open_orders(&book, trader), 1);
        assert_eq!(notional(&book, trader), 480);
        assert_eq!(book.risk_accounting_anomalies(), 0);
    }

    // ---- STP and kill switch at trigger ---------------------------------

    #[test]
    fn test_elected_stop_obeys_self_trade_prevention() {
        let mut book = new_book();
        book.set_stp_mode(STPMode::CancelTaker);
        let owner = user(5);
        book.add_order(limit(1, 94, 5, Side::Buy, owner))
            .expect("owner's bid");
        book.add_order(limit(2, 96, 1, Side::Buy, user(6)))
            .expect("other bid");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            97,
            100,
            3,
            2,
            owner,
            TimeInForce::Gtc,
        ))
        .expect("owner's stop");
        book.submit_market_order_with_user(id(3), 1, Side::Sell, user(7))
            .expect("trade at 96");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            })
        );
        assert_eq!(book.visible_quantity_at_price(94, Side::Buy), Some(5));

        // CancelMaker: the owner's bid is cancelled and the market order
        // walks on.
        let mut book = new_book();
        book.set_stp_mode(STPMode::CancelMaker);
        book.add_order(limit(1, 94, 5, Side::Buy, owner))
            .expect("owner's bid");
        book.add_order(limit(4, 93, 5, Side::Buy, user(6)))
            .expect("deeper bid");
        book.add_order(limit(2, 96, 1, Side::Buy, user(6)))
            .expect("other bid");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            97,
            100,
            3,
            2,
            owner,
            TimeInForce::Gtc,
        ))
        .expect("owner's stop");
        book.submit_market_order_with_user(id(3), 1, Side::Sell, user(7))
            .expect("trade at 96");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 2 })
        );
        assert!(book.get_order(id(1)).is_none(), "maker cancelled by STP");
        assert_eq!(book.visible_quantity_at_price(93, Side::Buy), Some(3));
    }

    #[test]
    fn test_kill_switch_at_trigger_rejects_the_market_order() {
        let book = new_book();
        book.add_order(limit(1, 94, 5, Side::Buy, user(1)))
            .expect("bid 94");
        book.add_order(limit(2, 96, 1, Side::Buy, user(1)))
            .expect("bid 96");
        book.add_order(stop(50, Side::Sell, 97, 100, 3, 2))
            .expect("stop");
        book.engage_kill_switch();
        // The raw match entry points are not kill-switch gated; the stop
        // they elect is, like any new flow.
        book.match_market_order(id(3), 1, Side::Sell)
            .expect("raw trade at 96");
        let child = book.stop_trigger_order_id(id(50));
        let rejected = Some(OrderStatus::Rejected {
            reason: RejectReason::KillSwitchActive,
        });
        assert_eq!(book.order_status(id(50)), rejected);
        assert_eq!(book.order_status(child), rejected);
        assert_eq!(book.trailing_stop_count(), 0);
        assert_eq!(book.visible_quantity_at_price(94, Side::Buy), Some(5));
    }

    // ---- snapshot -------------------------------------------------------

    /// A book with a trade, two pending stops, risk and a fee schedule.
    fn stateful_book() -> OrderBook<()> {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(100));
        book.add_order(limit(1, 90, 20, Side::Buy, user(1)))
            .expect("bid");
        book.add_order(limit(2, 100, 1, Side::Sell, user(1)))
            .expect("ask 100");
        book.add_order(limit(3, 104, 5, Side::Sell, user(1)))
            .expect("ask 104");
        book.submit_market_order(id(4), 1, Side::Buy)
            .expect("trade at 100");
        book.add_order(stop_tif(
            50,
            Side::Sell,
            95,
            100,
            5,
            3,
            user(2),
            TimeInForce::Gtc,
        ))
        .expect("sell stop");
        book.add_order(stop_tif(
            51,
            Side::Buy,
            110,
            100,
            10,
            2,
            user(2),
            TimeInForce::Gtc,
        ))
        .expect("buy stop");
        book
    }

    #[test]
    fn test_snapshot_v5_round_trips_pending_stops_and_last_trade() {
        let live = stateful_book();
        let json = live.snapshot_to_json(usize::MAX).expect("json");
        let package = OrderBookSnapshotPackage::from_json(&json).expect("parse");
        assert_eq!(package.version, 5);
        assert_eq!(package.snapshot.pending_stops.len(), 2);
        assert_eq!(package.snapshot.last_trade_price, Some(100));

        // The restored book's clock runs ahead of the live one, so the
        // level statistics of its first execution stay coherent.
        let clock = Arc::new(StubClock::starting_at(1_000)) as Arc<dyn Clock>;
        let mut restored = OrderBook::with_clock_and_namespace(SYMBOL, clock, namespace());
        restored.restore_from_snapshot_json(&json).expect("restore");
        let a = live.create_snapshot(usize::MAX).expect("snapshot");
        let b = restored.create_snapshot(usize::MAX).expect("snapshot");
        assert!(snapshots_match(&b, &a));
        assert_eq!(restored.trailing_stop_ids(), vec![id(50), id(51)]);
        assert_eq!(notional(&restored, user(2)), notional(&live, user(2)));
        assert_eq!(open_orders(&restored, user(2)), 2);

        // Both books evolve identically from here.
        for book in [&live, &restored] {
            book.submit_market_order(id(10), 3, Side::Buy)
                .expect("trade at 104");
        }
        assert_eq!(stop_price(&live, 50), (99, 104));
        let a = live.create_snapshot(usize::MAX).expect("snapshot");
        let b = restored.create_snapshot(usize::MAX).expect("snapshot");
        assert!(
            snapshots_match(&b, &a),
            "{}\n{}",
            serde_json::to_string(&a).expect("json"),
            serde_json::to_string(&b).expect("json")
        );

        // Tampering with a pending stop breaks the v5 checksum.
        let mut tampered = OrderBookSnapshotPackage::from_json(&json).expect("parse");
        if let Some(OrderType::TrailingStop { price, .. }) =
            tampered.snapshot.pending_stops.first_mut()
        {
            *price = Price::new(96);
        }
        assert!(matches!(
            tampered.validate(),
            Err(OrderBookError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn test_pre_v5_package_cannot_carry_pending_stops() {
        let live = stateful_book();
        let mut package = live.create_snapshot_package(usize::MAX).expect("package");
        package.version = 4;
        assert!(matches!(
            package.validate(),
            Err(OrderBookError::InvalidOperation { .. })
        ));
    }

    #[test]
    fn test_restore_refuses_an_unsettled_pending_stop() {
        let live = stateful_book();
        let mut snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        // A sell stop at or above the last trade (100) would trigger on the
        // next unrelated call.
        if let Some(OrderType::TrailingStop { price, .. }) = snapshot.pending_stops.first_mut() {
            *price = Price::new(100);
        }
        let target = new_book();
        let err = target
            .restore_from_snapshot(snapshot.clone())
            .expect_err("unsettled");
        assert!(
            matches!(err, OrderBookError::InvalidOperation { .. }),
            "{err:?}"
        );
        // A watermark behind the last trade is unsettled too.
        let mut snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        if let Some(OrderType::TrailingStop {
            last_reference_price,
            ..
        }) = snapshot.pending_stops.first_mut()
        {
            *last_reference_price = Price::new(99);
        }
        assert!(target.restore_from_snapshot(snapshot).is_err());
        // A pending stop whose id rests on a level is a duplicate.
        let mut snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        snapshot
            .pending_stops
            .push(stop(1, Side::Sell, 50, 100, 50, 1));
        assert!(matches!(
            target.restore_from_snapshot(snapshot),
            Err(OrderBookError::DuplicateOrderId { .. })
        ));
        assert!(target.best_bid().is_none(), "target untouched");
        assert_eq!(target.trailing_stop_count(), 0);
    }

    /// A verbatim `version: 4` package written on `main` before #286
    /// (5925a7a): asks 10 @ 100 (id 1) and 7 @ 101 (id 3), bid 9 @ 99
    /// (id 4), then a market buy of 4 that traded at 100. It keeps its
    /// original checksum and restores with no pending stop and no last
    /// trade price (v4 did not carry one).
    #[test]
    fn test_verbatim_v4_package_still_restores() {
        let fixture = include_str!("fixtures/snapshot_v4_0_14_pre286.json");
        let package = OrderBookSnapshotPackage::from_json(fixture).expect("parse v4");
        assert_eq!(package.version, 4);
        assert!(package.validate().is_ok(), "original checksum holds");
        let mut restored = OrderBook::<()>::new("BTC/USD");
        restored
            .restore_from_snapshot_package(package)
            .expect("v4 restores");
        assert_eq!(restored.best_ask(), Some(100));
        assert_eq!(restored.best_bid(), Some(99));
        assert_eq!(restored.visible_quantity_at_price(100, Side::Sell), Some(6));
        assert_eq!(restored.last_trade_price(), None);
        assert_eq!(restored.trailing_stop_count(), 0);
        // Re-packaged as v5.
        let json = restored.snapshot_to_json(usize::MAX).expect("json");
        assert_eq!(
            OrderBookSnapshotPackage::from_json(&json)
                .expect("parse")
                .version,
            5
        );
    }

    /// A verbatim `version: 4` package written on `main` before #286 whose
    /// ask side holds a trailing stop resting at 105 (the pre-0.14 model).
    /// It cannot be restored faithfully and is refused untouched.
    #[test]
    fn test_v4_package_with_a_resting_trailing_stop_is_refused() {
        let fixture = include_str!("fixtures/snapshot_v4_resting_trailing_stop.json");
        let package = OrderBookSnapshotPackage::from_json(fixture).expect("parse v4");
        assert!(package.validate().is_ok());
        let mut target = OrderBook::<()>::new("BTC/USD");
        target
            .add_order(limit(1, 50, 1, Side::Buy, user(1)))
            .expect("existing order");
        let err = target
            .restore_from_snapshot_package(package)
            .expect_err("resting stop refused");
        assert!(
            matches!(err, OrderBookError::StopOrdersUnsupported { order_id } if order_id == id(7)),
            "{err:?}"
        );
        assert_eq!(target.best_bid(), Some(50), "target untouched");
    }

    // ---- journal and replay ---------------------------------------------

    #[test]
    fn test_replay_reproduces_triggers_and_cascades() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut live = OrderBook::with_clock_and_namespace(SYMBOL, Arc::clone(&clock), namespace());
        live.set_fee_schedule(Some(FeeSchedule::new(-1, 3)));
        let live_trades = record_trades(&mut live);
        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        let mut seq = 0u64;
        let mut record = |command: SequencerCommand<()>, result: SequencerResult| {
            journal
                .append(&SequencerEvent {
                    sequence_num: seq,
                    timestamp_ns: seq,
                    command,
                    result,
                })
                .expect("append");
            seq += 1;
        };

        let adds = [
            limit(1, 99, 1, Side::Buy, user(1)),
            limit(2, 97, 1, Side::Buy, user(1)),
            limit(3, 95, 1, Side::Buy, user(1)),
            limit(4, 90, 100, Side::Buy, user(1)),
            limit(5, 100, 1, Side::Sell, user(1)),
            limit(6, 103, 3, Side::Sell, user(1)),
            limit(7, 120, 50, Side::Sell, user(1)),
        ];
        for order in adds {
            live.add_order(order).expect("add");
            record(
                SequencerCommand::AddOrder(order),
                SequencerResult::OrderAdded {
                    order_id: order.id(),
                },
            );
        }
        let market = |live: &OrderBook<()>, raw: u64, qty: u64, side: Side| {
            live.submit_market_order(id(raw), qty, side)
                .expect("market");
            SequencerCommand::MarketOrder {
                id: id(raw),
                quantity: qty,
                side,
            }
        };
        let command = market(&live, 10, 1, Side::Buy);
        record(command, SequencerResult::OrderAdded { order_id: id(10) });
        for order in [
            stop(51, Side::Sell, 98, 100, 2, 1),
            stop(52, Side::Sell, 96, 100, 4, 1),
            stop(53, Side::Sell, 94, 100, 6, 1),
            stop(54, Side::Buy, 110, 100, 10, 2),
        ] {
            live.add_order(order).expect("stop");
            record(
                SequencerCommand::AddOrder(order),
                SequencerResult::OrderAdded {
                    order_id: order.id(),
                },
            );
        }
        // Trail the buy stop down, trail the sells up, then cascade.
        let command = market(&live, 11, 3, Side::Buy);
        record(command, SequencerResult::OrderAdded { order_id: id(11) });
        let command = market(&live, 12, 2, Side::Sell);
        record(command, SequencerResult::OrderAdded { order_id: id(12) });
        let update = OrderUpdate::UpdateQuantity {
            order_id: id(54),
            new_quantity: Quantity::new(4),
        };
        live.update_order(update).expect("modify stop");
        record(
            SequencerCommand::UpdateOrder(update),
            SequencerResult::OrderUpdated { order_id: id(54) },
        );
        assert!(taken(&live_trades).len() >= 4, "cascade executed");

        let config = ReplayBookConfig::new(
            Some(FeeSchedule::new(-1, 3)),
            STPMode::None,
            None,
            None,
            None,
            None,
        )
        .with_trade_id_namespace(namespace());
        let (replayed, last) = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal, 0, SYMBOL, clock, &config,
        )
        .expect("replay");
        assert_eq!(last, seq - 1);
        let live_snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        assert!(!live_snapshot.pending_stops.is_empty(), "stop 54 pending");
        assert!(snapshots_match(
            &replayed.create_snapshot(usize::MAX).expect("snapshot"),
            &live_snapshot
        ));
        assert!(
            ReplayEngine::<()>::verify(&journal, &live_snapshot).expect("verify runs"),
            "default-config verify agrees too (no fees change the book here)"
        );
    }

    // ---- determinism and concurrency ------------------------------------

    fn scripted_trades() -> Vec<String> {
        let mut book = new_book();
        let trades = record_trades(&mut book);
        for (raw, price, qty) in [(1, 99, 2), (2, 98, 2), (3, 96, 2), (4, 90, 50)] {
            book.add_order(limit(raw, price, qty, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(limit(5, 101, 1, Side::Sell, user(1)))
            .expect("ask");
        book.submit_market_order(id(6), 1, Side::Buy)
            .expect("trade");
        for raw in 50..60u64 {
            let stop_px = 100 - (raw - 50) as u128;
            book.add_order(stop(raw, Side::Sell, stop_px, 101, 101 - stop_px as u64, 1))
                .expect("stop");
        }
        book.submit_market_order(id(7), 3, Side::Sell)
            .expect("sweep");
        taken(&trades)
    }

    #[test]
    fn test_identical_streams_emit_identical_trades() {
        let first = scripted_trades();
        assert!(first.len() > 3, "{first:?}");
        for _ in 0..5 {
            assert_eq!(scripted_trades(), first);
        }
    }

    #[test]
    fn test_concurrent_takers_elect_each_stop_exactly_once() {
        const THREADS: usize = 4;
        let book = Arc::new(new_book());
        book.add_order(limit(1, 50, 100_000, Side::Buy, user(1)))
            .expect("deep bid");
        book.add_order(limit(2, 200, 100_000, Side::Sell, user(1)))
            .expect("deep ask");
        book.add_order(limit(3, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.submit_market_order(id(4), 1, Side::Sell)
            .expect("trade at 100");
        for raw in 100..140u64 {
            book.add_order(stop(raw, Side::Sell, 60, 100, 40, 1))
                .expect("stop");
        }
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let book = Arc::clone(&book);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for n in 0..20u64 {
                        let taker = id(10_000 + (t as u64) * 100 + n);
                        let _ = book.submit_market_order(taker, 1, Side::Sell);
                        let _ = book.cancel_order(id(1_000_000));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(book.trailing_stop_count(), 0, "every stop elected");
        for raw in 100..140u64 {
            assert_eq!(
                book.order_status(id(raw)),
                Some(OrderStatus::Filled { filled_quantity: 1 }),
                "stop {raw} elected once"
            );
        }
        // 80 takers + 40 stops, one unit each, all at 50.
        assert_eq!(
            book.visible_quantity_at_price(50, Side::Buy),
            Some(100_000 - 80 - 40)
        );
    }
}

#[cfg(all(test, not(feature = "special_orders")))]
// tests may panic: rules/global_rules.md § Testing
mod without_special_orders {
    use crate::OrderBookError;
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::order_state::{OrderStateTracker, OrderStatus};
    use crate::orderbook::reject_reason::RejectReason;
    use crate::orderbook::sequencer::SequencerResult;
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};

    fn trailing(raw: u64) -> OrderType<()> {
        OrderType::TrailingStop {
            id: Id::from_u64(raw),
            price: Price::new(95),
            quantity: Quantity::new(3),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(5),
            last_reference_price: Price::new(100),
            extra_fields: (),
        }
    }

    /// #286: without `special_orders` a trailing stop is rejected untouched
    /// with code 23 (it used to rest as a limit order at its stop price).
    #[test]
    fn test_trailing_stop_is_rejected_untouched() {
        let mut book = OrderBook::<()>::new("NOSTOP");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_limit_order(Id::from_u64(1), 96, 5, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid above the stop price");
        let err = book.add_order(trailing(50)).expect_err("rejected");
        assert!(
            matches!(err, OrderBookError::StopOrdersUnsupported { order_id } if order_id == Id::from_u64(50)),
            "{err:?}"
        );
        assert_eq!(
            RejectReason::from(&err),
            RejectReason::StopOrdersUnsupported
        );
        assert_eq!(RejectReason::from(&err).as_u16(), 23);
        assert!(matches!(
            SequencerResult::from(&err),
            SequencerResult::RejectedWithCode {
                code: RejectReason::StopOrdersUnsupported,
                may_have_mutated: false,
                ..
            }
        ));
        assert_eq!(
            book.order_status(Id::from_u64(50)),
            Some(OrderStatus::Rejected {
                reason: RejectReason::StopOrdersUnsupported
            })
        );
        // Untouched: the bid at 96 did not trade against a sell at 95.
        assert_eq!(book.last_trade_price(), None);
        assert_eq!(book.visible_quantity_at_price(96, Side::Buy), Some(5));
        assert!(book.get_order(Id::from_u64(50)).is_none());
        assert_eq!(book.best_ask(), None);
        // Every submit entry point that takes an order refuses it.
        assert!(matches!(
            book.add_order_with_result(trailing(51)),
            Err(OrderBookError::StopOrdersUnsupported { .. })
        ));
        assert!(
            book.add_order_with_committed(trailing(52))
                .is_err_and(|f| matches!(f.error, OrderBookError::StopOrdersUnsupported { .. }))
        );
    }

    /// A snapshot carrying pending stops cannot be restored without the
    /// feature; the target is untouched.
    #[test]
    fn test_snapshot_with_pending_stops_is_refused() {
        let book = OrderBook::<()>::new("NOSTOP");
        let mut snapshot = book.create_snapshot(usize::MAX).expect("snapshot");
        snapshot.pending_stops.push(trailing(50));
        let target = OrderBook::<()>::new("NOSTOP");
        target
            .add_limit_order(Id::from_u64(1), 90, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("existing");
        assert!(matches!(
            target.restore_from_snapshot(snapshot),
            Err(OrderBookError::StopOrdersUnsupported { .. })
        ));
        assert_eq!(target.best_bid(), Some(90));
    }
}
