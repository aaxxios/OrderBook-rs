//! NATS JetStream order book change publisher.
//!
//! This module provides [`NatsBookChangePublisher`], which converts
//! [`PriceLevelChangedEvent`](crate::orderbook::book_change_event::PriceLevelChangedEvent)s from the order book into batched NATS JetStream
//! messages. Events are collected via a bounded channel and flushed either when
//! the batch window elapses or the batch reaches its maximum size.
//!
//! Published subjects:
//!
//! - `{prefix}.{symbol}.changes` — all changes (mixed sides)
//! - `{prefix}.{symbol}.bid` — bid-side changes only
//! - `{prefix}.{symbol}.ask` — ask-side changes only
//!
//! The listener callback is non-blocking: it sends each event into a bounded
//! channel and returns immediately. A background Tokio task drains the channel,
//! batches events, and publishes to NATS with capped exponential backoff and
//! jitter between retries (see
//! [`BASE_RETRY_DELAY_MS`](crate::orderbook::nats::BASE_RETRY_DELAY_MS) and
//! [`MAX_RETRY_DELAY_MS`](crate::orderbook::nats::MAX_RETRY_DELAY_MS)).
//!
//! Configuration limits and the task-failure error type are shared with the
//! trade publisher and live in [`crate::orderbook::nats`].
//!
//! # Payload format
//!
//! Unlike [`NatsTradePublisher`](crate::orderbook::nats::NatsTradePublisher),
//! this publisher does not take an
//! [`EventSerializer`](crate::orderbook::serialization::EventSerializer): the
//! trait serializes single events, not a [`BookChangeBatch`], so batches are
//! always encoded as **JSON** (`serde_json`). Every message carries a
//! `Content-Type: application/json` header (added in #295; earlier releases
//! sent none) next to `Nats-Sequence`. Pluggable batch serialization is a
//! possible future enhancement.
//!
//! # Error accounting
//!
//! Counters use one **per-batch** granularity: a flushed batch increments
//! `publish_count` once when every subject it targets (`changes`, plus
//! `bid` / `ask` when present) is acknowledged, and `error_count` once
//! otherwise (serialization failure, a subject exhausting its retries, or
//! an exhausted sequence counter), so `publish_count + error_count` equals
//! the number of batches that reached the publish step. The trade publisher
//! applies the same rule to its unit, the trade (both of its subjects
//! count once). Before #295 this publisher counted one error per failed
//! subject.
//!
//! # Runtime requirements
//!
//! The background task runs on the Tokio runtime handle passed to
//! [`NatsBookChangePublisher::new`]. That runtime must have its **time driver
//! enabled** (`Builder::enable_time` / `enable_all`; `#[tokio::main]` does
//! this by default) because the task uses `tokio::time::timeout_at` and
//! `tokio::time::sleep`. Tokio cannot report from a `Handle` whether timers
//! are enabled, so this is not checked up front: without them the task
//! panics on its first batch, and [`NatsBookChangePublisher::shutdown`]
//! returns [`NatsPublisherError::TaskPanicked`]. The listener itself never
//! panics; once the task is gone, events are counted in `dropped_events`.
//!
//! # Feature Gate
//!
//! This module is only available when the `nats` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! orderbook-rs = { version = "0.15", features = ["nats"] }
//! ```

use crate::orderbook::book_change_event::{PriceLevelChangedEvent, PriceLevelChangedListener};
use crate::orderbook::nats::{
    MAX_BATCH_WINDOW_MS, MAX_MIN_PUBLISH_INTERVAL_MS, NatsPublisherError,
};
use crate::orderbook::nats_common::{
    DrainGate, DropLog, LinkState, RetryPolicy, ShutdownState, add_metric, batch_deadline,
    checked_reserve, clamp_channel_capacity, clamp_duration_ms, clamp_max_batch_size,
    clamp_max_retries, counter_exhausted, drain_buffered, increment_metric, new_batch_buffer,
    new_jitter_seed, publish_with_backoff, shutdown_task, shutdown_task_with_deadline, store_slot,
    throttle_or_shutdown,
};
use pricelevel::Side;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

/// Name used in this publisher's log fields.
const PUBLISHER_NAME: &str = "book_change";

/// `Content-Type` of every published batch: batches are always JSON (see
/// the module docs).
const CONTENT_TYPE: &str = "application/json";

/// Default batch window in milliseconds. Events are accumulated for at most
/// this duration before being flushed to NATS.
const DEFAULT_BATCH_WINDOW_MS: u64 = 1;

/// Default maximum number of events per batch. When this limit is reached the
/// batch is flushed immediately, regardless of the time window.
const DEFAULT_MAX_BATCH_SIZE: usize = 100;

/// Default bounded-channel capacity. When the channel is full, new events are
/// dropped and `dropped_events` is incremented.
const DEFAULT_CHANNEL_CAPACITY: usize = 10_000;

/// Default maximum number of retry attempts for transient NATS publish failures.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default minimum interval in milliseconds between consecutive publish
/// operations. Set to 0 to disable throttling.
const DEFAULT_MIN_PUBLISH_INTERVAL_MS: u64 = 0;

/// A batched order book change payload published to NATS JetStream.
///
/// Each batch contains one or more [`BookChangeEntry`] values collected within
/// the configured batch window. Consumers use [`BookChangeBatch::sequence`]
/// (the publisher's per-batch counter) for batch-level ordering, and
/// [`BookChangeEntry::engine_seq`] for per-event gap detection across all
/// outbound streams of the source `OrderBook<T>`.
#[derive(Debug, Clone, Serialize)]
pub struct BookChangeBatch {
    /// The symbol this batch belongs to.
    pub symbol: String,

    /// Monotonically increasing **publisher-side** sequence number for this
    /// batch. Independent of [`BookChangeEntry::engine_seq`]: batches are
    /// minted by the NATS publisher when it flushes, while each entry's
    /// `engine_seq` was minted by the upstream `OrderBook<T>` at emission
    /// time.
    pub sequence: u64,

    /// Unix timestamp in milliseconds when the batch was flushed.
    pub timestamp_ms: u64,

    /// Number of individual change events in this batch.
    pub event_count: usize,

    /// The individual price level changes.
    pub changes: Vec<BookChangeEntry>,
}

/// A single price level change within a [`BookChangeBatch`].
#[derive(Debug, Clone, Serialize)]
pub struct BookChangeEntry {
    /// The order book side that changed.
    pub side: Side,

    /// The price level that changed.
    pub price: u128,

    /// The new visible quantity at this price level after the change.
    pub quantity: u64,

    /// Strictly monotonic global engine sequence number for this entry.
    /// Inherited from [`PriceLevelChangedEvent::engine_seq`] at conversion
    /// time. Independent of [`BookChangeBatch::sequence`] (which is the
    /// publisher's per-batch counter).
    pub engine_seq: u64,
}

impl From<PriceLevelChangedEvent> for BookChangeEntry {
    #[inline]
    fn from(event: PriceLevelChangedEvent) -> Self {
        Self {
            side: event.side,
            price: event.price,
            quantity: event.quantity,
            engine_seq: event.engine_seq,
        }
    }
}

/// A publisher that batches [`PriceLevelChangedEvent`]s and publishes them to
/// NATS JetStream.
///
/// The publisher wraps a JetStream context and provides a non-blocking
/// [`into_listener`](NatsBookChangePublisher::into_listener) method that returns
/// a [`PriceLevelChangedListener`] suitable for use with
/// [`OrderBook::set_price_level_listener`](crate::orderbook::OrderBook::set_price_level_listener).
///
/// # Batching
///
/// Events are collected in a bounded channel and flushed by a background task
/// when either the [`batch_window_ms`](NatsBookChangePublisher::with_batch_window_ms)
/// elapses or [`max_batch_size`](NatsBookChangePublisher::with_max_batch_size)
/// events have been collected.
///
/// # Throttling
///
/// An optional minimum publish interval prevents flooding on high-activity
/// books. When set, the publisher enforces at least
/// [`min_publish_interval_ms`](NatsBookChangePublisher::with_min_publish_interval_ms)
/// between consecutive NATS publishes.
///
/// # Metrics
///
/// The publisher tracks the following counters via atomic operations:
///
/// - **publish_count** — number of batches published successfully (every
///   subject acknowledged), counted once per batch
/// - **error_count** — number of batches that **failed** to publish, counted
///   once per batch whatever the number of failed subjects (see the
///   [module docs](self#error-accounting))
/// - **events_received** — total events received from the listener callback
/// - **batches_published** — total batches flushed to NATS
/// - **dropped_events** — events dropped because the channel was full, the
///   background task was no longer running, or shutdown gave up on
///   an unreachable NATS link
/// - **sequence** — monotonically increasing batch sequence number
///
/// # Example
///
/// ```rust,no_run
/// use orderbook_rs::orderbook::nats_book_change::NatsBookChangePublisher;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = async_nats::connect("nats://localhost:4222").await?;
/// let jetstream = async_nats::jetstream::new(client);
/// let handle = tokio::runtime::Handle::current();
///
/// let publisher = NatsBookChangePublisher::new(
///     jetstream,
///     "BTC/USD".to_string(),
///     "book".to_string(),
///     handle,
/// );
/// let (metrics, listener) = publisher.into_listener();
/// // Wire `listener` into OrderBook::set_price_level_listener()
/// // Read metrics via `metrics.publish_count()`, `metrics.dropped_events()`, etc.
/// # Ok(())
/// # }
/// ```
pub struct NatsBookChangePublisher {
    /// JetStream context for publishing messages.
    jetstream: async_nats::jetstream::Context,

    /// The order book symbol (e.g. `"BTC/USD"`).
    symbol: String,

    /// Subject prefix. Batches are published to `{prefix}.{symbol}.changes`,
    /// `{prefix}.{symbol}.bid`, and `{prefix}.{symbol}.ask`.
    subject_prefix: String,

    /// Handle to the Tokio runtime for spawning the background batch task.
    runtime: tokio::runtime::Handle,

    /// Batch window duration in milliseconds.
    batch_window_ms: u64,

    /// Maximum number of events per batch before an early flush.
    max_batch_size: usize,

    /// Bounded channel capacity for the event buffer.
    channel_capacity: usize,

    /// Minimum interval in milliseconds between consecutive publishes.
    min_publish_interval_ms: u64,

    /// Maximum retry attempts for transient NATS failures.
    max_retries: u32,

    /// Monotonically increasing batch sequence number.
    sequence: AtomicU64,

    /// Count of successfully published batches (once per batch).
    publish_count: AtomicU64,

    /// Count of batches that failed to publish (once per batch).
    error_count: AtomicU64,

    /// Total events received from the listener callback.
    events_received: AtomicU64,

    /// Total batches successfully flushed to NATS.
    batches_published: AtomicU64,

    /// Events dropped because the bounded channel was full or closed.
    dropped_events: AtomicU64,

    /// Per-publisher seed for retry backoff jitter.
    jitter_seed: u64,

    /// Connected / disconnected transition tracker for `INFO` logging.
    link: LinkState,

    /// Rate-limited logging of events the listener had to drop.
    drop_log: DropLog,

    /// Join handle for the single background batch task, populated by
    /// [`into_listener`](NatsBookChangePublisher::into_listener). Taken and
    /// awaited by [`shutdown`](NatsBookChangePublisher::shutdown) so teardown
    /// can join the task rather than leaving it detached.
    task_handle: Mutex<Option<JoinHandle<()>>>,

    /// One-shot signal that asks the background task to drain any buffered
    /// events, flush them, and exit. Sent by
    /// [`shutdown`](NatsBookChangePublisher::shutdown).
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,

    /// Shutdown intent and the give-up latch shared with the background
    /// task, so a flush already running when `shutdown()` is called stops
    /// retrying against an unreachable link (#295).
    shutdown_state: ShutdownState,
}

/// Sequence numbers assigned to one flushed batch, one per published
/// subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatchSequences {
    /// Sequence of the `{prefix}.{symbol}.changes` publish.
    changes: u64,
    /// Sequence of the `{prefix}.{symbol}.bid` publish, if the batch has
    /// bid-side changes.
    bid: Option<u64>,
    /// Sequence of the `{prefix}.{symbol}.ask` publish, if the batch has
    /// ask-side changes.
    ask: Option<u64>,
}

/// Reserves, in a single atomic checked update, one sequence number per
/// subject the batch publishes to (`changes`, plus `bid` / `ask` when
/// present), and assigns them in publish order.
///
/// Returns `None`, leaving `counter` unchanged, when the reservation would
/// overflow `u64`: the caller then refuses the whole batch instead of
/// emitting it on only some subjects.
#[must_use]
fn reserve_batch_sequences(
    counter: &AtomicU64,
    has_bid: bool,
    has_ask: bool,
) -> Option<BatchSequences> {
    let subjects = 1u64
        .checked_add(u64::from(has_bid))?
        .checked_add(u64::from(has_ask))?;
    let first = checked_reserve(counter, subjects)?;
    // The reservation covers `first..first + subjects`, so these additions
    // stay in range; they are still checked.
    let bid = if has_bid {
        Some(first.checked_add(1)?)
    } else {
        None
    };
    let ask = if has_ask {
        Some(first.checked_add(1)?.checked_add(u64::from(has_bid))?)
    } else {
        None
    };
    Some(BatchSequences {
        changes: first,
        bid,
        ask,
    })
}

impl NatsBookChangePublisher {
    /// Create a new NATS book change publisher.
    ///
    /// # Arguments
    ///
    /// * `jetstream` — JetStream context obtained from an `async_nats` client
    /// * `symbol` — the order book symbol (e.g. `"BTC/USD"`)
    /// * `subject_prefix` — prefix for NATS subjects (e.g. `"book"`)
    /// * `runtime` — handle to the Tokio runtime for spawning the batch task.
    ///   Its time driver must be enabled; see the
    ///   [module docs](self#runtime-requirements). The runtime is supplied
    ///   explicitly, so construction never looks up an ambient runtime.
    #[inline]
    pub fn new(
        jetstream: async_nats::jetstream::Context,
        symbol: String,
        subject_prefix: String,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            jetstream,
            symbol,
            subject_prefix,
            runtime,
            batch_window_ms: DEFAULT_BATCH_WINDOW_MS,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            min_publish_interval_ms: DEFAULT_MIN_PUBLISH_INTERVAL_MS,
            max_retries: DEFAULT_MAX_RETRIES,
            sequence: AtomicU64::new(0),
            publish_count: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            events_received: AtomicU64::new(0),
            batches_published: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            jitter_seed: new_jitter_seed(),
            link: LinkState::default(),
            drop_log: DropLog::default(),
            task_handle: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
            shutdown_state: ShutdownState::default(),
        }
    }

    /// Set the batch window duration in milliseconds.
    ///
    /// Events are accumulated for at most this duration before being flushed.
    /// Defaults to `DEFAULT_BATCH_WINDOW_MS` (1 ms).
    ///
    /// Values above [`MAX_BATCH_WINDOW_MS`] (60,000 ms) are **clamped** to it
    /// with a `tracing::warn!`; the builder never panics.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_batch_window_ms(mut self, batch_window_ms: u64) -> Self {
        self.batch_window_ms =
            clamp_duration_ms("batch_window_ms", batch_window_ms, MAX_BATCH_WINDOW_MS);
        self
    }

    /// Set the maximum number of events per batch.
    ///
    /// When the batch reaches this size it is flushed immediately, regardless
    /// of the time window. Defaults to `DEFAULT_MAX_BATCH_SIZE` (100).
    ///
    /// The value is **clamped** into `1..=`[`MAX_BATCH_SIZE`](crate::orderbook::nats::MAX_BATCH_SIZE) with a
    /// `tracing::warn!`: `0` becomes `1` (a zero batch size could not drain
    /// buffered events on shutdown) and larger values become the maximum.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = clamp_max_batch_size(max_batch_size);
        self
    }

    /// Set the bounded channel capacity.
    ///
    /// When the channel is full, new events are dropped and `dropped_events`
    /// is incremented. Defaults to `DEFAULT_CHANNEL_CAPACITY` (10,000).
    ///
    /// A `channel_capacity` of `0`, or one above [`MAX_CHANNEL_CAPACITY`](crate::orderbook::nats::MAX_CHANNEL_CAPACITY)
    /// (Tokio's semaphore limit), is invalid for a Tokio mpsc channel. Rather
    /// than panic on caller-supplied (possibly runtime-derived) input, it is
    /// **clamped** into `1..=`[`MAX_CHANNEL_CAPACITY`](crate::orderbook::nats::MAX_CHANNEL_CAPACITY) with a
    /// `tracing::warn!`; the builder never aborts the process.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.channel_capacity = clamp_channel_capacity(channel_capacity);
        self
    }

    /// Set the minimum interval in milliseconds between consecutive publishes.
    ///
    /// When set to a value greater than 0, the publisher will wait at least
    /// this long between consecutive NATS publish operations. Defaults to
    /// `DEFAULT_MIN_PUBLISH_INTERVAL_MS` (0, disabled).
    ///
    /// Values above [`MAX_MIN_PUBLISH_INTERVAL_MS`] (60,000 ms) are
    /// **clamped** to it with a `tracing::warn!`.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_min_publish_interval_ms(mut self, min_publish_interval_ms: u64) -> Self {
        self.min_publish_interval_ms = clamp_duration_ms(
            "min_publish_interval_ms",
            min_publish_interval_ms,
            MAX_MIN_PUBLISH_INTERVAL_MS,
        );
        self
    }

    /// Set the maximum number of retry attempts for transient NATS failures.
    ///
    /// Defaults to `DEFAULT_MAX_RETRIES` (3). Set to 0 to disable retries.
    /// Retry `n` (zero-based) waits a jittered delay in `[c / 2, c]` where
    /// `c = min(BASE_RETRY_DELAY_MS * 2^n, MAX_RETRY_DELAY_MS)`.
    ///
    /// Values above [`MAX_PUBLISH_RETRIES`](crate::orderbook::nats::MAX_PUBLISH_RETRIES) (10) are **clamped** to it with a
    /// `tracing::warn!`, so one failing publish (and `shutdown()`) stays
    /// bounded.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = clamp_max_retries(max_retries);
        self
    }

    /// Returns the number of successfully published batches (once per
    /// batch).
    #[must_use]
    #[inline]
    pub fn publish_count(&self) -> u64 {
        self.publish_count.load(Ordering::Relaxed)
    }

    /// Returns the number of batches that failed to publish (once per
    /// batch, however many of its subjects failed).
    #[must_use]
    #[inline]
    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    /// Returns the total number of events received from the listener callback.
    #[must_use]
    #[inline]
    pub fn events_received(&self) -> u64 {
        self.events_received.load(Ordering::Relaxed)
    }

    /// Returns the total number of batches successfully flushed to NATS.
    #[must_use]
    #[inline]
    pub fn batches_published(&self) -> u64 {
        self.batches_published.load(Ordering::Relaxed)
    }

    /// Returns the number of events dropped because the channel was full or
    /// the background task was no longer running.
    #[must_use]
    #[inline]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Returns the current batch sequence number (next value to be assigned).
    #[must_use]
    #[inline]
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }

    /// Convert this publisher into a [`PriceLevelChangedListener`] callback.
    ///
    /// This method consumes `self`, wraps it in an `Arc`, spawns a background
    /// batch task on the configured Tokio runtime, and returns both the `Arc`
    /// handle (for reading metrics) and the listener callback.
    ///
    /// The listener sends each [`PriceLevelChangedEvent`] into a bounded
    /// channel. The background task drains the channel, batches events, and
    /// publishes them to NATS JetStream.
    ///
    /// The runtime passed to [`new`](Self::new) must have its time driver
    /// enabled; see the [module docs](self#runtime-requirements). Call
    /// [`shutdown`](Self::shutdown) to stop the task and learn whether it
    /// failed.
    ///
    /// # Returns
    ///
    /// A tuple of `(Arc<NatsBookChangePublisher>, PriceLevelChangedListener)`.
    /// The `Arc` handle allows the caller to read metrics after wiring the
    /// listener into the order book.
    pub fn into_listener(self) -> (Arc<Self>, PriceLevelChangedListener) {
        let channel_capacity = self.channel_capacity;
        let publisher = Arc::new(self);
        let handle = Arc::clone(&publisher);

        let (tx, rx) = mpsc::channel::<PriceLevelChangedEvent>(channel_capacity);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Spawn the background batch task and retain its join handle so
        // `shutdown` can await it instead of leaving it detached.
        let batch_publisher = Arc::clone(&publisher);
        let join = publisher
            .runtime
            .spawn(Self::batch_task(batch_publisher, rx, shutdown_rx));
        store_slot(&publisher.task_handle, join);
        store_slot(&publisher.shutdown_tx, shutdown_tx);

        // Build the listener closure: non-blocking send only.
        let listener_publisher = Arc::clone(&publisher);
        let listener = Arc::new(move |event: PriceLevelChangedEvent| {
            increment_metric(&listener_publisher.events_received, "events_received");
            match tx.try_send(event) {
                Ok(()) => listener_publisher.drop_log.on_sent(),
                Err(err) => listener_publisher.drop_log.on_dropped(
                    &err,
                    &listener_publisher.dropped_events,
                    PUBLISHER_NAME,
                ),
            }
        });

        (handle, listener)
    }

    /// Gracefully shut down the background batch task.
    ///
    /// Signals the background task to stop accepting events, drain the ones
    /// still buffered in the channel, flush them to NATS (without the
    /// `min_publish_interval_ms` throttle), and exit, then awaits the task's
    /// join handle so teardown does not race in-flight publishes. The signal
    /// is observed both while idle and while a batch window is open.
    ///
    /// Note that the [`PriceLevelChangedListener`] closure still holds a channel
    /// sender, so shutdown does not rely on the listener being dropped first;
    /// the explicit signal is what unblocks the task. After shutdown, further
    /// events are dropped and counted in `dropped_events`.
    ///
    /// Safe to call more than once and from any task: only the call that
    /// joins the task reports its outcome; later calls (and a call racing
    /// the one joining) return `Ok(())` immediately.
    ///
    /// Cancel-safe: dropping the returned future (for example under
    /// `tokio::time::timeout`) does not detach the task; a later call joins
    /// it.
    ///
    /// # Bounded with NATS down
    ///
    /// Each publish retries at most [`MAX_PUBLISH_RETRIES`](crate::orderbook::nats::MAX_PUBLISH_RETRIES) times. Once
    /// shutdown is requested (in the drain, or in a flush that was already
    /// running when this was called), the first publish that exhausts its
    /// retries while the link is down ends publishing: the remaining
    /// buffered events are counted in `dropped_events` and the task exits. Use
    /// [`shutdown_with_deadline`](Self::shutdown_with_deadline) for a hard
    /// wall-clock bound.
    ///
    /// # Errors
    ///
    /// - [`NatsPublisherError::TaskPanicked`] if the background task panicked
    ///   (for example because the runtime has no time driver).
    /// - [`NatsPublisherError::TaskCancelled`] if the task was cancelled,
    ///   for example because its runtime shut down first.
    pub async fn shutdown(&self) -> Result<(), NatsPublisherError> {
        shutdown_task(
            &self.shutdown_state,
            &self.shutdown_tx,
            &self.task_handle,
            PUBLISHER_NAME,
        )
        .await
    }

    /// Like [`shutdown`](Self::shutdown), but waits at most `deadline` for
    /// the drain.
    ///
    /// If the task has not finished when the deadline expires it is aborted
    /// and joined, and the events it had not published yet are discarded
    /// **without** being counted in any metric. Cancel-safe like
    /// `shutdown`.
    ///
    /// # Errors
    ///
    /// - [`NatsPublisherError::ShutdownTimedOut`] if the deadline expired.
    /// - [`NatsPublisherError::TaskPanicked`] /
    ///   [`NatsPublisherError::TaskCancelled`] as for `shutdown`.
    pub async fn shutdown_with_deadline(
        &self,
        deadline: Duration,
    ) -> Result<(), NatsPublisherError> {
        shutdown_task_with_deadline(
            &self.shutdown_state,
            &self.shutdown_tx,
            &self.task_handle,
            PUBLISHER_NAME,
            deadline,
        )
        .await
    }

    /// Background task that drains the event channel, batches events, and
    /// publishes them to NATS.
    ///
    /// The task flushes when either:
    /// - The batch window timer elapses (configurable via `batch_window_ms`)
    /// - The batch reaches `max_batch_size` events
    ///
    /// When throttling is enabled (`min_publish_interval_ms > 0`), the task
    /// waits at least that duration between consecutive publishes.
    ///
    /// `shutdown_rx` is polled only until it completes: every branch that
    /// observes it returns, so the `oneshot::Receiver` is never polled again
    /// after completion.
    async fn batch_task(
        publisher: Arc<Self>,
        mut rx: mpsc::Receiver<PriceLevelChangedEvent>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        info!(
            publisher = PUBLISHER_NAME,
            symbol = %publisher.symbol,
            prefix = %publisher.subject_prefix,
            "NATS publisher task started"
        );
        let batch_window = Duration::from_millis(publisher.batch_window_ms);
        let min_interval = (publisher.min_publish_interval_ms > 0)
            .then(|| Duration::from_millis(publisher.min_publish_interval_ms));

        let mut batch: Vec<BookChangeEntry> = new_batch_buffer(publisher.max_batch_size);
        let mut last_publish = tokio::time::Instant::now();

        loop {
            // Wait for the first event, a channel close, or a shutdown signal.
            if batch.is_empty() {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        Self::drain_on_shutdown(&publisher, &mut rx, &mut batch).await;
                        return;
                    }
                    maybe = rx.recv() => match maybe {
                        Some(event) => batch.push(BookChangeEntry::from(event)),
                        None => break, // Channel closed
                    },
                }
            }

            // Collect more events within the batch window. A deadline that
            // overflows the clock (not reachable with the clamped window)
            // flushes immediately instead of panicking.
            if let Some(deadline) = batch_deadline(tokio::time::Instant::now(), batch_window) {
                while batch.len() < publisher.max_batch_size {
                    tokio::select! {
                        biased;
                        _ = &mut shutdown_rx => {
                            Self::drain_on_shutdown(&publisher, &mut rx, &mut batch).await;
                            return;
                        }
                        received = tokio::time::timeout_at(deadline, rx.recv()) => {
                            match received {
                                Ok(Some(event)) => batch.push(BookChangeEntry::from(event)),
                                Ok(None) => {
                                    // Channel closed — flush remaining and exit
                                    let mut gate = DrainGate::normal(&publisher.shutdown_state);
                                    Self::flush_batch(&publisher, &mut batch, &mut gate).await;
                                    return;
                                }
                                Err(_) => break, // Timeout — flush batch
                            }
                        }
                    }
                }
            } else {
                debug!(
                    publisher = PUBLISHER_NAME,
                    "batch window deadline overflows the clock; flushing immediately"
                );
            }

            Self::flush_batch(
                &publisher,
                &mut batch,
                &mut DrainGate::normal(&publisher.shutdown_state),
            )
            .await;

            // Throttle before the next flush, raced with the shutdown signal
            // so a long interval never delays teardown.
            if throttle_or_shutdown(&mut last_publish, min_interval, &mut shutdown_rx).await {
                Self::drain_on_shutdown(&publisher, &mut rx, &mut batch).await;
                return;
            }
        }

        // Flush any remaining events
        Self::flush_batch(
            &publisher,
            &mut batch,
            &mut DrainGate::normal(&publisher.shutdown_state),
        )
        .await;
    }

    /// Shutdown path: close the channel to new events, then flush the current
    /// batch plus everything already buffered in `max_batch_size` chunks, so
    /// no accepted event is lost. Closing first bounds the loop even while
    /// the listener keeps firing. The throttle is skipped so teardown is
    /// prompt. Once a publish exhausts its retries with the link down, the
    /// rest is counted in `dropped_events` instead of being published
    /// ([`DrainGate`]).
    async fn drain_on_shutdown(
        publisher: &Arc<Self>,
        rx: &mut mpsc::Receiver<PriceLevelChangedEvent>,
        batch: &mut Vec<BookChangeEntry>,
    ) {
        rx.close();
        let mut gate = DrainGate::draining(&publisher.shutdown_state);
        loop {
            drain_buffered(rx, batch, publisher.max_batch_size);
            if batch.is_empty() {
                break;
            }
            Self::flush_batch(publisher, batch, &mut gate).await;
        }
    }

    /// Flush the accumulated batch to NATS JetStream.
    ///
    /// Publishes to three subjects:
    /// - `{prefix}.{symbol}.changes` — all changes
    /// - `{prefix}.{symbol}.bid` — bid-side changes only
    /// - `{prefix}.{symbol}.ask` — ask-side changes only
    ///
    /// Side-specific subjects are only published if the batch contains events
    /// for that side. Every sequence number the batch needs is reserved in one
    /// atomic step before anything is published, so a batch is either emitted
    /// on all of its subjects or refused as a whole (never partially). The
    /// throttle is applied by the caller, raced with the shutdown signal.
    ///
    /// The outcome is counted once per batch (see the
    /// [module docs](self#error-accounting)). A tripped `gate` (shutdown
    /// drain, link down) publishes nothing more: the batch's events are
    /// counted in `dropped_events`, and a batch whose first subject tripped
    /// it skips its remaining subjects.
    async fn flush_batch(
        publisher: &Arc<Self>,
        batch: &mut Vec<BookChangeEntry>,
        gate: &mut DrainGate<'_>,
    ) {
        if batch.is_empty() {
            return;
        }
        if gate.is_tripped() {
            add_metric(&publisher.dropped_events, batch.len(), "dropped_events");
            batch.clear();
            return;
        }

        let changes: Vec<BookChangeEntry> = std::mem::take(batch);
        let has_bid = changes.iter().any(|c| c.side == Side::Buy);
        let has_ask = changes.iter().any(|c| c.side == Side::Sell);

        let Some(BatchSequences {
            changes: seq,
            bid: bid_seq,
            ask: ask_seq,
        }) = reserve_batch_sequences(&publisher.sequence, has_bid, has_ask)
        else {
            // Not enough unique sequences left for every subject of this
            // batch: refuse the whole batch rather than wrap or emit it on
            // only some subjects.
            counter_exhausted("sequence");
            increment_metric(&publisher.error_count, "error_count");
            return;
        };
        let timestamp_ms = crate::utils::current_time_millis();

        let all_batch = BookChangeBatch {
            symbol: publisher.symbol.clone(),
            sequence: seq,
            timestamp_ms,
            event_count: changes.len(),
            changes: changes.clone(),
        };

        // Publish the aggregate changes subject
        let changes_subject = format!("{}.{}.changes", publisher.subject_prefix, publisher.symbol);
        let all_ok = Self::publish_batch(publisher, &changes_subject, &all_batch, seq, gate).await;

        // Publish bid-side subject if there are bid changes
        let bid_ok = match bid_seq {
            Some(_) if gate.is_tripped() => false,
            Some(bid_seq) => {
                let bid_changes: Vec<BookChangeEntry> = changes
                    .iter()
                    .filter(|c| c.side == Side::Buy)
                    .cloned()
                    .collect();
                let bid_batch = BookChangeBatch {
                    symbol: publisher.symbol.clone(),
                    sequence: bid_seq,
                    timestamp_ms,
                    event_count: bid_changes.len(),
                    changes: bid_changes,
                };
                let bid_subject = format!("{}.{}.bid", publisher.subject_prefix, publisher.symbol);
                Self::publish_batch(publisher, &bid_subject, &bid_batch, bid_seq, gate).await
            }
            None => true,
        };

        // Publish ask-side subject if there are ask changes
        let ask_ok = match ask_seq {
            Some(_) if gate.is_tripped() => false,
            Some(ask_seq) => {
                let ask_changes: Vec<BookChangeEntry> = changes
                    .iter()
                    .filter(|c| c.side == Side::Sell)
                    .cloned()
                    .collect();
                let ask_batch = BookChangeBatch {
                    symbol: publisher.symbol.clone(),
                    sequence: ask_seq,
                    timestamp_ms,
                    event_count: ask_changes.len(),
                    changes: ask_changes,
                };
                let ask_subject = format!("{}.{}.ask", publisher.subject_prefix, publisher.symbol);
                Self::publish_batch(publisher, &ask_subject, &ask_batch, ask_seq, gate).await
            }
            None => true,
        };

        if all_ok && bid_ok && ask_ok {
            increment_metric(&publisher.publish_count, "publish_count");
            increment_metric(&publisher.batches_published, "batches_published");
            trace!(seq, symbol = %publisher.symbol, "book change batch published to NATS");
        } else {
            // Once per batch, however many subjects failed.
            increment_metric(&publisher.error_count, "error_count");
        }
    }

    /// Serialize and publish a single batch to a NATS subject with retry logic.
    ///
    /// Returns `true` if the publish succeeded, `false` if serialization
    /// failed or all retries were exhausted (which may trip `gate`). Counting
    /// is left to [`Self::flush_batch`], once per batch.
    async fn publish_batch(
        publisher: &Arc<Self>,
        subject: &str,
        batch: &BookChangeBatch,
        seq: u64,
        gate: &mut DrainGate<'_>,
    ) -> bool {
        let payload = match serde_json::to_vec(batch) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!(error = %e, "failed to serialize book change batch for NATS");
                return false;
            }
        };

        let payload_bytes: bytes::Bytes = payload.into();

        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Sequence", seq.to_string().as_str());
        headers.insert("Content-Type", CONTENT_TYPE);

        let policy = RetryPolicy {
            max_retries: publisher.max_retries,
            jitter_seed: publisher.jitter_seed,
        };
        let published = publish_with_backoff(
            &publisher.jetstream,
            &publisher.link,
            policy,
            subject,
            payload_bytes,
            headers,
            seq,
        )
        .await;
        if !published {
            gate.on_publish_exhausted(&publisher.link, PUBLISHER_NAME);
        }
        published
    }
}

impl std::fmt::Debug for NatsBookChangePublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsBookChangePublisher")
            .field("symbol", &self.symbol)
            .field("subject_prefix", &self.subject_prefix)
            .field("batch_window_ms", &self.batch_window_ms)
            .field("max_batch_size", &self.max_batch_size)
            .field("channel_capacity", &self.channel_capacity)
            .field("min_publish_interval_ms", &self.min_publish_interval_ms)
            .field("max_retries", &self.max_retries)
            .field("sequence", &self.sequence.load(Ordering::Relaxed))
            .field("publish_count", &self.publish_count.load(Ordering::Relaxed))
            .field("error_count", &self.error_count.load(Ordering::Relaxed))
            .field(
                "events_received",
                &self.events_received.load(Ordering::Relaxed),
            )
            .field(
                "batches_published",
                &self.batches_published.load(Ordering::Relaxed),
            )
            .field(
                "dropped_events",
                &self.dropped_events.load(Ordering::Relaxed),
            )
            .finish()
    }
}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::orderbook::nats::{MAX_BATCH_SIZE, MAX_CHANNEL_CAPACITY};

    /// A JetStream context backed by a client that never reaches a server
    /// (`retry_on_initial_connect` connects in the background), with a short
    /// ack timeout so a publish attempt fails fast instead of waiting 5 s.
    async fn offline_jetstream() -> async_nats::jetstream::Context {
        let client = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect("nats://127.0.0.1:1")
            .await
            .expect("background connect never fails up front");
        let mut context = async_nats::jetstream::new(client);
        context.set_timeout(Duration::from_millis(20));
        context
    }

    fn event(side: Side, engine_seq: u64) -> PriceLevelChangedEvent {
        PriceLevelChangedEvent {
            side,
            price: 50_000,
            quantity: 10,
            engine_seq,
        }
    }

    async fn publisher() -> NatsBookChangePublisher {
        NatsBookChangePublisher::new(
            offline_jetstream().await,
            "BTC/USD".to_string(),
            "book".to_string(),
            tokio::runtime::Handle::current(),
        )
    }

    #[tokio::test]
    async fn test_builder_extreme_values_are_clamped_without_panicking() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(u64::MAX)
            .with_max_batch_size(usize::MAX)
            .with_channel_capacity(usize::MAX)
            .with_min_publish_interval_ms(u64::MAX)
            .with_max_retries(u32::MAX);
        assert_eq!(publisher.batch_window_ms, MAX_BATCH_WINDOW_MS);
        assert_eq!(
            publisher.max_retries,
            crate::orderbook::nats::MAX_PUBLISH_RETRIES
        );
        assert_eq!(publisher.max_batch_size, MAX_BATCH_SIZE);
        assert_eq!(publisher.channel_capacity, MAX_CHANNEL_CAPACITY);
        assert_eq!(
            publisher.min_publish_interval_ms,
            MAX_MIN_PUBLISH_INTERVAL_MS
        );

        let (handle, _listener) = publisher.into_listener();
        assert_eq!(handle.shutdown().await, Ok(()));
    }

    #[tokio::test]
    async fn test_builder_zero_values_are_clamped_and_shutdown_drains() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(0)
            .with_max_batch_size(0)
            .with_channel_capacity(0)
            .with_min_publish_interval_ms(0)
            .with_max_retries(0);
        assert_eq!(publisher.max_batch_size, 1);
        assert_eq!(publisher.channel_capacity, 1);

        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        assert_eq!(handle.events_received(), 1);
        // With max_batch_size clamped to 1 the buffered event reaches a flush
        // (two publishes: `changes` and `bid`) instead of being dropped.
        assert_eq!(handle.sequence(), 2, "one batch minted two sequences");
        assert_eq!(handle.publish_count(), 0, "no server to acknowledge");
        // #295: counted once per batch; the drain gave up on the link after
        // the `changes` subject, so `bid` was not attempted.
        assert_eq!(handle.error_count(), 1, "one failed batch, counted once");
    }

    #[tokio::test]
    async fn test_shutdown_flushes_buffered_events_with_huge_window() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(u64::MAX)
            .with_max_retries(1);
        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        listener(event(Side::Sell, 2));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // The 60 s (clamped) window must not delay shutdown.
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        assert_eq!(handle.sequence(), 3, "changes + bid + ask");
        assert_eq!(handle.error_count(), 1, "one failed batch, counted once");
        assert_eq!(handle.dropped_events(), 0);

        // After shutdown the listener drops (and counts) events, never panics.
        listener(event(Side::Buy, 3));
        assert_eq!(handle.dropped_events(), 1);
    }

    #[test]
    fn test_reserve_batch_sequences_assigns_consecutive_numbers() {
        let counter = AtomicU64::new(10);
        assert_eq!(
            reserve_batch_sequences(&counter, true, true),
            Some(BatchSequences {
                changes: 10,
                bid: Some(11),
                ask: Some(12)
            })
        );
        assert_eq!(counter.load(Ordering::Relaxed), 13);
        assert_eq!(
            reserve_batch_sequences(&counter, false, true),
            Some(BatchSequences {
                changes: 13,
                bid: None,
                ask: Some(14)
            })
        );
        assert_eq!(
            reserve_batch_sequences(&counter, true, false),
            Some(BatchSequences {
                changes: 15,
                bid: Some(16),
                ask: None
            })
        );
        assert_eq!(counter.load(Ordering::Relaxed), 17);
    }

    #[test]
    fn test_reserve_batch_sequences_near_max_is_all_or_nothing() {
        // Three subjects need three sequences: with only two left the whole
        // batch is refused and the counter is untouched.
        let counter = AtomicU64::new(u64::MAX - 2);
        assert_eq!(reserve_batch_sequences(&counter, true, true), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX - 2);

        // A one-sided batch needs two and still fits exactly.
        assert_eq!(
            reserve_batch_sequences(&counter, true, false),
            Some(BatchSequences {
                changes: u64::MAX - 2,
                bid: Some(u64::MAX - 1),
                ask: None
            })
        );
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(reserve_batch_sequences(&counter, false, false), None);
    }

    #[tokio::test]
    async fn test_exhausted_sequence_refuses_whole_batch() {
        let publisher = publisher().await.with_max_retries(0);
        publisher.sequence.store(u64::MAX - 2, Ordering::Relaxed);
        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        listener(event(Side::Sell, 2));
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        // Either one two-sided batch (refused whole) or two one-sided batches
        // (the first fits exactly, the second is refused). Never a partial
        // emission that leaves the counter between the two.
        let sequence = handle.sequence();
        assert!(
            sequence == u64::MAX - 2 || sequence == u64::MAX,
            "unexpected sequence {sequence}"
        );
        assert_eq!(handle.publish_count(), 0);
    }

    #[tokio::test]
    async fn test_shutdown_during_throttle_completes_promptly() {
        let publisher = publisher()
            .await
            .with_min_publish_interval_ms(u64::MAX)
            .with_max_retries(0);
        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        // Let the task flush the first batch (it fails fast against the
        // offline client) and enter the throttle wait.
        let flushed = tokio::time::timeout(Duration::from_secs(5), async {
            while handle.error_count() < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(flushed.is_ok(), "first batch flushed");
        listener(event(Side::Sell, 2));
        let joined = tokio::time::timeout(Duration::from_secs(5), handle.shutdown()).await;
        assert_eq!(
            joined,
            Ok(Ok(())),
            "shutdown must not wait out the throttle"
        );
        assert_eq!(handle.sequence(), 4, "both batches were flushed");
        assert_eq!(handle.error_count(), 2, "one per failed batch");
        assert_eq!(handle.dropped_events(), 0);
    }

    /// #295: with NATS unreachable, the shutdown drain gives up after the
    /// first publish that exhausts its retries, counts the rest as dropped
    /// and completes promptly.
    #[tokio::test]
    async fn test_shutdown_with_link_down_is_bounded_and_counts_dropped() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(u64::MAX)
            .with_max_batch_size(10)
            .with_max_retries(2);
        let (handle, listener) = publisher.into_listener();
        for seq in 0..50 {
            listener(event(Side::Buy, seq));
        }
        let started = std::time::Instant::now();
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(handle.events_received(), 50);
        assert_eq!(handle.error_count(), 1, "the one batch that was attempted");
        assert_eq!(handle.dropped_events(), 40, "the other four batches");
        assert_eq!(handle.publish_count(), 0);
    }

    /// #295 (PR #296 review): shutdown requested while a normal flush is
    /// already publishing against an unreachable link trips the shared
    /// latch, so the drain that follows does not retry the next batch.
    #[tokio::test]
    async fn test_shutdown_during_normal_flush_with_link_down_is_bounded() {
        let publisher = publisher().await.with_max_batch_size(5).with_max_retries(2);
        let (handle, listener) = publisher.into_listener();
        for seq in 0..10 {
            listener(event(Side::Buy, seq));
        }
        // Wait until the first (normal) flush reserved its sequences.
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            while handle.sequence() == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(started.is_ok(), "the normal flush started publishing");
        let begun = std::time::Instant::now();
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        assert!(begun.elapsed() < Duration::from_secs(10));
        assert_eq!(handle.error_count(), 1, "only the in-flight batch failed");
        assert_eq!(handle.dropped_events(), 5, "the second batch was dropped");
        assert_eq!(handle.sequence(), 2, "the second batch reserved nothing");
    }

    /// #295: `shutdown_with_deadline` returns a typed timeout and aborts
    /// the task when the drain outlives the deadline.
    #[tokio::test]
    async fn test_shutdown_with_deadline_times_out_and_aborts() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(u64::MAX)
            .with_max_retries(u32::MAX);
        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        let started = std::time::Instant::now();
        assert_eq!(
            handle
                .shutdown_with_deadline(Duration::from_millis(100))
                .await,
            Err(NatsPublisherError::ShutdownTimedOut { timeout_ms: 100 })
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // The task was joined: nothing is left to shut down.
        assert_eq!(handle.shutdown().await, Ok(()));
    }

    /// #295: dropping a `shutdown()` future mid-drain does not detach the
    /// task; a later `shutdown()` joins it.
    #[tokio::test]
    async fn test_cancelled_shutdown_future_then_shutdown_joins_the_task() {
        let publisher = publisher()
            .await
            .with_batch_window_ms(u64::MAX)
            .with_max_retries(3);
        let (handle, listener) = publisher.into_listener();
        listener(event(Side::Buy, 1));
        let first = tokio::time::timeout(Duration::from_millis(5), handle.shutdown()).await;
        assert!(
            first.is_err(),
            "the drain outlives 5 ms (four failed attempts)"
        );
        assert_eq!(handle.error_count(), 0, "still draining");
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())));
        assert_eq!(
            handle.error_count(),
            1,
            "the second shutdown waited for the drain to finish"
        );
    }

    #[tokio::test]
    async fn test_shutdown_ok_when_idle_and_idempotent() {
        let (handle, _listener) = publisher().await.into_listener();
        assert_eq!(handle.shutdown().await, Ok(()));
        assert_eq!(handle.shutdown().await, Ok(()));
    }

    #[test]
    fn test_book_change_entry_from_event() {
        let event = PriceLevelChangedEvent {
            side: Side::Buy,
            price: 50_000,
            quantity: 100,
            engine_seq: 7,
        };
        let entry = BookChangeEntry::from(event);
        assert_eq!(entry.side, Side::Buy);
        assert_eq!(entry.price, 50_000);
        assert_eq!(entry.quantity, 100);
        assert_eq!(
            entry.engine_seq, 7,
            "BookChangeEntry must propagate engine_seq from the source event"
        );
    }

    #[test]
    fn test_book_change_entry_serializes_to_json() {
        let entry = BookChangeEntry {
            side: Side::Buy,
            price: 50_000,
            quantity: 100,
            engine_seq: 11,
        };
        let result = serde_json::to_value(&entry);
        assert!(result.is_ok());
        let value = result.unwrap_or(serde_json::Value::Null);
        assert_eq!(value.get("price").and_then(|v| v.as_u64()), Some(50_000));
        assert_eq!(value.get("quantity").and_then(|v| v.as_u64()), Some(100));
        assert_eq!(value.get("engine_seq").and_then(|v| v.as_u64()), Some(11));
        assert!(value.get("side").is_some());
    }

    #[test]
    fn test_book_change_batch_serializes_to_json() {
        let batch = BookChangeBatch {
            symbol: "BTC/USD".to_string(),
            sequence: 42,
            timestamp_ms: 1_700_000_000_000,
            event_count: 2,
            changes: vec![
                BookChangeEntry {
                    side: Side::Buy,
                    price: 50_000,
                    quantity: 100,
                    engine_seq: 1,
                },
                BookChangeEntry {
                    side: Side::Sell,
                    price: 50_100,
                    quantity: 200,
                    engine_seq: 2,
                },
            ],
        };
        let result = serde_json::to_vec(&batch);
        assert!(result.is_ok());
        let bytes = result.unwrap_or_default();
        assert!(!bytes.is_empty());

        let json_str = String::from_utf8(bytes).unwrap_or_default();
        assert!(json_str.contains("BTC/USD"));
        assert!(json_str.contains("\"sequence\":42"));
        assert!(json_str.contains("\"event_count\":2"));
    }

    #[test]
    fn test_book_change_batch_roundtrip_fields() {
        let batch = BookChangeBatch {
            symbol: "ETH/USDT".to_string(),
            sequence: 7,
            timestamp_ms: 1_700_000_000_000,
            event_count: 1,
            changes: vec![BookChangeEntry {
                side: Side::Sell,
                price: 2_000,
                quantity: 50,
                engine_seq: 3,
            }],
        };
        let json = serde_json::to_value(&batch);
        assert!(json.is_ok());
        let value = json.unwrap_or(serde_json::Value::Null);
        assert_eq!(
            value.get("symbol").and_then(|v| v.as_str()),
            Some("ETH/USDT")
        );
        assert_eq!(value.get("sequence").and_then(|v| v.as_u64()), Some(7));
        assert_eq!(value.get("event_count").and_then(|v| v.as_u64()), Some(1));
        let changes = value.get("changes").and_then(|v| v.as_array());
        assert!(changes.is_some());
        assert_eq!(changes.map(|c| c.len()), Some(1));
    }

    #[test]
    fn test_subject_formatting_changes() {
        let prefix = "book";
        let symbol = "BTC/USD";
        let changes_subject = format!("{prefix}.{symbol}.changes");
        let bid_subject = format!("{prefix}.{symbol}.bid");
        let ask_subject = format!("{prefix}.{symbol}.ask");

        assert_eq!(changes_subject, "book.BTC/USD.changes");
        assert_eq!(bid_subject, "book.BTC/USD.bid");
        assert_eq!(ask_subject, "book.BTC/USD.ask");
    }

    #[test]
    fn test_subject_formatting_with_custom_prefix() {
        let prefix = "orderbook.events";
        let symbol = "ETH-PERP";
        let changes_subject = format!("{prefix}.{symbol}.changes");
        let bid_subject = format!("{prefix}.{symbol}.bid");
        let ask_subject = format!("{prefix}.{symbol}.ask");

        assert_eq!(changes_subject, "orderbook.events.ETH-PERP.changes");
        assert_eq!(bid_subject, "orderbook.events.ETH-PERP.bid");
        assert_eq!(ask_subject, "orderbook.events.ETH-PERP.ask");
    }

    #[test]
    fn test_default_constants() {
        assert_eq!(DEFAULT_BATCH_WINDOW_MS, 1);
        assert_eq!(DEFAULT_MAX_BATCH_SIZE, 100);
        assert_eq!(DEFAULT_CHANNEL_CAPACITY, 10_000);
        assert_eq!(DEFAULT_MAX_RETRIES, 3);
        assert_eq!(crate::orderbook::nats::BASE_RETRY_DELAY_MS, 10);
        assert_eq!(DEFAULT_MIN_PUBLISH_INTERVAL_MS, 0);
    }

    #[test]
    fn test_empty_batch_serializes() {
        let batch = BookChangeBatch {
            symbol: "BTC/USD".to_string(),
            sequence: 0,
            timestamp_ms: 0,
            event_count: 0,
            changes: vec![],
        };
        let result = serde_json::to_vec(&batch);
        assert!(result.is_ok());
        let json_str = String::from_utf8(result.unwrap_or_default()).unwrap_or_default();
        assert!(json_str.contains("\"event_count\":0"));
        assert!(json_str.contains("\"changes\":[]"));
    }

    #[test]
    fn test_price_level_changed_event_serializes() {
        let event = PriceLevelChangedEvent {
            side: Side::Buy,
            price: 42_000,
            quantity: 500,
            engine_seq: 0,
        };
        let result = serde_json::to_value(&event);
        assert!(result.is_ok());
        let value = result.unwrap_or(serde_json::Value::Null);
        assert_eq!(value.get("price").and_then(|v| v.as_u64()), Some(42_000));
        assert_eq!(value.get("quantity").and_then(|v| v.as_u64()), Some(500));
    }

    #[test]
    fn test_nats_publish_error_display() {
        let err = crate::orderbook::OrderBookError::NatsPublishError {
            message: "timeout".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("nats publish error"));
        assert!(display.contains("timeout"));
    }

    #[test]
    fn test_nats_serialization_error_display() {
        let err = crate::orderbook::OrderBookError::NatsSerializationError {
            message: "invalid data".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("nats serialization error"));
        assert!(display.contains("invalid data"));
    }
}
