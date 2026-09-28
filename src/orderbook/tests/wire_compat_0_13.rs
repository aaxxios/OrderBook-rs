//! Wire-compatibility pins against payloads written by orderbook-rs 0.13.1
//! (pricelevel 0.9.2), for the pricelevel 0.10 upgrade (#239).
//!
//! The fixtures under `fixtures/` were produced by 0.13.1 itself from one
//! book: asks standard 10 @ 100 (id 1), iceberg 5 visible / 20 hidden @ 100
//! (id 2), standard 7 @ 101 (id 3); bid standard 9 @ 99 (id 4); then a market
//! buy of 12 (id 10) that filled id 1 and 2 units of the iceberg.

#[cfg(test)]
mod tests {
    use crate::orderbook::OrderBook;
    use crate::orderbook::trade::TradeResult;
    use pricelevel::{Id, Side, TimeInForce};
    use serde_json::Value;

    /// Replaces every scalar by a type tag, keeping object keys and array
    /// lengths, so two payloads with different timestamps / ids compare by
    /// shape only.
    fn shape(value: &Value) -> Value {
        match value {
            Value::Null => Value::String("null".to_string()),
            Value::Bool(_) => Value::String("bool".to_string()),
            Value::Number(_) => Value::String("number".to_string()),
            Value::String(_) => Value::String("string".to_string()),
            Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), shape(value)))
                    .collect(),
            ),
        }
    }

    fn fixture_book() -> OrderBook<()> {
        let book: OrderBook<()> = OrderBook::new("BTC/USD");
        book.add_limit_order(Id::from_u64(1), 100, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask 1");
        book.add_iceberg_order(
            Id::from_u64(2),
            100,
            5,
            20,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("iceberg 2");
        book.add_limit_order(Id::from_u64(3), 101, 7, Side::Sell, TimeInForce::Gtc, None)
            .expect("ask 3");
        book.add_limit_order(Id::from_u64(4), 99, 9, Side::Buy, TimeInForce::Gtc, None)
            .expect("bid 4");
        book.submit_market_order(Id::from_u64(10), 12, Side::Buy)
            .expect("market buy");
        book
    }

    /// `impl Serialize for OrderBook` maps every level through the fallible
    /// `PriceLevel::snapshot` (pricelevel 0.10). The level map must still
    /// serialize the snapshot itself, not a `{"Ok": ..}` wrapper: the JSON
    /// has exactly the 0.13.1 shape.
    #[test]
    fn test_orderbook_serialize_has_no_ok_wrapper_and_matches_0_13_1_shape() {
        let book = fixture_book();
        let json = serde_json::to_string(&book).expect("serialize book");
        assert!(!json.contains("\"Ok\""), "no Result wrapper: {json}");
        assert!(!json.contains("\"Err\""), "no Result wrapper: {json}");

        let current: Value = serde_json::from_str(&json).expect("parse current");
        let legacy: Value = serde_json::from_str(include_str!("fixtures/book_serde_0_13_1.json"))
            .expect("parse 0.13.1 fixture");
        assert_eq!(
            shape(&current),
            shape(&legacy),
            "OrderBook JSON shape must match 0.13.1"
        );

        let bid = current
            .get("bids")
            .and_then(|bids| bids.get("99"))
            .expect("bid level keyed by price");
        assert_eq!(bid.get("price").and_then(Value::as_u64), Some(99));
        assert_eq!(bid.get("order_count").and_then(Value::as_u64), Some(1));
    }

    /// A JSON `TradeResult` written by 0.13.1 still decodes: the new
    /// `MatchResult::error` field (pricelevel 0.10) defaults to "no error".
    #[test]
    fn test_trade_result_json_from_0_13_1_decodes() {
        let trade: TradeResult =
            serde_json::from_str(include_str!("fixtures/trade_result_0_13_1.json"))
                .expect("0.13.1 TradeResult JSON must decode");
        assert_eq!(trade.symbol, "BTC/USD");
        assert_eq!(trade.match_result.trades().len(), 2);
        assert!(trade.match_result.error().is_none(), "no error slot");
        assert_eq!(trade.total_maker_fees, 0);
        assert_eq!(trade.total_taker_fees, 0);
    }

    /// Documented wire break: a bincode `TradeResult` written by 0.13.1 does
    /// not decode under 0.14, because pricelevel 0.10 appended a positional
    /// `MatchResult::error` field. Mixed-version NATS consumers using the
    /// bincode serializer must upgrade producers and consumers together.
    #[cfg(feature = "bincode")]
    #[test]
    fn test_trade_result_bincode_from_0_13_1_does_not_decode() {
        use crate::orderbook::serialization::{BincodeEventSerializer, EventSerializer};

        let bytes = include_bytes!("fixtures/trade_result_0_13_1.bincode");
        let decoded = BincodeEventSerializer::new().deserialize_trade(bytes);
        assert!(
            decoded.is_err(),
            "0.13.1 bincode TradeResult is a documented wire break, got {decoded:?}"
        );
    }
}
