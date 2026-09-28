//! Self-Trade Prevention (STP) types and logic.
//!
//! Self-Trade Prevention prevents orders from the same user from matching
//! against each other in the order book. This is a critical exchange feature
//! that prevents wash trading.
//!
//! # Modes
//!
//! - `STPMode::None` — No STP checks (default, zero overhead).
//! - `STPMode::CancelTaker` — Cancel the incoming (taker) order on self-trade.
//! - `STPMode::CancelMaker` — Cancel the resting (maker) order and continue matching.
//! - `STPMode::CancelBoth` — Cancel both taker and maker orders.
//!
//! # Reachability
//!
//! `CancelTaker` and `CancelBoth` fire only when the sweep can still
//! execute into the same-user maker after the non-self depth queued ahead
//! of it; `CancelMaker` cancels every same-user order at a level the sweep
//! touches. See [`STPMode`](crate::orderbook::stp::STPMode) for the full
//! rule and its known asymmetry.
//!
//! # Bypass
//!
//! Orders with `user_id == Hash32::zero()` (anonymous) always bypass STP checks,
//! regardless of the configured mode.

use pricelevel::{Hash32, Id};
use serde::{Deserialize, Serialize};

/// Self-Trade Prevention mode for the order book.
///
/// Controls what happens when an incoming order would match against a resting
/// order from the same user (identified by [`Hash32`] user ID).
///
/// The default mode is [`STPMode::None`], which disables all STP checks and
/// incurs zero overhead in the matching hot path.
///
/// # Reachability
///
/// Introduced in #222. A same-user maker resting at a crossed level is not
/// by itself a self-trade. Under [`CancelTaker`](Self::CancelTaker) and
/// [`CancelBoth`](Self::CancelBoth) the engine first executes the taker
/// against the non-self depth queued ahead of that maker, and only cancels
/// if the taker could still execute at that price afterwards. So a taker the
/// depth in front already satisfies fills normally, and under `CancelBoth`
/// the maker it never reached keeps resting.
///
/// [`CancelMaker`](Self::CancelMaker) is deliberately **not** gated this
/// way: it cancels every same-user order at a level the sweep touches,
/// whether or not the taker could have executed into it. Cancelling the
/// maker is that mode's whole purpose and it never destroys the taker, so
/// the gate would only change which resting orders survive.
///
/// ## A known asymmetry in what counts as reachable
///
/// A residual too small to execute is treated differently depending on
/// where the walk is standing when it appears, and the two cases are worth
/// stating because they look alike from outside:
///
/// - A sub-lot residual left over **at the conflicting level** keeps the
///   self-trade verdict and cancels the taker. It is the taker's own
///   unfilled quantity sitting at a level that holds its own maker, and a
///   maker admitted before a [`lot_size`](crate::OrderBook::set_lot_size)
///   change keeps resting with a misaligned tranche, so that residual can
///   still be reachable depth.
/// - The identical residual arising **one level before** a deeper level
///   holding the same user's maker rests crossed against that maker
///   instead. The matching loop's zero-cap check runs at the top of each
///   level, before the self-trade scan, so the walk stops without ever
///   looking at the deeper level.
///
/// The modify precheck mirrors the loop, so a reprice and a direct submit
/// of the same order reach the same verdict in both cases. The asymmetry is
/// in the engine's definition of reachable, not between the two paths.
///
/// # Concurrency (#225)
///
/// The engine decides the STP action for a price level by snapshotting its
/// queue, and then acts on that decision in a second step. To keep the two
/// steps consistent, every STP-relevant submit and every matching-capable
/// modify (`UpdatePrice`, `UpdatePriceAndQuantity`, `Replace`) takes the
/// **exclusive** side of the book's submit gate, so no concurrent
/// admission, cancel or modify can land between the scan and the fill.
///
/// This serializes the book. Because an order carrying
/// `user_id == Hash32::zero()` is rejected with `MissingUserId` while STP
/// is enabled, every admissible `add_order` is identified, and every one of
/// them that can take liquidity runs one at a time. Post-only submits are
/// the exception on the submit path — they resolve before the STP scan is
/// reached and never take liquidity, so they keep the shared side — along
/// with `UpdateQuantity`, cancels and mass cancels.
///
/// One exclusive case is **not** about STP and applies in every
/// [`STPMode`], including [`None`](Self::None): while a book **holds** a
/// `ReserveOrder { auto_replenish: false, .. }` carrying hidden quantity,
/// every sweep on it runs exclusively (#230) — every matching-capable
/// submit, every cancel-then-add re-price and every match-only entry point,
/// plus the admission of the first such reserve. A sweep decides once
/// whether to capture makers whose hidden depth it would strand, so nothing
/// may cancel, admit or replace an order inside its capture window: the
/// sweep could otherwise consume a maker it never captured, or report a
/// captured maker's hidden quantity after a cancel freed its id for an
/// unrelated order. Cancels and mass cancels keep the shared side and never
/// read the count; they are excluded by the sweep's hold, not by their own.
/// Such books serialize their sweeps; books holding no such reserve are
/// unchanged.
///
/// Anonymous takers (`user_id == Hash32::zero()`) also stay on the shared
/// path, because STP is skipped for them — but on an STP-enabled book that
/// is reachable **only** through the match-only entry points
/// (`OrderBook::match_order_with_user`,
/// `OrderBook::match_market_order_with_user`,
/// `OrderBook::match_market_order_by_amount_with_user`), never through
/// `add_order`. Mixing anonymous flow into an STP book does not restore
/// concurrency for the identified flow: an anonymous sweep still waits for
/// any identified submit in progress, and which waiter proceeds first when
/// the gate is released is platform-dependent.
///
/// The unit of exclusion is one call, not one batch. The repricing sweeps
/// (`RepricingOperations::reprice_pegged_orders`,
/// `reprice_trailing_stops`, `reprice_special_orders`, `special_orders`
/// feature) drive the public `OrderBook::update_order` once per order, so
/// under STP they take and release the exclusive gate N times. That is
/// correct and deadlock-free — each re-price is individually atomic
/// against concurrent flow — but the sweep as a whole is not: other
/// submits interleave between consecutive re-prices, and a peg repriced
/// early in the sweep can be filled before a later one is even evaluated.
///
/// The guarantee covers every mutation, because the public API hands out no
/// level handles: `OrderBook::get_bids` / `get_asks`, which cloned the live
/// `Arc<PriceLevel>`s and let a caller mutate a level behind the gate, were
/// removed in 0.13.0 (#228). Every level mutation goes through `OrderBook`.
///
/// [`STPMode::None`] books are unaffected: with no STP scan there is no
/// window to protect, and their submits keep the shared, fully concurrent
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum STPMode {
    /// No self-trade prevention (default). Orders from the same user can
    /// match freely. This mode adds zero overhead to the matching engine.
    #[default]
    None = 0,

    /// Cancel the incoming (taker) order when a self-trade would occur.
    /// Resting orders remain in the book. Partial fills against different
    /// users that precede the self-trade are kept.
    ///
    /// "Would occur" means the sweep can still execute into the same-user
    /// maker after consuming the non-self depth queued ahead of it at that
    /// level. A taker that the depth in front already satisfies never
    /// reaches its own maker, so it fills normally and no cancellation is
    /// reported; so does a quote-notional taker whose remaining budget
    /// cannot fund another lot at that level's price. See the
    /// [reachability](Self#reachability) note.
    CancelTaker = 1,

    /// Cancel the resting (maker) order(s) from the same user and continue
    /// matching the taker against remaining orders. All same-user resting
    /// orders at each price level are removed before matching proceeds.
    ///
    /// This mode is **not** reachability-gated: every same-user order at a
    /// level the sweep touches is cancelled, including one resting behind
    /// more non-self depth than the taker can consume. The gate applies to
    /// [`CancelTaker`](Self::CancelTaker) and
    /// [`CancelBoth`](Self::CancelBoth) only. See the
    /// [reachability](Self#reachability) note.
    CancelMaker = 2,

    /// Cancel both the incoming (taker) and the resting (maker) order.
    /// Matching stops immediately. Partial fills against different users
    /// that precede the self-trade are kept.
    ///
    /// Gated on reachability exactly as [`CancelTaker`](Self::CancelTaker)
    /// is, and here the gate also protects the maker: one the sweep could
    /// not have executed into survives untouched rather than being
    /// cancelled. See the [reachability](Self#reachability) note.
    CancelBoth = 3,
}

impl std::fmt::Display for STPMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            STPMode::None => write!(f, "None"),
            STPMode::CancelTaker => write!(f, "CancelTaker"),
            STPMode::CancelMaker => write!(f, "CancelMaker"),
            STPMode::CancelBoth => write!(f, "CancelBoth"),
        }
    }
}

impl STPMode {
    /// Returns `true` if STP checks are enabled (any mode other than `None`).
    #[must_use]
    #[inline]
    pub fn is_enabled(self) -> bool {
        self != STPMode::None
    }
}

/// Result of an STP check against a single price level.
///
/// Used internally by the matching engine to decide how to proceed
/// after scanning orders at a price level for self-trade conflicts.
#[derive(Debug, Clone)]
pub(crate) enum STPAction {
    /// No self-trade detected at this level; proceed normally.
    NoConflict,

    /// CancelTaker triggered: match up to `safe_quantity` (quantity of
    /// non-same-user orders preceding the first same-user order), then stop.
    CancelTaker {
        /// Maximum quantity that can be safely matched before hitting
        /// a same-user order. Zero means the first order is same-user.
        safe_quantity: u64,
    },

    /// CancelMaker triggered: at least one same-user resting order exists at
    /// this level and must be cancelled before matching proceeds. The caller
    /// re-scans the snapshot in insertion-sequence order and cancels each
    /// same-user maker, so no per-level `Vec<Id>` is allocated here (#107).
    CancelMaker,

    /// CancelBoth triggered: match up to `safe_quantity`, then cancel
    /// the maker and stop.
    CancelBoth {
        /// Maximum quantity that can be safely matched before hitting
        /// a same-user order.
        safe_quantity: u64,
        /// The first same-user maker order ID to cancel.
        maker_order_id: Id,
    },
}

/// Scans orders at a price level and determines the STP action.
///
/// # Arguments
/// * `orders` — Resting orders at the price level, in FIFO (time-priority) order.
/// * `taker_user_id` — The user ID of the incoming (taker) order.
/// * `mode` — The active STP mode.
///
/// # Returns
/// The appropriate [`STPAction`] for the matching engine to take.
#[inline]
pub(crate) fn check_stp_at_level(
    orders: &[std::sync::Arc<pricelevel::OrderType<()>>],
    taker_user_id: Hash32,
    mode: STPMode,
) -> STPAction {
    // Fast path: no STP or anonymous taker
    if mode == STPMode::None || taker_user_id == Hash32::zero() {
        return STPAction::NoConflict;
    }

    match mode {
        STPMode::None => STPAction::NoConflict,

        STPMode::CancelTaker => {
            // Find the first same-user order and sum quantity before it
            let mut safe_quantity: u64 = 0;
            for order in orders {
                if order.user_id() == taker_user_id {
                    return STPAction::CancelTaker { safe_quantity };
                }
                // Sum visible quantity of non-same-user orders
                safe_quantity = safe_quantity.saturating_add(order.visible_quantity().as_u64());
            }
            STPAction::NoConflict
        }

        STPMode::CancelMaker => {
            // Signal a conflict if any resting order belongs to the taker; the
            // caller cancels the same-user makers by re-scanning the snapshot in
            // insertion-sequence order, so no `Vec<Id>` is built here (#107).
            if orders.iter().any(|o| o.user_id() == taker_user_id) {
                STPAction::CancelMaker
            } else {
                STPAction::NoConflict
            }
        }

        STPMode::CancelBoth => {
            // Find the first same-user order and sum quantity before it
            let mut safe_quantity: u64 = 0;
            for order in orders {
                if order.user_id() == taker_user_id {
                    return STPAction::CancelBoth {
                        safe_quantity,
                        maker_order_id: order.id(),
                    };
                }
                safe_quantity = safe_quantity.saturating_add(order.visible_quantity().as_u64());
            }
            STPAction::NoConflict
        }
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
    fn test_stp_mode_default_is_none() {
        assert_eq!(STPMode::default(), STPMode::None);
    }

    #[test]
    fn test_stp_mode_is_enabled() {
        assert!(!STPMode::None.is_enabled());
        assert!(STPMode::CancelTaker.is_enabled());
        assert!(STPMode::CancelMaker.is_enabled());
        assert!(STPMode::CancelBoth.is_enabled());
    }

    #[test]
    fn test_stp_mode_display() {
        assert_eq!(STPMode::None.to_string(), "None");
        assert_eq!(STPMode::CancelTaker.to_string(), "CancelTaker");
        assert_eq!(STPMode::CancelMaker.to_string(), "CancelMaker");
        assert_eq!(STPMode::CancelBoth.to_string(), "CancelBoth");
    }

    #[test]
    fn test_check_stp_none_mode_returns_no_conflict() {
        let orders = vec![];
        let action = check_stp_at_level(&orders, Hash32::zero(), STPMode::None);
        assert!(matches!(action, STPAction::NoConflict));
    }

    #[test]
    fn test_check_stp_zero_user_bypasses() {
        let user = Hash32::zero();
        let order = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(10),
            side: pricelevel::Side::Sell,
            user_id: user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![order];
        let action = check_stp_at_level(&orders, user, STPMode::CancelTaker);
        assert!(matches!(action, STPAction::NoConflict));
    }

    #[test]
    fn test_check_stp_cancel_taker_detects_same_user() {
        let user = Hash32::new([1u8; 32]);
        let order = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(10),
            side: pricelevel::Side::Sell,
            user_id: user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![order];
        let action = check_stp_at_level(&orders, user, STPMode::CancelTaker);
        match action {
            STPAction::CancelTaker { safe_quantity } => assert_eq!(safe_quantity, 0),
            _ => panic!("expected CancelTaker action"),
        }
    }

    #[test]
    fn test_check_stp_cancel_taker_safe_quantity_before_self() {
        let taker_user = Hash32::new([1u8; 32]);
        let other_user = Hash32::new([2u8; 32]);

        let other_order = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(5),
            side: pricelevel::Side::Sell,
            user_id: other_user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let same_order = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(10),
            side: pricelevel::Side::Sell,
            user_id: taker_user,
            timestamp: pricelevel::TimestampMs::new(1),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![other_order, same_order];
        let action = check_stp_at_level(&orders, taker_user, STPMode::CancelTaker);
        match action {
            STPAction::CancelTaker { safe_quantity } => assert_eq!(safe_quantity, 5),
            _ => panic!("expected CancelTaker action"),
        }
    }

    #[test]
    fn test_check_stp_cancel_maker_detects_same_user() {
        let taker_user = Hash32::new([1u8; 32]);
        let other_user = Hash32::new([2u8; 32]);

        let same1 = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(5),
            side: pricelevel::Side::Sell,
            user_id: taker_user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let other = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(3),
            side: pricelevel::Side::Sell,
            user_id: other_user,
            timestamp: pricelevel::TimestampMs::new(1),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let same2 = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(7),
            side: pricelevel::Side::Sell,
            user_id: taker_user,
            timestamp: pricelevel::TimestampMs::new(2),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![same1, other, same2];
        // CancelMaker is now a unit variant; per-id cancellation is the caller's
        // responsibility (it re-scans the snapshot), so the action just signals
        // that a same-user maker exists at this level (#107).
        let action = check_stp_at_level(&orders, taker_user, STPMode::CancelMaker);
        assert!(matches!(action, STPAction::CancelMaker));
    }

    #[test]
    fn test_check_stp_cancel_both_detects_self() {
        let user = Hash32::new([1u8; 32]);
        let other_user = Hash32::new([2u8; 32]);

        let other = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(3),
            side: pricelevel::Side::Sell,
            user_id: other_user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let same = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(10),
            side: pricelevel::Side::Sell,
            user_id: user,
            timestamp: pricelevel::TimestampMs::new(1),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![other, same.clone()];
        let action = check_stp_at_level(&orders, user, STPMode::CancelBoth);
        match action {
            STPAction::CancelBoth {
                safe_quantity,
                maker_order_id,
            } => {
                assert_eq!(safe_quantity, 3);
                assert_eq!(maker_order_id, same.id());
            }
            _ => panic!("expected CancelBoth action"),
        }
    }

    #[test]
    fn test_check_stp_no_conflict_when_different_users() {
        let taker_user = Hash32::new([1u8; 32]);
        let other_user = Hash32::new([2u8; 32]);

        let order = std::sync::Arc::new(pricelevel::OrderType::Standard {
            id: new_id(),
            price: pricelevel::Price::new(100),
            quantity: pricelevel::Quantity::new(10),
            side: pricelevel::Side::Sell,
            user_id: other_user,
            timestamp: pricelevel::TimestampMs::new(0),
            time_in_force: pricelevel::TimeInForce::Gtc,
            extra_fields: (),
        });
        let orders = vec![order];

        // All modes should return NoConflict for different users
        assert!(matches!(
            check_stp_at_level(&orders, taker_user, STPMode::CancelTaker),
            STPAction::NoConflict
        ));
        assert!(matches!(
            check_stp_at_level(&orders, taker_user, STPMode::CancelMaker),
            STPAction::NoConflict
        ));
        assert!(matches!(
            check_stp_at_level(&orders, taker_user, STPMode::CancelBoth),
            STPAction::NoConflict
        ));
    }
}
