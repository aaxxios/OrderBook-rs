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

// cancel_only_hdr — pre-loaded book + cancel workload.
// Measures `cancel_order` lookup + unlink cost.
//
// Methodology (issue #258): a single `cancel_order` here runs in the
// tens of ns (BENCH.md quotes a `p50` of 41 ns pre-fix — i.e. one host
// clock tick), so timing it with one `Instant` pair per call measures
// the clock, not the cancel. `record_batch` times `BATCH` cancels per
// `Instant` pair and records the per-op average instead; see
// `hdr_common::record_batch` for the trade-off.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, SEED_OWNERS, new_histogram, persist, record_batch, report, seed_resting};
use pricelevel::Id;

const SCENARIO: &str = "cancel_only";
const PRELOAD_OPS: u64 = 1_000_000;
const BATCH: u64 = 32;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    // Pre-load the book with PRELOAD_OPS resting orders, ids
    // 1..=PRELOAD_OPS. #259 PR review: seeded NON-crossing and asserted;
    // the old `submit_gtc` seed crossed, left about 11 % of the ids
    // resting, and most timed cancels were misses.
    seed_resting(&book, &mut rng, 1, PRELOAD_OPS, SEED_OWNERS);

    // Cancel each one, in order, `BATCH` at a time. Each cancel's result
    // (the removed order) is kept in a buffer reserved before the clock
    // and dropped after it.
    let mut results = Vec::with_capacity(BATCH as usize);
    let mut next = 0u64;
    while next < PRELOAD_OPS {
        let base = next;
        let k = BATCH.min(PRELOAD_OPS - next);
        results.clear();
        record_batch(&mut hist, k, |j| {
            results.push(book.cancel_order(Id::from_u64(base + j + 1)));
        });
        assert!(
            results.iter().all(|r| matches!(r, Ok(Some(_)))),
            "every cancel hits a resting order"
        );
        next += k;
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
