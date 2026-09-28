//! Order state machine for explicit lifecycle tracking.
//!
//! This module provides [`OrderStatus`], [`CancelReason`],
//! [`OrderStateTracker`], and [`OrderStateListener`] to track the full
//! lifecycle of every order from submission to terminal state.
//!
//! The tracker is an **optional** component on [`OrderBook`] — when not
//! configured, there is zero overhead on the matching hot path.
//!
//! [`OrderBook`]: super::OrderBook
//!
//! # State Transitions
//!
//! ```text
//! add_order (success, no match)    → Open
//! add_order (partial match)        → PartiallyFilled / Filled
//! add_order (rejected)             → Rejected
//! matching (resting order fills)   → PartiallyFilled → Filled
//! cancel_order                     → Cancelled { UserRequested }
//! mass_cancel_*                    → Cancelled { MassCancel* }
//! STP                              → Cancelled { SelfTradePrevention }
//! IOC/FOK insufficient liquidity   → Cancelled { InsufficientLiquidity }
//! sweep aborted by a level failure → Cancelled { MatchAborted } (filled = committed prefix)
//! ```

use super::clock::{Clock, MonotonicClock};
use super::reject_reason::RejectReason;
use dashmap::DashMap;
use pricelevel::Id;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Reason for order cancellation.
///
/// Each variant identifies the specific mechanism that triggered the
/// cancellation, enabling upstream services to provide detailed
/// notifications to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CancelReason {
    /// Cancelled by explicit user request via `cancel_order`.
    UserRequested,
    /// Cancelled by Self-Trade Prevention logic.
    SelfTradePrevention,
    /// Cancelled because the order's time-in-force expired.
    TimeInForceExpired,
    /// Cancelled by `cancel_all_orders`.
    MassCancelAll,
    /// Cancelled by `cancel_orders_by_side`.
    MassCancelBySide,
    /// Cancelled by `cancel_orders_by_user`.
    MassCancelByUser,
    /// Cancelled by `cancel_orders_by_price_range`.
    MassCancelByPriceRange,
    /// IOC or FOK order could not be fully filled.
    InsufficientLiquidity,
    /// The taker's matching sweep stopped at a price level that reported a
    /// failure (#240, `OrderBookError::MatchAborted`). `filled_quantity` is
    /// the committed prefix; the remainder was cancelled, never rested.
    /// Appended last so the positional (bincode) index of every earlier
    /// variant is unchanged.
    MatchAborted,
}

impl std::fmt::Display for CancelReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserRequested => write!(f, "user requested"),
            Self::SelfTradePrevention => write!(f, "self-trade prevention"),
            Self::TimeInForceExpired => write!(f, "time-in-force expired"),
            Self::MassCancelAll => write!(f, "mass cancel all"),
            Self::MassCancelBySide => write!(f, "mass cancel by side"),
            Self::MassCancelByUser => write!(f, "mass cancel by user"),
            Self::MassCancelByPriceRange => write!(f, "mass cancel by price range"),
            Self::InsufficientLiquidity => write!(f, "insufficient liquidity"),
            Self::MatchAborted => write!(f, "match aborted"),
        }
    }
}

/// Explicit order status for lifecycle tracking.
///
/// Every order transitions through a subset of these states. Terminal
/// states (`Filled`, `Cancelled`, `Rejected`) are retained by the
/// [`OrderStateTracker`] up to a configurable capacity for post-trade
/// queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    /// Order accepted and resting in the book, no fills yet.
    Open,

    /// Order partially filled, remainder still resting in the book.
    PartiallyFilled {
        /// Total quantity originally submitted.
        original_quantity: u64,
        /// Quantity filled so far.
        filled_quantity: u64,
    },

    /// Terminal: the order is off the book and nothing rests.
    ///
    /// Usually that is because the whole submitted quantity executed. For a
    /// two-tranche order whose hidden remainder was **discarded** it is not:
    /// `filled_quantity` is then the executed quantity only and is below the
    /// submitted total. That happens on both sides of the trade for a
    /// reserve without automatic replenishment whose visible tranche is
    /// exhausted — `pricelevel` removes the resting maker and strands its
    /// hidden tranche, and an aggressive taker's residual is discarded
    /// rather than rested. See the "Two-tranche takers" section on
    /// [`OrderBook::add_order`](super::OrderBook::add_order) for the
    /// accounting rule.
    Filled {
        /// Quantity that actually executed. Equal to the submitted total
        /// except when a two-tranche order's hidden remainder was
        /// discarded, where discarded quantity is never counted here.
        filled_quantity: u64,
    },

    /// Order cancelled (by user, STP, mass cancel, or expiry).
    Cancelled {
        /// Quantity filled before cancellation (0 if none).
        filled_quantity: u64,
        /// Reason for cancellation.
        reason: CancelReason,
    },

    /// Order rejected during validation (never entered the book).
    Rejected {
        /// Closed wire-side reject code. See [`RejectReason`].
        reason: RejectReason,
    },
}

impl OrderStatus {
    /// Returns `true` if this is a terminal state (no further transitions).
    #[must_use]
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderStatus::Filled { .. }
                | OrderStatus::Cancelled { .. }
                | OrderStatus::Rejected { .. }
        )
    }

    /// Returns `true` if the order is still active in the book.
    #[must_use]
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            OrderStatus::Open | OrderStatus::PartiallyFilled { .. }
        )
    }

    /// Returns the filled quantity, or 0 for `Open` and `Rejected`.
    #[must_use]
    #[inline]
    pub fn filled_quantity(&self) -> u64 {
        match self {
            OrderStatus::Open => 0,
            OrderStatus::PartiallyFilled {
                filled_quantity, ..
            } => *filled_quantity,
            OrderStatus::Filled { filled_quantity } => *filled_quantity,
            OrderStatus::Cancelled {
                filled_quantity, ..
            } => *filled_quantity,
            OrderStatus::Rejected { .. } => 0,
        }
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderStatus::Open => write!(f, "Open"),
            OrderStatus::PartiallyFilled {
                original_quantity,
                filled_quantity,
            } => write!(f, "PartiallyFilled({filled_quantity}/{original_quantity})"),
            OrderStatus::Filled { filled_quantity } => {
                write!(f, "Filled({filled_quantity})")
            }
            OrderStatus::Cancelled {
                filled_quantity,
                reason,
            } => write!(f, "Cancelled({reason}, filled={filled_quantity})"),
            OrderStatus::Rejected { reason } => write!(f, "Rejected({reason})"),
        }
    }
}

/// Callback invoked on every order state transition.
///
/// The listener receives the order ID, the previous status, and the new
/// status. Listeners are called synchronously from the thread performing
/// the book operation and must not block.
///
/// # Arguments
///
/// * `order_id` — the order whose status changed
/// * `old_status` — the previous status (or the new status if this is the
///   first transition, i.e., `Open` or `Rejected`)
/// * `new_status` — the status after the transition
///
/// # Re-entrancy contract (#209, #225)
///
/// Transitions are recorded from inside the book operation that caused
/// them, so the listener may fire while the book's submit gate is held —
/// and usually does. (The exception is the kill-switch rejection recorded
/// by `check_kill_switch_or_reject`, which the `submit_market_order`
/// family runs before taking the gate at all.) Since #225 that hold is
/// exclusive for fill-or-kill submits and for self-trade-prevention
/// relevant submits and matching-capable modifies. Like
/// [`TradeListener`](crate::orderbook::trade::TradeListener), it must
/// never call back into the same `OrderBook`'s mutating API (add / submit
/// / cancel / update / mass cancel / market sweeps) on the invoking
/// thread: the gate is not reentrant, so the nested acquisition **may**
/// deadlock — a nested shared acquisition of `std::sync::RwLock` is
/// unspecified and may succeed, panic or block — and it **always**
/// deadlocks when the gate is held exclusively. The prohibition is
/// absolute: a listener that happens to work today on an `STPMode::None`
/// book will hang the moment STP is enabled. Hand the transition off to a
/// queue or channel instead.
pub type OrderStateListener = Arc<dyn Fn(Id, &OrderStatus, &OrderStatus) + Send + Sync>;

/// Default number of terminal-state entries to retain before eviction.
const DEFAULT_RETENTION_CAPACITY: usize = 10_000;

/// Thread-safe tracker for order lifecycle states.
///
/// Stores the current [`OrderStatus`] for every order that has been
/// submitted to the book. Terminal states (`Filled`, `Cancelled`,
/// `Rejected`) are retained up to a configurable capacity (default 10,000);
/// when the limit is exceeded, the oldest terminal entries are evicted (FIFO).
///
/// # Thread Safety
///
/// Uses [`DashMap`] for lock-free concurrent reads and writes. The
/// terminal-state eviction queue uses a [`Mutex`]-protected
/// [`VecDeque`]; this is acceptable because eviction only happens on
/// terminal transitions (not on the matching hot path).
///
/// # Example
///
/// ```
/// use orderbook_rs::orderbook::order_state::OrderStateTracker;
///
/// let tracker = OrderStateTracker::new();
/// assert_eq!(tracker.len(), 0);
/// ```
pub struct OrderStateTracker {
    /// Current status of each tracked order.
    states: DashMap<Id, OrderStatus>,
    /// Timestamped transition history per order: `(timestamp_ms, status)`.
    ///
    /// Timestamps are the millisecond values returned by the installed
    /// [`Clock`]. History grows linearly with transitions for each order
    /// (e.g. many partial fills). Entries are evicted together with
    /// their state both by capacity-based eviction in
    /// [`enqueue_terminal`](Self::enqueue_terminal) and by
    /// [`purge_terminal_older_than`](Self::purge_terminal_older_than).
    history: DashMap<Id, Vec<(u64, OrderStatus)>>,
    /// FIFO queue of terminal-state order IDs for eviction.
    terminal_queue: Mutex<VecDeque<Id>>,
    /// Maximum number of terminal-state entries to retain.
    retention_capacity: usize,
    /// Optional listener invoked on every state transition.
    listener: Option<OrderStateListener>,
    /// Pluggable source of millisecond timestamps used when recording
    /// transition history and when computing cutoffs for
    /// [`purge_terminal_older_than`](Self::purge_terminal_older_than).
    /// Defaults to [`MonotonicClock`]; tests and sequencer replay can
    /// inject a [`super::clock::StubClock`] via [`Self::with_clock`] or
    /// [`Self::with_capacity_and_clock`].
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for OrderStateTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderStateTracker")
            .field("tracked_orders", &self.states.len())
            .field("retention_capacity", &self.retention_capacity)
            .field("has_listener", &self.listener.is_some())
            .finish()
    }
}

impl Default for OrderStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderStateTracker {
    /// Create a new tracker with default retention capacity (10,000).
    ///
    /// The tracker uses a [`MonotonicClock`] to stamp transition history.
    /// Use [`Self::with_clock`] to inject a different [`Clock`]
    /// implementation (e.g. a
    /// [`super::clock::StubClock`] for deterministic tests).
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(MonotonicClock) as Arc<dyn Clock>)
    }

    /// Create a new tracker with default retention capacity and a
    /// caller-provided [`Clock`] implementation.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            states: DashMap::new(),
            history: DashMap::new(),
            terminal_queue: Mutex::new(VecDeque::new()),
            retention_capacity: DEFAULT_RETENTION_CAPACITY,
            listener: None,
            clock,
        }
    }

    /// Create a new tracker with a custom retention capacity.
    ///
    /// # Arguments
    ///
    /// * `retention_capacity` — maximum number of terminal-state entries
    ///   to retain. When exceeded, the oldest entries are evicted.
    #[must_use]
    pub fn with_capacity(retention_capacity: usize) -> Self {
        Self::with_capacity_and_clock(
            retention_capacity,
            Arc::new(MonotonicClock) as Arc<dyn Clock>,
        )
    }

    /// Create a new tracker with a custom retention capacity and a
    /// caller-provided [`Clock`] implementation.
    #[must_use]
    pub fn with_capacity_and_clock(retention_capacity: usize, clock: Arc<dyn Clock>) -> Self {
        Self {
            states: DashMap::new(),
            history: DashMap::new(),
            terminal_queue: Mutex::new(VecDeque::new()),
            retention_capacity,
            listener: None,
            clock,
        }
    }

    /// Set the listener that will be invoked on every state transition.
    ///
    /// Only one listener is supported. Setting a new listener replaces
    /// the previous one.
    pub fn set_listener(&mut self, listener: OrderStateListener) {
        self.listener = Some(listener);
    }

    /// Returns the current status of an order, or `None` if unknown.
    #[must_use]
    pub fn get(&self, order_id: Id) -> Option<OrderStatus> {
        self.states
            .get(&order_id)
            .map(|entry| entry.value().clone())
    }

    /// Returns the number of tracked orders (active + retained terminal).
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Returns `true` if no orders are being tracked.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Record a new status for an order.
    ///
    /// If the order already has a status, the listener (if any) is called
    /// with both old and new. If this is the first status for the order,
    /// the listener receives the new status as both old and new.
    ///
    /// Terminal states trigger eviction of the oldest terminal entries
    /// when `retention_capacity` is exceeded.
    pub fn transition(&self, order_id: Id, new_status: OrderStatus) {
        let old_status = self
            .states
            .get(&order_id)
            .map(|entry| entry.value().clone());

        self.states.insert(order_id, new_status.clone());

        // Record timestamped history. The timestamp is in milliseconds,
        // sourced from the installed [`Clock`] (wall-clock in production,
        // logical counter under replay / tests).
        let ts = self.clock.now_millis().as_u64();
        self.history
            .entry(order_id)
            .or_default()
            .push((ts, new_status.clone()));

        // Notify listener
        if let Some(ref listener) = self.listener {
            let old = old_status.as_ref().unwrap_or(&new_status);
            listener(order_id, old, &new_status);
        }

        // Track terminal states for eviction
        if new_status.is_terminal() {
            self.enqueue_terminal(order_id);
        }
    }

    /// Lock the terminal-id eviction queue, recovering it if poisoned
    /// (#250).
    ///
    /// The queue is only an eviction *hint*: a `VecDeque<Id>` of terminal
    /// ids in arrival order. Every id popped from it is re-checked against
    /// `states` (see [`Self::evict_if_terminal`]) before anything is
    /// removed, and no caller-supplied code runs while the guard is held.
    /// A thread that panicked while holding the lock could therefore only
    /// have left an id pushed or popped — at worst one entry is retained a
    /// little longer or evicted in a slightly different order — never an
    /// active order removed or a structurally broken queue. Recovering with
    /// [`PoisonError::into_inner`](std::sync::PoisonError::into_inner) and
    /// clearing the poison keeps retention bounded; skipping the eviction
    /// on poison (the previous behaviour) let the tracker grow without
    /// bound.
    fn lock_terminal_queue(&self) -> std::sync::MutexGuard<'_, VecDeque<Id>> {
        match self.terminal_queue.lock() {
            Ok(queue) => queue,
            Err(poisoned) => {
                tracing::warn!(
                    "order-state terminal queue mutex was poisoned; recovering (the queue is an eviction hint, re-checked per id)"
                );
                self.terminal_queue.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    /// Remove `order_id`'s state and history only if its state is still
    /// terminal (#250).
    ///
    /// The check and the removal are one atomic [`DashMap::remove_if`] on
    /// `states`, so a concurrent [`Self::transition`] that re-activates the
    /// id (an id reused after a terminal state) can never be evicted
    /// between a separate "is it terminal?" read and the removal — the
    /// get / drop / remove sequence this replaces had exactly that window.
    ///
    /// The history is then removed with its own atomic `remove_if`, only
    /// while its last entry is still terminal. `transition` inserts the new
    /// state before appending to the history, so a concurrent
    /// re-activation that has already appended keeps its history; one that
    /// has not yet appended starts a fresh history (its `or_default`),
    /// losing only the evicted lifecycle's entries. Each map is locked on
    /// its own — no guard of one is held while taking the other — so this
    /// cannot deadlock against `transition` or `purge_terminal_older_than`.
    ///
    /// Returns `true` when the state entry was removed.
    fn evict_if_terminal(&self, order_id: &Id) -> bool {
        let removed = self
            .states
            .remove_if(order_id, |_, status| status.is_terminal())
            .is_some();
        if removed {
            self.history.remove_if(order_id, |_, history| {
                history
                    .last()
                    .is_none_or(|(_, status)| status.is_terminal())
            });
        }
        removed
    }

    /// Add a terminal order ID to the eviction queue and evict if needed.
    fn enqueue_terminal(&self, order_id: Id) {
        let mut queue = self.lock_terminal_queue();
        queue.push_back(order_id);
        while queue.len() > self.retention_capacity {
            let Some(evicted_id) = queue.pop_front() else {
                break;
            };
            // Only evict if still in terminal state (not overwritten).
            self.evict_if_terminal(&evicted_id);
        }
    }

    /// Returns the full transition history for an order.
    ///
    /// Each entry is a `(timestamp_ms, OrderStatus)` pair in chronological
    /// order. Timestamps come from the installed [`Clock`]
    /// ([`MonotonicClock`] by default). Returns `None` if the order ID
    /// was never submitted.
    ///
    /// # Examples
    ///
    /// ```
    /// use orderbook_rs::orderbook::order_state::{OrderStateTracker, OrderStatus};
    /// use pricelevel::Id;
    /// use uuid::Uuid;
    ///
    /// let tracker = OrderStateTracker::new();
    /// let id = Id::from_uuid(Uuid::new_v4());
    /// tracker.transition(id, OrderStatus::Open);
    /// let history = tracker.get_history(id);
    /// assert!(history.is_some());
    /// assert_eq!(history.as_ref().map(|h| h.len()), Some(1));
    /// ```
    #[must_use]
    pub fn get_history(&self, order_id: Id) -> Option<Vec<(u64, OrderStatus)>> {
        self.history
            .get(&order_id)
            .map(|entry| entry.value().clone())
    }

    /// Returns the number of orders currently in an active state
    /// (`Open` or `PartiallyFilled`).
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.states.iter().filter(|e| e.value().is_active()).count()
    }

    /// Returns the number of orders currently in a terminal state
    /// (`Filled`, `Cancelled`, or `Rejected`).
    #[must_use]
    pub fn terminal_count(&self) -> usize {
        self.states
            .iter()
            .filter(|e| e.value().is_terminal())
            .count()
    }

    /// Remove all terminal-state entries whose last transition is older
    /// than `older_than` ago.
    ///
    /// Active orders (`Open`, `PartiallyFilled`) are never purged.
    /// This is useful for bounded memory management in long-running
    /// processes.
    ///
    /// # Arguments
    ///
    /// * `older_than` — entries with a last-transition timestamp older
    ///   than `now - older_than` (milliseconds, per the installed
    ///   [`Clock`]) are removed.
    ///
    /// # Returns
    ///
    /// The number of entries purged.
    ///
    /// When `older_than` reaches back before timestamp `0` of the installed
    /// clock (or does not fit `u64` milliseconds), no entry can be that old
    /// and nothing is purged. The cutoff used to be clamped to `0` instead
    /// (#250), which purged entries stamped at exactly `0`.
    ///
    /// Each removal re-checks the entry is still terminal atomically (a
    /// `DashMap::remove_if`), so an id re-activated concurrently is kept and
    /// not counted.
    pub fn purge_terminal_older_than(&self, older_than: Duration) -> usize {
        let now_ms = self.clock.now_millis().as_u64();
        let Some(cutoff) = u64::try_from(older_than.as_millis())
            .ok()
            .and_then(|older_ms| now_ms.checked_sub(older_ms))
        else {
            return 0;
        };

        // Collect IDs to remove (avoid holding DashMap iterators during mutation)
        let to_remove: Vec<Id> = self
            .states
            .iter()
            .filter_map(|entry| {
                let id = *entry.key();
                let status = entry.value();
                if !status.is_terminal() {
                    return None;
                }
                // Check the last history entry's timestamp. The test
                // contract is that `older_than = 0` removes every terminal
                // entry — so the comparison is `<=` rather than `<` to
                // handle the degenerate case where `ts == cutoff` under
                // millisecond resolution.
                let is_old = self
                    .history
                    .get(&id)
                    .and_then(|h| h.value().last().map(|(ts, _)| *ts <= cutoff))
                    .unwrap_or(false);
                if is_old { Some(id) } else { None }
            })
            .collect();

        // Counted without arithmetic: at most `to_remove.len()` removals.
        to_remove
            .iter()
            .filter(|id| self.evict_if_terminal(id))
            .count()
    }

    /// Remove all tracked states. Useful for testing or book reset.
    pub fn clear(&self) {
        self.states.clear();
        self.history.clear();
        // #250: a poisoned queue is recovered and cleared too, rather than
        // left holding stale ids (see `lock_terminal_queue`).
        self.lock_terminal_queue().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    #[test]
    fn test_cancel_reason_display() {
        assert_eq!(CancelReason::UserRequested.to_string(), "user requested");
        assert_eq!(
            CancelReason::SelfTradePrevention.to_string(),
            "self-trade prevention"
        );
        assert_eq!(
            CancelReason::InsufficientLiquidity.to_string(),
            "insufficient liquidity"
        );
        assert_eq!(CancelReason::MassCancelAll.to_string(), "mass cancel all");
        assert_eq!(
            CancelReason::MassCancelBySide.to_string(),
            "mass cancel by side"
        );
        assert_eq!(
            CancelReason::MassCancelByUser.to_string(),
            "mass cancel by user"
        );
        assert_eq!(
            CancelReason::MassCancelByPriceRange.to_string(),
            "mass cancel by price range"
        );
        assert_eq!(
            CancelReason::TimeInForceExpired.to_string(),
            "time-in-force expired"
        );
    }

    #[test]
    fn test_order_status_is_terminal() {
        assert!(!OrderStatus::Open.is_terminal());
        assert!(
            !OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 50
            }
            .is_terminal()
        );
        assert!(
            OrderStatus::Filled {
                filled_quantity: 100
            }
            .is_terminal()
        );
        assert!(
            OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested
            }
            .is_terminal()
        );
        assert!(
            OrderStatus::Rejected {
                reason: RejectReason::Other(0)
            }
            .is_terminal()
        );
    }

    #[test]
    fn test_order_status_is_active() {
        assert!(OrderStatus::Open.is_active());
        assert!(
            OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 50
            }
            .is_active()
        );
        assert!(
            !OrderStatus::Filled {
                filled_quantity: 100
            }
            .is_active()
        );
        assert!(
            !OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested
            }
            .is_active()
        );
    }

    #[test]
    fn test_order_status_filled_quantity() {
        assert_eq!(OrderStatus::Open.filled_quantity(), 0);
        assert_eq!(
            OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 30
            }
            .filled_quantity(),
            30
        );
        assert_eq!(
            OrderStatus::Filled {
                filled_quantity: 100
            }
            .filled_quantity(),
            100
        );
        assert_eq!(
            OrderStatus::Cancelled {
                filled_quantity: 20,
                reason: CancelReason::UserRequested
            }
            .filled_quantity(),
            20
        );
        assert_eq!(
            OrderStatus::Rejected {
                reason: RejectReason::InvalidPrice
            }
            .filled_quantity(),
            0
        );
    }

    #[test]
    fn test_order_status_display() {
        assert_eq!(OrderStatus::Open.to_string(), "Open");
        assert_eq!(
            OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 30
            }
            .to_string(),
            "PartiallyFilled(30/100)"
        );
        assert_eq!(
            OrderStatus::Filled {
                filled_quantity: 100
            }
            .to_string(),
            "Filled(100)"
        );
        assert_eq!(
            OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested
            }
            .to_string(),
            "Cancelled(user requested, filled=0)"
        );
        assert_eq!(
            OrderStatus::Rejected {
                reason: RejectReason::InvalidPrice
            }
            .to_string(),
            "Rejected(invalid price)"
        );
    }

    #[test]
    fn test_tracker_new_is_empty() {
        let tracker = OrderStateTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_tracker_transition_and_get() {
        let tracker = OrderStateTracker::new();
        let id = new_id();

        tracker.transition(id, OrderStatus::Open);
        let status = tracker.get(id);
        assert!(status.is_some());
        assert_eq!(status, Some(OrderStatus::Open));
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_tracker_lifecycle_open_to_filled() {
        let tracker = OrderStateTracker::new();
        let id = new_id();

        tracker.transition(id, OrderStatus::Open);
        tracker.transition(
            id,
            OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 50,
            },
        );
        tracker.transition(
            id,
            OrderStatus::Filled {
                filled_quantity: 100,
            },
        );

        let status = tracker.get(id);
        assert_eq!(
            status,
            Some(OrderStatus::Filled {
                filled_quantity: 100
            })
        );
    }

    #[test]
    fn test_tracker_lifecycle_open_to_cancelled() {
        let tracker = OrderStateTracker::new();
        let id = new_id();

        tracker.transition(id, OrderStatus::Open);
        tracker.transition(
            id,
            OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            },
        );

        let status = tracker.get(id);
        assert!(matches!(status, Some(OrderStatus::Cancelled { .. })));
    }

    #[test]
    fn test_tracker_rejected_order() {
        let tracker = OrderStateTracker::new();
        let id = new_id();

        tracker.transition(
            id,
            OrderStatus::Rejected {
                reason: RejectReason::InvalidPrice,
            },
        );

        let status = tracker.get(id);
        assert!(matches!(status, Some(OrderStatus::Rejected { .. })));
    }

    #[test]
    fn test_tracker_unknown_order_returns_none() {
        let tracker = OrderStateTracker::new();
        assert!(tracker.get(new_id()).is_none());
    }

    #[test]
    fn test_tracker_retention_evicts_oldest() {
        let tracker = OrderStateTracker::with_capacity(3);

        // Fill up with terminal states
        for _ in 0..5 {
            let id = new_id();
            tracker.transition(
                id,
                OrderStatus::Filled {
                    filled_quantity: 100,
                },
            );
        }

        // Only 3 should remain (the most recent ones)
        assert!(tracker.len() <= 3);
    }

    #[test]
    fn test_tracker_active_orders_not_evicted() {
        let tracker = OrderStateTracker::with_capacity(2);
        let active_id = new_id();

        // Add an active order
        tracker.transition(active_id, OrderStatus::Open);

        // Add terminal orders to exceed capacity
        for _ in 0..5 {
            let id = new_id();
            tracker.transition(
                id,
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::MassCancelAll,
                },
            );
        }

        // Active order should still be tracked
        assert_eq!(tracker.get(active_id), Some(OrderStatus::Open));
    }

    #[test]
    fn test_tracker_listener_fires_on_transition() {
        let mut tracker = OrderStateTracker::new();
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let transitions_clone = Arc::clone(&transitions);

        tracker.set_listener(Arc::new(move |id, old, new| {
            if let Ok(mut t) = transitions_clone.lock() {
                t.push((id, old.clone(), new.clone()));
            }
        }));

        let id = new_id();
        tracker.transition(id, OrderStatus::Open);
        tracker.transition(
            id,
            OrderStatus::Filled {
                filled_quantity: 50,
            },
        );

        let t = transitions.lock();
        assert!(t.is_ok());
        let t = t.unwrap_or_else(|_| panic!("lock"));
        assert_eq!(t.len(), 2);

        // First transition: Open → Open (no prior state, so old == new)
        assert_eq!(t[0].1, OrderStatus::Open);
        assert_eq!(t[0].2, OrderStatus::Open);

        // Second transition: Open → Filled
        assert_eq!(t[1].1, OrderStatus::Open);
        assert_eq!(
            t[1].2,
            OrderStatus::Filled {
                filled_quantity: 50
            }
        );
    }

    #[test]
    fn test_tracker_clear() {
        let tracker = OrderStateTracker::new();
        let id = new_id();
        tracker.transition(id, OrderStatus::Open);
        assert!(!tracker.is_empty());

        tracker.clear();
        assert!(tracker.is_empty());
        assert!(tracker.get(id).is_none());
    }

    #[test]
    fn test_tracker_concurrent_access() {
        use std::thread;

        let tracker = Arc::new(OrderStateTracker::new());
        let mut handles = Vec::new();

        for _ in 0..10 {
            let t = Arc::clone(&tracker);
            let handle = thread::spawn(move || {
                for _ in 0..100 {
                    let id = new_id();
                    t.transition(id, OrderStatus::Open);
                    t.transition(
                        id,
                        OrderStatus::Filled {
                            filled_quantity: 100,
                        },
                    );
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().expect("thread panicked");
        }

        // 10 threads × 100 orders = 1000 orders, all Filled
        assert_eq!(tracker.len(), 1000);
    }

    #[test]
    fn test_order_status_serde_roundtrip() {
        let statuses = vec![
            OrderStatus::Open,
            OrderStatus::PartiallyFilled {
                original_quantity: 100,
                filled_quantity: 30,
            },
            OrderStatus::Filled {
                filled_quantity: 100,
            },
            OrderStatus::Cancelled {
                filled_quantity: 10,
                reason: CancelReason::SelfTradePrevention,
            },
            OrderStatus::Rejected {
                reason: RejectReason::InvalidPrice,
            },
        ];

        for status in &statuses {
            let json = serde_json::to_string(status);
            assert!(json.is_ok());
            let decoded: Result<OrderStatus, _> = serde_json::from_str(&json.unwrap_or_default());
            assert!(decoded.is_ok());
            assert_eq!(&decoded.unwrap_or(OrderStatus::Open), status);
        }
    }

    #[test]
    fn test_cancel_reason_serde_roundtrip() {
        let reasons = vec![
            CancelReason::UserRequested,
            CancelReason::SelfTradePrevention,
            CancelReason::TimeInForceExpired,
            CancelReason::MassCancelAll,
            CancelReason::MassCancelBySide,
            CancelReason::MassCancelByUser,
            CancelReason::MassCancelByPriceRange,
            CancelReason::InsufficientLiquidity,
        ];

        for reason in &reasons {
            let json = serde_json::to_string(reason);
            assert!(json.is_ok());
            let decoded: Result<CancelReason, _> = serde_json::from_str(&json.unwrap_or_default());
            assert!(decoded.is_ok());
            assert_eq!(&decoded.unwrap_or(CancelReason::UserRequested), reason);
        }
    }

    /// #250: a poisoned terminal-queue mutex is recovered, so eviction keeps
    /// bounding retention (it used to be skipped for good) and `clear`
    /// still empties the queue.
    #[test]
    fn test_tracker_poisoned_queue_still_evicts() {
        let tracker = Arc::new(OrderStateTracker::with_capacity(2));
        let poisoner = Arc::clone(&tracker);
        let joined = std::thread::spawn(move || {
            let _guard = poisoner.terminal_queue.lock();
            panic!("poison the terminal queue");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread panicked");
        assert!(tracker.terminal_queue.is_poisoned(), "mutex is poisoned");

        for _ in 0..6 {
            tracker.transition(new_id(), OrderStatus::Filled { filled_quantity: 1 });
        }
        assert_eq!(tracker.len(), 2, "retention still bounded after poison");
        assert!(
            !tracker.terminal_queue.is_poisoned(),
            "the poison is cleared on recovery"
        );

        tracker.clear();
        assert!(tracker.is_empty());
        assert!(
            tracker.lock_terminal_queue().is_empty(),
            "clear empties the recovered queue"
        );
    }

    /// #250: eviction removes a queued id only while its state is still
    /// terminal, and keeps the history of a re-activated id.
    #[test]
    fn test_tracker_evict_if_terminal_keeps_reactivated_id() {
        let tracker = OrderStateTracker::with_capacity(10);
        let id = new_id();
        tracker.transition(
            id,
            OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            },
        );
        tracker.transition(id, OrderStatus::Open);

        assert!(!tracker.evict_if_terminal(&id), "an active id is kept");
        assert_eq!(tracker.get(id), Some(OrderStatus::Open));
        assert_eq!(
            tracker.get_history(id).map(|history| history.len()),
            Some(2),
            "history kept"
        );

        tracker.transition(id, OrderStatus::Filled { filled_quantity: 3 });
        assert!(tracker.evict_if_terminal(&id), "a terminal id is evicted");
        assert_eq!(tracker.get(id), None);
        assert_eq!(tracker.get_history(id), None, "terminal history removed");
    }

    /// #250: concurrent re-activation vs eviction of the same id. With the
    /// former get / drop / remove sequence the evictor could remove the id
    /// after the re-activating `transition` had overwritten its terminal
    /// state; `remove_if` makes the check and the removal atomic, so the
    /// final `Open` always survives.
    #[test]
    fn test_tracker_eviction_never_removes_reactivated_state_under_contention() {
        use std::sync::Barrier;

        const ROUNDS: usize = 20_000;
        let tracker = Arc::new(OrderStateTracker::with_capacity(1));
        let id = new_id();
        let barrier = Arc::new(Barrier::new(2));

        let reactivator = {
            let tracker = Arc::clone(&tracker);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                // Only this thread transitions `id`, so right after its
                // `Open` returns the id must be present and active: the
                // evictor may only ever remove a terminal state.
                let mut lost = 0usize;
                for _ in 0..ROUNDS {
                    tracker.transition(id, OrderStatus::Filled { filled_quantity: 1 });
                    tracker.transition(id, OrderStatus::Open);
                    if tracker.get(id) != Some(OrderStatus::Open) {
                        lost += 1;
                    }
                }
                lost
            })
        };
        let evictor = {
            let tracker = Arc::clone(&tracker);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ROUNDS {
                    tracker.transition(new_id(), OrderStatus::Filled { filled_quantity: 1 });
                }
            })
        };
        let lost = reactivator.join().expect("reactivator finished");
        assert_eq!(lost, 0, "an active state was evicted {lost} times");
        assert!(evictor.join().is_ok(), "evictor finished");

        assert_eq!(
            tracker.get(id),
            Some(OrderStatus::Open),
            "the re-activated id is never evicted"
        );
        let last = tracker
            .get_history(id)
            .and_then(|history| history.last().map(|(_, status)| status.clone()));
        assert_eq!(last, Some(OrderStatus::Open), "its latest history survives");
    }

    /// #250: a retention window reaching back before the clock's epoch
    /// purges nothing; it used to clamp the cutoff to `0` and purge entries
    /// stamped at exactly `0`.
    #[test]
    fn test_tracker_purge_window_before_epoch_purges_nothing() {
        use super::super::clock::StubClock;

        let tracker = OrderStateTracker::with_clock(Arc::new(StubClock::starting_at(0)));
        let id = new_id();
        tracker.transition(id, OrderStatus::Filled { filled_quantity: 1 });
        assert_eq!(
            tracker.purge_terminal_older_than(Duration::from_millis(10)),
            0,
            "nothing is older than a cutoff before the epoch"
        );
        assert!(tracker.get(id).is_some());
        assert_eq!(
            tracker.purge_terminal_older_than(Duration::from_secs(u64::MAX)),
            0,
            "a window that does not fit u64 ms purges nothing"
        );
        assert_eq!(
            tracker.purge_terminal_older_than(Duration::ZERO),
            1,
            "a zero window still purges every terminal entry"
        );
        assert!(tracker.get(id).is_none());
    }
}
