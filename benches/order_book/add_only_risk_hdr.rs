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
// baseline of its own. This scenario has the same book geometry, PRNG
// and op-count shape as `add_only_hdr`, with a `RiskConfig` that
// enables all three checks (`max_open_orders_per_account`,
// `max_notional_per_account`, `price_band_bps`) but with limits wide
// enough that this workload never trips a rejection. That isolates the
// fixed per-op cost of walking the risk gate (counter lookups, the
// notional product, the price-band comparison) from the cost of a
// rejection branch, which is not what this scenario measures.
//
// Compare directly against `add_only`'s p50/p99/p99.9/p99.99: the delta
// is the risk-admission overhead on the passive-add path.
//
// # Non-crossing shape (issue #258 PR review)
//
// This does NOT reuse `hdr_common::submit_gtc`: that helper picks a
// random side *and* a price in the same tight `99..=101` band for every
// order, so once both sides have resting depth some submissions cross
// and execute rather than resting — not a passive-add-only workload,
// and on a one-sided book (which this shape produces intermittently)
// `ReferencePriceSource::Mid` has no midpoint to fall back to besides
// `LastTrade`, which is itself only set once a cross has happened.
// `submit_passive` below fixes both: bids always land in `90..=98`,
// asks always in `101..=109` — the two bands never overlap, so no
// submission here can ever cross the book — and once the first bid and
// first ask have rested (within the first two ops), `best_bid <
// best_ask` holds for the rest of the run, keeping `Mid` resolvable on
// every subsequent measured op.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, persist, pick_owner, record, report};
use orderbook_rs::{Id, OrderBook, ReferencePriceSource, RiskConfig, Side, TimeInForce};

const SCENARIO: &str = "add_only_risk";
const WARMUP_OPS: u64 = 200_000;
const MEASURED_OPS: u64 = 1_000_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

const BID_LO: u64 = 90;
const BID_HI: u64 = 98;
const ASK_LO: u64 = 101;
const ASK_HI: u64 = 109;
const QTY_LO: u64 = 1;
const QTY_HI: u64 = 100;

/// Deterministic non-crossing passive submit: `id`'s parity picks the
/// side (not `pick_side`'s coin flip) so bids and asks interleave
/// evenly from the very first op, rather than depending on the RNG to
/// eventually populate both sides.
fn submit_passive(book: &OrderBook<()>, rng: &mut Rng, id: u64) {
    let (side, price) = if id.is_multiple_of(2) {
        (Side::Buy, rng.range(BID_LO, BID_HI) as u128)
    } else {
        (Side::Sell, rng.range(ASK_LO, ASK_HI) as u128)
    };
    let qty = rng.range(QTY_LO, QTY_HI);
    let _ = book.add_limit_order_with_user(
        Id::from_u64(id),
        price,
        qty,
        side,
        TimeInForce::Gtc,
        pick_owner(rng),
        None,
    );
}

fn main() {
    let mut book = OrderBook::<()>::new("BENCH");
    // Every limit generous enough that this workload (4 owners, qty
    // 1..=100, non-crossing bid/ask bands) never trips a rejection —
    // this scenario measures the gate's fixed cost, not its rejection
    // branch.
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
        submit_passive(&book, &mut rng, i);
    }

    // Measurement — id space picks up where warmup stopped to avoid
    // collisions inside `order_locations`.
    for i in 0..MEASURED_OPS {
        let id = WARMUP_OPS + i;
        record(&mut hist, || submit_passive(&book, &mut rng, id));
    }

    report(SCENARIO, &hist);
    persist(SCENARIO, &hist).expect("persist hgrm");
}
