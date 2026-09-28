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

// notional_walk_hdr — taker market orders specified by quote-notional
// amount sweep multi-level book. Mirrors `aggressive_walk_hdr` but uses
// the `match_market_order_by_amount_with_user` path so we can compare
// p50 / p99 / p99.9 / p99.99 against the base-quantity sweep on the
// same book shape.
//
// Methodology (issue #258): same sub-tick concern and fix as
// `aggressive_walk_hdr` (pre-fix `p50` of 42 ns) — see that file's note
// and `hdr_common::record_batch`.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record_batch, report};
use pricelevel::{Id, Side, TimeInForce};

const SCENARIO: &str = "notional_walk";
const RESTING_PER_LEVEL: u64 = 100;
const NUM_LEVELS: u64 = 50;
const MEASURED_OPS: u64 = 100_000;
const BATCH: u64 = 32;
const SEED: u64 = 0xC1_C1_C1_C1_C1_C1_C1_C1;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);

    // Seed RESTING_PER_LEVEL asks at each of NUM_LEVELS prices.
    let mut next_id = 1u64;
    for level in 0..NUM_LEVELS {
        let price = (100 + level) as u128;
        for _ in 0..RESTING_PER_LEVEL {
            let _ = book.add_limit_order_with_user(
                Id::from_u64(next_id),
                price,
                rng.range(1, 10),
                Side::Sell,
                TimeInForce::Gtc,
                maker,
                None,
            );
            next_id += 1;
        }
    }

    // Aggressive notional Buy sweeps. Random budgets in [500, 2_000)
    // quote ticks — usually clear a few orders at the best level or
    // walk into the next. Batched `BATCH` at a time (see the
    // methodology note above).
    let mut done = 0u64;
    while done < MEASURED_OPS {
        let k = BATCH.min(MEASURED_OPS - done);
        record_batch(&mut hist, k, |j| {
            let amount = rng.range(500, 2_000) as u128;
            let id = Id::from_u64(next_id + done + j);
            let _ = book.submit_market_order_by_amount_with_user(id, amount, Side::Buy, taker);
        });
        done += k;
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
