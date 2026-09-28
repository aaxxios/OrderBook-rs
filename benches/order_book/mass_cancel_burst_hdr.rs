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

// mass_cancel_burst_hdr — dense book, then `cancel_all_orders` burst.
// Measures the bulk-cancel worst case as a single observation per cycle
// rather than per-order.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, persist, record, report, submit_gtc};

const SCENARIO: &str = "mass_cancel_burst";
const ORDERS_PER_BURST: u64 = 10_000;
const MEASURED_BURSTS: u64 = 500;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let mut next_id: u64 = 1;

    for _ in 0..MEASURED_BURSTS {
        // Re-load the book up to ORDERS_PER_BURST resting orders. Not
        // measured.
        for _ in 0..ORDERS_PER_BURST {
            submit_gtc(&book, &mut rng, next_id);
            next_id += 1;
        }

        // The single-burst measurement: time `cancel_all_orders` end to
        // end. The histogram entry is "ns to drain N orders", not per
        // order — useful as an operator-side wall-clock guard rather
        // than a per-op tail.
        record(&mut hist, || {
            let _ = book.cancel_all_orders();
        });
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
