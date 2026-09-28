//! Integration tests for the pre-trade risk layer on `OrderBook<T>`.
//!
//! Commit 2 of issue #54 covers the rejection paths that work with
//! the just-installed config: price-band breach, mid-fallback to
//! last-trade, no-reference fallthrough, market-order bypass, and
//! the public `set_risk_config` / `risk_config` / `disable_risk`
//! round-trip. Tests that depend on per-account counter state being
//! populated across submits (max_open_orders, max_notional, fill /
//! cancel deltas) land with commit 3, where `on_admission`,
//! `on_fill`, and `on_cancel` are wired into the engine.

#[cfg(test)]
mod tests_risk_layer {
    use orderbook_rs::{OrderBook, OrderBookError, ReferencePriceSource, RiskConfig};
    use pricelevel::{Hash32, Id, Side, TimeInForce};

    /// Fresh random order id (UUID v4).
    fn new_id() -> Id {
        Id::from_uuid(uuid::Uuid::new_v4())
    }

    fn new_book() -> OrderBook<()> {
        OrderBook::new("TEST")
    }

    fn account(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    // ───────────────────────────────────────────────────────────────
    // Public API round-trip
    // ───────────────────────────────────────────────────────────────

    #[test]
    fn risk_config_set_get_disable_round_trip() {
        let mut book = new_book();
        assert!(book.risk_config().is_none());

        let cfg = RiskConfig::new()
            .with_max_open_orders_per_account(7)
            .with_max_notional_per_account(123_456)
            .with_price_band_bps(250, ReferencePriceSource::LastTrade);
        book.set_risk_config(cfg.clone());

        let installed = book.risk_config().expect("config installed");
        assert_eq!(installed.max_open_orders_per_account, Some(7));
        assert_eq!(installed.max_notional_per_account, Some(123_456));
        assert_eq!(installed.price_band_bps, Some(250));
        assert_eq!(
            installed.reference_price,
            Some(ReferencePriceSource::LastTrade)
        );

        book.disable_risk();
        assert!(book.risk_config().is_none());
    }

    // ───────────────────────────────────────────────────────────────
    // Price-band rejection paths
    // ───────────────────────────────────────────────────────────────

    /// Seed two crossing orders so a trade prints and `last_trade_price`
    /// is set. After the helper returns, the book has no resting
    /// orders.
    fn seed_last_trade_price(book: &OrderBook<()>, price: u128) {
        // Resting ask at `price`.
        book.add_limit_order(new_id(), price, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("seed resting ask");
        // Aggressive buy crosses fully.
        book.add_limit_order(new_id(), price, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("aggressive buy fills the ask");
        assert_eq!(
            book.last_trade_price(),
            Some(price),
            "last trade must be set"
        );
    }

    #[test]
    fn limit_far_outside_price_band_returns_risk_price_band() {
        let mut book = new_book();
        seed_last_trade_price(&book, 1_000_000);
        // 1000 bps = 10% allowed band.
        book.set_risk_config(
            RiskConfig::new().with_price_band_bps(1_000, ReferencePriceSource::LastTrade),
        );

        // Submit at +30% from reference → rejected.
        let result =
            book.add_limit_order(new_id(), 1_300_000, 1, Side::Buy, TimeInForce::Gtc, None);
        match result {
            Err(OrderBookError::RiskPriceBand {
                submitted,
                reference,
                deviation_bps,
                limit_bps,
            }) => {
                assert_eq!(submitted, 1_300_000);
                assert_eq!(reference, 1_000_000);
                assert_eq!(deviation_bps, 3_000);
                assert_eq!(limit_bps, 1_000);
            }
            other => panic!("expected RiskPriceBand, got {other:?}"),
        }
    }

    #[test]
    fn limit_within_price_band_succeeds() {
        let mut book = new_book();
        seed_last_trade_price(&book, 1_000_000);
        book.set_risk_config(
            RiskConfig::new().with_price_band_bps(1_000, ReferencePriceSource::LastTrade),
        );

        // +5% from reference is well within the 10% band.
        let result =
            book.add_limit_order(new_id(), 1_050_000, 1, Side::Buy, TimeInForce::Gtc, None);
        assert!(
            result.is_ok(),
            "in-band order must be accepted; got {result:?}"
        );
    }

    #[test]
    fn mid_reference_falls_back_to_last_trade_when_one_sided() {
        let mut book = new_book();
        // Seed a last trade and confirm.
        seed_last_trade_price(&book, 1_000_000);
        // Add a single bid so the book is one-sided (no asks).
        book.add_limit_order(new_id(), 999_000, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("seed bid");
        assert!(book.best_ask().is_none(), "book must be one-sided");

        book.set_risk_config(RiskConfig::new().with_price_band_bps(500, ReferencePriceSource::Mid));

        // +30% from last_trade (1.3M) → rejected because Mid falls
        // back to last_trade when the book is one-sided.
        let result =
            book.add_limit_order(new_id(), 1_300_000, 1, Side::Sell, TimeInForce::Gtc, None);
        assert!(
            matches!(result, Err(OrderBookError::RiskPriceBand { .. })),
            "Mid reference should fall back to last_trade and reject; got {result:?}"
        );
    }

    #[test]
    fn band_skipped_with_warn_when_no_reference_available() {
        let mut book = new_book();
        // Empty book + no trades → no reference price exists.
        book.set_risk_config(RiskConfig::new().with_price_band_bps(100, ReferencePriceSource::Mid));

        // Far-out price should NOT be rejected because no reference
        // is available; the band check is skipped (warn-once latch).
        let result =
            book.add_limit_order(new_id(), 999_999_999, 1, Side::Buy, TimeInForce::Gtc, None);
        assert!(
            result.is_ok(),
            "no-reference path must skip the band check; got {result:?}"
        );
    }

    // ───────────────────────────────────────────────────────────────
    // Market-order bypass
    // ───────────────────────────────────────────────────────────────

    // ───────────────────────────────────────────────────────────────
    // Per-account counter state (commit 3 — admission/fill/cancel hooks)
    // ───────────────────────────────────────────────────────────────

    #[test]
    fn submit_above_max_open_orders_returns_risk_max_open() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(2));
        let acct = account(11);

        // Two admissions consume the quota.
        book.add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first order admitted");
        book.add_limit_order_with_user(new_id(), 101, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("second order admitted");

        // Third is rejected.
        let result = book.add_limit_order_with_user(
            new_id(),
            102,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        );
        match result {
            Err(OrderBookError::RiskMaxOpenOrders {
                account: a,
                current,
                limit,
            }) => {
                assert_eq!(a, acct);
                assert_eq!(current, 2);
                assert_eq!(limit, 2);
            }
            other => panic!("expected RiskMaxOpenOrders, got {other:?}"),
        }
    }

    #[test]
    fn submit_within_max_open_orders_succeeds() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(3));
        let acct = account(12);

        for i in 0..3 {
            book.add_limit_order_with_user(
                new_id(),
                100 + i,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None,
            )
            .unwrap_or_else(|err| panic!("admission {i} failed: {err:?}"));
        }
    }

    #[test]
    fn submit_above_max_notional_returns_risk_max_notional() {
        let mut book = new_book();
        // 1_000 notional ceiling per account.
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(1_000));
        let acct = account(13);

        // 8 * 100 = 800 notional consumed.
        book.add_limit_order_with_user(new_id(), 100, 8, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first admission within budget");

        // 3 * 100 = 300 attempted; 800 + 300 > 1_000 → reject.
        let result = book.add_limit_order_with_user(
            new_id(),
            100,
            3,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        );
        match result {
            Err(OrderBookError::RiskMaxNotional {
                account: a,
                current,
                attempted,
                limit,
            }) => {
                assert_eq!(a, acct);
                assert_eq!(current, 800);
                assert_eq!(attempted, 300);
                assert_eq!(limit, 1_000);
            }
            other => panic!("expected RiskMaxNotional, got {other:?}"),
        }
    }

    #[test]
    fn submit_within_max_notional_succeeds() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(1_000));
        let acct = account(14);

        // 8 * 100 = 800 in budget.
        book.add_limit_order_with_user(new_id(), 100, 8, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first within budget");

        // 2 * 100 = 200; 800 + 200 = 1_000, exactly at the limit, so
        // accepted (`current + attempted > limit` is the gate, strict).
        book.add_limit_order_with_user(new_id(), 100, 2, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("second hits ceiling exactly and is accepted");
    }

    #[test]
    fn cancel_decrements_counters() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let acct = account(15);

        let order_id = new_id();
        book.add_limit_order_with_user(order_id, 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first admission");

        // Second is rejected because the quota is full.
        assert!(
            matches!(
                book.add_limit_order_with_user(
                    new_id(),
                    100,
                    1,
                    Side::Buy,
                    TimeInForce::Gtc,
                    acct,
                    None,
                ),
                Err(OrderBookError::RiskMaxOpenOrders { .. })
            ),
            "second should be rejected"
        );

        // Cancel and retry; should now succeed.
        book.cancel_order(order_id)
            .expect("cancel returns Ok")
            .expect("cancel returns Some");
        book.add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("cancel must drop the counter and re-open the slot");
    }

    #[test]
    fn partial_fill_decrements_notional_and_keeps_count() {
        let mut book = new_book();
        // High open ceiling, tight notional ceiling: we want the
        // partial fill to free notional headroom for a follow-up.
        book.set_risk_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(10)
                .with_max_notional_per_account(2_000),
        );
        let maker_acct = account(16);
        let taker_acct = account(17);

        // Maker rests 10 @ 100 (1_000 notional).
        book.add_limit_order_with_user(
            new_id(),
            100,
            10,
            Side::Buy,
            TimeInForce::Gtc,
            maker_acct,
            None,
        )
        .expect("maker admitted");

        // Taker (different account) submits an aggressive sell that
        // partially fills the maker (qty 4 of 10 at price 100).
        book.submit_market_order_with_user(new_id(), 4, Side::Sell, taker_acct)
            .expect("aggressive sell fills 4 of 10");

        // Maker now has 600 notional (6 * 100). New maker admission
        // for 14 * 100 = 1_400 notional must succeed: 600 + 1_400 =
        // 2_000 (== limit, accepted by strict `>` gate). A larger one
        // (15 * 100 = 1_500) would be rejected.
        book.add_limit_order_with_user(
            new_id(),
            99,
            14,
            Side::Buy,
            TimeInForce::Gtc,
            maker_acct,
            None,
        )
        .expect("partial fill must free notional headroom");

        let breach = book.add_limit_order_with_user(
            new_id(),
            98,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            maker_acct,
            None,
        );
        assert!(
            matches!(breach, Err(OrderBookError::RiskMaxNotional { .. })),
            "ceiling already hit; expected RiskMaxNotional, got {breach:?}"
        );
    }

    #[test]
    fn full_fill_decrements_open_count() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let maker_acct = account(18);
        let taker_acct = account(19);

        // Maker uses the only slot.
        book.add_limit_order_with_user(
            new_id(),
            100,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            maker_acct,
            None,
        )
        .expect("maker admitted");

        // Aggressive sell fully consumes the maker.
        book.submit_market_order_with_user(new_id(), 5, Side::Sell, taker_acct)
            .expect("aggressive sell fills the maker fully");

        // Maker's slot must be free again.
        book.add_limit_order_with_user(
            new_id(),
            100,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            maker_acct,
            None,
        )
        .expect("full fill must drop open_count and re-open the slot");
    }

    #[test]
    fn disable_risk_clears_gates_keeps_counters() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(1));
        let acct = account(20);

        book.add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first admitted");
        // Quota full → second rejected.
        assert!(
            matches!(
                book.add_limit_order_with_user(
                    new_id(),
                    100,
                    1,
                    Side::Buy,
                    TimeInForce::Gtc,
                    acct,
                    None,
                ),
                Err(OrderBookError::RiskMaxOpenOrders { .. })
            ),
            "expected rejection at quota"
        );

        book.disable_risk();

        // After disable, gate is lifted and admission succeeds even
        // though the per-account counter still reads 1 underneath.
        book.add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("disable_risk lifts the gate");
        assert!(book.risk_config().is_none());
    }

    #[test]
    fn market_orders_bypass_risk_checks() {
        let mut book = new_book();
        // Seed resting liquidity for both market-order calls BEFORE
        // installing the risk config, so the seeding limits aren't
        // themselves blocked by the gate we're about to configure.
        book.add_limit_order(new_id(), 1_000_000, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("seed resting ask 1");
        book.add_limit_order(new_id(), 1_000_000, 10, Side::Sell, TimeInForce::Gtc, None)
            .expect("seed resting ask 2");

        // Configure a band so tight any submitted limit price would
        // fail, plus zero open-orders / notional ceilings. Market
        // orders carry no submitted price and no resting contribution
        // so they must bypass every gate.
        book.set_risk_config(
            RiskConfig::new()
                .with_price_band_bps(1, ReferencePriceSource::LastTrade)
                .with_max_open_orders_per_account(0)
                .with_max_notional_per_account(0),
        );

        // No user_id → submit_market_order; should match against a
        // resting ask and not be rejected by any risk gate.
        let result = book.submit_market_order(new_id(), 1, Side::Buy);
        assert!(
            result.is_ok(),
            "market orders must bypass risk checks; got {result:?}"
        );

        // With user_id variant: same story.
        let result = book.submit_market_order_with_user(new_id(), 1, Side::Buy, account(42));
        assert!(
            result.is_ok(),
            "submit_market_order_with_user must bypass risk; got {result:?}"
        );
    }

    // ───────────────────────────────────────────────────────────────
    // Snapshot persistence (commit 4 — RiskConfig + counter rebuild)
    // ───────────────────────────────────────────────────────────────

    #[test]
    fn risk_config_round_trips_through_snapshot() {
        // Build the original book, install a fully-configured risk
        // layer, and rest a few orders across two accounts so the
        // per-account counters carry meaningful state.
        let mut original = new_book();
        let cfg = RiskConfig::new()
            .with_max_open_orders_per_account(2)
            .with_max_notional_per_account(1_000)
            .with_price_band_bps(5_000, ReferencePriceSource::LastTrade);
        original.set_risk_config(cfg.clone());

        let acct_a = account(31);
        let acct_b = account(32);

        // Account A: 2 resting orders @ price 100 — saturates the
        // open-orders quota for that account post-restore.
        original
            .add_limit_order_with_user(new_id(), 100, 3, Side::Buy, TimeInForce::Gtc, acct_a, None)
            .expect("acct_a first admission");
        original
            .add_limit_order_with_user(new_id(), 100, 4, Side::Buy, TimeInForce::Gtc, acct_a, None)
            .expect("acct_a second admission");

        // Account B: a single resting order — quota still has room.
        original
            .add_limit_order_with_user(new_id(), 100, 2, Side::Buy, TimeInForce::Gtc, acct_b, None)
            .expect("acct_b first admission");

        // JSON round-trip via the public snapshot API.
        let json_payload = original
            .snapshot_to_json(10)
            .expect("serialize snapshot package to JSON");

        let mut restored = new_book();
        restored
            .restore_from_snapshot_json(&json_payload)
            .expect("restore from JSON");

        // 1. Config round-trips field-by-field.
        let restored_cfg = restored.risk_config().expect("config restored");
        assert_eq!(
            restored_cfg.max_open_orders_per_account,
            cfg.max_open_orders_per_account,
        );
        assert_eq!(
            restored_cfg.max_notional_per_account,
            cfg.max_notional_per_account,
        );
        assert_eq!(restored_cfg.price_band_bps, cfg.price_band_bps);
        assert_eq!(restored_cfg.reference_price, cfg.reference_price);

        // 2. Account A saturated its quota pre-snapshot. A new
        // submission must be rejected by the rebuilt counters.
        let breach = restored.add_limit_order_with_user(
            new_id(),
            100,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct_a,
            None,
        );
        match breach {
            Err(OrderBookError::RiskMaxOpenOrders {
                account: a,
                current,
                limit,
            }) => {
                assert_eq!(a, acct_a);
                assert_eq!(current, 2);
                assert_eq!(limit, 2);
            }
            other => {
                panic!("expected RiskMaxOpenOrders for acct_a after restore, got {other:?}");
            }
        }

        // 3. Account B still has one slot of headroom.
        restored
            .add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct_b, None)
            .expect("acct_b within rebuilt quota must succeed");
    }

    #[test]
    fn legacy_v2_snapshot_without_risk_config_field_defaults_to_none() {
        use orderbook_rs::orderbook::OrderBookSnapshotPackage;

        // Hand-rolled v2 payload that omits the new `risk_config`
        // field. Deserialization must succeed via `#[serde(default)]`
        // and yield `risk_config: None`. The checksum corresponds to
        // the empty snapshot below; we only assert the additive field
        // default — checksum validation is exercised elsewhere.
        let legacy_v2 = r#"{
            "version": 2,
            "snapshot": {
                "symbol": "LEGACY",
                "timestamp": 0,
                "bids": [],
                "asks": []
            },
            "checksum": "0000000000000000000000000000000000000000000000000000000000000000",
            "fee_schedule": null,
            "stp_mode": "None",
            "tick_size": null,
            "lot_size": null,
            "min_order_size": null,
            "max_order_size": null,
            "engine_seq": 0,
            "kill_switch_engaged": false
        }"#;

        let pkg =
            OrderBookSnapshotPackage::from_json(legacy_v2).expect("legacy v2 payload deserializes");
        assert!(
            pkg.risk_config.is_none(),
            "missing risk_config must default to None for v2 payloads"
        );
        assert_eq!(pkg.version, 2);
    }

    /// #99: `cancel_all_orders` must reset the per-account risk counters; otherwise
    /// an account stays pinned at its open-order limit and new flow is permanently
    /// rejected after a bulk unwind (the exact failure bulk cancel exists to avoid).
    #[test]
    fn cancel_all_orders_resets_open_order_counter() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(2));
        let acct = account(11);

        book.add_limit_order_with_user(new_id(), 100, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("first admitted (1/2)");
        book.add_limit_order_with_user(new_id(), 101, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("second admitted (2/2)");
        // At the limit, a third is rejected.
        assert!(matches!(
            book.add_limit_order_with_user(
                new_id(),
                102,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None
            ),
            Err(OrderBookError::RiskMaxOpenOrders { .. })
        ));

        // Bulk cancel must reset the per-account counters.
        let res = book.cancel_all_orders();
        assert_eq!(res.cancelled_count(), 2);

        // A fresh order from the same account is now admitted (counter was reset).
        book.add_limit_order_with_user(new_id(), 103, 1, Side::Buy, TimeInForce::Gtc, acct, None)
            .expect("re-admitted after cancel_all_orders");
    }

    // ───────────────────────────────────────────────────────────────
    // Checked notional arithmetic (#243)
    // ───────────────────────────────────────────────────────────────

    /// Price whose double does not fit in `u128`.
    const HALF_PLUS_ONE: u128 = u128::MAX / 2 + 1;

    /// #243: two resting orders whose notional sum overflows `u128` must not
    /// wrap the account counter and bypass the limit. Before the fix the
    /// second admission passed (`saturating_add` compared equal to the
    /// `u128::MAX` limit) and `fetch_add` wrapped the counter to zero.
    #[test]
    fn notional_limit_holds_when_sum_overflows_u128() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_notional_per_account(u128::MAX));
        let acct = account(60);

        let first = new_id();
        book.add_limit_order_with_user(
            first,
            HALF_PLUS_ONE,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        )
        .expect("first order fits in u128");

        let second = new_id();
        let result = book.add_limit_order_with_user(
            second,
            HALF_PLUS_ONE + 1,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        );
        assert!(
            matches!(result, Err(OrderBookError::RiskMaxNotional { limit, .. }) if limit == u128::MAX),
            "overflowing sum must reject, got {result:?}"
        );
        assert!(book.get_order(first).is_some());
        assert!(
            book.get_order(second).is_none(),
            "rejected order never rests"
        );

        // Still rejected afterwards: the counter did not wrap.
        assert!(matches!(
            book.add_limit_order_with_user(
                new_id(),
                HALF_PLUS_ONE,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None
            ),
            Err(OrderBookError::RiskMaxNotional { .. })
        ));
        assert_eq!(book.risk_accounting_anomalies(), 0);

        // Releasing the first order frees the account exactly.
        assert!(book.cancel_order(first).expect("cancel").is_some());
        book.add_limit_order_with_user(
            new_id(),
            HALF_PLUS_ONE,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        )
        .expect("admitted again after the release");
    }

    /// #243: with a config installed but no notional limit, an exposure the
    /// counters cannot represent is still rejected (typed, `limit = u128::MAX`)
    /// rather than wrapping the tracked value.
    #[test]
    fn unrepresentable_notional_rejects_without_notional_limit() {
        let mut book = new_book();
        book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(100));
        let acct = account(61);

        book.add_limit_order_with_user(
            new_id(),
            HALF_PLUS_ONE,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        )
        .expect("first order fits");
        let result = book.add_limit_order_with_user(
            new_id(),
            HALF_PLUS_ONE + 1,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            acct,
            None,
        );
        assert!(
            matches!(result, Err(OrderBookError::RiskMaxNotional { limit, .. }) if limit == u128::MAX),
            "got {result:?}"
        );
    }

    /// #243 / #250: a snapshot whose per-account risk aggregates overflow is
    /// rejected by the restore's prepare phase with a typed error, and the
    /// target book is left untouched.
    #[test]
    fn restore_rejects_overflowing_risk_aggregates_before_mutation() {
        let acct = account(62);
        // Rest both orders before the config exists (no admission check),
        // then install the config so the package carries it.
        let mut original = new_book();
        original
            .add_limit_order_with_user(
                new_id(),
                HALF_PLUS_ONE,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None,
            )
            .expect("first");
        original
            .add_limit_order_with_user(
                new_id(),
                HALF_PLUS_ONE + 1,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None,
            )
            .expect("second");
        original.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(10));
        let json = original.snapshot_to_json(10).expect("serialize");

        let mut target = new_book();
        let keep = new_id();
        target
            .add_limit_order_with_user(
                keep,
                100,
                1,
                Side::Sell,
                TimeInForce::Gtc,
                account(63),
                None,
            )
            .expect("pre-existing order");

        let result = target.restore_from_snapshot_json(&json);
        assert!(
            matches!(result, Err(OrderBookError::RiskMaxNotional { account: a, limit, .. }) if a == acct && limit == u128::MAX),
            "got {result:?}"
        );
        assert!(target.get_order(keep).is_some(), "live book untouched");
        assert!(target.risk_config().is_none(), "config untouched");
    }

    /// #243 review: a non-auto-replenishing reserve maker is removed once
    /// its visible tranche is exhausted and its hidden tranche is discarded
    /// (#230). The discarded remainder must be released from the maker's
    /// risk counters, or it stays booked and locks the account.
    #[test]
    fn discarded_reserve_remainder_is_released_from_risk() {
        use pricelevel::{OrderType, Price, Quantity, TimestampMs};

        let mut book = new_book();
        book.set_risk_config(
            RiskConfig::new()
                .with_max_open_orders_per_account(1)
                .with_max_notional_per_account(1_500),
        );
        let maker = account(64);
        let taker = account(65);

        let reserve = OrderType::ReserveOrder {
            id: new_id(),
            price: Price::new(100),
            visible_quantity: Quantity::new(5),
            hidden_quantity: Quantity::new(10),
            side: Side::Sell,
            user_id: maker,
            timestamp: TimestampMs::new(1_700_000_000_000),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(1),
            replenish_amount: None,
            auto_replenish: false,
            extra_fields: (),
        };
        let reserve_id = reserve.id();
        book.add_order(reserve)
            .expect("reserve maker rests (15 @ 100)");

        // Exhaust the visible tranche: the maker leaves the book and its
        // hidden 10 is discarded.
        book.add_limit_order_with_user(new_id(), 100, 5, Side::Buy, TimeInForce::Gtc, taker, None)
            .expect("taker");
        assert!(book.get_order(reserve_id).is_none(), "maker removed");

        // The account holds nothing: a full-size order must fit both the
        // open-order slot and the notional limit again.
        book.add_limit_order_with_user(
            new_id(),
            100,
            15,
            Side::Sell,
            TimeInForce::Gtc,
            maker,
            None,
        )
        .expect("maker account fully released");
        assert_eq!(book.risk_accounting_anomalies(), 0);
    }

    /// #243 review: concurrent submissions of the same id must leave the
    /// winner tracked by the risk layer; a loser can never release the
    /// winner's reservation.
    #[test]
    fn concurrent_same_id_submissions_keep_the_winner_tracked() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const THREADS: usize = 8;
        const ROUNDS: usize = 50;
        let acct = account(66);

        for round in 0..ROUNDS {
            let mut book = new_book();
            book.set_risk_config(RiskConfig::new().with_max_open_orders_per_account(2));
            let book = Arc::new(book);
            let id = new_id();
            let barrier = Arc::new(Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let book = Arc::clone(&book);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        book.add_limit_order_with_user(
                            id,
                            100,
                            1,
                            Side::Buy,
                            TimeInForce::Gtc,
                            acct,
                            None,
                        )
                        .is_ok()
                    })
                })
                .collect();
            let admitted = handles
                .into_iter()
                .map(|h| h.join().expect("submit thread"))
                .filter(|ok| *ok)
                .count();
            assert_eq!(admitted, 1, "round {round}: exactly one submission rests");
            assert!(book.get_order(id).is_some());

            // The winner holds exactly one slot: one more fits, a third
            // does not.
            book.add_limit_order_with_user(
                new_id(),
                99,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None,
            )
            .expect("second slot free");
            assert!(
                matches!(
                    book.add_limit_order_with_user(
                        new_id(),
                        98,
                        1,
                        Side::Buy,
                        TimeInForce::Gtc,
                        acct,
                        None
                    ),
                    Err(OrderBookError::RiskMaxOpenOrders { current: 2, .. })
                ),
                "round {round}: winner still counted"
            );

            // Cancelling the winner releases its slot.
            assert!(book.cancel_order(id).expect("cancel").is_some());
            book.add_limit_order_with_user(
                new_id(),
                97,
                1,
                Side::Buy,
                TimeInForce::Gtc,
                acct,
                None,
            )
            .expect("winner's slot released on cancel");
            assert_eq!(book.risk_accounting_anomalies(), 0, "round {round}");
        }
    }
}
