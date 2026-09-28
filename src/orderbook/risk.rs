//! Pre-trade risk layer for `OrderBook<T>`.
//!
//! This module provides the operator-driven, opt-in risk gating for new
//! flow on the order book. It is composed of:
//!
//! - [`RiskConfig`] — the operator-supplied limits (per-account open
//!   orders, per-account notional, price band against a reference price).
//! - [`ReferencePriceSource`] — selects the reference price used by the
//!   price-band check.
//! - [`RiskState`] — bound to an [`OrderBook`](crate::OrderBook),
//!   carries the optional config plus per-account counters
//!   (`DashMap<Hash32, RiskCounters>`) and per-resting-order entries
//!   (`DashMap<Id, RiskEntry>`). When [`RiskConfig`] is `None`, every
//!   check returns `Ok(())` and every hook is a no-op — the engine pays
//!   only the cost of an `Option::is_none` branch.
//!
//! Check ordering on submit is documented as
//! `kill_switch → risk → STP → fees → match`.
//!
//! ## Decision C
//!
//! Market orders skip every risk check (no submitted price; no rest;
//! no contribution to the resting open-order count). Kill switch still
//! gates them. `RiskState::check_market_admission` therefore returns
//! `Ok(())` unconditionally and exists only to keep the gate ordering
//! consistent across submit and add paths and to leave room for a
//! future per-account market-order rate limiter without breaking the
//! call shape.
//!
//! ## Checked accounting (#243)
//!
//! Every notional product (`price × quantity`), every counter increment
//! and every price-band cross-multiplication uses checked arithmetic.
//! An overflow is never clamped into a value that could pass a limit:
//!
//! - at admission it is a typed rejection
//!   ([`OrderBookError::RiskMaxNotional`] /
//!   [`OrderBookError::RiskMaxOpenOrders`]), whether or not the matching
//!   limit is configured, because the counters could no longer represent
//!   the account's exposure;
//! - the price band is decided exactly over the whole `u128` domain
//!   (an overflowing product resolves to the correct side of the band
//!   instead of both sides saturating and comparing equal);
//! - on release (fill, cancel, quantity decrease) a decrement larger
//!   than the counter means a double release or another accounting bug.
//!   It is logged at `WARN` with the order, account and counter, counted
//!   in [`RiskState::accounting_anomalies`] (and the
//!   `orderbook_risk_accounting_anomalies_total` metric), and the counter
//!   is set to zero. Zero is the only value that keeps the state usable:
//!   every live contribution is non-negative, so the counter cannot be
//!   below zero, and a wrap to near `MAX` would lock the account out of
//!   admission forever and block the eviction of its counters.

use crate::orderbook::error::OrderBookError;
use crossbeam::atomic::AtomicCell;
use dashmap::DashMap;
use pricelevel::{Hash32, Id};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{error, warn};

/// Source for the reference price used by the price-band check.
///
/// The price band rejects orders whose limit price deviates from the
/// reference by more than the configured number of basis points.
/// `LastTrade` and `Mid` resolve dynamically per check; `FixedPrice`
/// is operator-pinned (e.g. an external mark price piped in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ReferencePriceSource {
    /// Last executed trade price. The check is skipped when no trade
    /// has occurred yet on this book.
    LastTrade,
    /// Integer midpoint `(best_bid + best_ask) / 2`. Falls back to
    /// `LastTrade` when the book is one-sided. The check is skipped
    /// when neither a midpoint nor a last trade is available.
    Mid,
    /// Caller-supplied fixed reference price (raw integer ticks). The
    /// check always runs.
    FixedPrice(u128),
}

/// Per-`OrderBook` risk configuration.
///
/// Build via [`RiskConfig::new`] and the chained `with_*` methods. Empty
/// config (every field `None`) is a no-op passthrough — every check
/// returns `Ok(())`. The struct is `Default` and `Serialize`/
/// `Deserialize`, so it round-trips cleanly through the snapshot
/// package with `#[serde(default)]`.
///
/// # Semantics: submitted vs. resting
///
/// The `max_open_orders_per_account` and `max_notional_per_account`
/// limits are evaluated against the **submitted** quantity / notional,
/// **before** matching. An aggressive limit order that would fully
/// match against the opposite side and leave nothing resting is still
/// gated against these limits as if every contract were going to rest.
///
/// This is the standard pre-trade gating pattern in tier-one electronic
/// venues (CME / Nasdaq pre-trade risk hooks behave the same way): the
/// engine does not speculatively match before deciding whether to
/// admit. Counter updates **after** matching reflect the actual resting
/// remainder, so a fully-filled aggressive order does not leave
/// long-lived counter pressure on the account — only the in-flight
/// admission check sees the worst case.
///
/// If you need a "would-rest" projection instead of a "submitted"
/// admission gate, run a `peek_match` simulation in your gateway
/// layer and pass the resulting resting remainder in. Issue a
/// follow-up if you want this surfaced from the engine itself.
///
/// # Representable exposure
///
/// Whenever a config is installed, an admission whose notional
/// (`price × quantity`) or whose addition to the account's resting
/// notional does not fit in `u128` is rejected with
/// [`OrderBookError::RiskMaxNotional`], and an account whose open-order
/// count would exceed `u64::MAX` is rejected with
/// [`OrderBookError::RiskMaxOpenOrders`], even when the corresponding
/// limit is `None` (#243).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Maximum number of resting orders a single account may have on
    /// this book at any time. `None` disables the check.
    pub max_open_orders_per_account: Option<u64>,
    /// Maximum notional (`price × quantity`, in raw ticks) a single
    /// account may have resting on this book at any time. `None`
    /// disables the check.
    pub max_notional_per_account: Option<u128>,
    /// Maximum allowed deviation in basis points between an incoming
    /// limit price and the resolved reference price. `None` (or
    /// `reference_price = None`) disables the check.
    pub price_band_bps: Option<u32>,
    /// Reference price source used by the price-band check.
    pub reference_price: Option<ReferencePriceSource>,
}

impl RiskConfig {
    /// Construct an empty configuration with every limit disabled.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum number of resting orders per account.
    #[inline]
    #[must_use]
    pub fn with_max_open_orders_per_account(mut self, n: u64) -> Self {
        self.max_open_orders_per_account = Some(n);
        self
    }

    /// Set the maximum resting notional per account (in raw ticks).
    #[inline]
    #[must_use]
    pub fn with_max_notional_per_account(mut self, n: u128) -> Self {
        self.max_notional_per_account = Some(n);
        self
    }

    /// Set the price-band tolerance in basis points and the reference
    /// price source used to evaluate the band.
    #[inline]
    #[must_use]
    pub fn with_price_band_bps(mut self, bps: u32, source: ReferencePriceSource) -> Self {
        self.price_band_bps = Some(bps);
        self.reference_price = Some(source);
        self
    }
}

/// Per-account counters maintained by [`RiskState`].
///
/// Every update is a compare-and-swap loop with checked arithmetic
/// (#243), so a counter never wraps. Orderings are `Relaxed` for the
/// `AtomicU64` (the counters publish no other memory; each is an
/// independent value whose own modification order is all the gate
/// needs) and sequentially consistent for the `AtomicCell<u128>`, which
/// takes no ordering argument. The admission check reads the counters
/// before matching and the increment happens after, so concurrent
/// admissions can over-admit by at most one in-flight order per racing
/// thread; the counters themselves stay exact (linearizable sums).
#[derive(Debug, Default)]
pub struct RiskCounters {
    /// Number of resting orders this account currently has on the book.
    pub(super) open_count: AtomicU64,
    /// Sum of `price × remaining_qty` (in raw ticks) across all of
    /// this account's resting orders.
    pub(super) resting_notional: AtomicCell<u128>,
}

/// Per-resting-order risk bookkeeping.
///
/// One entry per order admitted into the resting book. Used on cancel
/// and fill to compute the deltas applied to per-account counters.
#[derive(Debug, Clone, Copy)]
pub(super) struct RiskEntry {
    pub(super) account: Hash32,
    pub(super) price: u128,
    pub(super) remaining_qty: u64,
    /// Reservation generation (#243 review): unique per live admission,
    /// so only the admission that created this entry can release it
    /// through its [`RiskReservation`]. `0` for entries rebuilt from a
    /// snapshot, which no reservation token ever carries.
    pub(super) generation: u64,
}

/// Proof that [`RiskState::on_admission`] reserved an order's risk
/// contribution (#243 review).
///
/// Handed back to [`RiskState::release_reservation`] when the order then
/// fails to rest. The release only removes the entry whose generation
/// matches, so a caller that lost a same-id race can never release the
/// winner's entry. `generation == 0` means nothing was reserved (no
/// `RiskConfig` installed) and the release is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a reservation must be released if the order does not rest"]
pub(super) struct RiskReservation {
    order_id: Id,
    generation: u64,
}

/// Notional pre-booked by [`RiskState::reserve_quantity_update`] before an
/// in-place quantity update mutates the price level (#243 review).
///
/// Settle it with [`RiskState::commit_quantity_update`] once the level
/// applied the update, or [`RiskState::rollback_quantity_update`] if it
/// did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a quantity reservation must be committed or rolled back"]
pub(super) struct QuantityReservation {
    order_id: Id,
    account: Hash32,
    price: u128,
    /// Generation of the entry the reservation was booked against, so
    /// settlement never touches a different entry for the same id.
    generation: u64,
    /// `false` when the order is untracked (nothing reserved or settled).
    tracked: bool,
    /// Quantity whose notional was added to the account's counter.
    reserved_qty: u64,
}

/// Risk state bound to a single [`OrderBook`](crate::OrderBook).
///
/// Carries the optional [`RiskConfig`], the per-account counters, the
/// per-order entry map, a one-shot warning latch for the
/// "no reference price available" code path, and the accounting-anomaly
/// counter. All public operations are no-ops when `config` is `None`.
#[derive(Debug, Default)]
pub struct RiskState {
    pub(super) config: Option<RiskConfig>,
    pub(super) counters: DashMap<Hash32, RiskCounters>,
    pub(super) orders: DashMap<Id, RiskEntry>,
    pub(super) warned_no_reference: AtomicBool,
    /// Number of accounting anomalies observed (release larger than the
    /// counter, fill larger than the tracked remainder, a post-trade
    /// counter overflow). Diagnostic only; see [`Self::accounting_anomalies`].
    pub(super) accounting_anomalies: AtomicU64,
    /// Last reservation generation handed out by [`Self::on_admission`].
    pub(super) generations: AtomicU64,
}

/// Basis points per unit (100 % = 10 000 bps).
const BPS_SCALE: u128 = 10_000;

/// Exact price-band verdict for the case where both `diff * 10_000` and
/// `bps_limit * reference` overflow `u128` (#243).
///
/// `diff * 10_000 > bps * reference` holds exactly when
/// `diff > floor(bps * reference / 10_000)` (for an integer `diff`).
/// Writing `reference = 10_000 * q + r` gives
/// `floor(bps * reference / 10_000) = bps * q + floor(bps * r / 10_000)`,
/// where `bps * r < 2^32 * 10_000` never overflows. If `bps * q` (or the
/// sum) overflows, the threshold exceeds every representable `diff`,
/// so the order is inside the band.
#[cold]
#[inline(never)]
fn band_breach_wide(diff: u128, reference: u128, bps_limit: u32) -> bool {
    let bps = u128::from(bps_limit);
    let (Some(q), Some(r)) = (
        reference.checked_div(BPS_SCALE),
        reference.checked_rem(BPS_SCALE),
    ) else {
        // Unreachable with a non-zero constant divisor; fail closed.
        return true;
    };
    let threshold = bps.checked_mul(q).and_then(|whole| {
        bps.checked_mul(r)
            .and_then(|partial| partial.checked_div(BPS_SCALE))
            .and_then(|fraction| whole.checked_add(fraction))
    });
    threshold.is_some_and(|threshold| diff > threshold)
}

/// Notional of `quantity` at `price`, or `None` when the product does
/// not fit in `u128`.
#[inline]
#[must_use]
fn checked_notional(quantity: u64, price: u128) -> Option<u128> {
    u128::from(quantity).checked_mul(price)
}

/// Checked increment of an `AtomicU64` via a CAS loop (`fetch_update`).
///
/// Returns `Ok(previous)` on success, or `Err(current)` without storing
/// anything when `current + delta` would overflow. `Relaxed` on both
/// success and failure: see [`RiskCounters`].
#[inline]
fn checked_add_u64(counter: &AtomicU64, delta: u64) -> Result<u64, u64> {
    counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(delta)
    })
}

/// Checked increment of an `AtomicCell<u128>` via a CAS loop.
///
/// Returns `Ok(previous)` on success, or `Err(current)` without storing
/// anything when `current + delta` would overflow. Allocation-free; the
/// loop only retries when another thread changed the cell between the
/// load and the exchange.
#[inline]
fn checked_add_u128(cell: &AtomicCell<u128>, delta: u128) -> Result<u128, u128> {
    let mut current = cell.load();
    loop {
        let next = current.checked_add(delta).ok_or(current)?;
        match cell.compare_exchange(current, next) {
            Ok(previous) => return Ok(previous),
            Err(actual) => current = actual,
        }
    }
}

/// Release `delta` from an `AtomicU64` via a CAS loop.
///
/// Returns `Ok(())` when the counter held at least `delta`. When it held
/// less, the counter is set to zero and `Err(observed)` reports the
/// value it held at the committing exchange; the caller must log and
/// count the anomaly (see the module docs for why zero is the
/// consistent value).
#[inline]
fn release_u64(counter: &AtomicU64, delta: u64) -> Result<(), u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let (next, underflow) = match current.checked_sub(delta) {
            Some(next) => (next, false),
            None => (0, true),
        };
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) if underflow => return Err(current),
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

/// Release `delta` from an `AtomicCell<u128>` via a CAS loop. Same
/// contract as [`release_u64`].
#[inline]
fn release_u128(cell: &AtomicCell<u128>, delta: u128) -> Result<(), u128> {
    let mut current = cell.load();
    loop {
        let (next, underflow) = match current.checked_sub(delta) {
            Some(next) => (next, false),
            None => (0, true),
        };
        match cell.compare_exchange(current, next) {
            Ok(_) if underflow => return Err(current),
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

/// Typed rejection for a notional that is over `limit` or not
/// representable. `limit` is `u128::MAX` when no notional limit is
/// configured (the rejection is then about representability).
#[cold]
#[inline(never)]
fn notional_rejection(
    cfg: &RiskConfig,
    account: Hash32,
    current: u128,
    attempted: Option<u128>,
) -> OrderBookError {
    OrderBookError::RiskMaxNotional {
        account,
        current,
        attempted: attempted.unwrap_or(u128::MAX),
        limit: cfg.max_notional_per_account.unwrap_or(u128::MAX),
    }
}

/// Typed rejection for an open-order count at `limit` or at `u64::MAX`.
#[cold]
#[inline(never)]
fn open_count_rejection(cfg: &RiskConfig, account: Hash32, current: u64) -> OrderBookError {
    OrderBookError::RiskMaxOpenOrders {
        account,
        current,
        limit: cfg.max_open_orders_per_account.unwrap_or(u64::MAX),
    }
}

/// Prepare-phase accumulator for rebuilding the risk state from a
/// snapshot (#243, consumed by the restore path of #207 / #250).
///
/// [`Self::accumulate`] runs against off-book structures and fails with a
/// typed error when an account's open-order count or resting notional
/// would overflow, so a snapshot whose aggregates are not representable
/// is rejected before any live state is touched.
/// [`RiskState::install_rebuild`] then installs the result infallibly
/// in the commit phase.
#[derive(Debug, Default)]
pub(super) struct RiskRebuild {
    /// Per-order entries, in the fixed traversal order they were
    /// accumulated in. Installed into a `DashMap`, so the order does not
    /// leak into book state.
    entries: Vec<(Id, RiskEntry)>,
    /// Per-account `(open_count, resting_notional)` totals. Only looked
    /// up and drained into a `DashMap`; iteration order never reaches
    /// book state.
    totals: HashMap<Hash32, (u64, u128)>,
}

impl RiskRebuild {
    /// Register one restored resting order.
    ///
    /// # Errors
    /// [`OrderBookError::RiskMaxOpenOrders`] (with `limit = u64::MAX`)
    /// when the account's open-order count would overflow, and
    /// [`OrderBookError::RiskMaxNotional`] (with `limit = u128::MAX`)
    /// when the order's notional or the account's resting notional
    /// would overflow `u128`. Configured limits are not enforced here: a
    /// snapshot legitimately carries whatever rested under the limits
    /// in force when it was taken.
    pub(super) fn accumulate(
        &mut self,
        order_id: Id,
        account: Hash32,
        price: u128,
        remaining_qty: u64,
    ) -> Result<(), OrderBookError> {
        let (open, notional) = self.totals.get(&account).copied().unwrap_or((0, 0));
        let next_open = open
            .checked_add(1)
            .ok_or(OrderBookError::RiskMaxOpenOrders {
                account,
                current: open,
                limit: u64::MAX,
            })?;
        let attempted = checked_notional(remaining_qty, price);
        let next_notional = attempted
            .and_then(|delta| notional.checked_add(delta))
            .ok_or(OrderBookError::RiskMaxNotional {
                account,
                current: notional,
                attempted: attempted.unwrap_or(u128::MAX),
                limit: u128::MAX,
            })?;
        self.totals.insert(account, (next_open, next_notional));
        self.entries.push((
            order_id,
            RiskEntry {
                account,
                price,
                remaining_qty,
                generation: 0,
            },
        ));
        Ok(())
    }
}

impl RiskState {
    /// Construct an empty state with no configuration installed.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install or replace the active risk configuration. Counters and
    /// per-order entries are preserved so that history rebuilt from a
    /// previous configuration remains consistent.
    pub fn set_config(&mut self, cfg: RiskConfig) {
        self.config = Some(cfg);
        self.warned_no_reference.store(false, Ordering::Relaxed);
    }

    /// Read-only access to the active configuration, if any.
    #[inline]
    #[must_use]
    pub fn config(&self) -> Option<&RiskConfig> {
        self.config.as_ref()
    }

    /// Drop the active configuration. Counters and per-order entries
    /// are preserved so a subsequent [`Self::set_config`] re-engages
    /// without dropping history.
    pub fn disable(&mut self) {
        self.config = None;
    }

    /// Number of risk accounting anomalies observed since this state was
    /// created (#243): a release larger than the account's counter (a
    /// double release), a fill larger than the order's tracked
    /// remainder, or a post-trade counter increment that would overflow.
    ///
    /// Each anomaly is also logged at `WARN` / `ERROR` with the order,
    /// account and counter involved. A non-zero value means the
    /// per-account counters were corrected (clamped to zero, or the
    /// increment skipped) and should be investigated. The count stops at
    /// `u64::MAX`; it is diagnostic, not protocol state, and is not part
    /// of the snapshot.
    #[inline]
    #[must_use]
    pub fn accounting_anomalies(&self) -> u64 {
        self.accounting_anomalies.load(Ordering::Relaxed)
    }

    /// Count one accounting anomaly (cold path).
    #[cold]
    #[inline(never)]
    fn count_anomaly(&self) {
        // Checked increment; a count already at `u64::MAX` stays there
        // (`fetch_update` returns `Err` and stores nothing).
        let _ = checked_add_u64(&self.accounting_anomalies, 1);
        crate::orderbook::metrics::record_risk_accounting_anomaly();
    }

    /// Log and count a release that exceeded its counter.
    #[cold]
    #[inline(never)]
    fn note_release_underflow(
        &self,
        order_id: Id,
        account: Hash32,
        counter: &'static str,
        observed: u128,
        released: u128,
    ) {
        self.count_anomaly();
        warn!(
            order_id = %order_id,
            account = %account,
            counter,
            observed,
            released,
            "risk: release exceeds the account counter (double release or accounting bug); counter set to zero"
        );
    }

    /// Release one order's contribution from `account`'s counters,
    /// logging and counting any underflow. `close` also releases one
    /// open-order slot. Takes only a read guard on the counters shard.
    #[inline]
    fn release(&self, order_id: Id, account: Hash32, notional: Option<u128>, close: bool) {
        let Some(counters) = self.counters.get(&account) else {
            return;
        };
        // An unrepresentable release can only come from an entry whose
        // quantity grew past what admission accepted; release everything
        // and report it as an underflow.
        let released = notional.unwrap_or(u128::MAX);
        if let Err(observed) = release_u128(&counters.resting_notional, released) {
            self.note_release_underflow(order_id, account, "resting_notional", observed, released);
        }
        if close && let Err(observed) = release_u64(&counters.open_count, 1) {
            self.note_release_underflow(order_id, account, "open_count", u128::from(observed), 1);
        }
    }

    /// Pre-trade limit-order admission check.
    ///
    /// Runs three checks in order: per-account open-order count,
    /// per-account notional, and price band. The price-band check is
    /// skipped when `reference_price` is `None` (caller resolved no
    /// reference; e.g. empty book and no trades yet). The first two also
    /// reject an admission the counters could not represent, whether or
    /// not the limit is configured (see [`RiskConfig`]).
    ///
    /// Allocation-free on the happy path. Cold rejection allocates one
    /// error variant.
    #[inline]
    pub(super) fn check_limit_admission(
        &self,
        account: Hash32,
        price: u128,
        quantity: u64,
        reference_price: Option<u128>,
    ) -> Result<(), OrderBookError> {
        let Some(cfg) = self.config.as_ref() else {
            return Ok(());
        };

        // One shard read for both counters.
        let (open_count, resting_notional) = self
            .counters
            .get(&account)
            .map(|c| {
                (
                    c.open_count.load(Ordering::Relaxed),
                    c.resting_notional.load(),
                )
            })
            .unwrap_or((0, 0));

        // 1. Per-account open-order count.
        let at_limit = cfg
            .max_open_orders_per_account
            .is_some_and(|limit| open_count >= limit);
        if at_limit || open_count.checked_add(1).is_none() {
            return Err(open_count_rejection(cfg, account, open_count));
        }

        // 2. Per-account notional: `current + attempted` must be
        // representable and within the limit. An overflow is never a pass.
        let attempted = checked_notional(quantity, price);
        match attempted.and_then(|a| resting_notional.checked_add(a)) {
            None => {
                return Err(notional_rejection(
                    cfg,
                    account,
                    resting_notional,
                    attempted,
                ));
            }
            Some(projected) => {
                if cfg
                    .max_notional_per_account
                    .is_some_and(|limit| projected > limit)
                {
                    return Err(notional_rejection(
                        cfg,
                        account,
                        resting_notional,
                        attempted,
                    ));
                }
            }
        }

        // 3. Price band against a reference price.
        self.check_price_band(cfg, price, reference_price)?;

        Ok(())
    }

    /// Price-band check shared by [`Self::check_limit_admission`] and
    /// [`Self::check_modify_admission`].
    ///
    /// Rejects when the deviation of `price` from the resolved
    /// `reference_price` *strictly* exceeds `cfg.price_band_bps`. The
    /// comparison cross-multiplies (`diff * 10_000` vs.
    /// `bps_limit * reference`) instead of dividing so the band never
    /// under-enforces: truncating integer division would floor the bps,
    /// letting an order whose true deviation is fractionally above the
    /// band round down to the limit and slip through (#113). An order
    /// exactly at the limit is admitted, preserving the original
    /// strict-`>` boundary semantics.
    ///
    /// Both products are checked (#243); before, both saturated to
    /// `u128::MAX` at extreme prices, compared equal, and any deviation
    /// passed. Now: only `diff * 10_000` overflowing means the deviation
    /// exceeds the (representable) band, so reject; only
    /// `bps_limit * reference` overflowing means the band exceeds every
    /// representable deviation, so pass; both overflowing falls back to
    /// the exact overflow-free comparison in [`band_breach_wide`]. The
    /// verdict is therefore exact over the whole `u128` domain, and the
    /// common (non-overflowing) path costs the same two multiplications
    /// as before.
    ///
    /// Skips silently (warning once per book) when the band is configured
    /// but no reference price is currently available.
    #[inline]
    fn check_price_band(
        &self,
        cfg: &RiskConfig,
        price: u128,
        reference_price: Option<u128>,
    ) -> Result<(), OrderBookError> {
        if let (Some(bps_limit), Some(reference)) = (cfg.price_band_bps, reference_price) {
            if reference > 0 {
                let diff = price.abs_diff(reference);
                let scaled_diff = diff.checked_mul(BPS_SCALE);
                let band = u128::from(bps_limit).checked_mul(reference);
                let breach = match (scaled_diff, band) {
                    (Some(scaled), Some(band)) => scaled > band,
                    (None, Some(_)) => true,
                    (Some(_), None) => false,
                    (None, None) => band_breach_wide(diff, reference, bps_limit),
                };
                if breach {
                    return Err(Self::price_band_rejection(
                        price,
                        reference,
                        diff,
                        scaled_diff,
                        bps_limit,
                    ));
                }
            }
        } else if cfg.price_band_bps.is_some()
            && cfg.reference_price.is_some()
            && reference_price.is_none()
        {
            // Band is configured but no reference is currently
            // available (empty book + no trades). Warn once per book
            // and skip the check.
            if self
                .warned_no_reference
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!(
                    "risk: price-band check configured but no reference price available; \
                     check skipped until a trade or two-sided book establishes a reference"
                );
            }
        }

        Ok(())
    }

    /// Build the [`OrderBookError::RiskPriceBand`] payload. The floored
    /// deviation is recomputed only for display: exact when
    /// `diff * 10_000` fits in `u128`, otherwise split into whole and
    /// fractional parts of `diff / reference` (the fractional part is
    /// approximated only when `remainder * 10_000` overflows too), and
    /// reported as `u32::MAX` when it does not fit in `u32`.
    #[cold]
    #[inline(never)]
    fn price_band_rejection(
        submitted: u128,
        reference: u128,
        diff: u128,
        scaled_diff: Option<u128>,
        limit_bps: u32,
    ) -> OrderBookError {
        let deviation_bps = scaled_diff
            .and_then(|scaled| scaled.checked_div(reference))
            .or_else(|| {
                let whole = diff.checked_div(reference)?.checked_mul(BPS_SCALE)?;
                let remainder = diff.checked_rem(reference)?;
                let fraction = remainder
                    .checked_mul(BPS_SCALE)
                    .and_then(|scaled| scaled.checked_div(reference))
                    .or_else(|| remainder.checked_div(reference.checked_div(BPS_SCALE)?))?;
                whole.checked_add(fraction)
            })
            .and_then(|bps| u32::try_from(bps).ok())
            .unwrap_or(u32::MAX);
        OrderBookError::RiskPriceBand {
            submitted,
            reference,
            deviation_bps,
            limit_bps,
        }
    }

    /// Pre-trade admission check for an in-place **modify** of a resting
    /// order (`UpdatePrice` / `UpdatePriceAndQuantity` / `Replace`).
    ///
    /// A modify replaces one resting order with another: the account's
    /// `open_count` is unchanged (one out, one in) and — critically — the
    /// *original* order's contribution is still counted in the account's
    /// counters at the moment this runs (the validate-first guard checks
    /// admission *before* cancelling, #98). Reusing
    /// [`Self::check_limit_admission`] here would therefore double-count
    /// the original and falsely reject. This check instead:
    ///
    /// - runs the **price band** on `new_price` (same logic as the
    ///   limit-admission band, via [`Self::check_price_band`]),
    /// - runs the **notional** check using the *projected* resting
    ///   notional `current - old_price*old_qty + new_price*new_qty` (the
    ///   old order's contribution is already inside `current`), with
    ///   checked `u128` arithmetic: an unrepresentable projection is
    ///   rejected whether or not `max_notional_per_account` is set,
    ///   exactly as the post-cancel admission would reject it,
    /// - does **not** check `max_open_orders_per_account` (a modify cannot
    ///   change the resting order count).
    ///
    /// Returns `Ok(())` when no [`RiskConfig`] is installed.
    ///
    /// # Errors
    /// Returns [`OrderBookError::RiskMaxNotional`] or
    /// [`OrderBookError::RiskPriceBand`] when the projected modify would
    /// breach the corresponding limit.
    #[inline]
    pub(super) fn check_modify_admission(
        &self,
        order_id: Id,
        account: Hash32,
        new_price: u128,
        new_qty: u64,
        reference_price: Option<u128>,
    ) -> Result<(), OrderBookError> {
        let Some(cfg) = self.config.as_ref() else {
            return Ok(());
        };

        // Look up the original order's tracked risk contribution. If it is NOT
        // tracked — admitted while no `RiskConfig` was installed, then a config
        // was installed before this modify — the modify is, from the risk
        // layer's view, a genuinely new admission: `on_cancel` will be a no-op
        // for the untracked original and `add_order` runs FULL admission
        // post-cancel. Mirror that exactly (full `check_limit_admission`,
        // including the open-order count) so the validate-first guard predicts
        // the post-cancel verdict and never passes a modify that `add_order`
        // would then reject — which would destroy the original.
        let old_contribution = {
            let Some(entry) = self.orders.get(&order_id) else {
                return self.check_limit_admission(account, new_price, new_qty, reference_price);
            };
            checked_notional(entry.remaining_qty, entry.price)
        };

        // Tracked original: a modify is net one-out-one-in, so `open_count` is
        // unchanged (skip that gate) and only the notional and price band can
        // newly breach. Project the account's resting notional by swapping the
        // original's contribution (already inside `current`) for the new one.
        let current = self
            .counters
            .get(&account)
            .map(|c| c.resting_notional.load())
            .unwrap_or(0);
        let new_contribution = checked_notional(new_qty, new_price);
        // Releasing more than `current` mirrors what `on_cancel` would do
        // on the post-cancel path: the (logged) release sets the counter to
        // zero, so the projection starts from zero too. An unrepresentable
        // old contribution cannot come from a checked admission; treat it
        // as releasing everything, like `on_cancel`.
        let base = old_contribution
            .and_then(|old| current.checked_sub(old))
            .unwrap_or(0);
        let projected = new_contribution.and_then(|new| base.checked_add(new));
        let breach = match projected {
            None => true,
            Some(projected) => cfg
                .max_notional_per_account
                .is_some_and(|limit| projected > limit),
        };
        if breach {
            return Err(notional_rejection(cfg, account, current, new_contribution));
        }

        // Price band against the reference price on the new limit price.
        self.check_price_band(cfg, new_price, reference_price)?;

        Ok(())
    }

    /// Pre-trade market-order admission check.
    ///
    /// Per design decision C, market orders skip every risk check (no
    /// submitted price for the band, no resting contribution for the
    /// open-order or notional counters). This helper exists to keep
    /// the documented gate ordering consistent across submit and add
    /// paths and reserves room for a future per-account market-order
    /// rate limiter without breaking the call shape.
    #[inline]
    pub(super) fn check_market_admission(&self, _account: Hash32) -> Result<(), OrderBookError> {
        Ok(())
    }

    /// Reserve the risk contribution of an order that is about to rest.
    ///
    /// Claims `order_id` in the per-order map and increments the
    /// account's `open_count` and `resting_notional` with checked CAS
    /// loops, all while holding the order map shard's write guard, so the
    /// claim and the counters commit together. All or nothing: on error no
    /// counter is changed and no entry is inserted, so the caller must not
    /// rest the order. Call it **before** placing the order on its level
    /// and hand the returned [`RiskReservation`] to
    /// [`Self::release_reservation`] if the placement then fails.
    ///
    /// An id that is already tracked is rejected before any counter is
    /// touched (#243 review): the book's duplicate-id check is not atomic
    /// with admission, and overwriting the entry would let a same-id loser
    /// release the winner's contribution.
    ///
    /// The pre-trade [`Self::check_limit_admission`] already rejects any
    /// submission whose worst-case notional would overflow, so the
    /// counter errors can only occur when concurrent admissions on the
    /// same account raced past that check.
    ///
    /// Allocation only when a new account counter is created
    /// (first-ever order from that account on this book) or the
    /// per-order map's bucket grows.
    ///
    /// # Errors
    /// [`OrderBookError::DuplicateOrderId`] when `order_id` is already
    /// tracked, [`OrderBookError::RiskMaxOpenOrders`] when the open-order
    /// count would overflow `u64`, [`OrderBookError::RiskMaxNotional`]
    /// when the order's notional or the account's resting notional would
    /// overflow `u128` (`limit` carries the configured limit, or the
    /// type's `MAX` when none is set), and
    /// [`OrderBookError::InvalidOperation`] if the reservation generation
    /// counter is exhausted (`u64::MAX` admissions).
    pub(super) fn on_admission(
        &self,
        order_id: Id,
        account: Hash32,
        price: u128,
        remaining_qty: u64,
    ) -> Result<RiskReservation, OrderBookError> {
        let Some(cfg) = self.config.as_ref() else {
            return Ok(RiskReservation {
                order_id,
                generation: 0,
            });
        };
        let generation = self.next_generation()?;
        let notional_delta = checked_notional(remaining_qty, price);

        // Lock order: the orders shard, then the counters shard. No other
        // path holds a counters guard while taking an orders guard, so the
        // nesting cannot deadlock.
        let slot = match self.orders.entry(order_id) {
            dashmap::Entry::Occupied(_) => {
                return Err(OrderBookError::DuplicateOrderId { order_id });
            }
            dashmap::Entry::Vacant(slot) => slot,
        };

        let reserved = {
            // `entry` holds the counters shard's write guard for the whole
            // block, which serializes this reservation against
            // `evict_if_zeroed` (see there). It is dropped at the end of the
            // block, before the eviction below takes the same shard.
            let counters = self.counters.entry(account).or_default();
            match notional_delta {
                None => Err(notional_rejection(
                    cfg,
                    account,
                    counters.resting_notional.load(),
                    None,
                )),
                Some(delta) => match checked_add_u64(&counters.open_count, 1) {
                    Err(current) => Err(open_count_rejection(cfg, account, current)),
                    Ok(_) => match checked_add_u128(&counters.resting_notional, delta) {
                        Ok(_) => Ok(()),
                        Err(current) => {
                            // Roll back the open-count reservation so the
                            // failure leaves no trace. It was incremented
                            // just above, so the release cannot underflow;
                            // if it somehow does, it is logged and counted.
                            if let Err(observed) = release_u64(&counters.open_count, 1) {
                                self.note_release_underflow(
                                    order_id,
                                    account,
                                    "open_count",
                                    u128::from(observed),
                                    1,
                                );
                            }
                            Err(notional_rejection(cfg, account, current, Some(delta)))
                        }
                    },
                },
            }
        };
        if let Err(err) = reserved {
            drop(slot);
            // A first-ever account leaves a zeroed counters entry behind;
            // reclaim it.
            self.evict_if_zeroed(account);
            return Err(err);
        }

        slot.insert(RiskEntry {
            account,
            price,
            remaining_qty,
            generation,
        });
        Ok(RiskReservation {
            order_id,
            generation,
        })
    }

    /// Next reservation generation (never `0`). Checked: exhaustion after
    /// `u64::MAX` admissions is a typed error, never a wrap that could make
    /// two live reservations share a generation.
    #[inline]
    fn next_generation(&self) -> Result<u64, OrderBookError> {
        checked_add_u64(&self.generations, 1)
            .ok()
            .and_then(|previous| previous.checked_add(1))
            .ok_or_else(Self::generation_exhausted)
    }

    #[cold]
    #[inline(never)]
    fn generation_exhausted() -> OrderBookError {
        OrderBookError::InvalidOperation {
            message: "risk reservation generation counter exhausted".to_string(),
        }
    }

    /// Undo a [`Self::on_admission`] reservation for an order that did not
    /// rest. Releases the entry and its counter contribution only when the
    /// entry still carries this reservation's generation, so it can never
    /// release another admission's entry for the same id. No-op for a
    /// reservation taken without a `RiskConfig`.
    pub(super) fn release_reservation(&self, reservation: RiskReservation) {
        if reservation.generation == 0 {
            return;
        }
        let Some((_, entry)) = self.orders.remove_if(&reservation.order_id, |_, entry| {
            entry.generation == reservation.generation
        }) else {
            return;
        };
        self.release(
            reservation.order_id,
            entry.account,
            checked_notional(entry.remaining_qty, entry.price),
            true,
        );
        self.evict_if_zeroed(entry.account);
    }

    /// Hook per fill against a resting maker order.
    ///
    /// Decrements the maker's `remaining_qty` and the per-account
    /// `resting_notional`. If the maker is fully filled, decrements
    /// `open_count`, removes the per-order entry, and evicts the
    /// per-account counters when the account drops to zero resting
    /// orders and zero notional (see [`Self::evict_if_zeroed`]). No-op
    /// when the maker is not tracked (e.g. risk was disabled when the
    /// maker was admitted, or the entry was already evicted by a prior
    /// cancel in the same submit call).
    ///
    /// `resting_notional` is reduced using the maker's **stored
    /// admission price** (`RiskEntry::price`), not the passed
    /// `maker_price`. The account's resting exposure was booked at the
    /// admission price, so admission / fill / cancel stay self-balancing
    /// regardless of the execution price. Today the matcher always
    /// trades a maker at its resting price (and a modify / repricing
    /// re-admits a fresh entry at the new price), so the two coincide; a
    /// mismatch is logged at `WARN` because a future price-improvement
    /// path that breaks that equality must revisit this accounting.
    ///
    /// A fill larger than the tracked remainder releases only the
    /// tracked remainder (what was booked) and is logged and counted as
    /// an accounting anomaly; a release larger than a counter sets it to
    /// zero with the same treatment (see the module docs).
    pub(super) fn on_fill(&self, maker_id: Id, filled_qty: u64, maker_price: u128) {
        if self.config.is_none() {
            return;
        }
        // Read-modify-write the entry. Use `get_mut` for the partial
        // case and `remove` for the full case to keep the map small.
        let (account, entry_price, released_qty, fully_filled, tracked_qty) = {
            let Some(mut entry) = self.orders.get_mut(&maker_id) else {
                return;
            };
            let tracked_qty = entry.remaining_qty;
            let (new_remaining, released_qty) = match tracked_qty.checked_sub(filled_qty) {
                Some(new_remaining) => (new_remaining, filled_qty),
                None => (0, tracked_qty),
            };
            let account = entry.account;
            let entry_price = entry.price;
            entry.remaining_qty = new_remaining;
            (
                account,
                entry_price,
                released_qty,
                new_remaining == 0,
                tracked_qty,
            )
        };

        if released_qty != filled_qty {
            self.note_fill_overshoot(maker_id, account, tracked_qty, filled_qty);
        }
        if maker_price != entry_price {
            Self::note_price_mismatch(maker_id, maker_price, entry_price);
        }

        // Self-balancing: release the filled portion at the admission
        // price the notional was booked at.
        self.release(
            maker_id,
            account,
            checked_notional(released_qty, entry_price),
            fully_filled,
        );
        // `release` drops its read guard on the counters shard before
        // returning, BEFORE `evict_if_zeroed` takes the write guard on the
        // same shard. DashMap shards are non-reentrant, so this ordering
        // matters: do not widen a read-guard scope across the eviction call
        // or it self-deadlocks.
        if fully_filled {
            self.orders.remove(&maker_id);
            self.evict_if_zeroed(account);
        }
    }

    /// Log and count a fill larger than the tracked remainder.
    #[cold]
    #[inline(never)]
    fn note_fill_overshoot(&self, order_id: Id, account: Hash32, tracked: u64, filled: u64) {
        self.count_anomaly();
        warn!(
            order_id = %order_id,
            account = %account,
            tracked,
            filled,
            "risk: fill exceeds the maker's tracked remaining quantity; releasing only the tracked remainder"
        );
    }

    /// Log a maker fill priced away from the entry's admission price.
    /// Accounting stays self-balancing (it uses the admission price), so
    /// this is not counted as an anomaly.
    #[cold]
    #[inline(never)]
    fn note_price_mismatch(order_id: Id, maker_price: u128, entry_price: u128) {
        warn!(
            order_id = %order_id,
            maker_price,
            entry_price,
            "risk: maker filled away from its admission price; resting notional released at the admission price"
        );
    }

    /// Pre-book the notional of an in-place quantity **increase** before
    /// the price level applies it (#211, #243 review).
    ///
    /// `projected_remaining` is the order's total remaining quantity after
    /// the update. When it exceeds the tracked remainder, the extra
    /// notional is added to the account's counter with a checked CAS; an
    /// overflow is a typed rejection and nothing is changed, so the level
    /// must not be updated. A decrease reserves nothing. The returned
    /// [`QuantityReservation`] must be settled with
    /// [`Self::commit_quantity_update`] after the level update succeeds, or
    /// [`Self::rollback_quantity_update`] if it does not happen.
    ///
    /// Untracked orders (no `RiskConfig`, or admitted before one was
    /// installed) get an empty reservation whose settlement is a no-op.
    ///
    /// # Errors
    /// [`OrderBookError::RiskMaxNotional`] when the increase's notional,
    /// or the account's resting notional plus it, would overflow `u128`.
    pub(super) fn reserve_quantity_update(
        &self,
        order_id: Id,
        projected_remaining: u64,
    ) -> Result<QuantityReservation, OrderBookError> {
        let untracked = QuantityReservation {
            order_id,
            account: Hash32::zero(),
            price: 0,
            generation: 0,
            tracked: false,
            reserved_qty: 0,
        };
        let Some(cfg) = self.config.as_ref() else {
            return Ok(untracked);
        };
        let (account, price, tracked_qty, generation) = {
            let Some(entry) = self.orders.get(&order_id) else {
                return Ok(untracked);
            };
            (
                entry.account,
                entry.price,
                entry.remaining_qty,
                entry.generation,
            )
        };
        let mut reservation = QuantityReservation {
            order_id,
            account,
            price,
            generation,
            tracked: true,
            reserved_qty: 0,
        };
        let Some(increase) = projected_remaining
            .checked_sub(tracked_qty)
            .filter(|increase| *increase > 0)
        else {
            return Ok(reservation);
        };
        let Some(counters) = self.counters.get(&account) else {
            // Tracked entry without counters cannot happen through the
            // hooks; there is nothing to reserve against.
            return Ok(reservation);
        };
        let delta = checked_notional(increase, price);
        match delta.map(|delta| checked_add_u128(&counters.resting_notional, delta)) {
            Some(Ok(_)) => {
                reservation.reserved_qty = increase;
                Ok(reservation)
            }
            Some(Err(current)) => Err(notional_rejection(cfg, account, current, delta)),
            None => Err(notional_rejection(
                cfg,
                account,
                counters.resting_notional.load(),
                None,
            )),
        }
    }

    /// Settle a [`QuantityReservation`] once the price level applied the
    /// update: set the entry's remaining quantity to `actual_remaining`
    /// (the level's post-update total) and adjust the account's notional
    /// from what is booked (the entry's current remainder plus the
    /// reservation) to exactly `actual_remaining × price`.
    ///
    /// The adjustment is normally a release (or nothing). An extra
    /// increment is only needed if the level stored more than was
    /// reserved, e.g. a fill raced in between; if that increment would
    /// overflow, the entry keeps the booked quantity and the anomaly is
    /// logged at `ERROR` and counted, so later releases stay within what
    /// the counter holds.
    pub(super) fn commit_quantity_update(
        &self,
        reservation: QuantityReservation,
        actual_remaining: u64,
    ) {
        if !reservation.tracked {
            return;
        }
        let QuantityReservation {
            order_id,
            account,
            price,
            reserved_qty,
            generation,
            ..
        } = reservation;
        let current = {
            let entry = self
                .orders
                .get_mut(&order_id)
                .filter(|entry| entry.generation == generation);
            let Some(mut entry) = entry else {
                // The order left the risk map in between (cancelled or
                // fully filled): its release did not include the
                // reservation, so give the reservation back.
                self.rollback_quantity_update(reservation);
                return;
            };
            let current = entry.remaining_qty;
            entry.remaining_qty = actual_remaining;
            current
        };
        // Booked for this order: `current + reserved_qty`. Target:
        // `actual_remaining`. Settle the difference.
        let booked = current.checked_add(reserved_qty);
        match booked.map(|booked| (actual_remaining.checked_sub(booked), booked)) {
            Some((Some(extra), booked)) if extra > 0 => {
                let added = self.counters.get(&account).is_none_or(|counters| {
                    checked_notional(extra, price).is_some_and(|delta| {
                        checked_add_u128(&counters.resting_notional, delta).is_ok()
                    })
                });
                if !added {
                    self.keep_booked_quantity(order_id, account, actual_remaining, booked);
                }
            }
            Some((None, booked)) => {
                if let Some(surplus) = booked.checked_sub(actual_remaining) {
                    self.release(order_id, account, checked_notional(surplus, price), false);
                }
            }
            Some((Some(_), _)) => {}
            // Unreachable: fills only lower `current`, so
            // `current + reserved_qty <= projected_remaining <= u64::MAX`.
            // Give the reservation back and report it.
            None => {
                self.rollback_quantity_update(reservation);
                self.keep_booked_quantity(order_id, account, actual_remaining, current);
            }
        }
    }

    /// Release a [`QuantityReservation`] whose level update did not
    /// happen (rejected, absent order, or error).
    pub(super) fn rollback_quantity_update(&self, reservation: QuantityReservation) {
        if !reservation.tracked || reservation.reserved_qty == 0 {
            return;
        }
        self.release(
            reservation.order_id,
            reservation.account,
            checked_notional(reservation.reserved_qty, reservation.price),
            false,
        );
        // The order may have left the book in between; its cancel could
        // not evict the account while the reservation was still booked.
        self.evict_if_zeroed(reservation.account);
    }

    /// Cold path of [`Self::commit_quantity_update`]: the extra increment
    /// would overflow, so track the quantity that is actually booked.
    #[cold]
    #[inline(never)]
    fn keep_booked_quantity(
        &self,
        order_id: Id,
        account: Hash32,
        actual_remaining: u64,
        booked: u64,
    ) {
        if let Some(mut entry) = self.orders.get_mut(&order_id)
            && entry.remaining_qty == actual_remaining
        {
            entry.remaining_qty = booked;
        }
        self.count_anomaly();
        error!(
            order_id = %order_id,
            account = %account,
            booked,
            "risk: settling a quantity update would overflow the account's resting notional; risk tracking keeps the booked quantity"
        );
    }

    /// Atomically evict an account's [`RiskCounters`] once it has no
    /// resting orders and zero resting notional, so the per-account map
    /// tracks currently-active accounts instead of growing with every
    /// distinct account ever seen.
    ///
    /// Race-safe against [`Self::on_admission`]. `remove_if` evaluates the
    /// predicate while holding the counters shard's write lock, and
    /// `on_admission` holds that *same* lock across its whole
    /// `entry(account).or_default()` plus the `open_count` /
    /// `resting_notional` increments — the `RefMut` is bound for the rest
    /// of that block — so this never observes a half-incremented counter.
    /// The two serialize: either the admission commits first and the
    /// predicate reads a non-zero `open_count` and keeps the entry, or the
    /// eviction commits first and the admission recreates the entry from
    /// zero. Eviction therefore reliably reclaims any account that reaches
    /// a genuine zero-resting state (the decrement that zeroes the account
    /// is the same call that attempts the eviction) without ever evicting
    /// an account that still has — or is concurrently regaining — a
    /// resting order.
    #[inline]
    fn evict_if_zeroed(&self, account: Hash32) {
        self.counters.remove_if(&account, |_, c| {
            c.open_count.load(Ordering::Relaxed) == 0 && c.resting_notional.load() == 0
        });
    }

    /// Hook on cancel of a resting order.
    ///
    /// Removes the entry and decrements both per-account counters
    /// using the entry's stored `remaining_qty` and `price`, then
    /// evicts the per-account counters when the account drops to zero
    /// resting orders and zero notional (see [`Self::evict_if_zeroed`]).
    /// No-op when the entry is not present.
    ///
    /// A release larger than a counter is logged, counted, and sets the
    /// counter to zero — same treatment as [`Self::on_fill`].
    pub(super) fn on_cancel(&self, order_id: Id) {
        if self.config.is_none() {
            return;
        }
        let Some((_, entry)) = self.orders.remove(&order_id) else {
            return;
        };
        self.release(
            order_id,
            entry.account,
            checked_notional(entry.remaining_qty, entry.price),
            true,
        );
        // `release` dropped its read guard before the write guard in
        // `evict_if_zeroed` — same non-reentrant shard, must not overlap.
        self.evict_if_zeroed(entry.account);
    }

    /// Hook when the matcher removes a maker from its level (#243 review).
    ///
    /// For a normally exhausted maker [`Self::on_fill`] already removed
    /// the entry and this is a no-op. A non-auto-replenishing reserve
    /// maker is removed when its visible tranche is exhausted, discarding
    /// its hidden tranche without a trade (#230); the fills released only
    /// the visible quantity, so the discarded remainder is released here,
    /// exactly like a cancel. Without this the discarded quantity stayed
    /// booked forever and counted against the account's limits.
    #[inline]
    pub(super) fn on_maker_removed(&self, maker_id: Id) {
        self.on_cancel(maker_id);
    }

    /// Install the per-order entries and per-account counters computed
    /// by a [`RiskRebuild`] in the prepare phase of a snapshot restore.
    ///
    /// Infallible: every aggregate was checked by
    /// [`RiskRebuild::accumulate`]. Expects an empty state (the restore
    /// calls [`Self::clear`] first) and is a no-op when no `RiskConfig`
    /// is installed, like [`Self::on_admission`].
    pub(super) fn install_rebuild(&self, rebuild: &RiskRebuild) {
        if self.config.is_none() {
            return;
        }
        for (account, (open_count, resting_notional)) in &rebuild.totals {
            self.counters.insert(
                *account,
                RiskCounters {
                    open_count: AtomicU64::new(*open_count),
                    resting_notional: AtomicCell::new(*resting_notional),
                },
            );
        }
        for (order_id, entry) in &rebuild.entries {
            self.orders.insert(*order_id, *entry);
        }
    }

    /// Drop all per-order risk entries and per-account counters in one shot.
    ///
    /// Used by [`OrderBook::cancel_all_orders`](super::book::OrderBook::cancel_all_orders),
    /// which empties the entire book in bulk — the per-order [`Self::on_cancel`]
    /// accounting collapses to a single clear, and leaving the maps populated would
    /// strand phantom open-order / notional counters that reject new flow (#99).
    /// No-op semantics when no `RiskConfig` is installed (the maps are already empty).
    pub(super) fn clear(&self) {
        self.orders.clear();
        self.counters.clear();
    }
}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::cast_possible_truncation, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use pricelevel::Id;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    fn account(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    fn open_count_of(state: &RiskState, acct: Hash32) -> u64 {
        state
            .counters
            .get(&acct)
            .map(|c| c.open_count.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    #[test]
    fn test_concurrent_admission_over_admission_is_bounded_issue_116() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const THREADS: usize = 16;
        const LIMIT: u64 = 4;

        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(LIMIT));
        let state = Arc::new(state);
        let acct = account(7);
        // Barrier releases all threads together to maximize the documented
        // check-then-increment race window.
        let barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    if state.check_limit_admission(acct, 100, 1, Some(100)).is_ok() {
                        let _ = state
                            .on_admission(Id::from_u64(i as u64), acct, 100, 1)
                            .expect("admission");
                        1u64
                    } else {
                        0
                    }
                })
            })
            .collect();

        let admitted: u64 = handles
            .into_iter()
            .map(|h| h.join().expect("admission thread"))
            .sum();

        let open_count = open_count_of(&state, acct);

        // The counter must equal the number of successful admissions.
        assert_eq!(
            open_count, admitted,
            "open_count must match the admissions that incremented it"
        );
        // A reject can only happen once the count reaches the limit, so at least
        // `LIMIT` admissions always occur.
        assert!(open_count >= LIMIT, "at least the limit is admitted");
        // Documented bound: over-admission never exceeds the limit by more than
        // one in-flight admission per racing thread.
        assert!(
            open_count <= LIMIT + THREADS as u64,
            "over-admission must stay bounded by limit + thread_count, got {open_count}"
        );
    }

    #[test]
    fn test_concurrent_fill_cancel_never_wraps_open_count_issue_116() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const ORDERS: u64 = 32;

        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10_000));
        let acct = account(9);
        // Pre-admit ORDERS resting orders (open_count == ORDERS).
        for i in 0..ORDERS {
            let _ = state
                .on_admission(Id::from_u64(i), acct, 100, 10)
                .expect("admission");
        }
        assert_eq!(open_count_of(&state, acct), ORDERS);

        let state = Arc::new(state);
        // Race a full fill against a cancel for every order: the saturating
        // decrement must never wrap `open_count` to a huge value (which would
        // lock the account out by reading as "at limit" forever).
        let barrier = Arc::new(Barrier::new((ORDERS * 2) as usize));
        let mut handles = Vec::new();
        for i in 0..ORDERS {
            for which in 0..2u8 {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                handles.push(thread::spawn(move || {
                    barrier.wait();
                    if which == 0 {
                        state.on_fill(Id::from_u64(i), 10, 100); // full fill
                    } else {
                        state.on_cancel(Id::from_u64(i));
                    }
                }));
            }
        }
        for h in handles {
            h.join().expect("fill/cancel thread");
        }

        let open_count = open_count_of(&state, acct);
        let resting_notional = state
            .counters
            .get(&acct)
            .map(|c| c.resting_notional.load())
            .unwrap_or(0);

        // Each order is decremented exactly once (whichever of fill/cancel wins
        // the DashMap entry removal; the other is a no-op), and a saturating
        // decrement never wraps — so both counters land at 0, never at a huge
        // wrapped value that would lock the account out.
        assert_eq!(open_count, 0, "all orders removed exactly once; no wrap");
        assert_eq!(
            resting_notional, 0,
            "resting_notional also reaches 0 without wrap"
        );
    }

    #[test]
    fn test_risk_config_builder() {
        let cfg = RiskConfig::new()
            .with_max_open_orders_per_account(5)
            .with_max_notional_per_account(1_000_000)
            .with_price_band_bps(500, ReferencePriceSource::LastTrade);
        assert_eq!(cfg.max_open_orders_per_account, Some(5));
        assert_eq!(cfg.max_notional_per_account, Some(1_000_000));
        assert_eq!(cfg.price_band_bps, Some(500));
        assert_eq!(cfg.reference_price, Some(ReferencePriceSource::LastTrade));
    }

    #[test]
    fn test_risk_state_no_config_is_passthrough() {
        let state = RiskState::new();
        let acct = account(1);
        let order_id = new_id();

        // Every check returns Ok.
        assert!(
            state
                .check_limit_admission(acct, 100, 10, Some(100))
                .is_ok()
        );
        assert!(state.check_market_admission(acct).is_ok());

        // Hooks are no-ops.
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");
        state.on_fill(order_id, 5, 100);
        state.on_cancel(order_id);

        // Counters never populated when no config is installed.
        assert!(state.counters.is_empty());
        assert!(state.orders.is_empty());
    }

    #[test]
    fn test_on_admission_then_on_cancel_round_trip() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(10)
                .with_max_notional_per_account(1_000_000),
        );

        let acct = account(2);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");

        let counters = state
            .counters
            .get(&acct)
            .expect("counters entry created on admission");
        assert_eq!(counters.open_count.load(Ordering::Relaxed), 1);
        assert_eq!(counters.resting_notional.load(), 1_000);
        drop(counters);

        state.on_cancel(order_id);
        // #115: the per-account counters entry is evicted once the account's
        // last resting order is removed (open_count and resting_notional both 0),
        // rather than lingering at zero and growing the map monotonically.
        assert!(
            state.counters.get(&acct).is_none(),
            "counters entry evicted after the account's last order is cancelled"
        );
        assert!(state.counters.is_empty());
        assert!(!state.orders.contains_key(&order_id));
    }

    #[test]
    fn test_on_fill_full_evicts_counters_issue_115() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));

        let acct = account(4);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");

        // Fully fill the account's only resting order: the per-order entry and
        // the now-zeroed per-account counters are both removed.
        state.on_fill(order_id, 10, 100);

        assert!(
            state.counters.get(&acct).is_none(),
            "counters entry evicted after the account's last order is fully filled"
        );
        assert!(state.counters.is_empty());
        assert!(!state.orders.contains_key(&order_id));
    }

    #[test]
    fn test_admission_fill_cancel_notional_self_balances_issue_115() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));

        let acct = account(5);
        let order_id = new_id();
        // Admit 10 @ 100 → resting_notional 1_000.
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");
        assert_eq!(
            state
                .counters
                .get(&acct)
                .map(|c| c.resting_notional.load())
                .unwrap_or(0),
            1_000
        );

        // Partial fill 4 @ 100 releases 400 at the entry's stored admission
        // price → resting_notional 600, open_count still 1 (entry retained).
        state.on_fill(order_id, 4, 100);
        let counters = state
            .counters
            .get(&acct)
            .expect("entry retained on partial");
        assert_eq!(counters.open_count.load(Ordering::Relaxed), 1);
        assert_eq!(counters.resting_notional.load(), 600);
        drop(counters);

        // Cancel the remaining 6 @ 100 releases the last 600 → both counters
        // reach 0 and the account entry is evicted. Admission/fill/cancel
        // self-balance to exactly zero with no residual notional.
        state.on_cancel(order_id);
        assert!(
            state.counters.get(&acct).is_none(),
            "counters self-balance to zero and evict after the last release"
        );
        assert!(state.orders.is_empty());
    }

    #[test]
    fn test_concurrent_admission_vs_eviction_is_consistent_issue_115() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        // Race a full-fill eviction of order A against a fresh admission of
        // order B on the SAME account, repeatedly, to exercise the
        // `evict_if_zeroed` / `on_admission` interleaving on the shared
        // counters shard. The ground-truth invariant that must always hold:
        // `open_count` equals the number of the account's orders still in the
        // per-order map. The eviction must never strand the account by
        // dropping B's increment (phantom under-count) nor wrap a counter.
        const ROUNDS: usize = 500;
        let acct = account(21);
        let a = Id::from_u64(1);
        let b = Id::from_u64(2);

        for round in 0..ROUNDS {
            let mut state = RiskState::new();
            state.set_config(RiskConfig::new().with_max_open_orders_per_account(10_000));
            // Pre-admit A so the account sits at open_count == 1.
            let _ = state.on_admission(a, acct, 100, 1).expect("admission");
            let state = Arc::new(state);
            let barrier = Arc::new(Barrier::new(2));

            let (s1, b1) = (Arc::clone(&state), Arc::clone(&barrier));
            let t1 = thread::spawn(move || {
                b1.wait();
                s1.on_fill(a, 1, 100); // full fill of A → attempts eviction
            });
            let (s2, b2) = (Arc::clone(&state), Arc::clone(&barrier));
            let t2 = thread::spawn(move || {
                b2.wait();
                let _ = s2.on_admission(b, acct, 100, 1).expect("admission"); // concurrent admission of B
            });
            t1.join().expect("fill thread");
            t2.join().expect("admission thread");

            // A is gone, B rests — regardless of who won the race.
            assert!(
                !state.orders.contains_key(&a),
                "round {round}: A fully filled"
            );
            assert!(
                state.orders.contains_key(&b),
                "round {round}: B's entry survives"
            );

            // open_count must equal the account's live resting-order count.
            // B is the only resting order, so this is exactly 1; never 0 (a lost
            // increment / phantom eviction) and never a wrapped value.
            let resting = state.orders.iter().filter(|e| e.account == acct).count() as u64;
            assert_eq!(
                open_count_of(&state, acct),
                resting,
                "round {round}: open_count must track the live resting-order count, never under/overcount"
            );

            // Drain B: the account fully zeroes and the entry is evicted.
            state.on_cancel(b);
            assert!(
                state.counters.get(&acct).is_none(),
                "round {round}: account evicted once its last order is removed"
            );
            assert!(
                state.orders.is_empty(),
                "round {round}: no stranded entries"
            );
        }
    }

    #[test]
    fn test_on_fill_partial_keeps_open_count() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));

        let acct = account(3);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");

        state.on_fill(order_id, 4, 100);

        let counters = state.counters.get(&acct).expect("counters entry present");
        assert_eq!(
            counters.open_count.load(Ordering::Relaxed),
            1,
            "partial fill must not drop open_count"
        );
        assert_eq!(
            counters.resting_notional.load(),
            6 * 100,
            "notional must be reduced by filled_qty * price"
        );
        let entry = state
            .orders
            .get(&order_id)
            .expect("entry retained after partial fill");
        assert_eq!(entry.remaining_qty, 6);
    }

    #[test]
    fn test_on_fill_full_decrements_open_count() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));

        let acct = account(4);
        let keep = new_id();
        let fill = new_id();
        // Two resting orders for the account. Fully filling one decrements
        // open_count by exactly one; the entry is retained because the
        // account still has a resting order (eviction needs both counters at 0).
        let _ = state.on_admission(keep, acct, 100, 10).expect("admission");
        let _ = state.on_admission(fill, acct, 100, 10).expect("admission");

        state.on_fill(fill, 10, 100);

        let counters = state
            .counters
            .get(&acct)
            .expect("entry retained while the account still has a resting order");
        assert_eq!(counters.open_count.load(Ordering::Relaxed), 1);
        assert_eq!(counters.resting_notional.load(), 1_000);
        assert!(!state.orders.contains_key(&fill));
        assert!(state.orders.contains_key(&keep));
    }

    #[test]
    fn test_check_limit_admission_max_open_orders_breach_returns_typed_error() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(2));

        let acct = account(5);
        let _ = state
            .on_admission(new_id(), acct, 100, 1)
            .expect("admission");
        let _ = state
            .on_admission(new_id(), acct, 100, 1)
            .expect("admission");

        let err = state
            .check_limit_admission(acct, 100, 1, Some(100))
            .expect_err("third admission must breach max_open_orders");
        match err {
            OrderBookError::RiskMaxOpenOrders {
                account: a,
                current,
                limit,
            } => {
                assert_eq!(a, acct);
                assert_eq!(current, 2);
                assert_eq!(limit, 2);
            }
            other => panic!("expected RiskMaxOpenOrders, got {other:?}"),
        }
    }

    #[test]
    fn test_check_limit_admission_max_notional_breach_returns_typed_error() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000));

        let acct = account(6);
        // Pre-load 800 of notional.
        let _ = state
            .on_admission(new_id(), acct, 100, 8)
            .expect("admission");

        // Attempt to add 300 more (price=100, qty=3).
        let err = state
            .check_limit_admission(acct, 100, 3, Some(100))
            .expect_err("notional should be exceeded");
        match err {
            OrderBookError::RiskMaxNotional {
                account: a,
                current,
                attempted,
                limit,
            } => {
                assert_eq!(a, acct);
                assert_eq!(current, 800);
                assert_eq!(attempted, 300);
                assert_eq!(limit, 1_000);
            }
            other => panic!("expected RiskMaxNotional, got {other:?}"),
        }
    }

    #[test]
    fn test_check_limit_admission_price_band_breach_returns_typed_error() {
        let mut state = RiskState::new();
        // 100 bps = 1% band.
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );

        let acct = account(7);
        // Reference 1_000_000, submitted 1_100_000 → +10_000 bps deviation.
        let err = state
            .check_limit_admission(acct, 1_100_000, 1, Some(1_000_000))
            .expect_err("price band should be exceeded");
        match err {
            OrderBookError::RiskPriceBand {
                submitted,
                reference,
                deviation_bps,
                limit_bps,
            } => {
                assert_eq!(submitted, 1_100_000);
                assert_eq!(reference, 1_000_000);
                assert_eq!(deviation_bps, 1_000); // 10% = 1_000 bps
                assert_eq!(limit_bps, 100);
            }
            other => panic!("expected RiskPriceBand, got {other:?}"),
        }
    }

    #[test]
    fn test_check_limit_admission_price_band_fractional_bps_is_rejected() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );
        let acct = account(11);

        // Reference 30_000, limit 100 bps → the band edge is exactly 30_300
        // (100 bps = 300 ticks). 30_301 is 100.33 bps: truncating division
        // floored this to 100 and admitted it; cross-multiplication rejects it.
        match state.check_limit_admission(acct, 30_301, 1, Some(30_000)) {
            Err(OrderBookError::RiskPriceBand {
                deviation_bps,
                limit_bps,
                ..
            }) => {
                assert_eq!(limit_bps, 100);
                assert_eq!(deviation_bps, 100, "display still shows the floored bps");
            }
            other => panic!("fractional over-band order must be rejected, got {other:?}"),
        }

        // An order exactly at the band edge (30_300 = 100.0 bps) is admitted —
        // the strict-`>` boundary semantics are preserved.
        assert!(
            state
                .check_limit_admission(acct, 30_300, 1, Some(30_000))
                .is_ok(),
            "exact-limit order must still be admitted"
        );

        // And just inside the band (30_299) is admitted.
        assert!(
            state
                .check_limit_admission(acct, 30_299, 1, Some(30_000))
                .is_ok()
        );
    }

    #[test]
    fn test_check_limit_admission_no_reference_price_skips_band_check() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );
        // No reference available. Check skipped → Ok.
        assert!(
            state
                .check_limit_admission(account(8), 999_999_999, 1, None)
                .is_ok()
        );
    }

    #[test]
    fn test_check_limit_admission_warns_only_once_when_no_reference_available() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );

        let acct = account(9);
        assert!(state.check_limit_admission(acct, 1, 1, None).is_ok());
        assert!(
            state.warned_no_reference.load(Ordering::Relaxed),
            "first call without reference should flip the latch"
        );
        // Second call: latch already set; check still passes, no
        // additional warning emitted (we cannot assert log count here
        // without a tracing-subscriber harness, but the latch is the
        // gate on the log site).
        assert!(state.check_limit_admission(acct, 2, 2, None).is_ok());
        assert!(state.warned_no_reference.load(Ordering::Relaxed));
    }

    #[test]
    fn test_within_limits_admission_succeeds() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(10)
                .with_max_notional_per_account(1_000_000)
                .with_price_band_bps(500, ReferencePriceSource::LastTrade),
        );

        let acct = account(10);
        // Reference 100, submitted 100 → 0 bps. All checks pass.
        assert!(state.check_limit_admission(acct, 100, 5, Some(100)).is_ok());
    }

    #[test]
    fn test_disable_keeps_counters() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));

        let acct = account(11);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 10)
            .expect("admission");

        state.disable();

        // Config gone, but counters remain.
        assert!(state.config().is_none());
        assert!(state.counters.contains_key(&acct));
        assert!(state.orders.contains_key(&order_id));

        // After disable, every check is a passthrough again.
        assert!(
            state
                .check_limit_admission(acct, 100, 100, Some(100))
                .is_ok()
        );
    }

    #[test]
    fn test_on_fill_overshoot_clamps_counters_at_zero() {
        // Regression: a stray double-fill or filled_qty > remaining
        // must not wrap counters via `fetch_sub`. Both decrements
        // saturate at zero.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(10_000));

        let acct = account(12);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 5)
            .expect("admission");

        // Decrement by far more than what was admitted.
        state.on_fill(order_id, 1_000_000, 100);

        // #243: the overshoot releases only the tracked remainder (5 @ 100),
        // so both counters land exactly at zero (no clamp needed) and the
        // account entry is evicted. The overshoot itself is an accounting
        // anomaly: logged and counted, never silent.
        assert!(
            state.counters.get(&acct).is_none(),
            "overshoot fill releases the tracked remainder and evicts; a wrap would leave a non-zero count and retain the entry"
        );
        assert!(state.orders.is_empty());
        assert_eq!(state.accounting_anomalies(), 1, "overshoot is counted");
    }

    #[test]
    fn test_on_cancel_after_fully_filled_is_noop_and_does_not_wrap() {
        // Regression: cancel after the entry has already been removed
        // by an on_fill must be a no-op and not under-flow the
        // counters that the prior fill already drove to zero.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));

        let acct = account(13);
        let order_id = new_id();
        let _ = state
            .on_admission(order_id, acct, 100, 5)
            .expect("admission");
        state.on_fill(order_id, 5, 100); // entry removed, counters evicted at 0
        state.on_cancel(order_id); // no-op (entry not present)

        // The full fill drove both counters to zero and evicted the entry; the
        // trailing cancel finds no entry, so it cannot underflow the counters.
        assert!(
            state.counters.get(&acct).is_none(),
            "fill evicted the zeroed entry; the later cancel is a no-op and cannot wrap"
        );
        assert!(state.orders.is_empty());
    }

    // ───────────────────────────────────────────────────────────────
    // Modify-aware admission (#98)
    // ───────────────────────────────────────────────────────────────

    #[test]
    fn test_check_modify_admission_no_config_is_passthrough() {
        let state = RiskState::new();
        assert!(
            state
                .check_modify_admission(new_id(), account(1), 999_999, 999, Some(100))
                .is_ok()
        );
    }

    #[test]
    fn test_check_modify_admission_ignores_open_order_count() {
        // A modify of a TRACKED order must never reject on the open-order
        // count: an account sitting exactly at the limit can still modify a
        // resting order (count is net unchanged).
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let acct = account(20);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 10).expect("admission"); // account at the limit

        assert!(
            state
                .check_modify_admission(id, acct, 110, 10, Some(105))
                .is_ok(),
            "modify of a tracked order must not be gated by max_open_orders_per_account"
        );
    }

    #[test]
    fn test_check_modify_admission_projects_notional_swapping_old_for_new() {
        // Notional ceiling 1_000. Original order contributes 100*8 = 800.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000));
        let acct = account(21);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 8).expect("admission"); // resting_notional = 800

        // Modify to 100*9 = 900 projects to 800 - 800 + 900 = 900 ≤ 1_000.
        assert!(
            state
                .check_modify_admission(id, acct, 100, 9, Some(100))
                .is_ok(),
            "projected notional 900 must be within the 1_000 ceiling"
        );

        // Modify to 100*11 = 1_100 projects to 800 - 800 + 1_100 = 1_100 > 1_000.
        match state.check_modify_admission(id, acct, 100, 11, Some(100)) {
            Err(OrderBookError::RiskMaxNotional {
                account: a,
                attempted,
                limit,
                ..
            }) => {
                assert_eq!(a, acct);
                assert_eq!(attempted, 1_100);
                assert_eq!(limit, 1_000);
            }
            other => panic!("expected RiskMaxNotional, got {other:?}"),
        }
    }

    #[test]
    fn test_check_modify_admission_projection_does_not_double_count_original() {
        // Regression: the naive limit-admission check would add the new
        // contribution on top of the (still-counted) original and falsely
        // reject. The projection subtracts the original's tracked contribution.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000));
        let acct = account(22);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 10).expect("admission"); // resting_notional = 1_000 (at ceiling)

        // Re-price to the same notional: 1_000 - 1_000 + 1_000 = 1_000 ≤ 1_000.
        assert!(
            state
                .check_modify_admission(id, acct, 200, 5, Some(150))
                .is_ok(),
            "an unchanged-notional modify must not double-count the original"
        );
    }

    #[test]
    fn test_check_modify_admission_price_band_on_new_price() {
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );
        let acct = account(23);
        let id = new_id();
        let _ = state
            .on_admission(id, acct, 1_000_000, 1)
            .expect("admission");

        // New price 1_100_000 vs reference 1_000_000 → +1_000 bps, far over band.
        match state.check_modify_admission(id, acct, 1_100_000, 1, Some(1_000_000)) {
            Err(OrderBookError::RiskPriceBand {
                submitted,
                reference,
                limit_bps,
                ..
            }) => {
                assert_eq!(submitted, 1_100_000);
                assert_eq!(reference, 1_000_000);
                assert_eq!(limit_bps, 100);
            }
            other => panic!("expected RiskPriceBand, got {other:?}"),
        }

        // A new price inside the band is admitted.
        assert!(
            state
                .check_modify_admission(id, acct, 1_005_000, 1, Some(1_000_000))
                .is_ok()
        );
    }

    #[test]
    fn test_check_modify_admission_untracked_original_runs_full_admission() {
        // If the original order has no RiskEntry (admitted while no RiskConfig
        // was installed, then a config was installed before this modify), the
        // modify is a genuinely new admission from the risk layer's view — full
        // admission applies, INCLUDING the open-order count. This mirrors
        // `add_order`'s post-cancel check so the validate-first guard predicts
        // the post-cancel verdict and never destroys the original.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let acct = account(24);
        // One OTHER tracked resting order already at the limit.
        let _ = state
            .on_admission(new_id(), acct, 100, 10)
            .expect("admission");

        // The order being modified is NOT tracked → full admission → rejected
        // on the open-order count (would be a 2nd order for the account).
        let untracked = new_id();
        match state.check_modify_admission(untracked, acct, 110, 5, Some(105)) {
            Err(OrderBookError::RiskMaxOpenOrders { .. }) => {}
            other => panic!(
                "untracked modify must run full admission and reject on open count, got {other:?}"
            ),
        }
    }

    // ───────────────────────────────────────────────────────────────
    // Checked notional arithmetic (#243)
    // ───────────────────────────────────────────────────────────────

    fn notional_of(state: &RiskState, acct: Hash32) -> u128 {
        state
            .counters
            .get(&acct)
            .map(|c| c.resting_notional.load())
            .unwrap_or(0)
    }

    /// Price whose double does not fit in `u128`.
    const HALF_PLUS_ONE: u128 = u128::MAX / 2 + 1;

    #[test]
    fn test_notional_limit_holds_at_u128_extremes_issue_243() {
        // Limit at the very top of the domain. Before #243 the second check
        // computed `current.saturating_add(attempted) = u128::MAX`, which is
        // not `> u128::MAX`, so it passed, and the `fetch_add` in
        // `on_admission` wrapped the counter to 0: the limit was bypassed.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(u128::MAX));
        let acct = account(40);

        state
            .check_limit_admission(acct, HALF_PLUS_ONE, 1, None)
            .expect("first order fits");
        let _ = state
            .on_admission(new_id(), acct, HALF_PLUS_ONE, 1)
            .expect("first reservation fits");

        match state.check_limit_admission(acct, HALF_PLUS_ONE, 1, None) {
            Err(OrderBookError::RiskMaxNotional {
                account: a,
                current,
                attempted,
                limit,
            }) => {
                assert_eq!(a, acct);
                assert_eq!(current, HALF_PLUS_ONE);
                assert_eq!(attempted, HALF_PLUS_ONE);
                assert_eq!(limit, u128::MAX);
            }
            other => panic!("overflowing sum must reject, got {other:?}"),
        }

        // The post-trade reservation is checked too (the race backstop):
        // all or nothing, nothing wraps, no entry is inserted.
        let racer = new_id();
        assert!(matches!(
            state.on_admission(racer, acct, HALF_PLUS_ONE, 1),
            Err(OrderBookError::RiskMaxNotional { .. })
        ));
        assert_eq!(notional_of(&state, acct), HALF_PLUS_ONE, "no wrap");
        assert_eq!(open_count_of(&state, acct), 1, "open-count rolled back");
        assert!(!state.orders.contains_key(&racer));
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_unrepresentable_notional_rejects_without_notional_limit_issue_243() {
        // Only an open-order limit is configured, but the counters still
        // track notional: an admission they cannot represent is rejected
        // with `limit = u128::MAX` instead of wrapping the tracked value.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let acct = account(41);

        // Single-order product overflow: 2 × u128::MAX.
        match state.check_limit_admission(acct, u128::MAX, 2, None) {
            Err(OrderBookError::RiskMaxNotional {
                attempted, limit, ..
            }) => {
                assert_eq!(attempted, u128::MAX, "unrepresentable product sentinel");
                assert_eq!(limit, u128::MAX, "no configured limit");
            }
            other => panic!("expected RiskMaxNotional, got {other:?}"),
        }
        assert!(state.on_admission(new_id(), acct, u128::MAX, 2).is_err());
        assert!(state.counters.is_empty(), "failed first reservation evicts");

        // Sum overflow across two orders.
        let _ = state
            .on_admission(new_id(), acct, HALF_PLUS_ONE, 1)
            .expect("first order fits");
        assert!(matches!(
            state.check_limit_admission(acct, HALF_PLUS_ONE, 1, None),
            Err(OrderBookError::RiskMaxNotional { .. })
        ));
    }

    #[test]
    fn test_open_count_exhaustion_rejects_issue_243() {
        let mut state = RiskState::new();
        // No open-order limit: exhaustion of the counter itself is the
        // rejection.
        state.set_config(RiskConfig::new().with_max_notional_per_account(u128::MAX));
        let acct = account(42);
        let _ = state
            .on_admission(new_id(), acct, 1, 1)
            .expect("seed the account");
        if let Some(c) = state.counters.get(&acct) {
            c.open_count.store(u64::MAX, Ordering::Relaxed);
        }

        match state.check_limit_admission(acct, 1, 1, None) {
            Err(OrderBookError::RiskMaxOpenOrders { current, limit, .. }) => {
                assert_eq!(current, u64::MAX);
                assert_eq!(limit, u64::MAX);
            }
            other => panic!("expected RiskMaxOpenOrders, got {other:?}"),
        }
        let id = new_id();
        assert!(matches!(
            state.on_admission(id, acct, 1, 1),
            Err(OrderBookError::RiskMaxOpenOrders { .. })
        ));
        assert_eq!(open_count_of(&state, acct), u64::MAX, "no wrap");
        assert_eq!(notional_of(&state, acct), 1, "notional untouched");
        assert!(!state.orders.contains_key(&id));
    }

    #[test]
    fn test_price_band_rejects_at_extreme_prices_issue_243() {
        // reference ≈ u128::MAX / 2, band 100 bps. Both `diff * 10_000` and
        // `bps * reference` overflow. Before #243 both saturated to
        // `u128::MAX`, compared equal, and a 10_000 bps deviation passed.
        let mut state = RiskState::new();
        state.set_config(
            RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::LastTrade),
        );
        let acct = account(43);
        let reference = u128::MAX / 2;

        match state.check_limit_admission(acct, u128::MAX, 1, Some(reference)) {
            Err(OrderBookError::RiskPriceBand {
                deviation_bps,
                limit_bps,
                ..
            }) => {
                assert_eq!(limit_bps, 100);
                assert_eq!(deviation_bps, 10_000, "100 % deviation, floored");
            }
            other => panic!("extreme deviation must reject, got {other:?}"),
        }

        // Only `diff * 10_000` overflows: reject.
        assert!(matches!(
            state.check_limit_admission(acct, u128::MAX, 1, Some(1 << 64)),
            Err(OrderBookError::RiskPriceBand {
                deviation_bps: u32::MAX,
                ..
            })
        ));

        // Inside the band at the same extreme scale: 50 bps < 100 bps.
        assert!(
            state
                .check_limit_admission(acct, reference + reference / 200, 1, Some(reference))
                .is_ok(),
            "an in-band order at extreme prices must still pass"
        );
        // Exactly at the band edge (strict `>`): admitted; one tick past it
        // is rejected.
        let edge = reference - reference % 10_000; // divisible by 10_000
        assert!(
            state
                .check_limit_admission(acct, edge + edge / 100, 1, Some(edge))
                .is_ok()
        );
        assert!(
            state
                .check_limit_admission(acct, edge + edge / 100 + 1, 1, Some(edge))
                .is_err(),
            "one tick past the edge must reject"
        );
    }

    #[test]
    fn test_band_breach_wide_matches_exact_comparison_issue_243() {
        // Cross-check the overflow-free comparison against the exact
        // product on values where the product fits.
        let values: [u128; 7] = [
            1,
            9_999,
            10_000,
            10_001,
            123_456_789,
            1 << 70,
            u128::from(u64::MAX),
        ];
        for &reference in &values {
            for &bps in &[0u32, 1, 100, 9_999, 10_000, u32::MAX] {
                for &diff in &values {
                    let exact = diff * 10_000 > u128::from(bps) * reference;
                    assert_eq!(
                        band_breach_wide(diff, reference, bps),
                        exact,
                        "diff={diff} reference={reference} bps={bps}"
                    );
                }
            }
        }
        // Huge band: threshold beyond u128 → never a breach.
        assert!(!band_breach_wide(u128::MAX, u128::MAX / 2, u32::MAX));
    }

    #[test]
    fn test_modify_admission_rejects_unrepresentable_projection_issue_243() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let acct = account(44);
        let id = new_id();
        let _ = state
            .on_admission(id, acct, HALF_PLUS_ONE, 1)
            .expect("original");
        let _ = state
            .on_admission(new_id(), acct, HALF_PLUS_ONE - 1, 1)
            .expect("second order, sum = u128::MAX");
        // Doubling the first order's quantity overflows the projection
        // even though no notional limit is configured: reject exactly as
        // the post-cancel admission would.
        assert!(matches!(
            state.check_modify_admission(id, acct, HALF_PLUS_ONE, 2, None),
            Err(OrderBookError::RiskMaxNotional {
                limit: u128::MAX,
                ..
            })
        ));
        // Same notional is fine.
        assert!(
            state
                .check_modify_admission(id, acct, HALF_PLUS_ONE, 1, None)
                .is_ok()
        );
    }

    #[test]
    fn test_release_underflow_is_counted_and_state_stays_consistent_issue_243() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));
        let acct = account(45);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 10).expect("admission");

        // Simulate a prior double release: the counter holds less than the
        // entry's booked contribution (1_000).
        if let Some(c) = state.counters.get(&acct) {
            c.resting_notional.store(400);
        }
        state.on_cancel(id);

        // The release clamped at zero (never wrapped), was counted, and the
        // account is fully reclaimed: counters and entries agree (empty).
        assert_eq!(state.accounting_anomalies(), 1);
        assert!(
            state.counters.get(&acct).is_none(),
            "zeroed account evicted"
        );
        assert!(state.orders.is_empty());

        // The account is not locked out afterwards.
        assert!(state.check_limit_admission(acct, 100, 10, None).is_ok());
    }

    #[test]
    fn test_open_count_release_underflow_is_counted_issue_243() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let acct = account(46);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 1).expect("admission");
        if let Some(c) = state.counters.get(&acct) {
            c.open_count.store(0, Ordering::Relaxed);
        }
        state.on_fill(id, 1, 100);
        assert_eq!(state.accounting_anomalies(), 1);
        assert!(state.counters.get(&acct).is_none());
        assert!(state.orders.is_empty());
    }

    #[test]
    fn test_quantity_increase_overflow_is_rejected_before_the_level_changes_issue_243() {
        // Review: the increase is reserved BEFORE the level mutates, so an
        // overflow is a typed rejection with nothing changed, not a
        // post-commit revert that leaves book and risk diverged.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let acct = account(47);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 1).expect("admission");
        // Another admission raced the counter close to the ceiling.
        if let Some(c) = state.counters.get(&acct) {
            c.resting_notional.store(u128::MAX - 50);
        }
        assert!(matches!(
            state.reserve_quantity_update(id, 2), // +100 would overflow
            Err(OrderBookError::RiskMaxNotional { .. })
        ));
        assert_eq!(notional_of(&state, acct), u128::MAX - 50, "no wrap");
        assert_eq!(state.orders.get(&id).map(|e| e.remaining_qty), Some(1));
        assert_eq!(
            state.accounting_anomalies(),
            0,
            "a rejection, not an anomaly"
        );
    }

    #[test]
    fn test_quantity_reservation_commit_and_rollback_settle_exactly_issue_243() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));
        let acct = account(52);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 10).expect("admission");

        // Increase 10 → 15, level applies it: +500 exactly.
        let r = state.reserve_quantity_update(id, 15).expect("reserve");
        assert_eq!(notional_of(&state, acct), 1_500, "pre-booked");
        state.commit_quantity_update(r, 15);
        assert_eq!(notional_of(&state, acct), 1_500);
        assert_eq!(state.orders.get(&id).map(|e| e.remaining_qty), Some(15));

        // Increase 15 → 20, level refuses: the reservation is given back.
        let r = state.reserve_quantity_update(id, 20).expect("reserve");
        assert_eq!(notional_of(&state, acct), 2_000);
        state.rollback_quantity_update(r);
        assert_eq!(notional_of(&state, acct), 1_500);
        assert_eq!(state.orders.get(&id).map(|e| e.remaining_qty), Some(15));

        // Decrease 15 → 4 reserves nothing and releases on commit.
        let r = state.reserve_quantity_update(id, 4).expect("reserve");
        assert_eq!(notional_of(&state, acct), 1_500);
        state.commit_quantity_update(r, 4);
        assert_eq!(notional_of(&state, acct), 400);

        // A fill races between reserve and commit (4 → 1 via 3 filled);
        // the level then stores the new total 8: settle to 8 × 100.
        let r = state.reserve_quantity_update(id, 8).expect("reserve");
        state.on_fill(id, 3, 100);
        state.commit_quantity_update(r, 8);
        assert_eq!(notional_of(&state, acct), 800);
        assert_eq!(state.orders.get(&id).map(|e| e.remaining_qty), Some(8));

        // The order leaves between reserve and commit: reservation returned.
        let r = state.reserve_quantity_update(id, 12).expect("reserve");
        state.on_cancel(id);
        state.commit_quantity_update(r, 12);
        assert!(state.counters.get(&acct).is_none(), "fully released");
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_admission_rejects_tracked_id_without_touching_counters_issue_243() {
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let acct = account(53);
        let id = new_id();
        let winner = state.on_admission(id, acct, 100, 10).expect("winner");
        assert!(matches!(
            state.on_admission(id, account(54), 200, 5),
            Err(OrderBookError::DuplicateOrderId { order_id }) if order_id == id
        ));
        // Winner's entry and counters untouched; the loser left nothing.
        assert_eq!(state.orders.get(&id).map(|e| e.account), Some(acct));
        assert_eq!(open_count_of(&state, acct), 1);
        assert_eq!(notional_of(&state, acct), 1_000);
        assert!(state.counters.get(&account(54)).is_none());

        // A stale token for the same id (different generation) cannot
        // release the winner's entry.
        let stale = RiskReservation {
            order_id: id,
            generation: winner.generation + 1,
        };
        state.release_reservation(stale);
        assert!(state.orders.contains_key(&id));
        assert_eq!(open_count_of(&state, acct), 1);

        // The winner's own token releases exactly its contribution.
        state.release_reservation(winner);
        assert!(state.orders.is_empty());
        assert!(state.counters.is_empty());
    }

    #[test]
    fn test_concurrent_same_id_admission_keeps_winner_tracked_issue_243() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const THREADS: usize = 8;
        const ROUNDS: u64 = 200;

        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(u64::MAX));
        let state = Arc::new(state);
        let acct = account(55);

        for round in 0..ROUNDS {
            let id = Id::from_u64(round);
            let barrier = Arc::new(Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let state = Arc::clone(&state);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        match state.on_admission(id, acct, 100, 1) {
                            Ok(token) => Some(token),
                            Err(OrderBookError::DuplicateOrderId { .. }) => None,
                            Err(other) => panic!("unexpected error {other:?}"),
                        }
                    })
                })
                .collect();
            let winners: Vec<RiskReservation> = handles
                .into_iter()
                .filter_map(|h| h.join().expect("admission thread"))
                .collect();
            assert_eq!(winners.len(), 1, "round {round}: exactly one winner");
            assert!(
                state.orders.contains_key(&id),
                "round {round}: winner tracked"
            );
        }
        // Every round left exactly one tracked order and one count each.
        assert_eq!(open_count_of(&state, acct), ROUNDS);
        assert_eq!(notional_of(&state, acct), u128::from(ROUNDS) * 100);
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_on_maker_removed_releases_discarded_remainder_issue_243() {
        // A non-auto reserve maker tracked at visible + hidden = 15 is
        // removed after its visible 5 fill: the discarded hidden 10 must be
        // released, not stay booked.
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let acct = account(56);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 15).expect("admission");
        state.on_fill(id, 5, 100);
        assert_eq!(notional_of(&state, acct), 1_000, "hidden still booked");
        state.on_maker_removed(id);
        assert!(state.counters.is_empty(), "discarded remainder released");
        assert!(state.orders.is_empty());
        assert_eq!(state.accounting_anomalies(), 0);
        // A normally exhausted maker: no-op after on_fill.
        let other = new_id();
        let _ = state.on_admission(other, acct, 100, 2).expect("admission");
        state.on_fill(other, 2, 100);
        state.on_maker_removed(other);
        assert!(state.counters.is_empty());
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_on_fill_price_mismatch_does_not_panic_and_uses_admission_price_issue_243() {
        // Replaces the former `debug_assert_eq!`: a mismatch is a warning,
        // and the release still uses the admission price (self-balancing).
        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_notional_per_account(1_000_000));
        let acct = account(48);
        let id = new_id();
        let _ = state.on_admission(id, acct, 100, 10).expect("admission");
        state.on_fill(id, 4, 250);
        assert_eq!(notional_of(&state, acct), 600);
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_concurrent_cas_is_linearizable_sum_preserved_issue_243() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const THREADS: u64 = 8;
        const ORDERS_PER_THREAD: u64 = 64;
        const FILLS_PER_ORDER: u64 = 4;
        const QTY: u64 = 8;
        const PRICE: u128 = 1_000_003;

        let mut state = RiskState::new();
        state.set_config(RiskConfig::new().with_max_open_orders_per_account(u64::MAX));
        let state = Arc::new(state);
        let acct = account(49);
        let barrier = Arc::new(Barrier::new(THREADS as usize));

        // Phase 1: concurrent admissions on the SAME account.
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..ORDERS_PER_THREAD {
                        let id = Id::from_u64(t * ORDERS_PER_THREAD + i);
                        let _ = state.on_admission(id, acct, PRICE, QTY).expect("admission");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("admission thread");
        }
        let total_orders = THREADS * ORDERS_PER_THREAD;
        assert_eq!(open_count_of(&state, acct), total_orders);
        assert_eq!(
            notional_of(&state, acct),
            u128::from(total_orders * QTY) * PRICE
        );

        // Phase 2: concurrent partial fills (read-guard path, contended CAS
        // on the same account's counters). Each thread fills only its own
        // orders, one unit per fill.
        let barrier = Arc::new(Barrier::new(THREADS as usize));
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..FILLS_PER_ORDER {
                        for i in 0..ORDERS_PER_THREAD {
                            state.on_fill(Id::from_u64(t * ORDERS_PER_THREAD + i), 1, PRICE);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("fill thread");
        }

        // Every increment and decrement is accounted exactly once.
        let expected_remaining = u128::from(total_orders * (QTY - FILLS_PER_ORDER)) * PRICE;
        assert_eq!(notional_of(&state, acct), expected_remaining);
        assert_eq!(open_count_of(&state, acct), total_orders);
        assert_eq!(state.accounting_anomalies(), 0);
    }

    #[test]
    fn test_checked_cas_helpers_under_contention_issue_243() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const THREADS: u64 = 8;
        const OPS: u64 = 2_000;

        let cell = Arc::new(AtomicCell::new(0u128));
        let counter = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(THREADS as usize));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let cell = Arc::clone(&cell);
                let counter = Arc::clone(&counter);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..OPS {
                        checked_add_u128(&cell, 3).expect("no overflow");
                        release_u128(&cell, 1).expect("no underflow");
                        checked_add_u64(&counter, 2).expect("no overflow");
                        release_u64(&counter, 1).expect("no underflow");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("helper thread");
        }
        assert_eq!(cell.load(), u128::from(THREADS * OPS * 2));
        assert_eq!(counter.load(Ordering::Relaxed), THREADS * OPS);

        // Overflow and underflow edges.
        let top = AtomicCell::new(u128::MAX);
        assert_eq!(checked_add_u128(&top, 1), Err(u128::MAX));
        assert_eq!(top.load(), u128::MAX, "nothing stored on overflow");
        let low = AtomicU64::new(1);
        assert_eq!(release_u64(&low, 2), Err(1));
        assert_eq!(low.load(Ordering::Relaxed), 0, "clamped at zero, reported");
    }

    #[test]
    fn test_risk_rebuild_accumulates_checked_and_installs_issue_243() {
        let acct = account(50);
        let other = account(51);

        let mut rebuild = RiskRebuild::default();
        rebuild
            .accumulate(Id::from_u64(1), acct, HALF_PLUS_ONE, 1)
            .expect("first fits");
        rebuild
            .accumulate(Id::from_u64(2), other, 100, 5)
            .expect("other account");
        match rebuild.accumulate(Id::from_u64(3), acct, HALF_PLUS_ONE, 1) {
            Err(OrderBookError::RiskMaxNotional {
                account: a,
                current,
                limit,
                ..
            }) => {
                assert_eq!(a, acct);
                assert_eq!(current, HALF_PLUS_ONE);
                assert_eq!(limit, u128::MAX);
            }
            other => panic!("aggregate overflow must be a typed error, got {other:?}"),
        }

        let mut state = RiskState::new();
        state.set_config(RiskConfig::new());
        state.install_rebuild(&rebuild);
        assert_eq!(
            open_count_of(&state, acct),
            1,
            "failed accumulate left no trace"
        );
        assert_eq!(notional_of(&state, acct), HALF_PLUS_ONE);
        assert_eq!(notional_of(&state, other), 500);
        assert_eq!(state.orders.len(), 2);

        // Installed state releases cleanly.
        state.on_cancel(Id::from_u64(1));
        state.on_cancel(Id::from_u64(2));
        assert!(state.counters.is_empty());
        assert_eq!(state.accounting_anomalies(), 0);

        // No config: install is a no-op, like `on_admission`.
        let empty = RiskState::new();
        empty.install_rebuild(&rebuild);
        assert!(empty.counters.is_empty() && empty.orders.is_empty());
    }
}
