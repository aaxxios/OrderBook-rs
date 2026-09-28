//! #230: `OrderBook::strandable_makers_resting`, the count that gates the
//! sweep's strandable-maker scan.
//!
//! The scan that captures makers whose hidden depth a sweep would strand has
//! to walk a level's resting orders, and `PriceLevel::iter_orders` is a
//! `DashMap` iterator that read-locks every shard of the map per level match.
//! Only a `ReserveOrder { auto_replenish: false, .. }` carrying hidden
//! quantity can ever be captured, so the book counts how many are resting
//! and the sweep skips the scan entirely while that count is zero.
//!
//! The count is exact: incremented at the two places an order is rested
//! (admission, snapshot-restore commit) and decremented at the three places
//! such a maker leaves a level (`cancel_order_with_reason`, the STP maker
//! cancel, the fill drain). These tests pin both directions, including the
//! gate closing again once the last one is gone.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::num::NonZeroU64;
    use std::sync::atomic::Ordering;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    const PRICE: u128 = 100;
    /// A GTD deadline far past any real clock reading, so the order is
    /// admitted and only the explicit eviction below expires it.
    const EXPIRY_DEADLINE_MS: u64 = 4_000_000_000_000;

    /// Read the count.
    fn count(book: &OrderBook<()>) -> usize {
        book.strandable_makers_resting.load(Ordering::Relaxed)
    }

    /// Is the sweep's scan armed?
    fn armed(book: &OrderBook<()>) -> bool {
        count(book) > 0
    }

    /// A reserve BUY at `PRICE` with the given tranches and replenishment
    /// policy.
    fn reserve_buy(
        id: Id,
        visible: u64,
        hidden: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: replenish_amount.and_then(NonZeroU64::new),
            auto_replenish,
            extra_fields: (),
        }
    }

    /// An iceberg BUY at `PRICE`.
    fn iceberg_buy(id: Id, visible: u64, hidden: u64) -> OrderType<()> {
        OrderType::IcebergOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// A fresh book has nothing to scan for.
    #[test]
    fn test_strandable_makers_resting_starts_false() {
        let book: OrderBook<()> = OrderBook::new("FLAG-NEW");
        assert!(!armed(&book), "a fresh book has rested nothing");
    }

    /// Icebergs and auto-replenishing reserves both carry hidden depth, so
    /// they make levels the scan would have to walk — but neither can ever
    /// strand anything, so neither arms the gate, not even across a sweep
    /// that consumes them.
    #[test]
    fn test_strandable_makers_resting_stays_false_for_iceberg_and_auto_reserve() {
        let book: OrderBook<()> = OrderBook::new("FLAG-SAFE");
        let iceberg_id = new_id();
        let auto_id = new_id();

        assert!(
            book.add_order(iceberg_buy(iceberg_id, 10, 20)).is_ok(),
            "iceberg must rest"
        );
        assert!(
            book.add_order(reserve_buy(auto_id, 10, 20, Some(10), true))
                .is_ok(),
            "auto-replenishing reserve must rest"
        );
        assert!(!armed(&book), "neither kind can strand hidden depth");

        // A sweep across both leaves the gate closed as well.
        assert!(
            book.add_limit_order(new_id(), PRICE, 15, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the crossing sell must be accepted"
        );
        assert!(!armed(&book), "matching does not arm the gate by itself");
    }

    /// Resting a non-auto reserve with hidden depth arms the gate.
    #[test]
    fn test_strandable_makers_resting_set_by_resting_non_auto_reserve() {
        let book: OrderBook<()> = OrderBook::new("FLAG-ARM");
        assert!(
            book.add_order(reserve_buy(new_id(), 10, 20, None, false))
                .is_ok(),
            "the reserve must rest"
        );
        assert!(armed(&book), "a strandable maker is now on the book");
    }

    /// Without hidden depth there is nothing to strand, so the same
    /// non-auto reserve leaves the gate closed.
    #[test]
    fn test_strandable_makers_resting_stays_false_without_hidden_depth() {
        let book: OrderBook<()> = OrderBook::new("FLAG-NO-HIDDEN");
        assert!(
            book.add_order(reserve_buy(new_id(), 10, 0, None, false))
                .is_ok(),
            "the reserve must rest"
        );
        assert!(!armed(&book), "no hidden tranche, nothing to strand");
    }

    /// The residual-resting path arms the gate too: a partially filled
    /// non-auto reserve that keeps a positive visible tranche rests with its
    /// hidden depth intact and can strand it later.
    #[test]
    fn test_strandable_makers_resting_set_by_rested_residual() {
        let book: OrderBook<()> = OrderBook::new("FLAG-RESIDUAL");
        assert!(
            book.add_limit_order(new_id(), PRICE, 5, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "contra depth must rest"
        );
        let taker_id = new_id();
        assert!(
            book.add_order(reserve_buy(taker_id, 10, 20, None, false))
                .is_ok(),
            "the aggressive reserve must rest its residual"
        );
        assert!(
            book.get_order(taker_id).is_some(),
            "5 of the visible tranche was taken, so the residual rests"
        );
        assert!(armed(&book), "the rested residual is strandable");
    }

    /// The flag is not part of the snapshot format; a restore re-derives it
    /// from the orders it installs, so a package carrying a strandable maker
    /// arms the gate on the restored book.
    #[test]
    fn test_strandable_makers_resting_rederived_by_snapshot_package_restore() {
        let source: OrderBook<()> = OrderBook::new("FLAG-RESTORE");
        let order_id = new_id();
        assert!(
            source
                .add_order(reserve_buy(order_id, 10, 20, None, false))
                .is_ok(),
            "the reserve must rest on the source book"
        );
        assert!(armed(&source), "the source book is armed");
        let package = match source.create_snapshot_package(usize::MAX) {
            Ok(package) => package,
            Err(error) => panic!("snapshot package must build: {error}"),
        };

        let mut restored: OrderBook<()> = OrderBook::new("FLAG-RESTORE");
        assert!(!armed(&restored), "the destination starts closed");
        assert!(
            restored.restore_from_snapshot_package(package).is_ok(),
            "the package must restore"
        );

        assert!(
            restored.get_order(order_id).is_some(),
            "the strandable maker is on the restored book"
        );
        assert!(
            armed(&restored),
            "restore must re-derive the gate from the installed orders"
        );
    }

    /// A package with no strandable maker leaves the gate closed, so the
    /// re-derivation is not a blanket `true` on every restore.
    #[test]
    fn test_strandable_makers_resting_stays_false_restoring_a_safe_package() {
        let source: OrderBook<()> = OrderBook::new("FLAG-RESTORE-SAFE");
        assert!(
            source.add_order(iceberg_buy(new_id(), 10, 20)).is_ok(),
            "the iceberg must rest on the source book"
        );
        let package = match source.create_snapshot_package(usize::MAX) {
            Ok(package) => package,
            Err(error) => panic!("snapshot package must build: {error}"),
        };

        let mut restored: OrderBook<()> = OrderBook::new("FLAG-RESTORE-SAFE");
        assert!(
            restored.restore_from_snapshot_package(package).is_ok(),
            "the package must restore"
        );
        assert!(!armed(&restored), "nothing installed can strand anything");
    }

    /// The count falls back to zero when the last strandable maker is
    /// **cancelled**, closing the gate again.
    #[test]
    fn test_strandable_makers_resting_returns_to_zero_on_cancel() {
        let book: OrderBook<()> = OrderBook::new("COUNT-CANCEL");
        let first = new_id();
        let second = new_id();
        assert!(
            book.add_order(reserve_buy(first, 10, 20, None, false))
                .is_ok()
        );
        assert!(
            book.add_order(reserve_buy(second, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&book), 2, "both strandable makers are counted");

        assert!(book.cancel_order(first).is_ok(), "first cancel");
        assert_eq!(count(&book), 1, "one decrement per removal");
        assert!(book.cancel_order(second).is_ok(), "second cancel");
        assert_eq!(count(&book), 0, "the gate closes again");
        assert!(!armed(&book), "no scan is armed once none rest");
    }

    /// Mass cancel funnels through the same helper, so the count follows.
    #[test]
    fn test_strandable_makers_resting_returns_to_zero_on_mass_cancel() {
        let book: OrderBook<()> = OrderBook::new("COUNT-MASS");
        assert!(
            book.add_order(reserve_buy(new_id(), 10, 20, None, false))
                .is_ok()
        );
        assert!(
            book.add_order(reserve_buy(new_id(), 10, 20, None, false))
                .is_ok()
        );
        // An iceberg is not counted and must not disturb the tally.
        assert!(book.add_order(iceberg_buy(new_id(), 10, 20)).is_ok());
        assert_eq!(count(&book), 2, "only the reserves are counted");

        let cancelled = book.cancel_all_orders();
        assert_eq!(cancelled.cancelled_count(), 3, "every maker is cancelled");
        assert_eq!(
            count(&book),
            0,
            "mass cancel decrements through the same funnel"
        );
    }

    /// Consuming the last strandable maker through a sweep decrements it in
    /// the fill drain, which is the third removal path.
    #[test]
    fn test_strandable_makers_resting_returns_to_zero_when_consumed() {
        let book: OrderBook<()> = OrderBook::new("COUNT-CONSUMED");
        let maker_id = new_id();
        assert!(
            book.add_order(reserve_buy(maker_id, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&book), 1, "the maker is counted");

        assert!(
            book.add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the sweep takes the whole visible tranche"
        );

        assert!(book.get_order(maker_id).is_none(), "the maker is removed");
        assert_eq!(count(&book), 0, "the fill drain decrements the count");
        assert!(
            !armed(&book),
            "the gate closes after the last one is consumed"
        );
    }

    /// Cancellation versus matching of the same maker: whichever removes
    /// it, the count falls to zero exactly once and never twice.
    #[test]
    fn test_strandable_makers_resting_decrements_exactly_once_per_maker() {
        // Removed by cancel: the later sweep must not decrement again.
        let cancelled: OrderBook<()> = OrderBook::new("COUNT-ONCE-CANCEL");
        let maker = new_id();
        assert!(
            cancelled
                .add_order(reserve_buy(maker, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&cancelled), 1);
        assert!(cancelled.cancel_order(maker).is_ok(), "cancel removes it");
        assert_eq!(count(&cancelled), 0, "one decrement");
        assert!(
            cancelled
                .add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "a later sweep finds nothing to consume"
        );
        assert_eq!(count(&cancelled), 0, "no second decrement");

        // Removed by matching: a later cancel must not decrement again.
        let matched: OrderBook<()> = OrderBook::new("COUNT-ONCE-MATCH");
        let consumed = new_id();
        assert!(
            matched
                .add_order(reserve_buy(consumed, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&matched), 1);
        assert!(
            matched
                .add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the sweep consumes it"
        );
        assert_eq!(count(&matched), 0, "one decrement");
        assert!(
            matches!(matched.cancel_order(consumed), Ok(None)),
            "the order is already gone"
        );
        assert_eq!(count(&matched), 0, "no second decrement");
    }

    /// An id reused by a plain order after its strandable reserve was
    /// cancelled never decrements again and is never reported: the count
    /// follows the order body, not the id.
    #[test]
    fn test_strandable_makers_resting_ignores_a_reused_id() {
        let book: OrderBook<()> = OrderBook::new("COUNT-ID-REUSE");
        let shared_id = new_id();
        assert!(
            book.add_order(reserve_buy(shared_id, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&book), 1);
        assert!(book.cancel_order(shared_id).is_ok());
        assert_eq!(count(&book), 0, "the reserve is gone");

        // The same id, now a plain order.
        assert!(
            book.add_limit_order(shared_id, PRICE, 10, Side::Buy, TimeInForce::Gtc, None)
                .is_ok(),
            "the freed id is reusable"
        );
        assert_eq!(count(&book), 0, "a Standard order is not strandable");

        assert!(
            book.add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "a sweep consumes the plain order"
        );
        assert_eq!(count(&book), 0, "consuming it decrements nothing");
    }

    /// Every scoped mass-cancel entry point and TIF expiry eviction funnel
    /// through `cancel_order_with_reason`, so each decrements exactly.
    #[test]
    fn test_strandable_makers_resting_follows_every_mass_cancel_entry_point() {
        for label in ["by_side", "by_user", "by_price_range", "expiry"] {
            let book: OrderBook<()> = OrderBook::new("COUNT-MASS-PATHS");
            let user = pricelevel::Hash32::from([5u8; 32]);
            let mut order = reserve_buy(new_id(), 10, 20, None, false);
            if let OrderType::ReserveOrder {
                user_id,
                time_in_force,
                ..
            } = &mut order
            {
                *user_id = user;
                if label == "expiry" {
                    // Far enough ahead that admission accepts it against
                    // the book's real clock; the eviction below steps past
                    // it explicitly.
                    *time_in_force = TimeInForce::Gtd(EXPIRY_DEADLINE_MS);
                }
            }
            assert!(book.add_order(order).is_ok(), "{label}: the reserve rests");
            assert_eq!(count(&book), 1, "{label}: counted");

            match label {
                "by_side" => {
                    let removed = book.cancel_orders_by_side(Side::Buy);
                    assert_eq!(removed.cancelled_count(), 1, "{label}: one order cancelled");
                }
                "by_user" => {
                    let removed = book.cancel_orders_by_user(user);
                    assert_eq!(removed.cancelled_count(), 1, "{label}: one order cancelled");
                }
                "by_price_range" => {
                    let removed =
                        book.cancel_orders_by_price_range(Side::Buy, PRICE - 1, PRICE + 1);
                    assert_eq!(removed.cancelled_count(), 1, "{label}: one order cancelled");
                }
                _ => {
                    book.evict_expired_orders(TimestampMs::new(EXPIRY_DEADLINE_MS + 1));
                }
            }

            assert_eq!(count(&book), 0, "{label}: the count returns to zero");
        }
    }

    /// A cancel-then-add modify removes the maker and re-adds it, so the
    /// count is decremented and incremented and lands back at one.
    #[test]
    fn test_strandable_makers_resting_survives_a_cancel_then_add_modify() {
        let book: OrderBook<()> = OrderBook::new("COUNT-MODIFY");
        let maker = new_id();
        assert!(
            book.add_order(reserve_buy(maker, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&book), 1);

        let repriced = book.update_order(pricelevel::OrderUpdate::UpdatePrice {
            order_id: maker,
            new_price: Price::new(PRICE - 1),
        });
        assert!(repriced.is_ok(), "the re-price succeeds: {repriced:?}");

        assert_eq!(
            count(&book),
            1,
            "the cancel decremented and the re-add incremented"
        );
        match book.get_order(maker) {
            Some(order) => assert_eq!(order.price().as_u128(), PRICE - 1, "re-priced"),
            None => panic!("the re-priced maker must rest"),
        }
    }

    /// #230 / restore: a legacy package carrying the one shape `pricelevel`
    /// cannot execute — a non-auto reserve with no visible tranche — is
    /// rejected in the prepare phase, before any book state is touched, so
    /// the live book survives the failed restore untouched.
    /// Build a source book holding the ghost shape, the way a pre-#230 book
    /// could come to hold one: empty an admitted reserve's visible tranche
    /// through the level itself.
    fn book_holding_a_ghost(symbol: &str, ghost_id: Id) -> OrderBook<()> {
        let source: OrderBook<()> = OrderBook::new(symbol);
        assert!(
            source
                .add_order(reserve_buy(ghost_id, 10, 20, None, false))
                .is_ok(),
            "the reserve rests with a visible tranche"
        );
        {
            let level = source.bids.get(&PRICE).expect("the bid level exists");
            let emptied = level
                .value()
                .update_order(pricelevel::OrderUpdate::UpdateQuantity {
                    order_id: ghost_id,
                    new_quantity: Quantity::new(0),
                });
            assert!(
                emptied.is_ok(),
                "the level accepts the zero update: {emptied:?}"
            );
        }
        source
    }

    /// A destination book with state and configuration that a rejected
    /// restore must leave untouched.
    fn destination_with_state(symbol: &str, survivor: Id) -> OrderBook<()> {
        let mut destination: OrderBook<()> = OrderBook::new(symbol);
        destination.set_lot_size(5);
        destination.set_min_order_size(5);
        assert!(
            destination
                .add_limit_order(survivor, PRICE, 5, Side::Buy, TimeInForce::Gtc, None)
                .is_ok(),
            "the destination has state to protect"
        );
        destination
    }

    /// Assert a rejected restore left the destination exactly as it was.
    fn assert_destination_intact(book: &OrderBook<()>, survivor: Id, ghost_id: Id) {
        assert!(
            book.get_order(survivor).is_some(),
            "a rejected restore must leave the live book untouched"
        );
        assert!(
            book.get_order(ghost_id).is_none(),
            "nothing from the rejected package may land"
        );
        assert_eq!(
            book.best_bid(),
            Some(PRICE),
            "the destination's level survives"
        );
        assert_eq!(book.lot_size(), Some(5), "configuration survives");
        assert_eq!(book.min_order_size(), Some(5), "configuration survives");
    }

    /// The **direct** `restore_from_snapshot(&self)` path rejects the ghost
    /// in its prepare phase, before any state change.
    #[test]
    fn test_direct_restore_rejects_a_zero_visible_non_auto_reserve() {
        let ghost_id = new_id();
        let source = book_holding_a_ghost("GHOST-DIRECT", ghost_id);
        let snapshot = source.create_snapshot(usize::MAX);

        let survivor = new_id();
        let destination = destination_with_state("GHOST-DIRECT", survivor);

        match destination.restore_from_snapshot(snapshot) {
            Err(crate::orderbook::error::OrderBookError::ZeroVisibleTranche {
                order_id,
                hidden_quantity,
            }) => {
                assert_eq!(order_id, ghost_id);
                assert_eq!(hidden_quantity, 20);
            }
            other => panic!("expected ZeroVisibleTranche from the direct restore, got {other:?}"),
        }
        assert_destination_intact(&destination, survivor, ghost_id);
    }

    /// The **JSON** path rejects it too, through the same prepare phase.
    #[test]
    fn test_json_restore_rejects_a_zero_visible_non_auto_reserve() {
        let ghost_id = new_id();
        let source = book_holding_a_ghost("GHOST-JSON", ghost_id);
        let json = match source.snapshot_to_json(usize::MAX) {
            Ok(json) => json,
            Err(error) => panic!("snapshot json must build: {error}"),
        };

        let survivor = new_id();
        let mut destination = destination_with_state("GHOST-JSON", survivor);

        match destination.restore_from_snapshot_json(&json) {
            Err(crate::orderbook::error::OrderBookError::ZeroVisibleTranche {
                order_id,
                hidden_quantity,
            }) => {
                assert_eq!(order_id, ghost_id);
                assert_eq!(hidden_quantity, 20);
            }
            other => panic!("expected ZeroVisibleTranche from the json restore, got {other:?}"),
        }
        assert_destination_intact(&destination, survivor, ghost_id);
    }

    #[test]
    fn test_restore_rejects_a_package_holding_a_zero_visible_non_auto_reserve() {
        // Build the ghost on a source book by emptying the visible tranche
        // of an already-admitted reserve through the level itself, which is
        // how a pre-#230 book could come to hold one.
        let source: OrderBook<()> = OrderBook::new("GHOST-RESTORE");
        let ghost_id = new_id();
        assert!(
            source
                .add_order(reserve_buy(ghost_id, 10, 20, None, false))
                .is_ok(),
            "the reserve rests with a visible tranche"
        );
        let level = source.bids.get(&PRICE).expect("the bid level exists");
        let emptied = level
            .value()
            .update_order(pricelevel::OrderUpdate::UpdateQuantity {
                order_id: ghost_id,
                new_quantity: Quantity::new(0),
            });
        assert!(
            emptied.is_ok(),
            "the level accepts the zero update: {emptied:?}"
        );
        let package = match source.create_snapshot_package(usize::MAX) {
            Ok(package) => package,
            Err(error) => panic!("snapshot package must build: {error}"),
        };

        // The destination holds a healthy order that must survive.
        let mut destination: OrderBook<()> = OrderBook::new("GHOST-RESTORE");
        let survivor = new_id();
        assert!(
            destination
                .add_limit_order(survivor, PRICE, 5, Side::Buy, TimeInForce::Gtc, None)
                .is_ok(),
            "the destination has state to protect"
        );

        match destination.restore_from_snapshot_package(package) {
            Err(crate::orderbook::error::OrderBookError::ZeroVisibleTranche {
                order_id,
                hidden_quantity,
            }) => {
                assert_eq!(order_id, ghost_id, "the offending order is named");
                assert_eq!(hidden_quantity, 20, "the stranded tranche is reported");
            }
            other => panic!("expected ZeroVisibleTranche from the restore, got {other:?}"),
        }

        assert!(
            destination.get_order(survivor).is_some(),
            "a rejected restore must leave the live book untouched"
        );
        assert!(
            destination.get_order(ghost_id).is_none(),
            "nothing from the rejected package may land"
        );
        assert_eq!(
            destination.best_bid(),
            Some(PRICE),
            "the destination's own level survives"
        );
    }

    /// The two self-trade-prevention pre-match arms capture too (#230,
    /// coverage): under `CancelTaker`, a same-user taker sweeping a level
    /// that holds a strandable maker fills the non-self depth ahead of it,
    /// is cancelled at the same-user maker, and leaves that maker intact —
    /// so the count does not move and no discard is reported.
    #[test]
    fn test_stp_cancel_taker_arm_captures_without_removing_the_maker() {
        use crate::orderbook::stp::STPMode;

        let user = pricelevel::Hash32::from([7u8; 32]);
        let book: OrderBook<()> = OrderBook::with_stp_mode("STP-CAPTURE", STPMode::CancelTaker);

        // A strandable maker owned by the same user as the incoming taker,
        // resting behind a foreign maker at the same price.
        let foreign = new_id();
        assert!(
            book.add_limit_order_with_user(
                foreign,
                PRICE,
                3,
                Side::Buy,
                TimeInForce::Gtc,
                pricelevel::Hash32::from([9u8; 32]),
                None,
            )
            .is_ok(),
            "the foreign maker rests first"
        );
        let mut strandable = reserve_buy(new_id(), 10, 20, None, false);
        let strandable_id = strandable.id();
        if let OrderType::ReserveOrder { user_id, .. } = &mut strandable {
            *user_id = user;
        }
        assert!(book.add_order(strandable).is_ok(), "the reserve rests");
        assert_eq!(count(&book), 1, "the strandable maker is counted");

        // The same user sells into the level: it may take the foreign depth,
        // then STP cancels it at its own maker.
        let taker = book.add_limit_order_with_user(
            new_id(),
            PRICE,
            8,
            Side::Sell,
            TimeInForce::Gtc,
            user,
            None,
        );
        assert!(
            taker.is_err(),
            "CancelTaker must cancel the self-crossing taker: {taker:?}"
        );

        match book.get_order(strandable_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (10, 20),
                "the same-user maker is untouched by its own taker"
            ),
            None => panic!("the strandable maker must survive the STP cancel"),
        }
        assert_eq!(
            count(&book),
            1,
            "nothing was discarded, so the count does not move"
        );
    }

    /// The gate only controls *reporting*, never matching: a book whose
    /// flag was never armed still discards a non-auto reserve maker's
    /// hidden tranche exactly as before. (It cannot actually happen — the
    /// maker could not be resting without arming the gate — so this pins
    /// that the two concerns stay separate.)
    #[test]
    fn test_strandable_scan_gate_does_not_change_matching_outcome() {
        let book: OrderBook<()> = OrderBook::new("FLAG-SEMANTICS");
        let maker_id = new_id();
        assert!(
            book.add_order(reserve_buy(maker_id, 10, 20, None, false))
                .is_ok(),
            "the maker must rest"
        );
        // Close the gate behind the maker's back, then sweep it.
        book.strandable_makers_resting.store(0, Ordering::Relaxed);
        assert!(
            book.add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the crossing sell must be accepted"
        );

        assert!(
            book.get_order(maker_id).is_none(),
            "the depleted non-replenishing maker leaves the book either way"
        );
        assert!(
            book.best_bid().is_none(),
            "the emptied level is removed either way"
        );
    }

    /// `UpdateQuantity { new_quantity: 0 }` is a removal, not a resize
    /// (#223), and it funnels through the same `cancel_order_with_reason`
    /// a user cancel does — so it decrements the count on the counted path,
    /// once per maker and never twice. Two strandable makers make the
    /// "once" observable: a double decrement on the first would close the
    /// gate while the second is still resting.
    #[test]
    fn test_strandable_makers_resting_returns_to_zero_on_a_zero_quantity_update() {
        let book: OrderBook<()> = OrderBook::new("COUNT-ZERO-UPDATE");
        let first = new_id();
        let second = new_id();
        assert!(
            book.add_order(reserve_buy(first, 10, 20, None, false))
                .is_ok()
        );
        assert!(
            book.add_order(reserve_buy(second, 10, 20, None, false))
                .is_ok()
        );
        assert_eq!(count(&book), 2, "both strandable makers are counted");

        let zero_update = |order_id: Id| {
            book.update_order(pricelevel::OrderUpdate::UpdateQuantity {
                order_id,
                new_quantity: Quantity::new(0),
            })
        };

        assert!(
            matches!(zero_update(first), Ok(Some(_))),
            "the zero update removes the first maker"
        );
        assert_eq!(count(&book), 1, "exactly one decrement");
        assert!(
            armed(&book),
            "the second maker still rests, so the scan stays armed"
        );

        assert!(
            matches!(zero_update(second), Ok(Some(_))),
            "the zero update removes the second maker"
        );
        assert_eq!(count(&book), 0, "the gate closes again");
        assert!(!armed(&book), "no scan is armed once none rest");

        // Nothing is left to consume, so a later sweep cannot decrement a
        // second time for makers this path already removed.
        assert!(
            book.add_limit_order(new_id(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "a later sweep finds nothing to consume"
        );
        assert_eq!(count(&book), 0, "no second decrement");
    }
}
