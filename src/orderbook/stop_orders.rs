//! Off-book trailing stops (#286, `special_orders`).
//!
//! A [`OrderType::TrailingStop`](pricelevel::OrderType::TrailingStop) is
//! **never** placed on a price level. It is held in the book's
//! `PendingStops` store, where it is invisible to
//! matching, depth, analytics and the level lists of every snapshot, and it
//! is driven by the book's **last trade price**:
//!
//! - **Trail.** The watermark (the order's `last_reference_price`) follows
//!   the prints in the stop's favour: a sell stop keeps the highest print
//!   seen since admission, a buy stop the lowest. The stop price follows
//!   the watermark at `trail_amount` (sell: `watermark - trail`, buy:
//!   `watermark + trail`) and only ever tightens: a sell stop never moves
//!   down, a buy stop never moves up. The watermark given at admission is
//!   taken as is.
//! - **Trigger.** A sell stop is elected by a print at or below its stop
//!   price, a buy stop by a print at or above it. The elected stop leaves
//!   the store, releases its risk reservation and executes as an
//!   immediate-or-cancel **market order** for its quantity, on its side,
//!   for its user (self-trade prevention, fees and the trade-id / notional
//!   preflights apply as for any market taker); an unexecuted remainder is
//!   cancelled. With a [`StopProtection`](crate::StopProtection) collar on
//!   the book (#302) the child is an immediate-or-cancel **limit** order
//!   instead, at the stop price moved by the collar against it (see
//!   "Protection collar" below). A stop the last trade already crosses is
//!   refused at admission and on modify (`StopWouldTrigger`): a stop only
//!   triggers on a new print.
//! - **Path.** Each sweep is evaluated as a `PrintSegment`: the price it
//!   arrived from and its first and last prints (a sweep's prints are
//!   monotonic, so these are its extremes). At the first print, then at the
//!   last, stops trail and then the print elects the stops it crosses, so a
//!   falling sweep's first print can elect a buy stop its last print would
//!   miss, and a favourable first print trails a stop before the last print
//!   is tested against it.
//! - **When.** Evaluation runs automatically, under the submit gate the
//!   mutating call already holds, right before every call that traded
//!   returns (`add_order*`, `submit_market_order*`, the `match_*` entry
//!   points, `update_order`). It is iterative: the sweep of an elected
//!   stop's child order is evaluated next (first in, first out), until no
//!   sweep is left. Each stop is elected at most once (it leaves the store
//!   when elected), so a cascade is bounded by the number of pending stops.
//! - **Order.** Stops one print elects execute in trigger order: sell stops
//!   highest stop price first, buy stops lowest first, equal prices in
//!   admission order; the side the price moved towards to reach the print
//!   goes first. Stops of an earlier print execute before those of a later
//!   one. Stops are keyed by `(price, admission sequence)` in lock-free
//!   skip lists, so every scan is ordered and independent of hashing. The
//!   child order of stop `S` carries the id
//!   [`stop_trigger_order_id`](crate::orderbook::stop_orders::stop_trigger_order_id)
//!   (UUIDv5 of the book's trade-id namespace and `S`), so a replay with
//!   the same namespace reproduces it.
//! - **Kill switch.** While it is engaged, elections are suspended (prints
//!   still trail): a protective stop is not consumed by a child order the
//!   kill switch would reject; the first print at or through its stop
//!   price after the release elects it.
//!
//! Every mutation of the store happens under the **exclusive** submit gate:
//! while a stop is pending, every call that can trade and every call that
//! targets a pending stop takes the exclusive side (see
//! `OrderBook::acquire_submit_gate_for` / `acquire_gate_for_target`);
//! post-only adds and the quantity updates and cancels of other orders
//! keep the shared side and never touch the store. The count only grows
//! under the exclusive side, so a caller that read zero pending stops under
//! the shared side keeps reading zero. With no pending stop every path
//! pays a few relaxed loads of that count (gate decision, re-check,
//! evaluation) and one per matched level (the print recorder).
//!
//! # Protection collar (#302)
//!
//! [`OrderBook::set_stop_protection`] installs a per-book collar, an
//! absolute offset in price units (like CME protection points, except that
//! the remainder is cancelled, not rested at the limit: a stop whose band
//! is exhausted leaves its position unprotected). The child of an elected
//! stop is then an immediate-or-cancel limit order at
//!
//! - `stop - collar` for a sell stop,
//! - `stop + collar` for a buy stop,
//!
//! where `stop` is the stop's current (trailed) stop price at election, not
//! the print that elected it. The child trades only at levels at or inside
//! that limit; whatever does not fill is cancelled and nothing rests. The
//! remainder's reason tells why: `Cancelled { StopProtectionBand }` when
//! the collar cut the sweep (liquidity remained beyond the limit),
//! including an empty band over a non-empty side (a gap through the
//! collar: `filled_quantity: 0`); `Cancelled { InsufficientLiquidity }`
//! when the side ran out within the band. `Triggered` carries the child's
//! `limit_price` (`None` for a market child). A band that reaches or
//! passes the representable bound (a sell collar `>=` the stop price, a buy
//! `stop + collar >= u128::MAX`) has the bound as its limit (`0` /
//! `u128::MAX`): the collar does not restrict that side. With a tick size
//! the collar must be a multiple of it, so the limit of a tick-aligned stop is tick-aligned; the
//! limit is a bound on matching, never a resting price, so it is not
//! rounded. An empty band is not counted in the `InsufficientLiquidity`
//! reject metric (a limit that does not cross is not a rejection).
//!
//! No stop child trades beyond its own band (the taker whose print elects
//! the first stop is not collared). A cascade is still unbounded in
//! length: a ladder of stops spaced one collar apart walks the book
//! `k × collar` in one call. Without a collar (the default) the child is
//! the unpriced market order of 0.14.
//! The collar is read once per elected stop: paths without a pending stop
//! never touch it.
//!
//! # Lifecycle and the link to the child order
//!
//! The order state of a pending stop is `Open`. At election it records
//! `Triggered { child_id, trigger_price, limit_price }` (the listener's
//! election event: stop id, child-order id, trigger print, the child's
//! collar limit or `None` for a market child), and the child order's
//! `TradeResult`s carry `origin_stop_id = Some(stop)`. The stop then takes
//! the terminal state of its child order: `Filled` when it executed its
//! whole quantity, `Cancelled { InsufficientLiquidity }` for an unexecuted
//! remainder (including none executed), `Cancelled { StopProtectionBand }`
//! when a collar cut it (#302), `Cancelled { SelfTradePrevention }` /
//! `Cancelled { MatchAborted }` when self-trade prevention or a failed
//! price level stopped it, and `Rejected` when it was rejected untouched
//! (trade-id generator exhausted, notional or fee not representable, or a
//! child-order id already in use: `DuplicateOrderId`). The child order's
//! own id gets a tracker entry only on the paths every taker records one
//! (an STP cancel with no fill, an abort, an untouched rejection); the
//! kill switch no longer produces one, since elections are suspended.
//!
//! The child-order id is a UUIDv5 of the book's trade-id namespace and
//! the stop id. Keep the namespace private: someone who knows it can
//! derive the id and place an order under it first, which refuses the
//! stop's child order.
//!
//! # What a stop does not protect against
//!
//! - Without a collar the market order is **unpriced**: it walks the
//!   opposite side as far as its quantity needs, and the book applies no
//!   price band to market orders. A thin or gapped book fills it far from
//!   the stop price. Configure a [`StopProtection`](crate::StopProtection)
//!   to bound it; the price of that bound is that a stop whose band is
//!   empty is cancelled unexecuted, leaving the position open.
//! - The risk layer books a pending stop at its stop price (re-booked as it
//!   trails, without enforcing limits: a trailing sell stop can lift its
//!   account above `max_notional_per_account`, and the account's later
//!   admissions are rejected until it shrinks). That booking understates a
//!   child that fills through a gap (by up to the collar with one).
//! - A cascade's **length** is bounded only by the number of pending stops:
//!   one print can run every pending stop's child within the call that
//!   printed it. A collar bounds each child's price range, not how many
//!   children run (no cascade depth limit or velocity pause).
//! - `cancel_orders_by_price_range` matches a stop by its **current**
//!   (trailed) stop price, not the price it was admitted at.

use crate::orderbook::book::OrderBook;
use crate::orderbook::error::OrderBookError;
use crate::orderbook::matching::SweepReservation;
use crate::orderbook::modifications::OrderQuantity;
use crate::orderbook::order_state::{CancelReason, OrderStatus};
use crate::orderbook::reject_reason::RejectReason;
use crossbeam::atomic::AtomicCell;
use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use pricelevel::{
    Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TakerKind, TimeInForce,
};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::{debug, trace};
use uuid::Uuid;

/// Label hashed into the name of every stop-trigger child order id (#286).
const STOP_TRIGGER_ID_LABEL: &[u8] = b"orderbook-rs/stop-trigger";

/// Shards of the pending-stop id index (#286). Every mutation of the index
/// runs under the exclusive submit gate and a book rarely holds many
/// stops, so a few shards suffice (the default is four cache-padded shards
/// per core). A power of two above one, as `DashMap::with_shard_amount`
/// requires.
const PENDING_STOP_SHARDS: usize = 4;

/// Skip-list key of a pending stop: `(price, admission sequence)`, in price
/// ticks. The sequence is unique per stop, so keys never collide.
type StopKey = (u128, u64);

/// The id of the child order an elected stop executes as (#286): a market
/// order, or an immediate-or-cancel limit order with a
/// [`StopProtection`](crate::StopProtection) collar (#302).
///
/// UUIDv5 of `namespace` (the book's trade-id namespace) over a fixed label,
/// the stop id's variant tag and its 16 id bytes. Deterministic: a replay
/// that injects the recorded namespace reproduces it, like trade ids.
#[must_use]
pub fn stop_trigger_order_id(namespace: Uuid, stop_id: Id) -> Id {
    let tag: u8 = match stop_id {
        Id::Uuid(_) => 0,
        Id::Ulid(_) => 1,
        Id::Sequential(_) => 2,
    };
    let mut name = [0u8; STOP_TRIGGER_ID_LABEL.len() + 17];
    for (slot, byte) in name.iter_mut().zip(
        STOP_TRIGGER_ID_LABEL
            .iter()
            .copied()
            .chain(std::iter::once(tag))
            .chain(stop_id.as_bytes()),
    ) {
        *slot = byte;
    }
    Id::from_uuid(Uuid::new_v5(&namespace, &name))
}

/// The trailing parameters of a stop, read off its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StopTerms {
    /// Buy or sell stop.
    pub(super) side: Side,
    /// Current stop (trigger) price, in price ticks.
    pub(super) stop: u128,
    /// Watermark (`last_reference_price`), in price ticks.
    pub(super) watermark: u128,
    /// Trail distance, in price ticks.
    pub(super) trail: u128,
}

impl StopTerms {
    /// The terms of `order`, or `None` when it is not a trailing stop.
    #[inline]
    #[must_use]
    pub(super) fn of<E>(order: &OrderType<E>) -> Option<Self> {
        match order {
            OrderType::TrailingStop {
                price,
                side,
                trail_amount,
                last_reference_price,
                ..
            } => Some(Self {
                side: *side,
                stop: price.as_u128(),
                watermark: last_reference_price.as_u128(),
                trail: u128::from(trail_amount.as_u64()),
            }),
            _ => None,
        }
    }

    /// The terms after a trade at `price`: the watermark follows the trade
    /// in the stop's favour and the stop tightens to `trail` behind it.
    /// Unchanged when the trade does not improve the watermark. A stop the
    /// trail cannot represent (a sell trail deeper than the price, a buy
    /// stop above `u128::MAX`) keeps its price; the watermark still moves.
    #[inline]
    #[must_use]
    pub(super) fn trailed(self, price: u128) -> Self {
        match self.side {
            Side::Sell if price > self.watermark => Self {
                watermark: price,
                stop: price
                    .checked_sub(self.trail)
                    .map_or(self.stop, |candidate| candidate.max(self.stop)),
                ..self
            },
            Side::Buy if price < self.watermark => Self {
                watermark: price,
                stop: price
                    .checked_add(self.trail)
                    .map_or(self.stop, |candidate| candidate.min(self.stop)),
                ..self
            },
            _ => self,
        }
    }

    /// The first inconsistency in the terms, if any: a zero trail, or a
    /// stop price on the wrong side of its own watermark (above it for a
    /// sell stop, below it for a buy stop). Trailing preserves both
    /// properties, so they hold for every pending stop.
    #[inline]
    #[must_use]
    pub(super) fn inconsistency(self) -> Option<&'static str> {
        if self.trail == 0 {
            return Some("trail amount is zero");
        }
        match self.side {
            Side::Sell if self.stop > self.watermark => {
                Some("sell stop price is above its watermark (last_reference_price)")
            }
            Side::Buy if self.stop < self.watermark => {
                Some("buy stop price is below its watermark (last_reference_price)")
            }
            _ => None,
        }
    }

    /// Whether a trade at `price` elects the stop: at or below a sell
    /// stop, at or above a buy stop.
    #[inline]
    #[must_use]
    pub(super) fn elected_by(self, price: u128) -> bool {
        match self.side {
            Side::Sell => price <= self.stop,
            Side::Buy => price >= self.stop,
        }
    }
}

/// Writes `terms`' stop price and watermark into a trailing-stop `order`.
fn apply_terms<E>(order: &mut OrderType<E>, terms: StopTerms) {
    if let OrderType::TrailingStop {
        price,
        last_reference_price,
        ..
    } = order
    {
        *price = Price::new(terms.stop);
        *last_reference_price = Price::new(terms.watermark);
    }
}

/// A pending stop: its order (current stop price and watermark included)
/// and its admission sequence, the tie-break of every ordering.
#[derive(Debug, Clone)]
pub(super) struct StopEntry {
    /// The stop as it stands, unit-converted like a resting order.
    pub(super) order: OrderType<()>,
    /// Admission sequence; unique per stop in this book.
    pub(super) seq: u64,
}

/// One side's skip lists: by stop price (election order) and by watermark
/// (trail order).
#[derive(Debug, Default)]
struct SideIndex {
    /// `(stop price, seq)`: a sell side is elected from the back (highest
    /// stop first), a buy side from the front.
    by_stop: SkipMap<StopKey, Id>,
    /// `(watermark, seq)`: a sell side trails from the front (lowest
    /// watermark first), a buy side from the back.
    by_watermark: SkipMap<StopKey, Id>,
}

impl SideIndex {
    /// Indexes a stop.
    fn insert(&self, terms: StopTerms, seq: u64, id: Id) {
        self.by_stop.insert((terms.stop, seq), id);
        self.by_watermark.insert((terms.watermark, seq), id);
    }

    /// Removes a stop's keys.
    fn remove(&self, terms: StopTerms, seq: u64) {
        self.by_stop.remove(&(terms.stop, seq));
        self.by_watermark.remove(&(terms.watermark, seq));
    }
}

/// The book's store of pending trailing stops (#286).
///
/// A book that never holds a stop pays for this field with a few relaxed
/// loads of `count` per mutating call (and one per matched level) and
/// allocates nothing: the maps are built on the
/// first admission (or restore) of a stop. Measured against main, building
/// them with every book (a `DashMap` with the default shard count plus the
/// skip-list heads, several KB) shifted the allocation pattern of books
/// that never hold a stop enough to slow contended adds by 3 to 5 % at 2 /
/// 4 threads.
///
/// Every mutation runs under the exclusive submit gate (see the module
/// docs), so the count and the maps change together; lock-free readers
/// (`get_order`, snapshots) read the id index only.
#[derive(Debug, Default)]
pub(super) struct PendingStops {
    /// The maps, built on the first stop.
    store: OnceLock<Box<StopStore>>,
    /// Number of pending stops: the relaxed load every fast path reads.
    count: AtomicUsize,
    /// The prints of the current sweep, recorded while a stop is pending
    /// (see [`PrintSegment`]). Written only under the exclusive gate.
    path: PrintPath,
}

/// One sweep's prints, as the evaluation needs them (#286): the last trade
/// price before the sweep (`prev`, the direction the price arrived from),
/// the sweep's first print and its last print. A sweep walks the book away
/// from the touch, so its prints move monotonically from `first` to `last`
/// and the extremes of the sweep are its two ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PrintSegment {
    /// Last trade price before the sweep, if the book had traded.
    pub(super) prev: Option<u128>,
    /// The sweep's first print, in price ticks.
    pub(super) first: u128,
    /// The sweep's last print, in price ticks.
    pub(super) last: u128,
}

/// Recorder of the current sweep's first print (#286). The last print is
/// the book's last trade price.
#[derive(Debug, Default)]
struct PrintPath {
    /// A print was recorded and not consumed yet.
    open: AtomicBool,
    /// The book had traded before the first recorded print.
    has_prev: AtomicBool,
    /// Last trade price before the first recorded print.
    prev: AtomicCell<u128>,
    /// The first recorded print.
    first: AtomicCell<u128>,
}

/// Reusable scratch buffers of one evaluation pass (#286), kept per thread
/// so a pass allocates nothing once warm.
#[derive(Debug, Default)]
struct StopScratch {
    /// Segments waiting to be evaluated (the call's sweep, then the sweeps
    /// of the elected stops' child orders, in execution order).
    queue: VecDeque<PrintSegment>,
    /// Stops elected by the current segment, with their trigger price.
    elected: Vec<(StopEntry, u128)>,
    /// Stops a print trailed: `(id, new stop price)`.
    moved: Vec<(Id, u128)>,
    /// Ids a print can trail.
    stale: Vec<Id>,
    /// Sell stops a print elects: `(stop price, seq, id)`.
    candidates: Vec<(u128, u64, Id)>,
}

thread_local! {
    static STOP_SCRATCH: Cell<StopScratch> = Cell::new(StopScratch::default());
}

impl StopScratch {
    /// This thread's buffers (a fresh set when unavailable, e.g. during
    /// thread teardown or a nested pass on another book).
    fn take() -> Self {
        STOP_SCRATCH
            .try_with(|cell| cell.take())
            .unwrap_or_default()
    }

    /// Hands the buffers back, emptied, for the next pass on this thread.
    fn put(mut self) {
        self.queue.clear();
        self.elected.clear();
        self.moved.clear();
        self.stale.clear();
        self.candidates.clear();
        // Nothing to do if the thread is being torn down.
        let _ = STOP_SCRATCH.try_with(|cell| cell.set(self));
    }
}

/// The maps behind [`PendingStops`]: `entries` is the id index and the
/// ownership token of a stop's id; the two [`SideIndex`]es order the stops
/// for trailing and election.
#[derive(Debug)]
struct StopStore {
    /// Stop id to its entry.
    entries: DashMap<Id, StopEntry>,
    /// Sell stops.
    sell: SideIndex,
    /// Buy stops.
    buy: SideIndex,
    /// Next admission sequence.
    next_seq: AtomicU64,
}

impl StopStore {
    /// Empty maps.
    fn new() -> Self {
        Self {
            entries: DashMap::with_shard_amount(PENDING_STOP_SHARDS),
            sell: SideIndex::default(),
            buy: SideIndex::default(),
            next_seq: AtomicU64::new(0),
        }
    }

    /// The side index of `side`.
    #[inline]
    fn index(&self, side: Side) -> &SideIndex {
        match side {
            Side::Buy => &self.buy,
            Side::Sell => &self.sell,
        }
    }

    /// Takes the next admission sequence (checked).
    fn take_seq(&self) -> Result<u64, OrderBookError> {
        self.next_seq
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |seq| {
                seq.checked_add(1)
            })
            .map_err(|_| OrderBookError::ArithmeticOverflow {
                operation: "pending stop admission sequence",
            })
    }
}

impl PendingStops {
    /// An empty store; allocates nothing.
    #[must_use]
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The maps, when a stop was ever admitted.
    #[inline]
    fn store(&self) -> Option<&StopStore> {
        self.store.get().map(|store| &**store)
    }

    /// The maps, built on first use (under the exclusive gate).
    fn store_or_init(&self) -> &StopStore {
        self.store.get_or_init(|| Box::new(StopStore::new()))
    }

    /// `true` when no stop is pending: one relaxed load.
    #[inline]
    #[must_use]
    pub(super) fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    /// Number of pending stops.
    #[inline]
    #[must_use]
    pub(super) fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Whether `id` is a pending stop.
    #[inline]
    #[must_use]
    pub(super) fn contains(&self, id: Id) -> bool {
        !self.is_empty()
            && self
                .store()
                .is_some_and(|store| store.entries.contains_key(&id))
    }

    /// A copy of the pending stop `id`.
    #[must_use]
    pub(super) fn get(&self, id: Id) -> Option<StopEntry> {
        if self.is_empty() {
            return None;
        }
        self.store()?
            .entries
            .get(&id)
            .map(|entry| entry.value().clone())
    }

    /// Adds `order` as a new pending stop, behind every stop already
    /// pending. Returns its admission sequence.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::DuplicateOrderId`] when the id is already pending,
    /// [`OrderBookError::InvalidOperation`] when `order` is not a trailing
    /// stop, [`OrderBookError::ArithmeticOverflow`] when the sequence or the
    /// count cannot advance. Nothing is changed on error.
    pub(super) fn insert(&self, order: OrderType<()>) -> Result<u64, OrderBookError> {
        let seq = self.store_or_init().take_seq()?;
        self.insert_at(order, seq)?;
        Ok(seq)
    }

    /// Adds `order` with the admission sequence `seq` (restore path).
    ///
    /// # Errors
    ///
    /// As [`Self::insert`].
    pub(super) fn insert_at(&self, order: OrderType<()>, seq: u64) -> Result<(), OrderBookError> {
        let id = order.id();
        let terms = StopTerms::of(&order).ok_or_else(|| not_a_stop(id))?;
        let next_count = self.count.load(Ordering::Relaxed).checked_add(1).ok_or(
            OrderBookError::ArithmeticOverflow {
                operation: "pending stop count",
            },
        )?;
        let store = self.store_or_init();
        match store.entries.entry(id) {
            dashmap::Entry::Occupied(_) => {
                return Err(OrderBookError::DuplicateOrderId { order_id: id });
            }
            dashmap::Entry::Vacant(slot) => {
                slot.insert(StopEntry { order, seq });
            }
        }
        store.index(terms.side).insert(terms, seq, id);
        if next_count == 1 {
            // Prints recorded while the previous stops were pending are
            // stale for this one (the path is only recorded with stops).
            self.reset_path();
        }
        // Exclusive gate: no concurrent count update.
        self.count.store(next_count, Ordering::Relaxed);
        Ok(())
    }

    /// Removes the pending stop `id` and returns it.
    pub(super) fn remove(&self, id: Id) -> Option<StopEntry> {
        let store = self.store()?;
        let (_, entry) = store.entries.remove(&id)?;
        if let Some(terms) = StopTerms::of(&entry.order) {
            store.index(terms.side).remove(terms, entry.seq);
        }
        let remaining = self.count.load(Ordering::Relaxed).checked_sub(1);
        match remaining {
            Some(remaining) => self.count.store(remaining, Ordering::Relaxed),
            None => warn_count_underflow(id),
        }
        Some(entry)
    }

    /// Replaces the pending stop `id` with `order` (a modify). With
    /// `requeue` the stop takes a fresh admission sequence (it loses its
    /// time priority, like a re-priced resting order).
    ///
    /// # Errors
    ///
    /// [`OrderBookError::OrderNotFound`] when `id` is not pending,
    /// [`OrderBookError::InvalidOperation`] when `order` is not a trailing
    /// stop with the same id, [`OrderBookError::ArithmeticOverflow`] when a
    /// fresh sequence cannot be taken. Nothing is changed on error.
    pub(super) fn replace(
        &self,
        id: Id,
        order: OrderType<()>,
        requeue: bool,
    ) -> Result<(), OrderBookError> {
        let new_terms = match StopTerms::of(&order) {
            Some(terms) if order.id() == id => terms,
            _ => return Err(not_a_stop(id)),
        };
        let Some(store) = self.store() else {
            return Err(OrderBookError::OrderNotFound(id.to_string()));
        };
        if !store.entries.contains_key(&id) {
            return Err(OrderBookError::OrderNotFound(id.to_string()));
        }
        let seq = if requeue {
            Some(store.take_seq()?)
        } else {
            None
        };
        let Some(mut entry) = store.entries.get_mut(&id) else {
            return Err(OrderBookError::OrderNotFound(id.to_string()));
        };
        if let Some(old_terms) = StopTerms::of(&entry.order) {
            store.index(old_terms.side).remove(old_terms, entry.seq);
        }
        if let Some(seq) = seq {
            entry.seq = seq;
        }
        store.index(new_terms.side).insert(new_terms, entry.seq, id);
        entry.order = order;
        Ok(())
    }

    /// Records a print of the current sweep (#286), called per matched
    /// level with the last trade price before it. Only the sweep's first
    /// print is kept (its last is the last trade price); nothing happens
    /// while no stop is pending, so a book without stops pays one relaxed
    /// load per matched level.
    #[inline]
    pub(super) fn record_print(&self, prev: Option<u128>, price: u128) {
        if self.is_empty() || self.path.open.load(Ordering::Relaxed) {
            return;
        }
        self.record_first_print(prev, price);
    }

    /// Opens the path at the sweep's first print (exclusive gate: a stop is
    /// pending, so the trading call holds it).
    #[inline(never)]
    fn record_first_print(&self, prev: Option<u128>, price: u128) {
        self.path.has_prev.store(prev.is_some(), Ordering::Relaxed);
        self.path.prev.store(prev.unwrap_or(0));
        self.path.first.store(price);
        self.path.open.store(true, Ordering::Relaxed);
    }

    /// Consumes the recorded sweep, ending at `last` (the book's last trade
    /// price), if a print was recorded since the last call.
    pub(super) fn take_path(&self, last: Option<u128>) -> Option<PrintSegment> {
        // Load first: a call that cannot trade may reach here under the
        // shared gate, and must not write (the path is only ever open
        // after a trade, which runs exclusively while a stop is pending).
        if !self.path.open.load(Ordering::Relaxed) {
            return None;
        }
        self.path.open.store(false, Ordering::Relaxed);
        let first = self.path.first.load();
        Some(PrintSegment {
            prev: self
                .path
                .has_prev
                .load(Ordering::Relaxed)
                .then(|| self.path.prev.load()),
            first,
            last: last.unwrap_or(first),
        })
    }

    /// Drops a recorded, unconsumed sweep.
    #[inline]
    pub(super) fn reset_path(&self) {
        self.path.open.store(false, Ordering::Relaxed);
    }

    /// Removes and appends to `out`, with `price` as trigger price, every
    /// stop a print at `price` elects, in trigger order: sell stops highest
    /// stop price first, buy stops lowest first, equal prices in admission
    /// order; `sells_first` picks which side goes first. `candidates` is
    /// scratch. Visits only the elected stops (skip-list prefix per side).
    fn pop_elected(
        &self,
        price: u128,
        sells_first: bool,
        candidates: &mut Vec<(u128, u64, Id)>,
        out: &mut Vec<(StopEntry, u128)>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        candidates.clear();
        // Sell stops elected: stop >= price. Reverse key order yields the
        // highest stop first but equal stops in descending seq: re-sort.
        for entry in store.sell.by_stop.iter().rev() {
            if entry.key().0 < price {
                break;
            }
            candidates.push((entry.key().0, entry.key().1, *entry.value()));
        }
        candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let sells = candidates.len();
        // Buy stops elected: stop <= price, key order is already lowest
        // stop first, then admission order.
        for entry in store.buy.by_stop.iter() {
            if entry.key().0 > price {
                break;
            }
            candidates.push((entry.key().0, entry.key().1, *entry.value()));
        }
        let (first, second) = if sells_first {
            (0..sells, sells..candidates.len())
        } else {
            (sells..candidates.len(), 0..sells)
        };
        for index in first.chain(second) {
            if let Some(&(_, _, id)) = candidates.get(index)
                && let Some(entry) = self.remove(id)
            {
                out.push((entry, price));
            }
        }
    }

    /// Trails every stop whose watermark a trade at `price` improves, and
    /// pushes `(id, new stop price)` onto `moved` for each stop whose stop
    /// price changed. Visits only those stops (skip-list prefix), in no
    /// order that matters: each update is independent. Each trailed stop
    /// is re-keyed in both skip lists, so a favourable print costs
    /// O(k log n) for the k stops it trails (every stop of a side when the
    /// price makes a new extreme for all of them). `stale` is scratch.
    fn trail_into(&self, price: u128, stale: &mut Vec<Id>, moved: &mut Vec<(Id, u128)>) {
        let Some(store) = self.store() else {
            return;
        };
        for side in [Side::Sell, Side::Buy] {
            let index = store.index(side);
            stale.clear();
            match side {
                Side::Sell => stale.extend(
                    index
                        .by_watermark
                        .iter()
                        .take_while(|entry| entry.key().0 < price)
                        .map(|entry| *entry.value()),
                ),
                Side::Buy => stale.extend(
                    index
                        .by_watermark
                        .iter()
                        .rev()
                        .take_while(|entry| entry.key().0 > price)
                        .map(|entry| *entry.value()),
                ),
            }
            for &id in stale.iter() {
                let Some(mut entry) = store.entries.get_mut(&id) else {
                    continue;
                };
                let Some(old) = StopTerms::of(&entry.order) else {
                    continue;
                };
                let new = old.trailed(price);
                if new == old {
                    continue;
                }
                let seq = entry.seq;
                index.remove(old, seq);
                index.insert(new, seq, id);
                apply_terms(&mut entry.order, new);
                if new.stop != old.stop {
                    moved.push((id, new.stop));
                }
            }
        }
    }

    /// Every pending stop matching `keep`, in admission order.
    #[must_use]
    pub(super) fn collect(&self, keep: impl Fn(&OrderType<()>) -> bool) -> Vec<StopEntry> {
        if self.is_empty() {
            return Vec::new();
        }
        let Some(store) = self.store() else {
            return Vec::new();
        };
        let mut stops: Vec<StopEntry> = store
            .entries
            .iter()
            .filter(|entry| keep(&entry.value().order))
            .map(|entry| entry.value().clone())
            .collect();
        // `DashMap` iteration order is per-instance; the sequence is not.
        stops.sort_unstable_by_key(|entry| entry.seq);
        stops
    }

    /// Drops every pending stop and restarts the admission sequence. The
    /// maps, once built, are kept (empty).
    pub(super) fn clear(&self) {
        if let Some(store) = self.store() {
            store.entries.clear();
            for index in [&store.sell, &store.buy] {
                while index.by_stop.pop_front().is_some() {}
                while index.by_watermark.pop_front().is_some() {}
            }
            store.next_seq.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.reset_path();
    }

    /// Sets the next admission sequence (restore path, after installing
    /// `next` stops with sequences `0..next`). Builds nothing for `0`.
    pub(super) fn set_next_seq(&self, next: u64) {
        match self.store() {
            Some(store) => store.next_seq.store(next, Ordering::Relaxed),
            None if next == 0 => {}
            None => self.store_or_init().next_seq.store(next, Ordering::Relaxed),
        }
    }
}

/// `InvalidOperation` for an order that is not a trailing stop.
#[cold]
#[inline(never)]
fn not_a_stop(id: Id) -> OrderBookError {
    OrderBookError::InvalidOperation {
        message: format!("order {id} is not a trailing stop"),
    }
}

/// A removal found the pending-stop count at zero: an accounting bug, not a
/// reachable state. Logged; the count stays at zero.
#[cold]
#[inline(never)]
fn warn_count_underflow(id: Id) {
    tracing::warn!(
        order_id = %id,
        "pending stop count underflow on removal; count left at zero"
    );
}

/// The order id an `OrderUpdate` targets.
#[inline]
#[must_use]
pub(super) fn update_target(update: &OrderUpdate) -> Id {
    match update {
        OrderUpdate::UpdatePrice { order_id, .. }
        | OrderUpdate::UpdateQuantity { order_id, .. }
        | OrderUpdate::UpdatePriceAndQuantity { order_id, .. }
        | OrderUpdate::Cancel { order_id }
        | OrderUpdate::Replace { order_id, .. } => *order_id,
    }
}

/// What one evaluation pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct StopPass {
    /// Stops whose stop price trailed.
    pub(super) trailed: usize,
    /// Stops elected and executed.
    pub(super) elected: usize,
}

impl<T> OrderBook<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// The id of the child order the pending stop `stop_id` executes as
    /// when it is elected (#286): a UUIDv5 of this book's trade-id
    /// namespace and `stop_id`. Its trades carry it as `taker_order_id`.
    #[must_use]
    pub fn stop_trigger_order_id(&self, stop_id: Id) -> Id {
        stop_trigger_order_id(self.transaction_id_generator.namespace(), stop_id)
    }

    /// Checks a stop's own terms and its position against the last trade
    /// (#286): a positive quantity, the [`StopTerms::inconsistency`] rules,
    /// and not already crossed by the book's last trade price (a sell stop
    /// at or above it, a buy stop at or below it).
    ///
    /// # Errors
    ///
    /// [`OrderBookError::InvalidStopTerms`] or
    /// [`OrderBookError::StopWouldTrigger`].
    pub(super) fn check_stop_terms(
        &self,
        order_id: Id,
        terms: StopTerms,
        quantity: u64,
    ) -> Result<(), OrderBookError> {
        if quantity == 0 {
            return Err(OrderBookError::InvalidStopTerms {
                order_id,
                reason: "quantity is zero",
            });
        }
        if let Some(reason) = terms.inconsistency() {
            return Err(OrderBookError::InvalidStopTerms { order_id, reason });
        }
        if let Some(last_trade_price) = self.last_trade_price()
            && terms.elected_by(last_trade_price)
        {
            return Err(OrderBookError::StopWouldTrigger {
                order_id,
                stop_price: terms.stop,
                last_trade_price,
            });
        }
        Ok(())
    }

    /// Admits a trailing stop as a pending off-book stop (#286).
    ///
    /// Checks, in order: kill switch, time-in-force (`GTC`, `GTD` or
    /// `DAY`), the stop's terms (positive quantity and trail, stop price
    /// not beyond its watermark), the last trade price (a stop it already
    /// crosses is refused with [`OrderBookError::StopWouldTrigger`]: a stop
    /// never turns into a market order on entry), duplicate id (resting or
    /// pending), the risk open-order and notional limits at the stop price
    /// (the price band does not apply: a stop price is a trigger, not a
    /// resting price), the shared shape validator (tick, lot, size, expiry,
    /// STP user id) and the trail's tick alignment. Then the risk
    /// contribution is reserved and the stop enters the store as `Open`.
    /// The watermark is taken as given: the stop starts trailing at the
    /// next trade.
    ///
    /// # Errors
    ///
    /// The first failing check's typed error; nothing was changed.
    #[cold]
    #[inline(never)]
    pub(super) fn admit_trailing_stop(
        &self,
        order: OrderType<T>,
        records_rejections: bool,
    ) -> Result<Arc<OrderType<T>>, OrderBookError> {
        let order_id = order.id();
        self.check_kill_switch_or_reject(order_id)?;
        let reject = |err: OrderBookError| {
            if records_rejections {
                self.track_state(
                    order_id,
                    OrderStatus::Rejected {
                        reason: RejectReason::from(&err),
                    },
                );
            }
            err
        };
        let Some(terms) = StopTerms::of(&order) else {
            return Err(reject(not_a_stop(order_id)));
        };
        if matches!(order.time_in_force(), TimeInForce::Ioc | TimeInForce::Fok) {
            return Err(reject(OrderBookError::InvalidOperation {
                message: format!(
                    "trailing stop {order_id} must be GTC, GTD or DAY: it is pending until triggered"
                ),
            }));
        }
        let quantity = order.total_quantity().map_err(reject)?;
        self.check_stop_terms(order_id, terms, quantity)
            .map_err(reject)?;
        if self.order_locations.contains_key(&order_id) || self.pending_stops.contains(order_id) {
            if records_rejections {
                crate::orderbook::metrics::record_reject(RejectReason::DuplicateOrderId);
            }
            return Err(OrderBookError::DuplicateOrderId { order_id });
        }
        if let Err(err) =
            self.risk_state
                .check_limit_admission(order.user_id(), terms.stop, quantity, None)
        {
            if records_rejections {
                self.reject_with_risk(order_id, &err);
            }
            return Err(err);
        }
        if let Err(err) = self.validate_order_shape(&order) {
            if records_rejections {
                self.record_shape_rejection(&order, &err);
            }
            return Err(err);
        }
        if let Some(tick) = self.tick_size
            && tick > 0
            && !terms.trail.is_multiple_of(tick)
        {
            return Err(reject(OrderBookError::InvalidTickSize {
                price: terms.trail,
                tick_size: tick,
            }));
        }
        let reservation = self
            .risk_state
            .on_admission(order_id, order.user_id(), terms.stop, quantity)
            .map_err(reject)?;
        let unit = self.convert_to_unit_type(&order);
        if let Err(err) = self.pending_stops.insert(unit) {
            self.risk_state.release_reservation(reservation);
            return Err(reject(err));
        }
        self.track_state(order_id, OrderStatus::Open);
        debug!(
            symbol = %self.symbol,
            %order_id,
            side = %terms.side,
            stop = terms.stop,
            watermark = terms.watermark,
            trail = terms.trail,
            quantity,
            "trailing stop pending"
        );
        Ok(Arc::new(order))
    }

    /// The pending stop `order_id`, converted, when there is one.
    #[must_use]
    #[inline(never)]
    pub(super) fn pending_stop_order(&self, order_id: Id) -> Option<Arc<OrderType<T>>> {
        self.pending_stops
            .get(order_id)
            .map(|entry| Arc::new(self.convert_from_unit_type(&entry.order)))
    }

    /// Cancels the pending stop `order_id` with `reason` (#286): releases
    /// its risk reservation, records `Cancelled { filled_quantity: 0,
    /// reason }` and releases the id last. `None` when it is not pending.
    #[inline(never)]
    pub(super) fn cancel_pending_stop(
        &self,
        order_id: Id,
        reason: CancelReason,
    ) -> Option<Arc<OrderType<T>>> {
        let entry = self.pending_stops.get(order_id)?;
        self.risk_state.on_cancel(order_id);
        self.track_state(
            order_id,
            OrderStatus::Cancelled {
                filled_quantity: 0,
                reason,
            },
        );
        self.pending_stops.remove(order_id);
        trace!(symbol = %self.symbol, %order_id, %reason, "pending trailing stop cancelled");
        Some(Arc::new(self.convert_from_unit_type(&entry.order)))
    }

    /// Applies `update` to the pending stop it targets (#286), under the
    /// exclusive gate `update_order` holds.
    ///
    /// - `Cancel` and a zero `UpdateQuantity` cancel it (`UserRequested`),
    ///   the removal semantics of a resting order.
    /// - `UpdateQuantity` resizes it in place and keeps its time priority.
    /// - `UpdatePrice` sets its stop price; `UpdatePriceAndQuantity` both;
    ///   `Replace` stop price, quantity and side. Each takes a fresh
    ///   admission sequence (the stop loses its time priority). The
    ///   watermark and trail are kept.
    ///
    /// The projected stop must pass [`Self::check_stop_terms`]: a zero
    /// quantity on `UpdatePriceAndQuantity` / `Replace` is refused with
    /// [`OrderBookError::InvalidStopTerms`] (a pending stop never holds a
    /// zero quantity), and a stop price the last trade already crosses with
    /// [`OrderBookError::StopWouldTrigger`] (a modify never elects a stop).
    /// It then runs the shape validator and the modify-aware risk check
    /// (limits, no price band) before anything changes; its risk
    /// contribution is then re-booked. The order state stays `Open`.
    ///
    /// # Errors
    ///
    /// The first failing check's typed error, with the stop unchanged.
    #[inline(never)]
    pub(super) fn update_pending_stop(
        &self,
        update: OrderUpdate,
    ) -> Result<Option<Arc<OrderType<T>>>, OrderBookError> {
        let order_id = update_target(&update);
        let Some(entry) = self.pending_stops.get(order_id) else {
            return Ok(None);
        };
        let mut projected = self.convert_from_unit_type(&entry.order);
        let requeue = match update {
            OrderUpdate::Cancel { .. } => {
                return Ok(self.cancel_pending_stop(order_id, CancelReason::UserRequested));
            }
            OrderUpdate::UpdateQuantity { new_quantity, .. } => {
                if new_quantity.as_u64() == 0 {
                    return Ok(self.cancel_pending_stop(order_id, CancelReason::UserRequested));
                }
                set_stop_fields(&mut projected, None, Some(new_quantity), None);
                false
            }
            OrderUpdate::UpdatePrice { new_price, .. } => {
                if projected.price() == new_price {
                    return Err(OrderBookError::InvalidOperation {
                        message: "Cannot update price to the same value".to_string(),
                    });
                }
                set_stop_fields(&mut projected, Some(new_price), None, None);
                true
            }
            OrderUpdate::UpdatePriceAndQuantity {
                new_price,
                new_quantity,
                ..
            } => {
                set_stop_fields(&mut projected, Some(new_price), Some(new_quantity), None);
                true
            }
            OrderUpdate::Replace {
                price,
                quantity,
                side,
                ..
            } => {
                set_stop_fields(&mut projected, Some(price), Some(quantity), Some(side));
                true
            }
        };
        let quantity = projected.total_quantity()?;
        let Some(terms) = StopTerms::of(&projected) else {
            return Err(not_a_stop(order_id));
        };
        self.check_stop_terms(order_id, terms, quantity)?;
        self.validate_order_shape(&projected)?;
        self.risk_state.check_modify_admission(
            order_id,
            projected.user_id(),
            terms.stop,
            quantity,
            None,
        )?;
        // Re-book the risk contribution in place (the modify check above
        // projected exactly this swap under the exclusive gate): nothing is
        // released before the new booking exists, so a failure leaves the
        // stop and its booking unchanged.
        self.risk_state
            .rebook_order(order_id, terms.stop, quantity)?;
        let unit = self.convert_to_unit_type(&projected);
        if let Err(err) = self.pending_stops.replace(order_id, unit, requeue) {
            self.restore_stop_booking(&entry.order);
            return Err(err);
        }
        debug!(
            symbol = %self.symbol,
            %order_id,
            side = %terms.side,
            stop = terms.stop,
            quantity,
            "pending trailing stop modified"
        );
        Ok(Some(Arc::new(projected)))
    }

    /// Re-books a pending stop's risk contribution as it was before a
    /// modify whose store update failed after its re-booking (reverting a
    /// re-booking that was just applied under the exclusive gate).
    #[cold]
    #[inline(never)]
    fn restore_stop_booking(&self, order: &OrderType<()>) {
        let Some(terms) = StopTerms::of(order) else {
            return;
        };
        let quantity = order.visible_quantity().as_u64();
        if let Err(err) = self
            .risk_state
            .rebook_order(order.id(), terms.stop, quantity)
        {
            tracing::error!(
                order_id = %order.id(),
                error = %err,
                "pending trailing stop's risk booking could not be restored after a failed modify"
            );
        }
    }

    /// Ids of the pending stops matching `keep`, in admission order: the
    /// scope a mass cancel or an expiry eviction appends after the resting
    /// orders it collected.
    #[must_use]
    pub(super) fn pending_stop_ids(&self, keep: impl Fn(&OrderType<()>) -> bool) -> Vec<Id> {
        self.pending_stops
            .collect(keep)
            .into_iter()
            .map(|entry| entry.order.id())
            .collect()
    }

    /// Every pending stop in admission order (snapshot path).
    #[must_use]
    pub(super) fn pending_stop_snapshot(&self) -> Vec<OrderType<()>> {
        self.pending_stops
            .collect(|_| true)
            .into_iter()
            .map(|entry| entry.order)
            .collect()
    }

    /// Evaluates the pending stops against the prints of the call that just
    /// traded (#286), in path order, and executes the stops they elect.
    ///
    /// Each sweep is a [`PrintSegment`] (the price it arrived from, its
    /// first and its last print; a sweep's prints are monotonic, so its
    /// extremes are its ends). For each of the segment's points in order
    /// (first, then last when it differs):
    ///
    /// 1. every stop the print improves trails (sell: the watermark rises
    ///    to the print; buy: it falls), so a favourable first print trails
    ///    a stop before the sweep's last print can elect it;
    /// 2. every stop the print crosses is elected and leaves the store:
    ///    sell stops highest stop price first, buy stops lowest first,
    ///    equal prices in admission order; the side the price moved
    ///    towards to reach the print goes first (sells when it fell, buys
    ///    when it rose; for the first print of the book, the sweep's own
    ///    direction, sells when flat).
    ///
    /// Then the segment's elected stops execute in that order, each as a
    /// child order whose sweep is queued as a new segment. Segments are
    /// processed first in, first out until none is left. Each stop is
    /// elected at most once, so the cascade is bounded by the number of
    /// pending stops. While the kill switch is engaged elections are
    /// suspended (prints still trail): a protective stop is not consumed
    /// by a market order the kill switch would reject, and the next print
    /// at or through its stop price after the release elects it.
    ///
    /// Must be called with the submit gate held (exclusive whenever a stop
    /// is pending), right before a mutating entry point that can trade
    /// returns. Without a pending stop or a recorded print it reads two
    /// relaxed atomics and returns.
    #[inline]
    pub(super) fn run_stop_triggers(&self) -> StopPass {
        if self.pending_stops.is_empty() {
            return StopPass::default();
        }
        let Some(segment) = self.pending_stops.take_path(self.last_trade_price()) else {
            return StopPass::default();
        };
        self.evaluate_pending_stops(segment)
    }

    /// The body of [`Self::run_stop_triggers`] for a recorded sweep.
    #[inline(never)]
    fn evaluate_pending_stops(&self, segment: PrintSegment) -> StopPass {
        let mut pass = StopPass::default();
        let mut scratch = StopScratch::take();
        scratch.queue.push_back(segment);
        while let Some(segment) = scratch.queue.pop_front() {
            if self.pending_stops.is_empty() {
                break;
            }
            let second = (segment.last != segment.first).then_some(segment.last);
            let points = [
                (segment.prev, Some(segment.first)),
                (Some(segment.first), second),
            ];
            for (arrived_from, point) in points {
                let Some(price) = point else {
                    continue;
                };
                scratch.moved.clear();
                self.pending_stops
                    .trail_into(price, &mut scratch.stale, &mut scratch.moved);
                for &(stop_id, stop) in &scratch.moved {
                    self.risk_state.rebook_price(stop_id, stop);
                    trace!(symbol = %self.symbol, order_id = %stop_id, stop, print = price, "trailing stop trailed");
                }
                // Bounded by the stops trailed, each at most once per print.
                if let Some(trailed) = pass.trailed.checked_add(scratch.moved.len()) {
                    pass.trailed = trailed;
                }
                if self.is_kill_switch_engaged() {
                    continue;
                }
                let sells_first = match arrived_from {
                    Some(from) if price != from => price < from,
                    _ => segment.last <= segment.first,
                };
                self.pending_stops.pop_elected(
                    price,
                    sells_first,
                    &mut scratch.candidates,
                    &mut scratch.elected,
                );
            }
            let mut elected = std::mem::take(&mut scratch.elected);
            for (entry, trigger_price) in elected.drain(..) {
                if let Some(child_segment) = self.execute_elected_stop(entry, trigger_price) {
                    scratch.queue.push_back(child_segment);
                }
                // Bounded by the pending stops (each elected once).
                if let Some(count) = pass.elected.checked_add(1) {
                    pass.elected = count;
                }
            }
            scratch.elected = elected;
        }
        self.pending_stops.reset_path();
        scratch.put();
        pass
    }

    /// Executes one elected stop (#286), already out of the store: its
    /// risk reservation is released, `Triggered { child_id, trigger_price,
    /// limit_price }` is recorded for the stop (the listener's election
    /// event), its child order runs with `origin_stop_id` on its trades, and the stop
    /// records that order's terminal state. Returns the child's sweep, if
    /// it traded.
    ///
    /// The child is an immediate-or-cancel market order, or, with a
    /// [`StopProtection`](crate::StopProtection) on the book (#302), an
    /// immediate-or-cancel limit order at the stop's (trailed) stop price
    /// moved by the collar against it. The collar is read once per elected
    /// stop, never on a path without one.
    ///
    /// A child-order id that is already in use (a resting order or a
    /// pending stop carrying the derived id: only possible when the
    /// trade-id namespace is known to whoever picks order ids, which is
    /// why it should stay private) does not run: the stop ends
    /// `Rejected { DuplicateOrderId }` and the collision is logged.
    fn execute_elected_stop(&self, entry: StopEntry, trigger_price: u128) -> Option<PrintSegment> {
        let stop = entry.order;
        let stop_id = stop.id();
        let side = stop.side();
        let user_id = stop.user_id();
        let quantity = stop.visible_quantity().as_u64();
        let stop_price = stop.price().as_u128();
        let limit = self
            .stop_protection
            .map(|protection| protection.limit_for(side, stop_price));
        self.risk_state.on_cancel(stop_id);
        let child_id = self.stop_trigger_order_id(stop_id);
        if self.order_locations.contains_key(&child_id) || self.pending_stops.contains(child_id) {
            self.refuse_colliding_child(stop_id, child_id);
            return None;
        }
        self.track_state(
            stop_id,
            OrderStatus::Triggered {
                child_id,
                trigger_price,
                limit_price: limit,
            },
        );
        debug!(
            symbol = %self.symbol,
            order_id = %stop_id,
            %child_id,
            %side,
            stop = stop_price,
            trigger_price,
            quantity,
            collar_limit = ?limit,
            "trailing stop elected; executing as an immediate-or-cancel child"
        );
        self.pending_stops.reset_path();
        let status = self.execute_stop_child(stop_id, child_id, side, quantity, limit, user_id);
        self.track_state(stop_id, status);
        self.pending_stops.take_path(self.last_trade_price())
    }

    /// An elected stop whose child-order id is already in use (#286):
    /// the stop ends `Rejected { DuplicateOrderId }`, nothing trades.
    #[cold]
    #[inline(never)]
    fn refuse_colliding_child(&self, stop_id: Id, child_id: Id) {
        tracing::error!(
            symbol = %self.symbol,
            order_id = %stop_id,
            %child_id,
            "elected trailing stop's child-order id is already in use; stop rejected"
        );
        self.track_state(
            stop_id,
            OrderStatus::Rejected {
                reason: RejectReason::DuplicateOrderId,
            },
        );
    }

    /// Runs an elected stop's child order through the ungated matching path
    /// (the body of `match_market_order_committed`; the caller holds the
    /// gate) and returns the terminal state its stop takes.
    ///
    /// `limit` is `None` for the unpriced market child and the collar limit
    /// with a [`StopProtection`](crate::StopProtection) (#302). Either way
    /// the child only matches, it is never rested: the remainder is
    /// cancelled, with the reason from
    /// [`Self::stop_child_remainder_reason`] (`StopProtectionBand` when the
    /// collar cut it, including a band with nothing in it,
    /// `InsufficientLiquidity` otherwise).
    fn execute_stop_child(
        &self,
        stop_id: Id,
        child_id: Id,
        side: Side,
        quantity: u64,
        limit: Option<u128>,
        user_id: Hash32,
    ) -> OrderStatus {
        if self.check_kill_switch_or_reject(child_id).is_err() {
            return OrderStatus::Rejected {
                reason: RejectReason::KillSwitchActive,
            };
        }
        if let Err(err) = self.check_trade_id_headroom(child_id, side, limit) {
            return OrderStatus::Rejected {
                reason: RejectReason::from(&err),
            };
        }
        let verified = match self.check_trade_arithmetic_or_reject(child_id, side, quantity, limit)
        {
            Ok(verified) => verified,
            Err(err) => {
                return OrderStatus::Rejected {
                    reason: RejectReason::from(&err),
                };
            }
        };
        let outcome = match self.match_order_with_user_outcome(
            child_id,
            side,
            quantity,
            limit,
            user_id,
            TakerKind::Standard,
            SweepReservation::NONE,
            verified,
        ) {
            Ok(outcome) => outcome,
            Err(OrderBookError::InsufficientLiquidity { .. }) => {
                return OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::InsufficientLiquidity,
                };
            }
            Err(OrderBookError::SelfTradePrevented { .. }) => {
                return OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::SelfTradePrevention,
                };
            }
            Err(err) => {
                tracing::warn!(
                    symbol = %self.symbol,
                    order_id = %child_id,
                    error = %err,
                    "elected stop's child order rejected before trading"
                );
                return OrderStatus::Rejected {
                    reason: RejectReason::from(&err),
                };
            }
        };
        let stp_cancelled = outcome.taker_stp_cancelled;
        let remaining = outcome.result.remaining_quantity().as_u64();
        let executed = match quantity.checked_sub(remaining) {
            Some(executed) => executed,
            None => {
                // The `MatchResult` invariant rules this out (a sweep never
                // reports more remaining than it was given).
                tracing::error!(
                    order_id = %child_id,
                    quantity,
                    remaining,
                    "elected stop's child order reports more remaining than its quantity"
                );
                0
            }
        };
        match self.publish_match_outcome_from(outcome, false, Some(stop_id)) {
            Err(_) => OrderStatus::Cancelled {
                filled_quantity: executed,
                reason: CancelReason::MatchAborted,
            },
            Ok(_) if stp_cancelled => OrderStatus::Cancelled {
                filled_quantity: executed,
                reason: CancelReason::SelfTradePrevention,
            },
            Ok(_) if remaining == 0 => OrderStatus::Filled {
                filled_quantity: executed,
            },
            Ok(_) => OrderStatus::Cancelled {
                filled_quantity: executed,
                reason: self.stop_child_remainder_reason(side, limit),
            },
        }
    }

    /// Why an elected stop's child left a remainder (#302): with a collar
    /// limit and liquidity left on the opposite side only beyond it (best
    /// bid below a sell child's limit, best ask above a buy child's), the
    /// collar cut the sweep: [`CancelReason::StopProtectionBand`].
    /// Otherwise (no collar, or the side ran out within the band) the
    /// immediate-or-cancel remainder reason,
    /// [`CancelReason::InsufficientLiquidity`]. Reads the best-price cache
    /// once, only for a collared child with a remainder.
    #[inline]
    fn stop_child_remainder_reason(&self, side: Side, limit: Option<u128>) -> CancelReason {
        let Some(limit) = limit else {
            return CancelReason::InsufficientLiquidity;
        };
        let beyond_band = match side {
            Side::Sell => self.best_bid().is_some_and(|bid| bid < limit),
            Side::Buy => self.best_ask().is_some_and(|ask| ask > limit),
        };
        if beyond_band {
            CancelReason::StopProtectionBand
        } else {
            CancelReason::InsufficientLiquidity
        }
    }
}

/// Sets a trailing stop's stop price, quantity and side (each when given).
fn set_stop_fields<E>(
    order: &mut OrderType<E>,
    new_price: Option<Price>,
    new_quantity: Option<Quantity>,
    new_side: Option<Side>,
) {
    if let OrderType::TrailingStop {
        price,
        quantity,
        side,
        ..
    } = order
    {
        if let Some(new_price) = new_price {
            *price = new_price;
        }
        if let Some(new_quantity) = new_quantity {
            *quantity = new_quantity;
        }
        if let Some(new_side) = new_side {
            *side = new_side;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pricelevel::{TimestampMs, UuidGenerator};

    fn stop(id: u64, side: Side, stop: u128, watermark: u128, trail: u64) -> OrderType<()> {
        OrderType::TrailingStop {
            id: Id::from_u64(id),
            price: Price::new(stop),
            quantity: Quantity::new(5),
            side,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(trail),
            last_reference_price: Price::new(watermark),
            extra_fields: (),
        }
    }

    #[test]
    fn test_stop_terms_sell_trails_up_and_never_down() {
        let terms = StopTerms::of(&stop(1, Side::Sell, 95, 100, 5)).expect("stop");
        let up = terms.trailed(110);
        assert_eq!((up.stop, up.watermark), (105, 110));
        let down = up.trailed(90);
        assert_eq!(down, up, "a lower trade never loosens a sell stop");
        assert!(up.elected_by(105));
        assert!(!up.elected_by(106));
    }

    #[test]
    fn test_stop_terms_buy_trails_down_and_never_up() {
        let terms = StopTerms::of(&stop(1, Side::Buy, 105, 100, 5)).expect("stop");
        let down = terms.trailed(90);
        assert_eq!((down.stop, down.watermark), (95, 90));
        assert_eq!(down.trailed(120), down);
        assert!(down.elected_by(95));
        assert!(!down.elected_by(94));
    }

    #[test]
    fn test_stop_terms_trail_keeps_a_looser_initial_stop_until_it_tightens() {
        // Watermark 100, trail 5, stop 90 (looser than 95): a trade at 101
        // tightens it to 96; a trade at 100 moves nothing.
        let terms = StopTerms::of(&stop(1, Side::Sell, 90, 100, 5)).expect("stop");
        assert_eq!(terms.trailed(100), terms);
        assert_eq!(terms.trailed(101).stop, 96);
    }

    #[test]
    fn test_stop_terms_unrepresentable_trail_keeps_the_stop() {
        let sell = StopTerms::of(&stop(1, Side::Sell, 1, 2, 50)).expect("stop");
        let trailed = sell.trailed(10);
        assert_eq!((trailed.stop, trailed.watermark), (1, 10));
        let buy = StopTerms {
            side: Side::Buy,
            stop: u128::MAX,
            watermark: u128::MAX,
            trail: 10,
        };
        let trailed = buy.trailed(u128::MAX - 5);
        assert_eq!(trailed.stop, u128::MAX);
        assert_eq!(trailed.watermark, u128::MAX - 5);
    }

    #[test]
    fn test_pending_stops_insert_remove_and_duplicate() {
        let store = PendingStops::new();
        assert!(store.is_empty());
        assert_eq!(store.insert(stop(1, Side::Sell, 95, 100, 5)).ok(), Some(0));
        assert_eq!(store.insert(stop(2, Side::Buy, 105, 100, 5)).ok(), Some(1));
        assert!(matches!(
            store.insert(stop(1, Side::Buy, 105, 100, 5)),
            Err(OrderBookError::DuplicateOrderId { .. })
        ));
        assert_eq!(store.len(), 2);
        assert!(store.contains(Id::from_u64(2)));
        assert_eq!(store.remove(Id::from_u64(1)).map(|e| e.seq), Some(0));
        assert!(store.remove(Id::from_u64(1)).is_none());
        assert_eq!(store.len(), 1);
        store.clear();
        assert!(store.is_empty());
        assert!(!store.contains(Id::from_u64(2)));
    }

    #[test]
    fn test_pending_stops_rejects_a_non_stop() {
        let store = PendingStops::new();
        let order = OrderType::Standard {
            id: Id::from_u64(1),
            price: Price::new(100),
            quantity: Quantity::new(1),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        };
        assert!(matches!(
            store.insert(order),
            Err(OrderBookError::InvalidOperation { .. })
        ));
        assert!(store.is_empty());
    }

    /// Pops the stops a print at `price` elects, as ids.
    fn pop_ids(store: &PendingStops, price: u128, sells_first: bool) -> Vec<Id> {
        let mut candidates = Vec::new();
        let mut out = Vec::new();
        store.pop_elected(price, sells_first, &mut candidates, &mut out);
        out.into_iter().map(|(entry, _)| entry.order.id()).collect()
    }

    fn trail(store: &PendingStops, price: u128) -> Vec<(Id, u128)> {
        let mut moved = Vec::new();
        store.trail_into(price, &mut Vec::new(), &mut moved);
        moved
    }

    #[test]
    fn test_pending_stops_elected_in_trigger_order() {
        let store = PendingStops::new();
        // seq 0: sell 90; seq 1: buy 100; seq 2: sell 99; seq 3: sell 80;
        // seq 4: sell 99 (same price as seq 2); seq 5: buy 95.
        for order in [
            stop(10, Side::Sell, 90, 110, 20),
            stop(11, Side::Buy, 100, 80, 20),
            stop(12, Side::Sell, 99, 110, 11),
            stop(13, Side::Sell, 80, 110, 30),
            stop(14, Side::Sell, 99, 110, 11),
            stop(15, Side::Buy, 95, 80, 15),
        ] {
            store.insert(order).expect("insert");
        }
        // At 96: sells 99 (seq 2, then 4) elected; buy 95 elected; sell
        // 90 / 80 and buy 100 not.
        assert_eq!(
            pop_ids(&store, 96, true),
            [12u64, 14, 15].map(Id::from_u64).to_vec(),
            "sells highest first, equal prices in admission order, then buys"
        );
        assert_eq!(store.len(), 3);
        // Buys first: buy 100 at 100 (lowest first), then no sell.
        assert_eq!(pop_ids(&store, 100, false), vec![Id::from_u64(11)]);
        // At 85: sell 90 elected, then (sells first) nothing else.
        assert_eq!(pop_ids(&store, 85, true), vec![Id::from_u64(10)]);
        assert_eq!(pop_ids(&store, 85, true), Vec::<Id>::new(), "popped once");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_pending_stops_trail_rekeys_and_reports_moved_stops() {
        let store = PendingStops::new();
        store
            .insert(stop(1, Side::Sell, 95, 100, 5))
            .expect("insert");
        store
            .insert(stop(2, Side::Buy, 105, 100, 5))
            .expect("insert");
        assert_eq!(trail(&store, 110), vec![(Id::from_u64(1), 105)]);
        let sell = store.get(Id::from_u64(1)).expect("pending");
        let terms = StopTerms::of(&sell.order).expect("stop");
        assert_eq!((terms.stop, terms.watermark), (105, 110));
        assert_eq!(trail(&store, 90), vec![(Id::from_u64(2), 95)]);
        // Idempotent at the same price.
        assert!(trail(&store, 90).is_empty());
        assert!(trail(&store, 110).is_empty());
        // Re-keyed: the buy stop (now 95) is elected at 106 but not the
        // sell stop (now 105, elected at 105 and below).
        assert_eq!(pop_ids(&store, 106, true), vec![Id::from_u64(2)]);
        assert_eq!(pop_ids(&store, 105, true), vec![Id::from_u64(1)]);
    }

    #[test]
    fn test_pending_stops_replace_requeues_on_demand() {
        let store = PendingStops::new();
        store
            .insert(stop(1, Side::Sell, 95, 100, 5))
            .expect("insert");
        store
            .insert(stop(2, Side::Sell, 95, 100, 5))
            .expect("insert");
        store
            .replace(Id::from_u64(1), stop(1, Side::Sell, 95, 100, 5), true)
            .expect("replace");
        assert_eq!(
            pop_ids(&store, 90, true),
            vec![Id::from_u64(2), Id::from_u64(1)],
            "the requeued stop lost its priority"
        );
        assert!(matches!(
            store.replace(Id::from_u64(3), stop(3, Side::Sell, 96, 100, 5), false),
            Err(OrderBookError::OrderNotFound(_))
        ));
    }

    #[test]
    fn test_print_path_records_the_first_print_only_with_stops() {
        let store = PendingStops::new();
        store.record_print(None, 100);
        assert_eq!(
            store.take_path(Some(100)),
            None,
            "no stop: nothing recorded"
        );
        store
            .insert(stop(1, Side::Sell, 95, 100, 5))
            .expect("insert");
        store.record_print(Some(100), 102);
        store.record_print(Some(102), 101);
        assert_eq!(
            store.take_path(Some(98)),
            Some(PrintSegment {
                prev: Some(100),
                first: 102,
                last: 98
            })
        );
        assert_eq!(store.take_path(Some(98)), None, "consumed");
        store.record_print(Some(98), 97);
        store.clear();
        assert_eq!(store.take_path(Some(97)), None, "cleared with the stops");
    }

    #[test]
    fn test_pending_stops_collect_is_in_admission_order() {
        let store = PendingStops::new();
        for id in [5u64, 3, 9, 1] {
            store
                .insert(stop(id, Side::Sell, 95, 100, 5))
                .expect("insert");
        }
        let ids: Vec<Id> = store
            .collect(|_| true)
            .into_iter()
            .map(|entry| entry.order.id())
            .collect();
        assert_eq!(
            ids,
            [5u64, 3, 9, 1].map(Id::from_u64).to_vec(),
            "admission order, not id or hash order"
        );
    }

    #[test]
    fn test_stop_trigger_order_id_is_deterministic_and_distinct() {
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"TEST");
        let generator = UuidGenerator::new(namespace);
        let a = stop_trigger_order_id(generator.namespace(), Id::from_u64(1));
        assert_eq!(a, stop_trigger_order_id(namespace, Id::from_u64(1)));
        assert_ne!(a, stop_trigger_order_id(namespace, Id::from_u64(2)));
        assert_ne!(a, Id::from_u64(1));
        // Same 16 bytes, different id variant.
        assert_ne!(
            stop_trigger_order_id(namespace, Id::sequential(7)),
            stop_trigger_order_id(
                namespace,
                Id::from_uuid(Uuid::from_bytes(Id::sequential(7).as_bytes()))
            )
        );
        let other = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"OTHER");
        assert_ne!(a, stop_trigger_order_id(other, Id::from_u64(1)));
    }
}
