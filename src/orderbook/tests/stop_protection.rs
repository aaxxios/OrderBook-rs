//! #302: protection collar for elected stop orders.
//!
//! With `special_orders`: without a collar an elected stop's child is the
//! unpriced IOC market order of 0.14 (a thin-book cascade sweeps to the last
//! level); with one the child is an IOC limit at `stop - collar` (sell) /
//! `stop + collar` (buy), trades only within that band, cancels the rest and
//! never rests. Covers partial and empty bands, bound clamping, tick-size
//! validation, STP, snapshot v6 round trip (and a verbatim v5 package from
//! 0.14.0), and replay. Without the feature the collar is still configurable
//! and snapshotted.

#[cfg(all(test, feature = "special_orders"))]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::sequencer::{
        InMemoryJournal, Journal, ReplayBookConfig, ReplayEngine, SequencerCommand, SequencerEvent,
        SequencerResult, snapshots_match,
    };
    use crate::orderbook::snapshot::OrderBookSnapshotPackage;
    use crate::orderbook::stop_protection::StopProtection;
    use crate::orderbook::stp::STPMode;
    use crate::orderbook::trade::TradeResult;
    use crate::{Clock, FeeSchedule, OrderBookError, StubClock};
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    const SYMBOL: &str = "COLLAR/USD";

    fn id(raw: u64) -> Id {
        Id::from_u64(raw)
    }

    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    fn namespace() -> Uuid {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, b"stop-protection-302")
    }

    fn collar(raw: u128) -> Option<StopProtection> {
        Some(StopProtection::try_new(raw).expect("non-zero collar"))
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

    /// A trailing stop; the watermark sits `trail` away from the stop.
    fn stop(
        raw: u64,
        side: Side,
        stop: u128,
        trail: u64,
        qty: u64,
        owner: Hash32,
    ) -> OrderType<()> {
        let watermark = match side {
            Side::Sell => stop + u128::from(trail),
            Side::Buy => stop - u128::from(trail),
        };
        OrderType::TrailingStop {
            id: id(raw),
            price: Price::new(stop),
            quantity: Quantity::new(qty),
            side,
            user_id: owner,
            timestamp: TimestampMs::new(raw),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(trail),
            last_reference_price: Price::new(watermark),
            extra_fields: (),
        }
    }

    fn new_book() -> OrderBook<()> {
        let clock = Arc::new(StubClock::starting_at(0)) as Arc<dyn Clock>;
        let mut book = OrderBook::with_clock_and_namespace(SYMBOL, clock, namespace());
        book.set_order_state_tracker(OrderStateTracker::with_capacity(10_000));
        book
    }

    /// One fill: taker, the stop it descends from, price, quantity.
    type Fill = (Id, Option<Id>, u128, u64);
    type Fills = Arc<Mutex<Vec<Fill>>>;

    fn record_fills(book: &mut OrderBook<()>) -> Fills {
        let log: Fills = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            let mut sink = sink.lock().expect("log");
            for trade in result.match_result.trades().as_vec() {
                sink.push((
                    result.match_result.order_id(),
                    result.origin_stop_id,
                    trade.price().as_u128(),
                    trade.quantity().as_u64(),
                ));
            }
        }));
        log
    }

    fn fills(log: &Fills) -> Vec<Fill> {
        log.lock().expect("log").clone()
    }

    /// The fills of the child of stop `raw`, as `(price, qty)`.
    fn child_fills(log: &Fills, raw: u64) -> Vec<(u128, u64)> {
        fills(log)
            .into_iter()
            .filter(|fill| fill.1 == Some(id(raw)))
            .map(|fill| (fill.2, fill.3))
            .collect()
    }

    /// A thin bid ladder under three sell stops (100 x2, 99 x3, 96 x5):
    /// bids 100, 99, 98, 97, 96, 94 (one each) and 90 x5.
    fn thin_sell_book(protection: Option<StopProtection>) -> (OrderBook<()>, Fills) {
        let mut book = new_book();
        book.set_stop_protection(protection).expect("collar");
        let log = record_fills(&mut book);
        for (raw, price) in [(1, 100), (2, 99), (3, 98), (4, 97), (5, 96), (6, 94)] {
            book.add_order(limit(raw, price, 1, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(limit(7, 90, 5, Side::Buy, user(1)))
            .expect("bid 90");
        book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
            .expect("stop 50");
        book.add_order(stop(51, Side::Sell, 99, 5, 3, user(2)))
            .expect("stop 51");
        book.add_order(stop(52, Side::Sell, 96, 5, 5, user(2)))
            .expect("stop 52");
        // One unit at 100 elects the first stop.
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        (book, log)
    }

    // ---- default: unchanged 0.14 behaviour ---------------------------------

    #[test]
    fn test_stop_protection_default_is_unset() {
        let book = new_book();
        assert_eq!(book.stop_protection(), None);
        let package = book.create_snapshot_package(10).expect("package");
        assert_eq!(package.stop_protection, None);
    }

    #[test]
    fn test_cascade_without_collar_sweeps_to_the_last_bid() {
        let (book, log) = thin_sell_book(None);
        assert_eq!(child_fills(&log, 50), vec![(99, 1), (98, 1)]);
        assert_eq!(child_fills(&log, 51), vec![(97, 1), (96, 1), (94, 1)]);
        assert_eq!(child_fills(&log, 52), vec![(90, 5)]);
        for raw in [50, 51, 52] {
            assert!(matches!(
                book.order_status(id(raw)),
                Some(OrderStatus::Filled { .. })
            ));
        }
        assert_eq!(book.best_bid(), None, "the bid side is swept out");
        assert_eq!(book.last_trade_price(), Some(90));
    }

    // ---- the collar bounds every child -------------------------------------

    #[test]
    fn test_sell_cascade_with_collar_never_trades_below_stop_minus_collar() {
        let (book, log) = thin_sell_book(collar(2));
        // Stop 50 (100): limit 98, fills 99 and 98 (its whole quantity).
        assert_eq!(child_fills(&log, 50), vec![(99, 1), (98, 1)]);
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Filled { filled_quantity: 2 })
        );
        // Stop 51 (99), elected by the 99 print: limit 97, fills 97 and
        // the remaining 2 are cancelled at 96 (below its band).
        assert_eq!(child_fills(&log, 51), vec![(97, 1)]);
        assert_eq!(
            book.order_status(id(51)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::StopProtectionBand,
            })
        );
        // The cascade stopped at 97: stop 52 (96) is still pending.
        assert_eq!(book.trailing_stop_ids(), vec![id(52)]);
        assert_eq!(book.order_status(id(52)), Some(OrderStatus::Open));
        assert_eq!(book.last_trade_price(), Some(97));
        // Every child fill is within its stop's band.
        for (stop_id, stop_price) in [(50, 100u128), (51, 99)] {
            for (price, _) in child_fills(&log, stop_id) {
                assert!(price >= stop_price - 2, "{price} below the band");
            }
        }
        // No child rests: the bids below the bands are untouched and no
        // ask appeared.
        assert_eq!(book.best_bid(), Some(96));
        assert_eq!(book.visible_quantity_at_price(94, Side::Buy), Some(1));
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(5));
        assert_eq!(book.best_ask(), None);
        for raw in [50, 51] {
            assert!(
                book.get_order(book.stop_trigger_order_id(id(raw)))
                    .is_none()
            );
        }
    }

    #[test]
    fn test_buy_cascade_with_collar_never_trades_above_stop_plus_collar() {
        let mut book = new_book();
        book.set_stop_protection(collar(2)).expect("collar");
        let log = record_fills(&mut book);
        for (raw, price) in [(1, 100), (2, 101), (3, 102), (4, 103), (5, 104), (6, 106)] {
            book.add_order(limit(raw, price, 1, Side::Sell, user(1)))
                .expect("ask");
        }
        book.add_order(limit(7, 110, 5, Side::Sell, user(1)))
            .expect("ask 110");
        book.add_order(stop(50, Side::Buy, 100, 5, 2, user(2)))
            .expect("stop 50");
        book.add_order(stop(51, Side::Buy, 101, 5, 3, user(2)))
            .expect("stop 51");
        book.add_order(stop(52, Side::Buy, 104, 5, 5, user(2)))
            .expect("stop 52");
        book.submit_market_order(id(9), 1, Side::Buy)
            .expect("trade at 100");

        assert_eq!(child_fills(&log, 50), vec![(101, 1), (102, 1)]);
        assert_eq!(child_fills(&log, 51), vec![(103, 1)]);
        assert_eq!(
            book.order_status(id(51)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::StopProtectionBand,
            })
        );
        assert_eq!(book.trailing_stop_ids(), vec![id(52)]);
        assert_eq!(book.best_ask(), Some(104));
        assert_eq!(book.best_bid(), None, "no child rests");
    }

    #[test]
    fn test_empty_sell_band_cancels_the_stop_unfilled_with_band_reason() {
        let mut book = new_book();
        book.set_stop_protection(collar(5)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 90, 5, Side::Buy, user(1)))
            .expect("bid 90");
        book.add_order(stop(50, Side::Sell, 100, 5, 3, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");

        let child = book.stop_trigger_order_id(id(50));
        assert!(child_fills(&log, 50).is_empty(), "limit 95, best bid 90");
        let history: Vec<OrderStatus> = book
            .get_order_history(id(50))
            .expect("tracked")
            .into_iter()
            .map(|(_, status)| status)
            .collect();
        assert_eq!(
            history,
            vec![
                OrderStatus::Open,
                OrderStatus::Triggered {
                    child_id: child,
                    trigger_price: 100,
                    limit_price: Some(95),
                },
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::StopProtectionBand,
                },
            ]
        );
        assert_eq!(book.trailing_stop_count(), 0);
        assert!(book.get_order(child).is_none(), "the child never rests");
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.visible_quantity_at_price(90, Side::Buy), Some(5));
        assert_eq!(book.last_trade_price(), Some(100));
    }

    /// The reference is the stop price at election (trailed), not the print
    /// that elected it: a gap below the stop leaves a band above the print.
    #[test]
    fn test_collar_reference_is_the_trailed_stop_price() {
        let mut book = new_book();
        book.set_stop_protection(collar(3)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 110, 1, Side::Sell, user(1)))
            .expect("ask 110");
        book.add_order(limit(2, 104, 1, Side::Buy, user(1)))
            .expect("bid 104");
        book.add_order(limit(3, 101, 2, Side::Buy, user(1)))
            .expect("bid 101");
        // Stop 95 (watermark 100, trail 5).
        book.add_order(stop(50, Side::Sell, 95, 5, 2, user(2)))
            .expect("stop");
        // A print at 110 trails it to 105.
        book.submit_market_order(id(9), 1, Side::Buy)
            .expect("trade at 110");
        assert_eq!(book.last_trade_price(), Some(110));
        assert_eq!(
            book.get_order(id(50)).map(|o| o.price()),
            Some(Price::new(105))
        );
        // A print at 104 elects it. Its limit is 105 - 3 = 102, so the bid
        // at 101 is outside the band (the admitted stop 95 would give 92,
        // the print 104 would give 101: both would fill it).
        book.submit_market_order(id(10), 1, Side::Sell)
            .expect("trade at 104");
        assert!(child_fills(&log, 50).is_empty());
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::StopProtectionBand,
            })
        );
        assert_eq!(book.visible_quantity_at_price(101, Side::Buy), Some(2));
    }

    // ---- bounds of the band -------------------------------------------------

    #[test]
    fn test_sell_collar_above_the_stop_price_is_unbounded_below() {
        for (collar_units, filled) in [(10u128, true), (1, false)] {
            let mut book = new_book();
            book.set_stop_protection(collar(collar_units))
                .expect("collar");
            let log = record_fills(&mut book);
            book.add_order(limit(1, 3, 1, Side::Buy, user(1)))
                .expect("bid 3");
            book.add_order(limit(2, 1, 1, Side::Buy, user(1)))
                .expect("bid 1");
            book.add_order(stop(50, Side::Sell, 3, 5, 1, user(2)))
                .expect("stop");
            book.submit_market_order(id(9), 1, Side::Sell)
                .expect("trade at 3");
            if filled {
                // 3 - 10 underflows: limit 0, every bid is in the band.
                assert_eq!(child_fills(&log, 50), vec![(1, 1)]);
                assert_eq!(
                    book.order_status(id(50)),
                    Some(OrderStatus::Filled { filled_quantity: 1 })
                );
            } else {
                // Limit 2: the bid at 1 is outside.
                assert!(child_fills(&log, 50).is_empty());
                assert_eq!(book.visible_quantity_at_price(1, Side::Buy), Some(1));
            }
        }
    }

    #[test]
    fn test_buy_collar_past_u128_max_is_unbounded_above() {
        let top = u128::MAX;
        for (collar_units, filled) in [(10u128, true), (1, false)] {
            let mut book = new_book();
            book.set_stop_protection(collar(collar_units))
                .expect("collar");
            let log = record_fills(&mut book);
            book.add_order(limit(1, top - 2, 1, Side::Sell, user(1)))
                .expect("ask top - 2");
            book.add_order(limit(2, top, 1, Side::Sell, user(1)))
                .expect("ask top");
            book.add_order(stop(50, Side::Buy, top - 2, 10, 1, user(2)))
                .expect("stop");
            book.submit_market_order(id(9), 1, Side::Buy)
                .expect("trade at top - 2");
            if filled {
                // top - 2 + 10 overflows: limit u128::MAX.
                assert_eq!(child_fills(&log, 50), vec![(top, 1)]);
            } else {
                // Limit top - 1: the ask at top is outside.
                assert!(child_fills(&log, 50).is_empty());
                assert_eq!(book.best_ask(), Some(top));
            }
            assert_eq!(book.trailing_stop_count(), 0);
        }
    }

    // ---- configuration --------------------------------------------------------

    #[test]
    fn test_collar_must_be_a_multiple_of_the_tick_size() {
        let mut book = new_book();
        book.set_tick_size(5);
        let err = book
            .set_stop_protection(collar(12))
            .expect_err("12 is not a multiple of 5");
        assert!(
            matches!(
                err,
                OrderBookError::InvalidTickSize {
                    price: 12,
                    tick_size: 5
                }
            ),
            "{err:?}"
        );
        assert_eq!(book.stop_protection(), None, "nothing installed");
        book.set_stop_protection(collar(10)).expect("aligned");
        assert!(book.set_stop_protection(collar(7)).is_err());
        assert_eq!(book.stop_protection(), collar(10), "previous collar kept");
        assert!(matches!(
            StopProtection::try_new(0),
            Err(OrderBookError::InvalidStopProtection { collar: 0, .. })
        ));

        // A later tick size does not re-validate it (documented): the collar
        // still bounds execution exactly.
        book.set_tick_size(4);
        assert_eq!(book.stop_protection(), collar(10));
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 92, 1, Side::Buy, user(1)))
            .expect("bid 92");
        book.add_order(limit(3, 88, 1, Side::Buy, user(1)))
            .expect("bid 88");
        book.add_order(stop(50, Side::Sell, 100, 8, 2, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        assert_eq!(child_fills(&log, 50), vec![(92, 1)], "limit 90");
        assert_eq!(book.visible_quantity_at_price(88, Side::Buy), Some(1));

        book.set_stop_protection(None).expect("clear");
        assert_eq!(book.stop_protection(), None);
    }

    #[test]
    fn test_collar_installed_with_stops_pending_applies_at_election() {
        let mut book = new_book();
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 99, 1, Side::Buy, user(1)))
            .expect("bid 99");
        book.add_order(limit(3, 97, 1, Side::Buy, user(1)))
            .expect("bid 97");
        book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
            .expect("stop");
        book.set_stop_protection(collar(1)).expect("collar");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        assert_eq!(child_fills(&log, 50), vec![(99, 1)]);
        assert_eq!(book.visible_quantity_at_price(97, Side::Buy), Some(1));
    }

    // ---- self-trade prevention --------------------------------------------

    #[test]
    fn test_collar_child_obeys_self_trade_prevention() {
        let mut book = new_book();
        book.set_stp_mode(STPMode::CancelMaker);
        book.set_stop_protection(collar(2)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 99, 1, Side::Buy, user(2)))
            .expect("own bid 99");
        book.add_order(limit(3, 98, 1, Side::Buy, user(1)))
            .expect("bid 98");
        book.add_order(limit(4, 96, 1, Side::Buy, user(1)))
            .expect("bid 96");
        book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
            .expect("stop");
        book.submit_market_order_with_user(id(9), 1, Side::Sell, user(3))
            .expect("trade at 100");

        // Own bid at 99 cancelled by STP, 98 filled, 96 outside the band.
        assert_eq!(child_fills(&log, 50), vec![(98, 1)]);
        assert!(book.get_order(id(2)).is_none(), "own maker cancelled");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::StopProtectionBand,
            })
        );
        assert_eq!(book.visible_quantity_at_price(96, Side::Buy), Some(1));

        // CancelTaker: the child meets its own bid first and is cancelled.
        let mut book = new_book();
        book.set_stp_mode(STPMode::CancelTaker);
        book.set_stop_protection(collar(2)).expect("collar");
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 99, 1, Side::Buy, user(2)))
            .expect("own bid 99");
        book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
            .expect("stop");
        book.submit_market_order_with_user(id(9), 1, Side::Sell, user(3))
            .expect("trade at 100");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            })
        );
        assert_eq!(book.visible_quantity_at_price(99, Side::Buy), Some(1));
    }

    // ---- snapshot ---------------------------------------------------------------

    #[test]
    fn test_snapshot_v6_round_trips_the_collar() {
        let mut live = new_book();
        live.set_tick_size(1);
        live.set_stop_protection(collar(2)).expect("collar");
        for (raw, price) in [(1, 100), (2, 99), (3, 98), (4, 96)] {
            live.add_order(limit(raw, price, 1, Side::Buy, user(1)))
                .expect("bid");
        }
        live.add_order(stop(50, Side::Sell, 100, 5, 3, user(2)))
            .expect("stop");

        let json = live.snapshot_to_json(usize::MAX).expect("json");
        assert!(json.contains(r#""stop_protection":{"collar":2}"#), "{json}");
        let package = OrderBookSnapshotPackage::from_json(&json).expect("parse");
        assert_eq!(package.version, 6);
        assert_eq!(package.stop_protection, collar(2));

        let clock = Arc::new(StubClock::starting_at(1_000)) as Arc<dyn Clock>;
        let mut restored = OrderBook::with_clock_and_namespace(SYMBOL, clock, namespace());
        restored.set_order_state_tracker(OrderStateTracker::new());
        restored.restore_from_snapshot_json(&json).expect("restore");
        assert_eq!(restored.stop_protection(), collar(2));

        // Both elect the stop identically: limit 98, 96 untouched.
        for book in [&live, &restored] {
            book.submit_market_order(id(9), 1, Side::Sell)
                .expect("trade at 100");
            assert_eq!(book.visible_quantity_at_price(96, Side::Buy), Some(1));
            assert_eq!(
                book.order_status(id(50)),
                Some(OrderStatus::Cancelled {
                    filled_quantity: 2,
                    reason: CancelReason::StopProtectionBand,
                })
            );
        }
        assert!(snapshots_match(
            &restored.create_snapshot(usize::MAX).expect("snapshot"),
            &live.create_snapshot(usize::MAX).expect("snapshot")
        ));

        // A restore replaces a collar with the package's (here: none).
        let plain = new_book().snapshot_to_json(usize::MAX).expect("json");
        let mut target = OrderBook::<()>::new(SYMBOL);
        target.set_stop_protection(collar(9)).expect("collar");
        target.restore_from_snapshot_json(&plain).expect("restore");
        assert_eq!(target.stop_protection(), None, "no silent carry-over");
    }

    #[test]
    fn test_snapshot_collar_validation() {
        let mut live = new_book();
        live.set_stop_protection(collar(4)).expect("collar");
        let package = live.create_snapshot_package(10).expect("package");

        // A package labelled below v6 cannot carry a collar.
        let mut old = package.clone().relabelled_for_test(5).expect("relabel");
        assert!(matches!(
            old.validate(),
            Err(OrderBookError::InvalidOperation { .. })
        ));
        old.stop_protection = None;
        assert!(old.validate().is_ok(), "a v5 package without one is fine");

        // A zero collar does not decode.
        let json = package.to_json().expect("json");
        let zeroed = json.replace(
            r#""stop_protection":{"collar":4}"#,
            r#""stop_protection":{"collar":0}"#,
        );
        assert_ne!(zeroed, json);
        assert!(matches!(
            OrderBookSnapshotPackage::from_json(&zeroed),
            Err(OrderBookError::DeserializationError { .. })
        ));

        // A collar no longer aligned to the tick size (tick changed after
        // it was set) restores as captured.
        live.set_tick_size(3);
        let json = live.snapshot_to_json(10).expect("json");
        let mut target = OrderBook::<()>::new(SYMBOL);
        target.restore_from_snapshot_json(&json).expect("restore");
        assert_eq!(target.stop_protection(), collar(4));
        assert_eq!(target.tick_size(), Some(3));
    }

    /// A verbatim `version: 5` package written by 0.14.0 (8815698): bid 20
    /// @ 90, ask 5 @ 104, last trade 100, a sell stop (95, watermark 100,
    /// trail 5, qty 3) and a buy stop (110, watermark 100, trail 10, qty
    /// 2), fees -1 / 3 bps, STP `CancelTaker`, tick size 1, a risk config.
    /// It keeps its checksum, restores with no collar and every other
    /// field, and is re-packaged as v6.
    #[test]
    fn test_verbatim_v5_package_from_0_14_0_restores_without_a_collar() {
        let fixture = include_str!("fixtures/snapshot_v5_0_14_0_pending_stops.json");
        let package = OrderBookSnapshotPackage::from_json(fixture).expect("parse v5");
        assert_eq!(package.version, 5);
        assert_eq!(package.stop_protection, None);
        assert!(package.validate().is_ok(), "original checksum holds");

        let clock = Arc::new(StubClock::starting_at(1_000)) as Arc<dyn Clock>;
        let mut restored: OrderBook<()> =
            OrderBook::with_clock_and_namespace("STOP/USD", clock, namespace());
        restored.set_stop_protection(collar(1)).expect("collar");
        restored
            .restore_from_snapshot_package(package)
            .expect("v5 restores");
        assert_eq!(restored.stop_protection(), None, "v5 had no collar");
        assert_eq!(restored.stp_mode(), STPMode::CancelTaker);
        assert_eq!(restored.fee_schedule(), Some(FeeSchedule::new(-1, 3)));
        assert_eq!(restored.tick_size(), Some(1));
        assert!(restored.risk_config().is_some());
        assert_eq!(restored.best_bid(), Some(90));
        assert_eq!(restored.best_ask(), Some(104));
        assert_eq!(restored.last_trade_price(), Some(100));
        assert_eq!(restored.trailing_stop_ids(), vec![id(50), id(51)]);

        let json = restored.snapshot_to_json(usize::MAX).expect("json");
        let repackaged = OrderBookSnapshotPackage::from_json(&json).expect("parse");
        assert_eq!(repackaged.version, 6);
        assert!(repackaged.validate().is_ok());
    }

    // ---- replay -------------------------------------------------------------------

    #[test]
    fn test_replay_with_the_collar_reproduces_the_live_book() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut live = OrderBook::with_clock_and_namespace(SYMBOL, Arc::clone(&clock), namespace());
        live.set_fee_schedule(Some(FeeSchedule::new(-1, 3)));
        live.set_stop_protection(collar(2)).expect("collar");
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
            limit(1, 100, 1, Side::Buy, user(1)),
            limit(2, 99, 1, Side::Buy, user(1)),
            limit(3, 98, 1, Side::Buy, user(1)),
            limit(4, 97, 1, Side::Buy, user(1)),
            limit(5, 96, 1, Side::Buy, user(1)),
            limit(6, 94, 1, Side::Buy, user(1)),
            limit(7, 90, 5, Side::Buy, user(1)),
            stop(50, Side::Sell, 100, 5, 2, user(2)),
            stop(51, Side::Sell, 99, 5, 3, user(2)),
            stop(52, Side::Sell, 96, 5, 5, user(2)),
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
        live.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        record(
            SequencerCommand::MarketOrder {
                id: id(9),
                quantity: 1,
                side: Side::Sell,
            },
            SequencerResult::OrderAdded { order_id: id(9) },
        );
        let live_snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        assert_eq!(live_snapshot.pending_stops.len(), 1, "stop 52 pending");

        let config = ReplayBookConfig::new(
            Some(FeeSchedule::new(-1, 3)),
            STPMode::None,
            None,
            None,
            None,
            None,
        )
        .with_trade_id_namespace(namespace());
        let with_collar = config.clone().with_stop_protection(collar(2));
        assert_eq!(with_collar.stop_protection, collar(2));
        let (replayed, _) = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            Arc::clone(&clock),
            &with_collar,
        )
        .expect("replay");
        assert_eq!(replayed.stop_protection(), collar(2));
        assert!(snapshots_match(
            &replayed.create_snapshot(usize::MAX).expect("snapshot"),
            &live_snapshot
        ));

        // Replaying without the collar runs the unprotected cascade and
        // diverges: the config must carry it.
        let diverged = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal,
            0,
            SYMBOL,
            Arc::new(StubClock::starting_at(0)),
            &config,
        );
        match diverged {
            Ok((book, _)) => assert!(!snapshots_match(
                &book.create_snapshot(usize::MAX).expect("snapshot"),
                &live_snapshot
            )),
            Err(err) => panic!("replay itself should not fail: {err:?}"),
        }
    }

    /// The limit of `snapshots_match` as a collar check (PR #304 review):
    /// `OrderBookSnapshot` does not carry the collar, so a replay with a
    /// different collar over a range where no stop elects matches the live
    /// snapshot, although the books would elect the pending stop
    /// differently. Only an explicit `stop_protection()` comparison sees it.
    #[test]
    fn test_replay_with_another_collar_and_no_election_passes_snapshots_match() {
        let clock: Arc<dyn Clock> = Arc::new(StubClock::starting_at(0));
        let mut live = OrderBook::with_clock_and_namespace(SYMBOL, Arc::clone(&clock), namespace());
        live.set_stop_protection(collar(2)).expect("collar");
        let journal: InMemoryJournal<()> = InMemoryJournal::new();
        let orders = [
            limit(1, 100, 5, Side::Buy, user(1)),
            limit(2, 90, 5, Side::Buy, user(1)),
            limit(3, 110, 5, Side::Sell, user(1)),
            stop(50, Side::Sell, 95, 5, 3, user(2)),
        ];
        for (seq, order) in orders.into_iter().enumerate() {
            live.add_order(order).expect("add");
            journal
                .append(&SequencerEvent {
                    sequence_num: seq as u64,
                    timestamp_ns: seq as u64,
                    command: SequencerCommand::AddOrder(order),
                    result: SequencerResult::OrderAdded {
                        order_id: order.id(),
                    },
                })
                .expect("append");
        }
        let live_snapshot = live.create_snapshot(usize::MAX).expect("snapshot");
        assert_eq!(live_snapshot.pending_stops.len(), 1, "nothing elected");

        let config = ReplayBookConfig::default()
            .with_trade_id_namespace(namespace())
            .with_stop_protection(collar(5));
        let (replayed, _) = ReplayEngine::<()>::replay_from_with_clock_and_config(
            &journal, 0, SYMBOL, clock, &config,
        )
        .expect("replay");
        assert!(
            snapshots_match(
                &replayed.create_snapshot(usize::MAX).expect("snapshot"),
                &live_snapshot
            ),
            "the snapshot does not carry the collar"
        );
        assert_ne!(
            replayed.stop_protection(),
            live.stop_protection(),
            "only the explicit comparison sees the mismatch"
        );
        let package = live.create_snapshot_package(usize::MAX).expect("package");
        assert_ne!(replayed.stop_protection(), package.stop_protection);
    }

    // ---- review follow-ups: reasons, limit on Triggered, STP, lot, fees ----

    /// The stop's order-state history as a list.
    fn history(book: &OrderBook<()>, raw: u64) -> Vec<OrderStatus> {
        book.get_order_history(id(raw))
            .expect("tracked")
            .into_iter()
            .map(|(_, status)| status)
            .collect()
    }

    #[test]
    fn test_empty_buy_band_cancels_the_stop_unfilled_with_band_reason() {
        let mut book = new_book();
        book.set_stop_protection(collar(5)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Sell, user(1)))
            .expect("ask 100");
        book.add_order(limit(2, 110, 5, Side::Sell, user(1)))
            .expect("ask 110");
        book.add_order(stop(50, Side::Buy, 100, 5, 2, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Buy)
            .expect("trade at 100");
        assert!(child_fills(&log, 50).is_empty(), "limit 105, best ask 110");
        assert_eq!(
            history(&book, 50),
            vec![
                OrderStatus::Open,
                OrderStatus::Triggered {
                    child_id: book.stop_trigger_order_id(id(50)),
                    trigger_price: 100,
                    limit_price: Some(105),
                },
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::StopProtectionBand,
                },
            ]
        );
        assert_eq!(book.visible_quantity_at_price(110, Side::Sell), Some(5));
        assert_eq!(book.best_bid(), None, "the child never rests");
    }

    /// `InsufficientLiquidity` when the side runs out within the band (partial
    /// and empty), `StopProtectionBand` only when liquidity is left beyond it.
    #[test]
    fn test_side_exhausted_within_the_band_keeps_insufficient_liquidity() {
        // Partial: 99 fills, then the bid side is empty.
        let mut book = new_book();
        book.set_stop_protection(collar(5)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 99, 1, Side::Buy, user(1)))
            .expect("bid 99");
        book.add_order(stop(50, Side::Sell, 100, 5, 3, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        assert_eq!(child_fills(&log, 50), vec![(99, 1)]);
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::InsufficientLiquidity,
            })
        );

        // Empty: the trigger took the only bid.
        let mut book = new_book();
        book.set_stop_protection(collar(5)).expect("collar");
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::InsufficientLiquidity,
            })
        );

        // Band clamped to 0 (collar above the stop): nothing can lie beyond
        // it, so a remainder is always InsufficientLiquidity.
        let mut book = new_book();
        book.set_stop_protection(collar(10)).expect("collar");
        book.add_order(limit(1, 3, 1, Side::Buy, user(1)))
            .expect("bid 3");
        book.add_order(limit(2, 1, 1, Side::Buy, user(1)))
            .expect("bid 1");
        book.add_order(stop(50, Side::Sell, 3, 5, 2, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 3");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::InsufficientLiquidity,
            })
        );
    }

    #[test]
    fn test_triggered_carries_the_child_limit_or_none() {
        for (protection, expected) in [(None, None), (collar(2), Some(98))] {
            let mut book = new_book();
            book.set_stop_protection(protection).expect("collar");
            book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
                .expect("bid 100");
            book.add_order(limit(2, 99, 5, Side::Buy, user(1)))
                .expect("bid 99");
            book.add_order(stop(50, Side::Sell, 100, 5, 2, user(2)))
                .expect("stop");
            book.submit_market_order(id(9), 1, Side::Sell)
                .expect("trade at 100");
            let child = book.stop_trigger_order_id(id(50));
            let triggered = OrderStatus::Triggered {
                child_id: child,
                trigger_price: 100,
                limit_price: expected,
            };
            assert_eq!(
                history(&book, 50),
                vec![
                    OrderStatus::Open,
                    triggered.clone(),
                    OrderStatus::Filled { filled_quantity: 2 },
                ]
            );
            let text = triggered.to_string();
            match expected {
                Some(limit) => assert!(text.ends_with(&format!(", limit={limit})")), "{text}"),
                None => assert!(!text.contains("limit="), "{text}"),
            }
        }
    }

    /// A stop trailed by a sweep's first print and elected by its last in
    /// the same sweep: the collar is anchored on the trailed stop price.
    #[test]
    fn test_stop_trailed_and_elected_in_one_sweep_anchors_on_the_trailed_stop() {
        let mut book = new_book();
        book.set_stop_protection(collar(2)).expect("collar");
        let log = record_fills(&mut book);
        for (raw, price) in [(1, 112), (2, 108), (3, 104), (4, 103), (5, 101)] {
            book.add_order(limit(raw, price, 1, Side::Buy, user(1)))
                .expect("bid");
        }
        // Stop 95, watermark 100, trail 5.
        book.add_order(stop(50, Side::Sell, 95, 5, 2, user(2)))
            .expect("stop");
        // One sell sweep prints 112, 108, 104: the first print trails the
        // stop to 107, the last (104) elects it. Limit 107 - 2 = 105: the
        // bids at 103 / 101 are outside (an anchor on the admitted stop
        // would give 93, on the print 102: both would fill 103).
        book.submit_market_order(id(9), 3, Side::Sell)
            .expect("sweep");
        assert!(child_fills(&log, 50).is_empty());
        assert_eq!(
            history(&book, 50),
            vec![
                OrderStatus::Open,
                OrderStatus::Triggered {
                    child_id: book.stop_trigger_order_id(id(50)),
                    trigger_price: 104,
                    limit_price: Some(105),
                },
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::StopProtectionBand,
                },
            ]
        );
        assert_eq!(book.visible_quantity_at_price(103, Side::Buy), Some(1));
    }

    #[test]
    fn test_collar_child_with_stp_cancel_both() {
        let mut book = new_book();
        book.set_stp_mode(STPMode::CancelBoth);
        book.set_stop_protection(collar(3)).expect("collar");
        let log = record_fills(&mut book);
        book.add_order(limit(1, 100, 1, Side::Buy, user(1)))
            .expect("bid 100");
        book.add_order(limit(2, 99, 1, Side::Buy, user(1)))
            .expect("bid 99");
        book.add_order(limit(3, 98, 1, Side::Buy, user(2)))
            .expect("own bid 98");
        book.add_order(limit(4, 96, 1, Side::Buy, user(1)))
            .expect("bid 96");
        book.add_order(stop(50, Side::Sell, 100, 5, 3, user(2)))
            .expect("stop");
        book.submit_market_order_with_user(id(9), 1, Side::Sell, user(3))
            .expect("trade at 100");
        // Limit 97: 99 fills, the own bid at 98 cancels both.
        assert_eq!(child_fills(&log, 50), vec![(99, 1)]);
        assert!(book.get_order(id(3)).is_none(), "own maker cancelled");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::SelfTradePrevention,
            })
        );
        assert_eq!(book.visible_quantity_at_price(96, Side::Buy), Some(1));
        assert_eq!(book.best_ask(), None, "the child never rests");
    }

    #[test]
    fn test_collar_child_on_a_lot_size_book() {
        let mut book = new_book();
        book.set_lot_size(2);
        book.set_stop_protection(collar(2)).expect("collar");
        let log = record_fills(&mut book);
        for (raw, price, qty) in [(1, 100, 2), (2, 99, 2), (3, 98, 2), (4, 95, 4)] {
            book.add_order(limit(raw, price, qty, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(stop(50, Side::Sell, 100, 5, 6, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 2, Side::Sell)
            .expect("trade at 100");
        assert_eq!(child_fills(&log, 50), vec![(99, 2), (98, 2)]);
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 4,
                reason: CancelReason::StopProtectionBand,
            })
        );
        assert_eq!(book.visible_quantity_at_price(95, Side::Buy), Some(4));
    }

    #[test]
    fn test_collared_child_trades_carry_their_fees() {
        let mut book = new_book();
        let schedule = FeeSchedule::new(-2, 10);
        book.set_fee_schedule(Some(schedule));
        book.set_stop_protection(collar(10_000)).expect("collar");
        type FeeLog = Arc<Mutex<Vec<(Option<Id>, i128, i128, Vec<(u128, u64)>)>>>;
        let fees: FeeLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&fees);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            sink.lock().expect("log").push((
                result.origin_stop_id,
                result.total_maker_fees,
                result.total_taker_fees,
                result
                    .match_result
                    .trades()
                    .as_vec()
                    .iter()
                    .map(|t| (t.price().as_u128(), t.quantity().as_u64()))
                    .collect(),
            ));
        }));
        for (raw, price) in [(1, 100_000), (2, 95_000), (3, 90_000), (4, 85_000)] {
            book.add_order(limit(raw, price, 1, Side::Buy, user(1)))
                .expect("bid");
        }
        book.add_order(stop(50, Side::Sell, 100_000, 5_000, 3, user(2)))
            .expect("stop");
        book.submit_market_order(id(9), 1, Side::Sell)
            .expect("trade at 100000");

        let log = fees.lock().expect("log").clone();
        let child: Vec<_> = log
            .into_iter()
            .filter(|entry| entry.0 == Some(id(50)))
            .collect();
        assert_eq!(child.len(), 1, "one TradeResult for the child");
        let (_, maker, taker, trades) = child[0].clone();
        assert_eq!(trades, vec![(95_000, 1), (90_000, 1)], "limit 90000");
        let expected = |is_maker: bool| -> i128 {
            trades
                .iter()
                .map(|&(price, qty)| {
                    schedule
                        .calculate_fee(price * u128::from(qty), is_maker)
                        .expect("fee")
                })
                .sum()
        };
        assert_eq!(taker, expected(false));
        assert_eq!(taker, 95 + 90, "10 bps taker fee");
        assert_eq!(maker, expected(true));
        assert!(maker < 0, "maker rebate");
        assert_eq!(
            book.order_status(id(50)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 2,
                reason: CancelReason::StopProtectionBand,
            })
        );
    }
}

#[cfg(all(test, not(feature = "special_orders")))]
// tests may panic: rules/global_rules.md § Testing
mod without_special_orders {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::stop_protection::StopProtection;

    /// The collar is configurable and snapshotted in every build, so a
    /// package round-trips through a build without trailing stops.
    #[test]
    fn test_collar_is_configured_and_snapshotted_without_the_feature() {
        let mut book = OrderBook::<()>::new("NOSTOP");
        let protection = StopProtection::try_new(3).expect("non-zero");
        book.set_stop_protection(Some(protection)).expect("collar");
        assert_eq!(book.stop_protection(), Some(protection));
        let json = book.snapshot_to_json(10).expect("json");
        let mut restored = OrderBook::<()>::new("NOSTOP");
        restored.restore_from_snapshot_json(&json).expect("restore");
        assert_eq!(restored.stop_protection(), Some(protection));
    }
}
