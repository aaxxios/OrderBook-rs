//! Off-book trailing stops (#286, `special_orders`).
//!
//! A [`OrderType::TrailingStop`] is **never** placed on a price level. It is
//! held in the book's [`PendingStops`] store, where it is invisible to
//! matching, depth, analytics and the level lists of every snapshot, and it
//! is driven by the book's **last trade price**:
//!
//! - **Trail.** The watermark (the order's `last_reference_price`) follows
//!   the last trade in the stop's favour: a sell stop keeps the highest last
//!   trade seen since admission, a buy stop the lowest. The stop price
//!   follows the watermark at `trail_amount` (sell: `watermark - trail`,
//!   buy: `watermark + trail`) and only ever tightens: a sell stop never
//!   moves down, a buy stop never moves up.
//! - **Trigger.** A sell stop is elected when the last trade is at or below
//!   its stop price, a buy stop when it is at or above. The elected stop
//!   leaves the store, releases its risk reservation and executes as an
//!   immediate-or-cancel **market order** for its quantity, on its side,
//!   for its user (self-trade prevention, fees and the trade-id / notional
//!   preflights apply as for any market taker); an unexecuted remainder is
//!   cancelled.
//! - **When.** Evaluation runs automatically, under the submit gate the
//!   mutating call already holds, right before every call that can trade
//!   returns (`add_order*`, `submit_market_order*`, the `match_*` entry
//!   points, `update_order`), and when a stop is admitted or modified. It
//!   is iterative: the trades of an elected stop can elect more stops, and
//!   evaluation repeats until no stop is elected by the current last trade
//!   price. Each stop is elected at most once (it leaves the store before
//!   its market order runs), so a cascade ends after at most as many rounds
//!   as there are pending stops.
//! - **Order.** Stops elected by the same last trade price execute in
//!   admission order (time priority), sell and buy alike. Stops are keyed
//!   by `(price, admission sequence)` in lock-free skip lists, so the scan
//!   for the next elected stop and the watermark update are ordered and
//!   independent of hashing. The market order of stop `S` carries the id
//!   [`stop_trigger_order_id`] (UUIDv5 of the book's trade-id namespace and
//!   `S`), so a replay with the same namespace reproduces it.
//!
//! Every mutation of the store happens under the **exclusive** submit gate:
//! while a stop is pending, every gated mutator of the book takes the
//! exclusive side (see `OrderBook::acquire_coherent_submit_gate`), and the
//! count only grows under it, so a caller that read zero pending stops
//! under the shared side keeps reading zero. With no pending stop the cost
//! on every path is one relaxed atomic load.
//!
//! The order state of a pending stop is `Open`. When elected it takes the
//! terminal state of its market order: `Filled` when the market order
//! executed its whole quantity, `Cancelled { InsufficientLiquidity }` for
//! an unexecuted remainder (including none executed), `Cancelled {
//! SelfTradePrevention }` / `Cancelled { MatchAborted }` when self-trade
//! prevention or a failed price level stopped it, and `Rejected` when the
//! market order was rejected untouched (kill switch engaged, trade-id
//! generator exhausted, notional or fee not representable).

use crate::orderbook::book::OrderBook;
use crate::orderbook::error::OrderBookError;
use crate::orderbook::matching::SweepReservation;
use crate::orderbook::modifications::OrderQuantity;
use crate::orderbook::order_state::{CancelReason, OrderStatus};
use crate::orderbook::reject_reason::RejectReason;
use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use pricelevel::{
    Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TakerKind, TimeInForce,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::{debug, trace};
use uuid::Uuid;

/// Label hashed into the name of every stop-trigger market order id (#286).
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

/// The id of the market order an elected stop executes as (#286).
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
/// A book that never holds a stop pays for this field with one relaxed
/// load per mutating call and allocates nothing: the maps are built on the
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
    /// Number of pending stops: the one relaxed load the fast path reads.
    count: AtomicUsize,
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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |seq| {
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

    /// Trails every stop whose watermark a trade at `price` improves, and
    /// pushes `(id, new stop price)` onto `moved` for each stop whose stop
    /// price changed. Visits only those stops (skip-list prefix), in no
    /// order that matters: each update is independent.
    pub(super) fn trail(&self, price: u128, moved: &mut Vec<(Id, u128)>) {
        let Some(store) = self.store() else {
            return;
        };
        for side in [Side::Sell, Side::Buy] {
            let index = store.index(side);
            let stale: Vec<Id> = match side {
                Side::Sell => index
                    .by_watermark
                    .iter()
                    .take_while(|entry| entry.key().0 < price)
                    .map(|entry| *entry.value())
                    .collect(),
                Side::Buy => index
                    .by_watermark
                    .iter()
                    .rev()
                    .take_while(|entry| entry.key().0 > price)
                    .map(|entry| *entry.value())
                    .collect(),
            };
            for id in stale {
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

    /// Pushes every stop a trade at `price` elects onto `out` as
    /// `(admission sequence, id)`, sorted by sequence (time priority).
    /// Visits only the elected stops (skip-list prefix per side).
    pub(super) fn elected(&self, price: u128, out: &mut Vec<(u64, Id)>) {
        let Some(store) = self.store() else {
            return;
        };
        for entry in store.sell.by_stop.iter().rev() {
            if entry.key().0 < price {
                break;
            }
            out.push((entry.key().1, *entry.value()));
        }
        for entry in store.buy.by_stop.iter() {
            if entry.key().0 > price {
                break;
            }
            out.push((entry.key().1, *entry.value()));
        }
        // Sequences are unique, so the unstable sort is deterministic.
        out.sort_unstable_by_key(|(seq, _)| *seq);
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
    /// The id of the market order the pending stop `stop_id` executes as
    /// when it is elected (#286): a UUIDv5 of this book's trade-id
    /// namespace and `stop_id`. Its trades carry it as `taker_order_id`.
    #[must_use]
    pub fn stop_trigger_order_id(&self, stop_id: Id) -> Id {
        stop_trigger_order_id(self.transaction_id_generator.namespace(), stop_id)
    }

    /// Admits a trailing stop as a pending off-book stop (#286).
    ///
    /// Checks, in order: kill switch, time-in-force (`GTC`, `GTD` or
    /// `DAY`), duplicate id (resting or pending), the risk open-order and
    /// notional limits at the stop price (the price band does not apply:
    /// a stop price is a trigger, not a resting price), the shared shape
    /// validator (tick, lot, size, expiry, STP user id) and the trail's
    /// tick alignment. Then the risk contribution is reserved and the stop
    /// enters the store as `Open`. Evaluation against the current last
    /// trade happens when the gated entry point returns.
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
        let quantity = order.total_quantity().map_err(&reject)?;
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
            .map_err(&reject)?;
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
    /// - `Cancel` and a zero `UpdateQuantity` cancel it (`UserRequested`).
    /// - `UpdateQuantity` resizes it in place and keeps its time priority.
    /// - `UpdatePrice` sets its stop price; `UpdatePriceAndQuantity` both;
    ///   `Replace` stop price, quantity and side. Each takes a fresh
    ///   admission sequence (the stop loses its time priority). The
    ///   watermark and trail are kept.
    ///
    /// The projected stop runs the shape validator and the modify-aware
    /// risk check (limits, no price band) before anything changes; its risk
    /// contribution is then re-booked. The order state stays `Open`. The
    /// caller evaluates the stops before returning, so a modify that moves
    /// the stop through the last trade price elects it.
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
        self.validate_order_shape(&projected)?;
        let quantity = projected.total_quantity()?;
        let Some(terms) = StopTerms::of(&projected) else {
            return Err(not_a_stop(order_id));
        };
        self.risk_state.check_modify_admission(
            order_id,
            projected.user_id(),
            terms.stop,
            quantity,
            None,
        )?;
        // Re-book the risk contribution: the modify check above projected
        // exactly this swap under the exclusive gate, so the re-admission
        // can only fail on an exhausted reservation generation; the old
        // booking is then restored.
        self.risk_state.on_cancel(order_id);
        if let Err(err) =
            self.risk_state
                .on_admission(order_id, projected.user_id(), terms.stop, quantity)
        {
            self.restore_stop_booking(&entry.order);
            return Err(err);
        }
        let unit = self.convert_to_unit_type(&projected);
        if let Err(err) = self.pending_stops.replace(order_id, unit, requeue) {
            self.risk_state.on_cancel(order_id);
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
    /// modify that failed after releasing it.
    #[cold]
    #[inline(never)]
    fn restore_stop_booking(&self, order: &OrderType<()>) {
        let Some(terms) = StopTerms::of(order) else {
            return;
        };
        let quantity = order.visible_quantity().as_u64();
        if let Err(err) =
            self.risk_state
                .on_admission(order.id(), order.user_id(), terms.stop, quantity)
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

    /// Evaluates the pending stops against the last trade price (#286):
    /// trails them, then elects and executes every stop the price crossed,
    /// in admission order, and repeats with the new last trade price until
    /// no stop is elected.
    ///
    /// Must be called with the submit gate held (exclusive whenever a stop
    /// is pending), right before a mutating entry point that can trade
    /// returns. With no pending stop it is one relaxed load.
    #[inline]
    pub(super) fn run_stop_triggers(&self) -> StopPass {
        if self.pending_stops.is_empty() {
            return StopPass::default();
        }
        self.evaluate_pending_stops()
    }

    /// The body of [`Self::run_stop_triggers`] once a stop is pending.
    #[inline(never)]
    fn evaluate_pending_stops(&self) -> StopPass {
        let mut pass = StopPass::default();
        let mut moved: Vec<(Id, u128)> = Vec::new();
        let mut elected: Vec<(u64, Id)> = Vec::new();
        // Each round elects at least one stop (which never comes back) or
        // ends the loop, so it runs at most `len + 1` rounds.
        while let Some(price) = self.last_trade_price() {
            if self.pending_stops.is_empty() {
                break;
            }
            moved.clear();
            self.pending_stops.trail(price, &mut moved);
            for &(stop_id, stop) in &moved {
                self.risk_state.rebook_price(stop_id, stop);
                trace!(symbol = %self.symbol, order_id = %stop_id, stop, last_trade = price, "trailing stop trailed");
            }
            // Bounded by the stops trailed, each at most once per round.
            if let Some(trailed) = pass.trailed.checked_add(moved.len()) {
                pass.trailed = trailed;
            }
            elected.clear();
            self.pending_stops.elected(price, &mut elected);
            if elected.is_empty() {
                break;
            }
            for &(_, stop_id) in &elected {
                if let Some(entry) = self.pending_stops.get(stop_id) {
                    self.execute_elected_stop(&entry.order, price);
                    // Bounded by the pending stops (each elected once).
                    if let Some(count) = pass.elected.checked_add(1) {
                        pass.elected = count;
                    }
                }
            }
        }
        pass
    }

    /// Executes one elected stop (#286): the stop leaves the store after
    /// its risk reservation is released, then its market order runs and
    /// the stop records that order's terminal state.
    fn execute_elected_stop(&self, stop: &OrderType<()>, last_trade: u128) {
        let stop_id = stop.id();
        let side = stop.side();
        let user_id = stop.user_id();
        let quantity = stop.visible_quantity().as_u64();
        self.risk_state.on_cancel(stop_id);
        self.pending_stops.remove(stop_id);
        let child_id = self.stop_trigger_order_id(stop_id);
        debug!(
            symbol = %self.symbol,
            order_id = %stop_id,
            %child_id,
            %side,
            stop = stop.price().as_u128(),
            last_trade,
            quantity,
            "trailing stop elected; executing as a market order"
        );
        let status = self.execute_stop_market_order(child_id, side, quantity, user_id);
        self.track_state(stop_id, status);
    }

    /// Runs an elected stop's market order through the ungated market path
    /// (the body of `match_market_order_committed`; the caller holds the
    /// gate) and returns the terminal state its stop takes.
    fn execute_stop_market_order(
        &self,
        child_id: Id,
        side: Side,
        quantity: u64,
        user_id: Hash32,
    ) -> OrderStatus {
        if self.check_kill_switch_or_reject(child_id).is_err() {
            return OrderStatus::Rejected {
                reason: RejectReason::KillSwitchActive,
            };
        }
        if let Err(err) = self.check_trade_id_headroom(child_id, side, None) {
            return OrderStatus::Rejected {
                reason: RejectReason::from(&err),
            };
        }
        let verified = match self.check_trade_arithmetic_or_reject(child_id, side, quantity, None) {
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
            None,
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
                    "elected stop's market order rejected before trading"
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
                    "elected stop's market order reports more remaining than its quantity"
                );
                0
            }
        };
        match self.publish_match_outcome(outcome, false) {
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
                reason: CancelReason::InsufficientLiquidity,
            },
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

    #[test]
    fn test_pending_stops_elected_in_admission_order_across_sides() {
        let store = PendingStops::new();
        // seq 0: sell stop 90; seq 1: buy stop 100; seq 2: sell stop 99;
        // seq 3: sell stop 80 (not elected at 95).
        for order in [
            stop(10, Side::Sell, 90, 110, 20),
            stop(11, Side::Buy, 100, 80, 20),
            stop(12, Side::Sell, 99, 110, 11),
            stop(13, Side::Sell, 80, 110, 30),
        ] {
            store.insert(order).expect("insert");
        }
        let mut out = Vec::new();
        store.elected(95, &mut out);
        // Sell 99 elected (95 <= 99), sell 90 not, buy 100 not (95 < 100).
        assert_eq!(out, vec![(2, Id::from_u64(12))]);
        out.clear();
        store.elected(100, &mut out);
        // Buy 100 elected (100 >= 100); sell 99 no longer (100 > 99).
        assert_eq!(out, vec![(1, Id::from_u64(11))]);
        out.clear();
        store.elected(85, &mut out);
        assert_eq!(out, vec![(0, Id::from_u64(10)), (2, Id::from_u64(12))]);
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
        let mut moved = Vec::new();
        store.trail(110, &mut moved);
        assert_eq!(moved, vec![(Id::from_u64(1), 105)]);
        let sell = store.get(Id::from_u64(1)).expect("pending");
        let terms = StopTerms::of(&sell.order).expect("stop");
        assert_eq!((terms.stop, terms.watermark), (105, 110));
        // Re-keyed: now elected at 105, not at 106.
        let mut out = Vec::new();
        store.elected(106, &mut out);
        assert!(out.iter().all(|(_, id)| *id != Id::from_u64(1)));
        out.clear();
        store.elected(105, &mut out);
        assert!(out.contains(&(0, Id::from_u64(1))));
        moved.clear();
        store.trail(90, &mut moved);
        assert_eq!(moved, vec![(Id::from_u64(2), 95)]);
        // Idempotent at the same price.
        moved.clear();
        store.trail(90, &mut moved);
        store.trail(110, &mut moved);
        assert!(moved.is_empty());
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
            .replace(Id::from_u64(1), stop(1, Side::Sell, 96, 100, 5), true)
            .expect("replace");
        let mut out = Vec::new();
        store.elected(90, &mut out);
        assert_eq!(out, vec![(1, Id::from_u64(2)), (2, Id::from_u64(1))]);
        assert!(matches!(
            store.replace(Id::from_u64(3), stop(3, Side::Sell, 96, 100, 5), false),
            Err(OrderBookError::OrderNotFound(_))
        ));
        assert!(matches!(
            store.replace(Id::from_u64(1), stop(9, Side::Sell, 96, 100, 5), false),
            Err(OrderBookError::InvalidOperation { .. })
        ));
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
