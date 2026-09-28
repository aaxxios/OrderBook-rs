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

// add_only_risk_hdr — pure passive limit-order entry with a `RiskConfig`
// installed, added for #258/#259.
//
// Every other HDR scenario in this suite runs on a book with no risk
// gating at all (`RiskState` is `None`, so every check is a no-op
// `Option::is_none` branch per `src/orderbook/risk.rs`'s module docs).
// That leaves the pre-trade risk layer added by #243 with no latency
// baseline of its own. This scenario is `add_only_hdr` unchanged in
// every other respect — same book geometry, same PRNG, same op mix —
// with a `RiskConfig` that enables all three checks
// (`max_open_orders_per_account`, `max_notional_per_account`,
// `price_band_bps`) but with limits wide enough that this workload never
// trips a rejection. That isolates the fixed per-op cost of walking the
// risk gate (counter lookups, the notional product, the price-band
// comparison) from the cost of a rejection branch, which is not what
// this scenario measures.
//
// Compare directly against `add_only`'s p50/p99/p99.9/p99.99: the delta
// is the risk-admission overhead on the passive-add path.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, persist, record, report, submit_gtc};
use orderbook_rs::{OrderBook, ReferencePriceSource, RiskConfig};

const SCENARIO: &str = "add_only_risk";
const WARMUP_OPS: u64 = 200_000;
const MEASURED_OPS: u64 = 1_000_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

fn main() {
    let mut book = OrderBook::<()>::new("BENCH");
    // Every limit generous enough that this workload (4 owners, prices
    // 99..=101, qty 1..=100, one-sided-friendly seeding via
    // `submit_gtc`) never trips a rejection — this scenario measures the
    // gate's fixed cost, not its rejection branch.
    book.set_risk_config(
        RiskConfig::new()
            .with_max_open_orders_per_account(10_000_000)
            .with_max_notional_per_account(u128::MAX / 4)
            .with_price_band_bps(1_000_000, ReferencePriceSource::Mid),
    );
    let book = book;

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
