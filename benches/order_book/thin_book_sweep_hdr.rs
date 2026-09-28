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

// thin_book_sweep_hdr — book near-empty, IOC probing.
// Exercises the partial-fill / cancel-the-remainder path.
//
// Methodology (issue #258): most probes here fully fill the thin
// resting depth or partial-fill and short-circuit (BENCH.md quotes a
// pre-fix `p50` of 42-83 ns — at or within two host clock ticks), so a
// single `Instant` pair per call would measure the clock, not the
// probe. `record_batch` times `BATCH` probes per `Instant` pair and
// records the per-op average instead; the unmeasured refill still runs
// every `REFILL_EVERY` probes exactly as before, on the same global op
// counter. See `hdr_common::record_batch` for the trade-off.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record_batch, report};
use pricelevel::{Id, Side, TimeInForce};

const SCENARIO: &str = "thin_book_sweep";
// Re-seed a thin slice (RESTING orders at one or two prices) every
// REFILL_EVERY ops so the book never goes fully empty across the
// measurement window.
const RESTING_PER_REFILL: u64 = 3;
const REFILL_EVERY: u64 = 5;
const MEASURED_OPS: u64 = 200_000;
// Batch size must equal `REFILL_EVERY` (not just divide it), pinning
// every batch's start exactly on a refill boundary. A larger batch
// (e.g. 32) front-loads every refill due within it into the unmeasured
// pre-pass, so the first probes in the batch would sweep a book that
// has already been topped up several refills ahead of where the
// original, unbatched loop would have had it at that point — a
// deeper-than-intended book changes the *workload*, not just how it is
// timed (measured: a batch of 32 here moved this scenario's `p50` from
// ~42 ns to ~966 ns, an order of magnitude too large to be the timing
// fix alone). At `BATCH == REFILL_EVERY` every batch contains at most
// one refill, always at its first index, exactly reproducing the
// original per-probe book-depth profile.
const BATCH: u64 = REFILL_EVERY;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id: u64 = 1;
    let mut op: u64 = 0;

    while op < MEASURED_OPS {
        let k = BATCH.min(MEASURED_OPS - op);

        // Unmeasured pre-pass: perform every refill due within this
        // batch of `k` probes *before* starting the batch's `Instant`
        // pair, so refills stay outside the timed region exactly like
        // the pre-batching version (only the IOC probes below are
        // timed).
        for idx in 0..k {
            if (op + idx).is_multiple_of(REFILL_EVERY) {
                for _ in 0..RESTING_PER_REFILL {
                    let _ = book.add_limit_order_with_user(
                        Id::from_u64(next_id),
                        rng.range(99, 101) as u128,
                        rng.range(1, 5),
                        Side::Sell,
                        TimeInForce::Gtc,
                        maker,
                        None,
                    );
                    next_id += 1;
                }
            }
        }

        record_batch(&mut hist, k, |_| {
            // IOC buy probe — frequently larger than the resting depth
            // so the engine ends up partial-filling and cancelling the
            // remainder.
            let id = Id::from_u64(next_id);
            next_id += 1;
            let qty = rng.range(1, 20);
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
        op += k;
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
