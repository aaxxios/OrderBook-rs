// This crate root is entirely bench code, not production (issue #242's
// Production Panic Policy gate, `[lints.clippy]` in `Cargo.toml`, is
// package-wide and would otherwise apply here too). Bench fixtures freely
// `.unwrap()` / `.expect()` setup, index fixed-size scratch buffers and do
// raw arithmetic on sample sizes; none of that reaches `src/`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

// stp_contention_hdr — N threads sharing ONE `OrderBook<()>`, comparing
// `STPMode::None` (baseline; every submit takes the shared side of the
// `submit_gate` `RwLock`) against `STPMode::CancelMaker` (STP-active
// submits take the exclusive side instead — #225) across thread counts
// 1 / 2 / 4 / 8. Exists to measure the contention cost of the #225 gate
// change; run this file on `main` and on the #225 branch and diff the two.
//
// Closed-loop **per-thread** service time: each thread issues its next op
// only after the previous one returns, so — like every other scenario in
// this suite — this under-reports the queueing delay a real load
// generator would see under saturation (see `BENCH.md` "Coordinated
// omission"). What this scenario captures that the single-threaded ones
// cannot: contention on the shared `submit_gate` and on the underlying
// `dashmap` / `SkipMap` structures when multiple threads mutate the same
// book at once.
//
// Workload per thread (fixed `OPS_PER_THREAD`, mirrors `mixed_70_20_10_hdr`'s
// realistic split): 70% passive limit adds a few ticks off `MID` (always
// rest, never cross), 20% cancels of that thread's own resting orders, 10%
// aggressive limit orders that jump past the whole passive band and cross
// it in one shot. Every thread owns one user id drawn from an 8-id pool, so
// under `CancelMaker` a thread's own aggressive crossings routinely hit its
// own resting makers — the same-user scan the exclusive gate exists to
// isolate from concurrent mutation.
//
// Reading the output: for a given thread count, compare
// `stp_contention_none_t<N>` against `stp_contention_cancel_maker_t<N>` —
// same op mix, same book geometry, only the STP mode (and therefore the
// gate mode) differs. `None` stays on the shared path at every thread
// count and is the contention baseline; any extra tail or `ops_per_s` drop
// in `CancelMaker` as thread count climbs is the cost of serializing
// STP-active submits through the exclusive gate. Compare the same
// scenario across `main` and this branch to isolate the #225 change from
// ordinary multi-core contention that both configurations share.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record, report};
use hdrhistogram::Histogram;
use orderbook_rs::{OrderBook, STPMode};
use pricelevel::{Hash32, Id, Side, TimeInForce};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

const THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];
const OPS_PER_THREAD: u64 = 50_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

// Owner pool shared by the pre-seed phase and the worker threads. 8 covers
// the largest entry in `THREAD_COUNTS` with one id per thread and no reuse.
const OWNER_POOL: usize = 8;
// Resting orders pre-loaded per owner, per side, before threads start.
const SEED_ORDERS_PER_OWNER_SIDE: u64 = 20;

// Book geometry. Passive adds rest within `1..=PASSIVE_TICKS` of `MID` and
// never cross — asks always sit above every possible passive-bid price and
// vice versa. The aggressive slice jumps `CROSS_TICKS` past `MID`, beyond
// the entire passive band, so it always finds resting depth to cross.
const MID: u128 = 1_000;
const PASSIVE_TICKS: u64 = 5;
const CROSS_TICKS: u64 = 6;
const QTY_LO: u64 = 1;
const QTY_HI: u64 = 50;
const AGGR_QTY_LO: u64 = 1;
const AGGR_QTY_HI: u64 = 20;

const MODES: [(STPMode, &str); 2] = [
    (STPMode::None, "none"),
    (STPMode::CancelMaker, "cancel_maker"),
];

#[derive(Clone, Copy)]
enum Op {
    PassiveAdd,
    Cancel,
    Aggressive,
}

fn pick_op(rng: &mut Rng) -> Op {
    let v = rng.next() % 100;
    if v < 70 {
        Op::PassiveAdd
    } else if v < 90 {
        Op::Cancel
    } else {
        Op::Aggressive
    }
}

fn pick_side(rng: &mut Rng) -> Side {
    if rng.next().is_multiple_of(2) {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// Seed both sides of the book with resting depth from the full owner pool
/// before any worker thread starts. Uses id namespace `0` (ids `1..`),
/// disjoint from every worker thread's namespace — see `run_scenario`.
fn seed_book(book: &OrderBook<()>) {
    let mut rng = Rng::new(SEED);
    let mut id = 1u64;
    for k in 0..OWNER_POOL {
        let user = owner(0x10 + k as u8);
        for _ in 0..SEED_ORDERS_PER_OWNER_SIDE {
            let ticks = rng.range(1, PASSIVE_TICKS) as u128;
            let qty = rng.range(QTY_LO, QTY_HI);
            let _ = book.add_limit_order_with_user(
                Id::from_u64(id),
                MID - ticks,
                qty,
                Side::Buy,
                TimeInForce::Gtc,
                user,
                None,
            );
            id += 1;

            let ticks = rng.range(1, PASSIVE_TICKS) as u128;
            let qty = rng.range(QTY_LO, QTY_HI);
            let _ = book.add_limit_order_with_user(
                Id::from_u64(id),
                MID + ticks,
                qty,
                Side::Sell,
                TimeInForce::Gtc,
                user,
                None,
            );
            id += 1;
        }
    }
}

fn apply(
    book: &OrderBook<()>,
    rng: &mut Rng,
    next_id: &mut u64,
    user: Hash32,
    mine: &mut Vec<Id>,
    op: Op,
) {
    match op {
        Op::PassiveAdd => {
            let id = Id::from_u64(*next_id);
            *next_id += 1;
            let side = pick_side(rng);
            let ticks = rng.range(1, PASSIVE_TICKS) as u128;
            let price = match side {
                Side::Buy => MID - ticks,
                Side::Sell => MID + ticks,
            };
            let qty = rng.range(QTY_LO, QTY_HI);
            if book
                .add_limit_order_with_user(id, price, qty, side, TimeInForce::Gtc, user, None)
                .is_ok()
            {
                mine.push(id);
            }
        }
        Op::Cancel => {
            if !mine.is_empty() {
                let idx = rng.range(0, (mine.len() - 1) as u64) as usize;
                let id = mine.swap_remove(idx);
                let _ = book.cancel_order(id);
            }
        }
        Op::Aggressive => {
            let id = Id::from_u64(*next_id);
            *next_id += 1;
            let side = pick_side(rng);
            // Fixed offset past the whole passive band — always crosses.
            let price = match side {
                Side::Buy => MID + CROSS_TICKS as u128,
                Side::Sell => MID - CROSS_TICKS as u128,
            };
            let qty = rng.range(AGGR_QTY_LO, AGGR_QTY_HI);
            let _ =
                book.add_limit_order_with_user(id, price, qty, side, TimeInForce::Ioc, user, None);
        }
    }
}

/// Run one `(mode, threads)` scenario: fresh book, pre-seeded, `threads`
/// workers released together on a `Barrier`, each running `OPS_PER_THREAD`
/// closed-loop ops into its own histogram. Returns the merged histogram
/// plus aggregate throughput across the measured window (barrier release
/// to last-thread-done).
fn run_scenario(mode: STPMode, threads: usize) -> (Histogram<u64>, f64) {
    let book: Arc<OrderBook<()>> = Arc::new(OrderBook::<()>::with_stp_mode("BENCH", mode));
    seed_book(&book);

    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::with_capacity(threads);

    for t in 0..threads {
        let book = Arc::clone(&book);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // Id namespace `t + 1` (namespace `0` is the pre-seed phase) so
            // no id ever collides across threads or with the seed phase.
            let mut next_id: u64 = (t as u64 + 1) << 40;
            let mut rng = Rng::new(SEED.wrapping_add(t as u64 + 1));
            let user = owner(0x10 + (t % OWNER_POOL) as u8);
            let mut mine: Vec<Id> = Vec::with_capacity(OPS_PER_THREAD as usize);
            let mut hist = new_histogram();

            barrier.wait();
            for _ in 0..OPS_PER_THREAD {
                let op = pick_op(&mut rng);
                record(&mut hist, || {
                    apply(&book, &mut rng, &mut next_id, user, &mut mine, op)
                });
            }
            barrier.wait();
            hist
        }));
    }

    // Release all workers together and stop the clock when the last one
    // reaches the completion barrier — join() below is then just cleanup.
    barrier.wait();
    let start = Instant::now();
    barrier.wait();
    let elapsed = start.elapsed();

    let mut merged = new_histogram();
    for handle in handles {
        let hist = handle.join().expect("worker thread panicked");
        merged.add(&hist).expect("merge per-thread histogram");
    }

    // Sanity check while developing: confirm the aggressive slice is
    // actually crossing (non-zero last trade price) rather than silently
    // resting or erroring out every time. `debug_assertions` is off for
    // `cargo bench` (release profile), so this is compiled out there.
    if cfg!(debug_assertions) {
        eprintln!(
            "debug: mode={mode:?} threads={threads} last_trade_price={:?}",
            book.last_trade_price()
        );
    }

    let total_ops = threads as u64 * OPS_PER_THREAD;
    let ops_per_s = total_ops as f64 / elapsed.as_secs_f64();
    (merged, ops_per_s)
}

fn main() {
    for (mode, label) in MODES {
        for threads in THREAD_COUNTS {
            let scenario = format!("stp_contention_{label}_t{threads}");
            let (hist, ops_per_s) = run_scenario(mode, threads);
            report(&scenario, &hist);
            println!("throughput scenario={scenario} ops_per_s={ops_per_s:.0}");
            persist(&scenario, &hist).expect("persist hgrm");
        }
    }
}
