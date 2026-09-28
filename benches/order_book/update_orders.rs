use criterion::{BatchSize, BenchmarkId, Criterion};
use orderbook_rs::OrderBook;
use pricelevel::{Id, Side, TimeInForce};
use std::hint::black_box;
use uuid::Uuid;

/// Register all benchmarks for updating orders in an order book
///
/// # Methodology (issue #258)
///
/// Every group here used to build a fresh 100-order book *inside*
/// `b.iter`, collect the ids to touch, and drop the book at the end of
/// the same closure — so the reported number was book construction +
/// id collection + N cancels/updates + book teardown, not the N ops.
/// `iter_batched_ref` builds the book and pre-collects the target ids in
/// the unmeasured `setup` closure and takes `(book, ids)` by `&mut` in
/// `routine`, so the book's drop also lands outside the measurement
/// window — only the N `cancel_order` / `update_order` calls are timed.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Update Orders");

    // Benchmark canceling orders
    group.bench_function("cancel_orders", |b| {
        b.iter_batched_ref(
            || {
                let order_book = setup_order_book_with_orders(100);
                let ids = collect_order_ids(&order_book, 50);
                (order_book, ids)
            },
            |(order_book, ids)| {
                for id in ids.iter() {
                    let _ = black_box(order_book.cancel_order(*id));
                }
            },
            BatchSize::SmallInput,
        )
    });

    // Benchmark updating order quantities
    group.bench_function("update_quantities", |b| {
        b.iter_batched_ref(
            || {
                let order_book = setup_order_book_with_orders(100);
                let ids = collect_order_ids(&order_book, 50);
                (order_book, ids)
            },
            |(order_book, ids)| {
                for id in ids.iter() {
                    let update = pricelevel::OrderUpdate::UpdateQuantity {
                        order_id: *id,
                        new_quantity: pricelevel::Quantity::new(20),
                    };
                    let _ = black_box(order_book.update_order(update));
                }
            },
            BatchSize::SmallInput,
        )
    });

    // Parametrized benchmark with different order counts for cancellation
    for order_count in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("cancel_order_count_scaling", order_count),
            order_count,
            |b, &order_count| {
                b.iter_batched_ref(
                    || {
                        let order_book = setup_order_book_with_orders(order_count);
                        let ids = collect_order_ids(&order_book, order_count / 4);
                        (order_book, ids)
                    },
                    |(order_book, ids)| {
                        for id in ids.iter() {
                            let _ = black_box(order_book.cancel_order(*id));
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

// Helper function to set up an order book with orders
fn setup_order_book_with_orders(order_count: u64) -> OrderBook {
    let order_book = OrderBook::new("TEST-SYMBOL");

    // Add orders to the book
    for _i in 0..order_count {
        let id = Id::from_uuid(Uuid::new_v4());
        order_book
            .add_limit_order(id, 1000, 10, Side::Buy, TimeInForce::Gtc, None)
            .unwrap();
    }

    order_book
}

// Helper function to collect some order IDs from the book
fn collect_order_ids(order_book: &OrderBook, count: u64) -> Vec<Id> {
    // In a real implementation, you would extract these from the order book
    // This is a placeholder function
    let all_orders = order_book.get_all_orders();
    all_orders
        .iter()
        .take(count as usize)
        .map(|order| order.id())
        .collect()
}
