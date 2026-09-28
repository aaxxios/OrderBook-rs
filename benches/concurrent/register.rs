use criterion::{BenchmarkId, Criterion, criterion_group};
use orderbook_rs::OrderBook;
use pricelevel::{Id, Side, TimeInForce};
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// A book with no-op trade and price-level listeners installed (#249):
/// measures the deferred, ordered emission path (buffer, stamp, dispatch)
/// against the listener-free fast path.
fn book_with_listeners() -> OrderBook {
    let trade: orderbook_rs::orderbook::trade::TradeListener =
        Arc::new(|result: &orderbook_rs::orderbook::trade::TradeResult| {
            black_box(result.engine_seq);
        });
    let level: orderbook_rs::orderbook::book_change_event::PriceLevelChangedListener = Arc::new(
        |event: orderbook_rs::orderbook::book_change_event::PriceLevelChangedEvent| {
            black_box(event.engine_seq);
        },
    );
    OrderBook::with_trade_and_price_level_listener("TEST-SYMBOL", trade, level)
}

pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Concurrent Operations");

    // Test with various thread counts
    for thread_count in [2, 4, 8, 16].iter() {
        group.bench_with_input(
            BenchmarkId::new("concurrent_add_limit_orders", thread_count),
            thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    measure_concurrent_operation(
                        thread_count,
                        iters,
                        |order_book, _thread_id, _iteration| {
                            // Each thread adds orders with unique IDs
                            let id = new_id();
                            order_book
                                .add_limit_order(id, 1000, 10, Side::Buy, TimeInForce::Gtc, None)
                                .unwrap();
                        },
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("concurrent_add_limit_orders_with_listeners", thread_count),
            thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    measure_concurrent_operation_on(
                        book_with_listeners(),
                        thread_count,
                        iters,
                        |order_book, _thread_id, _iteration| {
                            let id = new_id();
                            order_book
                                .add_limit_order(id, 1000, 10, Side::Buy, TimeInForce::Gtc, None)
                                .unwrap();
                        },
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("concurrent_mixed_operations", thread_count),
            thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    measure_concurrent_mixed_operations_on(
                        OrderBook::new("TEST-SYMBOL"),
                        thread_count,
                        iters,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("concurrent_mixed_operations_with_listeners", thread_count),
            thread_count,
            |b, &thread_count| {
                b.iter_custom(|iters| {
                    measure_concurrent_mixed_operations_on(
                        book_with_listeners(),
                        thread_count,
                        iters,
                    )
                });
            },
        );
    }

    group.finish();
}

/// Measures time for concurrent operations on an order book
fn measure_concurrent_operation<F>(thread_count: usize, iterations: u64, operation: F) -> Duration
where
    F: Fn(&Arc<OrderBook>, usize, u64) + Send + Sync + 'static,
{
    measure_concurrent_operation_on(
        OrderBook::new("TEST-SYMBOL"),
        thread_count,
        iterations,
        operation,
    )
}

/// [`measure_concurrent_operation`] on a caller-built book.
fn measure_concurrent_operation_on<F>(
    order_book: OrderBook,
    thread_count: usize,
    iterations: u64,
    operation: F,
) -> Duration
where
    F: Fn(&Arc<OrderBook>, usize, u64) + Send + Sync + 'static,
{
    let order_book = Arc::new(order_book);
    let operation = Arc::new(operation);
    let barrier = Arc::new(Barrier::new(thread_count + 1)); // +1 for main thread

    let mut handles = Vec::with_capacity(thread_count);

    for thread_id in 0..thread_count {
        let thread_order_book = Arc::clone(&order_book);
        let thread_barrier = Arc::clone(&barrier);
        let thread_operation = Arc::clone(&operation);

        handles.push(thread::spawn(move || {
            // Wait for all threads to be ready
            thread_barrier.wait();

            for i in 0..iterations {
                thread_operation(&thread_order_book, thread_id, i);
            }

            // Signal completion
            thread_barrier.wait();
        }));
    }

    // Start timing
    barrier.wait();
    let start = Instant::now();

    // Wait for all threads to complete
    barrier.wait();
    let duration = start.elapsed();

    // Join all threads
    for handle in handles {
        let _ = handle.join();
    }

    duration
}

/// Measures time for mixed concurrent operations (add, match, cancel) on an order book
fn measure_concurrent_mixed_operations_on(
    order_book: OrderBook,
    thread_count: usize,
    iterations: u64,
) -> Duration {
    let order_book: Arc<OrderBook> = Arc::new(order_book);
    let barrier = Arc::new(Barrier::new(thread_count + 1)); // +1 for main thread

    // Pre-populate with some orders
    for i in 0..200 {
        let id = new_id();
        let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
        let price = if side == Side::Buy { 990 } else { 1010 };
        order_book
            .add_limit_order(id, price, 10, side, TimeInForce::Gtc, None)
            .unwrap();
    }

    let mut handles = Vec::with_capacity(thread_count);

    for thread_id in 0..thread_count {
        let thread_order_book = Arc::clone(&order_book);
        let thread_barrier = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            // Wait for all threads to be ready
            thread_barrier.wait();

            for i in 0..iterations {
                // Determine operation based on iteration
                match i % 4 {
                    0 => {
                        // Add a new order
                        let id = new_id();
                        let side = if thread_id % 2 == 0 {
                            Side::Buy
                        } else {
                            Side::Sell
                        };
                        let price = if side == Side::Buy { 990 } else { 1010 };
                        thread_order_book
                            .add_limit_order(id, price, 10, side, TimeInForce::Gtc, None)
                            .unwrap();
                    }
                    1 => {
                        // Match with a market order
                        let id = new_id();
                        let side = if thread_id % 2 == 0 {
                            Side::Buy
                        } else {
                            Side::Sell
                        };
                        thread_order_book.submit_market_order(id, 5, side).ok();
                    }
                    2 => {
                        // Get all orders and maybe cancel some
                        if let Some(order) = thread_order_book.get_all_orders().get(thread_id % 10)
                        {
                            thread_order_book.cancel_order(order.id()).ok();
                        }
                    }
                    _ => {
                        // Create a snapshot
                        thread_order_book.create_snapshot(5).expect("snapshot");
                    }
                }
            }

            // Signal completion
            thread_barrier.wait();
        }));
    }

    // Start timing
    barrier.wait();
    let start = Instant::now();

    // Wait for all threads to complete
    barrier.wait();
    let duration = start.elapsed();

    // Join all threads
    for handle in handles {
        let _ = handle.join();
    }

    duration
}

criterion_group!(concurrent_benches, register_benchmarks);
