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

// stp_sweep_hdr — aggressive self-crossing sweep under STP `CancelMaker`.
// Every measured op crosses a multi-level book where the near-best levels
// hold both a same-user (taker) resting maker and other-user maker depth,
// so the per-level STP scan finds a maker to cancel inline before falling
// through to real foreign liquidity. Exercises the pooled snapshot scan
// path (#107).
//
// Foreign liquidity profile (#225). The original version seeded
// `OTHER_PER_LEVEL` other-maker orders once, up front, and never refilled
// them. Foreign depth was gone within a few hundred of the 100_000
// measured ops, so almost the entire measured window swept a book with no
// real liquidity left to fill against — not representative. This version
// tops the levels the sweep is currently walking back up (unmeasured, via
// `order_count_at_price`) before every measured op, so foreign depth
// survives the whole run instead of only existing at t=0.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record, report};
use orderbook_rs::{OrderBook, STPMode};
use pricelevel::{Id, Side, TimeInForce};

const SCENARIO: &str = "stp_sweep";
// Other-maker depth per level — the liquidity the taker actually fills
// against after its own same-user makers are cancelled by STP.
const OTHER_PER_LEVEL: u64 = 8;
const NUM_LEVELS: u64 = 50;
// Levels, starting at the current best ask, kept topped up to
// `OTHER_PER_LEVEL` other-maker orders before every measured op. This is
// the "first few levels" window the sweep actually lives in; levels
// beyond it keep only their initial seed and are touched, if at all, by
// the rare multi-level sweep.
const TOPUP_WINDOW: u64 = 5;
const MEASURED_OPS: u64 = 100_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    // STP `CancelMaker`: an incoming taker order that would self-cross cancels
    // the resting same-user makers per level and keeps matching. This is the
    // path that re-scans the pooled snapshot buffer.
    let book = OrderBook::<()>::with_stp_mode("BENCH", STPMode::CancelMaker);
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();

    let taker = owner(0xBB);
    let other = owner(0xCC);

    // Seed each ask level with other-maker Sell depth plus at least one
    // taker-owned Sell, so every measured Buy sweep hits the CancelMaker scan
    // (a same-user maker to cancel) before filling against the other maker.
    let mut next_id = 1u64;
    for level in 0..NUM_LEVELS {
        let price = (100 + level) as u128;
        // One taker-owned resting Sell at this level — the STP target.
        let _ = book.add_limit_order_with_user(
            Id::from_u64(next_id),
            price,
            rng.range(1, 5),
            Side::Sell,
            TimeInForce::Gtc,
            taker,
            None,
        );
        next_id += 1;
        // Other-maker depth the taker actually fills against.
        for _ in 0..OTHER_PER_LEVEL {
            let _ = book.add_limit_order_with_user(
                Id::from_u64(next_id),
                price,
                rng.range(1, 10),
                Side::Sell,
                TimeInForce::Gtc,
                other,
                None,
            );
            next_id += 1;
        }
    }

    // Aggressive self-crossing Buys from the taker. Each crosses the best ask
    // levels: the per-level STP scan finds the taker's own resting Sell and
    // cancels it inline, then matches against the other maker's depth.
    for _ in 0..MEASURED_OPS {
        // Unmeasured: keep the levels the sweep is currently walking through
        // topped up with other-maker depth. `order_count_at_price` also
        // counts a taker-owned order when one is still resting at that
        // price, so this slightly under-tops (by at most the one taker
        // order per level) rather than over-tops — a fine approximation for
        // a liquidity *profile*, not a precise accounting (#225).
        let best = book.best_ask().unwrap_or(100);
        for w in 0..TOPUP_WINDOW {
            let price = best + w as u128;
            let resting = book.order_count_at_price(price, Side::Sell).unwrap_or(0) as u64;
            for _ in resting..OTHER_PER_LEVEL {
                let _ = book.add_limit_order_with_user(
                    Id::from_u64(next_id),
                    price,
                    rng.range(1, 10),
                    Side::Sell,
                    TimeInForce::Gtc,
                    other,
                    None,
                );
                next_id += 1;
            }
        }

        // Unmeasured: re-seed a fresh taker-owned Sell at the best level so
        // every measured op still exercises the CancelMaker scan.
        let _ = book.add_limit_order_with_user(
            Id::from_u64(next_id),
            best,
            1,
            Side::Sell,
            TimeInForce::Gtc,
            taker,
            None,
        );
        next_id += 1;

        let qty = rng.range(5, 20);
        let id = Id::from_u64(next_id);
        next_id += 1;
        record(&mut hist, || {
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
