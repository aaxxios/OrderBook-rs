use criterion::{BatchSize, BenchmarkId, Criterion};
use orderbook_rs::OrderBook;
use pricelevel::{Id, Side, TimeInForce};
use std::hint::black_box;

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Register all benchmarks for adding orders to an order book
///
/// # Methodology (issue #258)
///
/// Every group here used to build a fresh `OrderBook` *inside* the timed
/// closure and let it (plus every id) drop inside the same closure, so
/// the reported number was `OrderBook::new` + N adds + N `Drop`s, not N
/// adds. `iter_batched_ref` moves the book construction and the id
/// pre-generation into the unmeasured `setup` closure and takes the book
/// by `&mut` in `routine`, so the book's own drop happens after the
/// measurement window closes too — only the N `add_*_order` calls are
/// timed. `BatchSize::SmallInput` is fine here: even the 1000-order
/// input is a handful of `Arc`s per level, not a large struct.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Add Orders");

    // Benchmark adding limit orders
    group.bench_function("add_limit_orders", |b| {
        b.iter_batched_ref(
            || {
                let order_book: OrderBook = OrderBook::new("TEST-SYMBOL");
                let ids: Vec<Id> = (0..100).map(|_| new_id()).collect();
                (order_book, ids)
            },
            |(order_book, ids)| {
                for (i, id) in ids.iter().enumerate() {
                    let _ = black_box(order_book.add_limit_order(
                        *id,
                        1000 + i as u128,
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

    // Benchmark adding iceberg orders
    group.bench_function("add_iceberg_orders", |b| {
        b.iter_batched_ref(
            || {
                let order_book: OrderBook = OrderBook::new("TEST-SYMBOL");
                let ids: Vec<Id> = (0..100).map(|_| new_id()).collect();
                (order_book, ids)
            },
            |(order_book, ids)| {
                for (i, id) in ids.iter().enumerate() {
                    let _ = black_box(order_book.add_iceberg_order(
                        *id,
                        1000 + i as u128,
                        5,
                        15,
                        Side::Sell,
                        TimeInForce::Gtc,
                        None,
                    ));
                }
            },
            BatchSize::SmallInput,
        )
    });

    // Benchmark adding post-only orders
    group.bench_function("add_post_only_orders", |b| {
        b.iter_batched_ref(
            || {
                let order_book: OrderBook = OrderBook::new("TEST-SYMBOL");
                let ids: Vec<Id> = (0..100).map(|_| new_id()).collect();
                (order_book, ids)
            },
            |(order_book, ids)| {
                for (i, id) in ids.iter().enumerate() {
                    let _ = black_box(order_book.add_post_only_order(
                        *id,
                        1000 + i as u128,
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

    // Parametrized benchmark with different order counts
    for order_count in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("order_count_scaling", order_count),
            order_count,
            |b, &order_count| {
                b.iter_batched_ref(
                    || {
                        let order_book: OrderBook = OrderBook::new("TEST-SYMBOL");
                        let ids: Vec<Id> = (0..order_count).map(|_| new_id()).collect();
                        (order_book, ids)
                    },
                    |(order_book, ids)| {
                        for id in ids.iter() {
                            let _ = black_box(order_book.add_limit_order(
                                *id,
                                1000,
                                10,
                                Side::Buy,
                                TimeInForce::Gtc,
                                None,
                            ));
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}
