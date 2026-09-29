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

// aggressive_walk_hdr — taker market orders sweep multi-level book.
// Measures the fill-loop tail under saturating liquidity.
//
// Methodology (issue #258): most sweeps here fill within a single level
// (BENCH.md quotes a pre-fix `p50` of 42 ns — one host clock tick), so a
// single `Instant` pair per call would measure the clock, not the fill
// loop. `record_batch` times `BATCH` sweeps per `Instant` pair and
// records the per-op average instead; see `hdr_common::record_batch`
// for the trade-off.
//
// Liquidity (issue #259): the ladder holds about 27 500 lots, and 100 000
// takers of up to 20 lots empty it after about 2 200 of them; before
// #259 the remaining ~98 % of the samples timed a market order rejected
// by an empty book. The ladder is now re-seeded (unmeasured, between
// batches) whenever the resting quantity could not cover a full batch
// of the largest taker, so every measured taker trades. Numbers are
// not comparable with pre-#259 runs of this bench.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record_batch, report};
use pricelevel::{Id, Side, TimeInForce};

const SCENARIO: &str = "aggressive_walk";
// Pre-load enough resting depth for every aggressive sweep to fill.
const RESTING_PER_LEVEL: u64 = 100;
const NUM_LEVELS: u64 = 50;
const MEASURED_OPS: u64 = 100_000;
const BATCH: u64 = 32;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);

    // Seed RESTING_PER_LEVEL asks at each of NUM_LEVELS prices; re-run
    // (unmeasured) whenever the ladder runs low, see the note above.
    let mut next_id = 1u64;
    let mut resting = 0u64;
    let seed = |rng: &mut Rng, next_id: &mut u64, resting: &mut u64| {
        for level in 0..NUM_LEVELS {
            let price = (100 + level) as u128;
            for _ in 0..RESTING_PER_LEVEL {
                let qty = rng.range(1, 10);
                let _ = book.add_limit_order_with_user(
                    Id::from_u64(*next_id),
                    price,
                    qty,
                    Side::Sell,
                    TimeInForce::Gtc,
                    maker,
                    None,
                );
                *next_id += 1;
                *resting += qty;
            }
        }
    };
    seed(&mut rng, &mut next_id, &mut resting);

    // Aggressive Buy sweeps. Each sweeps 5..=20 lots — usually clears
    // a few orders within the same price level. Batched `BATCH` at a
    // time (see the methodology note above).
    let mut done = 0u64;
    while done < MEASURED_OPS {
        let k = BATCH.min(MEASURED_OPS - done);
        if resting < k * 20 {
            seed(&mut rng, &mut next_id, &mut resting);
        }
        record_batch(&mut hist, k, |_| {
            let qty = rng.range(5, 20);
            let id = Id::from_u64(next_id);
            next_id += 1;
            let result = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
            let filled = result
                .ok()
                .and_then(|r| r.executed_quantity().ok())
                .map_or(0, |q| q.as_u64());
            resting = resting.saturating_sub(filled);
        });
        done += k;
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
