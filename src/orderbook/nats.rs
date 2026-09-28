//! NATS JetStream trade event publisher.
//!
//! This module provides [`NatsTradePublisher`], which converts trade events
//! from the order book's [`TradeListener`] callback into NATS JetStream
//! messages. Each trade is published to two subjects:
//!
//! - `{prefix}.{symbol}` — per-symbol stream
//! - `{prefix}.all` — aggregate stream
//!
//! The listener callback is non-blocking on the matching hot path: it clones
//! the [`TradeResult`] into a bounded channel and returns immediately — no
//! serialization, no `format!`, and no per-trade task spawn happen on the
//! engine thread. A single background Tokio task drains the channel, batches
//! and (optionally) throttles, and performs the serialization, subject
//! construction, and JetStream publish with capped exponential backoff and
//! jitter between retries (see
//! [`BASE_RETRY_DELAY_MS`](crate::orderbook::nats::BASE_RETRY_DELAY_MS) and
//! [`MAX_RETRY_DELAY_MS`](crate::orderbook::nats::MAX_RETRY_DELAY_MS)). This
//! mirrors the sibling [`NatsBookChangePublisher`](crate::orderbook::nats_book_change::NatsBookChangePublisher)
//! so neither outbound path floods the runtime with tiny per-event tasks under
//! a burst.
//!
//! # Runtime requirements
//!
//! The background task runs on the Tokio runtime handle passed to
//! [`NatsTradePublisher::new`]. That runtime must have its **time driver
//! enabled** (`Builder::enable_time` / `enable_all`; `#[tokio::main]` does
//! this by default) because the task uses `tokio::time::timeout_at` and
//! `tokio::time::sleep`. Tokio cannot report from a `Handle` whether timers
//! are enabled, so this is not checked up front: without them the task
//! panics on its first batch, and [`NatsTradePublisher::shutdown`] returns
//! [`NatsPublisherError::TaskPanicked`]. The listener itself never panics;
//! once the task is gone, events are counted in `dropped_events`.
//!
//! # Feature Gate
//!
//! This module is only available when the `nats` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! orderbook-rs = { version = "0.6", features = ["nats"] }
//! ```

use crate::orderbook::nats_common::{
    DropLog, LinkState, RetryPolicy, batch_deadline, checked_reserve, clamp_channel_capacity,
    clamp_duration_ms, clamp_max_batch_size, counter_exhausted, drain_buffered, increment_metric,
    new_batch_buffer, new_jitter_seed, publish_with_backoff, shutdown_task, store_slot,
    throttle_or_shutdown,
};
use crate::orderbook::serialization::{EventSerializer, JsonEventSerializer};
use crate::orderbook::trade::{TradeListener, TradeResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

pub use crate::orderbook::nats_common::{
    BASE_RETRY_DELAY_MS, MAX_BATCH_SIZE, MAX_BATCH_WINDOW_MS, MAX_CHANNEL_CAPACITY,
    MAX_MIN_PUBLISH_INTERVAL_MS, MAX_RETRY_DELAY_MS, NatsPublisherError,
};

/// Name used in this publisher's log fields.
const PUBLISHER_NAME: &str = "trade";

/// Records the outcome of publishing one trade to its two subjects, using a
/// single **per-trade** granularity shared with `publish_count`.
///
/// Increments `publish_count` once on a clean success (both subjects ok) or
/// `error_count` once otherwise — so a partial failure (one subject ok, the
/// other exhausted) counts as exactly one failed trade, never as two and never
/// as none. Returns `true` on success. With this rule
/// `publish_count + error_count` always equals the number of trades that reached
/// the publish step.
fn account_publish_outcome(
    publish_count: &AtomicU64,
    error_count: &AtomicU64,
    symbol_ok: bool,
    all_ok: bool,
) -> bool {
    if symbol_ok && all_ok {
        increment_metric(publish_count, "publish_count");
        true
    } else {
        increment_metric(error_count, "error_count");
        false
    }
}

/// Default batch window in milliseconds. Trades are drained from the channel
/// for at most this duration before the accumulated batch is published.
const DEFAULT_BATCH_WINDOW_MS: u64 = 1;

/// Default maximum number of trades drained per batch. When this limit is
/// reached the batch is flushed immediately, regardless of the time window.
const DEFAULT_MAX_BATCH_SIZE: usize = 100;

/// Default bounded-channel capacity. When the channel is full, new trades are
/// dropped and `dropped_events` is incremented.
const DEFAULT_CHANNEL_CAPACITY: usize = 10_000;

/// Default minimum interval in milliseconds between consecutive flushes. Set to
/// 0 to disable throttling.
const DEFAULT_MIN_PUBLISH_INTERVAL_MS: u64 = 0;

/// Default maximum number of retry attempts for transient NATS publish failures.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// A trade event publisher that sends [`TradeResult`] events to NATS JetStream.
///
/// The publisher wraps a JetStream context and provides a non-blocking
/// [`into_listener`](NatsTradePublisher::into_listener) method that returns a
/// [`TradeListener`] suitable for use with [`OrderBook::trade_listener`].
///
/// # Batching and throttling
///
/// The listener callback pushes each trade into a bounded channel and returns
/// immediately. A single background task drains the channel, accumulating
/// trades until either the
/// [`batch_window_ms`](NatsTradePublisher::with_batch_window_ms) elapses or
/// [`max_batch_size`](NatsTradePublisher::with_max_batch_size) trades have been
/// collected, then publishes them. An optional
/// [`min_publish_interval_ms`](NatsTradePublisher::with_min_publish_interval_ms)
/// throttles consecutive flushes on a high-activity book.
///
/// # Metrics
///
/// The publisher tracks the following counters via atomic operations:
///
/// - **publish_count** — number of trades published successfully (counted once
///   per trade, when both its symbol and aggregate publishes succeed)
/// - **error_count** — number of trades that **failed** to publish, counted once
///   per trade (a serialization failure, or one or both subjects exhausting
///   their retries). Same per-trade granularity as `publish_count`, so
///   `publish_count + error_count` equals the number of trades processed by the
///   background task and a partial failure is attributable to exactly one trade.
/// - **events_received** — total trades received from the listener callback
/// - **batches_published** — total drain/flush cycles performed
/// - **dropped_events** — trades dropped because the channel was full or the
///   background task was no longer running
/// - **sequence** — monotonically increasing sequence number; each publish
///   (symbol-specific and aggregate) receives its own unique value
///
/// # Example
///
/// ```rust,no_run
/// use orderbook_rs::orderbook::nats::NatsTradePublisher;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = async_nats::connect("nats://localhost:4222").await?;
/// let jetstream = async_nats::jetstream::new(client);
/// let handle = tokio::runtime::Handle::current();
///
/// let publisher = NatsTradePublisher::new(jetstream, "trades".to_string(), handle);
/// let (handle, listener) = publisher.into_listener();
/// // Use `listener` as the OrderBook's trade_listener
/// // Use `handle` to read metrics: handle.publish_count(), handle.error_count()
/// # Ok(())
/// # }
/// ```
pub struct NatsTradePublisher {
    /// JetStream context for publishing messages.
    jetstream: async_nats::jetstream::Context,

    /// Subject prefix. Messages are published to `{prefix}.{symbol}` and
    /// `{prefix}.all`.
    subject_prefix: String,

    /// The `{prefix}.all` aggregate subject, precomputed once at construction
    /// so the publish path never rebuilds it.
    all_subject: String,

    /// Handle to the Tokio runtime used for spawning the background batch task.
    runtime: tokio::runtime::Handle,

    /// Batch window duration in milliseconds.
    batch_window_ms: u64,

    /// Maximum number of trades per batch before an early flush.
    max_batch_size: usize,

    /// Bounded channel capacity for the trade buffer.
    channel_capacity: usize,

    /// Minimum interval in milliseconds between consecutive flushes.
    min_publish_interval_ms: u64,

    /// Maximum number of retry attempts for transient failures.
    max_retries: u32,

    /// Monotonically increasing sequence number embedded in each published
    /// message as a NATS header. Written exclusively by the single background
    /// `publish_task`; the `Relaxed` ordering on its `fetch_add` is correct
    /// only because no other writer exists (the `into_listener(self)` consuming
    /// signature spawns exactly one task per publisher).
    sequence: AtomicU64,

    /// Count of trades published successfully — incremented once per trade when
    /// both its symbol and aggregate subjects succeed.
    publish_count: AtomicU64,

    /// Count of trades that failed to publish — incremented once per trade (a
    /// serialize failure, or one or both subjects exhausting retries). Shares the
    /// per-trade granularity of `publish_count`.
    error_count: AtomicU64,

    /// Total trades received from the listener callback.
    events_received: AtomicU64,

    /// Total drain/flush cycles performed.
    batches_published: AtomicU64,

    /// Trades dropped because the bounded channel was full.
    dropped_events: AtomicU64,

    /// Per-publisher seed for retry backoff jitter.
    jitter_seed: u64,

    /// Connected / disconnected transition tracker for `INFO` logging.
    link: LinkState,

    /// Rate-limited logging of events the listener had to drop.
    drop_log: DropLog,

    /// Pluggable event serializer. Defaults to [`JsonEventSerializer`] for
    /// backward compatibility. Can be overridden via
    /// [`with_serializer`](NatsTradePublisher::with_serializer).
    serializer: Arc<dyn EventSerializer>,

    /// Join handle for the single background batch task, populated by
    /// [`into_listener`](NatsTradePublisher::into_listener). Taken and awaited
    /// by [`shutdown`](NatsTradePublisher::shutdown) so teardown can join the
    /// task rather than leaving it detached.
    task_handle: Mutex<Option<JoinHandle<()>>>,

    /// One-shot signal that asks the background task to drain any buffered
    /// trades, flush them, and exit. Sent by
    /// [`shutdown`](NatsTradePublisher::shutdown).
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl NatsTradePublisher {
    /// Create a new NATS trade publisher.
    ///
    /// # Arguments
    ///
    /// * `jetstream` — JetStream context obtained from an `async_nats` client
    /// * `subject_prefix` — prefix for NATS subjects (e.g. `"trades"`)
    /// * `runtime` — handle to the Tokio runtime for spawning the batch task.
    ///   Its time driver must be enabled; see the
    ///   [module docs](self#runtime-requirements). The runtime is supplied
    ///   explicitly, so construction never looks up an ambient runtime.
    #[inline]
    pub fn new(
        jetstream: async_nats::jetstream::Context,
        subject_prefix: String,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let all_subject = format!("{subject_prefix}.all");
        Self {
            jetstream,
            subject_prefix,
            all_subject,
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
            serializer: Arc::new(JsonEventSerializer),
            task_handle: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
        }
    }

    /// Set the batch window duration in milliseconds.
    ///
    /// Trades are accumulated for at most this duration before being flushed.
    /// Defaults to [`DEFAULT_BATCH_WINDOW_MS`] (1 ms).
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

    /// Set the maximum number of trades per batch.
    ///
    /// When the batch reaches this size it is flushed immediately, regardless
    /// of the time window. Defaults to [`DEFAULT_MAX_BATCH_SIZE`] (100).
    ///
    /// The value is **clamped** into `1..=`[`MAX_BATCH_SIZE`] with a
    /// `tracing::warn!`: `0` becomes `1` (a zero batch size could not drain
    /// buffered trades on shutdown) and larger values become the maximum.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_max_batch_size(mut self, max_batch_size: usize) -> Self {
        self.max_batch_size = clamp_max_batch_size(max_batch_size);
        self
    }

    /// Set the bounded channel capacity.
    ///
    /// When the channel is full, new trades are dropped and `dropped_events`
    /// is incremented. Defaults to [`DEFAULT_CHANNEL_CAPACITY`] (10,000).
    ///
    /// A `channel_capacity` of `0`, or one above [`MAX_CHANNEL_CAPACITY`]
    /// (Tokio's semaphore limit), is invalid for a Tokio mpsc channel. Rather
    /// than panic on caller-supplied (possibly runtime-derived) input, it is
    /// **clamped** into `1..=`[`MAX_CHANNEL_CAPACITY`] with a
    /// `tracing::warn!`; the builder never aborts the process.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.channel_capacity = clamp_channel_capacity(channel_capacity);
        self
    }

    /// Set the minimum interval in milliseconds between consecutive flushes.
    ///
    /// When set to a value greater than 0, the background task waits at least
    /// this long between consecutive flushes. Defaults to
    /// [`DEFAULT_MIN_PUBLISH_INTERVAL_MS`] (0, disabled).
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
    /// Defaults to [`DEFAULT_MAX_RETRIES`] (3). Set to 0 to disable retries.
    /// Retry `n` (zero-based) waits a jittered delay in
    /// `[c / 2, c]` where `c = min(BASE_RETRY_DELAY_MS * 2^n, MAX_RETRY_DELAY_MS)`.
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set a custom event serializer.
    ///
    /// Defaults to [`JsonEventSerializer`]. Use this to switch to a more
    /// compact binary format (e.g. `BincodeEventSerializer`) for lower
    /// latency publishing.
    ///
    /// The serializer runs inside the background task and must return a
    /// `SerializationError` instead of panicking. If it panics anyway, the
    /// task stops and [`shutdown`](Self::shutdown) reports
    /// [`NatsPublisherError::TaskPanicked`].
    ///
    /// # Arguments
    ///
    /// * `serializer` — the serializer implementation to use
    #[must_use = "builders do nothing unless consumed"]
    #[inline]
    pub fn with_serializer(mut self, serializer: Arc<dyn EventSerializer>) -> Self {
        self.serializer = serializer;
        self
    }

    /// Returns the number of successfully published trades.
    #[must_use]
    #[inline]
    pub fn publish_count(&self) -> u64 {
        self.publish_count.load(Ordering::Relaxed)
    }

    /// Returns the number of permanently failed publish attempts.
    #[must_use]
    #[inline]
    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    /// Returns the total number of trades received from the listener callback.
    #[must_use]
    #[inline]
    pub fn events_received(&self) -> u64 {
        self.events_received.load(Ordering::Relaxed)
    }

    /// Returns the total number of drain/flush cycles performed.
    #[must_use]
    #[inline]
    pub fn batches_published(&self) -> u64 {
        self.batches_published.load(Ordering::Relaxed)
    }

    /// Returns the number of trades dropped because the channel was full or
    /// the background task was no longer running.
    #[must_use]
    #[inline]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Returns the current sequence number (next value to be assigned).
    #[must_use]
    #[inline]
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }

    /// Returns a reference to the configured event serializer.
    #[must_use]
    #[inline]
    pub fn serializer(&self) -> &dyn EventSerializer {
        self.serializer.as_ref()
    }

    /// Convert this publisher into a [`TradeListener`] callback.
    ///
    /// This method consumes `self`, wraps it in an `Arc`, spawns a single
    /// background batch task on the configured Tokio runtime, and returns both
    /// the `Arc` handle (for reading metrics) and the listener callback.
    ///
    /// The returned listener clones each [`TradeResult`] into a bounded channel
    /// and returns immediately — no serialization, no `format!`, and no task
    /// spawn happen on the matching hot path. The background task drains the
    /// channel, batches the trades, and publishes each to both
    /// `{prefix}.{symbol}` and the precomputed `{prefix}.all` subject with a
    /// unique sequence number per publish.
    ///
    /// The runtime passed to [`new`](Self::new) must have its time driver
    /// enabled; see the [module docs](self#runtime-requirements). Call
    /// [`shutdown`](Self::shutdown) to stop the task and learn whether it
    /// failed.
    ///
    /// # Returns
    ///
    /// A tuple of `(Arc<NatsTradePublisher>, TradeListener)`. The `Arc` handle
    /// allows the caller to read metrics (`publish_count`, `error_count`,
    /// `events_received`, `dropped_events`, `sequence`) after wiring the
    /// listener into the order book.
    pub fn into_listener(self) -> (Arc<Self>, TradeListener) {
        let channel_capacity = self.channel_capacity;
        let publisher = Arc::new(self);
        let handle = Arc::clone(&publisher);

        let (tx, rx) = mpsc::channel::<TradeResult>(channel_capacity);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Spawn the single background batch task and retain its join handle so
        // `shutdown` can await it instead of leaving it detached.
        let task_publisher = Arc::clone(&publisher);
        let join = publisher
            .runtime
            .spawn(Self::publish_task(task_publisher, rx, shutdown_rx));
        store_slot(&publisher.task_handle, join);
        store_slot(&publisher.shutdown_tx, shutdown_tx);

        // Build the hot-path listener closure: clone + non-blocking send only.
        let listener_publisher = Arc::clone(&publisher);
        let listener = Arc::new(move |trade_result: &TradeResult| {
            increment_metric(&listener_publisher.events_received, "events_received");
            match tx.try_send(trade_result.clone()) {
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

    /// Gracefully shut down the background publish task.
    ///
    /// Signals the background task to stop accepting trades, drain the ones
    /// still buffered in the channel, flush them to NATS (without the
    /// `min_publish_interval_ms` throttle), and exit, then awaits the task's
    /// join handle so teardown does not race in-flight publishes. The signal
    /// is observed both while idle and while a batch window is open.
    ///
    /// Note that the [`TradeListener`] closure still holds a channel sender, so
    /// shutdown does not rely on the listener being dropped first; the explicit
    /// signal is what unblocks the task. After shutdown, further trades are
    /// dropped and counted in `dropped_events`.
    ///
    /// Safe to call more than once and from any task: only the call that
    /// joins the task reports its outcome; later calls (and a call racing
    /// the one joining) return `Ok(())` immediately.
    ///
    /// # Errors
    ///
    /// - [`NatsPublisherError::TaskPanicked`] if the background task panicked
    ///   (for example in a caller-supplied serializer, or because the runtime
    ///   has no time driver).
    /// - [`NatsPublisherError::TaskCancelled`] if the task was cancelled,
    ///   for example because its runtime shut down first.
    pub async fn shutdown(&self) -> Result<(), NatsPublisherError> {
        shutdown_task(&self.shutdown_tx, &self.task_handle, PUBLISHER_NAME).await
    }

    /// Background task that drains the trade channel, batches trades, and
    /// publishes them to NATS.
    ///
    /// The task flushes when either:
    /// - The batch window timer elapses (configurable via `batch_window_ms`)
    /// - The batch reaches `max_batch_size` trades
    ///
    /// When throttling is enabled (`min_publish_interval_ms > 0`), the task
    /// waits at least that duration between consecutive flushes.
    ///
    /// `shutdown_rx` is polled only until it completes: every branch that
    /// observes it returns, so the `oneshot::Receiver` is never polled again
    /// after completion.
    async fn publish_task(
        publisher: Arc<Self>,
        mut rx: mpsc::Receiver<TradeResult>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        info!(
            publisher = PUBLISHER_NAME,
            prefix = %publisher.subject_prefix,
            "NATS publisher task started"
        );
        let batch_window = Duration::from_millis(publisher.batch_window_ms);
        let min_interval = (publisher.min_publish_interval_ms > 0)
            .then(|| Duration::from_millis(publisher.min_publish_interval_ms));

        let mut batch: Vec<TradeResult> = new_batch_buffer(publisher.max_batch_size);
        let mut last_publish = tokio::time::Instant::now();

        loop {
            // Wait for the first trade, a channel close, or a shutdown signal.
            if batch.is_empty() {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        Self::drain_on_shutdown(&publisher, &mut rx, &mut batch).await;
                        return;
                    }
                    maybe = rx.recv() => match maybe {
                        Some(trade) => batch.push(trade),
                        None => break, // Channel closed
                    },
                }
            }

            // Collect more trades within the batch window. A deadline that
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
                                Ok(Some(trade)) => batch.push(trade),
                                Ok(None) => {
                                    // Channel closed — flush remaining and exit.
                                    Self::flush_batch(&publisher, &mut batch).await;
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

            Self::flush_batch(&publisher, &mut batch).await;

            // Throttle before the next flush, raced with the shutdown signal
            // so a long interval never delays teardown.
            if throttle_or_shutdown(&mut last_publish, min_interval, &mut shutdown_rx).await {
                Self::drain_on_shutdown(&publisher, &mut rx, &mut batch).await;
                return;
            }
        }

        // Flush any remaining trades.
        Self::flush_batch(&publisher, &mut batch).await;
    }

    /// Shutdown path: close the channel to new trades, then flush the current
    /// batch plus everything already buffered in `max_batch_size` chunks, so
    /// no accepted trade is lost. Closing first bounds the loop even while the
    /// listener keeps firing. The throttle is skipped so teardown is prompt.
    async fn drain_on_shutdown(
        publisher: &Arc<Self>,
        rx: &mut mpsc::Receiver<TradeResult>,
        batch: &mut Vec<TradeResult>,
    ) {
        rx.close();
        loop {
            drain_buffered(rx, batch, publisher.max_batch_size);
            if batch.is_empty() {
                break;
            }
            Self::flush_batch(publisher, batch).await;
        }
    }

    /// Flush the accumulated batch: serialize and publish each trade to its
    /// per-symbol and aggregate subjects. The throttle is applied by the
    /// caller, raced with the shutdown signal.
    ///
    /// Serialization, subject construction, and the JetStream publish all
    /// happen here in the background task — never on the matching hot path.
    async fn flush_batch(publisher: &Arc<Self>, batch: &mut Vec<TradeResult>) {
        if batch.is_empty() {
            return;
        }

        let trades = std::mem::take(batch);
        for trade in trades {
            let payload = match publisher.serializer.serialize_trade(&trade) {
                Ok(bytes) => bytes,
                Err(e) => {
                    increment_metric(&publisher.error_count, "error_count");
                    error!(error = %e, "failed to serialize trade result for NATS");
                    continue;
                }
            };

            // Reserve both sequence numbers at once; never wrap.
            let Some((symbol_seq, all_seq)) = checked_reserve(&publisher.sequence, 2)
                .and_then(|first| first.checked_add(1).map(|second| (first, second)))
            else {
                counter_exhausted("sequence");
                increment_metric(&publisher.error_count, "error_count");
                continue;
            };
            let symbol_subject = format!("{}.{}", publisher.subject_prefix, trade.symbol);
            let all_subject = publisher.all_subject.clone();
            let payload_bytes: bytes::Bytes = payload.into();

            Self::publish_with_retry(
                Arc::clone(publisher),
                symbol_subject,
                all_subject,
                payload_bytes,
                symbol_seq,
                all_seq,
            )
            .await;
        }

        increment_metric(&publisher.batches_published, "batches_published");
    }

    /// Publish a trade event to both the symbol-specific and aggregate subjects
    /// with retry logic for transient failures.
    ///
    /// Each subject receives its own unique sequence number in the
    /// `Nats-Sequence` header so consumers can deduplicate per-stream without
    /// collisions between the symbol and aggregate streams.
    async fn publish_with_retry(
        publisher: Arc<Self>,
        symbol_subject: String,
        all_subject: String,
        payload: bytes::Bytes,
        symbol_seq: u64,
        all_seq: u64,
    ) {
        let content_type = publisher.serializer.content_type();

        let mut symbol_headers = async_nats::HeaderMap::new();
        symbol_headers.insert("Nats-Sequence", symbol_seq.to_string().as_str());
        symbol_headers.insert("Content-Type", content_type);

        let mut all_headers = async_nats::HeaderMap::new();
        all_headers.insert("Nats-Sequence", all_seq.to_string().as_str());
        all_headers.insert("Content-Type", content_type);

        let policy = RetryPolicy {
            max_retries: publisher.max_retries,
            jitter_seed: publisher.jitter_seed,
        };

        // Publish to symbol-specific subject
        let symbol_ok = publish_with_backoff(
            &publisher.jetstream,
            &publisher.link,
            policy,
            &symbol_subject,
            payload.clone(),
            symbol_headers,
            symbol_seq,
        )
        .await;

        // Publish to aggregate subject
        let all_ok = publish_with_backoff(
            &publisher.jetstream,
            &publisher.link,
            policy,
            &all_subject,
            payload,
            all_headers,
            all_seq,
        )
        .await;

        // Per-trade accounting: a trade is either a clean success or a failure,
        // counted once on the matching counter. `error_count` is NOT
        // incremented per subject so a trade whose two subjects both fail is
        // not double-counted.
        if account_publish_outcome(
            &publisher.publish_count,
            &publisher.error_count,
            symbol_ok,
            all_ok,
        ) {
            trace!(symbol_seq, all_seq, symbol = %symbol_subject, "trade event published to NATS");
        }
    }
}

impl std::fmt::Debug for NatsTradePublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsTradePublisher")
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
            .field("serializer", &self.serializer.content_type())
            .finish()
    }
}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::orderbook::book_change_event::PriceLevelChangedEvent;
    use crate::orderbook::serialization::SerializationError;
    use pricelevel::{Id, MatchResult, Quantity};
    use uuid::Uuid;

    /// A JetStream context backed by a client that never reaches a server
    /// (`retry_on_initial_connect` connects in the background), so tests can
    /// build publishers without a NATS server. Nothing in these tests
    /// reaches an actual publish.
    async fn offline_jetstream() -> async_nats::jetstream::Context {
        let client = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect("nats://127.0.0.1:1")
            .await
            .expect("background connect never fails up front");
        async_nats::jetstream::new(client)
    }

    /// Serializer test double: panics or fails on every trade.
    #[derive(Debug)]
    struct FaultySerializer {
        panic: bool,
    }

    // Deliberate panic to exercise the task-failure path.
    #[allow(clippy::panic_in_result_fn, clippy::manual_assert)]
    impl EventSerializer for FaultySerializer {
        fn serialize_trade(&self, _trade: &TradeResult) -> Result<Vec<u8>, SerializationError> {
            if self.panic {
                panic!("serializer boom");
            }
            Err(SerializationError::Bincode("refused".to_string()))
        }

        fn serialize_book_change(
            &self,
            _event: &PriceLevelChangedEvent,
        ) -> Result<Vec<u8>, SerializationError> {
            Err(SerializationError::Bincode("unused".to_string()))
        }

        fn deserialize_trade(&self, _data: &[u8]) -> Result<TradeResult, SerializationError> {
            Err(SerializationError::Bincode("unused".to_string()))
        }

        fn deserialize_book_change(
            &self,
            _data: &[u8],
        ) -> Result<PriceLevelChangedEvent, SerializationError> {
            Err(SerializationError::Bincode("unused".to_string()))
        }

        fn content_type(&self) -> &'static str {
            "application/x-test"
        }
    }

    #[tokio::test]
    async fn test_builder_extreme_values_are_clamped_without_panicking() {
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        )
        .with_batch_window_ms(u64::MAX)
        .with_max_batch_size(usize::MAX)
        .with_channel_capacity(usize::MAX)
        .with_min_publish_interval_ms(u64::MAX)
        .with_max_retries(u32::MAX);
        assert_eq!(publisher.batch_window_ms, MAX_BATCH_WINDOW_MS);
        assert_eq!(publisher.max_batch_size, MAX_BATCH_SIZE);
        assert_eq!(publisher.channel_capacity, MAX_CHANNEL_CAPACITY);
        assert_eq!(
            publisher.min_publish_interval_ms,
            MAX_MIN_PUBLISH_INTERVAL_MS
        );

        // Spawning the task with the clamped values must not panic, and a
        // shutdown while idle is prompt and clean.
        let (handle, _listener) = publisher.into_listener();
        assert_eq!(handle.shutdown().await, Ok(()));
    }

    #[tokio::test]
    async fn test_builder_zero_values_are_clamped_and_shutdown_drains() {
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        )
        .with_batch_window_ms(0)
        .with_max_batch_size(0)
        .with_channel_capacity(0)
        .with_min_publish_interval_ms(0)
        .with_max_retries(0)
        .with_serializer(Arc::new(FaultySerializer { panic: false }));
        assert_eq!(publisher.max_batch_size, 1);
        assert_eq!(publisher.channel_capacity, 1);

        let (handle, listener) = publisher.into_listener();
        listener(&make_trade_result("BTC/USD"));
        // max_batch_size 0 used to drop buffered trades on shutdown; clamped
        // to 1, every accepted trade reaches the flush (and fails serializing
        // here, so no NATS publish is attempted).
        assert_eq!(handle.shutdown().await, Ok(()));
        assert_eq!(handle.events_received(), 1);
        assert_eq!(
            handle.error_count() + handle.dropped_events(),
            1,
            "the trade was either flushed (error_count) or dropped at a full channel"
        );
    }

    #[tokio::test]
    async fn test_shutdown_flushes_buffered_trades_with_huge_window() {
        // A clamped 60 s window must not delay shutdown: the signal is
        // observed while the batch window is open.
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        )
        .with_batch_window_ms(u64::MAX)
        .with_serializer(Arc::new(FaultySerializer { panic: false }));
        let (handle, listener) = publisher.into_listener();
        for _ in 0..5 {
            listener(&make_trade_result("BTC/USD"));
        }
        // Let the task pick up the first trade and open its batch window.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let joined = tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await;
        assert_eq!(joined, Ok(Ok(())), "shutdown must not wait out the window");
        assert_eq!(handle.error_count(), 5, "every buffered trade was flushed");
        assert_eq!(handle.dropped_events(), 0);
        assert_eq!(handle.sequence(), 0, "no publish was attempted");
    }

    #[tokio::test]
    async fn test_shutdown_during_throttle_completes_promptly() {
        // A 60 s (clamped) publish interval must not delay shutdown: the
        // throttle wait is raced with the shutdown signal.
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        )
        .with_min_publish_interval_ms(u64::MAX)
        .with_serializer(Arc::new(FaultySerializer { panic: false }));
        let (handle, listener) = publisher.into_listener();
        listener(&make_trade_result("BTC/USD"));
        // Let the task flush the first trade and enter the throttle wait.
        for _ in 0..1_000 {
            if handle.batches_published() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(handle.batches_published(), 1, "first flush happened");
        // Buffered while the task is throttled; must still be drained.
        listener(&make_trade_result("BTC/USD"));
        let joined = tokio::time::timeout(Duration::from_secs(5), handle.shutdown()).await;
        assert_eq!(
            joined,
            Ok(Ok(())),
            "shutdown must not wait out the throttle"
        );
        assert_eq!(handle.error_count(), 2, "both trades reached a flush");
        assert_eq!(handle.dropped_events(), 0);
    }

    #[tokio::test]
    async fn test_shutdown_reports_task_panic() {
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        )
        .with_serializer(Arc::new(FaultySerializer { panic: true }));
        let (handle, listener) = publisher.into_listener();
        listener(&make_trade_result("BTC/USD"));
        assert_eq!(
            handle.shutdown().await,
            Err(NatsPublisherError::TaskPanicked {
                message: "serializer boom".to_string()
            })
        );
        // The failure is reported once; a repeated shutdown is a no-op.
        assert_eq!(handle.shutdown().await, Ok(()));

        // The listener keeps working after the task died: trades are counted
        // as dropped, never panicking.
        listener(&make_trade_result("BTC/USD"));
        listener(&make_trade_result("BTC/USD"));
        assert_eq!(handle.dropped_events(), 2);
    }

    #[tokio::test]
    async fn test_shutdown_ok_when_idle_and_idempotent() {
        let publisher = NatsTradePublisher::new(
            offline_jetstream().await,
            "trades".to_string(),
            tokio::runtime::Handle::current(),
        );
        let (handle, _listener) = publisher.into_listener();
        assert_eq!(handle.shutdown().await, Ok(()));
        assert_eq!(handle.shutdown().await, Ok(()));
    }

    fn make_trade_result(symbol: &str) -> TradeResult {
        let order_id = Id::from_uuid(Uuid::new_v4());
        let match_result = MatchResult::new(order_id, Quantity::new(100));
        TradeResult::new(symbol.to_string(), match_result).expect("valid trade result")
    }

    #[test]
    fn test_trade_result_serializes_to_json() {
        let tr = make_trade_result("BTC/USD");
        let result = serde_json::to_vec(&tr);
        assert!(result.is_ok());
        let bytes = result.unwrap_or_default();
        assert!(!bytes.is_empty());

        // Verify it contains expected fields
        let json_str = String::from_utf8(bytes).unwrap_or_default();
        assert!(json_str.contains("BTC/USD"));
        assert!(json_str.contains("match_result"));
    }

    #[test]
    fn test_trade_result_serialize_roundtrip_fields() {
        let tr = make_trade_result("ETH/USDT");
        let json = serde_json::to_value(&tr);
        assert!(json.is_ok());
        let value = json.unwrap_or(serde_json::Value::Null);
        assert_eq!(
            value.get("symbol").and_then(|v| v.as_str()),
            Some("ETH/USDT")
        );
        assert_eq!(
            value.get("total_maker_fees").and_then(|v| v.as_i64()),
            Some(0)
        );
        assert_eq!(
            value.get("total_taker_fees").and_then(|v| v.as_i64()),
            Some(0)
        );
    }

    #[test]
    fn test_subject_formatting() {
        let prefix = "trades";
        let symbol = "BTC/USD";
        let symbol_subject = format!("{prefix}.{symbol}");
        let all_subject = format!("{prefix}.all");

        assert_eq!(symbol_subject, "trades.BTC/USD");
        assert_eq!(all_subject, "trades.all");
    }

    #[test]
    fn test_subject_formatting_with_custom_prefix() {
        let prefix = "orderbook.events.trades";
        let symbol = "ETH-PERP";
        let symbol_subject = format!("{prefix}.{symbol}");
        let all_subject = format!("{prefix}.all");

        assert_eq!(symbol_subject, "orderbook.events.trades.ETH-PERP");
        assert_eq!(all_subject, "orderbook.events.trades.all");
    }

    #[test]
    fn test_precomputed_all_subject_matches_format() {
        // The aggregate subject is precomputed once at construction; it must
        // equal what the per-publish path would otherwise format.
        let prefix = "trades";
        let precomputed = format!("{prefix}.all");
        assert_eq!(precomputed, "trades.all");
    }

    #[test]
    fn test_default_max_retries() {
        assert_eq!(DEFAULT_MAX_RETRIES, 3);
    }

    #[test]
    fn test_base_retry_delay() {
        assert_eq!(BASE_RETRY_DELAY_MS, 10);
    }

    #[test]
    fn test_default_batch_constants() {
        assert_eq!(DEFAULT_BATCH_WINDOW_MS, 1);
        assert_eq!(DEFAULT_MAX_BATCH_SIZE, 100);
        assert_eq!(DEFAULT_CHANNEL_CAPACITY, 10_000);
        assert_eq!(DEFAULT_MIN_PUBLISH_INTERVAL_MS, 0);
    }

    #[test]
    fn test_publish_outcome_accounting_is_per_trade() {
        // #127: publish_count and error_count share one per-trade granularity.
        let publish_count = AtomicU64::new(0);
        let error_count = AtomicU64::new(0);

        // Clean success.
        assert!(account_publish_outcome(
            &publish_count,
            &error_count,
            true,
            true
        ));
        // Partial failure: symbol ok, aggregate exhausted.
        assert!(!account_publish_outcome(
            &publish_count,
            &error_count,
            true,
            false
        ));
        // Partial failure: aggregate ok, symbol exhausted.
        assert!(!account_publish_outcome(
            &publish_count,
            &error_count,
            false,
            true
        ));
        // Full failure: both exhausted.
        assert!(!account_publish_outcome(
            &publish_count,
            &error_count,
            false,
            false
        ));

        let pc = publish_count.load(Ordering::Relaxed);
        let ec = error_count.load(Ordering::Relaxed);
        assert_eq!(pc, 1, "exactly one successful trade");
        assert_eq!(
            ec, 3,
            "three failed trades (two partial, one full), one increment each"
        );
        // Every trade incremented exactly one counter — the totals reconcile.
        assert_eq!(
            pc + ec,
            4,
            "publish_count + error_count == trades processed"
        );
    }

    #[test]
    fn test_nats_publish_error_display() {
        let err = crate::orderbook::OrderBookError::NatsPublishError {
            message: "connection refused".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("nats publish error"));
        assert!(display.contains("connection refused"));
    }

    #[test]
    fn test_nats_serialization_error_display() {
        let err = crate::orderbook::OrderBookError::NatsSerializationError {
            message: "invalid utf-8".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("nats serialization error"));
        assert!(display.contains("invalid utf-8"));
    }
}
