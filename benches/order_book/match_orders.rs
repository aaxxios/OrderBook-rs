use criterion::{BatchSize, BenchmarkId, Criterion};
use orderbook_rs::OrderBook;
use pricelevel::{Id, Side, TimeInForce};
use std::hint::black_box;

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Register all benchmarks for matching orders in an order book
///
/// # Methodology (issue #258)
///
/// `setup_limit_order_book` / `setup_iceberg_order_book` used to run
/// *inside* `b.iter`, so every sample paid for building a fresh 50-100
/// order book (and dropping it) in addition to the one market order this
/// group claims to measure. `iter_batched_ref` moves the book
/// construction into the unmeasured `setup` closure and takes the book
/// by `&mut` in `routine`, so its drop also lands after the measurement
/// window closes — only `submit_market_order` is timed.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Match Orders");
    group.sample_size(100); // Adjust sample size for more consistent results

    // Benchmark market order against limit orders
    group.bench_function("match_market_against_limit", |b| {
        b.iter_batched_ref(
            || setup_limit_order_book(100),
            |order_book| {
                let id = new_id();
                let _ = black_box(order_book.submit_market_order(id, 50, Side::Buy));
            },
            BatchSize::SmallInput,
        )
    });

    // Benchmark market order against iceberg orders
    group.bench_function("match_market_against_iceberg", |b| {
        b.iter_batched_ref(
            || setup_iceberg_order_book(100),
            |order_book| {
                let id = new_id();
                let _ = black_box(order_book.submit_market_order(id, 75, Side::Buy));
            },
            BatchSize::SmallInput,
        )
    });

    // Benchmark with different match quantities against limit orders
    for match_quantity in [10, 50, 100, 200, 500].iter() {
        group.bench_with_input(
            BenchmarkId::new("match_quantity_scaling", match_quantity),
            match_quantity,
            |b, &match_quantity| {
                b.iter_batched_ref(
                    || setup_limit_order_book(50),
                    |order_book| {
                        let id = new_id();
                        let _ = black_box(order_book.submit_market_order(
                            id,
                            match_quantity,
                            Side::Buy,
                        ));
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

// Helper function to set up an order book with limit orders
fn setup_limit_order_book(order_count: u64) -> OrderBook {
    let order_book = OrderBook::new("TEST-SYMBOL");

    for _i in 0..order_count {
        let id = new_id();
        order_book
            .add_limit_order(id, 1000, 10, Side::Sell, TimeInForce::Gtc, None)
            .unwrap();
    }

    order_book
}

// Helper function to set up an order book with iceberg orders
fn setup_iceberg_order_book(order_count: u64) -> OrderBook {
    let order_book = OrderBook::new("TEST-SYMBOL");

    for _i in 0..order_count {
        let id = new_id();
        order_book
            .add_iceberg_order(id, 1000, 5, 15, Side::Sell, TimeInForce::Gtc, None)
            .unwrap();
    }

    order_book
}
