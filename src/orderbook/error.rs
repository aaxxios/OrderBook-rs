//! Order book error types

use pricelevel::{Hash32, PriceLevelError, Side};
use std::fmt;

/// Errors that can occur within the OrderBook
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OrderBookError {
    /// Error from underlying price level operations
    PriceLevelError(PriceLevelError),

    /// Order not found in the book
    OrderNotFound(String),

    /// Invalid price level
    InvalidPriceLevel(u128),

    /// Price crossing (bid >= ask)
    PriceCrossing {
        /// Price that would cause crossing
        price: u128,
        /// Side of the order
        side: Side,
        /// Best opposite price, in price ticks; `None` when the opposite
        /// side emptied before the error was built (before 0.14.0 this
        /// case read `0`, #247).
        opposite_price: Option<u128>,
    },

    /// Insufficient liquidity for market order
    InsufficientLiquidity {
        /// The side of the market order
        side: Side,
        /// Quantity requested
        requested: u64,
        /// Quantity available
        available: u64,
    },

    /// Insufficient liquidity for a quote-notional market order. Returned by
    /// the `*_by_amount` paths when the book cannot fund a single whole lot
    /// against the requested notional. Distinct from
    /// [`OrderBookError::InsufficientLiquidity`] so callers can pattern-match
    /// on quote-vs-base semantics.
    InsufficientLiquidityNotional {
        /// The side of the market order
        side: Side,
        /// Notional (quote-asset value) requested
        requested: u128,
        /// Notional actually consumed before the walk gave up. Always `0`
        /// when this error is constructed (a non-zero `spent` returns
        /// `Ok(MatchResult)` with the partial fill instead).
        spent: u128,
    },

    /// Operation not permitted for specified order type
    InvalidOperation {
        /// Description of the error
        message: String,
    },

    /// New flow (submit / modify / replace) is rejected because the
    /// kill switch is engaged. Cancel and mass-cancel paths still
    /// operate so operators can drain the book in an orderly way.
    KillSwitchActive,

    /// Error while serializing snapshot data
    SerializationError {
        /// Underlying error message
        message: String,
    },

    /// Error while deserializing snapshot data
    DeserializationError {
        /// Underlying error message
        message: String,
    },

    /// Snapshot integrity check failed
    ChecksumMismatch {
        /// Expected checksum value
        expected: String,
        /// Actual checksum value
        actual: String,
    },

    /// Order price is not a multiple of the configured tick size
    InvalidTickSize {
        /// The order price that failed validation
        price: u128,
        /// The configured tick size
        tick_size: u128,
    },

    /// Order quantity is not a multiple of the configured lot size
    InvalidLotSize {
        /// The quantity that failed validation: the order quantity, a single
        /// tranche of a two-tranche order (iceberg / reserve), or the capped
        /// quantity a reserve's replenishment would transfer into its visible
        /// tranche. The transfer case can report a value the caller never
        /// submitted — with `replenish_amount` unset and automatic
        /// replenishment on it is `pricelevel`'s
        /// `DEFAULT_RESERVE_REPLENISH_AMOUNT` capped by the hidden tranche.
        quantity: u64,
        /// The configured lot size
        lot_size: u64,
    },

    /// Order quantity is outside the allowed min/max range
    OrderSizeOutOfRange {
        /// The order quantity that failed validation
        quantity: u64,
        /// The configured minimum order size, if any
        min: Option<u64>,
        /// The configured maximum order size, if any
        max: Option<u64>,
    },

    /// Order rejected because its `order_id` duplicates an order that is
    /// already resting on the book. Admitting it would overwrite the
    /// existing order's location and orphan it (the prior order could no
    /// longer be cancelled or modified by id), so the engine rejects the
    /// duplicate instead of silently replacing the live order. Maps to the
    /// stable wire code `RejectReason::DuplicateOrderId`.
    ///
    /// This guards against sequential reuse of a *live* order's id. It is
    /// not atomic against two concurrent submissions of the same fresh id
    /// on the lock-free admission path — serializing order ids is the
    /// ingress / sequencing layer's responsibility.
    DuplicateOrderId {
        /// The duplicate order ID that was rejected
        order_id: pricelevel::Id,
    },

    /// Order rejected because its two-tranche total (`visible + hidden`)
    /// overflows `u64` and therefore cannot be represented by the engine's
    /// quantity arithmetic (#210). On the direct `add_order` path this is
    /// raised before the risk gate (which would otherwise evaluate the
    /// saturated total), and always before any match, listener, or book
    /// mutation. Maps to the stable wire code
    /// `RejectReason::InvalidQuantity`.
    QuantityOverflow {
        /// Visible-tranche quantity of the rejected order.
        visible: u64,
        /// Hidden-tranche quantity of the rejected order.
        hidden: u64,
    },

    /// Order rejected because `user_id` is `Hash32::zero()` while
    /// Self-Trade Prevention is enabled. All orders must carry a non-zero
    /// `user_id` when STP mode is active.
    MissingUserId {
        /// The order ID that was rejected
        order_id: pricelevel::Id,
    },

    /// Self-trade prevention triggered: the incoming order would have
    /// matched against a resting order from the same user.
    SelfTradePrevented {
        /// The STP mode that was active
        mode: crate::orderbook::stp::STPMode,
        /// The taker (incoming) order ID
        taker_order_id: pricelevel::Id,
        /// The user ID that triggered the STP check
        user_id: pricelevel::Hash32,
    },

    /// Per-account open-order limit breached.
    ///
    /// Returned by limit-order admission when the requesting account
    /// already has `current` resting orders and the configured ceiling
    /// is `limit`. `current >= limit` always holds when this variant
    /// is constructed. When no ceiling is configured, `limit` is
    /// `u64::MAX` and the rejection means the count is no longer
    /// representable (#243).
    RiskMaxOpenOrders {
        /// Account that breached the limit.
        account: Hash32,
        /// Account's current resting-order count at check time.
        current: u64,
        /// Configured maximum.
        limit: u64,
    },

    /// Per-account notional limit would be breached by this admission.
    ///
    /// `current + attempted > limit` always holds when this variant
    /// is constructed, where an addition that overflows `u128` counts as
    /// exceeding every limit (#243). `attempted` is computed as
    /// `submitted_quantity * submitted_price` and is `u128::MAX` when
    /// that product itself overflows. When no notional ceiling is
    /// configured, `limit` is `u128::MAX` and the rejection means the
    /// account's exposure is no longer representable.
    RiskMaxNotional {
        /// Account that breached the limit.
        account: Hash32,
        /// Account's current resting notional at check time (raw ticks).
        current: u128,
        /// Notional this submission would add (raw ticks).
        attempted: u128,
        /// Configured maximum (raw ticks).
        limit: u128,
    },

    /// A `ReserveOrder` with `auto_replenish == false` was rejected because
    /// its visible tranche is zero while it carries hidden quantity (#230).
    ///
    /// That shape is the one two-tranche order `pricelevel` cannot execute.
    /// Its `match_against` returns `(0, None, 0, remaining)` for it: no
    /// trade, and the maker is removed from the level with its whole hidden
    /// tranche stranded, the first time a taker reaches it. The other
    /// zero-visible two-tranche shapes are fine and stay admissible — an
    /// `IcebergOrder` draws its entire hidden tranche into visible on match
    /// (the upstream "degenerate guard"), and an auto-replenishing
    /// `ReserveOrder` refreshes `min(amount_or_default, hidden)` and
    /// re-queues — so both execute rather than vanishing.
    ///
    /// The rule is enforced by `validate_order_shape`, so it covers
    /// `add_order` and the projected order of every quantity-carrying
    /// modify (`UpdateQuantity`, `UpdatePriceAndQuantity`, `Replace`),
    /// which since #221 set the visible tranche; and by the prepare phase of
    /// every snapshot restore, which rejects a package carrying the shape
    /// before touching book state. Single-tranche kinds are unaffected.
    /// Maps to the stable wire code `RejectReason::InvalidQuantity`.
    ZeroVisibleTranche {
        /// The order that was rejected.
        order_id: pricelevel::Id,
        /// Hidden-tranche quantity behind the empty visible one.
        hidden_quantity: u64,
    },

    /// A cancel-then-add modify (`UpdatePrice`, `UpdatePriceAndQuantity`,
    /// `Replace`) was rejected because re-adding the order would exhaust its
    /// visible tranche and discard its hidden remainder (#230).
    ///
    /// Raised only for a `ReserveOrder` with `auto_replenish == false` and a
    /// non-empty hidden tranche whose projected price crosses into at least
    /// `visible_quantity` of contra depth. Because such a residual does not
    /// rest, letting the modify proceed would cancel the original and then
    /// silently destroy the re-added order. The check runs **before** the
    /// cancel, so the original keeps resting untouched — the validate-first
    /// atomic-modify contract of #98 / #168.
    ///
    /// `crossable_quantity` is the dry-run estimate produced by the same
    /// lot-size- and STP-aware feasibility walk fill-or-kill uses, capped at
    /// the order's total quantity. It is `>= visible_quantity` and
    /// `< visible_quantity + hidden_quantity` when this variant is
    /// constructed: a projected **full** fill is not rejected, because it
    /// discards nothing. Maps to the stable wire code
    /// `RejectReason::ReserveResidualWouldBeDiscarded`.
    ReserveResidualWouldBeDiscarded {
        /// The order the modify would have destroyed.
        order_id: pricelevel::Id,
        /// Projected visible tranche, in quantity units.
        visible_quantity: u64,
        /// Contra depth the re-add would cross into, in quantity units.
        crossable_quantity: u64,
        /// Projected hidden tranche of the order, in quantity units. The
        /// tranche as it would be re-added, **not** the amount that would be
        /// lost: the sweep draws from it before the residual is abandoned.
        hidden_quantity: u64,
        /// Quantity that would actually be destroyed, in quantity units:
        /// `visible_quantity + hidden_quantity - crossable_quantity`, the
        /// residual the re-add would leave unmatched and then discard.
        /// Always `> 0` and `<= hidden_quantity` when this variant is
        /// constructed.
        discarded_quantity: u64,
    },

    /// Submitted price exceeds the configured price band against the
    /// reference price.
    ///
    /// `deviation_bps > limit_bps` always holds when this variant is
    /// constructed.
    RiskPriceBand {
        /// Limit price submitted by the caller (raw ticks).
        submitted: u128,
        /// Resolved reference price at check time (raw ticks).
        reference: u128,
        /// Computed deviation in basis points. Saturates at `u32::MAX`.
        deviation_bps: u32,
        /// Configured maximum allowed deviation in basis points.
        limit_bps: u32,
    },

    /// A matching sweep stopped at a price level that reported a failure
    /// (#240): `pricelevel` 0.10 signals a mid-sweep failure such as
    /// [`PriceLevelError::CounterExhausted`] or
    /// [`PriceLevelError::CapacityExceeded`] through
    /// `MatchResult::error()` while keeping the prefix it committed, and
    /// the book's own fold of that prefix can refuse to grow as well.
    ///
    /// The sweep stops at the failed level and never walks on to a worse
    /// price. Trades committed before the failure are **real**: they left
    /// the makers' levels, so they are emitted exactly like a partial fill
    /// (trade listener, price-level listener, risk `on_fill`, maker order
    /// state and location cleanup). The taker's remainder never rests; its
    /// terminal state is
    /// `OrderStatus::Cancelled { filled_quantity: executed_quantity,
    /// reason: CancelReason::MatchAborted }`.
    ///
    /// Returned by `update_order`, it also means the modify's original was
    /// already cancelled: the aborted sweep was its re-add, which traded
    /// before the level failed, so the original cannot be restored (#247).
    /// For such a re-add the terminal state's `filled_quantity` is
    /// cumulative (the original's tracked fills plus `executed_quantity`).
    ///
    /// Carries primitive and upstream types only, so this module stays a
    /// leaf: the committed trades themselves travel through the listeners
    /// and, for a journal, through `SubmitFailure::committed`. Maps to the
    /// stable wire code `RejectReason::MatchAborted`.
    MatchAborted {
        /// The taker whose sweep was aborted.
        order_id: pricelevel::Id,
        /// Quantity the taker executed before the abort, in quantity
        /// units (the committed prefix; `0` when the first level failed).
        executed_quantity: u64,
        /// Number of trades in the committed prefix.
        trade_count: usize,
        /// The price-level failure that stopped the sweep. Boxed so the
        /// variant does not widen every `Result<_, OrderBookError>`.
        source: Box<PriceLevelError>,
    },

    /// A cancel whose price level removed the order and then reported a
    /// failure (#248): pricelevel can commit a removal and then find a
    /// broken level invariant, poisoning the level.
    ///
    /// The order **is gone**. The book completed the removal exactly like a
    /// successful cancel (price-level event, `Cancelled` order state,
    /// location, user index, risk release, special-order tracking), so its
    /// indices agree with the level; the error only reports that the level
    /// is now faulty (later mutations on it will likely be refused;
    /// reconstruct it from a snapshot). Mass cancels list such an order as
    /// cancelled and also record the fault.
    ///
    /// Carries primitive and upstream types only, so this module stays a
    /// leaf.
    OrderRemovedWithLevelFault {
        /// The order that was removed.
        order_id: pricelevel::Id,
        /// The failure the level reported after the removal. Boxed so the
        /// variant does not widen every `Result<_, OrderBookError>`.
        source: Box<PriceLevelError>,
    },

    /// A cancel-then-add modify (`UpdatePrice`, `UpdatePriceAndQuantity`,
    /// `Replace`) cancelled the original, the re-add then failed before any
    /// trade, and the original was **restored** (#247).
    ///
    /// Every admission check runs before the cancel, so this only follows a
    /// failure the book could not predict: a concurrent mutation under the
    /// shared submit gate, or a price level or allocation failing. The
    /// original rests again with the same id, price, quantity and
    /// timestamp, and its order state is restored, but at the **back** of
    /// its level's queue: its time priority is lost. The cancel and re-add
    /// level events were emitted. Maps to the stable wire code
    /// `RejectReason::ModifyRolledBack`.
    ModifyRolledBack {
        /// The order whose modify was rolled back; it rests again.
        order_id: pricelevel::Id,
        /// Why the re-add failed.
        source: Box<OrderBookError>,
    },

    /// A cancel-then-add modify cancelled the original and the re-add
    /// failed; the order **is gone** (#247).
    ///
    /// Either the re-added order traded and then failed (its trades are
    /// real, `executed_quantity > 0`, `restore_error` is `None`; the
    /// remainder did not rest), or it failed before trading and the
    /// original could not be restored either (`restore_error` says why).
    /// The book's indices hold no trace of the order and its state is
    /// terminal (`Cancelled { RestFailed }` for a failed restore, unless a
    /// live order now owns the id). A re-add sweep aborted by a failed
    /// level after trading is reported as [`Self::MatchAborted`] instead,
    /// as before. Maps to the stable wire code
    /// `RejectReason::ModifyOrderLost`.
    ModifyOrderLost {
        /// The order that was lost.
        order_id: pricelevel::Id,
        /// Quantity the **re-added** order executed before failing, in
        /// quantity units; the original's earlier fills are not included.
        /// The order's terminal state carries the cumulative figure.
        executed_quantity: u64,
        /// Why the re-add failed.
        source: Box<OrderBookError>,
        /// Why the original could not be restored; `None` when the re-add
        /// traded, which rules a restore out.
        restore_error: Option<Box<OrderBookError>>,
    },

    /// The order a `UpdatePriceAndQuantity` / `Replace` modify read had
    /// changed by the time the modify cancelled it (#247): a concurrent
    /// taker filled part of it, or a concurrent quantity update resized it,
    /// under the shared submit gate. The caller named the new quantity
    /// against a state that no longer exists, so the modify is not applied.
    /// Only ever the `source` of [`Self::ModifyRolledBack`] (the remainder
    /// is restored) or [`Self::ModifyOrderLost`] (it could not be).
    /// `UpdatePrice` does not raise it: it re-adds the cancelled remainder.
    OrderChangedDuringModify {
        /// The modified order.
        order_id: pricelevel::Id,
        /// Total quantity when the modify read the order, in quantity units.
        read_quantity: u64,
        /// Total quantity the cancel removed, in quantity units.
        cancelled_quantity: u64,
    },

    /// A taker traded and then the per-account risk layer refused to
    /// reserve its residual, so the residual did **not** rest (#291).
    ///
    /// The pre-trade risk check admits the whole order before the sweep,
    /// so this only follows a reservation the check could not predict:
    /// concurrent admissions on the same account under the shared submit
    /// gate, or a counter that cannot represent the residual. The trades
    /// are real (`executed_quantity > 0`, published to the listeners like
    /// a partial fill); the taker ends
    /// `OrderStatus::Cancelled { filled_quantity: executed_quantity,
    /// reason: CancelReason::RestFailed }`. A risk refusal before any
    /// trade keeps its own variant (`RiskMaxOpenOrders`,
    /// `RiskMaxNotional`, ...), which is pre-mutation.
    ///
    /// Distinct from the risk rejections because it mutates the book: a
    /// journal records it as may-have-mutated and replay re-executes the
    /// sweep with the residual refused, which needs no `RiskConfig`.
    /// Returned by `update_order` only as the `source` of
    /// [`Self::ModifyOrderLost`]. Maps to the stable wire code
    /// `RejectReason::RiskRejectedAfterTrades`.
    RiskRejectedAfterTrades {
        /// The taker whose residual was refused.
        order_id: pricelevel::Id,
        /// Quantity the taker executed before the refusal, in quantity
        /// units.
        executed_quantity: u64,
        /// The risk layer's refusal. Boxed so the variant does not widen
        /// every `Result<_, OrderBookError>`.
        source: Box<OrderBookError>,
    },

    /// A taker's fee could not be computed exactly under the configured
    /// `FeeSchedule` (#244).
    ///
    /// Raised before the book is touched: every taker's worst-case notional
    /// (the worst price it can reach times its quantity, or the amount of a
    /// quote-notional order) is checked against both legs of the schedule,
    /// and a notional whose `notional × |bps|` does not fit `u128` is
    /// rejected untouched instead of producing a clamped fee. Mirrors
    /// `FeeOverflow`'s fields with primitives so this module stays a leaf.
    /// Maps to the stable wire code `RejectReason::FeeOverflow`.
    FeeOverflow {
        /// The worst-case notional that could not be priced, in quote units.
        notional: u128,
        /// The signed fee rate in basis points that overflowed (maker or
        /// taker).
        bps: i32,
        /// Largest notional guaranteed exact at that rate.
        max_guaranteed_exact_notional: u128,
    },

    /// A taker's worst-case notional does not fit `u128` (#244): the worst
    /// price it can reach times its quantity overflows, so neither the
    /// trade's `quote_notional` nor its fees could be computed. Raised
    /// before the book is touched. Maps to the stable wire code
    /// `RejectReason::NotionalOverflow`.
    NotionalOverflow {
        /// The worst price the taker can reach, in price ticks.
        price: u128,
        /// The taker's quantity, in quantity units.
        quantity: u64,
    },

    /// A read-only analytics aggregate (VWAP / impact notional, cumulative
    /// depth, side volume, histogram bound, ...) does not fit its integer
    /// type: a `u128` price-times-quantity product or sum, or a `u64`
    /// quantity sum, overflowed (or a difference underflowed) with
    /// type-valid extreme inputs (#245). The analytics are pure reads, so
    /// no book state is touched; the caller gets this error instead of a
    /// panic (debug), a wrapped value (release) or a clamped one.
    ///
    /// `operation` is a static, allocation-free name of the aggregate that
    /// overflowed (for example `"vwap notional"`). Not a reject: maps to
    /// the wire code `RejectReason::Other(0)`.
    ArithmeticOverflow {
        /// Static name of the aggregate whose checked arithmetic failed.
        operation: &'static str,
    },

    /// A read-only analytics call could not reserve its result buffer
    /// (`Vec::try_reserve_exact` failed) (#245). The requested size is
    /// already capped by the call's documented maximum (for example
    /// [`MAX_DEPTH_DISTRIBUTION_BINS`](crate::orderbook::book::MAX_DEPTH_DISTRIBUTION_BINS)),
    /// so this signals allocator exhaustion, not an unbounded request. Not
    /// a reject: maps to the wire code `RejectReason::Other(0)`.
    AllocationFailed {
        /// Static name of the buffer that could not be reserved.
        operation: &'static str,
        /// Number of elements that could not be reserved.
        requested: usize,
    },

    /// The engine sequence counter cannot mint another value (#250):
    /// `engine_seq` is at `u64::MAX`, so `engine_seq + 1` is not
    /// representable. Returned by
    /// [`OrderBook::next_engine_seq`](crate::orderbook::OrderBook::next_engine_seq)
    /// instead of wrapping to `0` (which would break the strict
    /// monotonicity downstream gap detection relies on), and by the
    /// snapshot-package restore when the package carries an `engine_seq`
    /// the restored book could never advance. Not a reject: maps to the
    /// wire code `RejectReason::Other(0)`.
    EngineSeqExhausted {
        /// The counter value that cannot be advanced.
        engine_seq: u64,
    },

    /// A snapshot being restored describes a crossed or locked book (#250):
    /// its best bid is at or above its best ask. A live book can never rest
    /// such a pair (the incoming side would have matched), so the snapshot
    /// is malformed and is rejected in the restore's prepare phase, before
    /// any live state is touched. Not a reject: maps to the wire code
    /// `RejectReason::Other(0)`.
    SnapshotCrossed {
        /// Highest bid price in the snapshot, in price ticks.
        best_bid: u128,
        /// Lowest ask price in the snapshot, in price ticks.
        best_ask: u128,
    },

    /// A trailing stop reached a book that cannot hold it (#286).
    ///
    /// Trailing stops are held off book and triggered by the book's last
    /// trade price, which needs the `special_orders` feature. Without it,
    /// `add_order` rejects an `OrderType::TrailingStop` untouched with this
    /// error (before 0.14.0 it was silently rested as a limit order at its
    /// stop price), and a snapshot carrying pending stops cannot be
    /// restored. Also raised, with the feature, by a snapshot restore that
    /// finds a trailing stop resting on a price level (the pre-0.14 model,
    /// which no longer exists). Maps to the stable wire code
    /// `RejectReason::StopOrdersUnsupported` (23).
    StopOrdersUnsupported {
        /// The trailing stop that was refused.
        order_id: pricelevel::Id,
    },

    /// Failed to publish a trade event to NATS JetStream.
    #[cfg(feature = "nats")]
    NatsPublishError {
        /// Description of the publish failure
        message: String,
    },

    /// Failed to serialize a trade event for NATS publishing.
    #[cfg(feature = "nats")]
    NatsSerializationError {
        /// Description of the serialization failure
        message: String,
    },
}

impl fmt::Display for OrderBookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderBookError::PriceLevelError(err) => write!(f, "Price level error: {err}"),
            OrderBookError::OrderNotFound(id) => write!(f, "Order not found: {id}"),
            OrderBookError::InvalidPriceLevel(price) => write!(f, "Invalid price level: {price}"),
            OrderBookError::PriceCrossing {
                price,
                side,
                opposite_price,
            } => match opposite_price {
                Some(opposite_price) => write!(
                    f,
                    "Price crossing: {side} {price} would cross opposite at {opposite_price}"
                ),
                None => write!(
                    f,
                    "Price crossing: {side} {price} would cross the opposite side"
                ),
            },
            OrderBookError::InsufficientLiquidity {
                side,
                requested,
                available,
            } => {
                write!(
                    f,
                    "Insufficient liquidity for {side} order: requested {requested}, available {available}"
                )
            }
            OrderBookError::InsufficientLiquidityNotional {
                side,
                requested,
                spent,
            } => {
                write!(
                    f,
                    "Insufficient liquidity by notional for {side} order: requested {requested}, spent {spent}"
                )
            }
            OrderBookError::InvalidOperation { message } => {
                write!(f, "Invalid operation: {message}")
            }
            OrderBookError::KillSwitchActive => {
                write!(
                    f,
                    "kill switch active: new order entry and modifications are halted"
                )
            }
            OrderBookError::SerializationError { message } => {
                write!(f, "Serialization error: {message}")
            }
            OrderBookError::DeserializationError { message } => {
                write!(f, "Deserialization error: {message}")
            }
            OrderBookError::ChecksumMismatch { expected, actual } => {
                write!(
                    f,
                    "Checksum mismatch: expected {expected}, but computed {actual}"
                )
            }
            OrderBookError::InvalidTickSize { price, tick_size } => {
                write!(
                    f,
                    "invalid tick size: price {price} is not a multiple of tick size {tick_size}"
                )
            }
            OrderBookError::InvalidLotSize { quantity, lot_size } => {
                write!(
                    f,
                    "invalid lot size: quantity {quantity} is not a multiple of lot size {lot_size}"
                )
            }
            OrderBookError::OrderSizeOutOfRange { quantity, min, max } => {
                write!(
                    f,
                    "order size out of range: quantity {quantity}, min {min:?}, max {max:?}"
                )
            }
            OrderBookError::DuplicateOrderId { order_id } => {
                write!(
                    f,
                    "duplicate order id: order {order_id} is already resting on the book"
                )
            }
            OrderBookError::MissingUserId { order_id } => {
                write!(
                    f,
                    "missing user_id: order {order_id} rejected because STP is enabled and user_id is zero"
                )
            }
            OrderBookError::QuantityOverflow { visible, hidden } => {
                write!(
                    f,
                    "quantity overflow: visible {visible} + hidden {hidden} exceeds u64"
                )
            }
            OrderBookError::SelfTradePrevented {
                mode,
                taker_order_id,
                user_id,
            } => {
                write!(
                    f,
                    "self-trade prevented ({mode}): taker {taker_order_id}, user {user_id}"
                )
            }
            OrderBookError::RiskMaxOpenOrders {
                account,
                current,
                limit,
            } => {
                write!(
                    f,
                    "risk: account {account} has {current} open orders (limit {limit})"
                )
            }
            OrderBookError::RiskMaxNotional {
                account,
                current,
                attempted,
                limit,
            } => {
                write!(
                    f,
                    "risk: account {account} notional {current} + attempted {attempted} would exceed limit {limit}"
                )
            }
            OrderBookError::RiskPriceBand {
                submitted,
                reference,
                deviation_bps,
                limit_bps,
            } => {
                write!(
                    f,
                    "risk: submitted price {submitted} deviates {deviation_bps} bps from reference {reference} (limit {limit_bps} bps)"
                )
            }
            OrderBookError::ZeroVisibleTranche {
                order_id,
                hidden_quantity,
            } => {
                write!(
                    f,
                    "zero visible tranche: order {order_id} carries {hidden_quantity} hidden units behind an empty visible tranche; a two-tranche order must display a positive visible quantity"
                )
            }
            OrderBookError::ReserveResidualWouldBeDiscarded {
                order_id,
                visible_quantity,
                crossable_quantity,
                hidden_quantity,
                discarded_quantity,
            } => {
                write!(
                    f,
                    "reserve residual would be discarded: re-adding order {order_id} would cross {crossable_quantity} units, exhausting its visible tranche of {visible_quantity} and discarding {discarded_quantity} of its {hidden_quantity} hidden units because automatic replenishment is off; cancel and resubmit deliberately instead"
                )
            }
            OrderBookError::MatchAborted {
                order_id,
                executed_quantity,
                trade_count,
                source,
            } => {
                write!(
                    f,
                    "match aborted: taker {order_id} stopped by a price level failure after executing {executed_quantity} in {trade_count} trades; remainder cancelled: {source}"
                )
            }
            OrderBookError::OrderRemovedWithLevelFault { order_id, source } => {
                write!(
                    f,
                    "order {order_id} was removed but its price level then failed: {source}"
                )
            }
            OrderBookError::OrderChangedDuringModify {
                order_id,
                read_quantity,
                cancelled_quantity,
            } => {
                write!(
                    f,
                    "order {order_id} changed during the modify: read with {read_quantity} units, cancelled with {cancelled_quantity}"
                )
            }
            OrderBookError::RiskRejectedAfterTrades {
                order_id,
                executed_quantity,
                source,
            } => {
                write!(
                    f,
                    "risk rejected the residual of order {order_id} after it executed {executed_quantity}; remainder cancelled: {source}"
                )
            }
            OrderBookError::ModifyRolledBack { order_id, source } => {
                write!(
                    f,
                    "modify rolled back: re-adding order {order_id} failed before any trade and the original was restored at the back of its level: {source}"
                )
            }
            OrderBookError::ModifyOrderLost {
                order_id,
                executed_quantity,
                source,
                restore_error,
            } => match restore_error {
                Some(restore_error) => write!(
                    f,
                    "modify lost order {order_id}: the re-add failed ({source}) and the original could not be restored ({restore_error})"
                ),
                None => write!(
                    f,
                    "modify lost order {order_id}: the re-add executed {executed_quantity} and then failed; remainder cancelled: {source}"
                ),
            },
            OrderBookError::FeeOverflow {
                notional,
                bps,
                max_guaranteed_exact_notional,
            } => {
                write!(
                    f,
                    "fee overflow: worst-case notional {notional} × |{bps}| bps exceeds the u128 domain; max guaranteed-exact notional at this rate is {max_guaranteed_exact_notional}"
                )
            }
            OrderBookError::NotionalOverflow { price, quantity } => {
                write!(
                    f,
                    "notional overflow: worst-case price {price} × quantity {quantity} exceeds u128"
                )
            }
            OrderBookError::ArithmeticOverflow { operation } => {
                write!(f, "arithmetic overflow in {operation}")
            }
            OrderBookError::AllocationFailed {
                operation,
                requested,
            } => {
                write!(
                    f,
                    "allocation failed: could not reserve {requested} elements for {operation}"
                )
            }
            OrderBookError::EngineSeqExhausted { engine_seq } => {
                write!(
                    f,
                    "engine sequence exhausted: engine_seq {engine_seq} cannot be advanced without wrapping"
                )
            }
            OrderBookError::SnapshotCrossed { best_bid, best_ask } => {
                write!(
                    f,
                    "snapshot is crossed: best bid {best_bid} is at or above best ask {best_ask}"
                )
            }
            OrderBookError::StopOrdersUnsupported { order_id } => {
                write!(
                    f,
                    "trailing stop {order_id} is not supported here: pending stops are held off book and need the special_orders feature"
                )
            }
            #[cfg(feature = "nats")]
            OrderBookError::NatsPublishError { message } => {
                write!(f, "nats publish error: {message}")
            }
            #[cfg(feature = "nats")]
            OrderBookError::NatsSerializationError { message } => {
                write!(f, "nats serialization error: {message}")
            }
        }
    }
}

impl std::error::Error for OrderBookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OrderBookError::MatchAborted { source, .. }
            | OrderBookError::OrderRemovedWithLevelFault { source, .. } => Some(source.as_ref()),
            OrderBookError::ModifyRolledBack { source, .. }
            | OrderBookError::ModifyOrderLost { source, .. }
            | OrderBookError::RiskRejectedAfterTrades { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<PriceLevelError> for OrderBookError {
    fn from(err: PriceLevelError) -> Self {
        OrderBookError::PriceLevelError(err)
    }
}

impl From<crate::orderbook::serialization::SerializationError> for OrderBookError {
    /// Folds a typed [`SerializationError`](crate::orderbook::serialization::SerializationError)
    /// into [`OrderBookError::SerializationError`], preserving the underlying
    /// serde / bincode message via the error's `Display`. Enables
    /// `?`-propagation of an `EventSerializer` failure on paths returning
    /// `OrderBookError`.
    fn from(err: crate::orderbook::serialization::SerializationError) -> Self {
        OrderBookError::SerializationError {
            message: err.to_string(),
        }
    }
}

/// Errors that can occur in BookManager operations
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ManagerError {
    /// Trade processor has already been started. A processor that was
    /// since stopped counts as started: it cannot be restarted.
    ProcessorAlreadyStarted,

    /// An order book already exists for the symbol; `add_book` refuses to
    /// overwrite it (which would silently drop the existing book's resting
    /// orders).
    BookAlreadyExists {
        /// The symbol that already has a book.
        symbol: String,
    },

    /// `BookManagerTokio` was asked to start its trade processor from a
    /// thread that is not inside a Tokio runtime (#255). Nothing was
    /// consumed: the manager can start the processor later from inside a
    /// runtime, or through `start_trade_processor_on` with an explicit
    /// runtime handle.
    NoRuntime,

    /// The operating system refused to spawn the `BookManagerStd` trade
    /// processor thread (#255). Nothing was consumed: the manager can retry
    /// the start.
    ThreadSpawn {
        /// Kind of the underlying `std::io::Error`.
        kind: std::io::ErrorKind,
        /// Display text of the underlying `std::io::Error`.
        message: String,
    },

    /// `stop_trade_processor` was called while no trade processor is
    /// running: it was never started, or it was already stopped (#255).
    ProcessorNotRunning,

    /// The trade processor panicked (#255). Reported by
    /// `stop_trade_processor` when it joins the thread (`BookManagerStd`) or
    /// awaits the task (`BookManagerTokio`). The panic can only come from caller-supplied
    /// code: the handler given to `start_trade_processor_with`, or the
    /// installed `tracing` subscriber.
    ProcessorPanicked {
        /// The panic payload when it is a string, otherwise a placeholder.
        message: String,
    },

    /// The Tokio trade processor task was cancelled before it finished
    /// (#255), for example because its runtime shut down. Reported by
    /// `BookManagerTokio::stop_trade_processor`.
    ProcessorCancelled,
}

impl fmt::Display for ManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManagerError::ProcessorAlreadyStarted => {
                write!(f, "trade processor already started")
            }
            ManagerError::BookAlreadyExists { symbol } => {
                write!(f, "order book already exists for symbol: {symbol}")
            }
            ManagerError::NoRuntime => {
                write!(f, "no Tokio runtime available to start the trade processor")
            }
            ManagerError::ThreadSpawn { kind, message } => {
                write!(
                    f,
                    "failed to spawn trade processor thread ({kind:?}): {message}"
                )
            }
            ManagerError::ProcessorNotRunning => {
                write!(f, "trade processor is not running")
            }
            ManagerError::ProcessorPanicked { message } => {
                write!(f, "trade processor panicked: {message}")
            }
            ManagerError::ProcessorCancelled => {
                write!(f, "trade processor task was cancelled")
            }
        }
    }
}

impl std::error::Error for ManagerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use pricelevel::{Hash32, Id};
    use uuid::Uuid;

    #[test]
    fn test_clone_order_not_found() {
        let error = OrderBookError::OrderNotFound("order123".to_string());
        let cloned = error.clone();
        assert!(matches!(cloned, OrderBookError::OrderNotFound(ref s) if s == "order123"));
    }

    #[test]
    fn test_clone_invalid_price_level() {
        let error = OrderBookError::InvalidPriceLevel(12345);
        let cloned = error.clone();
        assert!(matches!(cloned, OrderBookError::InvalidPriceLevel(12345)));
    }

    #[test]
    fn test_clone_price_crossing() {
        let error = OrderBookError::PriceCrossing {
            price: 100,
            side: Side::Buy,
            opposite_price: Some(99),
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::PriceCrossing {
                price: 100,
                side: Side::Buy,
                opposite_price: Some(99)
            }
        ));
    }

    #[test]
    fn test_clone_insufficient_liquidity() {
        let error = OrderBookError::InsufficientLiquidity {
            side: Side::Sell,
            requested: 1000,
            available: 500,
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::InsufficientLiquidity {
                side: Side::Sell,
                requested: 1000,
                available: 500
            }
        ));
    }

    #[test]
    fn test_clone_insufficient_liquidity_notional() {
        let error = OrderBookError::InsufficientLiquidityNotional {
            side: Side::Buy,
            requested: 1_000_000,
            spent: 0,
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::InsufficientLiquidityNotional {
                side: Side::Buy,
                requested: 1_000_000,
                spent: 0
            }
        ));
    }

    #[test]
    fn test_display_insufficient_liquidity_notional() {
        let error = OrderBookError::InsufficientLiquidityNotional {
            side: Side::Sell,
            requested: 42,
            spent: 0,
        };
        let s = format!("{error}");
        assert!(s.contains("Insufficient liquidity by notional"));
        assert!(s.contains("42"));
    }

    #[test]
    fn test_clone_invalid_operation() {
        let error = OrderBookError::InvalidOperation {
            message: "Cannot cancel filled order".to_string(),
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::InvalidOperation { ref message } if message == "Cannot cancel filled order"
        ));
    }

    #[test]
    fn test_clone_serialization_error() {
        let error = OrderBookError::SerializationError {
            message: "Failed to serialize".to_string(),
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::SerializationError { ref message } if message == "Failed to serialize"
        ));
    }

    #[test]
    fn test_clone_checksum_mismatch() {
        let error = OrderBookError::ChecksumMismatch {
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::ChecksumMismatch { ref expected, ref actual }
            if expected == "abc123" && actual == "def456"
        ));
    }

    #[test]
    fn test_clone_invalid_tick_size() {
        let error = OrderBookError::InvalidTickSize {
            price: 10050,
            tick_size: 100,
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::InvalidTickSize {
                price: 10050,
                tick_size: 100
            }
        ));
    }

    #[test]
    fn test_clone_invalid_lot_size() {
        let error = OrderBookError::InvalidLotSize {
            quantity: 75,
            lot_size: 10,
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::InvalidLotSize {
                quantity: 75,
                lot_size: 10
            }
        ));
    }

    #[test]
    fn test_clone_order_size_out_of_range() {
        let error = OrderBookError::OrderSizeOutOfRange {
            quantity: 5,
            min: Some(10),
            max: Some(1000),
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::OrderSizeOutOfRange {
                quantity: 5,
                min: Some(10),
                max: Some(1000)
            }
        ));
    }

    #[test]
    fn test_clone_missing_user_id() {
        let order_id = Id::from_uuid(Uuid::new_v4());
        let error = OrderBookError::MissingUserId { order_id };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::MissingUserId { order_id: id } if id == order_id
        ));
    }

    #[test]
    fn test_clone_self_trade_prevented() {
        let taker_id = Id::from_uuid(Uuid::new_v4());
        let user_id = Hash32::from([1u8; 32]);
        let error = OrderBookError::SelfTradePrevented {
            mode: crate::orderbook::stp::STPMode::CancelMaker,
            taker_order_id: taker_id,
            user_id,
        };
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::SelfTradePrevented {
                mode: crate::orderbook::stp::STPMode::CancelMaker,
                taker_order_id: id,
                user_id: uid
            } if id == taker_id && uid == user_id
        ));
    }

    #[test]
    fn test_clone_price_level_error_parse_error() {
        let price_level_err = PriceLevelError::ParseError {
            message: "Parse failed".to_string(),
        };
        let error = OrderBookError::PriceLevelError(price_level_err);
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::PriceLevelError(PriceLevelError::ParseError { ref message })
            if message == "Parse failed"
        ));
    }

    #[test]
    fn test_clone_price_level_error_invalid_format() {
        let price_level_err = PriceLevelError::InvalidFormat;
        let error = OrderBookError::PriceLevelError(price_level_err);
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::PriceLevelError(PriceLevelError::InvalidFormat)
        ));
    }

    #[test]
    fn test_clone_price_level_error_unknown_order_type() {
        let price_level_err = PriceLevelError::UnknownOrderType("CUSTOM".to_string());
        let error = OrderBookError::PriceLevelError(price_level_err);
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::PriceLevelError(PriceLevelError::UnknownOrderType(ref s))
            if s == "CUSTOM"
        ));
    }

    #[test]
    fn test_clone_price_level_error_checksum_mismatch() {
        let price_level_err = PriceLevelError::ChecksumMismatch {
            expected: "hash1".to_string(),
            actual: "hash2".to_string(),
        };
        let error = OrderBookError::PriceLevelError(price_level_err);
        let cloned = error.clone();
        assert!(matches!(
            cloned,
            OrderBookError::PriceLevelError(PriceLevelError::ChecksumMismatch {
                ref expected,
                ref actual
            }) if expected == "hash1" && actual == "hash2"
        ));
    }

    #[test]
    fn test_manager_error_lifecycle_variants_display_issue_255() {
        let cases = [
            (
                ManagerError::NoRuntime,
                "no Tokio runtime available to start the trade processor",
            ),
            (
                ManagerError::ThreadSpawn {
                    kind: std::io::ErrorKind::WouldBlock,
                    message: "resource temporarily unavailable".to_string(),
                },
                "failed to spawn trade processor thread (WouldBlock): resource temporarily unavailable",
            ),
            (
                ManagerError::ProcessorNotRunning,
                "trade processor is not running",
            ),
            (
                ManagerError::ProcessorPanicked {
                    message: "boom".to_string(),
                },
                "trade processor panicked: boom",
            ),
            (
                ManagerError::ProcessorCancelled,
                "trade processor task was cancelled",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.clone().to_string(), expected);
        }
    }
}
