//! #247 (Copilot on #285): removing a price level that became empty must
//! never unlink an order a concurrent submit admitted into it.
//!
//! Every removal of an emptied level (single-order cancel, `UpdateQuantity`,
//! the sweep's drain, a failed rest's cleanup) and every admission into a
//! level run under the price's stripe of `OrderBook::level_locks`. These
//! stress tests hammer one price with admissions on the shared submit gate
//! while other threads keep emptying the level, then check that the book's
//! indices and its level maps agree: every located order is reachable
//! through `bids` / `asks`, and every order resting on a level is located.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::{Arc, Barrier};
    use std::thread;

    const PRICE: u128 = 100;
    const ROUNDS: u64 = 6_000;
    const ADDERS: u64 = 3;

    fn sell(raw: u64) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(raw),
            price: Price::new(PRICE),
            quantity: Quantity::new(1),
            side: Side::Sell,
            time_in_force: TimeInForce::Gtc,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    fn buy(raw: u64) -> OrderType<()> {
        OrderType::Standard {
            id: Id::from_u64(raw),
            price: Price::new(PRICE),
            quantity: Quantity::new(1),
            side: Side::Buy,
            time_in_force: TimeInForce::Ioc,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            extra_fields: (),
        }
    }

    /// Every located order is reachable through its level, and every order
    /// resting on a level is located: no orphan in either direction.
    fn assert_indices_agree(book: &OrderBook<()>) {
        let mut resting = 0usize;
        for side in [&book.bids, &book.asks] {
            for entry in side.iter() {
                for order in entry.value().iter_orders() {
                    resting = resting.checked_add(1).expect("count fits");
                    assert_eq!(
                        book.order_locations.get(&order.id()).map(|loc| loc.price),
                        Some(*entry.key()),
                        "order {} rests on a level but is not located there",
                        order.id()
                    );
                }
            }
        }
        for location in book.order_locations.iter() {
            assert!(
                book.get_order(*location.key()).is_some(),
                "order {} is located but unreachable through the level maps",
                location.key()
            );
        }
        assert_eq!(
            resting,
            book.order_locations.len(),
            "level and index counts"
        );
    }

    /// Deterministic half of the contract: a remover acting on a stale
    /// "this level is empty" observation must not unlink the level once an
    /// admission refilled it; the check is re-run under the stripe.
    #[test]
    fn test_stale_empty_observation_does_not_remove_a_refilled_level() {
        let book = OrderBook::<()>::new("RACE-STALE");
        book.add_order(sell(1)).expect("rest");
        book.cancel_order(Id::from_u64(1)).expect("cancel");
        assert!(book.asks.is_empty(), "the emptied level was removed");

        // Recreate the level, observe it empty, then refill it before the
        // removal runs: the stale observation must not unlink it.
        book.add_order(sell(2)).expect("rest");
        let level = book.asks.get(&PRICE).map(|entry| Arc::clone(entry.value()));
        book.cancel_order(Id::from_u64(2)).expect("cancel");
        assert!(level.is_some_and(|level| level.order_count() == 0));
        book.add_order(sell(3)).expect("refill");
        assert!(
            !book.remove_level_if_empty(Side::Sell, PRICE),
            "refilled level kept"
        );
        assert!(book.get_order(Id::from_u64(3)).is_some());
        assert_indices_agree(&book);

        // An empty level is removed, and removing an absent one is a no-op.
        book.cancel_order(Id::from_u64(3)).expect("cancel");
        assert!(
            !book.remove_level_if_empty(Side::Sell, PRICE),
            "already removed"
        );
        assert!(book.asks.is_empty());
    }

    /// Emptier cancels its own resting order at the price; adders admit
    /// orders at the same price that are never cancelled. Each adder order
    /// must end resting and reachable.
    #[test]
    fn test_cancel_emptying_a_level_never_unlinks_a_concurrent_admission() {
        let book = Arc::new(OrderBook::<()>::new("RACE-CANCEL"));
        let barrier = Arc::new(Barrier::new(usize::try_from(ADDERS + 1).expect("small")));
        let mut workers = Vec::new();
        {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS {
                    let raw = 10_000_000 + round;
                    book.add_order(sell(raw)).expect("emptier rests");
                    book.cancel_order(Id::from_u64(raw))
                        .expect("emptier cancels");
                }
            }));
        }
        for adder in 0..ADDERS {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS / 4 {
                    book.add_order(sell(adder * 1_000_000 + round))
                        .expect("adder rests");
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }

        for adder in 0..ADDERS {
            for round in 0..ROUNDS / 4 {
                let order_id = Id::from_u64(adder * 1_000_000 + round);
                assert!(book.get_order(order_id).is_some(), "{order_id} reachable");
            }
        }
        assert_indices_agree(&book);
    }

    /// Sweeps empty the level through the matching drain while adders keep
    /// admitting at the same price. Whatever the interleaving (an adder's
    /// order may itself be consumed by a later sweep), the indices and the
    /// level maps must agree at the end.
    #[test]
    fn test_sweep_drain_never_unlinks_a_concurrent_admission() {
        let book = Arc::new(OrderBook::<()>::new("RACE-SWEEP"));
        let barrier = Arc::new(Barrier::new(usize::try_from(ADDERS + 1).expect("small")));
        let mut workers = Vec::new();
        {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS {
                    // An IOC buy that finds no ask is simply cancelled.
                    let _ = book.add_order(buy(20_000_000 + round));
                }
            }));
        }
        for adder in 0..ADDERS {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                for round in 0..ROUNDS / 2 {
                    book.add_order(sell(adder * 1_000_000 + round))
                        .expect("adder rests");
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }
        assert_indices_agree(&book);
    }
}
