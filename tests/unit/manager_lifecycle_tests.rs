/******************************************************************************
   Trade processor lifecycle of BookManagerStd / BookManagerTokio (#255):
   typed start failures instead of panics, explicit stop path that handles
   pending events and joins / awaits the processor, dropped-event counter,
   and panicked / cancelled processors surfaced as typed errors.
******************************************************************************/

use orderbook_rs::ManagerError;
use orderbook_rs::orderbook::OrderBook;
use orderbook_rs::orderbook::manager::{BookManager, BookManagerStd, BookManagerTokio};
use orderbook_rs::orderbook::trade::TradeEvent;
use pricelevel::{Id, Side, TimeInForce};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const SYMBOL: &str = "BTC/USD";

fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Rest a sell and cross it with a market buy: exactly one trade event.
fn trade_once(book: &OrderBook<()>) {
    book.add_limit_order(new_id(), 100, 10, Side::Sell, TimeInForce::Gtc, None)
        .expect("rest maker");
    book.submit_market_order(new_id(), 10, Side::Buy)
        .expect("cross maker");
}

/// Handler that records the engine sequence of every event it sees.
fn recording_handler() -> (
    Arc<Mutex<Vec<u64>>>,
    impl FnMut(TradeEvent) + Send + 'static,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    (seen, move |event: TradeEvent| {
        sink.lock().expect("sink lock").push(event.engine_seq);
    })
}

fn panicking_handler(event: TradeEvent) {
    panic!("handler boom for {}", event.symbol);
}

// ─── BookManagerStd ─────────────────────────────────────────────────────────

#[test]
fn test_std_stop_trade_processor_handles_pending_events_ok() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    mgr.add_book(SYMBOL).expect("add book");
    // Buffered before the processor exists.
    for _ in 0..3 {
        trade_once(mgr.get_book(SYMBOL).expect("book"));
    }

    let (seen, handler) = recording_handler();
    mgr.start_trade_processor_with(handler).expect("start");
    for _ in 0..2 {
        trade_once(mgr.get_book(SYMBOL).expect("book"));
    }

    mgr.stop_trade_processor().expect("clean stop");
    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(seen.len(), 5, "every queued event handled before exit");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "events handled in channel order");
    assert_eq!(mgr.dropped_trade_events(), 0);
}

#[test]
fn test_std_stop_trade_processor_not_running_returns_error() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    assert!(matches!(
        mgr.stop_trade_processor(),
        Err(ManagerError::ProcessorNotRunning)
    ));

    // Stopping twice: the second call has nothing to stop.
    mgr.start_trade_processor().expect("start");
    mgr.stop_trade_processor().expect("first stop");
    assert!(matches!(
        mgr.stop_trade_processor(),
        Err(ManagerError::ProcessorNotRunning)
    ));
    // A stopped processor cannot be restarted.
    assert!(matches!(
        mgr.start_trade_processor(),
        Err(ManagerError::ProcessorAlreadyStarted)
    ));
}

#[test]
fn test_std_trade_after_stop_increments_dropped_counter() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    mgr.add_book(SYMBOL).expect("add book");
    mgr.start_trade_processor().expect("start");
    mgr.stop_trade_processor().expect("stop");
    assert_eq!(mgr.dropped_trade_events(), 0);

    let book = mgr.get_book(SYMBOL).expect("book");
    trade_once(book);
    assert_eq!(mgr.dropped_trade_events(), 1);
    // The listener keeps counting (and does not panic) on every later drop.
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), 2);
}

#[test]
fn test_std_panicking_handler_surfaces_processor_panicked() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    mgr.add_book(SYMBOL).expect("add book");
    mgr.start_trade_processor_with(panicking_handler)
        .expect("start");
    trade_once(mgr.get_book(SYMBOL).expect("book"));

    match mgr.stop_trade_processor() {
        Err(ManagerError::ProcessorPanicked { message }) => {
            assert!(message.contains("handler boom for BTC/USD"), "{message}");
        }
        other => panic!("expected ProcessorPanicked, got {other:?}"),
    }

    // The processor thread is gone, so the next trade is dropped and counted.
    let before = mgr.dropped_trade_events();
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), before + 1);
}

#[test]
fn test_std_manager_drop_without_stop_ends_processor() {
    // Fallback path: dropping the manager drops every sender, so the thread
    // exits on its own. Nothing to assert beyond "does not hang or panic".
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    mgr.add_book(SYMBOL).expect("add book");
    mgr.start_trade_processor().expect("start");
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    drop(mgr);
}

// `ManagerError::ThreadSpawn` needs the OS to refuse a thread, which a test
// cannot provoke portably; the path keeps the receiver for a retry and is
// covered by review and by the Display test in `src/orderbook/error.rs`.

// ─── BookManagerTokio ───────────────────────────────────────────────────────

#[test]
fn test_tokio_start_outside_runtime_returns_no_runtime() {
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");

    assert!(matches!(
        mgr.start_trade_processor(),
        Err(ManagerError::NoRuntime)
    ));
    let (seen, handler) = recording_handler();
    assert!(matches!(
        mgr.start_trade_processor_with(handler),
        Err(ManagerError::NoRuntime)
    ));

    // Nothing was consumed: a trade buffers, and a later start inside a
    // runtime picks it up.
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (seen_again, handler) = recording_handler();
    runtime.block_on(async {
        mgr.start_trade_processor_with(handler)
            .expect("start inside runtime");
        mgr.stop_trade_processor().await.expect("stop");
    });
    assert!(seen.lock().expect("lock").is_empty());
    assert_eq!(seen_again.lock().expect("lock").len(), 1);
    assert_eq!(mgr.dropped_trade_events(), 0);
}

#[test]
fn test_tokio_start_trade_processor_on_explicit_handle_from_plain_thread() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .expect("runtime");
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");

    // This thread is not inside the runtime.
    let (seen, handler) = recording_handler();
    mgr.start_trade_processor_on(runtime.handle(), handler)
        .expect("start on explicit handle");
    trade_once(mgr.get_book(SYMBOL).expect("book"));

    runtime
        .block_on(mgr.stop_trade_processor())
        .expect("clean stop");
    assert_eq!(seen.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn test_tokio_stop_trade_processor_handles_pending_events_ok() {
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");
    for _ in 0..3 {
        trade_once(mgr.get_book(SYMBOL).expect("book"));
    }

    let (seen, handler) = recording_handler();
    mgr.start_trade_processor_with(handler).expect("start");
    for _ in 0..2 {
        trade_once(mgr.get_book(SYMBOL).expect("book"));
    }

    mgr.stop_trade_processor().await.expect("clean stop");
    let seen = seen.lock().expect("seen lock").clone();
    assert_eq!(seen.len(), 5, "every queued event handled before exit");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    assert_eq!(seen, sorted, "events handled in channel order");
    assert_eq!(mgr.dropped_trade_events(), 0);
}

#[tokio::test]
async fn test_tokio_stop_trade_processor_not_running_returns_error() {
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    assert!(matches!(
        mgr.stop_trade_processor().await,
        Err(ManagerError::ProcessorNotRunning)
    ));

    mgr.start_trade_processor().expect("start");
    assert!(matches!(
        mgr.start_trade_processor(),
        Err(ManagerError::ProcessorAlreadyStarted)
    ));
    mgr.stop_trade_processor().await.expect("first stop");
    assert!(matches!(
        mgr.stop_trade_processor().await,
        Err(ManagerError::ProcessorNotRunning)
    ));
    assert!(matches!(
        mgr.start_trade_processor(),
        Err(ManagerError::ProcessorAlreadyStarted)
    ));
}

#[tokio::test]
async fn test_tokio_trade_after_stop_increments_dropped_counter() {
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");
    mgr.start_trade_processor().expect("start");
    mgr.stop_trade_processor().await.expect("stop");
    assert_eq!(mgr.dropped_trade_events(), 0);

    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), 1);
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), 2);
}

#[tokio::test]
async fn test_tokio_panicking_handler_surfaces_processor_panicked() {
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");
    mgr.start_trade_processor_with(panicking_handler)
        .expect("start");
    trade_once(mgr.get_book(SYMBOL).expect("book"));

    match mgr.stop_trade_processor().await {
        Err(ManagerError::ProcessorPanicked { message }) => {
            assert!(message.contains("handler boom for BTC/USD"), "{message}");
        }
        other => panic!("expected ProcessorPanicked, got {other:?}"),
    }

    let before = mgr.dropped_trade_events();
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), before + 1);
}

#[test]
fn test_tokio_runtime_shut_down_surfaces_processor_cancelled() {
    let dead = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let handle = dead.handle().clone();
    drop(dead);

    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    mgr.start_trade_processor_on(&handle, move |_event| {
        counter.fetch_add(1, Ordering::Relaxed);
    })
    .expect("spawn onto a shut-down runtime is not a start error");

    let live = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    assert!(matches!(
        live.block_on(mgr.stop_trade_processor()),
        Err(ManagerError::ProcessorCancelled)
    ));

    // The cancelled task dropped its receiver: trades are counted as dropped.
    trade_once(mgr.get_book(SYMBOL).expect("book"));
    assert_eq!(mgr.dropped_trade_events(), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn test_tokio_stop_trade_processor_future_is_send() {
    fn assert_send<F: Send>(_: &F) {}
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    let future = mgr.stop_trade_processor();
    assert_send(&future);
}
