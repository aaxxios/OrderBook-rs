//! The headline scenarios from `benches/order_book/*_hdr.rs` (plus a
//! snapshot, a replay and a contended scenario), reimplemented against
//! `crate::adapter` so the exact same source runs against two different
//! `orderbook-rs` checkouts (issues #258 / #259).
//!
//! Each scenario returns a `Histogram<u64>` of per-op nanosecond
//! latencies. Only the operation under test is timed: seeding, refills
//! and the drop of anything the operation returns happen outside the
//! `Instant` pair. Scenarios whose single op is at or near the host
//! clock tick (about 41.67 ns on Apple silicon) use [`record_batch`],
//! which times `BATCH` ops per `Instant` pair and records the per-op
//! average; scenarios well above the tick use [`record`], one sample
//! per op. Every scenario says which it uses and why.

use hdrhistogram::Histogram;
use orderbook_rs::Id;
use orderbook_rs::Side;
use std::sync::{Arc, Barrier};
use std::time::Instant;

use crate::adapter::{
    Book, add_limit_order_with_user, cancel_all, cancel_order, create_snapshot, journal_of_adds,
    new_book, new_listener_book, new_stp_book, owner, replay, restore, snapshot_package,
    submit_market_order_with_user,
};
use crate::rng::Rng;

const BATCH: u64 = 32;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

pub fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 10_000_000_000, 3).expect("hist bounds")
}

/// Times one op; the op's return value is dropped after the clock stops.
fn record<R, F: FnOnce() -> R>(h: &mut Histogram<u64>, f: F) -> R {
    let t0 = Instant::now();
    let out = std::hint::black_box(f());
    let elapsed = t0.elapsed().as_nanos() as u64;
    h.record(elapsed.max(1)).expect("record");
    out
}

/// Times `k` ops in one `Instant` pair and records the per-op average.
fn record_batch<F: FnMut(u64)>(h: &mut Histogram<u64>, k: u64, mut f: F) {
    let t0 = Instant::now();
    for i in 0..k {
        f(i);
        std::hint::black_box(());
    }
    let elapsed = t0.elapsed().as_nanos() as u64;
    let per_op = elapsed.checked_div(k).unwrap_or(elapsed).max(1);
    h.record(per_op).expect("record");
}

/// Owner pool mirroring `hdr_common::OWNERS` / `pick_owner` — every
/// submit/cancel below picks one of these, never the userless path.
const OWNERS: u8 = 4;

fn pick_owner(rng: &mut Rng) -> [u8; 32] {
    owner(((rng.next() % OWNERS as u64) as u8) + 1)
}

fn pick_side(rng: &mut Rng) -> Side {
    if rng.next().is_multiple_of(2) {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// `hdr_common::submit_gtc`: a GTC limit at a random price in `99..=101`
/// on a random side. The band is tight on purpose, so a good share of
/// these CROSS the opposite side (this is not a purely passive add).
fn submit_one(book: &Book, rng: &mut Rng, id: u64) {
    let price = rng.range(99, 101) as u128;
    let qty = rng.range(1, 100);
    let side = pick_side(rng);
    add_limit_order_with_user(book, Id::from_u64(id), price, qty, side, pick_owner(rng));
}

/// `add_only_hdr`: 200k warmup + N measured `submit_gtc` calls, one
/// sample per op (hundreds of ns, well above the tick).
pub fn add_only(warmup_ops: u64, measured_ops: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    for i in 0..warmup_ops {
        submit_one(&book, &mut rng, i);
    }
    for i in 0..measured_ops {
        let id = warmup_ops + i;
        record(&mut hist, || submit_one(&book, &mut rng, id));
    }
    hist
}

/// `cancel_only_hdr`: pre-loaded book, sequential cancels, batched
/// (a single cancel is at or below the tick).
pub fn cancel_only(preload_ops: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    for i in 0..preload_ops {
        submit_one(&book, &mut rng, i + 1);
    }

    let mut next = 0u64;
    while next < preload_ops {
        let base = next;
        let k = BATCH.min(preload_ops - next);
        record_batch(&mut hist, k, |j| {
            cancel_order(&book, Id::from_u64(base + j + 1));
        });
        next += k;
    }
    hist
}

/// Taker market buys sweep a 50-level x 100-order ask ladder, batched.
///
/// Differs from `aggressive_walk_hdr` before #259 on purpose: that bench
/// seeds the ladder once (about 27 500 lots) and then sends 100 000
/// takers of 5..=20 lots, so the ladder is empty after about 2 200 of
/// them and the remaining ~98 % of the samples time a market order
/// rejected by an empty book. Here the ladder is re-seeded (unmeasured,
/// between batches) whenever the resting quantity could not cover a
/// full batch of the largest taker, so every measured taker trades.
pub fn aggressive_walk(
    resting_per_level: u64,
    num_levels: u64,
    measured_ops: u64,
) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id = 1u64;
    let mut resting = 0u64;

    let seed = |rng: &mut Rng, next_id: &mut u64, resting: &mut u64| {
        for level in 0..num_levels {
            let price = (100 + level) as u128;
            for _ in 0..resting_per_level {
                let qty = rng.range(1, 10);
                add_limit_order_with_user(
                    &book,
                    Id::from_u64(*next_id),
                    price,
                    qty,
                    Side::Sell,
                    maker,
                );
                *next_id += 1;
                *resting += qty;
            }
        }
    };
    seed(&mut rng, &mut next_id, &mut resting);

    let mut done = 0u64;
    while done < measured_ops {
        let k = BATCH.min(measured_ops - done);
        if resting < k * 20 {
            seed(&mut rng, &mut next_id, &mut resting);
        }
        record_batch(&mut hist, k, |_| {
            let qty = rng.range(5, 20);
            let id = Id::from_u64(next_id);
            next_id += 1;
            submit_market_order_with_user(&book, id, qty, Side::Buy, taker);
            resting -= qty.min(resting);
        });
        done += k;
    }
    hist
}

/// `mixed_70_20_10_hdr`: 70 % `submit_gtc`, 20 % cancel of a random
/// earlier id, 10 % market order of 1..=10 lots; one sample per op.
pub fn mixed_70_20_10(warmup_ops: u64, measured_ops: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let mut next_id = 1u64;

    fn apply(book: &Book, rng: &mut Rng, next_id: &mut u64) {
        let v = rng.next() % 100;
        if v < 70 {
            let id = *next_id;
            *next_id += 1;
            submit_one(book, rng, id);
        } else if v < 90 {
            if *next_id > 1 {
                let target = rng.range(1, *next_id - 1);
                cancel_order(book, Id::from_u64(target));
            }
        } else {
            let id = Id::from_u64(*next_id);
            *next_id += 1;
            let qty = rng.range(1, 10);
            let side = pick_side(rng);
            let user = pick_owner(rng);
            submit_market_order_with_user(book, id, qty, side, user);
        }
    }

    for _ in 0..warmup_ops {
        apply(&book, &mut rng, &mut next_id);
    }
    for _ in 0..measured_ops {
        record(&mut hist, || apply(&book, &mut rng, &mut next_id));
    }
    hist
}

/// `thin_book_sweep_hdr`: 3 asks of 1..=5 lots refilled every 5 probes
/// (unmeasured), market-buy probes of 1..=20 lots batched 5 at a time.
pub fn thin_book_sweep(measured_ops: u64) -> Histogram<u64> {
    const RESTING_PER_REFILL: u64 = 3;
    const REFILL_EVERY: u64 = 5;
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id = 1u64;
    let mut op = 0u64;

    while op < measured_ops {
        let k = REFILL_EVERY.min(measured_ops - op);
        for _ in 0..RESTING_PER_REFILL {
            let price = rng.range(99, 101) as u128;
            let qty = rng.range(1, 5);
            add_limit_order_with_user(&book, Id::from_u64(next_id), price, qty, Side::Sell, maker);
            next_id += 1;
        }
        record_batch(&mut hist, k, |_| {
            let id = Id::from_u64(next_id);
            next_id += 1;
            let qty = rng.range(1, 20);
            submit_market_order_with_user(&book, id, qty, Side::Buy, taker);
        });
        op += k;
    }
    hist
}

/// `mass_cancel_burst_hdr`: `orders_per_burst` `submit_gtc` calls
/// (unmeasured), then one timed `cancel_all_orders`. One sample is one
/// whole burst, not one cancel.
pub fn mass_cancel_burst(orders_per_burst: u64, bursts: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let mut next_id = 1u64;

    for _ in 0..bursts {
        for _ in 0..orders_per_burst {
            submit_one(&book, &mut rng, next_id);
            next_id += 1;
        }
        record(&mut hist, || cancel_all(&book));
    }
    hist
}

/// Self-trade prevention, `CancelMaker`: before each op (unmeasured) one
/// level at 1 000 is seeded with the taker's own 5-lot ask at the front
/// and two 5-lot asks from another owner behind it; the timed op is the
/// taker's 10-lot market buy, which cancels its own maker and fills the
/// other two, emptying the level. One sample per op (above the tick).
pub fn stp_cancel_maker(measured_ops: u64) -> Histogram<u64> {
    let book = new_stp_book("BENCH");
    let mut hist = new_histogram();
    let taker = owner(0xBB);
    let other = owner(0xAA);
    let mut next_id = 1u64;

    for _ in 0..measured_ops {
        for user in [taker, other, other] {
            add_limit_order_with_user(&book, Id::from_u64(next_id), 1_000, 5, Side::Sell, user);
            next_id += 1;
        }
        let id = Id::from_u64(next_id);
        next_id += 1;
        record(&mut hist, || {
            submit_market_order_with_user(&book, id, 10, Side::Buy, taker)
        });
    }
    hist
}

/// A non-crossing book of `orders` resting orders: bids on 900..=999,
/// asks on 1 001..=1 100, four owners. Returns the book and its ids.
fn resting_book(orders: u64) -> (Book, Vec<Id>) {
    let book = new_book("BENCH");
    let mut rng = Rng::new(SEED);
    let mut ids = Vec::with_capacity(orders as usize);
    for i in 0..orders {
        let id = Id::from_u64(i + 1);
        let side = pick_side(&mut rng);
        let offset = rng.range(0, 99) as u128;
        let price = match side {
            Side::Buy => 900 + offset,
            Side::Sell => 1_001 + offset,
        };
        let qty = rng.range(1, 100);
        add_limit_order_with_user(&book, id, price, qty, side, pick_owner(&mut rng));
        ids.push(id);
    }
    (book, ids)
}

/// Full-depth `create_snapshot` of a `orders`-order book; the snapshot
/// is dropped after the clock stops. One sample per snapshot.
pub fn snapshot_create(orders: u64, samples: u64) -> Histogram<u64> {
    let (book, _) = resting_book(orders);
    let mut hist = new_histogram();
    for _ in 0..samples {
        let snapshot = record(&mut hist, || create_snapshot(&book));
        drop(snapshot);
    }
    hist
}

/// `restore_from_snapshot_package` of a full-depth package of a
/// `orders`-order book into a fresh empty book. The package clone and
/// the empty book are built before the clock starts, and the restored
/// book is dropped after it stops. One sample per restore.
pub fn snapshot_restore(orders: u64, samples: u64) -> Histogram<u64> {
    let (book, _) = resting_book(orders);
    let package = snapshot_package(&book);
    let mut hist = new_histogram();
    for _ in 0..samples {
        let input = package.clone();
        let mut target = new_book("BENCH");
        record(&mut hist, || restore(&mut target, input));
        drop(target);
    }
    hist
}

/// `ReplayEngine::replay_from` of an in-memory journal of `events`
/// `AddOrder` events (the resting orders of [`resting_book`], so no
/// event crosses). The replayed book is dropped after the clock stops.
/// One sample per replay.
pub fn replay_adds(events: u64, samples: u64) -> Histogram<u64> {
    let (book, ids) = resting_book(events);
    let journal = journal_of_adds(&book, &ids);
    drop(book);
    let mut hist = new_histogram();
    for _ in 0..samples {
        let replayed = record(&mut hist, || replay(&journal));
        drop(replayed);
    }
    hist
}

/// `threads` threads add GTC buys at ONE price for ONE account on one
/// shared book (the shape of `concurrent_add_limit_orders[_with_listeners]`
/// in `benches/concurrent/register.rs`), each timing `BATCH` adds per
/// `Instant` pair; the per-thread histograms are merged. `warmup` adds
/// per thread run before a barrier and are not recorded.
pub fn contended_same_price_adds(
    threads: u64,
    warmup: u64,
    per_thread: u64,
    listeners: bool,
) -> Histogram<u64> {
    let book = Arc::new(if listeners {
        new_listener_book("BENCH")
    } else {
        new_book("BENCH")
    });
    let barrier = Arc::new(Barrier::new(threads as usize));
    let account = owner(0x11);
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let book = Arc::clone(&book);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let base = (t + 1) * 1_000_000_000;
                let mut next = base;
                let add = |next: &mut u64| {
                    add_limit_order_with_user(
                        &book,
                        Id::from_u64(*next),
                        1_000,
                        10,
                        Side::Buy,
                        account,
                    );
                    *next += 1;
                };
                for _ in 0..warmup {
                    add(&mut next);
                }
                barrier.wait();
                let mut hist = new_histogram();
                let mut done = 0u64;
                while done < per_thread {
                    let k = BATCH.min(per_thread - done);
                    record_batch(&mut hist, k, |_| add(&mut next));
                    done += k;
                }
                hist
            })
        })
        .collect();
    let mut merged = new_histogram();
    for handle in handles {
        let hist = handle.join().expect("contended worker");
        merged.add(&hist).expect("merge histograms");
    }
    merged
}
