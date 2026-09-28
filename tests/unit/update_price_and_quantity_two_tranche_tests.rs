//! #221: `UpdatePriceAndQuantity` on two-tranche (Iceberg / Reserve) orders.
//!
//! - `new_quantity` sets the **visible** tranche and leaves hidden
//!   untouched, the same contract as `UpdateQuantity` and `Replace`. A
//!   reserve order used to read it as a total target and only ever reduce,
//!   so a requested increase was silently dropped and a decrease was drawn
//!   across both tranches.
//! - The three quantity-carrying update variants agree on the resulting
//!   tranche split for the same requested quantity.
//! - Because the requested size is now actually applied, shape validation
//!   and the risk gate see the real total (`visible + hidden`); a rejected
//!   update leaves the original order untouched.

#[cfg(test)]
mod tests_update_price_and_quantity_two_tranche {
    use orderbook_rs::orderbook::modifications::OrderQuantity;
    use orderbook_rs::{DefaultOrderBook, OrderBook, OrderBookError, RiskConfig};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    const PRICE: u128 = 1000;
    const NEW_PRICE: u128 = 1010;

    /// Build a reserve buy order with the given tranche split.
    fn reserve_order(id: Id, visible: u64, hidden: u64, user_id: Hash32) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id,
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(2),
            replenish_amount: std::num::NonZeroU64::new(3),
            auto_replenish: true,
            extra_fields: (),
        }
    }

    /// Book holding a single reserve buy order with the given split.
    fn book_with_reserve(id: Id, visible: u64, hidden: u64) -> OrderBook<()> {
        let book: OrderBook<()> = DefaultOrderBook::new("TWO-TRANCHE");
        let added = book.add_order(reserve_order(id, visible, hidden, Hash32::zero()));
        assert!(added.is_ok(), "seeding the reserve order must succeed");
        assert!(
            book.best_ask().is_none(),
            "no contra liquidity: an update on this book can never cross"
        );
        book
    }

    /// Assert that `order_id` rests at `price` with the given reserve split.
    fn assert_reserve_state(
        book: &OrderBook<()>,
        order_id: Id,
        price: u128,
        visible: u64,
        hidden: u64,
    ) {
        let Some(order) = book.get_order(order_id) else {
            panic!("the reserve order is no longer on the book");
        };
        assert_eq!(order.price().as_u128(), price);
        assert_eq!(order.total_quantity(), visible + hidden);
        let OrderType::ReserveOrder {
            visible_quantity,
            hidden_quantity,
            ..
        } = order.as_ref()
        else {
            panic!("expected a reserve order");
        };
        assert_eq!(visible_quantity.as_u64(), visible);
        assert_eq!(hidden_quantity.as_u64(), hidden);
    }

    #[test]
    fn test_update_price_and_quantity_reserve_increase_sets_visible_and_keeps_hidden() {
        let order_id = new_id();
        let book = book_with_reserve(order_id, 5, 5);

        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(15),
        });

        assert!(matches!(result, Ok(Some(_))), "the update must be applied");
        assert_reserve_state(&book, order_id, NEW_PRICE, 15, 5);
        assert_eq!(book.best_bid(), Some(NEW_PRICE));
    }

    #[test]
    fn test_update_price_and_quantity_reserve_decrease_sets_visible_and_keeps_hidden() {
        let order_id = new_id();
        let book = book_with_reserve(order_id, 30, 70);

        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(10),
        });

        assert!(matches!(result, Ok(Some(_))), "the update must be applied");
        assert_reserve_state(&book, order_id, NEW_PRICE, 10, 70);
    }

    #[test]
    fn test_update_price_and_quantity_reserve_amplified_case_sets_visible_and_keeps_hidden() {
        let order_id = new_id();
        let book = book_with_reserve(order_id, 30, 70);

        // The issue's amplified case: the requested 80 is the new visible
        // tranche. It used to be read as a 100 -> 80 total reduction and
        // left the order at 10 visible / 70 hidden.
        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(80),
        });

        assert!(matches!(result, Ok(Some(_))), "the update must be applied");
        assert_reserve_state(&book, order_id, NEW_PRICE, 80, 70);
    }

    #[test]
    fn test_update_price_and_quantity_iceberg_sets_visible_and_keeps_hidden() {
        let order_id = new_id();
        let book: OrderBook<()> = DefaultOrderBook::new("TWO-TRANCHE");
        let added =
            book.add_iceberg_order(order_id, PRICE, 5, 5, Side::Buy, TimeInForce::Gtc, None);
        assert!(added.is_ok(), "seeding the iceberg order must succeed");

        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(15),
        });

        assert!(matches!(result, Ok(Some(_))), "the update must be applied");
        let Some(order) = book.get_order(order_id) else {
            panic!("the iceberg order is no longer on the book");
        };
        assert_eq!(order.price().as_u128(), NEW_PRICE);
        assert_eq!(order.total_quantity(), 20);
        let OrderType::IcebergOrder {
            visible_quantity,
            hidden_quantity,
            ..
        } = order.as_ref()
        else {
            panic!("expected an iceberg order");
        };
        assert_eq!(visible_quantity.as_u64(), 15);
        assert_eq!(hidden_quantity.as_u64(), 5);
    }

    /// Each variant runs on its own book holding a single resting reserve
    /// order and no contra liquidity, and the price is kept at `PRICE`, so
    /// the cancel-then-add variants re-enter without crossing and the
    /// comparison isolates the quantity semantics.
    #[test]
    fn test_update_variants_agree_on_reserve_tranche_split() {
        let quantity_id = new_id();
        let quantity_book = book_with_reserve(quantity_id, 30, 70);
        let by_quantity = quantity_book.update_order(OrderUpdate::UpdateQuantity {
            order_id: quantity_id,
            new_quantity: Quantity::new(45),
        });

        let replace_id = new_id();
        let replace_book = book_with_reserve(replace_id, 30, 70);
        let by_replace = replace_book.update_order(OrderUpdate::Replace {
            order_id: replace_id,
            price: Price::new(PRICE),
            quantity: Quantity::new(45),
            side: Side::Buy,
        });

        let both_id = new_id();
        let both_book = book_with_reserve(both_id, 30, 70);
        let by_both = both_book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id: both_id,
            new_price: Price::new(PRICE),
            new_quantity: Quantity::new(45),
        });

        assert!(matches!(by_quantity, Ok(Some(_))), "UpdateQuantity applied");
        assert!(matches!(by_replace, Ok(Some(_))), "Replace applied");
        assert!(
            matches!(by_both, Ok(Some(_))),
            "UpdatePriceAndQuantity applied"
        );

        assert_reserve_state(&quantity_book, quantity_id, PRICE, 45, 70);
        assert_reserve_state(&replace_book, replace_id, PRICE, 45, 70);
        assert_reserve_state(&both_book, both_id, PRICE, 45, 70);
    }

    #[test]
    fn test_update_price_and_quantity_reserve_rejected_by_max_order_size_leaves_original_unchanged()
    {
        let order_id = new_id();
        let mut book: OrderBook<()> = DefaultOrderBook::new("TWO-TRANCHE");
        book.set_max_order_size(15);
        let added = book.add_order(reserve_order(order_id, 5, 5, Hash32::zero()));
        assert!(added.is_ok(), "seeding the reserve order must succeed");

        // The projected total is 15 visible + 5 hidden = 20, above the cap.
        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(15),
        });

        assert!(
            matches!(
                result,
                Err(OrderBookError::OrderSizeOutOfRange {
                    quantity: 20,
                    min: None,
                    max: Some(15),
                })
            ),
            "expected the shape validator to reject the projected total"
        );
        assert_reserve_state(&book, order_id, PRICE, 5, 5);
    }

    #[test]
    fn test_update_price_and_quantity_reserve_rejected_by_risk_notional_leaves_original_unchanged()
    {
        let order_id = new_id();
        let user_id = Hash32::new([7; 32]);
        let mut book: OrderBook<()> = DefaultOrderBook::new("TWO-TRANCHE");
        // The modify-aware check swaps the order's tracked contribution, so
        // the config must be installed before the order is admitted.
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(15_000));
        let added = book.add_order(reserve_order(order_id, 5, 5, user_id));
        assert!(added.is_ok(), "seeding the reserve order must succeed");

        // The re-priced projection is 20 units at 1010 ticks = 20_200
        // notional, above the 15_000 cap. Before #221 the reserve stayed at
        // 10 units, 10_100 at the new price, and the update went through.
        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(15),
        });

        let Err(OrderBookError::RiskMaxNotional {
            attempted, limit, ..
        }) = result
        else {
            panic!("expected the notional gate to reject the update");
        };
        assert_eq!(attempted, 20_200);
        assert_eq!(limit, 15_000);
        assert_reserve_state(&book, order_id, PRICE, 5, 5);
    }

    #[test]
    fn test_update_price_and_quantity_reserve_overflow_rejected_leaves_original_unchanged() {
        let order_id = new_id();
        let book = book_with_reserve(order_id, 5, 5);

        // u64::MAX visible + 5 hidden is unrepresentable (#210).
        let result = book.update_order(OrderUpdate::UpdatePriceAndQuantity {
            order_id,
            new_price: Price::new(NEW_PRICE),
            new_quantity: Quantity::new(u64::MAX),
        });

        assert!(
            matches!(
                result,
                Err(OrderBookError::QuantityOverflow {
                    visible: u64::MAX,
                    hidden: 5,
                })
            ),
            "expected the two-tranche representability check to reject it"
        );
        assert_reserve_state(&book, order_id, PRICE, 5, 5);
    }
}
