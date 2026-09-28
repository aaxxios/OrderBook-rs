//! Allocation-budget regression test for the mixed hot-path workload.
//!
//! Feature-gated on `alloc-counters`. Runs 10 000 mixed ops after a
//! 1 000-op warmup and asserts the per-op allocation count stays
//! below a conservative ceiling tuned to catch regressions, **not**
//! to certify zero — `DashMap` + `SkipMap` allocate during bucket
//! grow on early submissions and that is fine.
//!
//! The ceiling is intentionally loose so the test does not flake on
//! shard-grow events or platform-specific allocator behaviour. A real
//! "one alloc per regression" guard belongs in the bench output's
//! tighter floor. This integration test is the CI guard.

// This crate root is entirely integration-test code, not production
// (issue #242's Production Panic Policy gate, `[lints.clippy]` in
// `Cargo.toml`, is package-wide and would otherwise apply here too).
// Test fixtures freely `.unwrap()` / `.expect()` setup, index scratch
// buffers and do raw arithmetic on sample sizes; none of that reaches
// `src/`. Panics and assertions in tests are permitted by
// `rules/global_rules.md`'s Testing section.
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
#![cfg(feature = "alloc-counters")]

use orderbook_rs::OrderBook;
use orderbook_rs::utils::CountingAllocator;
use pricelevel::{Hash32, Id, Side, TimeInForce};
use std::alloc::System;

#[global_allocator]
static GLOBAL: CountingAllocator<System> = CountingAllocator::new(System);

const WARMUP_OPS: u64 = 1_000;
const MEASURED_OPS: u64 = 10_000;
// Ceiling on the median allocs/op across `WINDOWS` windows. Measured on
// 0.13.1 (#262): medians range from about 6.5 to 10.4 across runs (debug
// and release alike). 15.0 sits about 45% above the worst observed median,
// so it catches a structural regression (an extra allocation on every op
// is +1/op; a per-order `Vec` in the hot path is several) without
// flipping on noise.
const ALLOCS_PER_OP_CEILING: f64 = 15.0;
// Independent measured windows; the ceiling applies to their median.
const WINDOWS: usize = 7;

fn account(byte: u8) -> Hash32 {
    let mut bytes = [0u8; 32];
    bytes[0] = byte;
    Hash32::new(bytes)
}

fn run_workload(book: &OrderBook<()>, count: u64, base: u64) {
    let acct = account(1);
    for i in 0..count {
        let id = Id::from_u64(base + i);
        let bucket = (base + i) % 5;
        match bucket {
            0..=2 => {
                let _ = book.add_limit_order_with_user(
                    id,
                    100 + (bucket as u128),
                    1 + (i % 10),
                    Side::Buy,
                    TimeInForce::Gtc,
                    acct,
                    None,
                );
            }
            3 => {
                let target = Id::from_u64(base + i.saturating_sub(1));
                let _ = book.cancel_order(target);
            }
            _ => {
                let _ = book.submit_market_order_with_user(id, 1, Side::Sell, acct);
            }
        }
    }
}

/// Build a seeded book, warm it up, then count allocations across one
/// measured window of `MEASURED_OPS` mixed ops.
fn measure_window() -> f64 {
    let book = OrderBook::<()>::new("BUDGET");

    // Seed liquidity so cancels and aggressive market orders find
    // something to interact with.
    for i in 0..50 {
        let _ = book.add_limit_order_with_user(
            Id::from_u64(1_000_000 + i),
            100,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            account(2),
            None,
        );
    }

    run_workload(&book, WARMUP_OPS, 1);
    let before = GLOBAL.snapshot();
    run_workload(&book, MEASURED_OPS, WARMUP_OPS + 1);
    let after = GLOBAL.snapshot();

    let delta = after
        .since(before)
        .expect("allocation counters are monotonic");
    delta.allocs as f64 / MEASURED_OPS as f64
}

#[test]
fn alloc_budget_mixed_workload_stays_under_ceiling() {
    // The workload is deterministic (fixed ids, prices and order of
    // operations), but the process-wide allocation count is not: the
    // counter sees every thread and `crossbeam-epoch` allocates
    // deferred-free bags on a schedule that depends on epoch advancement.
    // Measured on 0.13.1 (#262), one window ranged from about 5.7 to 12.3
    // allocs/op across runs, while the median of several windows (fresh
    // book each) stayed within about 6.5 to 10.4. The ceiling is asserted
    // on that median.
    let mut samples: Vec<f64> = (0..WINDOWS).map(|_| measure_window()).collect();
    samples.sort_by(f64::total_cmp);
    let median = samples[WINDOWS / 2];

    assert!(
        median < ALLOCS_PER_OP_CEILING,
        "alloc-budget regression: median {:.4} allocs/op over {} windows of {} ops \
         (ceiling {:.4}); samples {:?}",
        median,
        WINDOWS,
        MEASURED_OPS,
        ALLOCS_PER_OP_CEILING,
        samples,
    );
}
