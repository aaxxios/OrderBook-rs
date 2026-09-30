//! #286: workloads on a book holding pending trailing stops
//! (`special_orders`).
//!
//! Ten pending sell stops sit far below the market (stop 500, watermark
//! 1000, trail 500) after a trade at 1000, so the measured orders neither
//! trail nor elect them: the rows measure what a book pays while stops are
//! pending (the exclusive submit gate and one evaluation pass per mutating
//! call), against `match_market_against_limit` / `add_limit_orders`.
//!
//! Registered in its own group, last, so the shared rows run under the same
//! conditions with and without this group (the comparison against main
//! runs the same sequence of shared benches on both sides).

use criterion::{BatchSize, Criterion};
use orderbook_rs::OrderBook;
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
use std::hint::black_box;

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Rests `count` sell limits of 10 at 1000.
fn with_asks(order_book: OrderBook, count: u64) -> OrderBook {
    for _ in 0..count {
        order_book
            .add_limit_order(new_id(), 1000, 10, Side::Sell, TimeInForce::Gtc, None)
            .unwrap();
    }
    order_book
}

/// Trades once at 1000, then adds `count` pending sell stops that later
/// trades at 1000 neither trail nor elect.
fn with_pending_stops(order_book: OrderBook, count: u64) -> OrderBook {
    let _ = order_book.submit_market_order(new_id(), 1, Side::Buy);
    for _ in 0..count {
        order_book
            .add_order(OrderType::TrailingStop {
                id: new_id(),
                price: Price::new(500),
                quantity: Quantity::new(10),
                side: Side::Sell,
                user_id: Hash32::zero(),
                timestamp: TimestampMs::new(0),
                time_in_force: TimeInForce::Gtc,
                trail_amount: Quantity::new(500),
                last_reference_price: Price::new(1000),
                extra_fields: (),
            })
            .unwrap();
    }
    order_book
}

/// Register the pending-stop workloads.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Pending Stops");

    group.bench_function("match_market_against_limit_with_pending_stops", |b| {
        b.iter_batched_ref(
            || with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 100), 10),
            |order_book| {
                let _ = black_box(order_book.submit_market_order(new_id(), 50, Side::Buy));
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("add_limit_orders_with_pending_stops", |b| {
        b.iter_batched_ref(
            || {
                let order_book =
                    with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 100), 10);
                let ids: Vec<Id> = (0..100).map(|_| new_id()).collect();
                (order_book, ids)
            },
            |(order_book, ids)| {
                for (i, id) in ids.iter().enumerate() {
                    let _ = black_box(order_book.add_limit_order(
                        *id,
                        900 - i as u128,
                        10,
                        Side::Buy,
                        TimeInForce::Gtc,
                        None,
                    ));
                }
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}
