//! Issue #241: pricelevel 0.10 level statistics have a single-writer
//! contract, while takers on the shared submit gate may sweep one level
//! concurrently. The documented contract (see `OrderBook`'s "Level
//! statistics are advisory under concurrent takers") is pinned here with
//! assertions that hold in **every** interleaving, so the test cannot flake:
//!
//! - trades and quantities are exact regardless of concurrency;
//! - a snapshot taken mid-sweep is internally valid for everything that is
//!   not an execution aggregate, and each execution aggregate is bounded by
//!   its final total and never moves backwards between successive snapshots
//!   (it may lag or be torn across fields, which is not asserted either way);
//! - once the sweeps return, the aggregates are exact, and the book is
//!   `snapshots_match`-equal to a book that executed the same fills on one
//!   thread (what replay does).

#[cfg(test)]
mod tests {
    use orderbook_rs::orderbook::sequencer::snapshots_match;
    use orderbook_rs::{DefaultOrderBook, OrderBook};
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    const PRICE: u128 = 100;
    const MAKERS: u64 = 4_000;
    const TAKERS: u64 = 4;
    const FILLS_PER_TAKER: u64 = 500;
    /// Every taker call buys one unit from a one-unit maker: one trade.
    const TOTAL_FILLS: u64 = TAKERS * FILLS_PER_TAKER;

    /// Seeds `MAKERS` one-unit asks at `PRICE` with fixed ids and
    /// timestamps, so two independently seeded books are identical.
    fn seeded_book() -> OrderBook<()> {
        let book: OrderBook<()> = DefaultOrderBook::new("STATS241");
        for n in 1..=MAKERS {
            book.add_order(OrderType::Standard {
                id: Id::from_u64(n),
                price: Price::new(PRICE),
                quantity: Quantity::new(1),
                side: Side::Sell,
                time_in_force: TimeInForce::Gtc,
                user_id: Hash32::zero(),
                timestamp: TimestampMs::new(0),
                extra_fields: (),
            })
            .expect("seed ask");
        }
        book
    }

    /// Execution aggregates of the single ask level, in a fixed order.
    fn exec_stats(snapshot: &orderbook_rs::OrderBookSnapshot) -> (usize, u64, u128) {
        let level = snapshot.asks.first().expect("the ask level never empties");
        let stats = level.statistics();
        (
            stats.orders_executed(),
            stats.quantity_executed(),
            stats.value_executed(),
        )
    }

    #[test]
    fn level_statistics_are_advisory_during_and_exact_after_concurrent_sweeps() {
        let live = Arc::new(seeded_book());
        let running = Arc::new(AtomicBool::new(true));
        // Number of snapshots the reader captured; takers pause half-way
        // until at least one exists, so the test always observes the book
        // between the first and the last fill.
        let captures = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(usize::try_from(TAKERS).expect("small") + 1));

        let mut takers = Vec::new();
        for t in 0..TAKERS {
            let book = Arc::clone(&live);
            let barrier = Arc::clone(&barrier);
            let captures = Arc::clone(&captures);
            takers.push(thread::spawn(move || {
                barrier.wait();
                let mut executed = 0u64;
                let mut trades = 0usize;
                for i in 0..FILLS_PER_TAKER {
                    if i == FILLS_PER_TAKER / 2 {
                        while captures.load(Ordering::Acquire) == 0 {
                            thread::yield_now();
                        }
                    }
                    // Anonymous sweep: shared side of the submit gate, so
                    // takers overlap on the same level.
                    let result = book
                        .match_order(
                            Id::from_u64(1_000_000 + t * FILLS_PER_TAKER + i),
                            Side::Buy,
                            1,
                            Some(PRICE),
                        )
                        .expect("sweep against ample depth");
                    executed += result.executed_quantity().expect("executed").as_u64();
                    trades += result.trades().as_vec().len();
                }
                (executed, trades)
            }));
        }

        let reader_book = Arc::clone(&live);
        let reader_running = Arc::clone(&running);
        let reader_barrier = Arc::clone(&barrier);
        let reader_captures = Arc::clone(&captures);
        let reader = thread::spawn(move || {
            reader_barrier.wait();
            let mut captured = Vec::new();
            while reader_running.load(Ordering::Acquire) {
                // A level walk racing a sweep may exhaust pricelevel's
                // bounded recollection; that is a typed error, not a torn
                // snapshot, and is simply skipped here.
                if let Ok(snapshot) = reader_book.create_snapshot(usize::MAX) {
                    let level = snapshot.asks.first().expect("ask level").clone();
                    captured.push((exec_stats(&snapshot), level));
                    reader_captures.fetch_add(1, Ordering::Release);
                }
            }
            captured
        });

        let mut executed = 0u64;
        let mut trades = 0usize;
        for handle in takers {
            let (e, t) = handle.join().expect("taker thread");
            executed += e;
            trades += t;
        }
        running.store(false, Ordering::Release);
        let captured = reader.join().expect("reader thread");
        assert!(
            !captured.is_empty(),
            "the reader must observe the book before the sweeps complete"
        );

        // Trades are exact regardless of concurrency.
        assert_eq!(executed, TOTAL_FILLS);
        assert_eq!(trades, usize::try_from(TOTAL_FILLS).expect("small"));

        // Quiescent: the aggregates are the true totals.
        let after = live
            .create_snapshot(usize::MAX)
            .expect("quiescent snapshot");
        let final_stats = exec_stats(&after);
        assert_eq!(
            final_stats,
            (
                usize::try_from(TOTAL_FILLS).expect("small"),
                TOTAL_FILLS,
                u128::from(TOTAL_FILLS) * PRICE,
            )
        );
        let level = after.asks.first().expect("ask level");
        assert_eq!(level.visible_quantity().as_u64(), MAKERS - TOTAL_FILLS);
        assert!(!level.statistics().stats_degraded());

        // Mid-sweep snapshots: the non-statistics state is coherent; each
        // aggregate is bounded by its final total and monotone across this
        // reader's successive captures. Cross-field agreement is NOT
        // asserted: that is exactly what is advisory.
        let mut previous = (0usize, 0u64, 0u128);
        let mut previous_visible = MAKERS;
        for (stats, level) in &captured {
            let visible: u64 = level
                .orders()
                .iter()
                .map(|order| order.visible_quantity().as_u64())
                .sum();
            assert_eq!(visible, level.visible_quantity().as_u64());
            assert_eq!(level.orders().len(), level.order_count());
            assert!(visible <= previous_visible, "resting depth grew");
            previous_visible = visible;

            assert!(stats.0 <= final_stats.0 && stats.1 <= final_stats.1);
            assert!(stats.2 <= final_stats.2);
            assert!(stats.0 >= previous.0 && stats.1 >= previous.1 && stats.2 >= previous.2);
            previous = *stats;
        }

        // Replay parity: a book that executed the same fills on one thread
        // (one writer per level, as replay) compares equal, statistics
        // included, to the quiescent live book.
        let serial = seeded_book();
        for i in 0..TOTAL_FILLS {
            serial
                .match_order(Id::from_u64(2_000_000 + i), Side::Buy, 1, Some(PRICE))
                .expect("serial sweep");
        }
        let serial_snapshot = serial.create_snapshot(usize::MAX).expect("snapshot");
        assert!(snapshots_match(&after, &serial_snapshot));
    }
}
