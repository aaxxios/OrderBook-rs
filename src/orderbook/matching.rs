//! Contains the core matching engine logic for the order book.
//!
//! The matching engine supports Self-Trade Prevention (STP) when configured
//! via [`crate::STPMode`]. When STP is disabled (`STPMode::None`, the default),
//! the matching hot path is unchanged with zero overhead.

// panic-policy-ratchet: see #242, removed by the fix issue
#![allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]

use crate::orderbook::book_change_event::PriceLevelChangedEvent;
use crate::orderbook::order_state::{CancelReason, OrderStatus};
use crate::orderbook::pool::MatchingPool;
use crate::orderbook::reject_reason::RejectReason;
use crate::orderbook::stp::{STPAction, check_stp_at_level};
use crate::{OrderBook, OrderBookError};
use either::Either;
use pricelevel::{
    CapacityResource, Hash32, Id, MatchResult, OrderType, PriceLevelError, Quantity, Side,
    TakerKind, TimeInForce,
};
use std::sync::atomic::Ordering;

// Reusable sweep scratch buffers (#107, #230). Module-level so the sweep and
// its early-exit helpers share one pool per thread.
thread_local! {
    static MATCHING_POOL: MatchingPool = MatchingPool::new();
}

/// Resources a fill-or-kill taker's sweep will draw on, measured by the
/// feasibility walk [`OrderBook::fok_fillable_quantity`] before anything is
/// mutated (#240).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FokFeasibility {
    /// Quantity the sweep would fill, in quantity units.
    pub(crate) fillable: u64,
    /// Upper bound on the trades the sweep can emit, and therefore on the
    /// trade ids it draws from the book's `UuidGenerator`: at a level with
    /// no hidden depth every maker trades at most once, so the level
    /// contributes `min(makers, quantity taken there)`; at a level with
    /// hidden depth a replenishing maker can trade again, so only the
    /// quantity taken there (one unit per trade at least) bounds it.
    pub(crate) max_trades: u64,
    /// Maker steps the sweep takes without replenishment:
    /// `Σ min(makers, quantity taken)` over the levels it reaches. Sizes the
    /// up-front reservation of the trade and filled-id buffers. Trades from
    /// iceberg / reserve replenishments beyond it grow those buffers during
    /// the sweep (see the residual documented on `OrderBook::add_order`).
    pub(crate) maker_steps: u64,
}

/// Overflow of a fill-or-kill feasibility accumulator. Every accumulator is
/// bounded by the taker's `u64` quantity, so this is an invariant breach,
/// reported as a typed error instead of clamping.
#[cold]
#[inline(never)]
fn fok_counter_overflow() -> OrderBookError {
    OrderBookError::InvalidOperation {
        message: "fill-or-kill feasibility accumulator overflowed u64".to_string(),
    }
}

/// Return a sweep's scratch buffers to the thread-local pool. `stp_orders`
/// only came from the pool when STP was active; otherwise it is an empty,
/// never-filled `Vec` that is simply dropped.
#[inline]
fn release_sweep_buffers(
    filled_orders: Vec<(Id, u64)>,
    empty_price_levels: Vec<u128>,
    strandable_makers: Option<Vec<(Id, u64)>>,
    stp_orders: Vec<std::sync::Arc<OrderType<()>>>,
    stp_active: bool,
) {
    MATCHING_POOL.with(|pool| {
        pool.return_filled_orders_vec(filled_orders);
        if let Some(strandable) = strandable_makers {
            pool.return_filled_orders_vec(strandable);
        }
        pool.return_price_vec(empty_price_levels);
        if stp_active {
            pool.return_order_snapshot_vec(stp_orders);
        }
    });
}
/// Matchable depth of a single resting order: its visible quantity plus any
/// hidden quantity the sweep can actually draw. An iceberg always replenishes
/// its hidden tranche; a reserve only when `auto_replenish` is set — a
/// non-auto-replenish reserve drops its hidden unfilled, so that hidden is NOT
/// reachable depth.
///
/// As of #136 the non-STP and STP-NoConflict FOK feasibility paths delegate to
/// `PriceLevel::matchable_quantity` (the authoritative upstream dry run). This
/// helper remains only for the STP `CancelMaker` case, which must sum the
/// matchable depth of the *non-self* makers — a per-user filter the upstream
/// primitive cannot express. It is therefore the one FOK-feasibility path that
/// no longer rides on pricelevel's authoritative dry run, so its per-order total
/// MUST stay equal to `OrderType::match_against`'s for every resting kind: if a
/// new `OrderType` variant is added whose sweep total differs from
/// `visible + drawable_hidden`, update this helper or the CancelMaker FOK path
/// will silently mis-predict while the delegated paths stay correct.
#[inline]
fn order_matchable_qty(order: &OrderType<()>) -> u64 {
    let visible = order.visible_quantity().as_u64();
    let drawable_hidden = match order {
        OrderType::IcebergOrder {
            hidden_quantity, ..
        } => hidden_quantity.as_u64(),
        OrderType::ReserveOrder {
            hidden_quantity,
            auto_replenish,
            ..
        } if *auto_replenish => hidden_quantity.as_u64(),
        _ => 0,
    };
    visible.saturating_add(drawable_hidden)
}

/// Hidden quantity `filled_id` would strand, looked up in the sweep's
/// captured strandable makers (#230).
///
/// `strandable` is sorted by [`Id::as_bytes`] — `Id` does not implement
/// `Ord` upstream, so that stable 16-byte projection is the sort key. Two
/// *different* ids can share one projection (`Sequential` zero-pads into the
/// same 16 bytes a `Uuid` could occupy), so the equal-key run is walked and
/// the ids compared for real equality rather than trusting the key alone.
/// That run is length 1 in every realistic book.
#[must_use]
#[inline]
fn find_strandable(strandable: &[(Id, u64)], filled_id: Id) -> Option<u64> {
    let key = filled_id.as_bytes();
    let start = strandable.partition_point(|(id, _)| id.as_bytes() < key);
    strandable
        .get(start..)?
        .iter()
        .take_while(|(id, _)| id.as_bytes() == key)
        .find(|(id, _)| *id == filled_id)
        .map(|(_, hidden)| *hidden)
}

/// Outcome of an internal match: the [`MatchResult`] plus whether self-trade
/// prevention cancelled the taker. The flag lets the resting caller (`add_order`)
/// know it must NOT rest the residual — a partially-filled taker that then
/// self-crossed under `CancelTaker` / `CancelBoth` is cancelled, not rested (#97).
#[derive(Debug)]
pub(crate) struct MatchOutcome {
    /// The trades, filled-order ids, and remaining quantity of the match.
    pub(crate) result: MatchResult,
    /// `true` when STP cancelled the taker, so any residual must not rest.
    pub(crate) taker_stp_cancelled: bool,
    /// `true` when a per-level post-only guard refused to trade (#209):
    /// the taker was submitted as [`TakerKind::PostOnly`] and reached a
    /// crossable level, so the book must reject it (`PriceCrossing`) —
    /// structurally zero trades were emitted.
    pub(crate) taker_post_only_rejected: bool,
    /// `Some(OrderBookError::MatchAborted)` when the sweep stopped at a
    /// failed price level (#240). `result` then holds exactly the committed
    /// prefix, which the caller must publish like a partial fill before it
    /// returns this error; the remainder must never rest. The taker's
    /// terminal `Cancelled { MatchAborted }` state and the reject metric are
    /// already recorded by the sweep.
    pub(crate) aborted: Option<OrderBookError>,
}

impl MatchOutcome {
    /// Wrap a `MatchResult` with no STP cancellation (common case / empty book).
    #[inline]
    fn resting(result: MatchResult) -> Self {
        Self {
            result,
            taker_stp_cancelled: false,
            taker_post_only_rejected: false,
            aborted: None,
        }
    }

    /// The bare `MatchResult`, or the abort error when the sweep stopped at
    /// a failed level. Used by the raw `match_*` entry points, which hand
    /// the result to the caller instead of publishing it.
    ///
    /// # Errors
    ///
    /// [`OrderBookError::MatchAborted`] when [`Self::aborted`] is set.
    #[inline]
    pub(crate) fn into_result(self) -> Result<MatchResult, OrderBookError> {
        match self.aborted {
            Some(err) => Err(err),
            None => Ok(self.result),
        }
    }
}

/// Selects how the matching loop measures its budget.
///
/// `BaseQty` is the legacy base-asset quantity path (existing market and
/// limit orders). `QuoteAmount` is the quote-notional path used by
/// `match_market_order_by_amount` (Binance `quoteOrderQty` semantics).
/// Always `None` limit price for the notional path — quote-notional is
/// market-only.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MatchMode {
    /// Base-quantity match. `limit_price = None` for market orders.
    BaseQty {
        /// Total base-asset quantity to match.
        quantity: u64,
        /// Optional price ceiling (Buy) / floor (Sell). `None` for
        /// market orders.
        limit_price: Option<u128>,
    },
    /// Quote-notional match (market-only).
    QuoteAmount {
        /// Total quote-asset value to consume from the book.
        amount: u128,
    },
}

impl MatchMode {
    /// Returns the limit-price guard used inside the level walk. `None`
    /// for any market path (base-qty market or quote-notional).
    #[inline]
    #[must_use]
    fn limit_price(&self) -> Option<u128> {
        match self {
            Self::BaseQty { limit_price, .. } => *limit_price,
            Self::QuoteAmount { .. } => None,
        }
    }

    /// Returns the initial `MatchResult` quantity slot. For quote-notional
    /// the actual base-qty filled is unknown upfront, so `u64::MAX` is
    /// used as a working upper bound during the loop (see
    /// `MatchResult::add_trade` invariants). The notional path is
    /// normalized at the end of `match_order_inner` so that the returned
    /// `MatchResult.remaining_quantity()` is `0` rather than the sentinel.
    #[inline]
    #[must_use]
    fn initial_match_quantity(&self) -> u64 {
        match self {
            Self::BaseQty { quantity, .. } => *quantity,
            Self::QuoteAmount { .. } => u64::MAX,
        }
    }
}

/// Tracks the matching loop's remaining budget against either base
/// quantity or quote notional. Designed to keep the base-qty hot path
/// allocation- and branch-light: the `BaseQty` arm of every helper is
/// a single arithmetic op the optimizer can fold.
#[derive(Debug, Clone)]
enum StopCondition {
    /// Base-quantity remaining.
    BaseQty {
        /// Base-asset quantity left to fill.
        remaining: u64,
    },
    /// Quote-notional remaining.
    QuoteAmount {
        /// Quote-asset value left to consume.
        remaining: u128,
    },
}

impl StopCondition {
    /// Build a fresh stop condition from the matching mode.
    #[inline]
    fn from_mode(mode: &MatchMode) -> Self {
        match mode {
            MatchMode::BaseQty { quantity, .. } => Self::BaseQty {
                remaining: *quantity,
            },
            MatchMode::QuoteAmount { amount } => Self::QuoteAmount { remaining: *amount },
        }
    }

    /// Per-level base-qty cap respecting `lot_size`. A return of `0`
    /// signals the caller to stop walking (dust below one full lot at
    /// the current level price).
    ///
    /// `lot <= 1` ⇒ no rounding (single arithmetic path); preserves the
    /// existing base-qty performance profile when lot enforcement is not
    /// configured.
    #[inline]
    #[must_use]
    fn level_qty_cap(&self, level_price: u128, lot: u64) -> u64 {
        let raw = match self {
            Self::BaseQty { remaining } => *remaining,
            Self::QuoteAmount { remaining } => {
                if level_price == 0 || *remaining < level_price {
                    return 0;
                }
                (*remaining / level_price).min(u128::from(u64::MAX)) as u64
            }
        };
        if lot <= 1 { raw } else { raw - (raw % lot) }
    }

    /// Decrement the remaining budget by what was actually executed at
    /// the given price.
    #[inline]
    fn consume(&mut self, executed_qty: u64, level_price: u128) {
        match self {
            Self::BaseQty { remaining } => {
                *remaining = remaining.saturating_sub(executed_qty);
            }
            Self::QuoteAmount { remaining } => {
                let spent = level_price.saturating_mul(u128::from(executed_qty));
                *remaining = remaining.saturating_sub(spent);
            }
        }
    }

    /// Returns `true` when no further fills are needed (budget exhausted).
    #[inline]
    #[must_use]
    fn is_done(&self) -> bool {
        match self {
            Self::BaseQty { remaining } => *remaining == 0,
            Self::QuoteAmount { remaining } => *remaining == 0,
        }
    }

    /// Whether the remaining budget is quote-notional dust at `level_price`:
    /// nonzero, but unable to fund one more lot at that price.
    ///
    /// Only the notional arm is walked past on the STP-cancelling paths. A
    /// base-quantity residual, whatever its size, is the taker's own
    /// quantity still unfilled at a level holding its own maker; walked
    /// past, it would rest crossed against that maker (a maker admitted
    /// before a lot-size change keeps resting with a misaligned tranche —
    /// see `set_lot_size` — so a sub-lot residual is reachable), so the
    /// base arm keeps the STP verdict. A
    /// notional taker never rests, and its dust at one price can still
    /// fund a whole lot at a cheaper level, so it walks on instead.
    #[inline]
    #[must_use]
    fn is_dust_at(&self, level_price: u128, lot: u64) -> bool {
        matches!(self, Self::QuoteAmount { .. }) && self.level_qty_cap(level_price, lot) == 0
    }

    /// Whether a zero [`Self::level_qty_cap`] at the level the walk is
    /// standing on ends the whole walk, or only this level.
    ///
    /// The taker's side fixes the walk direction and therefore the price
    /// monotonicity of the levels still ahead: a buy taker walks asks
    /// ascending, so every later level is **dearer**; a sell taker walks
    /// bids descending, so every later level is **cheaper**.
    ///
    /// - `BaseQty`: the cap is the lot-rounded residual and does not depend
    ///   on the level price at all. Zero here is zero at every remaining
    ///   level whichever way the walk runs, so it is terminal on both sides.
    /// - `QuoteAmount` on a buy: the cap is `remaining / level_price`, which
    ///   is non-increasing as the walk moves to dearer asks. A budget that
    ///   cannot fund one lot here funds even less further on, so zero is
    ///   terminal.
    /// - `QuoteAmount` on a sell: the walk moves to cheaper bids, so
    ///   `remaining / level_price` can rise again. A budget that cannot fund
    ///   one lot at this bid may fund a whole one at a lower bid, so the
    ///   level is skipped and the walk continues — unless the budget cannot
    ///   fund a lot at **any** price, which is the exact test below.
    ///
    /// The sell arm's exact terminal condition is `remaining < lot`. The cap
    /// is `(remaining / level_price)` rounded down to a multiple of `lot`,
    /// and the cheapest a level can be is a price of `1`, at which the cap is
    /// `remaining` itself; no level can yield more. So `remaining < lot`
    /// means every level still ahead caps at zero and the walk is over,
    /// while `remaining >= lot` means some reachable price would fund a lot
    /// and the walk must go on to find out whether the book offers one.
    /// With no lot size configured (`lot <= 1`) that reduces to
    /// `remaining == 0`, which the loop's own `is_done` check already
    /// handles, so a notional sell then stops only on an exhausted budget or
    /// an exhausted side.
    ///
    /// Traversal cost: a notional sell whose budget stays at or above one
    /// lot visits every remaining level on its side, which is strictly more
    /// than the old unconditional break did. The bound is the number of
    /// levels resting on that side; each skipped level costs one `u128`
    /// divide and no level mutation.
    ///
    /// Only ever consulted when the cap is already zero, so the base-qty
    /// hot path (`lot <= 1`, cap equal to the residual) never evaluates it.
    #[inline]
    #[must_use]
    fn zero_cap_is_terminal(&self, taker_side: Side, lot: u64) -> bool {
        match self {
            Self::BaseQty { .. } => true,
            Self::QuoteAmount { remaining } => {
                matches!(taker_side, Side::Buy) || *remaining < u128::from(lot.max(1))
            }
        }
    }
}

impl<T> OrderBook<T>
where
    T: Clone + Send + Sync + Default + 'static,
{
    /// Highly optimized internal matching function.
    ///
    /// This is the backward-compatible entry point that delegates to
    /// [`Self::match_order_with_user`] with `Hash32::zero()` (bypasses STP).
    ///
    /// # Performance Optimization
    /// Uses SkipMap which maintains prices in sorted order automatically.
    /// This eliminates O(N log N) sorting overhead, reducing time complexity
    /// from O(N log N) to O(M log N), where:
    /// - N = total number of price levels
    /// - M = number of price levels actually matched (typically << N)
    ///
    /// In the happy case (single price level fill), complexity is O(log N).
    ///
    /// # Concurrency
    ///
    /// An anonymous sweep takes the shared side of the submit gate (unless
    /// a strandable maker rests), so two concurrent calls can match at the
    /// same price level at once. Their trades and the level's queue stay
    /// exact; the level's execution statistics seen by a concurrent snapshot
    /// are advisory until both return (see [`OrderBook`]'s "Level statistics
    /// are advisory under concurrent takers").
    ///
    /// # Errors
    ///
    /// Same as [`Self::match_order_with_user`]. Like every raw `match_*`
    /// entry point, an aborted partial fill (#240) returns
    /// [`OrderBookError::MatchAborted`] carrying only a summary
    /// (`executed_quantity`, `trade_count`): the raw family publishes no
    /// trades, so the committed prefix is lost to the caller. Install a
    /// trade listener and use a publishing entry point, or the submit
    /// `*_with_committed` APIs, to see it.
    pub fn match_order(
        &self,
        order_id: Id,
        side: Side,
        quantity: u64,
        limit_price: Option<u128>,
    ) -> Result<MatchResult, OrderBookError> {
        // #209: shared submit gate; calls the ungated outcome variant so
        // the non-reentrant gate is acquired exactly once.
        //
        // #230: through the coherent helper, so this sweep upgrades to the
        // exclusive side in a book that holds strandable makers. An
        // anonymous taker never runs the STP scan, so the strandable rule is
        // the only one that can apply here — but it must apply: on the
        // shared side a concurrent cancel could remove a maker this sweep
        // has already captured and free its id for an unrelated order, and
        // the drain would then report a discard that never happened.
        let _gate = self.acquire_coherent_submit_gate(false);
        self.match_order_with_user_outcome(
            order_id,
            side,
            quantity,
            limit_price,
            Hash32::zero(),
            TakerKind::Standard,
            0,
        )
        .and_then(MatchOutcome::into_result)
    }

    /// Internal matching function with Self-Trade Prevention support.
    ///
    /// When `taker_user_id` is `Hash32::zero()` or `stp_mode` is `None`,
    /// the STP check is skipped entirely (zero overhead fast path).
    ///
    /// # Arguments
    /// * `order_id` — The taker (incoming) order's unique identifier.
    /// * `side` — The side of the incoming order (`Buy` or `Sell`).
    /// * `quantity` — The quantity to match.
    /// * `limit_price` — Optional price limit (`None` for market orders).
    /// * `taker_user_id` — The user ID of the incoming order for STP checks.
    ///
    /// # Errors
    /// Returns [`OrderBookError::InsufficientLiquidity`] for market orders
    /// when no liquidity is available, or [`OrderBookError::SelfTradePrevented`]
    /// when STP in `CancelTaker` or `CancelBoth` mode cancels the entire taker.
    /// Returns [`OrderBookError::MatchAborted`] when the sweep stopped at a
    /// price level that reported a failure (#240): the trades committed
    /// before it are real and stay executed (makers, risk, order state and
    /// price-level listener reflect them), but this raw entry point does not
    /// publish trades, so the error's `executed_quantity` / `trade_count`
    /// are all it reports. Use a publishing entry point (`add_order`,
    /// `submit_*`, `match_*_order`) to receive the committed trades.
    pub fn match_order_with_user(
        &self,
        order_id: Id,
        side: Side,
        quantity: u64,
        limit_price: Option<u128>,
        taker_user_id: Hash32,
    ) -> Result<MatchResult, OrderBookError> {
        // #209: submit gate (see `match_order`). #225: exclusive when STP
        // is engaged for this taker, so the per-level scan and the fill it
        // authorises see the same queue state. Match-only entry points
        // always sweep as `TakerKind::Standard`, hence `is_post_only =
        // false`.
        let _gate = self.acquire_coherent_submit_gate(self.submit_needs_exclusive_gate(
            false,
            taker_user_id,
            false,
            false,
        ));
        self.match_order_with_user_outcome(
            order_id,
            side,
            quantity,
            limit_price,
            taker_user_id,
            TakerKind::Standard,
            0,
        )
        .and_then(MatchOutcome::into_result)
    }

    /// Like [`Self::match_order_with_user`] but returns the full [`MatchOutcome`],
    /// including the STP-cancel signal the resting caller in `add_order` needs to
    /// avoid resting a self-cross residual (#97) and the abort (#240).
    ///
    /// `reserve_steps` is the fill-or-kill preflight reservation
    /// ([`FokFeasibility::maker_steps`]); `0` for every other taker.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn match_order_with_user_outcome(
        &self,
        order_id: Id,
        side: Side,
        quantity: u64,
        limit_price: Option<u128>,
        taker_user_id: Hash32,
        taker_kind: TakerKind,
        reserve_steps: u64,
    ) -> Result<MatchOutcome, OrderBookError> {
        self.match_order_inner(
            order_id,
            side,
            MatchMode::BaseQty {
                quantity,
                limit_price,
            },
            taker_user_id,
            taker_kind,
            reserve_steps,
        )
    }

    /// Internal entry point for the quote-notional matching path.
    ///
    /// Public callers reach this through
    /// [`OrderBook::match_market_order_by_amount_with_user`]; the function
    /// here is the matching-loop seam that drives the unified inner loop
    /// with `MatchMode::QuoteAmount`. Always market-only — there is no
    /// `limit_price` analogue for notional orders.
    ///
    /// # Errors
    /// Returns [`OrderBookError::InsufficientLiquidityNotional`] when no
    /// liquidity could be consumed (empty book or budget below one full
    /// lot at every reachable level), or
    /// [`OrderBookError::SelfTradePrevented`] when STP cancels the taker
    /// before any fills occur. A sweep stopped by a failed price level
    /// comes back as `Ok` with [`MatchOutcome::aborted`] set; the caller
    /// publishes the committed prefix and returns the abort (#240).
    pub(crate) fn match_order_by_amount_with_user(
        &self,
        order_id: Id,
        side: Side,
        amount: u128,
        taker_user_id: Hash32,
    ) -> Result<MatchOutcome, OrderBookError> {
        self.match_order_inner(
            order_id,
            side,
            MatchMode::QuoteAmount { amount },
            taker_user_id,
            TakerKind::Standard,
            0,
        )
    }

    /// Unified matching loop driven by [`MatchMode`] / [`StopCondition`].
    ///
    /// One inner implementation handles both base-quantity and
    /// quote-notional walks. The `BaseQty` path is identical in shape to
    /// the previous implementation: `level_qty_cap` is a no-op for
    /// `lot <= 1` and a single `% lot` otherwise; `consume` is one
    /// `saturating_sub` per level. The `QuoteAmount` path adds one
    /// `u128` divide per level (to derive the per-level qty cap) and one
    /// `u128` multiply per fill (to deduct from the remaining notional).
    ///
    /// `lot_size`, when configured, is enforced uniformly: per-level qty
    /// is rounded **down** to a multiple of `lot`. This complements the
    /// admission-time validation in `modifications.rs` and ensures
    /// notional walks never emit `qty=0` trades when budget is below one
    /// full lot.
    ///
    /// # Aborted sweeps (#240)
    ///
    /// A level whose `MatchResult::error()` is set committed a prefix and
    /// then failed; folding a level's trades into the aggregate can fail as
    /// well. Either way the sweep stops at that level — no later (worse)
    /// level is touched — the post-sweep bookkeeping still runs for every
    /// maker actually consumed, the taker is recorded as
    /// `Cancelled { MatchAborted }` with the committed quantity, and the
    /// outcome comes back with [`MatchOutcome::aborted`] set and `result`
    /// holding exactly the committed prefix.
    ///
    /// `reserve_steps > 0` (fill-or-kill only) reserves the trade and
    /// filled-id buffers before the first level is touched; a refused
    /// reservation rejects the taker untouched with
    /// [`OrderBookError::PriceLevelError`] (`CapacityExceeded`).
    fn match_order_inner(
        &self,
        order_id: Id,
        side: Side,
        mode: MatchMode,
        taker_user_id: Hash32,
        taker_kind: TakerKind,
        reserve_steps: u64,
    ) -> Result<MatchOutcome, OrderBookError> {
        self.cache.invalidate();
        let mut match_result =
            MatchResult::new(order_id, Quantity::new(mode.initial_match_quantity()));
        let mut stop = StopCondition::from_mode(&mode);
        let limit_price = mode.limit_price();
        let lot = self.lot_size.unwrap_or(1);
        // Deterministic taker timestamp for per-level matching: `pricelevel` 0.8's
        // `match_order` no longer reads the wall clock. Computed once so every trade
        // in this submit shares the taker's match time and replay stays deterministic.
        let taker_ts = self.clock().now_millis();

        // Determine if STP checks are needed for this match
        let stp_active = self.stp_mode.is_enabled() && taker_user_id != Hash32::zero();

        // Choose the appropriate side for matching
        let match_side = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };

        // Early exit if the opposite side is empty
        if match_side.is_empty() {
            return self
                .empty_book_result(side, &mode, match_result)
                .map(MatchOutcome::resting);
        }

        // Get reusable vectors from pool. `filled_orders` / `empty_price_levels`
        // are needed by every sweep. `stp_orders` is the per-level STP scan
        // scratch buffer (#107), reused across conflicting levels instead of a
        // fresh allocation per level — but it is only acquired when STP is
        // active, so the default (`STPMode::None`) hot path touches the pool
        // exactly as it did before #107 (an empty `Vec::new()` allocates nothing
        // and is never filled or returned).
        //
        // #230: makers that would strand hidden depth if this sweep consumed
        // them, captured per level BEFORE it is matched (the level drops them
        // during the match, so the hidden quantity is unrecoverable after).
        // Same `(Id, quantity)` shape as `filled_orders`, so it reuses the
        // same pool.
        //
        // The gate is read ONCE here, not per level: on a book that has never
        // rested a non-auto-replenishing reserve with hidden depth the whole
        // strandable path drops out of this sweep — `None` means no pool
        // acquire, no per-level capture, no lookup in the drain loop and
        // nothing to return. That matters because the capture walks the
        // level's `DashMap` of orders, which read-locks every shard.
        let watch_strandable = self.strandable_makers_resting.load(Ordering::Relaxed) > 0;
        let (mut filled_orders, mut empty_price_levels, mut strandable_makers) = MATCHING_POOL
            .with(|pool| {
                (
                    pool.get_filled_orders_vec(),
                    pool.get_price_vec(),
                    watch_strandable.then(|| pool.get_filled_orders_vec()),
                )
            });
        let mut stp_orders = if stp_active {
            MATCHING_POOL.with(|pool| pool.get_order_snapshot_vec())
        } else {
            Vec::new()
        };

        // Fill-or-kill preflight (#240): reserve the aggregate trade /
        // filled-id storage and the pooled maker buffer for every maker
        // step the feasibility walk predicted BEFORE the first level is
        // touched, so a refused allocation kills the taker with the book
        // untouched instead of stopping the sweep half-way.
        if reserve_steps > 0
            && let Err(err) =
                Self::reserve_sweep_steps(&mut match_result, &mut filled_orders, reserve_steps)
        {
            release_sweep_buffers(
                filled_orders,
                empty_price_levels,
                strandable_makers,
                stp_orders,
                stp_active,
            );
            return Err(self.reject_untouched(order_id, err));
        }

        // Track whether STP cancelled the taker
        let mut stp_taker_cancelled = false;
        let mut post_only_rejected = false;
        // First pricelevel failure hit by the sweep (#240): a level's
        // `MatchResult::error()`, a refused fold of its committed prefix or
        // of the level's worst-case reservation, or a failed STP
        // `snapshot_by_seq_into` (both taken before the level is touched, so
        // the prefix is that of the earlier levels). The sweep stops
        // at it and never walks on to a worse level; the post-sweep
        // bookkeeping below still runs so `order_locations`, the user index
        // and the level map stay consistent with the makers already
        // consumed, and the committed prefix is surfaced as an abort.
        let mut sweep_error: Option<PriceLevelError> = None;
        // A failed post-only probe (#240): no trade and no STP action can
        // have happened, so it is a clean, untouched rejection.
        let mut post_only_probe_error: Option<PriceLevelError> = None;

        // Iterate through prices in optimal order (already sorted by SkipMap)
        // For buy orders: iterate asks in ascending order (best ask first)
        // For sell orders: iterate bids in descending order (best bid first)
        let price_iter = match side {
            Side::Buy => Either::Left(match_side.iter()),
            Side::Sell => Either::Right(match_side.iter().rev()),
        };

        // Process each price level
        for entry in price_iter {
            let price = *entry.key();
            // Check price limit constraint early (only set for limit orders)
            if let Some(limit) = limit_price {
                match side {
                    Side::Buy if price > limit => break,
                    Side::Sell if price < limit => break,
                    _ => {}
                }
            }

            // Compute per-level base-qty cap respecting both the budget
            // (base-qty or notional) and `lot_size`. A zero cap means
            // dust-below-lot at the current price: nothing can execute
            // *here*. Whether that also ends the walk depends on the
            // direction it runs in — a notional sell keeps descending to
            // cheaper bids, everything else stops. See
            // `StopCondition::zero_cap_is_terminal`.
            let qty_cap = stop.level_qty_cap(price, lot);
            if qty_cap == 0 {
                if stop.zero_cap_is_terminal(side, lot) {
                    break;
                }
                continue;
            }

            // Get price level value from the entry
            let price_level = entry.value();

            // --- Post-only crossability verdict (#209) ---
            // Resolved BEFORE the STP block: a post-only taker must be a
            // pure no-op on rejection, and the CancelMaker / CancelBoth
            // arms below cancel same-user makers as a side effect. The
            // probe delegates to pricelevel's structural guard — for a
            // `TakerKind::PostOnly` taker `match_order` is a guaranteed
            // zero-mutation dry run: `Rejected` when the level holds
            // matchable depth (self-id-skips and unmatchable resting
            // orders excluded), `NotFilled` otherwise. Post-only
            // therefore always resolves to either a clean walk-on or a
            // clean `PriceCrossing` rejection — same policy as the
            // `will_cross_market` precheck, makers untouched, STP never
            // consulted (post-only precedence over STP is intentional
            // and documented on `update_order` / `add_post_only_order`).
            if taker_kind.is_post_only() {
                let probe = price_level.match_order(
                    qty_cap,
                    order_id,
                    TimeInForce::Gtc,
                    taker_kind,
                    taker_ts,
                    &self.transaction_id_generator,
                );
                // A failed probe (pricelevel could not linearize the depth
                // scan) is marked `Rejected` too, but it carries the typed
                // error and no verdict: surface the failure, never a
                // misleading `PriceCrossing` (#240). A post-only walk never
                // trades and never reaches the STP arms, so the book is
                // provably untouched: the taker is rejected cleanly, not
                // aborted.
                if let Some(err) = probe.error() {
                    post_only_probe_error = Some(err.clone());
                    break;
                }
                if probe.outcome().was_rejected() {
                    post_only_rejected = true;
                    break;
                }
                // No matchable depth at this crossing level; walk on.
                continue;
            }

            // #240: reserve the aggregate result (and the pooled filled-maker
            // buffer) for the most trades this level can emit BEFORE anything
            // touches it — including the STP arms below, whose CancelMaker /
            // CancelBoth branches cancel same-user makers — so folding its
            // committed trades cannot fail after the level was mutated. The
            // bound uses the pre-cancel maker count, slightly conservative
            // when STP then removes makers. A refused reservation aborts the
            // sweep here with the prefix of the earlier levels, this level
            // intact.
            if let Err(err) = Self::reserve_level_worst_case(
                &mut match_result,
                &mut filled_orders,
                price_level,
                qty_cap,
            ) {
                sweep_error = Some(err);
                break;
            }

            // --- STP pre-processing ---
            // When STP is active, check for self-trade conflicts before matching.
            // This is done per-price-level to handle partial fills correctly.
            if stp_active {
                // `check_stp_at_level` must see the resting orders in the exact order
                // the sweep consumes them — pure insertion sequence — so `safe_quantity`
                // and the CancelBoth `maker_order_id` correspond to what `match_order`
                // will actually fill/cancel. `snapshot_by_seq_into` (pricelevel 0.8.2)
                // refills the pooled `stp_orders` scratch buffer in that exact order
                // (clearing it first), so we reuse one allocation across every level
                // instead of allocating a fresh `Vec` per conflicting level (#107). The
                // order is identical to `snapshot_by_insertion_seq`: deterministic (fixing
                // the #94 DashMap non-determinism) AND faithful to the sweep even under
                // non-monotonic timestamps, unlike `snapshot_orders()` which is
                // `(timestamp, seq)`-ordered (the residual gap closed by #132 /
                // PriceLevel#102).
                // On error the buffer still holds the previous level's
                // orders (pricelevel leaves it untouched), so the STP verdict
                // must not be taken on it: stop before touching this level.
                if let Err(err) = price_level.snapshot_by_seq_into(&mut stp_orders) {
                    sweep_error = Some(err);
                    break;
                }
                let action = check_stp_at_level(&stp_orders, taker_user_id, self.stp_mode);

                // #225: test-only interleaving point. The verdict above was
                // taken on the queue state we just snapshotted; this is the
                // exact instant a competing mutation used to be able to slip
                // in before the level is acted on below. The hook lets a unit
                // test park here and drive that competitor deterministically.
                // Compiled out entirely outside `cfg(test)`.
                #[cfg(test)]
                if let Some(hook) = self.stp_interleave_hook.as_ref() {
                    hook(price);
                }

                match action {
                    STPAction::NoConflict => {
                        // No self-trade at this level; match normally below
                    }

                    STPAction::CancelTaker { safe_quantity } => {
                        // Match up to safe_quantity, then cancel the taker
                        if safe_quantity > 0 {
                            let match_qty = qty_cap.min(safe_quantity);
                            if match_qty > 0 {
                                if let Some(strandable) = strandable_makers.as_mut() {
                                    self.capture_strandable_makers(price_level, strandable);
                                }
                                let price_level_match = price_level.match_order(
                                    match_qty,
                                    order_id,
                                    TimeInForce::Gtc,
                                    taker_kind,
                                    taker_ts,
                                    &self.transaction_id_generator,
                                );
                                // #225: every maker filled here must come
                                // from the snapshot the STP verdict was taken
                                // on. A maker outside it means something
                                // landed between the scan and the sweep, i.e.
                                // the exclusive submit gate was not held.
                                debug_assert!(
                                    price_level_match.trades().as_vec().iter().all(|t| {
                                        stp_orders.iter().any(|o| o.id() == t.maker_order_id())
                                    }),
                                    "#225: CancelTaker pre-match filled a maker absent from the STP snapshot"
                                );
                                let executed = match_qty.saturating_sub(
                                    price_level_match.remaining_quantity().as_u64(),
                                );
                                if let Err(err) = self.process_level_match(
                                    &mut match_result,
                                    &price_level_match,
                                    &mut filled_orders,
                                    price,
                                    price_level,
                                    side,
                                    &mut empty_price_levels,
                                ) {
                                    sweep_error = Some(err);
                                    break;
                                }
                                stop.consume(executed, price);
                            }
                        }
                        // Reachability: the same-user maker is only reached
                        // if the taker can still execute at this price after
                        // the non-self depth in front of it. A budget the
                        // pre-match exhausted is an ordinary complete fill.
                        // Quote-notional dust — a residual that cannot fund
                        // one more unit at this price, the usual end of a
                        // notional sweep since `is_done()` is exact zero —
                        // cannot execute here either, so the maker is
                        // unreachable and survives, and the sweep walks on
                        // rather than breaking: a notional sell can still
                        // afford a whole lot at a cheaper bid (for a buy the
                        // next ask is dearer and the loop's own cap check
                        // ends the sweep). A base-quantity residual keeps
                        // the STP verdict whatever its size: walked past, it
                        // would rest crossed against the same-user maker
                        // (see `StopCondition::is_dust_at`).
                        // `check_modify_stp_self_cross` dry-runs the same
                        // decision on the modify path (#168).
                        if stop.is_done() {
                            break;
                        }
                        if stop.is_dust_at(price, lot) {
                            continue;
                        }
                        stp_taker_cancelled = true;
                        break;
                    }

                    STPAction::CancelMaker => {
                        // Cancel same-user resting orders, then match normally. We
                        // re-scan the pooled snapshot in insertion-sequence order —
                        // the exact order the old `maker_order_ids` Vec held — so the
                        // cancel order (and therefore emitted events / journal) is
                        // bit-identical to before, with no per-level allocation (#107).
                        // Each cancel runs on the level we already hold — it emits the
                        // level-change event, records OrderStatus::Cancelled
                        // { SelfTradePrevention }, and releases the per-account risk slot
                        // in lockstep, but does NOT remove the level from the map (no
                        // order_locations re-resolution either), so level removal stays
                        // with the post-walk empty_price_levels drain (#95).
                        for order in &stp_orders {
                            if order.user_id() == taker_user_id {
                                self.cancel_resting_maker_on_level(
                                    price_level,
                                    side.opposite(),
                                    order.id(),
                                    CancelReason::SelfTradePrevention,
                                );
                            }
                        }
                        // If the level is now empty, mark for removal and continue
                        if price_level.order_count() == 0 {
                            empty_price_levels.push(price);
                            continue;
                        }
                        // Fall through to normal matching below
                    }

                    STPAction::CancelBoth {
                        safe_quantity,
                        maker_order_id,
                    } => {
                        // Match up to safe_quantity, cancel the maker, then cancel taker
                        if safe_quantity > 0 {
                            let match_qty = qty_cap.min(safe_quantity);
                            if match_qty > 0 {
                                if let Some(strandable) = strandable_makers.as_mut() {
                                    self.capture_strandable_makers(price_level, strandable);
                                }
                                let price_level_match = price_level.match_order(
                                    match_qty,
                                    order_id,
                                    TimeInForce::Gtc,
                                    taker_kind,
                                    taker_ts,
                                    &self.transaction_id_generator,
                                );
                                // #225: see the CancelTaker arm — the makers
                                // filled here must all belong to the snapshot
                                // the STP verdict was taken on.
                                debug_assert!(
                                    price_level_match.trades().as_vec().iter().all(|t| {
                                        stp_orders.iter().any(|o| o.id() == t.maker_order_id())
                                    }),
                                    "#225: CancelBoth pre-match filled a maker absent from the STP snapshot"
                                );
                                let executed = match_qty.saturating_sub(
                                    price_level_match.remaining_quantity().as_u64(),
                                );
                                if let Err(err) = self.process_level_match(
                                    &mut match_result,
                                    &price_level_match,
                                    &mut filled_orders,
                                    price,
                                    price_level,
                                    side,
                                    &mut empty_price_levels,
                                ) {
                                    sweep_error = Some(err);
                                    break;
                                }
                                stop.consume(executed, price);
                            }
                        }
                        // Same reachability rule as `CancelTaker` above, and
                        // here it also gates the maker cancellation: a maker
                        // the taker never reached must survive untouched,
                        // whether the budget is spent or only quote-notional
                        // dust is left at this price. A base-quantity
                        // residual still cancels both.
                        if stop.is_done() {
                            break;
                        }
                        if stop.is_dust_at(price, lot) {
                            continue;
                        }
                        // Cancel the maker on the held level for the same lockstep
                        // event + state + risk effects as CancelMaker (#95); level
                        // removal stays with the empty_price_levels drain below.
                        self.cancel_resting_maker_on_level(
                            price_level,
                            side.opposite(),
                            maker_order_id,
                            CancelReason::SelfTradePrevention,
                        );
                        if price_level.order_count() == 0 {
                            empty_price_levels.push(price);
                        }
                        stp_taker_cancelled = true;
                        break;
                    }
                }
            }

            // --- Normal matching (no STP conflict or after CancelMaker cleanup) ---
            // The level's worst case was reserved above, before the STP arms.
            if let Some(strandable) = strandable_makers.as_mut() {
                self.capture_strandable_makers(price_level, strandable);
            }
            // #230: park here in tests — after this level's capture and
            // before its match — so a competitor can be driven against both
            // windows: admitting a strandable maker into a level the sweep
            // has not reached, and cancelling one the sweep has already
            // captured (then reusing its id).
            #[cfg(test)]
            if let Some(hook) = self.level_interleave_hook.as_ref() {
                hook(price);
            }
            let price_level_match = price_level.match_order(
                qty_cap,
                order_id,
                TimeInForce::Gtc,
                taker_kind,
                taker_ts,
                &self.transaction_id_generator,
            );
            // #225: when STP ran for this level, the sweep may only fill
            // makers the verdict was taken on. `stp_orders` is only populated
            // while `stp_active`, hence the guard.
            debug_assert!(
                !stp_active
                    || price_level_match
                        .trades()
                        .as_vec()
                        .iter()
                        .all(|t| { stp_orders.iter().any(|o| o.id() == t.maker_order_id()) }),
                "#225: sweep filled a maker absent from the STP snapshot"
            );
            let executed = qty_cap.saturating_sub(price_level_match.remaining_quantity().as_u64());

            if let Err(err) = self.process_level_match(
                &mut match_result,
                &price_level_match,
                &mut filled_orders,
                price,
                price_level,
                side,
                &mut empty_price_levels,
            ) {
                sweep_error = Some(err);
                break;
            }
            stop.consume(executed, price);

            // Early exit if budget is exhausted
            if stop.is_done() {
                break;
            }
        }

        // Batch remove empty price levels
        let levels_removed = !empty_price_levels.is_empty();
        for price in &empty_price_levels {
            match_side.remove(price);
        }
        if levels_removed {
            // Refresh the operational depth gauges now that levels may
            // have been removed. No-op when the `metrics` feature is
            // disabled.
            self.record_depth_metric();
        }

        // Batch remove filled orders from tracking and update state. Each entry
        // carries the maker's TRUE filled quantity (captured per-level in
        // `process_level_match`), so OrderStateTracker / lifecycle consumers and
        // any audit/risk reconciliation that sums filled quantity from terminal
        // events see the real executed amount instead of a `0` placeholder (#104).
        // `Filled { filled_quantity }` here is the executed quantity, which for
        // a removed non-auto-replenishing reserve maker is its visible tranche
        // only: `pricelevel` drops the hidden depth behind it rather than
        // refreshing (#230). Report that discard exactly as the aggressive
        // residual guard in `add_order_inner` does, so both sides of the trade
        // feed the same counter and the same `INFO` trace, distinguished by
        // `path`. `strandable_makers` is `None` on every sweep of a book that
        // never rested such a maker, so the lookup is skipped entirely there.
        // Sorted once so the per-maker lookup below is a binary search:
        // O((S + F) log S) for S captured and F filled, instead of the
        // O(S * F) linear scan. The report order is the `filled_orders`
        // order, which this does not touch.
        if let Some(strandable) = strandable_makers.as_mut() {
            strandable.sort_unstable_by_key(|(id, _)| id.as_bytes());
        }
        for (filled_id, filled_quantity) in &filled_orders {
            self.track_state(
                *filled_id,
                OrderStatus::Filled {
                    filled_quantity: *filled_quantity,
                },
            );
            if let Some(strandable) = strandable_makers.as_ref()
                && let Some(discarded_hidden) = find_strandable(strandable, *filled_id)
            {
                tracing::info!(
                    path = "maker",
                    order_id = %filled_id,
                    executed_quantity = *filled_quantity,
                    discarded_hidden_quantity = discarded_hidden,
                    "reserve maker removed: visible tranche exhausted without auto-replenishment"
                );
                crate::orderbook::metrics::record_reserve_hidden_discarded(discarded_hidden);
                // #230: the fill drain is the third and last place a
                // strandable maker leaves a level. Being in the capture list
                // AND in `filled_orders` is exactly that — and, because a
                // sweep in such a book runs exclusively, the two really are
                // the same order rather than an id reused in between.
                self.note_removed_strandable_maker();
            }
            self.order_locations.remove(filled_id);
            self.untrack_order_by_id(filled_id);
        }

        // Return vectors to pool for reuse. `stp_orders` only entered the pool
        // when STP was active; otherwise it is an empty, never-filled `Vec` that
        // is simply dropped.
        release_sweep_buffers(
            filled_orders,
            empty_price_levels,
            strandable_makers,
            stp_orders,
            stp_active,
        );

        // #240: a sweep stopped by a pricelevel failure. The bookkeeping above
        // already made the book's indices match the makers actually consumed;
        // `match_result` holds exactly the committed prefix. Hand it back as
        // an abort so the caller publishes it like a partial fill and never
        // rests the remainder.
        if let Some(source) = post_only_probe_error {
            return Err(self.reject_untouched(order_id, source));
        }
        if let Some(source) = sweep_error {
            return self.abort_sweep(order_id, &mode, match_result, source);
        }

        let no_fills = match_result.trades().as_vec().is_empty();

        // If STP cancelled the taker and no fills occurred at all, return STP error.
        // When partial fills happened, return Ok with the partial result so the
        // caller can see what was executed.
        if stp_taker_cancelled && no_fills {
            self.track_state(
                order_id,
                OrderStatus::Cancelled {
                    filled_quantity: 0,
                    reason: CancelReason::SelfTradePrevention,
                },
            );
            crate::orderbook::metrics::record_reject(
                crate::orderbook::reject_reason::RejectReason::SelfTradePrevention,
            );
            return Err(OrderBookError::SelfTradePrevented {
                mode: self.stp_mode,
                taker_order_id: order_id,
                user_id: taker_user_id,
            });
        }

        // Check for insufficient liquidity on market paths.
        if no_fills {
            match mode {
                MatchMode::BaseQty {
                    quantity,
                    limit_price: None,
                } => {
                    crate::orderbook::metrics::record_reject(
                        crate::orderbook::reject_reason::RejectReason::InsufficientLiquidity,
                    );
                    return Err(OrderBookError::InsufficientLiquidity {
                        side,
                        requested: quantity,
                        available: 0,
                    });
                }
                MatchMode::QuoteAmount { amount } => {
                    crate::orderbook::metrics::record_reject(
                        crate::orderbook::reject_reason::RejectReason::InsufficientLiquidity,
                    );
                    return Err(OrderBookError::InsufficientLiquidityNotional {
                        side,
                        requested: amount,
                        spent: 0,
                    });
                }
                MatchMode::BaseQty {
                    limit_price: Some(_),
                    ..
                } => {
                    // Limit orders that fail to match return Ok with an
                    // empty result — the unfilled portion becomes resting
                    // depth in the caller-driven flow.
                }
            }
        }

        // Normalize the quote-notional path so the public `MatchResult`
        // does not leak the `u64::MAX` working sentinel through
        // `remaining_quantity()`. The notional path measures progress in
        // quote currency, not base qty, so the natural meaning of
        // "remaining base qty" is zero — the residual the caller cares
        // about is `requested - executed_value`, available directly on
        // `MatchResult`.
        if matches!(mode, MatchMode::QuoteAmount { .. }) {
            match_result = Self::normalize_notional_match_result(order_id, match_result);
        }

        Ok(MatchOutcome {
            result: match_result,
            taker_stp_cancelled: stp_taker_cancelled,
            taker_post_only_rejected: post_only_rejected,
            aborted: None,
        })
    }

    /// Rebuild a `MatchResult` produced by the quote-notional path so its
    /// internal `remaining_quantity` is `0` (rather than
    /// `u64::MAX - executed_qty`). Trade list, filled-order ids, and
    /// monotonic engine sequence stamping are preserved.
    fn normalize_notional_match_result(order_id: Id, src: MatchResult) -> MatchResult {
        let executed_qty: u64 = src
            .trades()
            .as_vec()
            .iter()
            .map(|t| t.quantity().as_u64())
            .fold(0u64, u64::saturating_add);
        let mut rebuilt = MatchResult::new(order_id, Quantity::new(executed_qty));
        for trade in src.trades().as_vec() {
            // `add_trade` only fails on underflow; with `executed_qty`
            // exactly equal to the sum of trade quantities this cannot
            // underflow. Treat any error as a logic bug surfaced by
            // returning the original (unnormalized) result.
            if rebuilt.add_trade(*trade).is_err() {
                return src;
            }
        }
        for filled_id in src.filled_order_ids() {
            // Same fallback as `add_trade`: a refused append (capacity,
            // pricelevel 0.10) keeps the original, complete result.
            if rebuilt.add_filled_order_id(*filled_id).is_err() {
                return src;
            }
        }
        rebuilt
    }

    /// Build the empty-book result. Market paths return a typed error;
    /// limit paths return `Ok` with a zero-trade `MatchResult` so the
    /// caller can rest the order.
    #[cold]
    fn empty_book_result(
        &self,
        side: Side,
        mode: &MatchMode,
        match_result: MatchResult,
    ) -> Result<MatchResult, OrderBookError> {
        match mode {
            MatchMode::BaseQty {
                quantity,
                limit_price: None,
            } => {
                crate::orderbook::metrics::record_reject(
                    crate::orderbook::reject_reason::RejectReason::InsufficientLiquidity,
                );
                Err(OrderBookError::InsufficientLiquidity {
                    side,
                    requested: *quantity,
                    available: 0,
                })
            }
            MatchMode::QuoteAmount { amount } => {
                crate::orderbook::metrics::record_reject(
                    crate::orderbook::reject_reason::RejectReason::InsufficientLiquidity,
                );
                Err(OrderBookError::InsufficientLiquidityNotional {
                    side,
                    requested: *amount,
                    spent: 0,
                })
            }
            MatchMode::BaseQty {
                limit_price: Some(_),
                ..
            } => Ok(match_result),
        }
    }

    /// Record every maker resting at `price_level` whose hidden depth this
    /// sweep would **strand** if it consumed the maker's visible tranche
    /// (#230): a [`OrderType::ReserveOrder`] with `auto_replenish == false`
    /// and hidden quantity behind it. `pricelevel` removes such a maker once
    /// its visible tranche is fully taken, dropping the hidden depth instead
    /// of refreshing from it, and the order body is gone by the time the
    /// match returns — so the amount has to be captured beforehand to be
    /// reportable at all.
    ///
    /// Appends to `out`; the caller drains it after the sweep, matching ids
    /// against the makers the level actually removed. Every level is matched
    /// at most once per sweep, so a maker is captured at most once.
    ///
    /// Cost:
    ///
    /// - On a book with no strandable maker resting — the overwhelmingly
    ///   common case, and the one `strandable_makers_resting` detects — this
    ///   is never called at all: `match_order_inner` reads the count once
    ///   per sweep and skips the buffer, the captures and the drain lookup
    ///   wholesale. While one does rest, a level with no hidden depth costs
    ///   one relaxed atomic load here and nothing else.
    /// - On a book that did rest one, every level holding hidden depth
    ///   (so any two-tranche kind, not just the strandable ones) pays a
    ///   full pass over the level's resting orders. `iter_orders` is
    ///   `DashMap::iter` upstream, which read-locks **every shard** of the
    ///   map regardless of how few orders rest at the level, so this pass
    ///   is **not** free and shows up in the tails. The flag exists to
    ///   confine it to the books where it can actually report something.
    ///   The `reserve_sweep_hdr` benchmark covers both arms; see `BENCH.md`
    ///   for the current figures.
    #[inline]
    fn capture_strandable_makers(
        &self,
        price_level: &std::sync::Arc<pricelevel::PriceLevel>,
        out: &mut Vec<(Id, u64)>,
    ) {
        if self.strandable_makers_resting.load(Ordering::Relaxed) == 0
            || price_level.hidden_quantity() == 0
        {
            return;
        }
        for order in price_level.iter_orders() {
            if let OrderType::ReserveOrder {
                hidden_quantity,
                auto_replenish: false,
                ..
            } = order.as_ref()
                && hidden_quantity.as_u64() > 0
            {
                out.push((order.id(), hidden_quantity.as_u64()));
            }
        }
    }

    /// Processes match results from a single price level, updating the
    /// aggregate match result and bookkeeping vectors.
    ///
    /// Extracted to avoid code duplication between the normal path and
    /// the STP safe-quantity pre-match path. Routes outbound `engine_seq`
    /// stamping through [`OrderBook::next_engine_seq`] so the minting
    /// contract has a single source of truth.
    ///
    /// The book's installed `risk_state` is consulted on every trade so
    /// the maker's per-account `resting_notional` (and `open_count` on
    /// full fill) is decremented. The hook is a no-op when no
    /// `RiskConfig` is installed, matching the rest of the risk plumbing.
    ///
    /// The level's committed trades are folded into `match_result` **all or
    /// nothing** (#240): the exact room they need is reserved first, so the
    /// aggregate either carries every trade the level committed or, when the
    /// reservation is refused, none of them. The makers the level consumed
    /// are always recorded in `filled_orders` and the risk / price-level
    /// listener hooks always run, because those mirror the level's real
    /// state whatever the aggregate could hold.
    ///
    /// # Errors
    ///
    /// Returns the level's own failure (`MatchResult::error()`, the root
    /// cause: pricelevel committed the prefix it reports and then stopped)
    /// or, failing that, the refused fold. The caller stops the sweep on
    /// `Err`; every trade the level committed has been accounted for above.
    #[allow(clippy::too_many_arguments)]
    fn process_level_match(
        &self,
        match_result: &mut MatchResult,
        price_level_match: &MatchResult,
        filled_orders: &mut Vec<(Id, u64)>,
        price: u128,
        price_level: &std::sync::Arc<pricelevel::PriceLevel>,
        side: Side,
        empty_price_levels: &mut Vec<u128>,
    ) -> Result<(), PriceLevelError> {
        let level_trades = price_level_match.trades().as_vec();
        let level_filled = price_level_match.filled_order_ids();
        // Fallback only. The sweep already reserved this level's worst case
        // (`reserve_level_worst_case`) before touching it, so this is a no-op
        // unless the level emitted more trades than that bound: a
        // replenishing iceberg / reserve maker trading again, or a maker
        // admitted by a concurrent submit on the shared gate. It then grows
        // the aggregate so the fold stays all or nothing. A lone trade needs
        // no reservation (`add_trade` reserves its slot atomically).
        let needs_reserve = level_trades.len() > 1 || !level_filled.is_empty();
        let mut first_error: Option<PriceLevelError> = if needs_reserve {
            match_result.try_reserve(level_trades.len()).err()
        } else {
            None
        };
        let fold = first_error.is_none();

        // Process trades if any occurred
        if !level_trades.is_empty() {
            // Update last trade price atomically
            self.last_trade_price.store(price);
            self.has_traded.store(true, Ordering::Relaxed);

            // Add trades to result and update per-account risk counters
            // for the maker side of every trade.
            for trade in level_trades {
                // Room is reserved and pricelevel validated the quantities,
                // so a failure here is an invariant breach; the maker is
                // already mutated, so keep accounting and report it.
                if fold && let Err(err) = match_result.add_trade(*trade) {
                    first_error.get_or_insert(err);
                }
                self.risk_state.on_fill(
                    trade.maker_order_id(),
                    trade.quantity().as_u64(),
                    trade.price().as_u128(),
                );
            }

            // Notify price level changes
            if let Some(listener) = &self.price_level_changed_listener {
                let engine_seq = self.next_engine_seq();
                listener(PriceLevelChangedEvent {
                    side: side.opposite(),
                    price: price_level.price(),
                    quantity: price_level.visible_quantity(),
                    engine_seq,
                });
            }
        }

        // Collect fully-consumed makers for batch removal, each with its true
        // filled quantity. Sum the maker's trades from THIS per-level result,
        // where `filled_order_ids()` and `trades()` are kept consistent by
        // pricelevel (an id is recorded only after its trade is added) — so the
        // recorded `Filled { filled_quantity }` stays correct even if the
        // aggregate fold was refused (#104). Per-level trade counts are small;
        // this is the cold path, not the matching hot loop.
        for &filled_order_id in level_filled {
            if fold && let Err(err) = match_result.add_filled_order_id(filled_order_id) {
                first_error.get_or_insert(err);
            }
            // A maker's trades at one level sum to at most its own quantity,
            // so the checked sum cannot overflow; a `None` is an invariant
            // breach, reported rather than clamped.
            let filled_quantity = level_trades
                .iter()
                .filter(|trade| trade.maker_order_id() == filled_order_id)
                .try_fold(0u64, |acc, trade| {
                    acc.checked_add(trade.quantity().as_u64())
                });
            let filled_quantity = match filled_quantity {
                Some(quantity) => quantity,
                None => {
                    first_error.get_or_insert_with(|| PriceLevelError::InvalidOperation {
                        message: format!(
                            "filled quantity of maker {filled_order_id} overflows u64"
                        ),
                    });
                    0
                }
            };
            filled_orders.push((filled_order_id, filled_quantity));
            // The maker left its level. `on_fill` above already released a
            // normally exhausted maker (no-op here); a non-auto-replenishing
            // reserve maker was removed with its hidden tranche discarded
            // (#230), and that remainder is released now, in the same
            // removal, instead of staying booked forever (#243 review).
            self.risk_state.on_maker_removed(filled_order_id);
        }

        // Check if price level is empty and mark for removal
        if price_level.order_count() == 0 {
            empty_price_levels.push(price);
        }

        if !fold {
            // The aggregate could not take this level's committed trades.
            // The level, the makers' risk and order state already reflect
            // them, the trade stream will not. This is the ONLY path on which
            // the streams can disagree (see `doc/panic-boundaries.md`): say
            // so loudly and count it so it is detectable in production.
            Self::bump_diagnostic_counter(&self.match_fold_failures, "match_fold_failures");
            crate::orderbook::metrics::record_match_fold_failure();
            tracing::error!(
                price,
                level_trade_count = level_trades.len(),
                "committed trades of a price level could not be folded into the taker's result"
            );
        }

        // The level's own failure is the root cause: pricelevel committed
        // the prefix folded above and then stopped.
        match price_level_match.error() {
            Some(level_err) => Err(level_err.clone()),
            None => match first_error {
                Some(err) => Err(err),
                None => Ok(()),
            },
        }
    }

    /// Reserve the aggregate result and the pooled filled-maker buffer for
    /// the most trades one level can emit for a taker capped at `qty_cap`
    /// (#240): `min(resting makers, qty_cap)`. Each maker trades at most
    /// once unless a replenishing iceberg / reserve comes back with a fresh
    /// tranche, and every trade takes at least one unit.
    ///
    /// Residual (documented in `doc/panic-boundaries.md`): replenishment
    /// trades, and makers admitted to the level by a concurrent submit on
    /// the shared gate while it is being swept, can exceed this bound; the
    /// extra slots then grow during the fold, and only an allocator refusal
    /// at that point leaves the level's trades out of the aggregate.
    ///
    /// # Errors
    ///
    /// [`PriceLevelError::CapacityExceeded`] when a buffer cannot grow; the
    /// level has not been touched.
    #[inline]
    fn reserve_level_worst_case(
        match_result: &mut MatchResult,
        filled_orders: &mut Vec<(Id, u64)>,
        price_level: &pricelevel::PriceLevel,
        qty_cap: u64,
    ) -> Result<(), PriceLevelError> {
        let makers = u64::try_from(price_level.order_count()).unwrap_or(u64::MAX);
        Self::reserve_sweep_steps(match_result, filled_orders, makers.min(qty_cap))
    }

    // PriceLevel#219: `MatchResult::try_reserve` reserves trades and filled
    // ids with one count, so a level with no full fill still allocates the
    // filled-id vector, and the `min(makers, qty)` bound over-reserves on
    // deep levels. Split reservations / `try_absorb` upstream will remove
    // both; do not skip the pre-reservation in the meantime.
    /// Reserve room for `steps` maker steps in the aggregate result (trades
    /// and filled ids) and the pooled filled-maker buffer: the fill-or-kill
    /// preflight and the per-level worst case (#240). Amortized growth: a
    /// no-op when the room is already there, and the pooled buffer is
    /// reused across sweeps.
    ///
    /// # Errors
    ///
    /// [`PriceLevelError::CapacityExceeded`] when a buffer cannot grow (or
    /// `steps` does not fit `usize`); nothing observable has changed.
    #[inline]
    fn reserve_sweep_steps(
        match_result: &mut MatchResult,
        filled_orders: &mut Vec<(Id, u64)>,
        steps: u64,
    ) -> Result<(), PriceLevelError> {
        let steps = usize::try_from(steps).map_err(|_| PriceLevelError::CapacityExceeded {
            resource: CapacityResource::Trades,
            additional: usize::MAX,
        })?;
        match_result.try_reserve(steps)?;
        filled_orders
            .try_reserve(steps)
            .map_err(|_| PriceLevelError::CapacityExceeded {
                resource: CapacityResource::FilledOrderIds,
                additional: steps,
            })
    }

    /// Record a taker rejected by a pricelevel failure before any mutation
    /// (#240) — the fill-or-kill preflight, a failed post-only probe, an
    /// exhausted trade-id generator — as a terminal `Rejected` state with
    /// the resource code (`CapacityExceeded` / `CounterExhausted`), the
    /// reject metric, and the typed
    /// [`OrderBookError::PriceLevelError`]. Latches trade-id exhaustion
    /// when that is the cause.
    #[cold]
    #[inline(never)]
    pub(crate) fn reject_untouched(&self, order_id: Id, source: PriceLevelError) -> OrderBookError {
        let err = OrderBookError::PriceLevelError(source);
        let reason = RejectReason::from(&err);
        tracing::warn!(
            order_id = %order_id,
            error = %err,
            "taker rejected by a pricelevel failure before any mutation; book untouched"
        );
        if self.transaction_id_generator.is_exhausted() {
            self.latch_trade_ids_exhausted();
        }
        self.track_state(order_id, OrderStatus::Rejected { reason });
        crate::orderbook::metrics::record_reject(reason);
        err
    }

    /// Turn a sweep stopped at a failed level into an aborted
    /// [`MatchOutcome`] (#240).
    ///
    /// `match_result` is the committed prefix. The taker is recorded as
    /// `Cancelled { filled_quantity: executed, reason: MatchAborted }` —
    /// the same shape as an IOC whose remainder is cancelled — so every
    /// submission API reports one lifecycle, and its remainder is never
    /// rested by any caller.
    ///
    /// # Errors
    ///
    /// Only when the prefix's executed quantity cannot be summed, which the
    /// `MatchResult` invariant (every fold is a checked subtraction from a
    /// `u64` budget) rules out; reported as
    /// [`OrderBookError::PriceLevelError`] rather than guessed.
    #[cold]
    #[inline(never)]
    fn abort_sweep(
        &self,
        order_id: Id,
        mode: &MatchMode,
        mut match_result: MatchResult,
        source: PriceLevelError,
    ) -> Result<MatchOutcome, OrderBookError> {
        if matches!(mode, MatchMode::QuoteAmount { .. }) {
            match_result = Self::normalize_notional_match_result(order_id, match_result);
        }
        let executed_quantity = match_result.executed_quantity()?.as_u64();
        let trade_count = match_result.trades().len();
        tracing::error!(
            order_id = %order_id,
            executed_quantity,
            trade_count,
            error = %source,
            "matching sweep aborted by a price level failure; committed prefix kept, remainder cancelled"
        );
        self.track_state(
            order_id,
            OrderStatus::Cancelled {
                filled_quantity: executed_quantity,
                reason: CancelReason::MatchAborted,
            },
        );
        crate::orderbook::metrics::record_reject(RejectReason::MatchAborted);
        crate::orderbook::metrics::record_match_abort();
        Self::bump_diagnostic_counter(&self.match_aborts, "match_aborts");
        if matches!(
            source,
            PriceLevelError::CapacityExceeded {
                resource: CapacityResource::IdSequence,
                ..
            }
        ) {
            self.latch_trade_ids_exhausted();
        }
        Ok(MatchOutcome {
            result: match_result,
            taker_stp_cancelled: false,
            taker_post_only_rejected: false,
            aborted: Some(OrderBookError::MatchAborted {
                order_id,
                executed_quantity,
                trade_count,
                source: Box::new(source),
            }),
        })
    }

    /// Optimized peek match without memory pooling or sorting
    ///
    /// # Performance Optimization
    /// Uses SkipMap's natural ordering to eliminate sorting overhead.
    /// Time complexity: O(M log N) where M = price levels inspected.
    pub fn peek_match(&self, side: Side, quantity: u64, price_limit: Option<u128>) -> u64 {
        let price_levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };

        if price_levels.is_empty() {
            return 0;
        }

        let mut matched_quantity = 0u64;

        // Iterate through prices in optimal order (already sorted by SkipMap)
        let price_iter = match side {
            Side::Buy => Either::Left(price_levels.iter()),
            Side::Sell => Either::Right(price_levels.iter().rev()),
        };

        // Process each price level
        for entry in price_iter {
            // Early termination when we have enough quantity
            if matched_quantity >= quantity {
                break;
            }

            let price = *entry.key();

            // Check price limit
            if let Some(limit) = price_limit {
                match side {
                    Side::Buy if price > limit => break,
                    Side::Sell if price < limit => break,
                    _ => {}
                }
            }

            // Get available quantity at this level
            let price_level = entry.value();
            let available_quantity = price_level.total_quantity().unwrap_or(0);
            let needed_quantity = quantity.saturating_sub(matched_quantity);
            let quantity_to_match = needed_quantity.min(available_quantity);
            matched_quantity = matched_quantity.saturating_add(quantity_to_match);
        }

        matched_quantity
    }

    /// Faithful fill-or-kill feasibility: the quantity an immediate match of
    /// `quantity` would *actually* fill, mirroring the real walk in
    /// [`Self::match_order_inner`] — `lot_size` budget rounding, self-trade
    /// prevention, and per-order *drawable* depth (a non-auto-replenish reserve's
    /// hidden tranche is dropped unfilled by the sweep, so it is not counted).
    ///
    /// [`Self::peek_match`] only sums raw level depth, so it over-reports when STP
    /// would cancel makers / the taker, or when a non-replenish reserve's hidden is
    /// present. Routing FOK admission through this keeps fill-or-kill
    /// all-or-nothing: an order that cannot be fully filled is killed *before* any
    /// trade is emitted (#96).
    ///
    /// Alongside the fillable quantity the walk measures what the sweep will
    /// draw on (#240): an upper bound on the trades — and so trade ids — it
    /// can emit, and its non-replenishing maker steps, so the fill-or-kill
    /// preflight can check the trade-id headroom and reserve the result
    /// buffers before any mutation. See [`FokFeasibility`].
    ///
    /// # Errors
    ///
    /// Returns [`OrderBookError::PriceLevelError`] when a level's
    /// insertion-sequence view or its `matchable_quantity` dry run fails
    /// (both fallible since pricelevel 0.10). The caller must treat that as
    /// "feasibility unknown" and refuse the order rather than guess: a
    /// failed dry run is a kill, never zero depth.
    pub(crate) fn fok_fillable_quantity(
        &self,
        side: Side,
        quantity: u64,
        price_limit: Option<u128>,
        taker_user_id: Hash32,
        taker_id: Id,
    ) -> Result<FokFeasibility, OrderBookError> {
        let price_levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        let mut feasibility = FokFeasibility {
            fillable: 0,
            max_trades: 0,
            maker_steps: 0,
        };
        if quantity == 0 || price_levels.is_empty() {
            return Ok(feasibility);
        }

        let lot = self.lot_size.unwrap_or(1);
        let stp_active = self.stp_mode.is_enabled() && taker_user_id != Hash32::zero();

        let price_iter = match side {
            Side::Buy => Either::Left(price_levels.iter()),
            Side::Sell => Either::Right(price_levels.iter().rev()),
        };

        for entry in price_iter {
            if feasibility.fillable >= quantity {
                break;
            }

            let price = *entry.key();
            if let Some(limit) = price_limit {
                match side {
                    Side::Buy if price > limit => break,
                    Side::Sell if price < limit => break,
                    _ => {}
                }
            }

            // Lot-round the remaining budget exactly like `StopCondition::level_qty_cap`:
            // a budget below one full lot is dust and stops the walk.
            // `fillable < quantity` holds here (checked at the loop head).
            let Some(needed) = quantity.checked_sub(feasibility.fillable) else {
                break;
            };
            let cap = if lot <= 1 {
                needed
            } else {
                needed
                    .checked_rem(lot)
                    .and_then(|dust| needed.checked_sub(dust))
                    .unwrap_or(0)
            };
            if cap == 0 {
                break;
            }

            let price_level = entry.value();

            // Reachable depth at this level — the quantity the real sweep could
            // actually fill. The non-STP and STP-NoConflict cases delegate to
            // pricelevel's authoritative dry-run `PriceLevel::matchable_quantity`
            // (pricelevel 0.8.2), the single upstream source of truth for what
            // `match_order` would consume — including iceberg/reserve replenishment
            // and the removal of a non-auto-replenish reserve's undrawable hidden —
            // instead of re-deriving it from a hand-rolled per-order estimate that
            // could silently drift from `match_against` (#136, follow-up to #96).
            let (reachable, stop_after) = if stp_active {
                // Insertion-sequence order = the sweep's consumption order, so the
                // feasibility STP decision matches the real match even under
                // non-monotonic timestamps (#132).
                let orders = price_level.snapshot_by_insertion_seq()?;
                match check_stp_at_level(&orders, taker_user_id, self.stp_mode) {
                    // No self-trade: the whole level is reachable — delegate to the
                    // upstream dry run.
                    STPAction::NoConflict => {
                        (price_level.matchable_quantity(cap, taker_id)?, false)
                    }
                    // Same-user makers are cancelled, not filled: only non-self
                    // resting depth is reachable; the walk continues. The upstream
                    // primitive cannot filter by user, so the non-self matchable
                    // depth is still summed per order here.
                    STPAction::CancelMaker => {
                        let non_self: u64 = orders
                            .iter()
                            .filter(|o| o.user_id() != taker_user_id)
                            .map(|o| order_matchable_qty(o))
                            .sum();
                        (non_self, false)
                    }
                    // The taker is cancelled at the first same-user order: it can
                    // fill at most `safe_quantity` (visible-only, matching the real
                    // sweep's cap) here, then stops.
                    STPAction::CancelTaker { safe_quantity }
                    | STPAction::CancelBoth { safe_quantity, .. } => (safe_quantity, true),
                }
            } else {
                (price_level.matchable_quantity(cap, taker_id)?, false)
            };

            let taken = cap.min(reachable);
            feasibility.fillable = feasibility
                .fillable
                .checked_add(taken)
                .ok_or_else(fok_counter_overflow)?;
            // Trades at this level: one per maker without hidden depth
            // (each trades at most once), at most one per unit taken when a
            // replenishing maker can come back for more.
            let makers = u64::try_from(price_level.order_count()).unwrap_or(u64::MAX);
            let steps = makers.min(taken);
            let level_max_trades = if price_level.hidden_quantity() == 0 {
                steps
            } else {
                taken
            };
            feasibility.max_trades = feasibility
                .max_trades
                .checked_add(level_max_trades)
                .ok_or_else(fok_counter_overflow)?;
            feasibility.maker_steps = feasibility
                .maker_steps
                .checked_add(steps)
                .ok_or_else(fok_counter_overflow)?;
            if stop_after {
                break;
            }
        }

        Ok(feasibility)
    }

    /// Batch operation for multiple order matches (additional optimization)
    ///
    /// Each element follows [`Self::match_order`], including its #240
    /// asymmetry: an aborted partial fill is an `Err(MatchAborted)` with a
    /// summary only.
    pub fn match_orders_batch(
        &self,
        orders: &[(Id, Side, u64, Option<u128>)],
    ) -> Vec<Result<MatchResult, OrderBookError>> {
        let mut results = Vec::with_capacity(orders.len());

        for &(order_id, side, quantity, limit_price) in orders {
            let result = OrderBook::<T>::match_order(self, order_id, side, quantity, limit_price);
            results.push(result);
        }

        results
    }
}

#[cfg(test)]
mod stop_condition_tests {
    use super::*;

    #[test]
    fn test_base_qty_cap_no_lot_returns_remaining() {
        let stop = StopCondition::BaseQty { remaining: 1_000 };
        assert_eq!(stop.level_qty_cap(100, 1), 1_000);
    }

    #[test]
    fn test_base_qty_cap_rounds_down_to_lot() {
        let stop = StopCondition::BaseQty { remaining: 1_005 };
        // lot=100 ⇒ 1_005 - (1_005 % 100) = 1_005 - 5 = 1_000
        assert_eq!(stop.level_qty_cap(50, 100), 1_000);
    }

    #[test]
    fn test_base_qty_cap_zero_when_below_lot() {
        let stop = StopCondition::BaseQty { remaining: 5 };
        assert_eq!(stop.level_qty_cap(50, 100), 0);
    }

    #[test]
    fn test_quote_amount_cap_basic() {
        // 10_000 / 100 = 100 base
        let stop = StopCondition::QuoteAmount { remaining: 10_000 };
        assert_eq!(stop.level_qty_cap(100, 1), 100);
    }

    #[test]
    fn test_quote_amount_cap_dust_below_one_unit() {
        // remaining < level_price ⇒ 0
        let stop = StopCondition::QuoteAmount { remaining: 50 };
        assert_eq!(stop.level_qty_cap(100, 1), 0);
    }

    #[test]
    fn test_quote_amount_cap_lot_rounds_down() {
        // 1_400 / 100 = 14, lot=10 ⇒ 14 - (14 % 10) = 10
        let stop = StopCondition::QuoteAmount { remaining: 1_400 };
        assert_eq!(stop.level_qty_cap(100, 10), 10);
    }

    #[test]
    fn test_quote_amount_cap_zero_when_below_one_full_lot() {
        // 1_400 / 1_000 = 1, lot=10 ⇒ 1 - (1 % 10) = 0
        let stop = StopCondition::QuoteAmount { remaining: 1_400 };
        assert_eq!(stop.level_qty_cap(1_000, 10), 0);
    }

    #[test]
    fn test_quote_amount_cap_zero_price_is_zero_cap() {
        // Adversarial input: zero price should not divide-by-zero.
        let stop = StopCondition::QuoteAmount { remaining: 1_000 };
        assert_eq!(stop.level_qty_cap(0, 1), 0);
    }

    #[test]
    fn test_quote_amount_cap_saturates_to_u64_max() {
        // remaining = u128::MAX, level_price = 1 ⇒ derived qty would
        // exceed u64::MAX; must saturate at u64::MAX.
        let stop = StopCondition::QuoteAmount {
            remaining: u128::MAX,
        };
        assert_eq!(stop.level_qty_cap(1, 1), u64::MAX);
    }

    #[test]
    fn test_consume_base_qty_subtracts_executed() {
        let mut stop = StopCondition::BaseQty { remaining: 100 };
        stop.consume(30, 999);
        assert!(matches!(stop, StopCondition::BaseQty { remaining: 70 }));
    }

    #[test]
    fn test_consume_base_qty_saturates() {
        let mut stop = StopCondition::BaseQty { remaining: 5 };
        stop.consume(10, 999);
        assert!(matches!(stop, StopCondition::BaseQty { remaining: 0 }));
    }

    #[test]
    fn test_consume_quote_amount_deducts_price_times_qty() {
        let mut stop = StopCondition::QuoteAmount { remaining: 10_000 };
        stop.consume(30, 100); // spent = 100 * 30 = 3_000
        assert!(matches!(
            stop,
            StopCondition::QuoteAmount { remaining: 7_000 }
        ));
    }

    #[test]
    fn test_consume_quote_amount_saturates() {
        let mut stop = StopCondition::QuoteAmount { remaining: 100 };
        stop.consume(10, 1_000); // spent = 10_000 > 100, saturates
        assert!(matches!(stop, StopCondition::QuoteAmount { remaining: 0 }));
    }

    #[test]
    fn test_is_done_base_qty() {
        assert!(StopCondition::BaseQty { remaining: 0 }.is_done());
        assert!(!StopCondition::BaseQty { remaining: 1 }.is_done());
    }

    #[test]
    fn test_is_done_quote_amount() {
        assert!(StopCondition::QuoteAmount { remaining: 0 }.is_done());
        assert!(!StopCondition::QuoteAmount { remaining: 1 }.is_done());
    }

    #[test]
    fn test_zero_cap_is_terminal_for_base_qty_on_both_sides() {
        // The base cap is the lot-rounded residual and ignores the level
        // price, so a zero cap stays zero whichever way the walk runs.
        let stop = StopCondition::BaseQty { remaining: 5 };
        assert_eq!(stop.level_qty_cap(50, 100), 0);
        assert_eq!(stop.level_qty_cap(1, 100), 0);
        assert!(stop.zero_cap_is_terminal(Side::Buy, 100));
        assert!(stop.zero_cap_is_terminal(Side::Sell, 100));
    }

    #[test]
    fn test_zero_cap_is_terminal_for_quote_amount_on_a_buy() {
        // Buy walks asks ascending: 50 cannot fund a unit at 100 and funds
        // even less at the dearer 200 the walk would visit next.
        let stop = StopCondition::QuoteAmount { remaining: 50 };
        assert_eq!(stop.level_qty_cap(100, 1), 0);
        assert_eq!(stop.level_qty_cap(200, 1), 0);
        assert!(stop.zero_cap_is_terminal(Side::Buy, 1));
    }

    #[test]
    fn test_zero_cap_is_not_terminal_for_quote_amount_on_a_sell() {
        // Sell walks bids descending: 50 cannot fund a unit at 75 but funds
        // exactly one at the cheaper 50 the walk would visit next, so the
        // unaffordable level must be skipped rather than end the walk.
        let stop = StopCondition::QuoteAmount { remaining: 50 };
        assert_eq!(stop.level_qty_cap(75, 1), 0);
        assert_eq!(stop.level_qty_cap(50, 1), 1);
        assert!(!stop.zero_cap_is_terminal(Side::Sell, 1));
    }

    #[test]
    fn test_zero_cap_is_not_terminal_for_a_lot_rounded_quote_sell() {
        // Same rule under lot rounding: 500 caps at 500/75 = 6, rounded
        // down to a lot of 5 that is still 5 — affordable. Push the price
        // to 200 and the cap is 2, which rounds to 0; at the cheaper 100
        // it is 5 again, so the walk must go on.
        let stop = StopCondition::QuoteAmount { remaining: 500 };
        assert_eq!(stop.level_qty_cap(200, 5), 0);
        assert_eq!(stop.level_qty_cap(100, 5), 5);
        assert!(!stop.zero_cap_is_terminal(Side::Sell, 5));
        assert!(stop.zero_cap_is_terminal(Side::Buy, 5));
    }

    #[test]
    fn test_quote_sell_terminal_boundary_is_remaining_below_one_lot() {
        // The exact bound. The cheapest a level can be is a price of 1,
        // where the cap is `remaining` itself rounded down to a lot, so a
        // sell walk is over precisely when `remaining < lot`.
        let lot = 5;
        let below = StopCondition::QuoteAmount { remaining: 4 };
        assert_eq!(below.level_qty_cap(1, lot), 0, "no price can fund a lot");
        assert!(below.zero_cap_is_terminal(Side::Sell, lot));

        let exactly_one_lot = StopCondition::QuoteAmount { remaining: 5 };
        assert_eq!(
            exactly_one_lot.level_qty_cap(1, lot),
            5,
            "a price of 1 funds exactly one lot"
        );
        assert!(!exactly_one_lot.zero_cap_is_terminal(Side::Sell, lot));
    }

    #[test]
    fn test_quote_sell_without_lot_size_is_terminal_only_on_a_spent_budget() {
        // `lot <= 1` collapses the bound to `remaining == 0`, which the
        // loop's own `is_done` check reaches first, so a notional sell then
        // stops only on an exhausted budget or an exhausted side.
        let spent = StopCondition::QuoteAmount { remaining: 0 };
        assert!(spent.zero_cap_is_terminal(Side::Sell, 1));
        assert!(spent.zero_cap_is_terminal(Side::Sell, 0));

        let one = StopCondition::QuoteAmount { remaining: 1 };
        assert!(!one.zero_cap_is_terminal(Side::Sell, 1));
        assert!(!one.zero_cap_is_terminal(Side::Sell, 0));
    }
}
