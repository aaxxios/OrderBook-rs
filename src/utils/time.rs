// panic-policy-ratchet: see #242, removed by the fix issue
#![allow(clippy::cast_possible_truncation)]

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current wall-clock time in milliseconds since the UNIX epoch.
///
/// The value is the `SystemTime::now()` UNIX-epoch duration truncated to `u64`
/// milliseconds. Returns `0` if the system clock is set before the epoch
/// (1970-01-01), which should never happen on correctly-configured systems.
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
