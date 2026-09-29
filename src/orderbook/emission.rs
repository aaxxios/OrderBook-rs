//! Deferred, ordered listener emission (#249).
//!
//! The trade, price-level and order-state listeners are caller-supplied
//! code. They must not run while the book's submit gate is held or while a
//! mutation is half-applied (`rules/global_rules.md`, Production Panic
//! Policy on callbacks). This module moves every listener invocation out of
//! the mutation:
//!
//! 1. **Buffer.** While a mutating entry point holds the submit gate, every
//!    event it produces is pushed into a per-call *emission scope*: a
//!    thread-local buffer bound to that book. Trades sit in a side vector
//!    so an event is a few words. Buffers are recycled (per thread, and
//!    through a small per-book lock-free pool when another thread drained
//!    them), so steady-state buffering does not allocate beyond what an
//!    event already carries (a `TradeResult`'s trade list).
//! 2. **Stamp under the gate.** Just before the gate is released the scope
//!    is committed into the book's [`EventOutbox`]: under the outbox lock
//!    (a spin flag in front of a mutex only the flag holder takes)
//!    every event that carries an `engine_seq` is stamped (the same checked
//!    mint and exhaustion handling as before). Stamping and publishing the
//!    batch in one critical section makes delivery order equal
//!    `engine_seq` order, and doing it under the gate makes it consistent
//!    with commit order: a mutation that commits after another (for
//!    example after an exclusive fill-or-kill releases the gate) always
//!    stamps after it. Then one of two things happens, still under the
//!    lock:
//!    - **direct** (the uncontended case): the queue is empty and no
//!      thread is dispatching, so the committer claims the dispatcher role
//!      and keeps its batch; every later commit queues behind it;
//!    - **queued**: the batch is appended, *not ready*, under a ticket
//!      from the committing thread's own counter.
//! 3. **Release, then dispatch.** The gate is released. A direct committer
//!    delivers its batch and then drains whatever queued meanwhile. A
//!    queued batch becomes ready with one atomic `fetch_max` on the
//!    thread's monotonic released-ticket counter (no lock); the thread then
//!    tries to become the single dispatcher (an atomic flag). The
//!    dispatcher takes the whole ready prefix of the queue under one lock,
//!    invokes the listeners in order with no lock held, and repeats. A
//!    thread that finds another dispatcher active returns immediately: its
//!    batch is delivered by the active one. A not-ready head (a batch whose
//!    owner has not released its gate yet) stops the dispatcher; the owner
//!    dispatches it once it has released the gate. The flag, the released
//!    counters and the queue's non-empty flag are accessed `SeqCst` and a
//!    dispatcher that steps down re-checks the head, so no ready batch is
//!    left without a dispatcher.

//! # Delivery guarantee
//!
//! Per book, listener invocations form one **total order consistent with
//! commit order**: events of one call are delivered in the order the engine
//! produced them (identical to the pre-#249 order for a single thread), and
//! the `engine_seq` of trade and price-level events strictly increases
//! across the whole delivered stream, including under concurrent
//! submitters. Events are delivered **after** the producing mutation has
//! committed and its submit gate is released, on whichever thread is
//! dispatching at the time: usually the submitting thread before its call
//! returns, but under concurrency a submit can return before its events
//! have been delivered by another thread's dispatcher. A listener may
//! therefore observe a book state **newer** than the event it is handed.
//!
//! # Re-entrancy
//!
//! A listener may call back into the same book (submit, cancel, modify,
//! mass cancel): the gate is no longer held. The nested call commits its
//! own batch and returns without dispatching (the calling thread is already
//! the dispatcher); its events are delivered after the current batch, by
//! the same dispatcher loop. No deadlock, no reordering.
//!
//! # Panicking listener
//!
//! Listeners must not panic. If one does, the unwind leaves the book
//! consistent (every delivered event describes an already committed
//! mutation and no book lock is held), the submit gate is not poisoned, and
//! the dispatcher role is released by a drop guard. The remaining events
//! of the batch being delivered are dropped, logged at `ERROR` and counted
//! in [`OrderBook::dropped_listener_events`]; batches the dispatcher had
//! taken but not started are put back at the head of the queue, and every
//! queued batch is delivered by the next dispatch on the book (any later
//! mutation, or [`OrderBook::flush_listener_events`]). No `catch_unwind`.
//!
//! The outbox mutex is never held across caller code, so it can only be
//! poisoned by a panic in this module's own queue bookkeeping, which has no
//! panicking operation. Poisoning is nonetheless handled: the guard is
//! recovered, the poison cleared, and delivery continues; the queue is a
//! `VecDeque` whose every mutation is a single `push_back` / `pop_front` /
//! `push_front`, so it is structurally intact at any unwind point.
//!
//! # Limit
//!
//! Scopes nest per book: a gated call on another book made by caller code
//! that runs inside a mutation (a `Clock`, `T::clone`) opens that book's
//! scope on top of the current one and delivers its events after its own
//! gate is released (but still inside the outer book's mutation, on this
//! thread: its listeners must not drive the outer book). The released-
//! ticket counter is per thread, so the inner release can make the outer
//! scope's early-committed batches ready before the outer gate is
//! released; they still describe committed mutations and keep their
//! order.

use super::book::OrderBook;
use super::book_change_event::PriceLevelChangedEvent;
use super::order_state::OrderStatus;
use super::trade::TradeResult;
use crossbeam::queue::ArrayQueue;
use pricelevel::Id;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Backoff rounds on the outbox's spin flag before a waiter starts
/// yielding its time slice. Round `r` spins `2^min(r, 6)` times.
const FLAG_SPIN_ROUNDS: u32 = 12;

/// Drained event buffers a book keeps for reuse (#249). Under concurrent
/// submitters a committer's buffer is delivered, and drained, by another
/// thread; the pool hands it back to a committer instead of freeing it and
/// allocating a new one on every call.
const BUFFER_POOL: usize = 32;

/// One buffered listener event, recorded mid-mutation and delivered after
/// commit. Kept small (no inline `TradeResult`) so buffering a price-level
/// or order-state event moves a few words, not a whole trade.
#[derive(Debug)]
pub(super) enum PendingEvent {
    /// A trade emission: index of its `TradeResult` in the buffer's
    /// `trades`; `engine_seq` is stamped at commit.
    Trade(usize),
    /// A price-level change; `engine_seq` is stamped at commit.
    Level(PriceLevelChangedEvent),
    /// An order-state transition (carries no `engine_seq`).
    State {
        /// The order whose status changed.
        order_id: Id,
        /// Status before the transition (`new` for a first transition).
        old: OrderStatus,
        /// Status after the transition.
        new: OrderStatus,
    },
}

/// A buffer of pending events: the ordered event list plus the trade
/// payloads it indexes. Both vectors are recycled per thread.
#[derive(Debug, Default)]
pub(super) struct EventBuf {
    events: Vec<PendingEvent>,
    trades: Vec<TradeResult>,
}

impl EventBuf {
    /// `true` when no event is buffered.
    #[inline]
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Number of buffered events.
    #[inline]
    fn len(&self) -> usize {
        self.events.len()
    }

    /// Drop every buffered event, keeping the capacity.
    fn clear(&mut self) {
        self.events.clear();
        self.trades.clear();
    }

    /// Append a non-trade event. `Err` when the allocation is refused.
    fn push(&mut self, event: PendingEvent) -> Result<(), ()> {
        self.events.try_reserve(1).map_err(|_| ())?;
        self.events.push(event);
        Ok(())
    }

    /// Append a trade event. `Err` when the allocation is refused.
    fn push_trade(&mut self, trade: TradeResult) -> Result<(), ()> {
        self.events.try_reserve(1).map_err(|_| ())?;
        self.trades.try_reserve(1).map_err(|_| ())?;
        let index = self.trades.len();
        self.trades.push(trade);
        self.events.push(PendingEvent::Trade(index));
        Ok(())
    }
}

/// An event on its way into a scope: a small event, or a trade payload.
pub(super) enum Incoming {
    /// A price-level or order-state event.
    Event(PendingEvent),
    /// A trade.
    Trade(TradeResult),
}

/// A committed batch waiting in the outbox.
#[derive(Debug)]
struct Batch {
    /// The committing scope's ticket (every batch of one scope shares it).
    ticket: u64,
    /// The owner thread's released-ticket counter: the batch is ready once
    /// it reaches `ticket`. `None`: ready at once (committed outside any
    /// gate).
    owner_released: Option<Arc<AtomicU64>>,
    /// Stamped events, in production order.
    events: EventBuf,
}

impl Batch {
    /// `true` once the owner released the submit gate.
    #[inline]
    fn is_ready(&self) -> bool {
        self.owner_released
            .as_ref()
            .is_none_or(|released| released.load(Ordering::SeqCst) >= self.ticket)
    }
}

/// Queue state behind the outbox mutex.
#[derive(Debug, Default)]
struct OutboxState {
    /// Committed batches, in `engine_seq` / commit order.
    queue: VecDeque<Batch>,
}

/// The book's sequenced outbox and dispatcher state (#249).
///
/// Runtime-only: not part of the snapshot format, like the book's other
/// diagnostic counters.
#[derive(Debug)]
pub(super) struct EventOutbox {
    /// Taken only by the holder of `busy`, so it is never contended: a
    /// contended pthread mutex (the std implementation on macOS) parks
    /// waiters in the kernel, which dominated the contended commit path.
    state: Mutex<OutboxState>,
    /// Test-and-test-and-set spin flag in front of `state` (#249 perf):
    /// waiters spin with exponential backoff, then yield, instead of
    /// parking. The critical sections are a few atomics and one queue
    /// push or prefix pop.
    busy: AtomicBool,
    /// `true` while a thread holds the dispatcher role.
    dispatching: AtomicBool,
    /// `!state.queue.is_empty()`, written under the mutex only when it
    /// changes, read without it to skip empty dispatch attempts.
    nonempty: AtomicBool,
    /// Events dropped instead of delivered (a panicking listener's batch
    /// remainder, a scope unwound by an engine panic, a refused buffer
    /// allocation).
    dropped_events: AtomicU64,
    /// Listener invocations that unwound.
    listener_panics: AtomicU64,
    /// Drained buffers for reuse (bounded, lock-free; see [`BUFFER_POOL`]).
    pool: ArrayQueue<EventBuf>,
}

impl Default for EventOutbox {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            busy: AtomicBool::new(false),
            dispatching: AtomicBool::new(false),
            nonempty: AtomicBool::new(false),
            dropped_events: AtomicU64::new(0),
            listener_panics: AtomicU64::new(0),
            // Non-zero capacity: `ArrayQueue::new` only panics on zero.
            pool: ArrayQueue::new(BUFFER_POOL),
        }
    }
}

impl EventOutbox {
    /// Lock the queue state, recovering from poisoning (see the module
    /// docs: the state is structurally intact at every unwind point).
    fn lock(&self) -> OutboxGuard<'_> {
        let mut round = 0u32;
        loop {
            if !self.busy.load(Ordering::Relaxed)
                && self
                    .busy
                    .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
            if round < FLAG_SPIN_ROUNDS {
                let spins = 1u32.checked_shl(round.min(6)).unwrap_or(1);
                for _ in 0..spins {
                    std::hint::spin_loop();
                }
                round = round.checked_add(1).unwrap_or(round);
            } else {
                std::thread::yield_now();
            }
        }
        // Built before the mutex is taken, so the flag is released even if
        // the (unreachable) poison path below unwinds.
        let flag = FlagRelease(&self.busy);
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => self.recover(poisoned),
        };
        OutboxGuard { state, _flag: flag }
    }

    /// Recover a poisoned outbox lock (see [`Self::lock`]).
    #[cold]
    #[inline(never)]
    fn recover<'a>(
        &'a self,
        poisoned: PoisonError<MutexGuard<'a, OutboxState>>,
    ) -> MutexGuard<'a, OutboxState> {
        tracing::error!(
            "listener outbox mutex poisoned; recovering (queue state is intact at every unwind point)"
        );
        self.state.clear_poison();
        poisoned.into_inner()
    }

    /// Publish whether the queue is empty for the lock-free checks.
    #[inline]
    fn sync_queued(&self, state: &OutboxState) {
        // Called under the mutex, which orders every write: a relaxed read
        // sees the current value, and only transitions are stored, so
        // committers to a non-empty queue leave the line read-shared.
        let nonempty = !state.queue.is_empty();
        if self.nonempty.load(Ordering::Relaxed) != nonempty {
            self.nonempty.store(nonempty, Ordering::SeqCst);
        }
    }

    /// Add `n` to a diagnostic counter with checked arithmetic; at
    /// `u64::MAX` the counter stays put and the refusal is logged.
    fn bump(counter: &AtomicU64, n: u64, name: &'static str) {
        if counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(n))
            .is_err()
        {
            tracing::warn!(
                counter = name,
                "diagnostic counter at u64::MAX; not incremented"
            );
        }
    }

    /// Count `n` dropped events.
    fn count_dropped(&self, n: usize) {
        if n == 0 {
            return;
        }
        Self::bump(
            &self.dropped_events,
            u64::try_from(n).unwrap_or(u64::MAX),
            "dropped_listener_events",
        );
    }
}

/// Guard over the outbox state: the mutex guard plus the spin flag. Fields
/// drop in declaration order, so the mutex is unlocked before the flag is
/// released and the next flag holder never finds the mutex taken.
struct OutboxGuard<'a> {
    state: MutexGuard<'a, OutboxState>,
    _flag: FlagRelease<'a>,
}

impl std::ops::Deref for OutboxGuard<'_> {
    type Target = OutboxState;
    #[inline]
    fn deref(&self) -> &OutboxState {
        &self.state
    }
}

impl std::ops::DerefMut for OutboxGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut OutboxState {
        &mut self.state
    }
}

/// Releases the outbox spin flag on drop (also during an unwind).
struct FlagRelease<'a>(&'a AtomicBool);

impl Drop for FlagRelease<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Per-thread emission state. A scope is "open" while `owner` is non-null.
struct ThreadEmission {
    /// Outbox of the book whose gated call is running on this thread, or
    /// null when no scope is open.
    owner: *const EventOutbox,
    /// The open scope's events; when no scope is open, an empty buffer
    /// kept for its capacity (reused by the next scope on this thread).
    events: EventBuf,
    /// Set once the open scope committed a batch early (the ticket its
    /// batches carry).
    ticket: Option<u64>,
    /// A recycled batch buffer for this thread's dispatcher runs.
    delivering: VecDeque<Batch>,
    /// This thread's released-ticket counter, created on first use and
    /// shared (by `Arc`) with the batches it commits.
    released: Option<Arc<AtomicU64>>,
    /// Next ticket this thread hands to a queued scope. Per thread: a
    /// batch's readiness compares its ticket with its **owner's** released
    /// counter only, so tickets need to grow per thread, across every book
    /// the thread submits to (a per-book counter would let a high ticket
    /// released on one book make a lower one on another book ready before
    /// its gate is released). Starts at 1 (the released counter starts at
    /// 0, "nothing released"); at `u64::MAX` it stays put (a repeated
    /// ticket can only make a batch ready early; unreachable at one ticket
    /// per queued commit).
    next_ticket: u64,
}

thread_local! {
    static EMISSION: RefCell<ThreadEmission> = const {
        RefCell::new(ThreadEmission {
            owner: std::ptr::null(),
            events: EventBuf {
                events: Vec::new(),
                trades: Vec::new(),
            },
            ticket: None,
            delivering: VecDeque::new(),
            released: None,
            next_ticket: 1,
        })
    };
}

/// An enclosing scope saved while a nested one is open (PR #289 review).
struct SavedScope {
    owner: *const EventOutbox,
    events: EventBuf,
    ticket: Option<u64>,
}

/// Proof that [`open_scope`] opened a scope on this thread; consumed by
/// [`close_scope`]. Carries the enclosing scope, if any, which closing
/// restores.
pub(super) struct ScopeToken(Option<SavedScope>);

/// Open an emission scope for `outbox` on this thread.
///
/// Scopes nest (PR #289 review): caller code that runs inside one book's
/// mutation (a `Clock`, `T::clone`) and drives another book opens that
/// book's scope on top. The enclosing scope is saved in the token and
/// restored when the inner one closes, so the inner book buffers its own
/// events and dispatches them only after **its** gate is released, like
/// any other call. The non-nested path saves nothing. `None` only when the
/// thread-local state is unavailable (thread teardown); events then take
/// the immediate path.
fn open_scope(outbox: &EventOutbox) -> Option<ScopeToken> {
    EMISSION
        .try_with(|cell| {
            let mut tls = cell.try_borrow_mut().ok()?;
            let saved = if tls.owner.is_null() {
                None
            } else {
                Some(SavedScope {
                    owner: tls.owner,
                    events: std::mem::take(&mut tls.events),
                    ticket: tls.ticket.take(),
                })
            };
            tls.owner = std::ptr::from_ref(outbox);
            tls.ticket = None;
            if tls.events.events.capacity() == 0
                && let Some(buffer) = outbox.pool.pop()
            {
                tls.events = buffer;
            }
            Some(ScopeToken(saved))
        })
        .ok()
        .flatten()
}

/// Close the scope `token` opened, restore the enclosing one (if any), and
/// hand back the closed scope's unstamped events and early-commit ticket.
/// An empty buffer stays in place for reuse when there is nothing to
/// restore.
fn close_scope(token: ScopeToken) -> Option<(EventBuf, Option<u64>)> {
    EMISSION
        .try_with(|cell| {
            let mut tls = cell.try_borrow_mut().ok()?;
            let ticket = tls.ticket.take();
            let events = if tls.events.is_empty() {
                EventBuf::default()
            } else {
                std::mem::take(&mut tls.events)
            };
            match token.0 {
                Some(saved) => {
                    tls.owner = saved.owner;
                    tls.events = saved.events;
                    tls.ticket = saved.ticket;
                }
                None => tls.owner = std::ptr::null(),
            }
            Some((events, ticket))
        })
        .ok()
        .flatten()
}

/// Push `incoming` into the open scope of `outbox` on this thread. Gives
/// it back when no such scope is open.
fn push_in_scope(outbox: &EventOutbox, incoming: Incoming) -> Option<Incoming> {
    let mut incoming = Some(incoming);
    let pushed = EMISSION
        .try_with(|cell| {
            let Ok(mut tls) = cell.try_borrow_mut() else {
                return Ok(false);
            };
            if !std::ptr::eq(tls.owner, outbox) {
                return Ok(false);
            }
            match incoming.take() {
                Some(Incoming::Event(event)) => tls.events.push(event)?,
                Some(Incoming::Trade(trade)) => tls.events.push_trade(trade)?,
                None => {}
            }
            Ok(true)
        })
        .unwrap_or(Ok(false));
    match pushed {
        Ok(true) => None,
        Err(()) => {
            // Allocation refused: the event is dropped, loudly.
            tracing::error!("listener event buffer allocation refused; event dropped");
            outbox.count_dropped(1);
            None
        }
        Ok(false) => incoming,
    }
}

/// Take the open scope's pending events (for an early commit), leaving the
/// scope open with an empty buffer. `None` when no scope of `outbox` is
/// open on this thread.
fn take_scope_events(outbox: &EventOutbox) -> Option<(EventBuf, Option<u64>)> {
    EMISSION
        .try_with(|cell| {
            let mut tls = cell.try_borrow_mut().ok()?;
            if !std::ptr::eq(tls.owner, outbox) {
                return None;
            }
            Some((std::mem::take(&mut tls.events), tls.ticket))
        })
        .ok()
        .flatten()
}

/// Record the ticket an early commit used on the open scope of `outbox`.
fn set_scope_ticket(outbox: &EventOutbox, ticket: u64) {
    let _ = EMISSION.try_with(|cell| {
        if let Ok(mut tls) = cell.try_borrow_mut()
            && std::ptr::eq(tls.owner, outbox)
        {
            tls.ticket = Some(ticket);
        }
    });
}

/// Take an empty event buffer: this thread's recycled one when no scope is
/// open, a fresh (unallocated) one otherwise.
fn take_spare() -> EventBuf {
    EMISSION
        .try_with(|cell| {
            cell.try_borrow_mut()
                .ok()
                .filter(|tls| tls.owner.is_null())
                .map(|mut tls| std::mem::take(&mut tls.events))
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Park a drained event buffer for reuse: as this thread's buffer when
/// that one is empty and unallocated, otherwise in `outbox`'s pool (dropped
/// when the pool is full).
fn recycle(outbox: &EventOutbox, mut events: EventBuf) {
    events.clear();
    if events.events.capacity() == 0 && events.trades.capacity() == 0 {
        return;
    }
    let mut events = Some(events);
    let _ = EMISSION.try_with(|cell| {
        if let Ok(mut tls) = cell.try_borrow_mut()
            && tls.events.is_empty()
            && tls.events.events.capacity() == 0
            && let Some(buffer) = events.take()
        {
            tls.events = buffer;
        }
    });
    if let Some(buffer) = events {
        let _ = outbox.pool.push(buffer);
    }
}

/// The ticket for a queued scope batch (`existing`, or a fresh one from
/// this thread's counter) and this thread's released-ticket counter
/// (created on first use), in one thread-local access. The counter is
/// `None` only while the thread-local state is unavailable; a batch
/// committed then is ready at once and its ticket is irrelevant.
fn thread_owner(existing: Option<u64>) -> (u64, Option<Arc<AtomicU64>>) {
    EMISSION
        .try_with(|cell| {
            let Ok(mut tls) = cell.try_borrow_mut() else {
                return (existing.unwrap_or(0), None);
            };
            let ticket = existing.unwrap_or_else(|| {
                let ticket = tls.next_ticket;
                if let Some(next) = ticket.checked_add(1) {
                    tls.next_ticket = next;
                }
                ticket
            });
            let released = Arc::clone(
                tls.released
                    .get_or_insert_with(|| Arc::new(AtomicU64::new(0))),
            );
            (ticket, Some(released))
        })
        .unwrap_or((existing.unwrap_or(0), None))
}

/// Mark every batch this thread committed under `ticket` (or earlier)
/// ready: the owner has released the submit gate.
fn release_ticket(ticket: u64) {
    let _ = EMISSION.try_with(|cell| {
        if let Ok(tls) = cell.try_borrow()
            && let Some(released) = tls.released.as_ref()
        {
            released.fetch_max(ticket, Ordering::SeqCst);
        }
    });
}

/// Take this thread's recycled dispatcher buffer.
fn take_delivering() -> VecDeque<Batch> {
    EMISSION
        .try_with(|cell| {
            cell.try_borrow_mut()
                .map(|mut tls| std::mem::take(&mut tls.delivering))
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Park an emptied dispatcher buffer for reuse.
fn recycle_delivering(buffer: VecDeque<Batch>) {
    let _ = EMISSION.try_with(|cell| {
        if let Ok(mut tls) = cell.try_borrow_mut()
            && buffer.capacity() > tls.delivering.capacity()
        {
            tls.delivering = buffer;
        }
    });
}

/// What a gate scope's commit leaves to do once the gate is released.
pub(super) enum Committed {
    /// Nothing was committed.
    Nothing,
    /// Batches were queued under this ticket: release it and dispatch.
    Queued(u64),
    /// Fast path: the queue was empty and no thread was dispatching, so
    /// the committer claimed the dispatcher role under the outbox lock and
    /// kept its stamped batch. Every later commit queues behind it; the
    /// committer delivers it once the gate is released, then drains.
    Direct(EventBuf),
}

/// Emission scope held by a submit-gate guard: opened when the gate is
/// acquired on a book with a listener installed, committed just before the
/// gate is released, dispatched right after.
pub(super) struct GateEmission {
    token: ScopeToken,
}

impl GateEmission {
    /// Open the scope for `book` when it has any listener installed;
    /// `None` (no buffering at all) otherwise.
    #[inline]
    pub(super) fn open<T>(book: &OrderBook<T>) -> Option<Self> {
        if !book.has_event_listeners() {
            return None;
        }
        open_scope(&book.outbox).map(|token| Self { token })
    }

    /// Close the scope and commit its events under the (still held) gate.
    /// Returns what is left to do once the gate is released.
    ///
    /// When the thread is unwinding (an engine panic mid-mutation) the
    /// scope's uncommitted events are dropped and counted, batches it
    /// committed early are released (they describe committed trades), and
    /// nothing is dispatched: calling listeners during an unwind could
    /// abort the process.
    pub(super) fn commit<T>(self, book: &OrderBook<T>) -> Committed {
        let Some((events, ticket)) = close_scope(self.token) else {
            return Committed::Nothing;
        };
        if std::thread::panicking() {
            if !events.is_empty() {
                tracing::error!(
                    dropped = events.len(),
                    "mutation unwound mid-flight; its uncommitted listener events are dropped"
                );
                book.outbox.count_dropped(events.len());
            }
            if let Some(ticket) = ticket {
                release_ticket(ticket);
            }
            return Committed::Nothing;
        }
        if events.is_empty() {
            return ticket.map_or(Committed::Nothing, Committed::Queued);
        }
        book.commit_scope(events, ticket)
    }
}

/// Holds the dispatcher role; releases it on drop, including during an
/// unwind out of a panicking listener.
struct DispatchRole<'a> {
    outbox: &'a EventOutbox,
    /// Batches taken from the queue and not yet started.
    pending: VecDeque<Batch>,
    /// Events of the batch in flight not yet handed to a listener.
    undelivered: usize,
    /// `false` once the role was released normally.
    armed: bool,
}

impl Drop for DispatchRole<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Only reached by an unwind out of a listener. No listener is
        // called here: put the untouched batches back at the head (in
        // order), release the role, account for what is lost.
        {
            let mut state = self.outbox.lock();
            if state.queue.try_reserve(self.pending.len()).is_ok() {
                while let Some(batch) = self.pending.pop_back() {
                    state.queue.push_front(batch);
                }
            } else {
                // Allocation refused: the untouched batches are lost too.
                for batch in self.pending.drain(..) {
                    self.outbox.count_dropped(batch.events.len());
                }
            }
            self.outbox.sync_queued(&state);
        }
        self.outbox.dispatching.store(false, Ordering::SeqCst);
        EventOutbox::bump(&self.outbox.listener_panics, 1, "listener_panics");
        self.outbox.count_dropped(self.undelivered);
        tracing::error!(
            dropped = self.undelivered,
            "a listener panicked: the rest of its batch is dropped; queued batches are delivered by the next dispatch"
        );
    }
}

impl<T> OrderBook<T> {
    /// `true` when any listener (trade, price-level, order-state) is
    /// installed; gates all buffering (#249).
    #[inline]
    pub(super) fn has_event_listeners(&self) -> bool {
        self.trade_listener.is_some()
            || self.price_level_changed_listener.is_some()
            || self
                .order_state_tracker
                .as_ref()
                .is_some_and(|tracker| tracker.has_listener())
    }

    /// Buffer `event` (a price-level or order-state event) in this
    /// thread's open scope for this book, or, when none is open (an
    /// emission outside any gated entry point, for example a kill-switch
    /// rejection recorded before the gate), commit and dispatch it on its
    /// own right away. Either way it reaches the listeners through the
    /// ordered outbox.
    #[inline]
    pub(super) fn defer_event(&self, event: PendingEvent) {
        self.defer(Incoming::Event(event));
    }

    /// [`Self::defer_event`] for a trade.
    #[inline]
    pub(super) fn defer_trade(&self, trade: TradeResult) {
        self.defer(Incoming::Trade(trade));
    }

    /// Shared body of [`Self::defer_event`] / [`Self::defer_trade`].
    fn defer(&self, incoming: Incoming) {
        if let Some(incoming) = push_in_scope(&self.outbox, incoming) {
            let mut events = take_spare();
            let pushed = match incoming {
                Incoming::Event(event) => events.push(event),
                Incoming::Trade(trade) => events.push_trade(trade),
            };
            if pushed.is_err() {
                tracing::error!("listener event buffer allocation refused; event dropped");
                self.outbox.count_dropped(1);
                return;
            }
            self.commit_ready_batch(events);
            self.dispatch_listener_events();
        }
    }

    /// Commit the open scope's pending events now, followed by a trade
    /// event whose `engine_seq` the caller needs immediately (a
    /// result-returning submit, #249). Returns the minted sequence, or
    /// `None` when `engine_seq` is exhausted.
    ///
    /// `trade` is the listener's copy, `None` when no trade listener is
    /// installed (the sequence is still minted, in the same position as
    /// before #249). Without an open scope for this book the batch is
    /// committed ready and dispatched at once.
    pub(super) fn commit_with_trade_seq(&self, trade: Option<TradeResult>) -> Option<u64> {
        let (mut events, ticket, in_scope) = match take_scope_events(&self.outbox) {
            Some((events, ticket)) => (events, ticket, true),
            None => (take_spare(), None, false),
        };
        let (ticket, owner) = if in_scope {
            thread_owner(ticket)
        } else {
            (0, None)
        };
        let mut state = self.outbox.lock();
        self.stamp(&mut events);
        let seq = self.mint_event_seq();
        if let (Some(seq), Some(mut trade)) = (seq, trade) {
            trade.engine_seq = seq;
            if events.push_trade(trade).is_err() {
                tracing::error!("listener event buffer allocation refused; event dropped");
                self.outbox.count_dropped(1);
            }
        }
        self.enqueue(&mut state, ticket, owner, events);
        drop(state);
        if in_scope {
            set_scope_ticket(&self.outbox, ticket);
        } else {
            self.dispatch_listener_events();
        }
        seq
    }

    /// Stamp `events` and append them to the outbox as one batch that is
    /// ready at once (committed outside any gate).
    fn commit_ready_batch(&self, mut events: EventBuf) {
        let mut state = self.outbox.lock();
        self.stamp(&mut events);
        self.enqueue(&mut state, 0, None, events);
    }

    /// Commit a gate scope's events under the gate: stamp them and either
    /// keep them for direct delivery (queue empty, no dispatcher: the
    /// committer claims the role) or queue them under `ticket` (a fresh
    /// one when `None`), ready once this thread releases it.
    fn commit_scope(&self, mut events: EventBuf, ticket: Option<u64>) -> Committed {
        // Likely to queue (another thread dispatching, or batches waiting):
        // take the ticket and the readiness counter before the lock,
        // keeping the critical section short under contention.
        let mut owned = if ticket.is_some()
            || self.outbox.dispatching.load(Ordering::Relaxed)
            || self.outbox.nonempty.load(Ordering::Relaxed)
        {
            Some(thread_owner(ticket))
        } else {
            None
        };
        let mut state = self.outbox.lock();
        self.stamp(&mut events);
        if events.is_empty() {
            drop(state);
            recycle(&self.outbox, events);
            return ticket.map_or(Committed::Nothing, Committed::Queued);
        }
        if ticket.is_none() && state.queue.is_empty() && self.claim_dispatcher() {
            return Committed::Direct(events);
        }
        let (ticket, owner) = owned.take().unwrap_or_else(|| thread_owner(ticket));
        self.enqueue(&mut state, ticket, owner, events);
        Committed::Queued(ticket)
    }

    /// Try to take the dispatcher role. Reads the flag first so a thread
    /// that finds a dispatcher active does not take the flag's cache line
    /// exclusive (a failed compare-exchange would). `SeqCst` like every
    /// access in the stepping-down protocol.
    #[inline]
    fn claim_dispatcher(&self) -> bool {
        !self.outbox.dispatching.load(Ordering::SeqCst)
            && self
                .outbox
                .dispatching
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }

    /// Mint `engine_seq` for every event that carries one, in order. With
    /// the counter exhausted such events are suppressed (the first
    /// exhaustion is logged once by [`Self::mint_event_seq`]), exactly as
    /// immediate emission did; order-state events carry no sequence and
    /// are kept. Called under the outbox lock, so queue order is sequence
    /// order.
    fn stamp(&self, buf: &mut EventBuf) {
        let EventBuf { events, trades } = buf;
        events.retain_mut(|event| match event {
            PendingEvent::Trade(index) => match (self.mint_event_seq(), trades.get_mut(*index)) {
                (Some(seq), Some(trade)) => {
                    trade.engine_seq = seq;
                    true
                }
                _ => false,
            },
            PendingEvent::Level(level) => match self.mint_event_seq() {
                Some(seq) => {
                    level.engine_seq = seq;
                    true
                }
                None => false,
            },
            PendingEvent::State { .. } => true,
        });
    }

    /// Append a stamped batch (no-op for an empty one, whose buffer is
    /// recycled). A refused queue allocation drops the batch, loudly.
    fn enqueue(
        &self,
        state: &mut OutboxState,
        ticket: u64,
        owner_released: Option<Arc<AtomicU64>>,
        events: EventBuf,
    ) {
        if events.is_empty() {
            recycle(&self.outbox, events);
            return;
        }
        if state.queue.try_reserve(1).is_err() {
            tracing::error!(
                dropped = events.len(),
                "listener outbox allocation refused; batch dropped"
            );
            self.outbox.count_dropped(events.len());
            return;
        }
        state.queue.push_back(Batch {
            ticket,
            owner_released,
            events,
        });
        self.outbox.sync_queued(state);
    }

    /// The gate has been released: make this thread's batches under
    /// `ticket` ready (one atomic, no lock) and dispatch.
    pub(super) fn release_and_dispatch(&self, ticket: u64) {
        release_ticket(ticket);
        self.dispatch_listener_events();
    }

    /// Deliver a [`Committed::Direct`] batch (this thread already holds the
    /// dispatcher role), then keep dispatching what queued meanwhile.
    pub(super) fn deliver_direct(&self, events: EventBuf) {
        self.run_dispatcher(Some(events));
        if self.head_ready() {
            self.dispatch_listener_events();
        }
    }

    /// Become the dispatcher if no thread is, and deliver ready batches
    /// from the head of the queue until it is empty or its head is not
    /// ready. Returns immediately when another dispatcher is active (it
    /// delivers what this thread committed).
    ///
    /// No lost wakeup: a committer publishes readiness (`SeqCst`) before it
    /// tries the flag; a dispatcher that steps down clears the flag
    /// (`SeqCst`) and then re-checks the head, so either the committer
    /// wins the flag or the stepping-down dispatcher sees its batch ready.
    fn dispatch_listener_events(&self) {
        loop {
            if !self.outbox.nonempty.load(Ordering::SeqCst) || !self.claim_dispatcher() {
                return;
            }
            self.run_dispatcher(None);
            if !self.head_ready() {
                return;
            }
        }
    }

    /// `true` when the queue's head batch is ready to deliver.
    fn head_ready(&self) -> bool {
        if !self.outbox.nonempty.load(Ordering::SeqCst) {
            return false;
        }
        self.outbox
            .lock()
            .queue
            .front()
            .is_some_and(Batch::is_ready)
    }

    /// Dispatcher body, entered holding the role: deliver `first` (a direct
    /// batch), then repeatedly take the ready prefix of the queue under one
    /// lock and deliver it with no lock held; finally release the role.
    /// The caller re-checks the head afterwards.
    fn run_dispatcher(&self, first: Option<EventBuf>) {
        let mut role = DispatchRole {
            outbox: &self.outbox,
            pending: VecDeque::new(),
            undelivered: 0,
            armed: true,
        };
        if let Some(events) = first {
            self.deliver(events, &mut role);
        }
        if self.outbox.nonempty.load(Ordering::SeqCst) {
            role.pending = take_delivering();
            loop {
                {
                    let mut state = self.outbox.lock();
                    while state.queue.front().is_some_and(Batch::is_ready)
                        && role.pending.try_reserve(1).is_ok()
                    {
                        if let Some(batch) = state.queue.pop_front() {
                            role.pending.push_back(batch);
                        }
                    }
                    self.outbox.sync_queued(&state);
                }
                if role.pending.is_empty() {
                    break;
                }
                while let Some(batch) = role.pending.pop_front() {
                    self.deliver(batch.events, &mut role);
                }
                if !self.outbox.nonempty.load(Ordering::SeqCst) {
                    break;
                }
            }
            recycle_delivering(std::mem::take(&mut role.pending));
        }
        role.armed = false;
        self.outbox.dispatching.store(false, Ordering::SeqCst);
    }

    /// Invoke the listeners for one batch, in order, with no lock held.
    fn deliver(&self, buf: EventBuf, role: &mut DispatchRole<'_>) {
        let EventBuf { mut events, trades } = buf;
        let mut pending = events.drain(..);
        while let Some(event) = pending.next() {
            // What is lost if this listener unwinds: the events after it.
            role.undelivered = pending.len();
            match event {
                PendingEvent::Trade(index) => {
                    if let (Some(listener), Some(trade)) =
                        (self.trade_listener.as_ref(), trades.get(index))
                    {
                        listener(trade);
                    }
                }
                PendingEvent::Level(level) => {
                    if let Some(listener) = self.price_level_changed_listener.as_ref() {
                        listener(level);
                    }
                }
                PendingEvent::State { order_id, old, new } => {
                    if let Some(listener) = self
                        .order_state_tracker
                        .as_ref()
                        .and_then(|tracker| tracker.listener())
                    {
                        listener(order_id, &old, &new);
                    }
                }
            }
        }
        drop(pending);
        role.undelivered = 0;
        recycle(&self.outbox, EventBuf { events, trades });
    }

    /// Deliver every listener event still queued on this book (#249).
    ///
    /// Normally a no-op: each mutating call dispatches before it returns
    /// (or hands its events to the thread already dispatching). Events stay
    /// queued only after a listener panicked while other batches were
    /// waiting; they are delivered by the next mutation, or by this call.
    /// Returns immediately when another thread is dispatching.
    ///
    /// Operational recommendation: when [`Self::listener_panics`]
    /// increases, call this once the cause is addressed (or right away) so
    /// the batches queued behind the panicking one are not left waiting
    /// for the next mutation on a quiet book.
    pub fn flush_listener_events(&self) {
        self.dispatch_listener_events();
    }

    /// Operational backlog gauge (#249): listener events committed to this
    /// book's outbox and not yet taken by the dispatcher.
    ///
    /// Near zero in steady state. It grows while the dispatching thread is
    /// inside a slow or stalled listener, since every other submitter keeps
    /// committing behind it, and after a listener panic until the next
    /// dispatch ([`Self::flush_listener_events`]). **The outbox is not
    /// bounded**: nothing drops or rejects events because the backlog is
    /// large. Keeping listeners fast (the documented caller contract) is
    /// what bounds it; alert on a growing value. Excludes the batch a
    /// dispatcher is delivering at the moment. Takes the outbox lock and
    /// walks the queue, so poll it from monitoring, not per submit.
    #[must_use]
    pub fn pending_listener_events(&self) -> usize {
        let state = self.outbox.lock();
        // Every counted event occupies memory, so the sum cannot reach
        // `usize::MAX`; the fallback only keeps the arithmetic checked.
        match state
            .queue
            .iter()
            .try_fold(0usize, |total, batch| total.checked_add(batch.events.len()))
        {
            Some(total) => total,
            None => usize::MAX,
        }
    }

    /// Listener events dropped instead of delivered since the book was
    /// built (#249): the remainder of a batch whose listener panicked, the
    /// uncommitted events of a mutation that unwound, or a refused buffer
    /// allocation. Diagnostic only; not part of the snapshot format.
    #[must_use]
    #[inline]
    pub fn dropped_listener_events(&self) -> u64 {
        self.outbox.dropped_events.load(Ordering::Relaxed)
    }

    /// Listener invocations that panicked since the book was built (#249).
    /// Diagnostic only; not part of the snapshot format.
    #[must_use]
    #[inline]
    pub fn listener_panics(&self) -> u64 {
        self.outbox.listener_panics.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's released counter is shared by every book it submits to,
    /// so a ticket taken after a release (on any book) must not read as
    /// released until the thread releases it (PR review of #249: per-book
    /// ticket counters broke this).
    #[test]
    fn ticket_taken_after_a_release_is_not_ready_until_released() {
        let (earlier, released) = thread_owner(None);
        let released = released.expect("thread-local state");
        release_ticket(earlier);
        let (later, _) = thread_owner(None);
        assert!(later > earlier, "a thread's tickets grow across books");
        let batch = Batch {
            ticket: later,
            owner_released: Some(Arc::clone(&released)),
            events: EventBuf::default(),
        };
        assert!(!batch.is_ready(), "not ready before its owner releases it");
        release_ticket(later);
        assert!(batch.is_ready());
        let unowned = Batch {
            ticket: u64::MAX,
            owner_released: None,
            events: EventBuf::default(),
        };
        assert!(
            unowned.is_ready(),
            "a batch committed outside a gate is ready at once"
        );
    }

    /// Buffers drained by a dispatcher go back to the book's pool, and a
    /// scope with no thread-local buffer takes one from it.
    #[test]
    fn drained_buffers_are_pooled_and_reused() {
        let outbox = EventOutbox::default();
        let mut buffer = EventBuf::default();
        buffer
            .push(PendingEvent::State {
                order_id: Id::from_u64(1),
                old: OrderStatus::Open,
                new: OrderStatus::Open,
            })
            .expect("push");
        // Occupy this thread's slot so the buffer goes to the pool.
        let token = open_scope(&outbox).expect("scope");
        push_in_scope(
            &outbox,
            Incoming::Event(PendingEvent::State {
                order_id: Id::from_u64(2),
                old: OrderStatus::Open,
                new: OrderStatus::Open,
            }),
        );
        recycle(&outbox, buffer);
        assert_eq!(outbox.pool.len(), 1, "pooled, not dropped");
        let (events, _) = close_scope(token).expect("close");
        assert_eq!(events.len(), 1);
        // This thread's buffer is now unallocated: the next scope refills
        // it from the pool.
        let token = open_scope(&outbox).expect("scope");
        assert!(outbox.pool.is_empty(), "taken from the pool");
        let _ = close_scope(token);
    }

    /// Scopes nest per book (PR #289 review): an inner book's scope buffers
    /// only its own events, and closing it restores the outer book's scope
    /// with the outer events intact.
    #[test]
    fn nested_scope_saves_and_restores_the_outer_one() {
        let outer = EventOutbox::default();
        let inner = EventOutbox::default();
        let state = |raw: u64| {
            Incoming::Event(PendingEvent::State {
                order_id: Id::from_u64(raw),
                old: OrderStatus::Open,
                new: OrderStatus::Open,
            })
        };
        let outer_token = open_scope(&outer).expect("outer scope");
        assert!(push_in_scope(&outer, state(1)).is_none());
        let inner_token = open_scope(&inner).expect("inner scope");
        assert!(push_in_scope(&inner, state(2)).is_none());
        assert!(
            push_in_scope(&outer, state(9)).is_some(),
            "the outer book is not the current scope while the inner is open"
        );
        let (inner_events, _) = close_scope(inner_token).expect("close inner");
        assert_eq!(inner_events.len(), 1);
        assert!(push_in_scope(&outer, state(3)).is_none(), "outer restored");
        let (outer_events, _) = close_scope(outer_token).expect("close outer");
        assert_eq!(outer_events.len(), 2, "events 1 and 3");
        assert!(push_in_scope(&outer, state(4)).is_some(), "no scope open");
    }
}
