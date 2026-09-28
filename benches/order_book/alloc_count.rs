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
// alloc_count — feature-gated allocation profile of the mixed
// 70/20/10 hot-path workload. Reports `allocs_per_op` and a
// per-counter delta over a measurement window.
//
// Build / run:
//
//     cargo bench --features alloc-counters --bench alloc_count
#![cfg(feature = "alloc-counters")]

#[path = "hdr_common.rs"]
mod common;

use orderbook_rs::utils::CountingAllocator;
use std::alloc::System;

#[global_allocator]
static GLOBAL: CountingAllocator<System> = CountingAllocator::new(System);

use common::{Rng, pick_owner, pick_side};
use orderbook_rs::OrderBook;
use pricelevel::{Hash32, Id, Side, TimeInForce};

const SCENARIO: &str = "alloc_count_mixed_70_20_10";
const WARMUP_OPS: u64 = 200_000;
const MEASURED_OPS: u64 = 1_000_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

// Passive-add-only scenarios (issue #259 profiling lead, comment on
// #262): "1,000 passive adds on one price level: about 6.7 allocations
// and 18 KB allocated per add (18.1 MB total), with and without a user
// account" and "1,000 adds on distinct levels: 12.6 allocations and
// about 10 KB per add." `alloc_count_mixed_70_20_10` above never
// isolates a passive add from the cancel/aggressive traffic mixed in
// with it; these three scenarios reproduce the profiling lead's exact
// shape (warmup 1_000, measured 1_000, non-crossing Buy limit orders
// only) so the allocation floor of a single passive add is visible on
// its own, both confirming the lead's numbers and giving future
// `pricelevel` upgrades a dedicated regression signal.
const PASSIVE_WARMUP_OPS: u64 = 1_000;
const PASSIVE_MEASURED_OPS: u64 = 1_000;
const ONE_LEVEL_PRICE: u128 = 1_000;

fn account(byte: u8) -> Hash32 {
    let mut bytes = [0u8; 32];
    bytes[0] = byte;
    Hash32::new(bytes)
}

#[derive(Clone, Copy)]
enum Op {
    Submit,
    Cancel,
    Aggressive,
}

fn pick_op(rng: &mut Rng) -> Op {
    let v = rng.next() % 100;
    if v < 70 {
        Op::Submit
    } else if v < 90 {
        Op::Cancel
    } else {
        Op::Aggressive
    }
}

fn apply(book: &orderbook_rs::OrderBook<()>, rng: &mut Rng, next_id: &mut u64, op: Op) {
    match op {
        Op::Submit => {
            let id = Id::from_u64(*next_id);
            *next_id += 1;
            let price = rng.range(common::PRICE_LO, common::PRICE_HI) as u128;
            let qty = rng.range(common::QTY_LO, common::QTY_HI);
            let _ = book.add_limit_order_with_user(
                id,
                price,
                qty,
                pick_side(rng),
                TimeInForce::Gtc,
                pick_owner(rng),
                None,
            );
        }
        Op::Cancel => {
            if *next_id > 1 {
                let target = rng.range(1, *next_id - 1);
                let _ = book.cancel_order(Id::from_u64(target));
            }
        }
        Op::Aggressive => {
            let id = Id::from_u64(*next_id);
            *next_id += 1;
            let qty = rng.range(1, 10);
            let _ = book.submit_market_order_with_user(id, qty, pick_side(rng), pick_owner(rng));
        }
    }
}

/// Print the fixed-format console report and persist
/// `target/alloc-counters/<scenario>.md`, shared by every scenario in
/// this binary.
fn report(
    scenario: &str,
    warmup_ops: u64,
    measured_ops: u64,
    before: &orderbook_rs::AllocSnapshot,
) {
    let after = GLOBAL.snapshot();
    let delta = after
        .since(*before)
        .expect("allocation counters are monotonic");

    let allocs_per_op = delta.allocs as f64 / measured_ops as f64;
    let bytes_per_op = delta.bytes_allocated as f64 / measured_ops as f64;

    println!("scenario        : {scenario}");
    println!("warmup ops      : {warmup_ops}");
    println!("measured ops    : {measured_ops}");
    println!("allocs          : {}", delta.allocs);
    println!("deallocs        : {}", delta.deallocs);
    println!("bytes_alloc     : {}", delta.bytes_allocated);
    println!("bytes_dealloc   : {}", delta.bytes_deallocated);
    println!("allocs/op       : {allocs_per_op:.4}");
    println!("bytes_alloc/op  : {bytes_per_op:.2}");

    let summary = format!(
        "# {scenario}\n\
         \n\
         | counter         | value                |\n\
         |-----------------|----------------------|\n\
         | warmup_ops      | {warmup_ops}        |\n\
         | measured_ops    | {measured_ops}      |\n\
         | allocs          | {}                  |\n\
         | deallocs        | {}                  |\n\
         | bytes_alloc     | {}                  |\n\
         | bytes_dealloc   | {}                  |\n\
         | allocs/op       | {allocs_per_op:.4}  |\n\
         | bytes_alloc/op  | {bytes_per_op:.2}   |\n",
        delta.allocs, delta.deallocs, delta.bytes_allocated, delta.bytes_deallocated,
    );
    let _ = std::fs::create_dir_all("target/alloc-counters");
    let path = format!("target/alloc-counters/{scenario}.md");
    if let Err(e) = std::fs::write(&path, summary) {
        eprintln!("could not write {path}: {e}");
    } else {
        eprintln!("wrote {path}");
    }
}

/// Passive, non-crossing Buy limit adds only — no cancels, no
/// aggressive orders — at either one shared price level or one distinct
/// level per order, with or without a `user_id`. Reproduces the #259
/// profiling-lead shape exactly (see the module-level comment above).
fn run_passive_add_scenario(scenario: &str, one_level: bool, with_user: bool) {
    let book: OrderBook<()> = OrderBook::new("BENCH");
    let user = with_user.then(|| account(1));

    let submit = |book: &OrderBook<()>, id: u64| {
        let price = if one_level {
            ONE_LEVEL_PRICE
        } else {
            ONE_LEVEL_PRICE + id as u128
        };
        match user {
            Some(u) => {
                let _ = book.add_limit_order_with_user(
                    Id::from_u64(id),
                    price,
                    10,
                    Side::Buy,
                    TimeInForce::Gtc,
                    u,
                    None,
                );
            }
            None => {
                let _ = book.add_limit_order(
                    Id::from_u64(id),
                    price,
                    10,
                    Side::Buy,
                    TimeInForce::Gtc,
                    None,
                );
            }
        }
    };

    // Warmup — discarded.
    for i in 0..PASSIVE_WARMUP_OPS {
        submit(&book, i + 1);
    }

    let before = GLOBAL.snapshot();
    for i in 0..PASSIVE_MEASURED_OPS {
        submit(&book, PASSIVE_WARMUP_OPS + i + 1);
    }

    report(scenario, PASSIVE_WARMUP_OPS, PASSIVE_MEASURED_OPS, &before);
}

fn main() {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut next_id: u64 = 1;

    // Warmup — discarded.
    for _ in 0..WARMUP_OPS {
        let op = pick_op(&mut rng);
        apply(&book, &mut rng, &mut next_id, op);
    }

    // Capture pre-measurement counters.
    let before = GLOBAL.snapshot();

    for _ in 0..MEASURED_OPS {
        let op = pick_op(&mut rng);
        apply(&book, &mut rng, &mut next_id, op);
    }

    report(SCENARIO, WARMUP_OPS, MEASURED_OPS, &before);

    run_passive_add_scenario("alloc_count_add_only_one_level_with_user", true, true);
    run_passive_add_scenario("alloc_count_add_only_one_level_no_user", true, false);
    run_passive_add_scenario(
        "alloc_count_add_only_distinct_levels_with_user",
        false,
        true,
    );
}
