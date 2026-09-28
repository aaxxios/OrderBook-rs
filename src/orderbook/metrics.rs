//! Operational Prometheus-style metrics for the order book core.
//!
//! Issue #60 — feature-gated, additive observability hooks. When the
//! `metrics` feature is enabled the helpers in this module forward to
//! the `metrics` crate's global recorder; when the feature is off every
//! helper compiles down to a no-op so that call-sites in the matching
//! hot path stay unconditional and allocation-free.
//!
//! # Metrics surface
//!
//! - `orderbook_rejects_total{reason="…"}` — counter, incremented on
//!   every rejection that flows through `record_reject`. The label
//!   value is the [`RejectReason`] [`Display`] string (stable across
//!   `0.7.x`).
//! - `orderbook_depth_levels_bid` / `orderbook_depth_levels_ask` —
//!   gauges, updated on every book change to reflect the current count
//!   of distinct price levels on each side.
//! - `orderbook_trades_total` — counter, incremented exactly once per
//!   emitted trade transaction (a `MatchResult` may contain several).
//! - `orderbook_reserve_discards_total` /
//!   `orderbook_reserve_hidden_discarded_total` — counters, incremented
//!   whenever a reserve without automatic replenishment loses its hidden
//!   tranche because its visible one was exhausted (#230). The first
//!   counts orders, the second sums the hidden quantity that was dropped;
//!   neither can be derived from the other. Both sides of the trade feed
//!   them: the aggressive residual guard in `modifications.rs`
//!   (`add_order_inner`) and the maker removal in `matching.rs`, which
//!   `pricelevel` performs when a depleted non-replenishing maker leaves
//!   its level. The matching `INFO` trace carries a `path` field
//!   (`"taker"` / `"maker"`) so the two are distinguishable.
//! - `orderbook_risk_accounting_anomalies_total` — counter, incremented
//!   whenever the pre-trade risk layer detects an accounting anomaly
//!   (#243): a release larger than an account counter (double release),
//!   a fill larger than the maker's tracked remainder, or a post-trade
//!   counter increment that would overflow. Each one is also logged and
//!   counted in `RiskState::accounting_anomalies`.
//!
//! # Determinism
//!
//! Metrics emission is **out-of-band**: it does not influence matching,
//! does not allocate on the happy path, and does not cross the
//! determinism boundary. `restore_from_snapshot_package` deliberately
//! does **not** rehydrate metric counters — they are operational only
//! and live for the process lifetime.
//!
//! # Recorder boundary
//!
//! Every helper forwards to the process-wide recorder installed by the
//! caller (`metrics::set_global_recorder` or an exporter such as
//! `metrics-exporter-prometheus`). The crate never installs a recorder of
//! its own; with none installed the `metrics` crate falls back to its no-op
//! recorder. The recorder is caller-supplied code and runs synchronously on
//! the calling thread, including the matching path: it **must not panic**
//! and should return quickly (see `doc/panic-boundaries.md`, Caller-supplied
//! code obligations). How a counter accumulates `increment(n)` (wrapping,
//! saturating or checked) is the recorder's own contract; the helpers here
//! perform no integer arithmetic of their own. The `u64` to `f64` casts in
//! `record_depth` are exact for any count below 2^53 and cannot panic.
//!
//! [`RejectReason`]: crate::orderbook::reject_reason::RejectReason
//! [`Display`]: std::fmt::Display

use crate::orderbook::reject_reason::RejectReason;

/// Counter name: total order rejections, labelled by reject reason.
pub const REJECTS_TOTAL: &str = "orderbook_rejects_total";

/// Gauge name: current count of distinct bid price levels.
pub const DEPTH_LEVELS_BID: &str = "orderbook_depth_levels_bid";

/// Gauge name: current count of distinct ask price levels.
pub const DEPTH_LEVELS_ASK: &str = "orderbook_depth_levels_ask";

/// Counter name: monotonic count of every emitted trade transaction.
pub const TRADES_TOTAL: &str = "orderbook_trades_total";

/// Counter name: reserve residuals discarded for lack of automatic
/// replenishment (#230), counted in orders.
pub const RESERVE_DISCARDS_TOTAL: &str = "orderbook_reserve_discards_total";

/// Counter name: hidden quantity dropped by those discards (#230),
/// counted in quantity units.
pub const RESERVE_HIDDEN_DISCARDED_TOTAL: &str = "orderbook_reserve_hidden_discarded_total";

/// Counter name: pre-trade risk accounting anomalies (#243).
pub const RISK_ACCOUNTING_ANOMALIES_TOTAL: &str = "orderbook_risk_accounting_anomalies_total";
/// Counter name: matching sweeps aborted by a failed price level (#240).
pub const MATCH_ABORTS_TOTAL: &str = "orderbook_match_aborts_total";

/// Counter name: price levels whose committed trades could not be folded
/// into the taker's result (#240). The only path where the trade stream
/// and the book / risk / order-state streams can disagree; any non-zero
/// value needs operator attention.
pub const MATCH_FOLD_FAILURES_TOTAL: &str = "orderbook_match_fold_failures_total";

/// Counter name: books whose trade-id generator was found exhausted (#240).
/// Incremented once per book, when the book latches the condition.
pub const TRADE_IDS_EXHAUSTED_TOTAL: &str = "orderbook_trade_ids_exhausted_total";

/// Record one aborted matching sweep (#240). No-op without `metrics`.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_match_abort() {
    metrics::counter!(MATCH_ABORTS_TOTAL).increment(1);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_match_abort() {}

/// Record one refused fold of a level's committed trades (#240). No-op
/// without `metrics`.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_match_fold_failure() {
    metrics::counter!(MATCH_FOLD_FAILURES_TOTAL).increment(1);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_match_fold_failure() {}

/// Record that a book latched trade-id exhaustion (#240). Called once per
/// book. No-op without `metrics`.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_trade_ids_exhausted() {
    metrics::counter!(TRADE_IDS_EXHAUSTED_TOTAL).increment(1);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_trade_ids_exhausted() {}

/// Record an order rejection.
///
/// Increments `orderbook_rejects_total` by 1 with the
/// `reason="<RejectReason::Display>"` label. Compiles to a no-op when
/// the `metrics` feature is disabled.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_reject(reason: RejectReason) {
    let label = reason.to_string();
    metrics::counter!(REJECTS_TOTAL, "reason" => label).increment(1);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_reject(_reason: RejectReason) {}

/// Update the bid / ask depth gauges to the supplied counts.
///
/// Called from book-change emission paths. Compiles to a no-op when
/// the `metrics` feature is disabled.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_depth(bid_levels: u64, ask_levels: u64) {
    // `gauge!` accepts an `f64`; the input is a level count that
    // comfortably fits in `f64` precision for any realistic book.
    metrics::gauge!(DEPTH_LEVELS_BID).set(bid_levels as f64);
    metrics::gauge!(DEPTH_LEVELS_ASK).set(ask_levels as f64);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_depth(_bid_levels: u64, _ask_levels: u64) {}

/// Record `n` newly emitted trade transactions.
///
/// Called once per `TradeListener` callback with the number of
/// transactions in the underlying `MatchResult`. Compiles to a no-op
/// when the `metrics` feature is disabled.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_trades(n: u64) {
    if n == 0 {
        return;
    }
    metrics::counter!(TRADES_TOTAL).increment(n);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_trades(_n: u64) {}

/// Record one discarded reserve residual and the hidden quantity it
/// dropped (#230).
///
/// Increments `orderbook_reserve_discards_total` by 1 and
/// `orderbook_reserve_hidden_discarded_total` by `quantity`. Called once
/// per dropped order from each of the two paths that can drop one, both
/// requiring `auto_replenish == false` and an exhausted visible tranche:
///
/// - the aggressive residual guard in `add_order_inner`
///   (`modifications.rs`), where the taker's residual is discarded instead
///   of rested;
/// - the maker removal in `match_order_inner` (`matching.rs`), where
///   `pricelevel` takes a resting maker off its level and strands the
///   hidden depth behind it.
///
/// A zero `quantity` is not a discard and is ignored. Compiles to a no-op
/// when the `metrics` feature is disabled.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_reserve_hidden_discarded(quantity: u64) {
    if quantity == 0 {
        return;
    }
    metrics::counter!(RESERVE_DISCARDS_TOTAL).increment(1);
    metrics::counter!(RESERVE_HIDDEN_DISCARDED_TOTAL).increment(quantity);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_reserve_hidden_discarded(_quantity: u64) {}

/// Record one pre-trade risk accounting anomaly (#243).
///
/// Increments `orderbook_risk_accounting_anomalies_total` by 1. Called
/// from the risk layer's cold anomaly path only. Compiles to a no-op
/// when the `metrics` feature is disabled.
#[inline]
#[cfg(feature = "metrics")]
pub fn record_risk_accounting_anomaly() {
    metrics::counter!(RISK_ACCOUNTING_ANOMALIES_TOTAL).increment(1);
}

/// No-op when the `metrics` feature is disabled.
#[inline]
#[cfg(not(feature = "metrics"))]
pub fn record_risk_accounting_anomaly() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every call-site must compile and run without panicking
    /// regardless of feature state. The actual counter behaviour is
    /// covered by `tests/metrics/` (feature-gated).
    #[test]
    fn helpers_are_callable_unconditionally() {
        record_reject(RejectReason::KillSwitchActive);
        record_match_abort();
        record_match_fold_failure();
        record_trade_ids_exhausted();
        record_reject(RejectReason::Other(7777));
        record_depth(0, 0);
        record_depth(3, 5);
        record_trades(0);
        record_trades(4);
        record_reserve_hidden_discarded(0);
        record_reserve_hidden_discarded(20);
        record_risk_accounting_anomaly();
    }
}
