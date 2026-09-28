//! Shared plumbing for the NATS JetStream publishers (feature `nats`).
//!
//! [`NatsTradePublisher`](crate::orderbook::nats::NatsTradePublisher) and
//! [`NatsBookChangePublisher`](crate::orderbook::nats_book_change::NatsBookChangePublisher)
//! share the same configuration limits, bounded-channel hand-off, retry with
//! capped exponential backoff and jitter, background-task shutdown, and typed
//! task-failure error. This module holds that shared code so the two
//! publishers cannot drift apart. It depends only on `tokio`, `async-nats`,
//! `tracing` and `thiserror`; the core engine never depends on it.
//!
//! # Runtime requirements
//!
//! The publishers spawn their background task on the Tokio runtime handle the
//! caller passes to `new`. That runtime must have its **time driver enabled**
//! (`tokio::runtime::Builder::enable_time` / `enable_all`, which
//! `#[tokio::main]` does by default): the task uses `tokio::time::timeout_at`
//! and `tokio::time::sleep`, and Tokio panics inside the task when timers are
//! disabled. Tokio exposes no API to check that from a `Handle`, so it cannot
//! be validated up front; such a panic is contained in the task and reported
//! by `shutdown()` as [`NatsPublisherError::TaskPanicked`]. A runtime that has
//! already shut down cancels the task, reported as
//! [`NatsPublisherError::TaskCancelled`].

use crate::orderbook::error::OrderBookError;
use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};
use tracing::{debug, error, info, warn};

/// Largest accepted batch window, in milliseconds (one minute).
///
/// `with_batch_window_ms` clamps larger values down to this with a
/// `tracing::warn!`. The bound keeps `Instant + window` far from overflow and
/// keeps a buffered event from waiting longer than a minute to be published.
pub const MAX_BATCH_WINDOW_MS: u64 = 60_000;

/// Largest accepted minimum publish interval, in milliseconds (one minute).
///
/// `with_min_publish_interval_ms` clamps larger values down to this with a
/// `tracing::warn!`.
pub const MAX_MIN_PUBLISH_INTERVAL_MS: u64 = 60_000;

/// Largest accepted batch size, in events.
///
/// `with_max_batch_size` clamps larger values down to this, and `0` up to `1`,
/// with a `tracing::warn!`. The batch buffer is pre-allocated to this many
/// events with a fallible reservation, so the bound also caps that
/// allocation.
pub const MAX_BATCH_SIZE: usize = 65_536;

/// Largest accepted bounded-channel capacity, in events.
///
/// Equal to [`tokio::sync::Semaphore::MAX_PERMITS`]: Tokio's bounded `mpsc`
/// channel panics above it. `with_channel_capacity` clamps larger values
/// down to this, and `0` up to `1`, with a `tracing::warn!`. Tokio allocates
/// channel storage lazily, so a large capacity does not allocate up front.
pub const MAX_CHANNEL_CAPACITY: usize = tokio::sync::Semaphore::MAX_PERMITS;

/// Base delay, in milliseconds, of the exponential retry backoff.
///
/// Retry `n` (zero-based) has a delay ceiling of
/// `BASE_RETRY_DELAY_MS * 2^n`, capped at [`MAX_RETRY_DELAY_MS`].
pub const BASE_RETRY_DELAY_MS: u64 = 10;

/// Cap, in milliseconds, on a single retry backoff delay (five seconds).
pub const MAX_RETRY_DELAY_MS: u64 = 5_000;

/// Failure of a NATS publisher's background task, reported by `shutdown()`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NatsPublisherError {
    /// The background publish task panicked. The usual causes are a
    /// caller-supplied `EventSerializer` that panicked, or a runtime without
    /// the time driver enabled.
    #[error("nats publisher task panicked: {message}")]
    TaskPanicked {
        /// The panic payload when it is a string, otherwise a placeholder.
        message: String,
    },

    /// The background publish task was cancelled before it finished, for
    /// example because its Tokio runtime shut down first.
    #[error("nats publisher task was cancelled before it finished")]
    TaskCancelled,
}

impl NatsPublisherError {
    /// Maps a Tokio [`JoinError`] from the publisher task to a typed error.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub(crate) fn from_join_error(err: JoinError) -> Self {
        if err.is_cancelled() {
            return Self::TaskCancelled;
        }
        match err.try_into_panic() {
            Ok(payload) => Self::TaskPanicked {
                message: panic_payload_message(payload.as_ref()),
            },
            // A `JoinError` is either a panic or a cancellation.
            Err(_) => Self::TaskCancelled,
        }
    }
}

impl From<NatsPublisherError> for OrderBookError {
    /// Folds a publisher task failure into
    /// [`OrderBookError::NatsPublishError`], keeping its message.
    #[cold]
    fn from(err: NatsPublisherError) -> Self {
        OrderBookError::NatsPublishError {
            message: err.to_string(),
        }
    }
}

/// Extracts a readable message from a panic payload.
#[cold]
fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

// ─── Builder validation ─────────────────────────────────────────────────────

/// Clamps a bounded-channel capacity into `1..=MAX_CHANNEL_CAPACITY`.
///
/// `0` and values above [`MAX_CHANNEL_CAPACITY`] would make Tokio's
/// `mpsc::channel` panic; they are recoverable bad input, so they are clamped
/// with a `tracing::warn!` instead.
#[must_use]
pub(crate) fn clamp_channel_capacity(requested: usize) -> usize {
    if requested == 0 {
        warn!("with_channel_capacity(0) is invalid; clamping to 1");
        1
    } else if requested > MAX_CHANNEL_CAPACITY {
        warn!(
            requested,
            max = MAX_CHANNEL_CAPACITY,
            "with_channel_capacity above the Tokio channel limit; clamping to the maximum"
        );
        MAX_CHANNEL_CAPACITY
    } else {
        requested
    }
}

/// Clamps a batch size into `1..=MAX_BATCH_SIZE`.
///
/// A batch size of `0` would never let the shutdown drain make progress (it
/// would drop buffered events); very large sizes would make the batch
/// pre-allocation unbounded. Both are clamped with a `tracing::warn!`.
#[must_use]
pub(crate) fn clamp_max_batch_size(requested: usize) -> usize {
    if requested == 0 {
        warn!("with_max_batch_size(0) is invalid; clamping to 1");
        1
    } else if requested > MAX_BATCH_SIZE {
        warn!(
            requested,
            max = MAX_BATCH_SIZE,
            "with_max_batch_size above the documented maximum; clamping"
        );
        MAX_BATCH_SIZE
    } else {
        requested
    }
}

/// Clamps a millisecond duration setting down to `max` with a
/// `tracing::warn!` naming the builder `setting`.
#[must_use]
pub(crate) fn clamp_duration_ms(setting: &'static str, requested: u64, max: u64) -> u64 {
    if requested > max {
        warn!(
            setting,
            requested, max, "millisecond setting above the documented maximum; clamping"
        );
        max
    } else {
        requested
    }
}

// ─── Background task helpers ────────────────────────────────────────────────

/// Pre-allocates a batch buffer of `capacity` events with a fallible
/// reservation. On failure the buffer starts empty and grows on demand.
#[must_use]
pub(crate) fn new_batch_buffer<U>(capacity: usize) -> Vec<U> {
    let mut batch = Vec::new();
    if let Err(e) = batch.try_reserve(capacity) {
        warn!(
            capacity,
            error = %e,
            "could not pre-allocate the NATS batch buffer; growing on demand"
        );
    }
    batch
}

/// Drains every immediately-available item from `rx` into `out` until `out`
/// holds `limit` items, without awaiting new sends. Returns the number
/// drained.
///
/// Used by the shutdown path to flush events that were already accepted into
/// the channel before teardown, so none are silently lost. `try_recv` never
/// blocks: it stops as soon as the channel is momentarily empty or closed.
pub(crate) fn drain_buffered<T, U: From<T>>(
    rx: &mut mpsc::Receiver<T>,
    out: &mut Vec<U>,
    limit: usize,
) -> usize {
    let start = out.len();
    while out.len() < limit {
        match rx.try_recv() {
            Ok(item) => out.push(U::from(item)),
            Err(_) => break,
        }
    }
    // `out` only grew, so `start..` is always in range.
    out.get(start..).map_or(0, <[U]>::len)
}

/// Deadline `now + window` for collecting a batch, or `None` if it overflows
/// the clock (the caller then flushes immediately).
#[inline]
#[must_use]
pub(crate) fn batch_deadline(
    now: tokio::time::Instant,
    window: Duration,
) -> Option<tokio::time::Instant> {
    now.checked_add(window)
}

/// Remaining throttle wait: `interval - elapsed`, or `None` when the interval
/// has already elapsed.
#[inline]
#[must_use]
pub(crate) fn throttle_remaining(interval: Duration, elapsed: Duration) -> Option<Duration> {
    interval
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
}

/// Sleeps out the remainder of the throttle interval since `last_publish`,
/// racing the wait against the shutdown signal, then stamps `last_publish`
/// with the current instant.
///
/// Returns `true` when `shutdown_rx` completed during the wait (a shutdown
/// request, or its sender dropped); the caller must then go straight to its
/// shutdown drain and must not poll `shutdown_rx` again. Returns `false`
/// when the wait ran out or no throttle applies, in which case
/// `shutdown_rx` was not completed.
pub(crate) async fn throttle_or_shutdown(
    last_publish: &mut tokio::time::Instant,
    min_interval: Option<Duration>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> bool {
    let mut shutdown_requested = false;
    if let Some(interval) = min_interval
        && let Some(remaining) = throttle_remaining(interval, last_publish.elapsed())
    {
        tokio::select! {
            biased;
            _ = &mut *shutdown_rx => shutdown_requested = true,
            () = tokio::time::sleep(remaining) => {}
        }
    }
    *last_publish = tokio::time::Instant::now();
    shutdown_requested
}

// ─── Counters ───────────────────────────────────────────────────────────────

/// Atomically reserves `n` consecutive values from `counter` and returns the
/// first one, or `None` if that would overflow `u64` (the counter is left
/// unchanged). Never wraps.
#[inline]
#[must_use]
pub(crate) fn checked_reserve(counter: &AtomicU64, n: u64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(n)
        })
        .ok()
}

/// Increments a metric counter by one without wrapping. On overflow the
/// counter stays at `u64::MAX` and an `ERROR` is logged naming it.
#[inline]
pub(crate) fn increment_metric(counter: &AtomicU64, name: &'static str) {
    if checked_reserve(counter, 1).is_none() {
        counter_exhausted(name);
    }
}

/// Logs a counter that reached `u64::MAX`.
#[cold]
#[inline(never)]
pub(crate) fn counter_exhausted(name: &'static str) {
    error!(
        counter = name,
        "NATS publisher counter reached u64::MAX and is no longer incremented"
    );
}

// ─── Listener hand-off ──────────────────────────────────────────────────────

/// Rate-limited logging for events the listener could not hand to the
/// background task.
///
/// A full channel logs one `WARN` per overload episode (reset by the next
/// successful send) instead of one per dropped event on the matching hot
/// path; a closed channel (the task is no longer running) logs one `WARN`
/// for the publisher's lifetime. Every dropped event is still counted.
#[derive(Debug, Default)]
pub(crate) struct DropLog {
    /// Set while the channel is full and the overload warning was emitted.
    full_logged: AtomicBool,
    /// Set once the closed-channel warning was emitted.
    closed_logged: AtomicBool,
}

impl DropLog {
    /// Records a successful send, ending a full-channel episode.
    #[inline]
    pub(crate) fn on_sent(&self) {
        if self.full_logged.load(Ordering::Relaxed) {
            self.full_logged.store(false, Ordering::Relaxed);
        }
    }

    /// Records a failed send: counts it in `dropped` and logs per the rules
    /// above.
    pub(crate) fn on_dropped<T>(
        &self,
        err: &TrySendError<T>,
        dropped: &AtomicU64,
        publisher: &'static str,
    ) {
        increment_metric(dropped, "dropped_events");
        match err {
            TrySendError::Full(_) => {
                if !self.full_logged.load(Ordering::Relaxed)
                    && !self.full_logged.swap(true, Ordering::Relaxed)
                {
                    warn!(
                        publisher,
                        "NATS publisher channel full; dropping events until it drains"
                    );
                }
            }
            TrySendError::Closed(_) => {
                if !self.closed_logged.load(Ordering::Relaxed)
                    && !self.closed_logged.swap(true, Ordering::Relaxed)
                {
                    warn!(
                        publisher,
                        "NATS publisher task is not running; events are dropped (shutdown() reports why)"
                    );
                }
            }
        }
    }
}

// ─── Connection state ───────────────────────────────────────────────────────

/// Tracks whether the publish path is currently failing, logging `INFO` at
/// each connected / disconnected transition (not per publish).
#[derive(Debug, Default)]
pub(crate) struct LinkState {
    /// `true` while publishes are failing.
    down: AtomicBool,
}

impl LinkState {
    /// Records an acknowledged publish.
    #[inline]
    fn mark_up(&self, jetstream: &async_nats::jetstream::Context, subject: &str) {
        if self.down.load(Ordering::Relaxed) && self.down.swap(false, Ordering::Relaxed) {
            info!(
                subject,
                state = ?jetstream.client().connection_state(),
                "NATS publisher connected: publishes are acknowledged again"
            );
        }
    }

    /// Records a failed publish attempt.
    #[inline]
    fn mark_down(&self, jetstream: &async_nats::jetstream::Context, subject: &str) {
        if !self.down.load(Ordering::Relaxed) && !self.down.swap(true, Ordering::Relaxed) {
            info!(
                subject,
                state = ?jetstream.client().connection_state(),
                "NATS publisher disconnected: publishes are failing, retrying with backoff"
            );
        }
    }
}

// ─── Backoff ────────────────────────────────────────────────────────────────

/// Multiplicative mixing constants (wyhash).
const MIX_K0: u64 = 0xa076_1d64_78bd_642f;
const MIX_K1: u64 = 0xe703_7ed1_a0b4_28db;

/// Process-wide counter feeding [`new_jitter_seed`], so two publishers built
/// in the same clock tick still get different seeds.
static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 64x64 -> 128-bit multiply folded to 64 bits (`lo ^ hi`).
#[inline]
#[must_use]
fn mum(a: u64, b: u64) -> u64 {
    // `(2^64 - 1)^2 < 2^128`, so the product never overflows.
    match u128::from(a).checked_mul(u128::from(b)) {
        Some(product) => {
            let lo = u64::try_from(product & u128::from(u64::MAX)).unwrap_or_default();
            let hi = product
                .checked_shr(64)
                .and_then(|high| u64::try_from(high).ok())
                .unwrap_or_default();
            lo ^ hi
        }
        None => a ^ b,
    }
}

/// Deterministic 64-bit hash of `(seed, sequence, retry)` used as jitter.
#[inline]
#[must_use]
fn jitter_hash(seed: u64, sequence: u64, retry: u64) -> u64 {
    let first = mum(seed ^ MIX_K0, sequence ^ MIX_K1);
    mum(first ^ MIX_K0, retry ^ MIX_K1)
}

/// Builds a per-publisher jitter seed from a process-wide counter and the
/// wall clock. Never panics: a clock before the Unix epoch contributes `0`
/// and an exhausted counter keeps its last value.
///
/// The seed only decorrelates retry timing between publishers; it has no
/// effect on published data or replay determinism.
#[must_use]
pub(crate) fn new_jitter_seed() -> u64 {
    let counter = SEED_COUNTER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or_else(|current| current);
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| {
            mum(
                elapsed.as_secs() ^ MIX_K0,
                u64::from(elapsed.subsec_nanos()) ^ MIX_K1,
            )
        })
        .unwrap_or_default();
    mum(counter ^ MIX_K0, clock ^ MIX_K1)
}

/// Delay ceiling, in milliseconds, for zero-based retry `retry`:
/// `BASE_RETRY_DELAY_MS * 2^retry`, capped at [`MAX_RETRY_DELAY_MS`].
/// Monotonically non-decreasing in `retry`; every step is checked.
#[inline]
#[must_use]
pub(crate) fn backoff_ceiling_ms(retry: u64) -> u64 {
    u32::try_from(retry)
        .ok()
        .and_then(|shift| 1u64.checked_shl(shift))
        .and_then(|factor| BASE_RETRY_DELAY_MS.checked_mul(factor))
        .map_or(MAX_RETRY_DELAY_MS, |delay| delay.min(MAX_RETRY_DELAY_MS))
}

/// Jittered backoff delay, in milliseconds, for zero-based retry `retry` of
/// the publish carrying `sequence` ("equal jitter").
///
/// The result lies in `[ceiling / 2, ceiling]` where `ceiling` is
/// [`backoff_ceiling_ms`]`(retry)`, so it never exceeds
/// [`MAX_RETRY_DELAY_MS`] and never collapses to zero. The jitter is a hash
/// of `(seed, sequence, retry)`: deterministic for a given seed, and
/// decorrelated across publishers (different seeds) and messages.
#[inline]
#[must_use]
pub(crate) fn backoff_delay_ms(seed: u64, sequence: u64, retry: u64) -> u64 {
    let ceiling = backoff_ceiling_ms(retry);
    let floor = ceiling.checked_div(2).unwrap_or_default();
    // Jitter range is `0..=ceiling - floor`.
    let span = ceiling
        .checked_sub(floor)
        .and_then(|width| width.checked_add(1))
        .unwrap_or(1);
    let jitter = jitter_hash(seed, sequence, retry)
        .checked_rem(span)
        .unwrap_or_default();
    floor.checked_add(jitter).unwrap_or(ceiling)
}

/// Retry configuration shared by one publisher's publishes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// Retries after the first attempt (`0` disables retrying).
    pub(crate) max_retries: u32,
    /// Per-publisher jitter seed from [`new_jitter_seed`].
    pub(crate) jitter_seed: u64,
}

/// Publishes one message with capped exponential backoff and jitter between
/// attempts.
///
/// Returns `true` once JetStream acknowledges the message, `false` after
/// `max_retries + 1` failed attempts (logged at `ERROR`). Each failed attempt
/// is logged at `WARN`; connected / disconnected transitions at `INFO`.
#[inline(never)]
pub(crate) async fn publish_with_backoff(
    jetstream: &async_nats::jetstream::Context,
    link: &LinkState,
    policy: RetryPolicy,
    subject: &str,
    payload: bytes::Bytes,
    headers: async_nats::HeaderMap,
    sequence: u64,
) -> bool {
    let max_retries = u64::from(policy.max_retries);

    // `attempt` is the one-based attempt number and `retry` the zero-based
    // retry index, so `max_retries + 1` attempts run with no arithmetic.
    for (attempt, retry) in (1..=u64::MAX).zip(0..=max_retries) {
        let publish_result = jetstream
            .publish_with_headers(subject.to_string(), headers.clone(), payload.clone())
            .await;

        match publish_result {
            Ok(ack_future) => match ack_future.await {
                Ok(_) => {
                    link.mark_up(jetstream, subject);
                    return true;
                }
                Err(e) => {
                    warn!(
                        attempt,
                        max_retries,
                        subject,
                        error = %e,
                        "NATS ack failed, retrying"
                    );
                }
            },
            Err(e) => {
                warn!(
                    attempt,
                    max_retries,
                    subject,
                    error = %e,
                    "NATS publish failed, retrying"
                );
            }
        }
        link.mark_down(jetstream, subject);

        if retry < max_retries {
            let delay_ms = backoff_delay_ms(policy.jitter_seed, sequence, retry);
            debug!(attempt, delay_ms, subject, "NATS publish backoff");
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    error!(
        subject,
        max_retries, "NATS publish failed after all retries"
    );
    false
}

// ─── Task lifecycle ─────────────────────────────────────────────────────────

/// Stores `value` in a task-lifecycle slot.
///
/// Lock poisoning is recovered with [`PoisonError::into_inner`]: every
/// critical section on these slots is a single `Option` assignment or
/// `take()` that cannot panic halfway, so a poisoned slot still holds either
/// the previous or the new `Option`, both of which are consistent.
pub(crate) fn store_slot<V>(slot: &Mutex<Option<V>>, value: V) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
}

/// Takes the value out of a task-lifecycle slot, recovering a poisoned lock
/// as described on [`store_slot`].
#[must_use]
pub(crate) fn take_slot<V>(slot: &Mutex<Option<V>>) -> Option<V> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Signals the background task to drain and exit, then joins it.
///
/// Returns `Ok(())` when the task finished normally or was already joined by
/// an earlier call, and the typed [`NatsPublisherError`] when it panicked or
/// was cancelled. No lock guard is held across the `.await`.
pub(crate) async fn shutdown_task(
    shutdown_tx: &Mutex<Option<oneshot::Sender<()>>>,
    task_handle: &Mutex<Option<JoinHandle<()>>>,
    publisher: &'static str,
) -> Result<(), NatsPublisherError> {
    if let Some(tx) = take_slot(shutdown_tx)
        && tx.send(()).is_err()
    {
        debug!(
            publisher,
            "NATS publisher task exited before the shutdown signal"
        );
    }

    let Some(handle) = take_slot(task_handle) else {
        return Ok(());
    };
    match handle.await {
        Ok(()) => {
            info!(publisher, "NATS publisher task stopped");
            Ok(())
        }
        Err(join_error) => {
            let err = NatsPublisherError::from_join_error(join_error);
            error!(publisher, error = %err, "NATS publisher task failed");
            Err(err)
        }
    }
}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_channel_capacity_bounds() {
        assert_eq!(clamp_channel_capacity(0), 1);
        assert_eq!(clamp_channel_capacity(1), 1);
        assert_eq!(clamp_channel_capacity(10_000), 10_000);
        assert_eq!(
            clamp_channel_capacity(MAX_CHANNEL_CAPACITY),
            MAX_CHANNEL_CAPACITY
        );
        assert_eq!(clamp_channel_capacity(usize::MAX), MAX_CHANNEL_CAPACITY);
    }

    #[test]
    fn test_clamped_channel_capacity_builds_channel_without_panicking() {
        // The clamped maximum must be accepted by Tokio (no up-front allocation).
        let (tx, mut rx) = mpsc::channel::<u8>(clamp_channel_capacity(usize::MAX));
        tx.try_send(7).expect("channel has room");
        assert_eq!(rx.try_recv().ok(), Some(7));
    }

    #[test]
    fn test_clamp_max_batch_size_bounds() {
        assert_eq!(clamp_max_batch_size(0), 1);
        assert_eq!(clamp_max_batch_size(1), 1);
        assert_eq!(clamp_max_batch_size(100), 100);
        assert_eq!(clamp_max_batch_size(MAX_BATCH_SIZE), MAX_BATCH_SIZE);
        assert_eq!(clamp_max_batch_size(usize::MAX), MAX_BATCH_SIZE);
    }

    #[test]
    fn test_clamp_duration_ms_bounds() {
        assert_eq!(clamp_duration_ms("w", 0, MAX_BATCH_WINDOW_MS), 0);
        assert_eq!(clamp_duration_ms("w", 5, MAX_BATCH_WINDOW_MS), 5);
        assert_eq!(
            clamp_duration_ms("w", u64::MAX, MAX_BATCH_WINDOW_MS),
            MAX_BATCH_WINDOW_MS
        );
    }

    #[test]
    fn test_new_batch_buffer_reserves_capacity() {
        let batch: Vec<u64> = new_batch_buffer(MAX_BATCH_SIZE);
        assert!(batch.capacity() >= MAX_BATCH_SIZE);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_new_batch_buffer_huge_request_does_not_panic() {
        // try_reserve reports capacity overflow instead of panicking.
        let batch: Vec<u64> = new_batch_buffer(usize::MAX);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_batch_deadline_overflow_is_none() {
        let now = tokio::time::Instant::now();
        assert!(batch_deadline(now, Duration::from_millis(MAX_BATCH_WINDOW_MS)).is_some());
        assert!(batch_deadline(now, Duration::MAX).is_none());
    }

    #[test]
    fn test_throttle_remaining_is_checked() {
        let interval = Duration::from_millis(10);
        assert_eq!(
            throttle_remaining(interval, Duration::from_millis(4)),
            Some(Duration::from_millis(6))
        );
        assert_eq!(throttle_remaining(interval, interval), None);
        assert_eq!(throttle_remaining(interval, Duration::MAX), None);
    }

    #[tokio::test]
    async fn test_throttle_or_shutdown_returns_promptly_on_shutdown() {
        let (tx, mut rx) = oneshot::channel::<()>();
        let mut last_publish = tokio::time::Instant::now();
        let interval = Some(Duration::from_millis(MAX_MIN_PUBLISH_INTERVAL_MS));
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(());
        });
        let started = std::time::Instant::now();
        let observed = tokio::time::timeout(
            Duration::from_secs(5),
            throttle_or_shutdown(&mut last_publish, interval, &mut rx),
        )
        .await;
        assert_eq!(observed, Ok(true), "shutdown must interrupt the throttle");
        assert!(started.elapsed() < Duration::from_secs(5));
        sender.await.expect("sender task");
    }

    #[tokio::test]
    async fn test_throttle_or_shutdown_without_signal_waits_out_interval() {
        let (_tx, mut rx) = oneshot::channel::<()>();
        let mut last_publish = tokio::time::Instant::now();
        assert!(!throttle_or_shutdown(&mut last_publish, None, &mut rx).await);
        let before = last_publish;
        assert!(
            !throttle_or_shutdown(&mut last_publish, Some(Duration::from_millis(5)), &mut rx).await
        );
        assert!(last_publish >= before, "last_publish is re-stamped");
    }

    #[test]
    fn test_drain_buffered_collects_all_pending_items() {
        // The shutdown path must drain every already-accepted item so none is
        // lost on teardown.
        let (tx, mut rx) = mpsc::channel::<u32>(4);
        for i in 0..3u32 {
            tx.try_send(i).expect("channel has room");
        }
        let mut out: Vec<u32> = Vec::new();
        let drained = drain_buffered(&mut rx, &mut out, 100);
        assert_eq!(drained, 3, "all buffered items must be drained");
        assert_eq!(out, vec![0, 1, 2], "drain preserves FIFO order");

        let mut out2: Vec<u32> = Vec::new();
        assert_eq!(drain_buffered(&mut rx, &mut out2, 100), 0);
        assert!(out2.is_empty());
    }

    #[test]
    fn test_drain_buffered_respects_limit() {
        let (tx, mut rx) = mpsc::channel::<u32>(8);
        for i in 0..5u32 {
            tx.try_send(i).expect("channel has room");
        }
        let mut out: Vec<u32> = Vec::new();
        assert_eq!(drain_buffered(&mut rx, &mut out, 2), 2);
        assert_eq!(out, vec![0, 1]);
        let mut rest: Vec<u32> = Vec::new();
        assert_eq!(drain_buffered(&mut rx, &mut rest, 100), 3);
        assert_eq!(rest, vec![2, 3, 4]);
    }

    #[test]
    fn test_drain_buffered_after_close_still_yields_buffered_items() {
        // Shutdown closes the receiver first; already-buffered items remain.
        let (tx, mut rx) = mpsc::channel::<u32>(4);
        tx.try_send(1).expect("channel has room");
        rx.close();
        assert!(matches!(tx.try_send(2), Err(TrySendError::Closed(_))));
        let mut out: Vec<u32> = Vec::new();
        assert_eq!(drain_buffered(&mut rx, &mut out, 1), 1);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn test_checked_reserve_never_wraps() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(checked_reserve(&counter, 1), Some(u64::MAX - 1));
        assert_eq!(checked_reserve(&counter, 1), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);

        let pair = AtomicU64::new(u64::MAX - 1);
        assert_eq!(checked_reserve(&pair, 2), None, "a pair must fit whole");
        assert_eq!(pair.load(Ordering::Relaxed), u64::MAX - 1);
    }

    #[test]
    fn test_increment_metric_at_max_does_not_wrap() {
        let counter = AtomicU64::new(u64::MAX);
        increment_metric(&counter, "test");
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn test_drop_log_counts_full_and_closed() {
        let log = DropLog::default();
        let dropped = AtomicU64::new(0);
        let (tx, rx) = mpsc::channel::<u8>(1);
        tx.try_send(0).expect("channel has room");
        for _ in 0..3 {
            let err = tx.try_send(1).expect_err("channel is full");
            log.on_dropped(&err, &dropped, "test");
        }
        assert!(log.full_logged.load(Ordering::Relaxed));
        log.on_sent();
        assert!(!log.full_logged.load(Ordering::Relaxed));

        drop(rx);
        let err = tx.try_send(2).expect_err("receiver dropped");
        assert!(matches!(err, TrySendError::Closed(_)));
        log.on_dropped(&err, &dropped, "test");
        assert!(log.closed_logged.load(Ordering::Relaxed));
        assert_eq!(dropped.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn test_backoff_ceiling_sequence_is_capped_and_monotone() {
        let expected = [10u64, 20, 40, 80, 160, 320, 640, 1_280, 2_560, 5_000, 5_000];
        for (retry, want) in (0u64..).zip(expected) {
            assert_eq!(backoff_ceiling_ms(retry), want, "retry {retry}");
        }
        let mut previous = 0u64;
        for retry in (0u64..200).chain([u64::from(u32::MAX), u64::MAX]) {
            let ceiling = backoff_ceiling_ms(retry);
            assert!(ceiling >= previous, "ceiling must not decrease");
            assert!(ceiling <= MAX_RETRY_DELAY_MS, "ceiling must be capped");
            previous = ceiling;
        }
        assert_eq!(backoff_ceiling_ms(u64::MAX), MAX_RETRY_DELAY_MS);
    }

    #[test]
    fn test_backoff_delay_jitter_within_bounds() {
        for seed in [0u64, 1, 42, u64::MAX, new_jitter_seed()] {
            for sequence in [0u64, 1, 7, u64::MAX] {
                for retry in (0u64..40).chain([u64::MAX]) {
                    let ceiling = backoff_ceiling_ms(retry);
                    let delay = backoff_delay_ms(seed, sequence, retry);
                    assert!(delay <= ceiling, "delay {delay} above ceiling {ceiling}");
                    assert!(delay >= ceiling / 2, "delay {delay} below half ceiling");
                    assert!(delay > 0, "delay never collapses to zero");
                }
            }
        }
    }

    #[test]
    fn test_backoff_delay_is_deterministic_for_seed() {
        let first: Vec<u64> = (0u64..20).map(|r| backoff_delay_ms(1234, 99, r)).collect();
        let second: Vec<u64> = (0u64..20).map(|r| backoff_delay_ms(1234, 99, r)).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn test_backoff_delay_jitter_varies_with_seed() {
        // At the cap the jitter span is 2_501 values; distinct seeds should
        // not all land on the same delay.
        let delays: std::collections::HashSet<u64> = (0u64..64)
            .map(|seed| backoff_delay_ms(seed, 5, 20))
            .collect();
        assert!(delays.len() > 1, "jitter must depend on the seed");
    }

    #[test]
    fn test_new_jitter_seed_differs_between_calls() {
        assert_ne!(new_jitter_seed(), new_jitter_seed());
    }

    #[test]
    fn test_poisoned_slot_is_recovered() {
        let slot: std::sync::Arc<Mutex<Option<u32>>> = std::sync::Arc::new(Mutex::new(None));
        let poisoner = std::sync::Arc::clone(&slot);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("lock");
            panic!("poison the slot");
        })
        .join();
        assert!(slot.is_poisoned());
        store_slot(&slot, 5);
        assert_eq!(take_slot(&slot), Some(5));
        assert_eq!(take_slot(&slot), None);
    }

    #[tokio::test]
    async fn test_shutdown_task_reports_panic() {
        let shutdown_tx: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);
        let task_handle: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
        let (tx, rx) = oneshot::channel::<()>();
        store_slot(&shutdown_tx, tx);
        store_slot(
            &task_handle,
            tokio::spawn(async move {
                let _ = rx.await;
                panic!("task boom");
            }),
        );
        let result = shutdown_task(&shutdown_tx, &task_handle, "test").await;
        assert_eq!(
            result,
            Err(NatsPublisherError::TaskPanicked {
                message: "task boom".to_string()
            })
        );
        // The failure is reported once; a second call is a no-op.
        assert_eq!(
            shutdown_task(&shutdown_tx, &task_handle, "test").await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn test_shutdown_task_ok_when_task_exits_normally() {
        let shutdown_tx: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);
        let task_handle: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
        let (tx, rx) = oneshot::channel::<()>();
        store_slot(&shutdown_tx, tx);
        store_slot(
            &task_handle,
            tokio::spawn(async move {
                let _ = rx.await;
            }),
        );
        assert_eq!(
            shutdown_task(&shutdown_tx, &task_handle, "test").await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn test_shutdown_task_reports_cancellation() {
        let shutdown_tx: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);
        let task_handle: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
        let handle = tokio::spawn(std::future::pending::<()>());
        handle.abort();
        store_slot(&task_handle, handle);
        assert_eq!(
            shutdown_task(&shutdown_tx, &task_handle, "test").await,
            Err(NatsPublisherError::TaskCancelled)
        );
    }

    #[test]
    fn test_nats_publisher_error_display_and_conversion() {
        let err = NatsPublisherError::TaskPanicked {
            message: "boom".to_string(),
        };
        assert_eq!(err.to_string(), "nats publisher task panicked: boom");
        let book_err = OrderBookError::from(err);
        assert!(book_err.to_string().contains("boom"));
        assert_eq!(
            NatsPublisherError::TaskCancelled.to_string(),
            "nats publisher task was cancelled before it finished"
        );
    }

    #[test]
    fn test_panic_payload_message_variants() {
        let static_payload: Box<dyn Any + Send> = Box::new("static");
        assert_eq!(panic_payload_message(static_payload.as_ref()), "static");
        let owned_payload: Box<dyn Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_payload_message(owned_payload.as_ref()), "owned");
        let other_payload: Box<dyn Any + Send> = Box::new(5u8);
        assert_eq!(
            panic_payload_message(other_payload.as_ref()),
            "non-string panic payload"
        );
    }
}
