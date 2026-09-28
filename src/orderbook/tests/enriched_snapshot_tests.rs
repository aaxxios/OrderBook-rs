//! Tests for enriched snapshots with pre-calculated metrics

#[cfg(test)]
mod tests {
    use crate::OrderBook;
    use crate::orderbook::snapshot::MetricFlags;
    use pricelevel::{Id, Side, TimeInForce};

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    fn setup_test_book() -> OrderBook<()> {
        let book = OrderBook::<()>::new("BTC/USD");

        // Add buy orders
        let _ = book.add_limit_order(new_id(), 100, 10, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 99, 20, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 98, 30, Side::Buy, TimeInForce::Gtc, None);

        // Add sell orders
        let _ = book.add_limit_order(new_id(), 101, 15, Side::Sell, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 102, 25, Side::Sell, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 103, 35, Side::Sell, TimeInForce::Gtc, None);

        book
    }

    #[test]
    fn test_enriched_snapshot_all_metrics() {
        let book = setup_test_book();

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        // Check snapshot basics
        assert_eq!(snapshot.symbol, "BTC/USD");
        assert_eq!(snapshot.bids.len(), 3);
        assert_eq!(snapshot.asks.len(), 3);

        // Check mid price
        assert!(snapshot.mid_price.is_some());
        let mid = snapshot.mid_price.unwrap();
        assert!((mid - 100.5).abs() < 0.01); // (100 + 101) / 2

        // Check spread bps
        assert!(snapshot.spread_bps.is_some());
        let spread_bps = snapshot.spread_bps.unwrap();
        assert!(spread_bps > 0.0);

        // Check depths
        assert_eq!(snapshot.bid_depth_total, 60); // 10 + 20 + 30
        assert_eq!(snapshot.ask_depth_total, 75); // 15 + 25 + 35

        // Check VWAP
        assert!(snapshot.vwap_bid.is_some());
        assert!(snapshot.vwap_ask.is_some());

        // Check imbalance (more asks than bids)
        assert!(snapshot.order_book_imbalance < 0.0);
    }

    #[test]
    fn test_enriched_snapshot_custom_metrics() {
        let book = setup_test_book();

        let flags = MetricFlags::MID_PRICE | MetricFlags::SPREAD;
        let snapshot = book
            .enriched_snapshot_with_metrics(10, flags)
            .expect("enriched snapshot");

        // Check that selected metrics are calculated
        assert!(snapshot.mid_price.is_some());
        assert!(snapshot.spread_bps.is_some());

        // Check that unselected metrics have default values
        assert_eq!(snapshot.bid_depth_total, 0);
        assert_eq!(snapshot.ask_depth_total, 0);
        assert!(snapshot.vwap_bid.is_none());
        assert!(snapshot.vwap_ask.is_none());
        assert_eq!(snapshot.order_book_imbalance, 0.0);
    }

    #[test]
    fn test_enriched_snapshot_only_depth() {
        let book = setup_test_book();

        let snapshot = book
            .enriched_snapshot_with_metrics(10, MetricFlags::DEPTH)
            .expect("enriched snapshot");

        assert_eq!(snapshot.bid_depth_total, 60);
        assert_eq!(snapshot.ask_depth_total, 75);
        assert!(snapshot.mid_price.is_none());
        assert!(snapshot.spread_bps.is_none());
    }

    #[test]
    fn test_enriched_snapshot_only_vwap() {
        let book = setup_test_book();

        let snapshot = book
            .enriched_snapshot_with_metrics(10, MetricFlags::VWAP)
            .expect("enriched snapshot");

        assert!(snapshot.vwap_bid.is_some());
        assert!(snapshot.vwap_ask.is_some());

        // VWAP bid = (100*10 + 99*20 + 98*30) / (10+20+30) = 5920/60 = 98.666...
        let vwap_bid = snapshot.vwap_bid.unwrap();
        assert!((vwap_bid - 98.666).abs() < 0.01);

        // VWAP ask = (101*15 + 102*25 + 103*35) / (15+25+35) = 7670/75 = 102.266...
        let vwap_ask = snapshot.vwap_ask.unwrap();
        assert!((vwap_ask - 102.266).abs() < 0.01);
    }

    #[test]
    fn test_enriched_snapshot_only_imbalance() {
        let book = setup_test_book();

        let snapshot = book
            .enriched_snapshot_with_metrics(10, MetricFlags::IMBALANCE)
            .expect("enriched snapshot");

        // Imbalance = (60 - 75) / (60 + 75) = -15/135 ≈ -0.111
        assert!((snapshot.order_book_imbalance - (-0.111)).abs() < 0.01);
    }

    #[test]
    fn test_enriched_snapshot_empty_book() {
        let book = OrderBook::<()>::new("EMPTY");

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        assert!(snapshot.mid_price.is_none());
        assert!(snapshot.spread_bps.is_none());
        assert_eq!(snapshot.bid_depth_total, 0);
        assert_eq!(snapshot.ask_depth_total, 0);
        assert!(snapshot.vwap_bid.is_none());
        assert!(snapshot.vwap_ask.is_none());
        assert_eq!(snapshot.order_book_imbalance, 0.0);
    }

    #[test]
    fn test_enriched_snapshot_one_sided() {
        let book = OrderBook::<()>::new("ONE_SIDED");
        let _ = book.add_limit_order(new_id(), 100, 50, Side::Buy, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        assert!(snapshot.mid_price.is_none()); // No ask side
        assert!(snapshot.spread_bps.is_none());
        assert_eq!(snapshot.bid_depth_total, 50);
        assert_eq!(snapshot.ask_depth_total, 0);
    }

    #[test]
    fn test_enriched_snapshot_limited_depth() {
        let book = setup_test_book();

        let snapshot = book.enriched_snapshot(2).expect("enriched snapshot"); // Only top 2 levels

        assert_eq!(snapshot.bids.len(), 2);
        assert_eq!(snapshot.asks.len(), 2);

        // Depth should only include top 2 levels
        assert_eq!(snapshot.bid_depth_total, 30); // 10 + 20 (not including 30)
        assert_eq!(snapshot.ask_depth_total, 40); // 15 + 25 (not including 35)
    }

    #[test]
    fn test_enriched_snapshot_mid_price_calculation() {
        let book = OrderBook::<()>::new("MID_TEST");
        let _ = book.add_limit_order(new_id(), 100, 10, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 110, 10, Side::Sell, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        assert!(snapshot.mid_price.is_some());
        let mid = snapshot.mid_price.unwrap();
        assert!((mid - 105.0).abs() < 0.01); // (100 + 110) / 2
    }

    #[test]
    fn test_enriched_snapshot_spread_bps_calculation() {
        let book = OrderBook::<()>::new("SPREAD_TEST");
        let _ = book.add_limit_order(new_id(), 10000, 10, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 10100, 10, Side::Sell, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        assert!(snapshot.spread_bps.is_some());
        let spread_bps = snapshot.spread_bps.unwrap();

        // Spread = 100, Mid = 10050, BPS = (100/10050) * 10000 ≈ 99.5
        assert!((spread_bps - 99.5).abs() < 0.5);
    }

    #[test]
    fn test_enriched_snapshot_balanced_book() {
        let book = OrderBook::<()>::new("BALANCED");
        let _ = book.add_limit_order(new_id(), 100, 50, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 101, 50, Side::Sell, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        // Perfectly balanced book should have imbalance near 0
        assert!((snapshot.order_book_imbalance - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_enriched_snapshot_buy_heavy() {
        let book = OrderBook::<()>::new("BUY_HEAVY");
        let _ = book.add_limit_order(new_id(), 100, 100, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 101, 50, Side::Sell, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        // More buy volume, positive imbalance
        assert!(snapshot.order_book_imbalance > 0.0);
    }

    #[test]
    fn test_enriched_snapshot_sell_heavy() {
        let book = OrderBook::<()>::new("SELL_HEAVY");
        let _ = book.add_limit_order(new_id(), 100, 30, Side::Buy, TimeInForce::Gtc, None);
        let _ = book.add_limit_order(new_id(), 101, 100, Side::Sell, TimeInForce::Gtc, None);

        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        // More sell volume, negative imbalance
        assert!(snapshot.order_book_imbalance < 0.0);
    }

    #[test]
    fn test_metric_flags_combination() {
        let flags = MetricFlags::MID_PRICE | MetricFlags::DEPTH | MetricFlags::VWAP;

        assert!(flags.contains(MetricFlags::MID_PRICE));
        assert!(flags.contains(MetricFlags::DEPTH));
        assert!(flags.contains(MetricFlags::VWAP));
        assert!(!flags.contains(MetricFlags::SPREAD));
        assert!(!flags.contains(MetricFlags::IMBALANCE));
    }

    #[test]
    fn test_metric_flags_all() {
        let flags = MetricFlags::ALL;

        assert!(flags.contains(MetricFlags::MID_PRICE));
        assert!(flags.contains(MetricFlags::SPREAD));
        assert!(flags.contains(MetricFlags::DEPTH));
        assert!(flags.contains(MetricFlags::VWAP));
        assert!(flags.contains(MetricFlags::IMBALANCE));
    }

    #[test]
    fn test_enriched_snapshot_vwap_limited_levels() {
        let book = setup_test_book();

        // Request only 2 levels for VWAP calculation
        let snapshot = book
            .enriched_snapshot_with_metrics(2, MetricFlags::VWAP)
            .expect("enriched snapshot");

        assert!(snapshot.vwap_bid.is_some());
        assert!(snapshot.vwap_ask.is_some());

        // VWAP should only use top 2 levels
        // Bid VWAP = (100*10 + 99*20) / 30 = 2980/30 = 99.333...
        let vwap_bid = snapshot.vwap_bid.unwrap();
        assert!((vwap_bid - 99.333).abs() < 0.01);
    }

    #[test]
    fn test_enriched_snapshot_serialization() {
        let book = setup_test_book();
        let snapshot = book.enriched_snapshot(10).expect("enriched snapshot");

        // Test that it can be serialized
        let json = serde_json::to_string(&snapshot);
        assert!(json.is_ok());

        // Test that it can be deserialized
        let json_str = json.unwrap();
        let deserialized: Result<crate::orderbook::snapshot::EnrichedSnapshot, _> =
            serde_json::from_str(&json_str);
        assert!(deserialized.is_ok());

        let deserialized_snapshot = deserialized.unwrap();
        assert_eq!(deserialized_snapshot.symbol, snapshot.symbol);
        assert_eq!(deserialized_snapshot.bids.len(), snapshot.bids.len());
    }
}
