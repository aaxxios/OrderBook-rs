/******************************************************************************
   Author: Joaquín Béjar García
   Email: jb@taunais.com
   Date: 2/10/25
******************************************************************************/

//! Multi-book management with centralized trade event routing.
//!
//! This module provides book management through a trait-based design, with implementations
//! for both standard library (`BookManagerStd`) and Tokio (`BookManagerTokio`) channels.
//!
//! # Trade processor lifecycle (#255)
//!
//! Both managers route every trade of every managed book through one
//! channel to a single trade processor, and expose the same lifecycle:
//!
//! - `start_trade_processor()` / `start_trade_processor_with(handler)` start
//!   the processor. Failures are typed, never panics: `BookManagerStd`
//!   reports [`ManagerError::ThreadSpawn`] when the OS refuses the thread,
//!   `BookManagerTokio` reports [`ManagerError::NoRuntime`] when called
//!   outside a Tokio runtime (or use `start_trade_processor_on` with an
//!   explicit [`tokio::runtime::Handle`]). A failed start consumes nothing
//!   and can be retried.
//! - `stop_trade_processor()` signals the processor, lets it handle every
//!   event queued before the signal, and joins it (`BookManagerStd`) or
//!   awaits its `JoinHandle` (`BookManagerTokio`). A processor that
//!   panicked is reported as [`ManagerError::ProcessorPanicked`]; a Tokio
//!   task that was cancelled as [`ManagerError::ProcessorCancelled`].
//! - `dropped_trade_events()` counts trade events a listener could not
//!   deliver because the processor is gone (stopped, or panicked). The
//!   first drop is logged at `ERROR` once per manager instead of once per
//!   event; with the `metrics` feature each drop also increments
//!   `orderbook_manager_trade_events_dropped_total`.

use crate::orderbook::OrderBook;
use crate::orderbook::error::{ManagerError, OrderBookError};
use crate::orderbook::mass_cancel::MassCancelResult;
use crate::orderbook::trade::{TradeEvent, TradeListener, TradeResult};
use pricelevel::{Hash32, OrderType, Side, TimestampMs};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{debug, error, info};

/// Per-book outcome of [`OrderBook::evict_expired_orders`]: the evicted
/// orders, or the error that refused the sweep before anything was evicted.
type EvictResult<T> = Result<Vec<Arc<OrderType<T>>>, OrderBookError>;

/// Name given to the `BookManagerStd` trade processor thread.
const STD_PROCESSOR_THREAD_NAME: &str = "orderbook-trade-processor";

/// Message carried on a manager's trade-event channel.
#[expect(
    clippy::large_enum_variant,
    reason = "every message but the single Shutdown is a Trade; boxing it would add \
              one allocation per trade on the listener path for no size benefit"
)]
enum ProcessorMessage {
    /// A trade event produced by one of the managed books' listeners.
    Trade(TradeEvent),
    /// Stop signal sent by `stop_trade_processor`: the processor handles
    /// what is already queued and exits.
    Shutdown,
}

/// Dropped-event accounting shared by a manager and every trade listener it
/// installs (#255).
#[derive(Debug, Default)]
struct DropTracker {
    /// Trade events that could not be delivered to the processor.
    dropped: AtomicU64,
    /// Whether the first drop has already been logged. The processor cannot
    /// come back once its receiver is gone, so a manager has at most one
    /// drop episode and it is logged once, not once per event.
    logged: AtomicBool,
}

impl DropTracker {
    /// Count one undeliverable trade event for `symbol`.
    #[cold]
    fn record_drop(&self, symbol: &str) {
        // Checked increment (never wraps). `fetch_update` stores nothing when
        // the closure returns `None`, so the counter stays at `u64::MAX` in
        // the unreachable overflow case instead of wrapping to zero.
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            });
        crate::orderbook::metrics::record_manager_trade_event_dropped();
        if !self.logged.swap(true, Ordering::Relaxed) {
            error!(
                symbol,
                "trade processor is gone (stopped or panicked); trade events are \
                 being dropped and counted in dropped_trade_events() (logged once)"
            );
        }
    }

    /// Number of trade events dropped so far.
    #[inline]
    fn count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Build the event a manager's trade listener forwards to its processor.
#[inline]
fn trade_event_from(trade_result: &TradeResult) -> TradeEvent {
    TradeEvent {
        symbol: trade_result.symbol.clone(),
        trade_result: trade_result.clone(),
        // Wall-clock stamp for observability only: `TradeEvent::timestamp` is
        // not journaled and does not feed matching or replay (the engine's own
        // time comes from the injected `Clock`). `current_time_millis` reports
        // 0 if the system clock reads before the Unix epoch; a fallible
        // variant is tracked in #257 and this call site will adopt it there.
        timestamp: crate::current_time_millis(),
        engine_seq: trade_result.engine_seq,
    }
}

/// Default trade processor handler: log the event and each of its trades.
fn log_trade_event(event: TradeEvent) {
    let trades = event.trade_result.match_result.trades().as_vec();
    info!(
        symbol = %event.symbol,
        trades = trades.len(),
        executed_quantity = event
            .trade_result
            .match_result
            .executed_quantity()
            .map_or(0, |q| q.as_u64()),
        "processing trade event"
    );
    for trade in trades {
        info!(
            quantity = %trade.quantity(),
            price = %trade.price(),
            trade_id = %trade.trade_id(),
            "trade"
        );
    }
}

/// Text of a panic payload, for [`ManagerError::ProcessorPanicked`].
#[cold]
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Body of the `BookManagerStd` trade processor thread.
fn run_std_processor<F>(receiver: std::sync::mpsc::Receiver<ProcessorMessage>, mut handler: F)
where
    F: FnMut(TradeEvent),
{
    info!("Trade processor started");
    while let Ok(message) = receiver.recv() {
        match message {
            ProcessorMessage::Trade(event) => handler(event),
            ProcessorMessage::Shutdown => {
                // Handle whatever raced in behind the signal before exiting;
                // once this thread returns the receiver drops and later sends
                // fail, which the listeners count as dropped.
                loop {
                    match receiver.try_recv() {
                        Ok(ProcessorMessage::Trade(event)) => handler(event),
                        Ok(ProcessorMessage::Shutdown) => {}
                        Err(_) => break,
                    }
                }
                break;
            }
        }
    }
    info!("Trade processor stopped");
}

/// Trait for managing multiple order books with centralized trade event routing.
///
/// This trait defines the interface for book managers, allowing different
/// implementations using various channel types (std::mpsc, tokio::mpsc, etc.).
pub trait BookManager<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Add a new order book for a symbol with an automatically configured trade listener.
    ///
    /// # Errors
    ///
    /// Returns [`ManagerError::BookAlreadyExists`] if a book already exists for
    /// `symbol` — `add_book` refuses to overwrite it, which would silently drop
    /// the existing book's resting orders and order locations.
    fn add_book(&mut self, symbol: &str) -> Result<(), ManagerError>;

    /// Get a reference to an order book by symbol.
    fn get_book(&self, symbol: &str) -> Option<&OrderBook<T>>;

    /// Get a mutable reference to an order book by symbol.
    fn get_book_mut(&mut self, symbol: &str) -> Option<&mut OrderBook<T>>;

    /// Get the list of all symbols with order books in this manager.
    fn symbols(&self) -> Vec<String>;

    /// Remove an order book for a specific symbol.
    fn remove_book(&mut self, symbol: &str) -> Option<OrderBook<T>>;

    /// Check if a book exists for a specific symbol.
    fn has_book(&self, symbol: &str) -> bool;

    /// Get the number of order books in this manager.
    fn book_count(&self) -> usize;
}

/// BookManager implementation using standard library mpsc channels.
///
/// # Trade-event channel is unbounded by design
///
/// Trade events are pushed onto a `std::sync::mpsc` channel, which is
/// **unbounded**. This is deliberate: the matching path must never block to
/// deliver an audit event, so the producer cannot apply backpressure (a bounded
/// channel would force the synchronous matching path to block or to silently
/// drop trade events — both unacceptable).
///
/// **Start the processor before submitting orders.** The receiver sits in an
/// `Option` until [`start_trade_processor`](Self::start_trade_processor) is
/// called. If the consumer is never started — or lags persistently — trade
/// events buffer **without bound** and grow memory. Call `start_trade_processor`
/// before routing order flow, and keep the consumer draining at least as fast as
/// trades are produced.
///
/// # Shutdown
///
/// [`stop_trade_processor`](Self::stop_trade_processor) is the explicit
/// shutdown path: it handles every queued event, then joins the thread. If
/// the manager is dropped without it, the thread exits on its own once every
/// sender is gone (the manager's and each book's listener), but nobody joins
/// it and a panic in it goes unreported. A book taken out with
/// [`remove_book`](BookManager::remove_book) keeps its listener, so it keeps
/// the channel open until it is dropped too.
pub struct BookManagerStd<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Collection of order books indexed by symbol
    books: HashMap<String, OrderBook<T>>,
    /// Sender for trade events and the shutdown signal
    trade_sender: std::sync::mpsc::Sender<ProcessorMessage>,
    /// Receiver for trade events (taken when processor starts)
    trade_receiver: Option<std::sync::mpsc::Receiver<ProcessorMessage>>,
    /// Running processor thread, joined by `stop_trade_processor`
    processor: Option<std::thread::JoinHandle<()>>,
    /// Dropped-event accounting shared with every book's listener
    drops: Arc<DropTracker>,
}

impl<T> BookManagerStd<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Create a new BookManagerStd with a standard library mpsc channel.
    pub fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();

        Self {
            books: HashMap::new(),
            trade_sender: sender,
            trade_receiver: Some(receiver),
            processor: None,
            drops: Arc::new(DropTracker::default()),
        }
    }

    /// Start the trade event processor in a separate thread, logging every
    /// trade event at `INFO`.
    ///
    /// Equivalent to [`start_trade_processor_with`](Self::start_trade_processor_with)
    /// with the built-in logging handler. See that method for the contract.
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorAlreadyStarted`] if the processor has
    ///   already been started (including one that was since stopped).
    /// - [`ManagerError::ThreadSpawn`] if the OS refused to spawn the thread;
    ///   nothing was consumed and the start can be retried.
    pub fn start_trade_processor(&mut self) -> Result<(), ManagerError> {
        self.start_trade_processor_with(log_trade_event)
    }

    /// Start the trade event processor in a separate named thread
    /// (`orderbook-trade-processor`), calling `handler` for every trade event
    /// of every managed book, in channel order.
    ///
    /// **Call this before submitting orders.** The trade-event channel is
    /// unbounded (see the [type-level docs](BookManagerStd)); until the
    /// processor is running, every trade event buffers in the channel without
    /// bound. Events buffered before the start are handled first. The
    /// processor runs until [`stop_trade_processor`](Self::stop_trade_processor).
    ///
    /// `handler` is caller-supplied code running on the processor thread. It
    /// must not panic: a panic ends the processor, later trade events are
    /// counted in [`dropped_trade_events`](Self::dropped_trade_events), and
    /// `stop_trade_processor` reports it as [`ManagerError::ProcessorPanicked`].
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorAlreadyStarted`] if the processor has
    ///   already been started (including one that was since stopped).
    /// - [`ManagerError::ThreadSpawn`] if the OS refused to spawn the thread.
    ///   The channel is kept, so the start can be retried; `handler` is
    ///   dropped.
    pub fn start_trade_processor_with<F>(&mut self, handler: F) -> Result<(), ManagerError>
    where
        F: FnMut(TradeEvent) + Send + 'static,
    {
        let receiver = self
            .trade_receiver
            .take()
            .ok_or(ManagerError::ProcessorAlreadyStarted)?;

        // The receiver is handed to the thread only once it exists, so a
        // refused spawn leaves it here for a retry instead of dropping it
        // with the closure.
        let (handoff_tx, handoff_rx) =
            std::sync::mpsc::channel::<std::sync::mpsc::Receiver<ProcessorMessage>>();
        let spawned = std::thread::Builder::new()
            .name(STD_PROCESSOR_THREAD_NAME.to_string())
            .spawn(move || {
                if let Ok(receiver) = handoff_rx.recv() {
                    run_std_processor(receiver, handler);
                }
            });

        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                self.trade_receiver = Some(receiver);
                error!(error = %e, "failed to spawn trade processor thread");
                return Err(ManagerError::ThreadSpawn {
                    kind: e.kind(),
                    message: e.to_string(),
                });
            }
        };

        // The thread owns `handoff_rx` until it receives, so this send only
        // fails if the thread is already gone; recover the receiver then.
        if let Err(std::sync::mpsc::SendError(receiver)) = handoff_tx.send(receiver) {
            self.trade_receiver = Some(receiver);
            let message = match thread.join() {
                Ok(()) => "trade processor thread exited before start".to_string(),
                Err(payload) => panic_message(&*payload),
            };
            error!(%message, "trade processor thread did not start");
            return Err(ManagerError::ThreadSpawn {
                kind: std::io::ErrorKind::Other,
                message,
            });
        }

        self.processor = Some(thread);
        Ok(())
    }

    /// Stop the trade processor and join its thread.
    ///
    /// Sends a stop signal down the trade-event channel; the processor
    /// handles every event queued before (and any that raced in right behind)
    /// the signal, then exits. Blocks the calling thread until then. Once it
    /// has returned, trade events from the managed books are no longer
    /// processed: they are counted in
    /// [`dropped_trade_events`](Self::dropped_trade_events). The processor
    /// cannot be started again.
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorNotRunning`] if the processor was never
    ///   started or was already stopped.
    /// - [`ManagerError::ProcessorPanicked`] if the processor thread panicked
    ///   (in the handler or the `tracing` subscriber). The processor is
    ///   stopped either way.
    pub fn stop_trade_processor(&mut self) -> Result<(), ManagerError> {
        let thread = self
            .processor
            .take()
            .ok_or(ManagerError::ProcessorNotRunning)?;

        if self.trade_sender.send(ProcessorMessage::Shutdown).is_err() {
            debug!("trade processor exited before the stop signal");
        }

        match thread.join() {
            Ok(()) => {
                info!("Trade processor joined");
                Ok(())
            }
            Err(payload) => {
                let message = panic_message(&*payload);
                error!(%message, "trade processor panicked");
                Err(ManagerError::ProcessorPanicked { message })
            }
        }
    }

    /// Number of trade events the managed books' listeners could not deliver
    /// because the trade processor is gone (stopped, or panicked).
    ///
    /// Monotonic for the manager's lifetime. Events buffered while the
    /// processor has not started yet are not dropped and not counted.
    #[must_use]
    #[inline]
    pub fn dropped_trade_events(&self) -> u64 {
        self.drops.count()
    }
}

impl<T> BookManagerStd<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Cancel all orders across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    /// Books with no orders produce a result with `cancelled_count == 0`.
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::orderbook::manager::{BookManager, BookManagerStd};
    /// use pricelevel::{Id, Side, TimeInForce};
    /// use uuid::Uuid;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut mgr: BookManagerStd<()> = BookManagerStd::new();
    /// mgr.add_book("BTC/USD")?;
    /// mgr.add_book("ETH/USD")?;
    ///
    /// if let Some(book) = mgr.get_book("BTC/USD") {
    ///     book.add_limit_order(Id::from_uuid(Uuid::new_v4()), 100, 10, Side::Buy, TimeInForce::Gtc, None).ok();
    /// }
    ///
    /// let results = mgr.cancel_all_across_books();
    /// assert!(results.contains_key("BTC/USD"));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn cancel_all_across_books(&self) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_all_orders()))
            .collect()
    }

    /// Cancel all orders for a specific user across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    ///
    /// # Arguments
    ///
    /// * `user_id` — the user whose orders should be cancelled
    #[must_use]
    pub fn cancel_by_user_across_books(
        &self,
        user_id: Hash32,
    ) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_orders_by_user(user_id)))
            .collect()
    }

    /// Cancel all orders on a specific side across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    ///
    /// # Arguments
    ///
    /// * `side` — the side to cancel ([`Side::Buy`] or [`Side::Sell`])
    #[must_use]
    pub fn cancel_by_side_across_books(&self, side: Side) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_orders_by_side(side)))
            .collect()
    }

    /// Evict expired resting orders from a single managed book.
    ///
    /// Pass-through to [`OrderBook::evict_expired_orders`]. `now_ms` is
    /// caller-supplied Unix milliseconds (see that method for the boundary and
    /// determinism contract). Returns `None` when `symbol` is not managed, or
    /// `Some(result)` with the book's own result: the evicted orders in the
    /// book's documented deterministic order (empty when nothing expired), or
    /// the [`OrderBookError`] that refused the sweep before anything was
    /// evicted.
    #[must_use]
    pub fn evict_expired_orders(
        &self,
        symbol: &str,
        now_ms: TimestampMs,
    ) -> Option<EvictResult<T>> {
        self.books
            .get(symbol)
            .map(|book| book.evict_expired_orders(now_ms))
    }

    /// Evict expired resting orders across all managed books at `now_ms`.
    ///
    /// Returns a map from symbol to that book's result: its evicted orders (in
    /// the book's documented deterministic order), or the [`OrderBookError`]
    /// that refused that book's sweep before anything was evicted. A failing
    /// book does not stop the others. Books with nothing expired map to
    /// `Ok` with an empty vector. `now_ms` is caller-supplied Unix
    /// milliseconds.
    #[must_use]
    pub fn evict_expired_across_books(
        &self,
        now_ms: TimestampMs,
    ) -> HashMap<String, EvictResult<T>> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.evict_expired_orders(now_ms)))
            .collect()
    }
}

impl<T> BookManager<T> for BookManagerStd<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    fn add_book(&mut self, symbol: &str) -> Result<(), ManagerError> {
        if self.books.contains_key(symbol) {
            return Err(ManagerError::BookAlreadyExists {
                symbol: symbol.to_string(),
            });
        }

        let sender = self.trade_sender.clone();
        let drops = Arc::clone(&self.drops);
        let symbol_clone = symbol.to_string();

        let trade_listener: TradeListener = Arc::new(move |trade_result: &TradeResult| {
            let message = ProcessorMessage::Trade(trade_event_from(trade_result));
            if sender.send(message).is_err() {
                drops.record_drop(&symbol_clone);
            }
        });

        let book = OrderBook::with_trade_listener(symbol, trade_listener);
        self.books.insert(symbol.to_string(), book);
        info!("Added order book for symbol: {}", symbol);
        Ok(())
    }

    fn get_book(&self, symbol: &str) -> Option<&OrderBook<T>> {
        self.books.get(symbol)
    }

    fn get_book_mut(&mut self, symbol: &str) -> Option<&mut OrderBook<T>> {
        self.books.get_mut(symbol)
    }

    fn symbols(&self) -> Vec<String> {
        self.books.keys().cloned().collect()
    }

    fn remove_book(&mut self, symbol: &str) -> Option<OrderBook<T>> {
        let result = self.books.remove(symbol);
        if result.is_some() {
            info!("Removed order book for symbol: {}", symbol);
        }
        result
    }

    fn has_book(&self, symbol: &str) -> bool {
        self.books.contains_key(symbol)
    }

    fn book_count(&self) -> usize {
        self.books.len()
    }
}

impl<T> Default for BookManagerStd<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// BookManager implementation using Tokio mpsc channels.
///
/// # Trade-event channel is unbounded by design
///
/// Trade events are pushed onto a `tokio::sync::mpsc::unbounded_channel`. As with
/// [`BookManagerStd`], this is deliberate: the matching path must never block to
/// deliver an audit event, so the producer applies no backpressure (a bounded
/// channel would force the synchronous matching path to block or to silently drop
/// trade events).
///
/// **Start the processor before submitting orders.** The receiver sits in an
/// `Option` until [`start_trade_processor`](Self::start_trade_processor) is
/// called. If the consumer is never started — or lags persistently — trade
/// events buffer **without bound** and grow memory. Start the processor before
/// routing order flow and keep the consumer draining at least as fast as trades
/// are produced.
///
/// # Shutdown
///
/// [`stop_trade_processor`](Self::stop_trade_processor) is the explicit
/// shutdown path: it closes the channel, lets the task handle every queued
/// event, then awaits its `JoinHandle`. If the manager is dropped without
/// it, the task exits on its own once every sender is gone (the manager's
/// and each book's listener), but nobody awaits it and a panic in it goes
/// unreported. A book taken out with
/// [`remove_book`](BookManager::remove_book) keeps its listener, so it keeps
/// the channel open until it is dropped too.
pub struct BookManagerTokio<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Collection of order books indexed by symbol
    books: HashMap<String, OrderBook<T>>,
    /// Sender for trade events and the shutdown signal
    trade_sender: tokio::sync::mpsc::UnboundedSender<ProcessorMessage>,
    /// Receiver for trade events (taken when processor starts)
    trade_receiver: Option<tokio::sync::mpsc::UnboundedReceiver<ProcessorMessage>>,
    /// Running processor task, awaited by `stop_trade_processor`
    processor: Option<tokio::task::JoinHandle<()>>,
    /// Dropped-event accounting shared with every book's listener
    drops: Arc<DropTracker>,
}

impl<T> BookManagerTokio<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Create a new BookManagerTokio with a Tokio unbounded mpsc channel.
    ///
    /// Does not need a runtime; only starting the processor does.
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        Self {
            books: HashMap::new(),
            trade_sender: sender,
            trade_receiver: Some(receiver),
            processor: None,
            drops: Arc::new(DropTracker::default()),
        }
    }

    /// Start the trade event processor as a task on the current Tokio
    /// runtime, logging every trade event at `INFO`.
    ///
    /// Equivalent to [`start_trade_processor_with`](Self::start_trade_processor_with)
    /// with the built-in logging handler. See that method for the contract.
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorAlreadyStarted`] if the processor has
    ///   already been started (including one that was since stopped).
    /// - [`ManagerError::NoRuntime`] if called outside a Tokio runtime;
    ///   nothing was consumed and the start can be retried.
    pub fn start_trade_processor(&mut self) -> Result<(), ManagerError> {
        self.start_trade_processor_with(log_trade_event)
    }

    /// Start the trade event processor as a task on the current Tokio
    /// runtime, calling `handler` for every trade event of every managed
    /// book, in channel order.
    ///
    /// Uses [`tokio::runtime::Handle::try_current`], so calling it outside a
    /// runtime is an error, not a panic. To start from a thread without a
    /// runtime, use [`start_trade_processor_on`](Self::start_trade_processor_on).
    ///
    /// **Call this before submitting orders.** The trade-event channel is
    /// unbounded (see the [type-level docs](BookManagerTokio)); until the
    /// processor is running, every trade event buffers in the channel without
    /// bound. Events buffered before the start are handled first. The
    /// processor runs until [`stop_trade_processor`](Self::stop_trade_processor).
    ///
    /// `handler` is caller-supplied code running inside the task. It must
    /// not panic and must return quickly (it runs on a runtime worker; move
    /// blocking work to `tokio::task::spawn_blocking`). A panic ends the
    /// processor, later trade events are counted in
    /// [`dropped_trade_events`](Self::dropped_trade_events), and
    /// `stop_trade_processor` reports it as [`ManagerError::ProcessorPanicked`].
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorAlreadyStarted`] if the processor has
    ///   already been started (including one that was since stopped).
    /// - [`ManagerError::NoRuntime`] if called outside a Tokio runtime. The
    ///   channel is kept, so the start can be retried; `handler` is dropped.
    pub fn start_trade_processor_with<F>(&mut self, handler: F) -> Result<(), ManagerError>
    where
        F: FnMut(TradeEvent) + Send + 'static,
    {
        if self.trade_receiver.is_none() {
            return Err(ManagerError::ProcessorAlreadyStarted);
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| ManagerError::NoRuntime)?;
        self.start_trade_processor_on(&runtime, handler)
    }

    /// Start the trade event processor as a task on the runtime behind
    /// `runtime`, calling `handler` for every trade event.
    ///
    /// Same contract as [`start_trade_processor_with`](Self::start_trade_processor_with),
    /// but callable from any thread, with or without a current runtime. If
    /// that runtime has already shut down, Tokio cancels the task at once:
    /// [`stop_trade_processor`](Self::stop_trade_processor) then reports
    /// [`ManagerError::ProcessorCancelled`] and trade events are counted in
    /// [`dropped_trade_events`](Self::dropped_trade_events).
    ///
    /// # Errors
    ///
    /// Returns [`ManagerError::ProcessorAlreadyStarted`] if the processor has
    /// already been started (including one that was since stopped).
    pub fn start_trade_processor_on<F>(
        &mut self,
        runtime: &tokio::runtime::Handle,
        mut handler: F,
    ) -> Result<(), ManagerError>
    where
        F: FnMut(TradeEvent) + Send + 'static,
    {
        let mut receiver = self
            .trade_receiver
            .take()
            .ok_or(ManagerError::ProcessorAlreadyStarted)?;

        let task = runtime.spawn(async move {
            info!("Trade processor started (Tokio)");

            while let Some(message) = receiver.recv().await {
                match message {
                    ProcessorMessage::Trade(event) => handler(event),
                    // Closing refuses new sends (the listeners count them as
                    // dropped) while `recv` still yields what is queued, then
                    // `None`.
                    ProcessorMessage::Shutdown => receiver.close(),
                }
            }

            info!("Trade processor stopped (Tokio)");
        });

        self.processor = Some(task);
        Ok(())
    }

    /// Stop the trade processor and await its task.
    ///
    /// Sends a stop signal down the trade-event channel; the task closes the
    /// channel, handles every event already queued, then exits. Once the
    /// channel is closed, trade events from the managed books are no longer
    /// processed: they are counted in
    /// [`dropped_trade_events`](Self::dropped_trade_events). The processor
    /// cannot be started again.
    ///
    /// Not cancel-safe: if this future is dropped before it completes, the
    /// stop signal has been sent and the task still exits, but its
    /// `JoinHandle` is dropped, so its outcome is lost and a later call
    /// returns [`ManagerError::ProcessorNotRunning`].
    ///
    /// # Errors
    ///
    /// - [`ManagerError::ProcessorNotRunning`] if the processor was never
    ///   started or was already stopped.
    /// - [`ManagerError::ProcessorPanicked`] if the task panicked (in the
    ///   handler or the `tracing` subscriber).
    /// - [`ManagerError::ProcessorCancelled`] if the task was cancelled, for
    ///   example because its runtime shut down.
    ///
    /// The processor is stopped in every case.
    pub async fn stop_trade_processor(&mut self) -> Result<(), ManagerError> {
        let task = self
            .processor
            .take()
            .ok_or(ManagerError::ProcessorNotRunning)?;

        if self.trade_sender.send(ProcessorMessage::Shutdown).is_err() {
            debug!("trade processor exited before the stop signal (Tokio)");
        }

        match task.await {
            Ok(()) => {
                info!("Trade processor joined (Tokio)");
                Ok(())
            }
            Err(join_error) if join_error.is_panic() => {
                let payload = join_error.into_panic();
                let message = panic_message(&*payload);
                error!(%message, "trade processor panicked (Tokio)");
                Err(ManagerError::ProcessorPanicked { message })
            }
            Err(_) => {
                error!("trade processor task was cancelled (Tokio)");
                Err(ManagerError::ProcessorCancelled)
            }
        }
    }

    /// Number of trade events the managed books' listeners could not deliver
    /// because the trade processor is gone (stopped, panicked, or cancelled).
    ///
    /// Monotonic for the manager's lifetime. Events buffered while the
    /// processor has not started yet are not dropped and not counted.
    #[must_use]
    #[inline]
    pub fn dropped_trade_events(&self) -> u64 {
        self.drops.count()
    }
}

impl<T> BookManagerTokio<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Cancel all orders across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    /// Books with no orders produce a result with `cancelled_count == 0`.
    #[must_use]
    pub fn cancel_all_across_books(&self) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_all_orders()))
            .collect()
    }

    /// Cancel all orders for a specific user across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    ///
    /// # Arguments
    ///
    /// * `user_id` — the user whose orders should be cancelled
    #[must_use]
    pub fn cancel_by_user_across_books(
        &self,
        user_id: Hash32,
    ) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_orders_by_user(user_id)))
            .collect()
    }

    /// Cancel all orders on a specific side across all managed books.
    ///
    /// Returns a map from symbol to the [`MassCancelResult`] for that book.
    ///
    /// # Arguments
    ///
    /// * `side` — the side to cancel ([`Side::Buy`] or [`Side::Sell`])
    #[must_use]
    pub fn cancel_by_side_across_books(&self, side: Side) -> HashMap<String, MassCancelResult> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.cancel_orders_by_side(side)))
            .collect()
    }

    /// Evict expired resting orders from a single managed book.
    ///
    /// Pass-through to [`OrderBook::evict_expired_orders`]. `now_ms` is
    /// caller-supplied Unix milliseconds (see that method for the boundary and
    /// determinism contract). Returns `None` when `symbol` is not managed, or
    /// `Some(result)` with the book's own result: the evicted orders in the
    /// book's documented deterministic order (empty when nothing expired), or
    /// the [`OrderBookError`] that refused the sweep before anything was
    /// evicted.
    #[must_use]
    pub fn evict_expired_orders(
        &self,
        symbol: &str,
        now_ms: TimestampMs,
    ) -> Option<EvictResult<T>> {
        self.books
            .get(symbol)
            .map(|book| book.evict_expired_orders(now_ms))
    }

    /// Evict expired resting orders across all managed books at `now_ms`.
    ///
    /// Returns a map from symbol to that book's result: its evicted orders (in
    /// the book's documented deterministic order), or the [`OrderBookError`]
    /// that refused that book's sweep before anything was evicted. A failing
    /// book does not stop the others. Books with nothing expired map to
    /// `Ok` with an empty vector. `now_ms` is caller-supplied Unix
    /// milliseconds.
    #[must_use]
    pub fn evict_expired_across_books(
        &self,
        now_ms: TimestampMs,
    ) -> HashMap<String, EvictResult<T>> {
        self.books
            .iter()
            .map(|(symbol, book)| (symbol.clone(), book.evict_expired_orders(now_ms)))
            .collect()
    }
}

impl<T> BookManager<T> for BookManagerTokio<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    fn add_book(&mut self, symbol: &str) -> Result<(), ManagerError> {
        if self.books.contains_key(symbol) {
            return Err(ManagerError::BookAlreadyExists {
                symbol: symbol.to_string(),
            });
        }

        let sender = self.trade_sender.clone();
        let drops = Arc::clone(&self.drops);
        let symbol_clone = symbol.to_string();

        let trade_listener: TradeListener = Arc::new(move |trade_result: &TradeResult| {
            let message = ProcessorMessage::Trade(trade_event_from(trade_result));
            if sender.send(message).is_err() {
                drops.record_drop(&symbol_clone);
            }
        });

        let book = OrderBook::with_trade_listener(symbol, trade_listener);
        self.books.insert(symbol.to_string(), book);
        info!("Added order book for symbol: {}", symbol);
        Ok(())
    }

    fn get_book(&self, symbol: &str) -> Option<&OrderBook<T>> {
        self.books.get(symbol)
    }

    fn get_book_mut(&mut self, symbol: &str) -> Option<&mut OrderBook<T>> {
        self.books.get_mut(symbol)
    }

    fn symbols(&self) -> Vec<String> {
        self.books.keys().cloned().collect()
    }

    fn remove_book(&mut self, symbol: &str) -> Option<OrderBook<T>> {
        let result = self.books.remove(symbol);
        if result.is_some() {
            info!("Removed order book for symbol: {}", symbol);
        }
        result
    }

    fn has_book(&self, symbol: &str) -> bool {
        self.books.contains_key(symbol)
    }

    fn book_count(&self) -> usize {
        self.books.len()
    }
}

impl<T> Default for BookManagerTokio<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}
