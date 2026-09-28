//! #226: per-kind lot-size validation in `validate_order_shape`.
//!
//! A reserve order used to be routed through the old `_` catch-all and was
//! only checked on its *total*, so a 15 visible / 5 hidden reserve slipped
//! into a lot-10 book while the identical iceberg was rejected. The reserve
//! now takes the iceberg's per-tranche rule plus a check on the capped
//! transfer that replenishment moves from hidden into the visible tranche —
//! the latter only while `auto_replenish` is on, the single flag that
//! decides whether anything is ever transferred (#230).

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::error::OrderBookError;
    use pricelevel::{
        DEFAULT_RESERVE_REPLENISH_AMOUNT, Hash32, Id, OrderType, Price, Quantity, Side,
        TimeInForce, TimestampMs,
    };
    use std::num::NonZeroU64;

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    const PRICE: u128 = 100;

    /// A lot-`lot` book with no other admission constraint.
    fn book_with_lot(lot: u64) -> OrderBook<()> {
        OrderBook::with_lot_size("LOT", lot)
    }

    /// A reserve order with the given tranches and replenishment policy.
    fn reserve(
        visible: u64,
        hidden: u64,
        threshold: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> OrderType<()> {
        OrderType::ReserveOrder {
            id: new_id(),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(crate::utils::current_time_millis()),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(threshold),
            replenish_amount: replenish_amount.and_then(NonZeroU64::new),
            auto_replenish,
            extra_fields: (),
        }
    }

    /// An iceberg order with the given tranches.
    fn iceberg(visible: u64, hidden: u64) -> OrderType<()> {
        OrderType::IcebergOrder {
            id: new_id(),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(crate::utils::current_time_millis()),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// Assert the validation failed with `InvalidLotSize` on `quantity`.
    fn assert_invalid_lot<V: std::fmt::Debug>(
        result: Result<V, OrderBookError>,
        quantity: u64,
        lot: u64,
    ) {
        match result {
            Err(OrderBookError::InvalidLotSize {
                quantity: reported,
                lot_size,
            }) => {
                assert_eq!(reported, quantity, "offending quantity reported");
                assert_eq!(lot_size, lot, "configured lot size reported");
            }
            other => panic!("expected InvalidLotSize {{ quantity: {quantity} }}, got {other:?}"),
        }
    }

    // --- Reserve: per-tranche rule (the issue's repro) ---

    /// The headline asymmetry: a 15/5 reserve totals 20 — a multiple of 10 —
    /// but its visible tranche is not, so it must be rejected on the tranche.
    #[test]
    fn test_validate_order_shape_reserve_misaligned_visible_rejects() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(15, 5, 0, None, false));
        assert_invalid_lot(result, 15, 10);
    }

    /// The identical iceberg keeps rejecting on the same quantity.
    #[test]
    fn test_validate_order_shape_iceberg_misaligned_visible_rejects() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&iceberg(15, 5));
        assert_invalid_lot(result, 15, 10);
    }

    /// A misaligned *hidden* tranche behind an aligned visible one is rejected
    /// on the hidden quantity. An aligned visible plus a misaligned hidden can
    /// never make an aligned total (10 + 15 == 25 here), so the old total rule
    /// would have rejected this order too; what the per-tranche rule adds is
    /// naming the offending tranche, 15, rather than the total, 25.
    #[test]
    fn test_validate_order_shape_reserve_misaligned_hidden_rejects() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 15, 0, None, false));
        assert_invalid_lot(result, 15, 10);
    }

    /// Both tranches aligned and no replenishment transfer: accepted.
    #[test]
    fn test_validate_order_shape_reserve_aligned_tranches_accepts() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 20, 0, None, false));
        assert!(result.is_ok(), "aligned reserve rejected: {result:?}");
    }

    // --- Reserve: the capped replenishment transfer ---

    /// An explicit replenish amount is dead configuration without
    /// `auto_replenish` (#230): nothing is ever transferred on either path —
    /// `pricelevel` removes a depleted resting maker and the residual helper
    /// leaves the visible tranche empty, which ends the order — so the
    /// misaligned 7 cannot reach a level and the order is accepted.
    #[test]
    fn test_validate_order_shape_reserve_misaligned_replenish_amount_without_auto_accepts() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 20, 0, Some(7), false));
        assert!(
            result.is_ok(),
            "non-replenishing reserve rejected on a dead amount: {result:?}"
        );
    }

    /// The same amount is rejected with `auto_replenish` on, where it is the
    /// transfer both `pricelevel` and the residual helper perform: a 10/20
    /// reserve replenishing 7 would otherwise rest as 7/13.
    #[test]
    fn test_validate_order_shape_reserve_misaligned_replenish_amount_auto_rejects() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 20, 0, Some(7), true));
        assert_invalid_lot(result, 7, 10);
    }

    /// A replenish amount larger than the hidden tranche is capped by it:
    /// `min(25, 20) == 20` is aligned, so the order is accepted even though
    /// 25 alone is not a multiple of 10.
    #[test]
    fn test_validate_order_shape_reserve_replenish_amount_capped_by_hidden_accepts() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 20, 0, Some(25), true));
        assert!(result.is_ok(), "capped transfer rejected: {result:?}");
    }

    /// With no hidden tranche nothing is ever transferred, so a misaligned
    /// replenish amount is irrelevant.
    #[test]
    fn test_validate_order_shape_reserve_no_hidden_ignores_replenish_amount() {
        let book = book_with_lot(10);
        let result = book.validate_order_shape(&reserve(10, 0, 0, Some(7), true));
        assert!(result.is_ok(), "hidden-less reserve rejected: {result:?}");
    }

    /// Without an explicit amount, `auto_replenish` makes `pricelevel` move
    /// its default (80) capped by hidden: `min(80, 50) == 50` is a multiple
    /// of 25.
    #[test]
    fn test_validate_order_shape_reserve_default_replenish_capped_accepts() {
        let book = book_with_lot(25);
        let result = book.validate_order_shape(&reserve(25, 50, 0, None, true));
        assert!(
            result.is_ok(),
            "capped default transfer rejected: {result:?}"
        );
    }

    /// With enough hidden depth the default transfer is not capped, and 80 is
    /// not a multiple of 25.
    #[test]
    fn test_validate_order_shape_reserve_default_replenish_uncapped_rejects() {
        assert!(
            !DEFAULT_RESERVE_REPLENISH_AMOUNT.get().is_multiple_of(25),
            "this case assumes the upstream default is not a multiple of 25"
        );
        let book = book_with_lot(25);
        let result = book.validate_order_shape(&reserve(25, 100, 0, None, true));
        assert_invalid_lot(result, DEFAULT_RESERVE_REPLENISH_AMOUNT.get(), 25);
    }

    /// The very same shape is accepted without `auto_replenish`: no amount
    /// and no auto-replenish means nothing is ever transferred.
    #[test]
    fn test_validate_order_shape_reserve_no_amount_no_auto_accepts() {
        let book = book_with_lot(25);
        let result = book.validate_order_shape(&reserve(25, 100, 0, None, false));
        assert!(
            result.is_ok(),
            "non-replenishing reserve rejected: {result:?}"
        );
    }

    /// The threshold is only compared against the visible tranche, never
    /// transferred, so it is unrestricted.
    #[test]
    fn test_validate_order_shape_reserve_misaligned_threshold_accepts() {
        let book = book_with_lot(10);
        for threshold in [15, 7] {
            let result = book.validate_order_shape(&reserve(10, 20, threshold, Some(10), true));
            assert!(result.is_ok(), "threshold {threshold} rejected: {result:?}");
        }
    }

    // --- Single-tranche kinds keep their quantity rule ---

    /// Every one-quantity kind is still validated on that quantity.
    #[test]
    fn test_validate_order_shape_single_tranche_kinds_check_quantity() {
        let book = book_with_lot(10);
        let id = new_id();
        let price = Price::new(PRICE);
        let timestamp = TimestampMs::new(crate::utils::current_time_millis());

        for aligned in [true, false] {
            let quantity = Quantity::new(if aligned { 20 } else { 15 });
            let kinds: [OrderType<()>; 5] = [
                OrderType::Standard {
                    id,
                    price,
                    quantity,
                    side: Side::Buy,
                    user_id: Hash32::zero(),
                    timestamp,
                    time_in_force: TimeInForce::Gtc,
                    extra_fields: (),
                },
                OrderType::PostOnly {
                    id,
                    price,
                    quantity,
                    side: Side::Buy,
                    user_id: Hash32::zero(),
                    timestamp,
                    time_in_force: TimeInForce::Gtc,
                    extra_fields: (),
                },
                OrderType::TrailingStop {
                    id,
                    price,
                    quantity,
                    side: Side::Buy,
                    user_id: Hash32::zero(),
                    timestamp,
                    time_in_force: TimeInForce::Gtc,
                    trail_amount: Quantity::new(5),
                    last_reference_price: price,
                    extra_fields: (),
                },
                OrderType::PeggedOrder {
                    id,
                    price,
                    quantity,
                    side: Side::Buy,
                    user_id: Hash32::zero(),
                    timestamp,
                    time_in_force: TimeInForce::Gtc,
                    reference_price_offset: 0,
                    reference_price_type: pricelevel::PegReferenceType::BestBid,
                    extra_fields: (),
                },
                OrderType::MarketToLimit {
                    id,
                    price,
                    quantity,
                    side: Side::Buy,
                    user_id: Hash32::zero(),
                    timestamp,
                    time_in_force: TimeInForce::Gtc,
                    extra_fields: (),
                },
            ];

            for kind in kinds {
                let result = book.validate_order_shape(&kind);
                if aligned {
                    assert!(result.is_ok(), "aligned {kind:?} rejected: {result:?}");
                } else {
                    assert_invalid_lot(result, 15, 10);
                }
            }
        }
    }

    // --- The narrowed symmetry ---

    /// Iceberg and Reserve share *identical* **lot-size** validation. Two
    /// Reserve-only rules break the symmetry and are asserted separately: the
    /// replenishment transfer check (so the shared verdict is taken on a
    /// reserve that never transfers, `auto_replenish` off), and the
    /// zero-visible ghost rule, which applies to the non-auto reserve alone
    /// (#230) and is asserted as an explicit divergence inside the loop.
    #[test]
    fn test_validate_order_shape_iceberg_and_reserve_share_tranche_verdicts() {
        let book = book_with_lot(10);
        let pairs = [
            (10, 20),
            (15, 5),
            (5, 15),
            (20, 0),
            (0, 20),
            (10, 25),
            (30, 30),
            (7, 3),
        ];

        for (visible, hidden) in pairs {
            let iceberg_verdict = book.validate_order_shape(&iceberg(visible, hidden));
            let reserve_verdict =
                book.validate_order_shape(&reserve(visible, hidden, 0, None, false));

            // The one deliberate asymmetry (#230). A zero visible tranche
            // behind hidden depth is a ghost ONLY for a non-auto-replenishing
            // reserve: `pricelevel` removes it without a trade and strands the
            // hidden. The identical iceberg executes — its degenerate guard
            // draws the whole hidden tranche into visible on match — so it
            // stays admissible and the two kinds diverge here on purpose.
            if visible == 0 && hidden > 0 {
                assert!(
                    iceberg_verdict.is_ok(),
                    "{visible}/{hidden}: a zero-visible iceberg is executable, \
                     so it must be accepted: {iceberg_verdict:?}"
                );
                match &reserve_verdict {
                    Err(OrderBookError::ZeroVisibleTranche {
                        hidden_quantity, ..
                    }) => assert_eq!(
                        *hidden_quantity, hidden,
                        "{visible}/{hidden}: the stranded tranche is reported"
                    ),
                    other => panic!(
                        "{visible}/{hidden}: a zero-visible non-auto reserve must be \
                         rejected as a ghost, got {other:?}"
                    ),
                }
                continue;
            }

            match (&iceberg_verdict, &reserve_verdict) {
                (Ok(_), Ok(_)) => {}
                (
                    Err(OrderBookError::InvalidLotSize {
                        quantity: iceberg_quantity,
                        lot_size: iceberg_lot,
                    }),
                    Err(OrderBookError::InvalidLotSize {
                        quantity: reserve_quantity,
                        lot_size: reserve_lot,
                    }),
                ) => {
                    assert_eq!(
                        iceberg_quantity, reserve_quantity,
                        "{visible}/{hidden}: verdicts must name the same quantity"
                    );
                    assert_eq!(
                        iceberg_lot, reserve_lot,
                        "{visible}/{hidden}: same lot size"
                    );
                }
                _ => panic!(
                    "{visible}/{hidden}: diverging verdicts — iceberg {iceberg_verdict:?}, reserve {reserve_verdict:?}"
                ),
            }
        }
    }

    /// Lot-size validation stays off when the book has none.
    #[test]
    fn test_validate_order_shape_without_lot_size_accepts_any_reserve() {
        let book: OrderBook<()> = OrderBook::new("NOLOT");
        let result = book.validate_order_shape(&reserve(15, 5, 3, Some(7), true));
        assert!(result.is_ok(), "unconstrained book rejected: {result:?}");
    }
}
