use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::warn;

/// Error returned by [`try_current_time_millis`] when the wall clock cannot be
/// represented as `u64` milliseconds since the UNIX epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TimeError {
    /// The system clock reads a time before the UNIX epoch (1970-01-01).
    #[error("system clock is {behind:?} before the UNIX epoch")]
    ClockBeforeEpoch {
        /// How far before the epoch the clock reads.
        behind: Duration,
    },
    /// The number of milliseconds since the epoch does not fit in `u64`.
    #[error("{millis} ms since the UNIX epoch does not fit in u64")]
    MillisOverflow {
        /// The unrepresentable millisecond count.
        millis: u128,
    },
}

/// Converts a `Duration` since the UNIX epoch to `u64` milliseconds.
///
/// # Errors
///
/// [`TimeError::MillisOverflow`] when the millisecond count exceeds `u64::MAX`.
pub(crate) fn duration_to_millis(since_epoch: Duration) -> Result<u64, TimeError> {
    let millis = since_epoch.as_millis();
    u64::try_from(millis).map_err(|_| TimeError::MillisOverflow { millis })
}

/// Converts a `SystemTime` to `u64` milliseconds since the UNIX epoch.
///
/// # Errors
///
/// - [`TimeError::ClockBeforeEpoch`] when `time` is before the epoch.
/// - [`TimeError::MillisOverflow`] when the millisecond count exceeds `u64::MAX`.
pub(crate) fn system_time_to_millis(time: SystemTime) -> Result<u64, TimeError> {
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .map_err(|e| TimeError::ClockBeforeEpoch {
            behind: e.duration(),
        })?;
    duration_to_millis(since_epoch)
}

/// Returns the current wall-clock time in milliseconds since the UNIX epoch,
/// or an error if it cannot be represented as `u64`.
///
/// Prefer this over [`current_time_millis`] wherever the caller can propagate
/// or act on a broken clock.
///
/// # Errors
///
/// - [`TimeError::ClockBeforeEpoch`] when the system clock is set before
///   1970-01-01.
/// - [`TimeError::MillisOverflow`] when the millisecond count exceeds
///   `u64::MAX` (roughly 584 million years after the epoch).
///
/// # Determinism
///
/// Same caveats as [`current_time_millis`]: wall clock, non-monotonic, not
/// reproducible. Never call it on a deterministic or replay-critical path.
pub fn try_current_time_millis() -> Result<u64, TimeError> {
    system_time_to_millis(SystemTime::now())
}

/// Latches so each fallback is logged once per process, not once per call.
static WARNED_BEFORE_EPOCH: AtomicBool = AtomicBool::new(false);
static WARNED_OVERFLOW: AtomicBool = AtomicBool::new(false);

/// Maps a [`TimeError`] to the documented fallback of [`current_time_millis`]
/// and emits a `tracing::warn!` the first time each kind is seen.
pub(crate) fn fallback_millis(err: TimeError) -> u64 {
    let (latch, value) = match err {
        TimeError::ClockBeforeEpoch { .. } => (&WARNED_BEFORE_EPOCH, 0),
        TimeError::MillisOverflow { .. } => (&WARNED_OVERFLOW, u64::MAX),
    };
    if !latch.swap(true, Ordering::Relaxed) {
        warn!(
            error = %err,
            fallback_ms = value,
            "wall clock not representable as u64 ms since the UNIX epoch; \
             using fallback (logged once)"
        );
    }
    value
}

/// Returns the current wall-clock time in milliseconds since the UNIX epoch.
///
/// Infallible wrapper over [`try_current_time_millis`] for callers whose
/// contract has no error channel (for example
/// [`Clock::now_millis`](crate::Clock::now_millis) on
/// [`MonotonicClock`](crate::MonotonicClock), trade-event stamping in the
/// book managers, and NATS batch timestamps).
///
/// # Fallback
///
/// The conversion never truncates. When the clock cannot be represented the
/// function returns an explicit sentinel and logs a `tracing::warn!` once per
/// process for each kind:
///
/// - clock before the UNIX epoch: returns `0`;
/// - millisecond count above `u64::MAX`: returns `u64::MAX`.
///
/// Callers that need to distinguish these cases must use
/// [`try_current_time_millis`].
///
/// # Determinism
///
/// This reads the **wall clock**, so it is **non-monotonic** (it can jump
/// forward or backward on NTP steps / clock adjustments) and is **not
/// reproducible** across runs. Do **not** call it on any deterministic or
/// replay-critical path — the matching engine, sequencer, and journal must take
/// their time from the injected [`Clock`](crate::Clock) trait
/// ([`MonotonicClock`](crate::MonotonicClock) in production,
/// [`StubClock`](crate::StubClock) in tests) so replays reproduce
/// engine-assigned timestamps byte-for-byte. This helper is for logging,
/// metrics, and other non-deterministic, non-journaled uses only.
#[must_use = "the current time is returned and should be used"]
pub fn current_time_millis() -> u64 {
    try_current_time_millis().unwrap_or_else(fallback_millis)
}
