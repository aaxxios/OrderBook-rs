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

// reserve_sweep_hdr: IOC probes into reserve-maker books, comparing
// strandable (non-auto-replenishing) vs auto-replenishing hidden depth, a
// dense strandable level, and a mixed book whose strandable-maker gate is
// either left open or closed before the sweeps run (#230).
//
// #230 adds `capture_strandable_makers` to `match_order_inner`
// (`src/orderbook/matching.rs`) and `strandable_makers_resting`, an exact
// `AtomicUsize` count of currently resting non-auto-replenishing
// `ReserveOrder`s with hidden depth: incremented on admission and on
// snapshot restore, decremented on cancel, mass cancel, expiry, STP
// removal and fill. Each sweep reads the count once, before any level is
// touched. On a book where it reads zero, that single relaxed atomic
// load is the sweep's entire cost: no pool buffer is acquired,
// `capture_strandable_makers` is never called for any level, and the
// post-sweep drain does no lookup. While the count is greater than zero,
// every matching-capable submit and every cancel-then-add re-price runs
// under the exclusive submit gate instead of the shared side, and
// admitting a new strandable reserve is itself always exclusive, in
// every `STPMode`, so no strandable maker can be admitted, cancelled or
// replaced while a sweep is capturing against the count it read; that is
// what keeps a sweep's capture attribution exact. Only when the count is
// positive does each matched level get checked (`hidden_quantity() > 0`)
// and, if it still holds hidden depth, walked with
// `PriceLevel::iter_orders()` to record which resting non-auto reserves
// have hidden quantity behind them. That capture is what lets the engine
// report the hidden depth a sweep strands when `pricelevel` drops a
// depleted maker's hidden tranche instead of refreshing from it: an
// `INFO` trace plus the `orderbook_reserve_discards_total` /
// `orderbook_reserve_hidden_discarded_total` metrics. `iter_orders` is
// `DashMap::iter` upstream, which read-locks every shard of the map
// regardless of how few orders rest at the level, so the walk is not
// free on any level holding hidden depth. These benches measure that
// path; run this file on `main` and on this branch and diff. `main` has
// none of this machinery at all, so on `main` every scenario below is
// just its plain matching workload.
//
// Five scenarios, run back to back on fresh books:
//
// - `reserve_sweep_nonauto` (`auto_replenish: false`): `thin_book_sweep_hdr`
//   geometry (3 resting asks refilled every 5 ops, IOC buy probes qty
//   `1..=20`) with `ReserveOrder` resting makers. Every resting maker is
//   strandable, the gate opens on the first rest, and every level-match
//   that still holds hidden depth pays the `iter_orders()` walk plus (on
//   a fully-consumed maker) the discard report. This is the cost
//   `capture_strandable_makers` adds.
// - `reserve_sweep_auto` (`auto_replenish: true`): same geometry, but
//   nothing is strandable, so this book never opens the gate. Each sweep
//   pays the one gate load and nothing else; `capture_strandable_makers`
//   is never called. Must cost the same as `main`.
// - `reserve_sweep_dense_nonauto`: one price level (`DENSE_PRICE`)
//   holding `DENSE_LEVEL_DEPTH` non-auto `ReserveOrder` makers, refilled
//   back to `DENSE_LEVEL_DEPTH` whenever the level is fully consumed (not
//   timed). IOC buy probes qty `16..=96`, large enough that one probe
//   routinely strands many makers in a single sweep. Bounds the capture
//   pass over a dense level and the post-sweep drain, which looks up
//   every filled maker against the captured list.
// - `reserve_sweep_mixed_armed`: one non-auto `ReserveOrder` (10 visible,
//   20 hidden) rests once at `ARMING_PRICE`, far above every probe, so it
//   is never touched and the gate stays open for the whole run. The rest
//   of the book runs `thin_book_sweep_hdr` geometry with `IcebergOrder`
//   resting makers in place of `ReserveOrder` ones. An iceberg holds
//   hidden depth but can never match `capture_strandable_makers`'s
//   `ReserveOrder` pattern, so every sweep pays the `iter_orders()` walk
//   on a level holding hidden depth without ever finding anything
//   strandable there: the pure cost of an open gate on levels that were
//   never going to report anything.
// - `reserve_sweep_mixed_disarmed`: identical to `reserve_sweep_mixed_armed`
//   except the arming maker is cancelled with `cancel_order` right after
//   resting, before the first probe. The cancel decrements
//   `strandable_makers_resting` back to zero, closing the gate before the
//   measured loop starts, so every sweep for the rest of the run pays the
//   same single relaxed atomic load as `reserve_sweep_auto` and never
//   calls `capture_strandable_makers`. This is the case the maintainer
//   asked to be exercised, since leaving a maker resting at a distant
//   price, as `reserve_sweep_mixed_armed` does, never closes the gate at
//   all.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record, record_batch, report};
use hdrhistogram::Histogram;
use orderbook_rs::OrderBook;
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};

// Re-seed a thin slice of reserve makers every REFILL_EVERY ops so the
// book never goes fully empty across the measurement window, mirroring
// `thin_book_sweep_hdr`.
const RESTING_PER_REFILL: u64 = 3;
const REFILL_EVERY: u64 = 5;
const MEASURED_OPS: u64 = 200_000;
// Methodology (issue #258): `reserve_sweep_nonauto` reads a `p50` of
// 83 ns pre-fix (two host clock ticks) — `run_scenario` (which also
// produces `reserve_sweep_auto`, sharing the same loop) batches `BATCH`
// probes per `Instant` pair and records the per-op average instead; see
// `hdr_common::record_batch` for the trade-off. `BATCH` must equal
// `REFILL_EVERY` (not just divide it): a larger batch front-loads every
// refill due within it into the unmeasured pre-pass, so the first
// probes in the batch would sweep a book already topped up several
// refills ahead of where the original, unbatched loop would have had it
// — a deeper-than-intended book changes the workload, not just how it
// is timed (this exact bug was caught in `thin_book_sweep_hdr`, which
// shares this file's geometry — see that file's comment). At `BATCH ==
// REFILL_EVERY` every batch contains at most one refill, always at its
// first index, exactly reproducing the original per-probe book-depth
// profile. `run_dense_nonauto` (tens of us/op) and `run_mixed`
// (~1.2 us/op, both comfortably above the tick even per single op) are
// deliberately left on single-op `record` — see their own comments.
const BATCH: u64 = REFILL_EVERY;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

const SCENARIOS: [(&str, bool); 2] = [
    ("reserve_sweep_nonauto", false),
    ("reserve_sweep_auto", true),
];

// `reserve_sweep_dense_nonauto`: one level, many strandable makers.
const DENSE_PRICE: u128 = 100;
const DENSE_LEVEL_DEPTH: u64 = 64;

// `reserve_sweep_mixed_armed`: one permanently-resting maker, far above
// every probe price (probes never reach above 101), keeps the
// strandable-maker gate open without ever being matched itself.
const ARMING_PRICE: u128 = 10_000;

/// Run one scenario on a fresh book: refill `RESTING_PER_REFILL` resting
/// `ReserveOrder` asks (`auto_replenish` as given) every `REFILL_EVERY`
/// ops (not timed), then time an IOC buy probe against the book. Returns
/// the probe-latency histogram.
fn run_scenario(auto_replenish: bool) -> Histogram<u64> {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id: u64 = 1;
    let mut op: u64 = 0;

    while op < MEASURED_OPS {
        let k = BATCH.min(MEASURED_OPS - op);

        // Unmeasured pre-pass: every refill due within this batch of
        // `k` probes, before the batch's `Instant` pair starts.
        for idx in 0..k {
            if (op + idx).is_multiple_of(REFILL_EVERY) {
                for _ in 0..RESTING_PER_REFILL {
                    let _ = book.add_order(OrderType::ReserveOrder {
                        id: Id::from_u64(next_id),
                        price: Price::new(rng.range(99, 101) as u128),
                        visible_quantity: Quantity::new(rng.range(1, 5)),
                        hidden_quantity: Quantity::new(rng.range(4, 12)),
                        side: Side::Sell,
                        user_id: maker,
                        timestamp: TimestampMs::new(0),
                        time_in_force: TimeInForce::Gtc,
                        replenish_threshold: Quantity::new(0),
                        replenish_amount: None,
                        auto_replenish,
                        extra_fields: (),
                    });
                    next_id += 1;
                }
            }
        }

        // IOC buy probes, frequently larger than the resting visible
        // tranche, so the engine partial-fills and, on the nonauto
        // scenario, strands (and reports) hidden depth.
        record_batch(&mut hist, k, |_| {
            let id = Id::from_u64(next_id);
            next_id += 1;
            let qty = rng.range(1, 20);
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
        op += k;
    }

    hist
}

/// Top `DENSE_PRICE` back up to `DENSE_LEVEL_DEPTH` non-auto `ReserveOrder`
/// makers. Called once before `run_dense_nonauto`'s measured loop and
/// again every time the level empties out inside it.
fn refill_dense_level(book: &OrderBook<()>, rng: &mut Rng, maker: Hash32, next_id: &mut u64) {
    for _ in 0..DENSE_LEVEL_DEPTH {
        let _ = book.add_order(OrderType::ReserveOrder {
            id: Id::from_u64(*next_id),
            price: Price::new(DENSE_PRICE),
            visible_quantity: Quantity::new(rng.range(1, 2)),
            hidden_quantity: Quantity::new(rng.range(4, 12)),
            side: Side::Sell,
            user_id: maker,
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: None,
            auto_replenish: false,
            extra_fields: (),
        });
        *next_id += 1;
    }
}

/// Run `reserve_sweep_dense_nonauto` on a fresh book: `DENSE_LEVEL_DEPTH`
/// non-auto reserve makers stacked on one level, refilled whenever the
/// level is fully consumed (not timed), then time an IOC buy probe with
/// qty `16..=96` against them, large enough that one probe routinely
/// strands several makers from that single level.
fn run_dense_nonauto() -> Histogram<u64> {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id: u64 = 1;

    refill_dense_level(&book, &mut rng, maker, &mut next_id);

    // Not batched (issue #258): this scenario's op cost is tens of us
    // (see BENCH.md), several orders of magnitude above the host clock
    // tick, so a single `Instant` pair per probe already measures the
    // operation, not the clock. `record` (not `record_batch`) keeps
    // this scenario's true per-op tail, including the reactive refill's
    // contribution when a probe drains the level — that refill is
    // itself part of what a real IOC probe here can trigger, so, unlike
    // the fixed-cadence refills elsewhere in this file, it belongs
    // inside the timed call, not in an unmeasured pre-pass.
    for _ in 0..MEASURED_OPS {
        if book.order_count_at_price(DENSE_PRICE, Side::Sell).is_none() {
            refill_dense_level(&book, &mut rng, maker, &mut next_id);
        }

        let id = Id::from_u64(next_id);
        next_id += 1;
        let qty = rng.range(16, 96);
        record(&mut hist, || {
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
    }

    hist
}

/// Run `reserve_sweep_mixed_armed` (`cancel_arming: false`) or
/// `reserve_sweep_mixed_disarmed` (`cancel_arming: true`) on a fresh
/// book. Either way, one non-auto reserve maker rests once at
/// `ARMING_PRICE`, not timed, far above every probe price so no probe
/// ever matches it. When `cancel_arming` is set, that maker is cancelled
/// immediately afterward, before the first probe, so the book holds no
/// resting strandable maker for the rest of the run; otherwise it keeps
/// resting untouched for the whole run, the only difference between the
/// two scenarios. The rest of the book then runs the `thin_book_sweep_hdr`
/// refill/probe cadence with `IcebergOrder` resting makers, none of which
/// `capture_strandable_makers` can ever report.
fn run_mixed(cancel_arming: bool) -> Histogram<u64> {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id: u64 = 1;

    let arming_id = Id::from_u64(next_id);
    let _ = book.add_order(OrderType::ReserveOrder {
        id: arming_id,
        price: Price::new(ARMING_PRICE),
        visible_quantity: Quantity::new(10),
        hidden_quantity: Quantity::new(20),
        side: Side::Sell,
        user_id: maker,
        timestamp: TimestampMs::new(0),
        time_in_force: TimeInForce::Gtc,
        replenish_threshold: Quantity::new(0),
        replenish_amount: None,
        auto_replenish: false,
        extra_fields: (),
    });
    next_id += 1;
    if cancel_arming {
        // Not timed: disarm before the first probe, so the book holds no
        // resting strandable maker once the measured loop starts.
        let _ = book.cancel_order(arming_id);
    }

    for i in 0..MEASURED_OPS {
        if i % REFILL_EVERY == 0 {
            // Drop a few resting iceberg asks. No measurement around the
            // refill; only the IOC probe below is timed.
            for _ in 0..RESTING_PER_REFILL {
                let _ = book.add_order(OrderType::IcebergOrder {
                    id: Id::from_u64(next_id),
                    price: Price::new(rng.range(99, 101) as u128),
                    visible_quantity: Quantity::new(rng.range(1, 5)),
                    hidden_quantity: Quantity::new(rng.range(4, 12)),
                    side: Side::Sell,
                    user_id: maker,
                    timestamp: TimestampMs::new(0),
                    time_in_force: TimeInForce::Gtc,
                    extra_fields: (),
                });
                next_id += 1;
            }
        }

        // IOC buy probe against the iceberg makers; the reserve at
        // `ARMING_PRICE` (cancelled or not) is never a candidate, no
        // probe qty comes close.
        let id = Id::from_u64(next_id);
        next_id += 1;
        let qty = rng.range(1, 20);
        record(&mut hist, || {
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
    }

    hist
}

fn main() {
    for (scenario, auto_replenish) in SCENARIOS {
        let hist = run_scenario(auto_replenish);
        report(scenario, &hist);
        persist(scenario, &hist).expect("persist hgrm");
    }

    let dense_hist = run_dense_nonauto();
    report("reserve_sweep_dense_nonauto", &dense_hist);
    persist("reserve_sweep_dense_nonauto", &dense_hist).expect("persist hgrm");

    let mixed_armed_hist = run_mixed(false);
    report("reserve_sweep_mixed_armed", &mixed_armed_hist);
    persist("reserve_sweep_mixed_armed", &mixed_armed_hist).expect("persist hgrm");

    let mixed_disarmed_hist = run_mixed(true);
    report("reserve_sweep_mixed_disarmed", &mixed_disarmed_hist);
    persist("reserve_sweep_mixed_disarmed", &mixed_disarmed_hist).expect("persist hgrm");
}
