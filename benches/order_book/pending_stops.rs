//! #286: workloads on a book holding pending trailing stops
//! (`special_orders`).
//!
//! - `*_0_stops` rows are the controls: the same setup (a trade at 1000
//!   first) without any stop, so the `with_pending_stops` / `quiet` rows
//!   measure only what pending stops add.
//! - `match_market_against_limit_with_pending_stops` /
//!   `add_limit_orders_with_pending_stops`: ten pending sell stops far
//!   below the market (stop 500, watermark 1000, trail 500) that the
//!   measured orders neither trail nor elect: the exclusive gate plus one
//!   evaluation pass per trading call.
//! - `trailing/N`: one market buy at a new high trails all `N` stops
//!   (every stop is re-keyed).
//! - `elect/N`: one market sell elects all `N` stops at one print; each
//!   runs its market order into a deep bid.
//! - `cascade/N`: one market sell elects the first of `N` stops whose
//!   market orders elect the next, one by one.
//! - `concurrent_*`: 2 / 4 threads adding limit orders (can trade: the
//!   exclusive gate while a stop is pending) or post-only orders (cannot
//!   trade: the shared gate), with 0 or 1 pending stop.
//!
//! Registered in its own group, last, so the shared rows run the same
//! sequence of benches in a comparison against a baseline without it.

use criterion::{BatchSize, BenchmarkId, Criterion};
use orderbook_rs::OrderBook;
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

fn owner(byte: u8) -> Hash32 {
    Hash32::new([byte; 32])
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

fn trailing_stop(stop: u128, watermark: u128, qty: u64) -> OrderType<()> {
    OrderType::TrailingStop {
        id: new_id(),
        price: Price::new(stop),
        quantity: Quantity::new(qty),
        side: Side::Sell,
        user_id: owner(9),
        timestamp: TimestampMs::new(0),
        time_in_force: TimeInForce::Gtc,
        trail_amount: Quantity::new(u64::try_from(watermark - stop).unwrap()),
        last_reference_price: Price::new(watermark),
        extra_fields: (),
    }
}

/// Trades once at 1000 (the book needs an ask at 1000), then adds `count`
/// pending sell stops that later trades at 1000 neither trail nor elect.
fn with_pending_stops(order_book: OrderBook, count: u64) -> OrderBook {
    let _ = order_book.submit_market_order(new_id(), 1, Side::Buy);
    for _ in 0..count {
        order_book.add_order(trailing_stop(500, 1000, 10)).unwrap();
    }
    order_book
}

/// Asks at 1001.. (one lot each) above a trade at 1000, and `n` sell stops
/// (stop 500, watermark 1000): every market buy of 1 prints a new high and
/// trails all of them.
fn trailing_book(n: u64) -> OrderBook {
    let book = with_asks(OrderBook::new("TEST-SYMBOL"), 1);
    let _ = book.submit_market_order(new_id(), 10, Side::Buy);
    for step in 1..=64u128 {
        book.add_limit_order(new_id(), 1000 + step, 1, Side::Sell, TimeInForce::Gtc, None)
            .unwrap();
    }
    for _ in 0..n {
        book.add_order(trailing_stop(500, 1000, 1)).unwrap();
    }
    book
}

/// A trade at 1000, a deep bid at 900 and `n` sell stops at 900: one
/// market sell elects all of them.
fn elect_book(n: u64) -> OrderBook {
    let book = with_asks(OrderBook::new("TEST-SYMBOL"), 1);
    let _ = book.submit_market_order(new_id(), 1, Side::Buy);
    book.add_limit_order(new_id(), 900, 1_000_000, Side::Buy, TimeInForce::Gtc, None)
        .unwrap();
    for _ in 0..n {
        book.add_order(trailing_stop(900, 1000, 1)).unwrap();
    }
    book
}

/// A trade at 6000, one-lot bids at 5001 and 5000 ..= 5001 - n, a deep bid
/// at 100, and `n` one-lot sell stops at 5000, 4999, ...: a market sell of
/// 2 prints 5001, 5000 and elects the stop at 5000, whose sale prints 4999
/// and elects the next, and so on.
fn cascade_book(n: u64) -> OrderBook {
    let book = OrderBook::new("TEST-SYMBOL");
    book.add_limit_order(new_id(), 6000, 1, Side::Sell, TimeInForce::Gtc, None)
        .unwrap();
    let _ = book.submit_market_order(new_id(), 1, Side::Buy);
    book.add_limit_order(new_id(), 100, 1_000_000, Side::Buy, TimeInForce::Gtc, None)
        .unwrap();
    book.add_limit_order(new_id(), 5001, 1, Side::Buy, TimeInForce::Gtc, None)
        .unwrap();
    for k in 0..u128::from(n) {
        book.add_limit_order(new_id(), 5000 - k, 1, Side::Buy, TimeInForce::Gtc, None)
            .unwrap();
        book.add_order(trailing_stop(5000 - k, 6000, 1)).unwrap();
    }
    book
}

/// `threads` threads each run `op` `iters` times on one shared book after
/// a barrier; returns the wall time from the barrier to the last thread.
fn measure_concurrent(
    book: OrderBook,
    threads: usize,
    iters: u64,
    op: fn(&OrderBook, usize, u64),
) -> Duration {
    let book = Arc::new(book);
    let barrier = Arc::new(Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|thread| {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..iters {
                    op(&book, thread, i);
                }
                barrier.wait();
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    barrier.wait();
    let elapsed = start.elapsed();
    for handle in handles {
        handle.join().unwrap();
    }
    elapsed
}

fn concurrent_limit_add(book: &OrderBook, _thread: usize, _i: u64) {
    book.add_limit_order(new_id(), 900, 10, Side::Buy, TimeInForce::Gtc, None)
        .unwrap();
}

fn concurrent_post_only_add(book: &OrderBook, _thread: usize, _i: u64) {
    book.add_post_only_order(new_id(), 900, 10, Side::Buy, TimeInForce::Gtc, None)
        .unwrap();
}

/// Register the pending-stop workloads.
pub fn register_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("OrderBook - Pending Stops");

    for stops in [0u64, 10] {
        let name = if stops == 0 {
            "match_market_against_limit_0_stops"
        } else {
            "match_market_against_limit_with_pending_stops"
        };
        group.bench_function(name, |b| {
            b.iter_batched_ref(
                || with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 100), stops),
                |order_book| {
                    let _ = black_box(order_book.submit_market_order(new_id(), 50, Side::Buy));
                },
                BatchSize::SmallInput,
            )
        });
        let name = if stops == 0 {
            "add_limit_orders_0_stops"
        } else {
            "add_limit_orders_with_pending_stops"
        };
        group.bench_function(name, |b| {
            b.iter_batched_ref(
                || {
                    let order_book =
                        with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 100), stops);
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
    }

    for n in [10u64, 1000] {
        group.bench_with_input(BenchmarkId::new("trailing", n), &n, |b, &n| {
            b.iter_batched_ref(
                || trailing_book(n),
                |book| {
                    let _ = black_box(book.submit_market_order(new_id(), 1, Side::Buy));
                },
                BatchSize::LargeInput,
            )
        });
        group.bench_with_input(BenchmarkId::new("elect", n), &n, |b, &n| {
            b.iter_batched_ref(
                || elect_book(n),
                |book| {
                    let _ = black_box(book.submit_market_order(new_id(), 1, Side::Sell));
                },
                BatchSize::LargeInput,
            )
        });
        group.bench_with_input(BenchmarkId::new("cascade", n), &n, |b, &n| {
            b.iter_batched_ref(
                || cascade_book(n),
                |book| {
                    let _ = black_box(book.submit_market_order(new_id(), 2, Side::Sell));
                },
                BatchSize::LargeInput,
            )
        });
    }

    for threads in [2usize, 4] {
        for stops in [0u64, 1] {
            let limit = format!("concurrent_add_limit_orders_{stops}_stops");
            group.bench_with_input(BenchmarkId::new(limit, threads), &threads, |b, &threads| {
                b.iter_custom(|iters| {
                    let book =
                        with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 1), stops);
                    measure_concurrent(book, threads, iters, concurrent_limit_add)
                })
            });
            let post_only = format!("concurrent_post_only_adds_{stops}_stops");
            group.bench_with_input(
                BenchmarkId::new(post_only, threads),
                &threads,
                |b, &threads| {
                    b.iter_custom(|iters| {
                        let book =
                            with_pending_stops(with_asks(OrderBook::new("TEST-SYMBOL"), 1), stops);
                        measure_concurrent(book, threads, iters, concurrent_post_only_add)
                    })
                },
            );
        }
    }

    group.finish();
}
