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

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, persist, record, report, submit_gtc};
use pricelevel::Id;

const SCENARIO: &str = "cancel_only";
const PRELOAD_OPS: u64 = 1_000_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    // Pre-load the book with PRELOAD_OPS resting orders. The id space is
    // 1..=PRELOAD_OPS so cancel ids are deterministic and present.
    for i in 0..PRELOAD_OPS {
        submit_gtc(&book, &mut rng, i + 1);
    }

    // Cancel each one, in order. No warmup phase needed — cancel cost is
    // dominated by `DashMap::remove` which has a stable distribution.
    for i in 0..PRELOAD_OPS {
        let id = Id::from_u64(i + 1);
        record(&mut hist, || {
            let _ = book.cancel_order(id);
        });
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
