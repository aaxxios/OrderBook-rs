//! The three headline scenarios from `benches/order_book/*_hdr.rs`,
//! reimplemented against `crate::adapter` so the exact same source runs
//! against two different `orderbook-rs` checkouts. Kept intentionally
//! small (three scenarios, not the full audited HDR suite) — this crate
//! exists to *compare* two versions, not to re-host the whole bench
//! suite; #259 is expected to grow this list as its own regressions
//! need isolating.
//!
//! Each scenario returns a `Histogram<u64>` of per-op nanosecond
//! latencies, using the same `record` / `record_batch` split as
//! `benches/order_book/hdr_common.rs` (issue #258): `cancel_only` and
//! `aggressive_walk` batch `BATCH` ops per `Instant` pair because a
//! single op is at or near the host clock-tick resolution; `add_only`
//! does not need batching.

use hdrhistogram::Histogram;
use orderbook_rs::Id;
use orderbook_rs::Side;
use std::time::Instant;

use crate::adapter::{
    add_limit_order_with_user, cancel_order, new_book, owner, submit_market_order_with_user,
};
use crate::rng::Rng;

const BATCH: u64 = 32;

fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("hist bounds")
}

fn record<F: FnOnce()>(h: &mut Histogram<u64>, f: F) {
    let t0 = Instant::now();
    f();
    std::hint::black_box(());
    let elapsed = t0.elapsed().as_nanos() as u64;
    h.record(elapsed.max(1)).expect("record");
}

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

/// Pure passive limit-order entry, no crossings. Mirrors
/// `benches/order_book/add_only_hdr.rs`'s `submit_gtc` shape exactly,
/// including the `user_id` every seed/measured op carries there.
pub fn add_only(warmup_ops: u64, measured_ops: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(0xA5A5_A5A5_A5A5_A5A5);
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

/// Owner pool mirroring `hdr_common::OWNERS` / `pick_owner` — every
/// submit/cancel below picks one of these, never the userless path.
const OWNERS: u8 = 4;

fn pick_owner(rng: &mut Rng) -> [u8; 32] {
    owner(((rng.next() % OWNERS as u64) as u8) + 1)
}

fn submit_one(book: &crate::adapter::Book, rng: &mut Rng, id: u64) {
    let price = rng.range(99, 101) as u128;
    let qty = rng.range(1, 100);
    let side = if rng.next().is_multiple_of(2) {
        Side::Buy
    } else {
        Side::Sell
    };
    add_limit_order_with_user(book, Id::from_u64(id), price, qty, side, pick_owner(rng));
}

/// Pre-loaded book, sequential cancels. Mirrors
/// `benches/order_book/cancel_only_hdr.rs`; batched per issue #258 (a
/// single cancel here is at or below the host clock tick). The preload
/// uses the same user-bearing `submit_one` as `add_only` so this
/// scenario cancels against the same book/index shape
/// `cancel_only_hdr` does, not a userless one (#258 PR review).
pub fn cancel_only(preload_ops: u64) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(0xA5A5_A5A5_A5A5_A5A5);
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

/// Taker market orders sweep a multi-level book. Mirrors
/// `benches/order_book/aggressive_walk_hdr.rs`, including its distinct
/// maker/taker owners.
pub fn aggressive_walk(
    resting_per_level: u64,
    num_levels: u64,
    measured_ops: u64,
) -> Histogram<u64> {
    let book = new_book("BENCH");
    let mut rng = Rng::new(0xA5A5_A5A5_A5A5_A5A5);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);

    let mut next_id = 1u64;
    for level in 0..num_levels {
        let price = (100 + level) as u128;
        for _ in 0..resting_per_level {
            add_limit_order_with_user(
                &book,
                Id::from_u64(next_id),
                price,
                rng.range(1, 10),
                Side::Sell,
                maker,
            );
            next_id += 1;
        }
    }

    let mut done = 0u64;
    while done < measured_ops {
        let k = BATCH.min(measured_ops - done);
        record_batch(&mut hist, k, |j| {
            let qty = rng.range(5, 20);
            let id = Id::from_u64(next_id + done + j);
            submit_market_order_with_user(&book, id, qty, Side::Buy, taker);
        });
        done += k;
    }
    hist
}
