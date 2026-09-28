#[cfg(test)]
mod tests {
    use crate::OrderBookError;
    use pricelevel::{PriceLevelError, Side};

    #[test]
    fn test_display_price_level_error() {
        let err = OrderBookError::PriceLevelError(PriceLevelError::InvalidFormat);
        assert_eq!(format!("{err}"), "Price level error: Invalid format");
    }

    #[test]
    fn test_display_order_not_found() {
        let order_id = "e4968197-6137-47a4-ba79-690d8c552248";
        let err = OrderBookError::OrderNotFound(order_id.to_string());
        assert_eq!(format!("{err}"), format!("Order not found: {}", order_id));
    }

    #[test]
    fn test_display_invalid_price_level() {
        let price = 1000;
        let err = OrderBookError::InvalidPriceLevel(price);
        assert_eq!(format!("{err}"), format!("Invalid price level: {}", price));
    }

    #[test]
    fn test_display_price_crossing() {
        let err = OrderBookError::PriceCrossing {
            price: 1000,
            side: Side::Buy,
            opposite_price: Some(999),
        };
        assert_eq!(
            format!("{err}"),
            "Price crossing: BUY 1000 would cross opposite at 999"
        );
    }

    /// #247: an opposite side that emptied is `None`, not a fake `0`.
    #[test]
    fn test_display_price_crossing_without_opposite_price() {
        let err = OrderBookError::PriceCrossing {
            price: 1000,
            side: Side::Buy,
            opposite_price: None,
        };
        assert_eq!(
            format!("{err}"),
            "Price crossing: BUY 1000 would cross the opposite side"
        );
    }

    #[test]
    fn test_display_insufficient_liquidity() {
        let err = OrderBookError::InsufficientLiquidity {
            side: Side::Sell,
            requested: 100,
            available: 50,
        };
        assert_eq!(
            format!("{err}"),
            "Insufficient liquidity for SELL order: requested 100, available 50"
        );
    }

    #[test]
    fn test_display_invalid_operation() {
        let message = "Cannot update price to the same value";
        let err = OrderBookError::InvalidOperation {
            message: message.to_string(),
        };
        assert_eq!(format!("{err}"), format!("Invalid operation: {}", message));
    }

    #[test]
    fn test_from_price_level_error() {
        let price_level_error = PriceLevelError::InvalidFormat;
        let order_book_error: OrderBookError = price_level_error.into();

        match order_book_error {
            OrderBookError::PriceLevelError(err) => match err {
                PriceLevelError::InvalidFormat => (),
                _ => panic!("Expected PriceLevelError::InvalidFormat"),
            },
            _ => panic!("Expected OrderBookError::PriceLevelError"),
        }
    }

    #[test]
    fn test_error_trait_implementation() {
        let err = OrderBookError::InvalidPriceLevel(1000);
        let _: &dyn std::error::Error = &err; // This will compile only if OrderBookError implements std::error::Error
    }

    #[test]
    fn test_missing_field_conversion() {
        let field_name = "price";
        let price_level_error = PriceLevelError::MissingField(field_name.to_string());
        let order_book_error: OrderBookError = price_level_error.into();

        match order_book_error {
            OrderBookError::PriceLevelError(PriceLevelError::MissingField(field)) => {
                assert_eq!(field, field_name);
            }
            _ => panic!("Expected OrderBookError::PriceLevelError(PriceLevelError::MissingField)"),
        }
    }

    #[test]
    fn test_invalid_field_value_conversion() {
        let price_level_error = PriceLevelError::InvalidFieldValue {
            field: "price".to_string(),
            value: "invalid".to_string(),
        };
        let order_book_error: OrderBookError = price_level_error.into();

        match order_book_error {
            OrderBookError::PriceLevelError(PriceLevelError::InvalidFieldValue {
                field,
                value,
            }) => {
                assert_eq!(field, "price");
                assert_eq!(value, "invalid");
            }
            _ => panic!(
                "Expected OrderBookError::PriceLevelError(PriceLevelError::InvalidFieldValue)"
            ),
        }
    }

    // --- #230: the two new variants ---

    /// `ZeroVisibleTranche` names the order and the tranche that would be
    /// stranded, and clones field-for-field.
    #[test]
    fn test_zero_visible_tranche_display_and_clone() {
        let order_id = pricelevel::Id::from_u64(77);
        let error = OrderBookError::ZeroVisibleTranche {
            order_id,
            hidden_quantity: 20,
        };

        let rendered = error.to_string();
        assert!(
            rendered.contains("zero visible tranche"),
            "names the rule: {rendered}"
        );
        assert!(
            rendered.contains("20"),
            "names the hidden tranche: {rendered}"
        );
        assert!(
            rendered.contains(&order_id.to_string()),
            "names the order: {rendered}"
        );

        match error.clone() {
            OrderBookError::ZeroVisibleTranche {
                order_id: cloned_id,
                hidden_quantity,
            } => {
                assert_eq!(cloned_id, order_id);
                assert_eq!(hidden_quantity, 20);
            }
            other => panic!("clone changed the variant: {other:?}"),
        }
        assert_eq!(
            error.clone().to_string(),
            rendered,
            "clone renders the same"
        );
    }

    /// `ReserveResidualWouldBeDiscarded` distinguishes the projected hidden
    /// tranche from the quantity that would actually be destroyed.
    #[test]
    fn test_reserve_residual_would_be_discarded_display_and_clone() {
        let order_id = pricelevel::Id::from_u64(78);
        let error = OrderBookError::ReserveResidualWouldBeDiscarded {
            order_id,
            visible_quantity: 10,
            crossable_quantity: 15,
            hidden_quantity: 20,
            discarded_quantity: 15,
        };

        let rendered = error.to_string();
        assert!(
            rendered.contains("would cross 15 units"),
            "names the crossable depth: {rendered}"
        );
        assert!(
            rendered.contains("visible tranche of 10"),
            "names the visible tranche: {rendered}"
        );
        assert!(
            rendered.contains("discarding 15 of its 20 hidden units"),
            "separates destroyed from projected hidden: {rendered}"
        );

        match error.clone() {
            OrderBookError::ReserveResidualWouldBeDiscarded {
                order_id: cloned_id,
                visible_quantity,
                crossable_quantity,
                hidden_quantity,
                discarded_quantity,
            } => {
                assert_eq!(cloned_id, order_id);
                assert_eq!(
                    (
                        visible_quantity,
                        crossable_quantity,
                        hidden_quantity,
                        discarded_quantity
                    ),
                    (10, 15, 20, 15)
                );
            }
            other => panic!("clone changed the variant: {other:?}"),
        }
        assert_eq!(
            error.clone().to_string(),
            rendered,
            "clone renders the same"
        );
    }
}
