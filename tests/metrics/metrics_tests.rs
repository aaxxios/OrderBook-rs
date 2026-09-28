//! Integration tests for the optional Prometheus metrics feature
//! (issue #60).
//!
//! Lives in a dedicated test binary so the global `metrics` recorder
//! is not perturbed by the broader integration suite under
//! `tests/unit/` (which constructs `OrderBook`s and triggers the
//! depth gauge updates as a side effect of every add / cancel).

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use orderbook_rs::orderbook::manager::{BookManager, BookManagerStd, BookManagerTokio};
use orderbook_rs::orderbook::metrics::{
    DEPTH_LEVELS_ASK, DEPTH_LEVELS_BID, MANAGER_TRADE_EVENTS_DROPPED_TOTAL, REJECTS_TOTAL,
    RESERVE_DISCARDS_TOTAL, RESERVE_HIDDEN_DISCARDED_TOTAL, TRADES_TOTAL,
};
use orderbook_rs::{OrderBook, StubClock};
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Captured counter / gauge state, keyed by metric name with a
/// `reason=…` suffix when the labels include a reason.
#[derive(Default)]
struct Captured {
    counters: HashMap<String, u64>,
    gauges: HashMap<String, f64>,
}

/// Process-wide capture storage. The `metrics` crate only allows the
/// global recorder to be installed once per process — every test in
/// this file shares the same recorder and reads from this storage.
fn captured() -> &'static Mutex<Captured> {
    static CAPTURED: OnceLock<Mutex<Captured>> = OnceLock::new();
    CAPTURED.get_or_init(|| Mutex::new(Captured::default()))
}

/// Build a "metric_name{label_value}" key, or just the metric name
/// when there are no labels — matches the format used in assertions.
fn label_key(key: &Key) -> String {
    let labels: Vec<String> = key
        .labels()
        .map(|l| format!("{}={}", l.key(), l.value()))
        .collect();
    if labels.is_empty() {
        key.name().to_string()
    } else {
        format!("{}{{{}}}", key.name(), labels.join(","))
    }
}

struct CapturingCounter {
    key: String,
}

impl metrics::CounterFn for CapturingCounter {
    fn increment(&self, value: u64) {
        let mut g = captured().lock().expect("captured lock");
        *g.counters.entry(self.key.clone()).or_insert(0) += value;
    }
    fn absolute(&self, value: u64) {
        let mut g = captured().lock().expect("captured lock");
        g.counters.insert(self.key.clone(), value);
    }
}

struct CapturingGauge {
    key: String,
}

impl metrics::GaugeFn for CapturingGauge {
    fn increment(&self, value: f64) {
        let mut g = captured().lock().expect("captured lock");
        *g.gauges.entry(self.key.clone()).or_insert(0.0) += value;
    }
    fn decrement(&self, value: f64) {
        let mut g = captured().lock().expect("captured lock");
        *g.gauges.entry(self.key.clone()).or_insert(0.0) -= value;
    }
    fn set(&self, value: f64) {
        let mut g = captured().lock().expect("captured lock");
        g.gauges.insert(self.key.clone(), value);
    }
}

struct CapturingHistogram;

impl metrics::HistogramFn for CapturingHistogram {
    fn record(&self, _value: f64) {}
}

struct CapturingRecorder;

impl Recorder for CapturingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        Counter::from_arc(std::sync::Arc::new(CapturingCounter {
            key: label_key(key),
        }))
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::from_arc(std::sync::Arc::new(CapturingGauge {
            key: label_key(key),
        }))
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::from_arc(std::sync::Arc::new(CapturingHistogram))
    }
}

/// Install the global capturing recorder once. Calling this from every
/// test is idempotent — the second installation attempt is a no-op.
fn install_recorder() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // `set_global_recorder` only succeeds once per process.
        let _ = metrics::set_global_recorder(CapturingRecorder);
    });
}

fn counter_value(key: &str) -> u64 {
    let g = captured().lock().expect("captured lock");
    g.counters.get(key).copied().unwrap_or(0)
}

fn gauge_value(key: &str) -> f64 {
    let g = captured().lock().expect("captured lock");
    g.gauges.get(key).copied().unwrap_or(0.0)
}

/// All tests in this module share the global `metrics` recorder and
/// the captured-state map. Take this lock at the top of every test
/// to serialize them — concurrent tests would otherwise step on each
/// other's gauge values.
fn serialized_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn counters_increment_on_rejects_and_trades() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();
    let book = OrderBook::<()>::new("METRICS-TEST");

    // Snapshot baseline counter values — other tests in this module
    // share the global recorder, so we reason about deltas.
    let trades_before = counter_value(TRADES_TOTAL);
    let kill_rejects_before =
        counter_value(&format!("{REJECTS_TOTAL}{{reason=kill switch active}}"));

    // Reject path: engage the kill switch and submit one order.
    book.engage_kill_switch();
    let rej = book.add_limit_order(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, None);
    assert!(rej.is_err(), "kill-switched add_order must Err");
    book.release_kill_switch();

    let kill_rejects_after =
        counter_value(&format!("{REJECTS_TOTAL}{{reason=kill switch active}}"));
    assert_eq!(
        kill_rejects_after - kill_rejects_before,
        1,
        "kill-switch reject must increment orderbook_rejects_total{{reason=...}} by exactly 1"
    );

    // Happy path: cross two limit orders to print a trade.
    book.add_limit_order(new_id(), 100, 5, Side::Sell, TimeInForce::Gtc, None)
        .expect("seed resting ask");
    book.add_limit_order(new_id(), 100, 5, Side::Buy, TimeInForce::Gtc, None)
        .expect("aggressive buy fills the ask");

    let trades_after = counter_value(TRADES_TOTAL);
    assert!(
        trades_after > trades_before,
        "orderbook_trades_total must increment after a fill (before={trades_before}, after={trades_after})"
    );
}

#[test]
fn depth_gauges_track_distinct_price_levels() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();
    let book = OrderBook::<()>::new("METRICS-DEPTH");

    // Place two distinct bid levels and one ask level.
    book.add_limit_order(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, None)
        .expect("bid 1");
    book.add_limit_order(new_id(), 99, 1, Side::Buy, TimeInForce::Gtc, None)
        .expect("bid 2");
    let ask_id = new_id();
    book.add_limit_order(ask_id, 110, 1, Side::Sell, TimeInForce::Gtc, None)
        .expect("ask 1");

    assert_eq!(
        gauge_value(DEPTH_LEVELS_BID) as u64,
        2,
        "orderbook_depth_levels_bid must reflect two distinct bid levels"
    );
    assert_eq!(
        gauge_value(DEPTH_LEVELS_ASK) as u64,
        1,
        "orderbook_depth_levels_ask must reflect one ask level"
    );

    // Cancel the unique ask — the ask gauge should go to 0.
    book.cancel_order(ask_id).expect("cancel ask");

    assert_eq!(
        gauge_value(DEPTH_LEVELS_ASK) as u64,
        0,
        "ask gauge must drop to 0 after the only ask level is removed"
    );
}

#[test]
fn metrics_do_not_affect_order_semantics() {
    // Determinism guard — issue #60 explicitly requires that metric
    // emission must NOT alter matching outcomes. Build two books with
    // the same symbol and identical inputs and confirm they produce
    // byte-identical snapshots after the same operation sequence.
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();
    // StubClock + identical symbols + identical order ids yields a
    // byte-identical state machine. If metrics emission ever bled
    // back into matching, the two snapshots would diverge.
    let book_a = OrderBook::<()>::with_clock("DET", Arc::new(StubClock::new()));
    let book_b = OrderBook::<()>::with_clock("DET", Arc::new(StubClock::new()));

    let scenarios: [(u128, u64, Side); 6] = [
        (100, 5, Side::Sell),
        (101, 3, Side::Sell),
        (99, 5, Side::Buy),
        (100, 4, Side::Buy),
        (102, 2, Side::Sell),
        (101, 3, Side::Buy),
    ];

    for (i, (price, qty, side)) in scenarios.into_iter().enumerate() {
        // Use a deterministic id derived from the index so the two
        // books mint structurally identical resting orders.
        let id = Id::from_u64(0xC0DE_0000 + i as u64);
        let _ = book_a.add_limit_order(id, price, qty, side, TimeInForce::Gtc, None);
        let _ = book_b.add_limit_order(id, price, qty, side, TimeInForce::Gtc, None);
    }

    let snap_a = book_a.create_snapshot(10).expect("snapshot");
    let snap_b = book_b.create_snapshot(10).expect("snapshot");

    // Compare the matched book *structure*, not the per-level `statistics`.
    // Those statistics carry `pricelevel` wall-clock fields (first_arrival_time,
    // last_execution_time, sum_waiting_time) that are captured from the real
    // clock at order arrival/execution, independent of the injected StubClock.
    // Two books built sequentially can therefore straddle a millisecond boundary
    // and differ there (notably under slow coverage instrumentation), even
    // though the determinism contract this test guards is that metric emission
    // does not alter the resting order/price/quantity state. Strip the volatile
    // statistics so the assertion is exact on book state and immune to timing.
    let value_a = strip_level_statistics(serde_json::to_value(&snap_a).expect("serialize snap_a"));
    let value_b = strip_level_statistics(serde_json::to_value(&snap_b).expect("serialize snap_b"));
    assert_eq!(
        value_a, value_b,
        "metrics emission must not affect book state — structural snapshots differ"
    );
}

/// Removes the per-level `statistics` object from each bid/ask level of a
/// serialized order-book snapshot. Those statistics carry wall-clock timestamps
/// not governed by the injected clock; stripping them makes a snapshot equality
/// comparison depend only on the matched order/price/quantity state. See
/// `metrics_do_not_affect_order_semantics` for why.
fn strip_level_statistics(mut value: serde_json::Value) -> serde_json::Value {
    for side in ["bids", "asks"] {
        if let Some(levels) = value.get_mut(side).and_then(|s| s.as_array_mut()) {
            for level in levels {
                if let Some(obj) = level.as_object_mut() {
                    obj.remove("statistics");
                }
            }
        }
    }
    value
}

/// A reserve BUY at 100 with 10 visible / 20 hidden and the given
/// replenishment policy.
fn reserve_buy(id: Id, auto_replenish: bool) -> OrderType<()> {
    OrderType::ReserveOrder {
        id,
        price: Price::new(100),
        visible_quantity: Quantity::new(10),
        hidden_quantity: Quantity::new(20),
        side: Side::Buy,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(0),
        time_in_force: TimeInForce::Gtc,
        replenish_threshold: Quantity::new(0),
        replenish_amount: None,
        auto_replenish,
        extra_fields: (),
    }
}

/// #230: the engine-side measurement of discarded quantity. A reserve
/// taker whose visible tranche the sweep exhausts without automatic
/// replenishment drops its hidden remainder; both the order count and the
/// quantity are counted, and neither moves when the residual rests.
///
/// This lives here rather than in `tests/unit/` because only this test
/// binary installs a `metrics` recorder: the conservation assertions over
/// in `reserve_residual_policy_tests` therefore measure `executed` three
/// independent ways and take the discard as a per-case literal, while the
/// engine's own count of what it dropped is asserted here.
#[test]
fn reserve_discard_counters_track_dropped_hidden_quantity() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();

    let discards_before = counter_value(RESERVE_DISCARDS_TOTAL);
    let quantity_before = counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL);

    // Resting branch: an auto-replenishing residual refreshes and rests, so
    // nothing is discarded and neither counter may move.
    let resting_book = OrderBook::<()>::new("METRICS-RSV-REST");
    resting_book
        .add_limit_order(new_id(), 100, 10, Side::Sell, TimeInForce::Gtc, None)
        .expect("seed contra depth");
    resting_book
        .add_order(reserve_buy(new_id(), true))
        .expect("auto-replenishing reserve rests its residual");

    assert_eq!(
        counter_value(RESERVE_DISCARDS_TOTAL),
        discards_before,
        "a refreshed, resting residual must not count as a discard"
    );
    assert_eq!(
        counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL),
        quantity_before,
        "a refreshed, resting residual must not count discarded quantity"
    );

    // Discard branch: the same shape without automatic replenishment ends
    // with its 20 hidden units dropped.
    let discard_book = OrderBook::<()>::new("METRICS-RSV-DISCARD");
    discard_book
        .add_limit_order(new_id(), 100, 10, Side::Sell, TimeInForce::Gtc, None)
        .expect("seed contra depth");
    discard_book
        .add_order(reserve_buy(new_id(), false))
        .expect("the submit succeeds; the residual is discarded, not rejected");

    assert_eq!(
        counter_value(RESERVE_DISCARDS_TOTAL) - discards_before,
        1,
        "orderbook_reserve_discards_total must count exactly one discarded order"
    );
    assert_eq!(
        counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL) - quantity_before,
        20,
        "orderbook_reserve_hidden_discarded_total must count the 20 dropped hidden units"
    );
}

/// #230: the maker side of the same discard feeds the same counters. A
/// non-auto-replenishing reserve resting as a maker is removed by
/// `pricelevel` once its visible tranche is taken, stranding its hidden
/// depth; that is the same loss of resting quantity as the aggressive
/// residual discard and must be just as observable.
#[test]
fn reserve_discard_counters_track_the_maker_path_too() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();

    let discards_before = counter_value(RESERVE_DISCARDS_TOTAL);
    let quantity_before = counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL);

    // The reserve rests as a maker, then an aggressive sell of 20 arrives.
    // Only the maker's visible tranche of 10 is exposed, so the sell takes
    // that, the level drops the 20 hidden with the maker, and the sell rests
    // its own remainder of 10 as an ask.
    let book = OrderBook::<()>::new("METRICS-RSV-MAKER");
    let maker_id = new_id();
    book.add_order(reserve_buy(maker_id, false))
        .expect("the reserve rests as a maker");
    book.add_limit_order(new_id(), 100, 20, Side::Sell, TimeInForce::Gtc, None)
        .expect("the aggressive sell takes the visible tranche");

    assert!(
        book.get_order(maker_id).is_none(),
        "the depleted non-replenishing maker leaves the book"
    );
    assert_eq!(
        book.best_ask(),
        Some(100),
        "the sell rests the 10 the stranded hidden tranche could not fill"
    );
    assert_eq!(
        counter_value(RESERVE_DISCARDS_TOTAL) - discards_before,
        1,
        "the maker removal must count exactly one discarded order"
    );
    assert_eq!(
        counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL) - quantity_before,
        20,
        "the maker removal must count the 20 stranded hidden units"
    );
}

/// A maker that **does** replenish is not a discard: automatic
/// replenishment refreshes its visible tranche from hidden, so nothing is
/// lost and neither counter moves.
#[test]
fn reserve_discard_counters_ignore_a_replenishing_maker() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();

    let discards_before = counter_value(RESERVE_DISCARDS_TOTAL);
    let quantity_before = counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL);

    let book = OrderBook::<()>::new("METRICS-RSV-MAKER-AUTO");
    let maker_id = new_id();
    book.add_order(reserve_buy(maker_id, true))
        .expect("the reserve rests as a maker");
    book.add_limit_order(new_id(), 100, 20, Side::Sell, TimeInForce::Gtc, None)
        .expect("the aggressive sell takes the visible tranche");

    // The same sell of 20 against a replenishing maker: the first 10 take
    // the visible tranche, `min(80, 20) = 20` refreshes it from hidden, and
    // the remaining 10 come out of the refreshed tranche. The maker survives
    // holding 10 visible / 0 hidden, so the full 30 stays accounted for and
    // nothing was dropped.
    match book.get_order(maker_id) {
        Some(order) => assert_eq!(
            (
                order.visible_quantity().as_u64(),
                order.hidden_quantity().as_u64()
            ),
            (10, 0),
            "a replenishing maker survives, refreshed and then partly taken"
        ),
        None => panic!("a replenishing maker must not leave the book"),
    }
    assert_eq!(
        counter_value(RESERVE_DISCARDS_TOTAL),
        discards_before,
        "a refreshed maker must not count as a discard"
    );
    assert_eq!(
        counter_value(RESERVE_HIDDEN_DISCARDED_TOTAL),
        quantity_before,
        "a refreshed maker must not count discarded quantity"
    );
}

/// Cross one resting sell with a market buy: exactly one trade event.
fn cross_once(book: &OrderBook<()>) {
    book.add_limit_order(new_id(), 100, 10, Side::Sell, TimeInForce::Gtc, None)
        .expect("rest maker");
    book.submit_market_order(new_id(), 10, Side::Buy)
        .expect("cross maker");
}

#[test]
fn manager_dropped_trade_events_counter_tracks_both_managers() {
    let _guard = serialized_test_lock().lock().expect("serialized lock");
    install_recorder();
    let before = counter_value(MANAGER_TRADE_EVENTS_DROPPED_TOTAL);

    // Std: a trade after the processor stopped is dropped and counted.
    let mut std_mgr: BookManagerStd<()> = BookManagerStd::new();
    std_mgr.add_book("MGR-STD").expect("add book");
    std_mgr.start_trade_processor().expect("start");
    cross_once(std_mgr.get_book("MGR-STD").expect("book"));
    std_mgr.stop_trade_processor().expect("stop");
    assert_eq!(
        counter_value(MANAGER_TRADE_EVENTS_DROPPED_TOTAL),
        before,
        "a delivered event is not a drop"
    );
    cross_once(std_mgr.get_book("MGR-STD").expect("book"));
    assert_eq!(std_mgr.dropped_trade_events(), 1);
    assert_eq!(
        counter_value(MANAGER_TRADE_EVENTS_DROPPED_TOTAL),
        before + 1
    );

    // Tokio: same contract.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut tokio_mgr: BookManagerTokio<()> = BookManagerTokio::new();
    tokio_mgr.add_book("MGR-TOKIO").expect("add book");
    runtime.block_on(async {
        tokio_mgr.start_trade_processor().expect("start");
        tokio_mgr.stop_trade_processor().await.expect("stop");
    });
    cross_once(tokio_mgr.get_book("MGR-TOKIO").expect("book"));
    assert_eq!(tokio_mgr.dropped_trade_events(), 1);
    assert_eq!(
        counter_value(MANAGER_TRADE_EVENTS_DROPPED_TOTAL),
        before + 2
    );
}
