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
//! trailing stop admitted (#286)    → Open (pending off book)
//! trailing stop elected            → Triggered { child_id, trigger_price } → Filled / Cancelled / Rejected
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
    /// The book could not rest the order after accepting it (#247): the
    /// price level or the per-account risk reservation refused its
    /// remainder after the sweep, or a cancel-then-add modify could neither
    /// re-add nor restore it. `filled_quantity` is what the order executed.
    /// Appended last so the positional (bincode) index of every earlier
    /// variant is unchanged.
    RestFailed,
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
            Self::RestFailed => write!(f, "rest failed"),
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

    /// A pending trailing stop was elected by a trade (#286) and is being
    /// executed as the market order `child_id`.
    ///
    /// Recorded for the **stop's** id right before its market order runs,
    /// so a listener sees the election and the link to the child: the
    /// child's trades carry the stop in `TradeResult::origin_stop_id`.
    /// Not terminal: the stop then takes its market order's terminal state
    /// (`Filled`, `Cancelled`, `Rejected`) in the same call. Appended
    /// last, so the positional (bincode) index of every earlier variant is
    /// unchanged.
    Triggered {
        /// The id of the stop's market order (see
        /// `OrderBook::stop_trigger_order_id`).
        child_id: Id,
        /// The trade price that elected the stop, in price ticks.
        trigger_price: u128,
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

    /// Returns the filled quantity, or 0 for `Open`, `Rejected` and
    /// `Triggered` (a stop's own id never fills; its market order does).
    #[must_use]
    #[inline]
    pub fn filled_quantity(&self) -> u64 {
        match self {
            OrderStatus::Open | OrderStatus::Triggered { .. } => 0,
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
            OrderStatus::Triggered {
                child_id,
                trigger_price,
            } => write!(f, "Triggered(child={child_id}, price={trigger_price})"),
        }
    }
}

/// Callback invoked on every order state transition.
///
/// The listener receives the order ID, the previous status, and the new
/// status. It must not block.
///
/// # Arguments
///
/// * `order_id` — the order whose status changed
/// * `old_status` — the previous status (or the new status if this is the
///   first transition, i.e., `Open` or `Rejected`)
/// * `new_status` — the status after the transition
///
/// # When it runs (#249)
///
/// For a tracker installed on an [`OrderBook`](crate::OrderBook), the
/// transition is **recorded** in the tracker during the book operation but
/// the listener runs only after that operation has committed and released
/// the submit gate, in the same ordered stream as the trade and
/// price-level listeners (events of one call keep the order the engine
/// produced them in). Called directly through
/// [`OrderStateTracker::transition`] on a standalone tracker, the listener
/// runs inline on the calling thread.
///
/// # Re-entrancy and obligations
///
/// Same as [`TradeListener`](crate::orderbook::trade::TradeListener):
/// re-entering the book is allowed; the listener must not panic and must
/// return quickly.
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
    /// Current status and timestamped transition history of each tracked
    /// order, in ONE map entry per id (PR #287 review of #250).
    ///
    /// Keeping both under the same `DashMap` entry makes every per-id
    /// operation atomic: a transition updates status and history under one
    /// shard lock, and an eviction removes exactly the lifecycle whose
    /// terminal status it checked — status and history together — so a
    /// concurrent transition of the same id can never be split from its
    /// history. History timestamps are the millisecond values returned by
    /// the installed [`Clock`] and grow linearly with transitions per order
    /// (e.g. many partial fills). Entries are evicted by capacity-based
    /// eviction in [`enqueue_terminal`](Self::enqueue_terminal) and by
    /// [`purge_terminal_older_than`](Self::purge_terminal_older_than).
    entries: DashMap<Id, TrackedOrder>,
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

/// One tracked order: its current status and its timestamped transition
/// history `(timestamp_ms, status)`, stored together so they are always
/// read, updated and evicted atomically.
#[derive(Debug, Clone)]
struct TrackedOrder {
    /// Current status (the last recorded transition).
    status: OrderStatus,
    /// Every recorded transition, oldest first.
    history: Vec<(u64, OrderStatus)>,
}

impl std::fmt::Debug for OrderStateTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderStateTracker")
            .field("tracked_orders", &self.entries.len())
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
            entries: DashMap::new(),
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
            entries: DashMap::new(),
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
        self.entries
            .get(&order_id)
            .map(|entry| entry.value().status.clone())
    }

    /// Returns the number of tracked orders (active + retained terminal).
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no orders are being tracked.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a new status for an order.
    ///
    /// If the order already has a status, the listener (if any) is called
    /// with both old and new. If this is the first status for the order,
    /// the listener receives the new status as both old and new.
    ///
    /// Terminal states trigger eviction of the oldest terminal entries
    /// when `retention_capacity` is exceeded.
    ///
    /// The status and the history entry are recorded under one map entry
    /// lock, so the old status handed to the listener is exactly the one
    /// this transition replaced. The clock is read, and the listener
    /// invoked, outside that lock.
    ///
    /// This standalone entry point calls the listener inline, on the
    /// calling thread, after the transition is recorded and a terminal id
    /// is queued for eviction, so a panicking listener (caller code that
    /// must not panic) leaves the tracker consistent. An [`OrderBook`](crate::OrderBook) that owns the
    /// tracker does not use it: the book records the transition and defers
    /// the listener until its mutation has committed and its submit gate is
    /// released (#249).
    pub fn transition(&self, order_id: Id, new_status: OrderStatus) {
        let old_status = self.record(order_id, &new_status);

        // Track terminal states for eviction before any caller code runs
        // (#294, as `record_transition` does): a panicking listener must
        // not leave a terminal id outside the eviction queue.
        if new_status.is_terminal() {
            self.enqueue_terminal(order_id);
        }

        // Notify listener
        if let Some(ref listener) = self.listener {
            let old = old_status.as_ref().unwrap_or(&new_status);
            listener(order_id, old, &new_status);
        }
    }

    /// Record a transition exactly like [`Self::transition`] but **without**
    /// invoking the listener (#249).
    ///
    /// Returns the `(old, new)` pair the listener must receive when one is
    /// installed (`old == new` for an order's first transition), `None`
    /// otherwise. The owning [`OrderBook`](crate::OrderBook) buffers the
    /// pair and delivers it after its mutation commits, outside the submit
    /// gate.
    pub(crate) fn record_transition(
        &self,
        order_id: Id,
        new_status: OrderStatus,
    ) -> Option<(OrderStatus, OrderStatus)> {
        let old_status = self.record(order_id, &new_status);
        if new_status.is_terminal() {
            self.enqueue_terminal(order_id);
        }
        self.listener.as_ref()?;
        let old = old_status.unwrap_or_else(|| new_status.clone());
        Some((old, new_status))
    }

    /// Undo the transition that recorded `status` for `order_id`, when it
    /// is still the latest one (#294).
    ///
    /// For a book whose rest path unwound after recording the resting
    /// state of an order its level never admitted: pops that history entry
    /// and restores the previous status, or forgets the order when it was
    /// its only transition. Reads no clock and invokes no listener (the
    /// unwinding emission scope drops the deferred event). A non-matching
    /// latest status, or no entry, is left untouched.
    pub(crate) fn withdraw_last_transition(&self, order_id: Id, status: &OrderStatus) {
        let dashmap::Entry::Occupied(mut occupied) = self.entries.entry(order_id) else {
            return;
        };
        let tracked = occupied.get_mut();
        if tracked.status != *status
            || tracked
                .history
                .last()
                .is_none_or(|(_, last)| last != status)
        {
            return;
        }
        tracked.history.pop();
        match tracked.history.last() {
            Some((_, previous)) => tracked.status = previous.clone(),
            None => {
                occupied.remove();
            }
        }
    }

    /// The installed listener, if any (#249: the owning book invokes it
    /// from its deferred dispatcher).
    #[inline]
    #[must_use]
    pub(crate) fn listener(&self) -> Option<&OrderStateListener> {
        self.listener.as_ref()
    }

    /// `true` when a listener is installed.
    #[inline]
    #[must_use]
    pub(crate) fn has_listener(&self) -> bool {
        self.listener.is_some()
    }

    /// Store `new_status` and its history entry under one map entry lock
    /// and return the status it replaced (`None` for a first transition).
    fn record(&self, order_id: Id, new_status: &OrderStatus) -> Option<OrderStatus> {
        // Timestamp in milliseconds from the installed [`Clock`]
        // (wall-clock in production, logical counter under replay / tests),
        // read before the entry lock is taken.
        let ts = self.clock.now_millis().as_u64();
        match self.entries.entry(order_id) {
            dashmap::Entry::Occupied(mut occupied) => {
                let tracked = occupied.get_mut();
                tracked.history.push((ts, new_status.clone()));
                Some(std::mem::replace(&mut tracked.status, new_status.clone()))
            }
            dashmap::Entry::Vacant(vacant) => {
                vacant.insert(TrackedOrder {
                    status: new_status.clone(),
                    history: vec![(ts, new_status.clone())],
                });
                None
            }
        }
    }

    /// Lock the terminal-id eviction queue, recovering it if poisoned
    /// (#250).
    ///
    /// The queue is only an eviction *hint*: a `VecDeque<Id>` of terminal
    /// ids in arrival order. Every id popped from it is re-checked against
    /// the tracked entry (see [`Self::evict_if_terminal`]) before anything
    /// is removed, and neither caller-supplied code nor any map lock is
    /// taken while the guard is held.
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

    /// Remove `order_id`'s entry — status and history together — only if
    /// its status is still terminal (#250, PR #287 review).
    ///
    /// One atomic [`DashMap::remove_if`] on the single per-id entry: a
    /// concurrent [`Self::transition`] that re-activates the id, or records
    /// a new lifecycle for it, either happens before (the predicate sees
    /// the new status and the entry is kept, or it is a new terminal
    /// lifecycle and is evicted whole) or after (it starts a fresh entry).
    /// No state is ever removed without its history or vice versa.
    ///
    /// Returns `true` when the entry was removed.
    fn evict_if_terminal(&self, order_id: &Id) -> bool {
        self.entries
            .remove_if(order_id, |_, tracked| tracked.status.is_terminal())
            .is_some()
    }

    /// Pop the oldest queued terminal id while the queue exceeds
    /// `retention_capacity`. The queue guard is released before returning,
    /// so the caller evicts without holding it.
    fn pop_over_capacity(&self) -> Option<Id> {
        let mut queue = self.lock_terminal_queue();
        if queue.len() > self.retention_capacity {
            queue.pop_front()
        } else {
            None
        }
    }

    /// Add a terminal order ID to the eviction queue and evict if needed.
    ///
    /// Lock discipline (PR #287 review): the queue guard is never held
    /// while a map lock is taken. Each over-capacity id is popped under the
    /// queue lock, the lock is released, and only then is the entry
    /// evicted, so eviction cannot invert lock order against
    /// [`Self::clear`] or anything else. No allocation.
    fn enqueue_terminal(&self, order_id: Id) {
        self.lock_terminal_queue().push_back(order_id);
        while let Some(evicted_id) = self.pop_over_capacity() {
            // Only evicts if still terminal (not overwritten).
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
        self.entries
            .get(&order_id)
            .map(|entry| entry.value().history.clone())
    }

    /// Returns the number of orders currently in an active state
    /// (`Open` or `PartiallyFilled`).
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.value().status.is_active())
            .count()
    }

    /// Returns the number of orders currently in a terminal state
    /// (`Filled`, `Cancelled`, or `Rejected`).
    #[must_use]
    pub fn terminal_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.value().status.is_terminal())
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
    /// Each removal re-checks, atomically with the removal (a
    /// `DashMap::remove_if`), that the entry is still terminal AND still
    /// older than the cutoff, so an id re-activated or re-terminated
    /// concurrently is kept and not counted.
    pub fn purge_terminal_older_than(&self, older_than: Duration) -> usize {
        let now_ms = self.clock.now_millis().as_u64();
        let Some(cutoff) = u64::try_from(older_than.as_millis())
            .ok()
            .and_then(|older_ms| now_ms.checked_sub(older_ms))
        else {
            return 0;
        };

        // Collect IDs to remove (avoid holding DashMap iterators during
        // mutation), then remove each one only if it still qualifies.
        let to_remove: Vec<Id> = self
            .entries
            .iter()
            .filter(|entry| is_purgeable(entry.value(), cutoff))
            .map(|entry| *entry.key())
            .collect();

        // Counted without arithmetic: at most `to_remove.len()` removals.
        to_remove
            .iter()
            .filter(|id| {
                self.entries
                    .remove_if(id, |_, tracked| is_purgeable(tracked, cutoff))
                    .is_some()
            })
            .count()
    }

    /// Remove all tracked states. Useful for testing or book reset.
    ///
    /// Takes the queue lock and the map locks one after the other, never
    /// nested, like every other tracker path (PR #287 review).
    pub fn clear(&self) {
        // #250: a poisoned queue is recovered and cleared too, rather than
        // left holding stale ids (see `lock_terminal_queue`). The guard is
        // a temporary, dropped before the map is cleared.
        self.lock_terminal_queue().clear();
        self.entries.clear();
    }
}

/// Is `tracked` a terminal entry whose last transition is at or before
/// `cutoff` (milliseconds)? The test contract is that `older_than = 0`
/// removes every terminal entry, so the comparison is `<=` rather than `<`
/// to handle `ts == cutoff` under millisecond resolution.
#[inline]
fn is_purgeable(tracked: &TrackedOrder, cutoff: u64) -> bool {
    tracked.status.is_terminal() && tracked.history.last().is_some_and(|(ts, _)| *ts <= cutoff)
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

    /// PR #287 review: eviction removes exactly the lifecycle whose terminal
    /// status it checked. One thread records successive terminal lifecycles
    /// of the same id while another floods terminal ids to force eviction
    /// (capacity 1). Only the first thread creates entries for `id`, so once
    /// its history is gone the status must be gone too: a state without its
    /// history (the split `states` / `history` removal could delete a newer
    /// lifecycle's history) is counted as a violation.
    #[test]
    fn test_tracker_eviction_never_splits_status_from_history_under_contention() {
        use std::sync::Barrier;

        const ROUNDS: usize = 20_000;
        let tracker = Arc::new(OrderStateTracker::with_capacity(1));
        let id = new_id();
        let barrier = Arc::new(Barrier::new(2));

        let lifecycles = {
            let tracker = Arc::clone(&tracker);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let mut split = 0usize;
                for round in 0..ROUNDS {
                    let status = if round % 2 == 0 {
                        OrderStatus::Filled { filled_quantity: 1 }
                    } else {
                        OrderStatus::Cancelled {
                            filled_quantity: 0,
                            reason: CancelReason::UserRequested,
                        }
                    };
                    tracker.transition(id, status);
                    // History first, then status: removal is the only
                    // concurrent change, so a missing history followed by a
                    // present status means they were removed separately.
                    let history = tracker.get_history(id);
                    let status = tracker.get(id);
                    if history.is_none() && status.is_some() {
                        split += 1;
                    }
                }
                split
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
        let split = lifecycles.join().expect("lifecycle thread finished");
        assert!(evictor.join().is_ok(), "evictor finished");
        assert_eq!(split, 0, "a status outlived its history {split} times");
    }

    /// PR #287 review: eviction and `clear` never hold the queue lock and a
    /// map lock at the same time, so running them concurrently cannot
    /// deadlock. The workload runs on a helper thread and the test fails on
    /// a timeout instead of hanging (the timeout only detects a hang).
    #[test]
    fn test_tracker_concurrent_eviction_and_clear_do_not_deadlock() {
        use std::sync::Barrier;
        use std::sync::mpsc;
        use std::time::Duration as StdDuration;

        const ROUNDS: usize = 5_000;
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let tracker = Arc::new(OrderStateTracker::with_capacity(4));
            let barrier = Arc::new(Barrier::new(3));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        tracker.transition(new_id(), OrderStatus::Filled { filled_quantity: 1 });
                    }
                }));
            }
            let clearer = {
                let tracker = Arc::clone(&tracker);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        tracker.clear();
                    }
                })
            };
            let all_joined =
                handles.into_iter().all(|h| h.join().is_ok()) && clearer.join().is_ok();
            let _ = done_tx.send(all_joined);
        });
        match done_rx.recv_timeout(StdDuration::from_secs(60)) {
            Ok(all_joined) => assert!(all_joined, "every worker finished"),
            Err(_) => panic!("concurrent eviction and clear deadlocked"),
        }
    }
}
