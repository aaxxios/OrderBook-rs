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

// ─── Manager dropped without stop: the stop signal disarms (#255) ───────────

/// Handler forwarding each event's symbol to a std channel the test reads.
fn forwarding_handler() -> (
    std::sync::mpsc::Receiver<String>,
    impl FnMut(TradeEvent) + Send + 'static,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    (rx, move |event: TradeEvent| {
        let _ = tx.send(event.symbol);
    })
}

#[test]
fn test_std_dropped_manager_keeps_processing_removed_book_until_it_drops() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    mgr.add_book(SYMBOL).expect("add book");
    let (rx, handler) = forwarding_handler();
    mgr.start_trade_processor_with(handler).expect("start");
    let book = mgr.remove_book(SYMBOL).expect("removed book");
    // Dropping the manager drops its stop sender: the processor must treat
    // that as "no stop will come", not as a stop, and must not spin.
    drop(mgr);

    trade_once(&book);
    let symbol = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("processor still handles the removed book's events");
    assert_eq!(symbol, SYMBOL);

    // Last sender gone: the processor exits and drops the handler, which
    // disconnects the forwarding channel.
    drop(book);
    assert!(matches!(
        rx.recv_timeout(std::time::Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn test_tokio_dropped_manager_keeps_processing_removed_book_until_it_drops() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .expect("runtime");
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    mgr.add_book(SYMBOL).expect("add book");
    let (rx, handler) = forwarding_handler();
    mgr.start_trade_processor_on(runtime.handle(), handler)
        .expect("start");
    let book = mgr.remove_book(SYMBOL).expect("removed book");
    drop(mgr);

    trade_once(&book);
    let symbol = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("processor still handles the removed book's events");
    assert_eq!(symbol, SYMBOL);

    drop(book);
    assert!(matches!(
        rx.recv_timeout(std::time::Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
}

// ─── Stop overlapping concurrent trading: nothing is lost (#255 review) ─────

const STRESS_THREADS: usize = 4;
const STRESS_TRADES_PER_THREAD: usize = 500;

/// Take `STRESS_THREADS` books out of the manager so other threads can trade
/// on them while the manager (still owning the processor) is stopped.
fn stress_books<M: BookManager<()>>(mgr: &mut M) -> Vec<OrderBook<()>> {
    (0..STRESS_THREADS)
        .map(|i| {
            let symbol = format!("STRESS-{i}");
            mgr.add_book(&symbol).expect("add book");
            mgr.remove_book(&symbol).expect("removed book")
        })
        .collect()
}

/// Trade on every book from its own thread, released together by `barrier`.
fn spawn_traders(
    books: Vec<OrderBook<()>>,
    barrier: &Arc<std::sync::Barrier>,
) -> Vec<std::thread::JoinHandle<()>> {
    books
        .into_iter()
        .map(|book| {
            let barrier = Arc::clone(barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..STRESS_TRADES_PER_THREAD {
                    trade_once(&book);
                }
            })
        })
        .collect()
}

fn counting_handler() -> (Arc<AtomicUsize>, impl FnMut(TradeEvent) + Send + 'static) {
    let processed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&processed);
    (processed, move |_event: TradeEvent| {
        counter.fetch_add(1, Ordering::Relaxed);
    })
}

#[test]
fn test_std_stop_during_concurrent_trading_processes_or_counts_every_event() {
    let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    let books = stress_books(&mut mgr);
    let (processed, handler) = counting_handler();
    mgr.start_trade_processor_with(handler).expect("start");

    let barrier = Arc::new(std::sync::Barrier::new(STRESS_THREADS + 1));
    let traders = spawn_traders(books, &barrier);
    barrier.wait();
    mgr.stop_trade_processor().expect("clean stop");
    for trader in traders {
        trader.join().expect("trader thread");
    }

    let sent = (STRESS_THREADS * STRESS_TRADES_PER_THREAD) as u64;
    let processed = processed.load(Ordering::Relaxed) as u64;
    assert_eq!(
        processed + mgr.dropped_trade_events(),
        sent,
        "every event is processed or counted (processed={processed})"
    );
}

#[test]
fn test_tokio_stop_during_concurrent_trading_processes_or_counts_every_event() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("runtime");
    let mut mgr: BookManagerTokio<()> = BookManagerTokio::new();
    let books = stress_books(&mut mgr);
    let (processed, handler) = counting_handler();
    mgr.start_trade_processor_on(runtime.handle(), handler)
        .expect("start");

    let barrier = Arc::new(std::sync::Barrier::new(STRESS_THREADS + 1));
    let traders = spawn_traders(books, &barrier);
    barrier.wait();
    runtime
        .block_on(mgr.stop_trade_processor())
        .expect("clean stop");
    for trader in traders {
        trader.join().expect("trader thread");
    }

    let sent = (STRESS_THREADS * STRESS_TRADES_PER_THREAD) as u64;
    let processed = processed.load(Ordering::Relaxed) as u64;
    assert_eq!(
        processed + mgr.dropped_trade_events(),
        sent,
        "every event is processed or counted (processed={processed})"
    );
}
