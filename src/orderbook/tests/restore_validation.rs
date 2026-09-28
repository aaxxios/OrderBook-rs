//! #250: counters that never wrap, and snapshot restore treated as untrusted
//! input.
//!
//! - `engine_seq` is minted with checked arithmetic: `next_engine_seq`
//!   returns `EngineSeqExhausted` at `u64::MAX`, the engine's emission paths
//!   suppress (and latch) instead of stamping a wrapped sequence, and a
//!   package carrying `engine_seq == u64::MAX` is rejected.
//! - Restore rejects a crossed or locked book (`SnapshotCrossed`) and an
//!   order whose `visible + hidden` does not fit `u64`, before any live
//!   state is touched.
//! - `OrderBookSnapshotPackage::new` propagates a failed aggregate refresh
//!   instead of checksumming stale aggregates.
//! - `spread` / `spread_bps` report `None` for a crossed read instead of a
//!   clamped `0`.
//! - Engine defences that only a crossed / locked book can reach (the
//!   residual-headroom pre-check, trailing stops resting inside the market)
//!   keep their regression coverage through the `cfg(test)`-only
//!   `restore_crossed_snapshot_for_test` hook, since the public restore now
//!   refuses such books.

#[cfg(test)]
mod tests {
    use crate::orderbook::book_change_event::PriceLevelChangedEvent;
    use crate::orderbook::snapshot::OrderBookSnapshotPackage;
    use crate::orderbook::trade::TradeResult;
    use crate::{OrderBook, OrderBookError, OrderBookSnapshot};
    use pricelevel::{
        Hash32, Id, OrderType, Price, PriceLevel, PriceLevelSnapshot, Quantity, Side, TimeInForce,
        TimestampMs,
    };
    use std::sync::{Arc, Mutex};

    const TS: u64 = 1_700_000_000_000;

    fn standard(id: u64, price: u128, quantity: u64, side: Side) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(id),
            price: Price::new(price),
            quantity: Quantity::new(quantity),
            side,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(TS),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// A one-order level snapshot built through pricelevel's own admission.
    fn level(id: u64, price: u128, quantity: u64, side: Side) -> PriceLevelSnapshot {
        let level = PriceLevel::new(price);
        assert!(
            level.add_order(standard(id, price, quantity, side)).is_ok(),
            "fixture level admits order {id}"
        );
        level.snapshot().expect("level snapshot")
    }

    fn snapshot(
        symbol: &str,
        bids: Vec<PriceLevelSnapshot>,
        asks: Vec<PriceLevelSnapshot>,
    ) -> OrderBookSnapshot {
        OrderBookSnapshot {
            symbol: symbol.to_string(),
            timestamp: TS,
            bids,
            asks,
        }
    }

    /// Seed a live, uncrossed book: bid 10_000, ask 10_100.
    fn populate(book: &OrderBook<()>) {
        book.add_limit_order(
            Id::from_u64(1),
            10_000,
            5,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        )
        .expect("add bid");
        book.add_limit_order(
            Id::from_u64(2),
            10_100,
            4,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("add ask");
    }

    /// Level state (orders and statistics) for byte-level before / after
    /// comparison; the top-level wall-clock timestamp is excluded.
    fn level_state_json(book: &OrderBook<()>) -> (String, String) {
        let snapshot = book.create_snapshot(usize::MAX).expect("snapshot");
        let bids = serde_json::to_string(&snapshot.bids).expect("serialize bids");
        let asks = serde_json::to_string(&snapshot.asks).expect("serialize asks");
        (bids, asks)
    }

    /// Everything a failed restore must leave untouched.
    fn assert_untouched(book: &OrderBook<()>, before: &(String, String), engine_seq: u64) {
        assert_eq!(&level_state_json(book), before, "levels byte-identical");
        assert_eq!(book.best_bid(), Some(10_000), "best bid untouched");
        assert_eq!(book.best_ask(), Some(10_100), "best ask untouched");
        assert!(book.get_order(Id::from_u64(1)).is_some(), "order 1 indexed");
        assert!(book.get_order(Id::from_u64(2)).is_some(), "order 2 indexed");
        assert_eq!(book.engine_seq(), engine_seq, "engine_seq untouched");
        assert!(!book.is_kill_switch_engaged(), "kill switch untouched");
        assert!(book.risk_config().is_none(), "risk config untouched");
    }

    /// Re-seal a package whose snapshot was replaced, so the checksum is
    /// valid and the restore's own validation is what gets exercised.
    fn reseal(package: &mut OrderBookSnapshotPackage) {
        package.checksum = String::new();
        match package.validate() {
            Err(OrderBookError::ChecksumMismatch { actual, .. }) => package.checksum = actual,
            other => panic!("expected a checksum mismatch to reseal, got {other:?}"),
        }
        assert!(package.validate().is_ok(), "resealed package validates");
    }

    /// A bid level holding one iceberg whose `visible + hidden` is exactly
    /// `u64::MAX`, then edited through serde so hidden is one more: a level
    /// snapshot pricelevel's own constructors could never produce.
    fn overflowing_level() -> PriceLevelSnapshot {
        let level = PriceLevel::new(9_000);
        let admitted = level.add_order(OrderType::IcebergOrder {
            id: Id::from_u64(77),
            price: Price::new(9_000),
            visible_quantity: Quantity::new(u64::MAX - 10),
            hidden_quantity: Quantity::new(10),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(TS),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        });
        assert!(admitted.is_ok(), "the representable iceberg is admitted");
        let snapshot = level.snapshot().expect("level snapshot");
        let mut value = serde_json::to_value(&snapshot).expect("level to json");
        let mut edited = false;
        bump_hidden(&mut value, &mut edited);
        assert!(edited, "the fixture edited the hidden tranche");
        serde_json::from_value(value).expect("edited level deserializes")
    }

    fn bump_hidden(value: &mut serde_json::Value, edited: &mut bool) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, inner) in map.iter_mut() {
                    if key == "hidden_quantity" && inner.as_u64() == Some(10) {
                        *inner = serde_json::Value::from(11u64);
                        *edited = true;
                    } else {
                        bump_hidden(inner, edited);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for inner in items {
                    bump_hidden(inner, edited);
                }
            }
            _ => {}
        }
    }

    // ---- crossed / locked books ----------------------------------------

    #[test]
    fn direct_restore_rejects_crossed_book_and_leaves_book_untouched() {
        let book: OrderBook<()> = OrderBook::new("CROSS");
        populate(&book);
        let before = level_state_json(&book);

        let crossed = snapshot(
            "CROSS",
            vec![level(10, 110, 5, Side::Buy)],
            vec![level(11, 100, 5, Side::Sell)],
        );
        let err = book
            .restore_from_snapshot(crossed)
            .expect_err("a crossed snapshot must be rejected");
        assert!(
            matches!(
                err,
                OrderBookError::SnapshotCrossed {
                    best_bid: 110,
                    best_ask: 100
                }
            ),
            "expected SnapshotCrossed {{ 110, 100 }}, got {err:?}"
        );
        assert_untouched(&book, &before, 0);
        assert!(book.get_order(Id::from_u64(10)).is_none(), "nothing leaked");
    }

    #[test]
    fn package_restore_rejects_locked_book_and_leaves_book_untouched() {
        let mut book: OrderBook<()> = OrderBook::new("LOCK");
        populate(&book);
        for _ in 0..3 {
            book.next_engine_seq().expect("mint");
        }
        let seq = book.engine_seq();
        let before = level_state_json(&book);

        let locked = snapshot(
            "LOCK",
            vec![level(10, 100, 5, Side::Buy)],
            vec![level(11, 100, 5, Side::Sell)],
        );
        let mut package =
            OrderBookSnapshotPackage::new(locked).expect("a locked snapshot still checksums");
        package.engine_seq = 999;
        package.kill_switch_engaged = true;
        package.risk_config = Some(crate::orderbook::risk::RiskConfig::default());

        let err = book
            .restore_from_snapshot_package(package)
            .expect_err("a locked snapshot must be rejected");
        assert!(
            matches!(
                err,
                OrderBookError::SnapshotCrossed {
                    best_bid: 100,
                    best_ask: 100
                }
            ),
            "expected SnapshotCrossed {{ 100, 100 }}, got {err:?}"
        );
        assert_untouched(&book, &before, seq);
    }

    #[test]
    fn one_sided_and_uncrossed_snapshots_still_restore() {
        let book: OrderBook<()> = OrderBook::new("ONE");
        book.restore_from_snapshot(snapshot("ONE", vec![level(10, 100, 5, Side::Buy)], vec![]))
            .expect("bid-only snapshot restores");
        assert_eq!(book.best_bid(), Some(100));

        book.restore_from_snapshot(snapshot(
            "ONE",
            vec![level(10, 100, 5, Side::Buy)],
            vec![level(11, 101, 5, Side::Sell)],
        ))
        .expect("a one-tick spread restores");
        assert_eq!(book.spread(), Some(1));
    }

    // ---- engine_seq ------------------------------------------------------

    #[test]
    fn package_restore_rejects_engine_seq_at_max_and_leaves_book_untouched() {
        let mut book: OrderBook<()> = OrderBook::new("SEQ");
        populate(&book);
        let before = level_state_json(&book);

        let source: OrderBook<()> = OrderBook::new("SEQ");
        let mut package = source.create_snapshot_package(10).expect("package");
        package.engine_seq = u64::MAX;
        package.kill_switch_engaged = true;

        let err = book
            .restore_from_snapshot_package(package)
            .expect_err("an engine_seq that cannot advance must be rejected");
        assert!(
            matches!(
                err,
                OrderBookError::EngineSeqExhausted {
                    engine_seq: u64::MAX
                }
            ),
            "expected EngineSeqExhausted {{ u64::MAX }}, got {err:?}"
        );
        assert_untouched(&book, &before, 0);
    }

    #[test]
    fn next_engine_seq_mints_up_to_max_minus_one_then_refuses_without_wrapping() {
        let mut book: OrderBook<()> = OrderBook::new("SEQ2");
        let source: OrderBook<()> = OrderBook::new("SEQ2");
        let mut package = source.create_snapshot_package(10).expect("package");
        package.engine_seq = u64::MAX - 1;
        book.restore_from_snapshot_package(package)
            .expect("u64::MAX - 1 can still advance once");

        assert_eq!(book.next_engine_seq().expect("last mint"), u64::MAX - 1);
        assert_eq!(book.engine_seq(), u64::MAX);
        for _ in 0..2 {
            let err = book.next_engine_seq().expect_err("exhausted");
            assert!(
                matches!(
                    err,
                    OrderBookError::EngineSeqExhausted {
                        engine_seq: u64::MAX
                    }
                ),
                "expected EngineSeqExhausted, got {err:?}"
            );
            assert_eq!(book.engine_seq(), u64::MAX, "the counter never wraps");
        }
    }

    /// The engine's own emission paths run after the mutation, so an
    /// exhausted counter suppresses the event (latched, logged once) while
    /// the book keeps working; a restore with an advancing counter clears the
    /// latch.
    #[test]
    fn exhausted_engine_seq_suppresses_emission_but_not_the_mutation() {
        let mut book: OrderBook<()> = OrderBook::new("SEQ3");
        let level_events: Arc<Mutex<Vec<PriceLevelChangedEvent>>> =
            Arc::new(Mutex::new(Vec::new()));
        let trades: Arc<Mutex<Vec<TradeResult>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&level_events);
        book.set_price_level_listener(Arc::new(move |event| {
            sink.lock().expect("level sink").push(event);
        }));
        let sink = Arc::clone(&trades);
        book.set_trade_listener(Arc::new(move |trade: &TradeResult| {
            sink.lock().expect("trade sink").push(trade.clone());
        }));

        let source: OrderBook<()> = OrderBook::new("SEQ3");
        let mut package = source.create_snapshot_package(10).expect("package");
        package.engine_seq = u64::MAX - 1;
        book.restore_from_snapshot_package(package)
            .expect("restore near the ceiling");

        // Last mintable seq: the level event carries it.
        book.add_limit_order(Id::from_u64(1), 100, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("rest ask");
        assert_eq!(
            level_events
                .lock()
                .expect("level sink")
                .iter()
                .map(|event| event.engine_seq)
                .collect::<Vec<_>>(),
            vec![u64::MAX - 1]
        );
        assert!(!book.engine_seq_exhausted());

        // Exhausted: the order still rests, no event is emitted.
        book.add_limit_order(Id::from_u64(2), 101, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("rest second ask");
        assert!(
            book.get_order(Id::from_u64(2)).is_some(),
            "mutation applied"
        );
        assert_eq!(level_events.lock().expect("level sink").len(), 1);
        assert!(book.engine_seq_exhausted(), "exhaustion latched");

        // A crossing submit still trades; its TradeResult is suppressed.
        book.add_limit_order(Id::from_u64(3), 100, 5, Side::Buy, TimeInForce::Gtc, None)
            .expect("crossing buy");
        assert!(book.get_order(Id::from_u64(1)).is_none(), "maker filled");
        assert_eq!(book.last_trade_price(), Some(100), "trade happened");
        assert!(
            trades.lock().expect("trade sink").is_empty(),
            "no stamped trade"
        );
        assert_eq!(level_events.lock().expect("level sink").len(), 1);
        assert_eq!(book.engine_seq(), u64::MAX, "the counter never wrapped");

        // A restore with an advancing counter clears the latch.
        let fresh = source.create_snapshot_package(10).expect("package");
        book.restore_from_snapshot_package(fresh)
            .expect("restore fresh counter");
        assert!(!book.engine_seq_exhausted(), "latch cleared by restore");
        assert_eq!(book.next_engine_seq().expect("mint"), 0);
    }

    /// Book restored with `engine_seq` at `u64::MAX`-1, the last seq burnt,
    /// a resting ask of 5 @ 100 and (optionally) a trade listener.
    fn exhausted_book_with_ask(symbol: &str, trades: Option<Arc<Mutex<Vec<TradeResult>>>>) -> OrderBook<()> {
        let mut book: OrderBook<()> = OrderBook::new(symbol);
        if let Some(sink) = trades {
            book.set_trade_listener(Arc::new(move |trade: &TradeResult| {
                sink.lock().expect("trade sink").push(trade.clone());
            }));
        }
        let source: OrderBook<()> = OrderBook::new(symbol);
        let mut package = source.create_snapshot_package(10).expect("package");
        package.engine_seq = u64::MAX - 1;
        book.restore_from_snapshot_package(package)
            .expect("restore near the ceiling");
        book.next_engine_seq().expect("burn the last seq");
        book.add_limit_order(Id::from_u64(1), 100, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("rest ask");
        book
    }

    /// PR #287 review: event stamping never affects the caller-owned
    /// result. With `engine_seq` exhausted, `add_order_with_result` still
    /// returns its fills (stamped `UNSTAMPED_ENGINE_SEQ`) while the
    /// listener event is suppressed.
    #[test]
    fn exhausted_engine_seq_still_returns_fills_to_add_order_with_result() {
        let trades: Arc<Mutex<Vec<TradeResult>>> = Arc::new(Mutex::new(Vec::new()));
        let book = exhausted_book_with_ask("SEQR", Some(Arc::clone(&trades)));

        let (_, result) = book
            .add_order_with_result(standard(2, 100, 5, Side::Buy))
            .expect("crossing buy");
        let result = result.expect("the committed fills are returned");
        assert_eq!(result.match_result.trades().len(), 1, "one fill");
        assert_eq!(result.engine_seq, crate::UNSTAMPED_ENGINE_SEQ);
        assert!(trades.lock().expect("trade sink").is_empty(), "listener suppressed");
        assert!(book.engine_seq_exhausted());
        assert!(book.get_order(Id::from_u64(1)).is_none(), "maker filled");
    }

    /// Same guarantee on the `*_with_committed` failure path: an IOC whose
    /// remainder cannot rest fails after real fills, and those fills are
    /// still handed back with the error. No listener installed.
    #[test]
    fn exhausted_engine_seq_still_returns_committed_fills_on_failure() {
        let book = exhausted_book_with_ask("SEQC", None);
        let taker = OrderType::Standard {
            id: Id::from_u64(2),
            price: Price::new(100),
            quantity: Quantity::new(10),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(TS),
            time_in_force: TimeInForce::Ioc,
            extra_fields: (),
        };
        match book.add_order_with_committed(taker) {
            Err(failure) => {
                let committed = failure.committed.expect("committed fills returned");
                assert_eq!(committed.match_result.trades().len(), 1);
                assert_eq!(committed.engine_seq, crate::UNSTAMPED_ENGINE_SEQ);
            }
            Ok((_, Some(result))) => {
                assert_eq!(result.match_result.trades().len(), 1);
                assert_eq!(result.engine_seq, crate::UNSTAMPED_ENGINE_SEQ);
            }
            Ok((_, None)) => panic!("the committed fills were lost"),
        }
        assert!(book.get_order(Id::from_u64(1)).is_none(), "maker filled");
    }

    // ---- tranche representability / aggregate refresh ------------------

    #[test]
    fn package_new_propagates_aggregate_refresh_failure() {
        let malformed = snapshot("OVF", vec![overflowing_level()], vec![]);
        let err = OrderBookSnapshotPackage::new(malformed)
            .expect_err("stale aggregates must not be checksummed");
        assert!(
            matches!(err, OrderBookError::PriceLevelError(_)),
            "expected PriceLevelError, got {err:?}"
        );

        let mut malformed = snapshot("OVF", vec![overflowing_level()], vec![]);
        assert!(
            malformed.refresh_aggregates().is_err(),
            "refresh_aggregates reports the overflow"
        );
    }

    #[test]
    fn restore_rejects_tranche_overflow_and_leaves_book_untouched() {
        let mut book: OrderBook<()> = OrderBook::new("OVF");
        populate(&book);
        let before = level_state_json(&book);

        // Direct path.
        let err = book
            .restore_from_snapshot(snapshot("OVF", vec![overflowing_level()], vec![]))
            .expect_err("an unrepresentable tranche must be rejected");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(_) | OrderBookError::QuantityOverflow { .. }
            ),
            "expected a typed overflow rejection, got {err:?}"
        );
        assert_untouched(&book, &before, 0);

        // Package path, with a valid checksum over the malformed payload.
        let source: OrderBook<()> = OrderBook::new("OVF");
        let mut package = source.create_snapshot_package(10).expect("package");
        package.snapshot = snapshot("OVF", vec![overflowing_level()], vec![]);
        package.risk_config = Some(crate::orderbook::risk::RiskConfig::default());
        reseal(&mut package);
        let err = book
            .restore_from_snapshot_package(package)
            .expect_err("an unrepresentable tranche must be rejected");
        assert!(
            matches!(
                err,
                OrderBookError::PriceLevelError(_) | OrderBookError::QuantityOverflow { .. }
            ),
            "expected a typed overflow rejection, got {err:?}"
        );
        assert_untouched(&book, &before, 0);
        assert!(book.get_order(Id::from_u64(77)).is_none(), "nothing leaked");
    }

    // ---- tick / lot: not enforced on restore ---------------------------

    /// A live book keeps orders admitted under a previous lot / tick size
    /// (documented on `set_lot_size`), so its snapshot must still restore.
    #[test]
    fn snapshot_of_book_with_orders_predating_a_lot_and_tick_change_restores() {
        let mut original: OrderBook<()> = OrderBook::new("LEGACY");
        populate(&original);
        original.set_lot_size(3);
        original.set_tick_size(7);

        let package = original.create_snapshot_package(10).expect("package");
        let mut restored: OrderBook<()> = OrderBook::new("LEGACY");
        restored
            .restore_from_snapshot_package(package)
            .expect("legacy-aligned orders round-trip");
        assert_eq!(level_state_json(&restored), level_state_json(&original));
        assert_eq!(restored.lot_size(), Some(3));
        assert_eq!(restored.tick_size(), Some(7));
    }

    // ---- spread ----------------------------------------------------------

    #[test]
    fn spread_of_crossed_book_is_none_and_locked_is_zero() {
        let crossed_snapshot = snapshot(
            "SPR",
            vec![level(10, 110, 5, Side::Buy)],
            vec![level(11, 100, 5, Side::Sell)],
        );
        assert_eq!(crossed_snapshot.spread(), None, "crossed snapshot");

        let book: OrderBook<()> = OrderBook::new("SPR");
        book.restore_crossed_snapshot_for_test(crossed_snapshot)
            .expect("test hook installs a crossed book");
        assert_eq!(book.spread(), None, "crossed book has no spread");
        assert_eq!(book.spread_bps(None), None, "nor a bps spread");

        let locked = snapshot(
            "SPR",
            vec![level(10, 100, 5, Side::Buy)],
            vec![level(11, 100, 5, Side::Sell)],
        );
        assert_eq!(locked.spread(), Some(0), "locked snapshot");
        book.restore_crossed_snapshot_for_test(locked)
            .expect("test hook installs a locked book");
        assert_eq!(book.spread(), Some(0), "locked book");
    }

    // ---- defences only a crossed / locked book reaches -----------------

    /// Migrated from `tests/unit/mutation_failure_atomicity_tests.rs`
    /// (#211), whose fixture — a near-capacity bid level AND a crossing ask
    /// at the same price — is a locked book the public restore now rejects
    /// (#250). A buy whose residual would overflow the same-side level's
    /// aggregate is rejected before any trade: the crossing ask stays fully
    /// intact.
    #[test]
    fn residual_headroom_rejects_before_any_trade() {
        let book: OrderBook<()> = OrderBook::new("HEAD");
        book.restore_crossed_snapshot_for_test(snapshot(
            "HEAD",
            vec![level(1, 100, u64::MAX - 2, Side::Buy)],
            vec![level(2, 100, 5, Side::Sell)],
        ))
        .expect("restore locked book");

        // Buy 10 @ 100 would fill 5 from the ask, then rest 5 into the bid
        // level — whose aggregate (u64::MAX - 2) cannot absorb it. The
        // headroom pre-check must reject BEFORE the fill happens.
        let err = book
            .add_limit_order(Id::from_u64(3), 100, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect_err("residual could not rest; submit must be rejected pre-trade");
        assert!(
            matches!(err, OrderBookError::InvalidOperation { .. }),
            "expected the typed capacity rejection, got {err:?}"
        );

        // Zero trades: the crossing ask is untouched and no last trade exists.
        assert!(book.last_trade_price().is_none(), "no trade was emitted");
        let ask = book.get_order(Id::from_u64(2)).expect("ask still resting");
        assert_eq!(ask.visible_quantity().as_u64(), 5, "ask fully intact");
        assert!(
            book.get_order(Id::from_u64(3)).is_none(),
            "rejected taker never rests"
        );
    }

    /// Migrated from `tests/unit/special_order_restore_tests.rs` (#194). A
    /// trailing stop only re-prices when its stop price sits inside the
    /// market (a Sell stop below the bid) — a crossed book, which the live
    /// matching path never builds and the public restore now rejects (#250).
    /// The restore-side re-registration is what #194 pins; the crossed
    /// fixture is installed through the test hook.
    #[cfg(feature = "special_orders")]
    #[test]
    fn restore_reregisters_trailing_stop_and_reprices_issue_194() {
        use crate::orderbook::repricing::RepricingOperations;

        let stop_id = Id::from_u64(2000);
        let stop = PriceLevel::new(100);
        let admitted = stop.add_order(OrderType::TrailingStop {
            id: stop_id,
            price: Price::new(100),
            quantity: Quantity::new(5),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(1),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(5),
            last_reference_price: Price::new(90),
            extra_fields: (),
        });
        assert!(admitted.is_ok(), "fixture level admits the stop");

        let restored: OrderBook<()> = OrderBook::new("TS/USD");
        restored
            .restore_crossed_snapshot_for_test(snapshot(
                "TS/USD",
                vec![level(1, 110, 10, Side::Buy)],
                vec![stop.snapshot().expect("stop level snapshot")],
            ))
            .expect("restore crossed book");

        assert_eq!(restored.trailing_stop_count(), 1, "stop re-registered");
        assert_eq!(restored.trailing_stop_ids(), vec![stop_id]);
        assert_eq!(restored.pegged_order_count(), 0, "no pegged orders");
        assert_eq!(restored.best_bid(), Some(110), "bid liquidity restored");
        let before = restored.get_order(stop_id).expect("stop restored");
        assert_eq!(before.price().as_u128(), 100);

        // best_bid 110 > watermark 90: the stop trails to 105, inside the
        // bid, so the validate-first re-add matches it against the bid.
        let repriced = restored
            .reprice_trailing_stops()
            .expect("reprice runs on the restored book");
        assert_eq!(repriced, 1, "the restored trailing stop re-prices");
        assert!(
            restored.get_order(stop_id).is_none(),
            "the trailing stop trailed into the market and triggered"
        );
        assert_eq!(restored.best_bid(), Some(110), "bid still best");
    }

    /// The public restore refuses the crossed #194 fixture.
    #[test]
    fn public_restore_refuses_trailing_stop_inside_the_market() {
        let stop = PriceLevel::new(100);
        let admitted = stop.add_order(OrderType::TrailingStop {
            id: Id::from_u64(2000),
            price: Price::new(100),
            quantity: Quantity::new(5),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(1),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(5),
            last_reference_price: Price::new(90),
            extra_fields: (),
        });
        assert!(admitted.is_ok(), "fixture level admits the stop");
        let book: OrderBook<()> = OrderBook::new("TS/USD");
        let err = book
            .restore_from_snapshot(snapshot(
                "TS/USD",
                vec![level(1, 110, 10, Side::Buy)],
                vec![stop.snapshot().expect("stop level snapshot")],
            ))
            .expect_err("crossed");
        assert!(
            matches!(
                err,
                OrderBookError::SnapshotCrossed {
                    best_bid: 110,
                    best_ask: 100
                }
            ),
            "got {err:?}"
        );
        assert!(book.best_bid().is_none(), "book untouched");
    }

    // ---- strandable-maker count ----------------------------------------

    #[test]
    fn strandable_count_decrement_at_zero_is_refused_not_wrapped() {
        use std::sync::atomic::Ordering;
        let book: OrderBook<()> = OrderBook::new("STR");
        assert_eq!(book.strandable_makers_resting.load(Ordering::Relaxed), 0);
        book.note_removed_strandable_maker();
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            0,
            "an unmatched decrement leaves the count at zero"
        );
    }
}
