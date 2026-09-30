// This crate root is entirely bench code, not production (issue #242's
// Production Panic Policy gate, `[lints.clippy]` in `Cargo.toml`, is
// package-wide and would otherwise apply here too). Bench fixtures freely
// `.unwrap()` / `.expect()` setup and do raw arithmetic on sample sizes;
// none of that reaches `src/`.
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

// pending_stops_hdr — tail latency of books holding pending trailing
// stops (#286, `special_orders`; Copilot / hot-path review on #301).
//
// Scenarios (N = pending stops):
//
// - `pending_stops_quiet_{0,10,1000}`: market buys of 1 at the same
//   price (1000) that neither trail nor elect the stops (sell stop 500,
//   watermark 1000). N = 0 is the control with the same setup. Batched
//   (`BATCH` ops per clock pair, see `hdr_common::record_batch`).
// - `pending_stops_trailing_{10,1000}`: every market buy of 1 prints a new
//   high and trails (re-keys) all N stops. One clock pair per op.
// - `pending_stops_elect_{10,1000}`: one market sell of 1 elects all N
//   stops at one print; each runs its market order into a deep bid. The
//   book is re-seeded between samples, untimed.
// - `pending_stops_cascade_{10,1000}`: one market sell elects the first
//   of N stops (5000, 4999, ...), whose market order elects the next, and
//   so on down a ladder of one-lot bids. Re-seeded between samples,
//   untimed.
// - `pending_stops_elect_collar_{10,1000}` / `pending_stops_cascade_collar_
//   {10,1000}` (#302): the same with a `StopProtection` collar, so each
//   child is an IOC limit (elect: collar 10, every child still fills at
//   900; cascade: collar 1, each child fills the next one-lot bid and the
//   last one finds nothing within its band instead of the deep bid).

#[path = "hdr_common.rs"]
mod common;

use common::{new_histogram, owner, persist, record, record_batch, report};
use orderbook_rs::{OrderBook, StopProtection};
use pricelevel::{Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};

const BATCH: u64 = 32;

fn trailing_stop(id: u64, stop: u128, watermark: u128, qty: u64) -> OrderType<()> {
    OrderType::TrailingStop {
        id: Id::from_u64(id),
        price: Price::new(stop),
        quantity: Quantity::new(qty),
        side: Side::Sell,
        user_id: owner(0x55),
        timestamp: TimestampMs::new(0),
        time_in_force: TimeInForce::Gtc,
        trail_amount: Quantity::new((watermark - stop) as u64),
        last_reference_price: Price::new(watermark),
        extra_fields: (),
    }
}

/// Monotonic id source (ids never collide across a scenario).
struct Ids(u64);

impl Ids {
    fn next(&mut self) -> Id {
        self.0 += 1;
        Id::from_u64(self.0)
    }
    fn next_raw(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

fn limit(book: &OrderBook<()>, ids: &mut Ids, price: u128, qty: u64, side: Side) {
    book.add_limit_order_with_user(
        ids.next(),
        price,
        qty,
        side,
        TimeInForce::Gtc,
        owner(0xAA),
        None,
    )
    .unwrap();
}

/// A trade at 1000 so no stop admitted afterwards below 1000 is crossed.
fn print_at_1000(book: &OrderBook<()>, ids: &mut Ids) {
    print_at(book, ids, 1000);
}

/// A trade at `price`.
fn print_at(book: &OrderBook<()>, ids: &mut Ids, price: u128) {
    limit(book, ids, price, 1, Side::Sell);
    book.submit_market_order_with_user(ids.next(), 1, Side::Buy, owner(0xBB))
        .unwrap();
}

fn quiet(n: u64) {
    let scenario = format!("pending_stops_quiet_{n}");
    let book = common::fresh_book();
    let mut ids = Ids(0);
    print_at_1000(&book, &mut ids);
    for _ in 0..n {
        let raw = ids.next_raw();
        book.add_order(trailing_stop(raw, 500, 1000, 1)).unwrap();
    }
    let mut hist = new_histogram();
    let mut done = 0u64;
    while done < 100_000 {
        // Refill the ask at 1000 (untimed), then BATCH market buys of 1.
        limit(&book, &mut ids, 1000, BATCH, Side::Sell);
        let first = ids.0;
        ids.0 += BATCH;
        record_batch(&mut hist, BATCH, |j| {
            let _ = book.submit_market_order_with_user(
                Id::from_u64(first + 1 + j),
                1,
                Side::Buy,
                owner(0xBB),
            );
        });
        done += BATCH;
    }
    report(&scenario, &hist);
    persist(&scenario, &hist).expect("persist hgrm");
}

fn trailing(n: u64, samples: u64) {
    let scenario = format!("pending_stops_trailing_{n}");
    let book = common::fresh_book();
    let mut ids = Ids(0);
    print_at_1000(&book, &mut ids);
    for _ in 0..n {
        let raw = ids.next_raw();
        book.add_order(trailing_stop(raw, 500, 1000, 1)).unwrap();
    }
    let mut hist = new_histogram();
    let mut price = 1000u128;
    for _ in 0..samples {
        price += 1;
        limit(&book, &mut ids, price, 1, Side::Sell);
        let taker = ids.next();
        record(&mut hist, || {
            book.submit_market_order_with_user(taker, 1, Side::Buy, owner(0xBB))
        })
        .unwrap();
    }
    report(&scenario, &hist);
    persist(&scenario, &hist).expect("persist hgrm");
}

/// `name` with a `_collar` infix when `collar` is set, and a book with it.
fn book_for(name: &str, n: u64, collar: Option<u128>) -> (String, OrderBook<()>) {
    let mut book = common::fresh_book();
    let scenario = match collar {
        Some(units) => {
            book.set_stop_protection(Some(StopProtection::try_new(units).unwrap()))
                .unwrap();
            format!("{name}_collar_{n}")
        }
        None => format!("{name}_{n}"),
    };
    (scenario, book)
}

fn elect(n: u64, samples: u64, collar: Option<u128>) {
    let (scenario, book) = book_for("pending_stops_elect", n, collar);
    let mut ids = Ids(0);
    limit(&book, &mut ids, 900, u64::MAX / 4, Side::Buy);
    let mut hist = new_histogram();
    for _ in 0..samples {
        print_at_1000(&book, &mut ids);
        for _ in 0..n {
            let raw = ids.next_raw();
            book.add_order(trailing_stop(raw, 900, 1000, 1)).unwrap();
        }
        let taker = ids.next();
        record(&mut hist, || {
            book.submit_market_order_with_user(taker, 1, Side::Sell, owner(0xBB))
        })
        .unwrap();
        assert_eq!(book.trailing_stop_count(), 0, "every stop elected");
    }
    report(&scenario, &hist);
    persist(&scenario, &hist).expect("persist hgrm");
}

fn cascade(n: u64, samples: u64, collar: Option<u128>) {
    let (scenario, book) = book_for("pending_stops_cascade", n, collar);
    let mut ids = Ids(0);
    limit(&book, &mut ids, 100, u64::MAX / 4, Side::Buy);
    let mut hist = new_histogram();
    for _ in 0..samples {
        // Ladder top 5000 leaves room for 1000 one-tick steps.
        print_at(&book, &mut ids, 6000);
        limit(&book, &mut ids, 5001, 1, Side::Buy);
        for k in 0..u128::from(n) {
            limit(&book, &mut ids, 5000 - k, 1, Side::Buy);
            let raw = ids.next_raw();
            book.add_order(trailing_stop(raw, 5000 - k, 6000, 1))
                .unwrap();
        }
        let taker = ids.next();
        record(&mut hist, || {
            book.submit_market_order_with_user(taker, 2, Side::Sell, owner(0xBB))
        })
        .unwrap();
        assert_eq!(book.trailing_stop_count(), 0, "the cascade ran through");
    }
    report(&scenario, &hist);
    persist(&scenario, &hist).expect("persist hgrm");
}

fn main() {
    for n in [0, 10, 1000] {
        quiet(n);
    }
    trailing(10, 50_000);
    trailing(1000, 5_000);
    for collar in [None, Some(10)] {
        elect(10, 5_000, collar);
        elect(1000, 200, collar);
    }
    for collar in [None, Some(1)] {
        cascade(10, 5_000, collar);
        cascade(1000, 200, collar);
    }
}
