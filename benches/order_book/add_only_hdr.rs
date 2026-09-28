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

// add_only_hdr — pure passive limit-order entry, no crossings.
// Measures `add_order` insert cost in isolation.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, persist, record, report, submit_gtc};

const SCENARIO: &str = "add_only";
const WARMUP_OPS: u64 = 200_000;
const MEASURED_OPS: u64 = 1_000_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    // Warmup — discarded.
    for i in 0..WARMUP_OPS {
        submit_gtc(&book, &mut rng, i);
    }

    // Measurement — id space picks up where warmup stopped to avoid
    // collisions inside `order_locations`.
    for i in 0..MEASURED_OPS {
        let id = WARMUP_OPS + i;
        record(&mut hist, || submit_gtc(&book, &mut rng, id));
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
