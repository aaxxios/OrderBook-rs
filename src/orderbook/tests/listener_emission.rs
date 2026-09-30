//! #249: listeners run after the mutation commits and after the submit
//! gate is released, delivered by a single active dispatcher in commit
//! order.
//!
//! - the single-threaded event stream (trade, price-level and order-state
//!   listeners interleaved) is byte-identical to the stream recorded on
//!   `main` before the change;
//! - concurrent submitters observe strictly increasing `engine_seq`;
//! - a listener may re-enter the book: no deadlock, nested events are
//!   delivered after the outer batch;
//! - a panicking listener leaves the book consistent and the gate
//!   unpoisoned;
//! - a poisoned submit gate engages the kill switch.

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::manual_assert
)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::book_change_event::PriceLevelChangedEvent;
    use crate::orderbook::fees::FeeSchedule;
    use crate::orderbook::order_state::{OrderStateTracker, OrderStatus};
    use crate::orderbook::trade::TradeResult;
    use crate::{OrderBookError, STPMode};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex, OnceLock, Weak, mpsc};
    use std::thread;
    use std::time::Duration;

    type Log = Arc<Mutex<Vec<String>>>;

    fn push(log: &Log, line: String) {
        log.lock().expect("log lock").push(line);
    }

    fn format_trade(result: &TradeResult) -> String {
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
        format!(
            "T seq={} taker={} makers=[{}] maker_fees={} taker_fees={}",
            result.engine_seq,
            result.match_result.order_id(),
            makers.join(","),
            result.total_maker_fees,
            result.total_taker_fees
        )
    }

    fn format_level(event: &PriceLevelChangedEvent) -> String {
        format!(
            "L seq={} {:?} {} q={}",
            event.engine_seq, event.side, event.price, event.quantity
        )
    }

    /// A book with all three listeners recording into one shared log.
    fn recording_book(log: &Log) -> OrderBook<()> {
        let mut book = OrderBook::<()>::new("EMIT");
        let trade_log = Arc::clone(log);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            push(&trade_log, format_trade(result));
        }));
        let level_log = Arc::clone(log);
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            push(&level_log, format_level(&event));
        }));
        let state_log = Arc::clone(log);
        let mut tracker = OrderStateTracker::new();
        tracker.set_listener(Arc::new(
            move |id: Id, old: &OrderStatus, new: &OrderStatus| {
                push(&state_log, format!("S {id} {old} -> {new}"));
            },
        ));
        book.set_order_state_tracker(tracker);
        book.set_fee_schedule(Some(FeeSchedule::new(-2, 5)));
        book
    }

    fn limit(id: u64, price: u128, qty: u64, side: Side, tif: TimeInForce) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(id),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            time_in_force: tif,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    fn outcome<V: std::fmt::Debug>(op: &str, result: &Result<V, OrderBookError>) -> String {
        match result {
            Ok(_) => format!("R {op} ok"),
            Err(err) => format!("R {op} err {err}"),
        }
    }

    /// The scripted single-threaded scenario: adds, a multi-level sweep,
    /// result-returning submits, a market order, cancels, modifies, a
    /// killed fill-or-kill, an IOC remainder, the kill switch, mass
    /// cancels. Every operation's outcome is logged after its events.
    fn run_scenario(book: &OrderBook<()>, log: &Log) {
        for (id, price, qty) in [(1, 101, 5), (2, 102, 5), (3, 102, 3), (4, 103, 10)] {
            let r = book.add_order(limit(id, price, qty, Side::Sell, TimeInForce::Gtc));
            push(log, outcome("add", &r));
        }
        for (id, price, qty) in [(5, 99, 5), (6, 98, 4), (7, 97, 6)] {
            let r = book.add_order(limit(id, price, qty, Side::Buy, TimeInForce::Gtc));
            push(log, outcome("add", &r));
        }
        // Sweep 101 fully and 102 partially.
        let r = book.add_order(limit(10, 102, 8, Side::Buy, TimeInForce::Gtc));
        push(log, outcome("sweep", &r));
        // Result-returning cross that rests a remainder.
        let r = book.add_order_with_result(limit(11, 102, 6, Side::Buy, TimeInForce::Gtc));
        if let Ok((_, Some(tr))) = &r {
            push(log, format!("C {}", format_trade(tr)));
        }
        push(log, outcome("with_result", &r));
        let r = book.submit_market_order(Id::from_u64(12), 7, Side::Sell);
        push(log, outcome("market", &r));
        let r = book.cancel_order(Id::from_u64(7));
        push(log, outcome("cancel", &r));
        let r = book.update_order(OrderUpdate::UpdatePrice {
            order_id: Id::from_u64(4),
            new_price: Price::new(104),
        });
        push(log, outcome("update_price", &r));
        let r = book.update_order(OrderUpdate::UpdateQuantity {
            order_id: Id::from_u64(4),
            new_quantity: Quantity::new(4),
        });
        push(log, outcome("update_qty", &r));
        let r = book.update_order(OrderUpdate::UpdatePrice {
            order_id: Id::from_u64(11),
            new_price: Price::new(104),
        });
        push(log, outcome("update_cross", &r));
        let r = book.add_order(limit(13, 110, 500, Side::Buy, TimeInForce::Fok));
        push(log, outcome("fok", &r));
        let r = book.add_order(limit(14, 98, 9, Side::Sell, TimeInForce::Ioc));
        push(log, outcome("ioc", &r));
        book.engage_kill_switch();
        let r = book.add_order(limit(15, 90, 1, Side::Buy, TimeInForce::Gtc));
        push(log, outcome("killed", &r));
        let r = book.add_limit_order(Id::from_u64(16), 90, 1, Side::Buy, TimeInForce::Gtc, None);
        push(log, outcome("killed_limit", &r));
        book.release_kill_switch();
        for (id, price, qty, side) in [
            (20, 95, 2, Side::Buy),
            (21, 94, 2, Side::Buy),
            (22, 120, 2, Side::Sell),
            (23, 121, 2, Side::Sell),
        ] {
            let r = book.add_order(limit(id, price, qty, side, TimeInForce::Gtc));
            push(log, outcome("add", &r));
        }
        let r = book.add_order_with_committed(limit(24, 121, 3, Side::Buy, TimeInForce::Ioc));
        if let Ok((_, Some(tr))) = &r {
            push(log, format!("C {}", format_trade(tr)));
        }
        push(
            log,
            format!("R committed {}", if r.is_ok() { "ok" } else { "err" }),
        );
        let r = book.submit_market_order_by_amount(Id::from_u64(25), 200, Side::Sell);
        push(log, outcome("by_amount", &r));
        let m = book.cancel_orders_by_side(Side::Buy);
        push(log, format!("R by_side {}", m.cancelled_count()));
        let m = book.cancel_all_orders();
        push(log, format!("R all {}", m.cancelled_count()));
    }

    /// Stream recorded on `main` (c59d74f, after #288 / #291) with the
    /// same scenario and the listeners still running inline. Emission moved
    /// after commit and gate release; for a single thread the order must
    /// not change at all. (#288 moved each resting order's `Open` state
    /// ahead of its level event on main; the branch follows it exactly.)
    const EXPECTED: &[&str] = &[
        "S 00000000-0000-0001-0000-000000000000 Open -> Open",
        "L seq=0 Sell 101 q=5",
        "R add ok",
        "S 00000000-0000-0002-0000-000000000000 Open -> Open",
        "L seq=1 Sell 102 q=5",
        "R add ok",
        "S 00000000-0000-0003-0000-000000000000 Open -> Open",
        "L seq=2 Sell 102 q=8",
        "R add ok",
        "S 00000000-0000-0004-0000-000000000000 Open -> Open",
        "L seq=3 Sell 103 q=10",
        "R add ok",
        "S 00000000-0000-0005-0000-000000000000 Open -> Open",
        "L seq=4 Buy 99 q=5",
        "R add ok",
        "S 00000000-0000-0006-0000-000000000000 Open -> Open",
        "L seq=5 Buy 98 q=4",
        "R add ok",
        "S 00000000-0000-0007-0000-000000000000 Open -> Open",
        "L seq=6 Buy 97 q=6",
        "R add ok",
        "L seq=7 Sell 101 q=0",
        "L seq=8 Sell 102 q=5",
        "S 00000000-0000-0001-0000-000000000000 Open -> Filled(5)",
        "T seq=9 taker=00000000-0000-000a-0000-000000000000 makers=[00000000-0000-0001-0000-000000000000@101x5,00000000-0000-0002-0000-000000000000@102x3] maker_fees=0 taker_fees=0",
        "S 00000000-0000-000a-0000-000000000000 Filled(8) -> Filled(8)",
        "R sweep ok",
        "L seq=10 Sell 102 q=0",
        "S 00000000-0000-0002-0000-000000000000 Open -> Filled(2)",
        "S 00000000-0000-0003-0000-000000000000 Open -> Filled(3)",
        "T seq=11 taker=00000000-0000-000b-0000-000000000000 makers=[00000000-0000-0002-0000-000000000000@102x2,00000000-0000-0003-0000-000000000000@102x3] maker_fees=0 taker_fees=0",
        "S 00000000-0000-000b-0000-000000000000 PartiallyFilled(5/6) -> PartiallyFilled(5/6)",
        "L seq=12 Buy 102 q=1",
        "C T seq=11 taker=00000000-0000-000b-0000-000000000000 makers=[00000000-0000-0002-0000-000000000000@102x2,00000000-0000-0003-0000-000000000000@102x3] maker_fees=0 taker_fees=0",
        "R with_result ok",
        "L seq=13 Buy 102 q=0",
        "L seq=14 Buy 99 q=0",
        "L seq=15 Buy 98 q=3",
        "S 00000000-0000-000b-0000-000000000000 PartiallyFilled(5/6) -> Filled(1)",
        "S 00000000-0000-0005-0000-000000000000 Open -> Filled(5)",
        "T seq=16 taker=00000000-0000-000c-0000-000000000000 makers=[00000000-0000-000b-0000-000000000000@102x1,00000000-0000-0005-0000-000000000000@99x5,00000000-0000-0006-0000-000000000000@98x1] maker_fees=0 taker_fees=0",
        "R market ok",
        "L seq=17 Buy 97 q=0",
        "S 00000000-0000-0007-0000-000000000000 Open -> Cancelled(user requested, filled=0)",
        "R cancel ok",
        "L seq=18 Sell 103 q=0",
        "S 00000000-0000-0004-0000-000000000000 Open -> Cancelled(user requested, filled=0)",
        "S 00000000-0000-0004-0000-000000000000 Cancelled(user requested, filled=0) -> Open",
        "L seq=19 Sell 104 q=10",
        "R update_price ok",
        "L seq=20 Sell 104 q=4",
        "R update_qty ok",
        "R update_cross ok",
        "S 00000000-0000-000d-0000-000000000000 Cancelled(insufficient liquidity, filled=0) -> Cancelled(insufficient liquidity, filled=0)",
        "R fok err Insufficient liquidity for BUY order: requested 500, available 4",
        "L seq=21 Buy 98 q=0",
        "S 00000000-0000-0006-0000-000000000000 Open -> Filled(3)",
        "T seq=22 taker=00000000-0000-000e-0000-000000000000 makers=[00000000-0000-0006-0000-000000000000@98x3] maker_fees=0 taker_fees=0",
        "S 00000000-0000-000e-0000-000000000000 Cancelled(insufficient liquidity, filled=3) -> Cancelled(insufficient liquidity, filled=3)",
        "R ioc err Insufficient liquidity for SELL order: requested 9, available 3",
        "S 00000000-0000-000f-0000-000000000000 Rejected(kill switch active) -> Rejected(kill switch active)",
        "R killed err kill switch active: new order entry and modifications are halted",
        "S 00000000-0000-0010-0000-000000000000 Rejected(kill switch active) -> Rejected(kill switch active)",
        "R killed_limit err kill switch active: new order entry and modifications are halted",
        "S 00000000-0000-0014-0000-000000000000 Open -> Open",
        "L seq=23 Buy 95 q=2",
        "R add ok",
        "S 00000000-0000-0015-0000-000000000000 Open -> Open",
        "L seq=24 Buy 94 q=2",
        "R add ok",
        "S 00000000-0000-0016-0000-000000000000 Open -> Open",
        "L seq=25 Sell 120 q=2",
        "R add ok",
        "S 00000000-0000-0017-0000-000000000000 Open -> Open",
        "L seq=26 Sell 121 q=2",
        "R add ok",
        "L seq=27 Sell 104 q=1",
        "T seq=28 taker=00000000-0000-0018-0000-000000000000 makers=[00000000-0000-0004-0000-000000000000@104x3] maker_fees=0 taker_fees=0",
        "S 00000000-0000-0018-0000-000000000000 Filled(3) -> Filled(3)",
        "C T seq=28 taker=00000000-0000-0018-0000-000000000000 makers=[00000000-0000-0004-0000-000000000000@104x3] maker_fees=0 taker_fees=0",
        "R committed ok",
        "L seq=29 Buy 95 q=0",
        "S 00000000-0000-0014-0000-000000000000 Open -> Filled(2)",
        "T seq=30 taker=00000000-0000-0019-0000-000000000000 makers=[00000000-0000-0014-0000-000000000000@95x2] maker_fees=0 taker_fees=0",
        "R by_amount ok",
        "L seq=31 Buy 94 q=0",
        "S 00000000-0000-0015-0000-000000000000 Open -> Cancelled(mass cancel by side, filled=0)",
        "R by_side 1",
        "L seq=32 Sell 104 q=0",
        "L seq=33 Sell 120 q=0",
        "L seq=34 Sell 121 q=0",
        "S 00000000-0000-0004-0000-000000000000 Open -> Cancelled(mass cancel all, filled=0)",
        "S 00000000-0000-0016-0000-000000000000 Open -> Cancelled(mass cancel all, filled=0)",
        "S 00000000-0000-0017-0000-000000000000 Open -> Cancelled(mass cancel all, filled=0)",
        "R all 3",
    ];

    #[test]
    fn single_thread_event_order_is_unchanged() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let book = recording_book(&log);
        run_scenario(&book, &log);
        let got = log.lock().expect("log lock").clone();
        if std::env::var_os("EMIT_PRINT").is_some() {
            for line in &got {
                println!("        {line:?},");
            }
        }
        assert_eq!(got, EXPECTED);
    }

    /// #286: a pending trailing stop's election runs under the gate of the
    /// call whose trade crossed it, so its market order's events follow
    /// that call's own events in the same batch: the crossing trade, the
    /// stop's `Triggered { child_id, trigger_price }` election event, the
    /// market order's level and trade events, then the stop's terminal
    /// state. The pre-#286 stream above is unchanged (it has no stop).
    #[cfg(feature = "special_orders")]
    const EXPECTED_WITH_STOP: &[&str] = &[
        "S 00000000-0000-0001-0000-000000000000 Open -> Open",
        "L seq=0 Buy 95 q=1",
        "R add ok",
        "S 00000000-0000-0002-0000-000000000000 Open -> Open",
        "L seq=1 Buy 94 q=2",
        "R add ok",
        "S 00000000-0000-0003-0000-000000000000 Open -> Open",
        "L seq=2 Buy 93 q=5",
        "R add ok",
        "S 00000000-0000-0004-0000-000000000000 Open -> Open",
        "L seq=3 Sell 101 q=1",
        "R add ok",
        "L seq=4 Sell 101 q=0",
        "S 00000000-0000-0004-0000-000000000000 Open -> Filled(1)",
        "T seq=5 taker=00000000-0000-0005-0000-000000000000 makers=[00000000-0000-0004-0000-000000000000@101x1] maker_fees=0 taker_fees=0",
        "R market ok",
        "S 00000000-0000-0032-0000-000000000000 Open -> Open",
        "R stop ok",
        "L seq=6 Buy 95 q=0",
        "S 00000000-0000-0001-0000-000000000000 Open -> Filled(1)",
        "T seq=7 taker=00000000-0000-0006-0000-000000000000 makers=[00000000-0000-0001-0000-000000000000@95x1] maker_fees=0 taker_fees=0",
        "S 00000000-0000-0006-0000-000000000000 Filled(1) -> Filled(1)",
        "S 00000000-0000-0032-0000-000000000000 Open -> Triggered(child=5e0c9aad-1fc6-598c-bd0d-c62ff57326d6, price=95)",
        "L seq=8 Buy 94 q=0",
        "L seq=9 Buy 93 q=3",
        "S 00000000-0000-0002-0000-000000000000 Open -> Filled(2)",
        "T seq=10 taker=5e0c9aad-1fc6-598c-bd0d-c62ff57326d6 makers=[00000000-0000-0002-0000-000000000000@94x2,00000000-0000-0003-0000-000000000000@93x2] maker_fees=0 taker_fees=0",
        "S 00000000-0000-0032-0000-000000000000 Triggered(child=5e0c9aad-1fc6-598c-bd0d-c62ff57326d6, price=95) -> Filled(4)",
        "C T seq=7 taker=00000000-0000-0006-0000-000000000000 makers=[00000000-0000-0001-0000-000000000000@95x1] maker_fees=0 taker_fees=0",
        "R elect ok",
    ];

    #[cfg(feature = "special_orders")]
    #[test]
    fn single_thread_event_order_with_a_trailing_stop() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let mut book = recording_book(&log);
        // Pins the stop's market-order id (UUIDv5 of the namespace).
        book.set_trade_id_namespace(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"EMIT"));
        for (id, price, qty) in [(1, 95, 1), (2, 94, 2), (3, 93, 5)] {
            let r = book.add_order(limit(id, price, qty, Side::Buy, TimeInForce::Gtc));
            push(&log, outcome("add", &r));
        }
        let r = book.add_order(limit(4, 101, 1, Side::Sell, TimeInForce::Gtc));
        push(&log, outcome("add", &r));
        let r = book.submit_market_order(Id::from_u64(5), 1, Side::Buy);
        push(&log, outcome("market", &r));
        // Sell stop at 96 (watermark 101, trail 5), pending off book: no
        // level event.
        let r = book.add_order(OrderType::TrailingStop {
            id: Id::from_u64(50),
            price: Price::new(96),
            quantity: Quantity::new(4),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(5),
            last_reference_price: Price::new(101),
            extra_fields: (),
        });
        push(&log, outcome("stop", &r));
        // A trade at 95 elects it; its market sell of 4 takes 2 @ 94 and
        // 2 @ 93.
        let r = book.add_order_with_result(limit(6, 95, 1, Side::Sell, TimeInForce::Gtc));
        if let Ok((_, Some(tr))) = &r {
            push(&log, format!("C {}", format_trade(tr)));
        }
        push(&log, outcome("elect", &r));
        let got = log.lock().expect("log lock").clone();
        if std::env::var_os("EMIT_PRINT").is_some() {
            for line in &got {
                println!("        {line:?},");
            }
        }
        assert_eq!(got, EXPECTED_WITH_STOP);
    }

    fn user(byte: u8) -> Hash32 {
        let mut bytes = [0u8; 32];
        bytes[0] = byte;
        Hash32::new(bytes)
    }

    fn limit_for(
        id: u64,
        price: u128,
        qty: u64,
        side: Side,
        tif: TimeInForce,
        owner: Hash32,
    ) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(id),
            price: Price::new(price),
            quantity: Quantity::new(qty),
            side,
            time_in_force: tif,
            user_id: owner,
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    /// Every located order is reachable through its level and every order
    /// resting on a level is located.
    fn assert_book_consistent(book: &OrderBook<()>) {
        let mut resting = 0usize;
        for side in [&book.bids, &book.asks] {
            for entry in side.iter() {
                for order in entry.value().iter_orders() {
                    resting += 1;
                    assert_eq!(
                        book.order_locations.get(&order.id()).map(|loc| loc.price),
                        Some(*entry.key()),
                        "order {} rests but is not located there",
                        order.id()
                    );
                }
            }
        }
        assert_eq!(
            resting,
            book.order_locations.len(),
            "level and index counts"
        );
    }

    /// N concurrent submitters on the shared gate: the merged trade +
    /// price-level stream is delivered in strictly increasing `engine_seq`,
    /// and no minted sequence is lost.
    #[test]
    fn concurrent_submitters_observe_strictly_increasing_engine_seq() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 400;
        let seqs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let states = Arc::new(AtomicUsize::new(0));
        let mut book = OrderBook::<()>::new("EMIT");
        let trade_seqs = Arc::clone(&seqs);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            trade_seqs.lock().expect("seqs").push(result.engine_seq);
        }));
        let level_seqs = Arc::clone(&seqs);
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            level_seqs.lock().expect("seqs").push(event.engine_seq);
        }));
        let state_count = Arc::clone(&states);
        let mut tracker = OrderStateTracker::with_capacity(1_000_000);
        tracker.set_listener(Arc::new(move |_: Id, _: &OrderStatus, _: &OrderStatus| {
            state_count.fetch_add(1, Ordering::Relaxed);
        }));
        book.set_order_state_tracker(tracker);
        let book = Arc::new(book);
        let barrier = Arc::new(Barrier::new(THREADS as usize));

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let book = Arc::clone(&book);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        let id = 1 + t * PER_THREAD + i;
                        // Makers and takers around a narrow band so orders
                        // rest, cross and empty levels concurrently.
                        let side = if (t + i) % 2 == 0 {
                            Side::Buy
                        } else {
                            Side::Sell
                        };
                        let price = match side {
                            Side::Buy => 100 + u128::from(i % 3),
                            Side::Sell => 101 + u128::from(i % 3),
                        };
                        let _ = book.add_order(limit(id, price, 1 + i % 4, side, TimeInForce::Gtc));
                        if i % 7 == 0 {
                            let _ = book.cancel_order(Id::from_u64(id));
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("submitter");
        }

        let seqs = seqs.lock().expect("seqs");
        assert!(seqs.len() > 1_000, "the scenario must produce many events");
        for pair in seqs.windows(2) {
            assert!(
                pair[0] < pair[1],
                "engine_seq must strictly increase in delivery order: {} then {}",
                pair[0],
                pair[1]
            );
        }
        // Every minted sequence was delivered: no gap, no loss.
        assert_eq!(seqs.len() as u64, book.engine_seq());
        assert_eq!(seqs.first().copied(), Some(0));
        assert!(states.load(Ordering::Relaxed) > 0);
        assert_eq!(book.dropped_listener_events(), 0);
        assert_eq!(book.listener_panics(), 0);
    }

    /// A listener that re-enters the book: on a self-trade-prevention book
    /// (exclusive gate on every identified taker, which deadlocked before
    /// #249) the trade listener submits a new crossing order from inside the
    /// callback. No deadlock; the nested call's events are delivered after
    /// every event of the outer call; the listener runs with the gate free
    /// and the outer mutation committed.
    #[test]
    fn reentrant_listener_submits_without_deadlock_and_in_order() {
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let log: Log = Arc::new(Mutex::new(Vec::new()));
            let slot: Arc<OnceLock<Weak<OrderBook<()>>>> = Arc::new(OnceLock::new());
            let gate_free_in_listener = Arc::new(AtomicBool::new(false));
            let reentered = Arc::new(AtomicBool::new(false));

            let mut book = recording_book(&log);
            book.set_stp_mode(STPMode::CancelTaker);
            let inner_log = Arc::clone(&log);
            let inner_slot = Arc::clone(&slot);
            let inner_gate_free = Arc::clone(&gate_free_in_listener);
            let inner_reentered = Arc::clone(&reentered);
            book.set_trade_listener(Arc::new(move |result: &TradeResult| {
                push(&inner_log, format_trade(result));
                let Some(book) = inner_slot.get().and_then(Weak::upgrade) else {
                    return;
                };
                // The gate is released and the outer mutation committed.
                let free = book.submit_gate.try_write().is_ok();
                inner_gate_free.store(free, Ordering::Relaxed);
                if result.match_result.order_id() == Id::from_u64(3)
                    && !inner_reentered.swap(true, Ordering::Relaxed)
                {
                    assert!(
                        book.get_order(Id::from_u64(1)).is_none(),
                        "the outer fill is committed before the listener runs"
                    );
                    let r =
                        book.add_order(limit_for(4, 99, 2, Side::Sell, TimeInForce::Gtc, user(9)));
                    push(&inner_log, outcome("nested", &r));
                }
            }));
            let book = Arc::new(book);
            slot.set(Arc::downgrade(&book)).expect("slot set once");

            book.add_order(limit_for(1, 100, 5, Side::Sell, TimeInForce::Gtc, user(1)))
                .expect("maker ask");
            book.add_order(limit_for(2, 99, 2, Side::Buy, TimeInForce::Gtc, user(2)))
                .expect("maker bid");
            push(&log, "outer start".to_string());
            let r = book.add_order(limit_for(3, 100, 5, Side::Buy, TimeInForce::Gtc, user(3)));
            push(&log, outcome("outer", &r));
            done_tx
                .send((
                    log.lock().expect("log").clone(),
                    gate_free_in_listener.load(Ordering::Relaxed),
                    reentered.load(Ordering::Relaxed),
                    book.get_order(Id::from_u64(2)).is_none(),
                    book.engine_seq(),
                ))
                .expect("send");
            assert_book_consistent(&book);
        });
        let (got, gate_free, reentered, nested_filled, _) = done_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("re-entrant submit deadlocked");
        worker.join().expect("worker");
        assert!(reentered, "the listener re-entered the book");
        assert!(gate_free, "the listener ran with the submit gate released");
        assert!(nested_filled, "the nested order traded against the bid");

        let start = got.iter().position(|l| l == "outer start").expect("start");
        let outer = &got[start + 1..];
        let pos = |needle: &str| {
            outer
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing from {outer:#?}"))
        };
        let outer_trade = pos("T seq=3 taker=00000000-0000-0003");
        let outer_state = pos("S 00000000-0000-0003-0000-000000000000 Filled(5) -> Filled(5)");
        let nested_returned = pos("R nested ok");
        let nested_level = pos("L seq=4 Buy 99 q=0");
        let nested_trade = pos("T seq=5 taker=00000000-0000-0004");
        let outer_returned = pos("R outer ok");
        // The whole outer batch (its trade, then its taker's state) is
        // delivered before any nested event; the nested call returned
        // without dispatching, from inside the outer trade callback.
        assert!(outer_trade < nested_returned);
        assert!(
            nested_returned < outer_state,
            "nested submit returns inside the callback"
        );
        assert!(
            outer_state < nested_level,
            "nested events follow the outer batch"
        );
        assert!(nested_level < nested_trade);
        assert!(
            nested_trade < outer_returned,
            "delivered before the outer call returns"
        );
        // One strictly increasing sequence across both calls.
        let seqs: Vec<u64> = got
            .iter()
            .filter_map(|l| l.split("seq=").nth(1))
            .filter_map(|rest| rest.split_whitespace().next())
            .map(|n| n.parse::<u64>().expect("seq"))
            .collect();
        assert!(seqs.windows(2).all(|p| p[0] < p[1]), "{seqs:?}");
    }

    /// A listener that panics: the unwind happens after commit, outside
    /// the gate. The book stays consistent, the gate is not poisoned, the
    /// rest of that batch is dropped and counted, and the next submission
    /// works and is delivered normally.
    #[test]
    fn panicking_listener_leaves_book_consistent_and_gate_unpoisoned() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let mut book = recording_book(&log);
        let trade_log = Arc::clone(&log);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            if result.match_result.order_id() == Id::from_u64(2) {
                panic!("listener bug");
            }
            push(&trade_log, format_trade(result));
        }));
        let book = Arc::new(book);
        book.add_order(limit(1, 100, 5, Side::Sell, TimeInForce::Gtc))
            .expect("maker");

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 100, 3, Side::Buy, TimeInForce::Gtc));
        })
        .join();
        assert!(
            joined.is_err(),
            "the listener panic propagates to the submitter"
        );

        assert!(
            !book.submit_gate.is_poisoned(),
            "listeners run outside the gate"
        );
        assert!(!book.submit_gate_poisoned());
        assert!(!book.is_kill_switch_engaged());
        assert_eq!(book.listener_panics(), 1);
        // The taker's `Filled` transition came after the panicking trade
        // event in the same batch.
        assert_eq!(book.dropped_listener_events(), 1);
        // The mutation committed before the listener ran.
        assert_eq!(book.visible_quantity_at_price(100, Side::Sell), Some(2));
        assert_eq!(
            book.order_status(Id::from_u64(2)),
            Some(OrderStatus::Filled { filled_quantity: 3 })
        );
        assert_book_consistent(&book);

        // Later submissions work and are delivered.
        log.lock().expect("log").clear();
        book.add_order(limit(3, 100, 2, Side::Buy, TimeInForce::Gtc))
            .expect("next submit");
        let got = log.lock().expect("log").clone();
        assert!(
            got.iter()
                .any(|l| l.starts_with("T ") && l.contains("taker=00000000-0000-0003"))
        );
        assert!(got.iter().any(|l| l.starts_with("L ")));
        assert_book_consistent(&book);
    }

    /// A listener that re-enters the book and then panics: the nested
    /// call's batch was queued behind the panicking one; it survives the
    /// unwind and is delivered, in order, by the next dispatch.
    #[test]
    fn batches_queued_behind_a_panicking_listener_are_delivered_later() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let slot: Arc<OnceLock<Weak<OrderBook<()>>>> = Arc::new(OnceLock::new());
        let mut book = recording_book(&log);
        let trade_log = Arc::clone(&log);
        let inner_slot = Arc::clone(&slot);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            push(&trade_log, format_trade(result));
            if result.match_result.order_id() == Id::from_u64(2)
                && let Some(book) = inner_slot.get().and_then(Weak::upgrade)
            {
                // Queued behind the current batch, then the listener dies.
                let _ = book.add_order(limit(3, 90, 1, Side::Buy, TimeInForce::Gtc));
                panic!("listener bug after re-entering");
            }
        }));
        let book = Arc::new(book);
        slot.set(Arc::downgrade(&book)).expect("slot set once");
        book.add_order(limit(1, 100, 5, Side::Sell, TimeInForce::Gtc))
            .expect("maker");

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 100, 1, Side::Buy, TimeInForce::Gtc));
        })
        .join();
        assert!(joined.is_err());
        assert_eq!(book.listener_panics(), 1);
        assert!(
            book.get_order(Id::from_u64(3)).is_some(),
            "nested order rests"
        );
        let nested = "S 00000000-0000-0003-0000-000000000000 Open -> Open";
        assert!(
            !log.lock().expect("log").iter().any(|l| l == nested),
            "the nested batch is still queued"
        );

        book.flush_listener_events();
        let got = log.lock().expect("log").clone();
        let level = got
            .iter()
            .position(|l| l.contains("Buy 90 q=1"))
            .expect("nested level event delivered");
        let state = got.iter().position(|l| l == nested).expect("nested state");
        // `rest_on_level` records the resting state before the admission
        // that emits the level event (#288); the batch keeps that order.
        assert!(state < level, "nested batch delivered in its own order");
        assert!(!book.submit_gate.is_poisoned());
        assert_book_consistent(&book);
    }

    /// Engine code panicking mid-mutation under the exclusive gate poisons
    /// it. The unwinding guard engages the kill switch (#294); the next
    /// acquisition clears the poison and the submission gets the typed
    /// `KillSwitchActive` error; cancels still work.
    #[test]
    fn submit_gate_poison_engages_kill_switch() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let mut book = recording_book(&log);
        book.level_interleave_hook = Some(Arc::new(|price: u128| {
            if price == 100 {
                panic!("injected engine panic mid-sweep");
            }
        }));
        let book = Arc::new(book);
        book.add_order(limit(1, 100, 5, Side::Sell, TimeInForce::Gtc))
            .expect("maker");
        book.add_order(limit(2, 90, 5, Side::Buy, TimeInForce::Gtc))
            .expect("resting bid");

        // Fill-or-kill takes the exclusive side, so the panic poisons it.
        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(3, 100, 5, Side::Buy, TimeInForce::Fok));
        })
        .join();
        assert!(joined.is_err());
        assert!(book.submit_gate.is_poisoned());
        // #294: the guard's drop detects the unwind and engages the kill
        // switch before the gate is released.
        assert!(book.is_kill_switch_engaged(), "detected at unwind");
        assert!(book.submit_gate_poisoned());

        // Next acquisition: typed error, poison cleared, latch kept.
        let err = book
            .add_order(limit(4, 80, 1, Side::Buy, TimeInForce::Gtc))
            .expect_err("rejected after poison");
        assert!(matches!(err, OrderBookError::KillSwitchActive), "{err:?}");
        assert!(book.is_kill_switch_engaged());
        assert!(book.submit_gate_poisoned());
        assert!(!book.submit_gate.is_poisoned());
        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(2),
                new_quantity: Quantity::new(1),
            })
            .expect_err("modifies are rejected");
        assert!(matches!(err, OrderBookError::KillSwitchActive));
        // Cancels still drain the book.
        assert!(
            book.cancel_order(Id::from_u64(2))
                .expect("cancel")
                .is_some()
        );
    }

    /// The backlog gauge: while the dispatching thread is stuck in a
    /// listener, other submitters queue behind it and
    /// `pending_listener_events` counts their events; it drains to zero
    /// once the listener returns, with every event delivered in order.
    #[test]
    fn stalled_listener_grows_the_backlog_gauge() {
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let seqs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut book = OrderBook::<()>::new("EMIT");
        let seen = Arc::clone(&seqs);
        let entered = Mutex::new(Some(entered_tx));
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            seen.lock().expect("seqs").push(event.engine_seq);
            // Stall on the first event only.
            if let Some(tx) = entered.lock().expect("entered").take() {
                tx.send(()).expect("signal");
                release_rx
                    .lock()
                    .expect("release")
                    .recv()
                    .expect("released");
            }
        }));
        let book = Arc::new(book);
        assert_eq!(book.pending_listener_events(), 0);

        let stalled = Arc::clone(&book);
        let dispatcher = thread::spawn(move || {
            stalled
                .add_order(limit(1, 90, 1, Side::Buy, TimeInForce::Gtc))
                .expect("first add");
        });
        entered_rx.recv().expect("listener entered");
        for id in 2..=6 {
            book.add_order(limit(
                id,
                90 - u128::from(id),
                1,
                Side::Buy,
                TimeInForce::Gtc,
            ))
            .expect("queued add");
        }
        assert_eq!(
            book.pending_listener_events(),
            5,
            "the five later adds wait behind the stalled listener"
        );
        release_tx.send(()).expect("release");
        dispatcher.join().expect("dispatcher");
        assert_eq!(book.pending_listener_events(), 0);
        let seqs = seqs.lock().expect("seqs");
        assert_eq!(*seqs, (0..6).collect::<Vec<u64>>());
    }

    /// A panic in the middle of a dispatcher's multi-batch run: the rest of
    /// the panicking batch is dropped, the untouched batch after it goes
    /// back to the head of the outbox and is delivered, in order, by the
    /// next dispatch.
    #[test]
    fn panic_mid_run_requeues_the_untouched_batches_in_order() {
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let mut book = OrderBook::<()>::new("EMIT");
        let level_log = Arc::clone(&log);
        let entered = Mutex::new(Some(entered_tx));
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            push(&level_log, format_level(&event));
            if let Some(tx) = entered.lock().expect("entered").take() {
                tx.send(()).expect("signal");
                release_rx
                    .lock()
                    .expect("release")
                    .recv()
                    .expect("released");
            }
        }));
        let state_log = Arc::clone(&log);
        let mut tracker = OrderStateTracker::new();
        tracker.set_listener(Arc::new(
            move |id: Id, old: &OrderStatus, new: &OrderStatus| {
                if id == Id::from_u64(3) {
                    panic!("state listener bug");
                }
                push(&state_log, format!("S {id} {old} -> {new}"));
            },
        ));
        book.set_order_state_tracker(tracker);
        let book = Arc::new(book);

        // Thread D becomes the dispatcher and stalls in order 1's level event.
        let stalled = Arc::clone(&book);
        let dispatcher = thread::spawn(move || {
            let _ = stalled.add_order(limit(1, 90, 1, Side::Buy, TimeInForce::Gtc));
        });
        entered_rx.recv().expect("listener entered");
        // Three batches queue behind it: [S2, L2], [S3, L3], [S4, L4].
        for id in 2..=4 {
            book.add_order(limit(
                id,
                90 - u128::from(id),
                1,
                Side::Buy,
                TimeInForce::Gtc,
            ))
            .expect("queued add");
        }
        assert_eq!(book.pending_listener_events(), 6);
        release_tx.send(()).expect("release");
        assert!(dispatcher.join().is_err(), "the panic unwinds out of D");
        assert_eq!(book.listener_panics(), 1);
        assert_eq!(
            book.dropped_listener_events(),
            1,
            "L3 after the panicking S3"
        );
        assert_eq!(book.pending_listener_events(), 2, "batch 4 was requeued");

        book.flush_listener_events();
        assert_eq!(book.pending_listener_events(), 0);
        let got = log.lock().expect("log").clone();
        let expect = [
            "S 00000000-0000-0001-0000-000000000000 Open -> Open",
            "L seq=0 Buy 90 q=1",
            "S 00000000-0000-0002-0000-000000000000 Open -> Open",
            "L seq=1 Buy 88 q=1",
            "S 00000000-0000-0004-0000-000000000000 Open -> Open",
            "L seq=3 Buy 86 q=1",
        ];
        assert_eq!(got, expect);
        assert!(!book.submit_gate.is_poisoned());
    }

    /// A `Clock` that, the first time book A's tracker reads it (inside
    /// A's mutation, under A's gate), submits an order to book B.
    struct DrivingClock {
        target: Arc<OnceLock<Weak<OrderBook<()>>>>,
        fired: AtomicBool,
    }

    impl std::fmt::Debug for DrivingClock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("DrivingClock")
        }
    }

    impl crate::orderbook::clock::Clock for DrivingClock {
        fn now_millis(&self) -> TimestampMs {
            if !self.fired.swap(true, Ordering::SeqCst)
                && let Some(book_b) = self.target.get().and_then(Weak::upgrade)
            {
                let _ =
                    book_b.add_order(limit_for(100, 50, 1, Side::Buy, TimeInForce::Gtc, user(7)));
            }
            TimestampMs::new(0)
        }
    }

    /// PR #289 review: caller code running inside book A's mutation (here
    /// A's tracker clock) drives book B, whose listener re-enters B. B's
    /// events must be buffered in B's own (nested) scope and delivered
    /// after B's gate is released: no deadlock (B's gate is exclusive: STP
    /// is on), and each book's stream stays strictly increasing.
    #[test]
    fn nested_scope_for_another_book_defers_its_dispatch_past_its_gate() {
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let b_seqs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
            let b_gate_free = Arc::new(Mutex::new(Vec::new()));
            let b_slot: Arc<OnceLock<Weak<OrderBook<()>>>> = Arc::new(OnceLock::new());

            let mut book_b = OrderBook::<()>::new("B");
            book_b.set_stp_mode(STPMode::CancelTaker);
            let seqs = Arc::clone(&b_seqs);
            let gate_free = Arc::clone(&b_gate_free);
            let inner_slot = Arc::clone(&b_slot);
            book_b.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
                seqs.lock().expect("seqs").push(event.engine_seq);
                let Some(book) = inner_slot.get().and_then(Weak::upgrade) else {
                    return;
                };
                gate_free
                    .lock()
                    .expect("gate")
                    .push(book.submit_gate.try_write().is_ok());
                if event.price == 50 {
                    // Re-enter B from B's listener.
                    let _ =
                        book.add_order(limit_for(101, 49, 1, Side::Buy, TimeInForce::Gtc, user(8)));
                }
            }));
            let book_b = Arc::new(book_b);
            b_slot.set(Arc::downgrade(&book_b)).expect("slot");

            let a_seqs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
            let mut book_a = OrderBook::<()>::new("A");
            let seqs_a = Arc::clone(&a_seqs);
            book_a.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
                seqs_a.lock().expect("seqs").push(event.engine_seq);
            }));
            let clock_slot: Arc<OnceLock<Weak<OrderBook<()>>>> = Arc::new(OnceLock::new());
            clock_slot.set(Arc::downgrade(&book_b)).expect("clock slot");
            book_a.set_order_state_tracker(OrderStateTracker::with_clock(Arc::new(DrivingClock {
                target: clock_slot,
                fired: AtomicBool::new(false),
            })));
            book_a
                .add_order(limit(1, 90, 1, Side::Buy, TimeInForce::Gtc))
                .expect("A add");
            book_a
                .add_order(limit(2, 91, 1, Side::Buy, TimeInForce::Gtc))
                .expect("A add 2");
            done_tx
                .send((
                    b_seqs.lock().expect("seqs").clone(),
                    b_gate_free.lock().expect("gate").clone(),
                    a_seqs.lock().expect("seqs").clone(),
                    book_b.get_order(Id::from_u64(101)).is_some(),
                ))
                .expect("send");
        });
        let (b_seqs, b_gate_free, a_seqs, nested_rests) = done_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("book B's listener deadlocked under B's gate");
        worker.join().expect("worker");
        assert!(nested_rests, "B's listener re-entered B");
        assert_eq!(b_seqs, vec![0, 1], "B's stream, strictly increasing");
        assert_eq!(
            b_gate_free,
            vec![true, true],
            "delivered after B's gate release"
        );
        assert_eq!(a_seqs, vec![0, 1], "A's stream unaffected");
    }

    /// PR #289 review: a snapshot-package restore rewinds `engine_seq`.
    /// Batches a listener panic left queued describe the replaced book and
    /// carry higher sequences; delivering them after the restore would make
    /// the stream go backwards. The restore discards them (counted in
    /// `dropped_listener_events`) and the restored book's events are
    /// delivered in strictly increasing order.
    #[test]
    fn package_restore_discards_batches_left_by_a_listener_panic() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let slot: Arc<OnceLock<Weak<OrderBook<()>>>> = Arc::new(OnceLock::new());
        let mut book = OrderBook::<()>::new("EMIT");
        let level_log = Arc::clone(&log);
        book.set_price_level_listener(Arc::new(move |event: PriceLevelChangedEvent| {
            push(&level_log, format!("L {}", event.engine_seq));
        }));
        let inner_slot = Arc::clone(&slot);
        book.set_trade_listener(Arc::new(move |result: &TradeResult| {
            if result.match_result.order_id() == Id::from_u64(2)
                && let Some(book) = inner_slot.get().and_then(Weak::upgrade)
            {
                // Queued behind the current batch, then the listener dies.
                let _ = book.add_order(limit(3, 90, 1, Side::Buy, TimeInForce::Gtc));
                panic!("listener bug after re-entering");
            }
        }));
        book.add_order(limit(1, 100, 5, Side::Sell, TimeInForce::Gtc))
            .expect("maker");
        let package = book.create_snapshot_package(10).expect("package");
        let restored_seq = package.engine_seq;
        let book = Arc::new(book);
        slot.set(Arc::downgrade(&book)).expect("slot");

        let panicking = Arc::clone(&book);
        let joined = thread::spawn(move || {
            let _ = panicking.add_order(limit(2, 100, 1, Side::Buy, TimeInForce::Gtc));
        })
        .join();
        assert!(joined.is_err());
        let queued = book.pending_listener_events();
        assert!(queued > 0, "the nested batch is left queued");
        assert!(book.engine_seq() > restored_seq);
        let dropped_before = book.dropped_listener_events();

        let Ok(mut book) = Arc::try_unwrap(book) else {
            panic!("the test holds the only strong reference");
        };
        book.restore_from_snapshot_package(package)
            .expect("restore");
        assert_eq!(book.engine_seq(), restored_seq, "the counter is rewound");
        assert_eq!(book.pending_listener_events(), 0, "stale batches discarded");
        assert_eq!(
            book.dropped_listener_events(),
            dropped_before + queued as u64,
            "discarded events are counted"
        );

        log.lock().expect("log").clear();
        book.add_order(limit(4, 80, 1, Side::Buy, TimeInForce::Gtc))
            .expect("post-restore add");
        book.add_order(limit(5, 79, 1, Side::Buy, TimeInForce::Gtc))
            .expect("post-restore add");
        book.flush_listener_events();
        let got = log.lock().expect("log").clone();
        let expect: Vec<String> = [restored_seq, restored_seq + 1]
            .iter()
            .map(|seq| format!("L {seq}"))
            .collect();
        assert_eq!(got, expect, "only the restored book's events, increasing");
    }

    /// `flush_listener_events` is a no-op on a quiet book and never
    /// dispatches while another thread holds the role.
    #[test]
    fn flush_on_quiet_book_is_noop() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let book = recording_book(&log);
        book.flush_listener_events();
        assert!(log.lock().expect("log").is_empty());
    }
}
