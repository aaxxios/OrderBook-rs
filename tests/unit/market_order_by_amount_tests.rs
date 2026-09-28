//! Integration tests for the quote-notional market-order path
//! (`match_market_order_by_amount` and friends).
//!
//! Covers the acceptance criteria from issue #85:
//!
//! - Single- and multi-level fills against ask / bid walls.
//! - Dust-below-best-price stops the walk cleanly (no `qty=0` trade).
//! - Lot-size enforcement (per-level qty rounded down to multiple of lot).
//! - Empty / exhausted-book error paths
//!   (`InsufficientLiquidityNotional`).
//! - Sell-side symmetry.
//! - Fee schedule applied to the resulting `TradeResult`
//!   (`amount` is exclusive — caller pays `amount + taker_fee`).
//! - `TradeListener` invoked exactly once per match with
//!   `quote_notional` populated.

use orderbook_rs::orderbook::trade::{TradeListener, TradeResult};
use orderbook_rs::{FeeSchedule, OrderBook, OrderBookError};
use pricelevel::{Id, Quantity, Side, TimeInForce};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Fresh random order id (UUID v4).
fn new_id() -> Id {
    Id::from_uuid(uuid::Uuid::new_v4())
}

/// Seed an ask wall: three asks at the supplied prices, each `qty` size,
/// using fresh ids. Helpful for setting up most happy-path tests.
fn seed_asks(book: &OrderBook<()>, prices: &[u128], qty: u64) {
    for &price in prices {
        book.add_limit_order(new_id(), price, qty, Side::Sell, TimeInForce::Gtc, None)
            .expect("seed ask");
    }
}

/// Seed a bid wall mirroring `seed_asks` for sell-side tests.
fn seed_bids(book: &OrderBook<()>, prices: &[u128], qty: u64) {
    for &price in prices {
        book.add_limit_order(new_id(), price, qty, Side::Buy, TimeInForce::Gtc, None)
            .expect("seed bid");
    }
}

#[test]
fn test_buy_single_level_exact_fit() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 100);

    // 100 * 50 = 5_000 fills exactly at best ask, leaves 50 resting.
    let result = book
        .match_market_order_by_amount(new_id(), 5_000, Side::Buy)
        .expect("notional buy must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].price().as_u128(), 100);
    assert_eq!(trades[0].quantity().as_u64(), 50);
    assert_eq!(result.executed_value().expect("executed value"), 5_000);
    assert_eq!(
        result.remaining_quantity(),
        Quantity::new(0),
        "notional path must not leak the u64::MAX working sentinel"
    );
    assert!(result.is_complete());
}

#[test]
fn test_buy_walks_three_levels() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100, 101, 102], 10);

    // 100*10 + 101*10 + 102*10 = 1_000 + 1_010 + 1_020 = 3_030 sweeps all.
    let result = book
        .match_market_order_by_amount(new_id(), 3_030, Side::Buy)
        .expect("notional buy must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 3);
    assert_eq!(result.executed_value().expect("executed value"), 3_030);
    assert_eq!(
        result.executed_quantity().expect("executed qty"),
        Quantity::new(30)
    );
}

#[test]
fn test_buy_with_dust_stops_short() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 100);

    // 5_050 / 100 = 50 (residual dust = 50).
    let result = book
        .match_market_order_by_amount(new_id(), 5_050, Side::Buy)
        .expect("notional buy must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 1, "no qty=0 trade emitted for residual dust");
    assert_eq!(trades[0].quantity().as_u64(), 50);
    let spent = result.executed_value().expect("executed value");
    assert_eq!(spent, 5_000);
    assert_eq!(5_050u128 - spent, 50, "dust = requested - spent");
}

#[test]
fn test_buy_with_lot_size_rounds_down() {
    let book: OrderBook<()> = OrderBook::with_lot_size("TEST", 10);
    seed_asks(&book, &[100], 100);

    // budget allows 14 units (1_400 / 100 = 14); lot=10 ⇒ 14 - 4 = 10.
    let result = book
        .match_market_order_by_amount(new_id(), 1_400, Side::Buy)
        .expect("notional buy must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].quantity().as_u64(), 10);
    assert_eq!(result.executed_value().expect("executed value"), 1_000);
}

#[test]
fn test_buy_lot_size_fills_best_level_when_budget_allows_full_lots() {
    let book: OrderBook<()> = OrderBook::with_lot_size("TEST", 10);
    // Asks sort ascending, so with [100, 1_000] the best ask is 100.
    seed_asks(&book, &[100, 1_000], 100);

    // budget = 5_000:
    //   best level 100: 5_000 / 100 = 50 units
    //   lot = 10 ⇒ 50 is already a whole number of lots
    //   so the order fills entirely at the best level.
    let result = book
        .match_market_order_by_amount(new_id(), 5_000, Side::Buy)
        .expect("notional buy must succeed");
    assert_eq!(result.executed_value().expect("executed value"), 5_000);
}

#[test]
fn test_buy_empty_book_errors_with_notional_variant() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    let err = book
        .match_market_order_by_amount(new_id(), 1_000, Side::Buy)
        .expect_err("empty book must return error");
    match err {
        OrderBookError::InsufficientLiquidityNotional {
            side,
            requested,
            spent,
        } => {
            assert_eq!(side, Side::Buy);
            assert_eq!(requested, 1_000);
            assert_eq!(spent, 0);
        }
        other => panic!("expected InsufficientLiquidityNotional, got {other:?}"),
    }
}

#[test]
fn test_buy_budget_below_one_full_lot_errors() {
    let book: OrderBook<()> = OrderBook::with_lot_size("TEST", 10);
    seed_asks(&book, &[100], 100);

    // budget = 500 ⇒ 5 units; lot=10 ⇒ qty_cap = 0 ⇒ no fills.
    let err = book
        .match_market_order_by_amount(new_id(), 500, Side::Buy)
        .expect_err("must error: budget below one full lot");
    match err {
        OrderBookError::InsufficientLiquidityNotional { .. } => {}
        other => panic!("expected InsufficientLiquidityNotional, got {other:?}"),
    }
}

#[test]
fn test_sell_symmetric_walk() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    // Best bid first (descending walk): 102, 101, 100.
    seed_bids(&book, &[100, 101, 102], 10);

    // Sell sweeps highest bids first. budget 3_030 → 102*10 + 101*10 + 100*10 = 3_030
    let result = book
        .match_market_order_by_amount(new_id(), 3_030, Side::Sell)
        .expect("notional sell must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 3);
    // First trade is at the highest bid (102).
    assert_eq!(trades[0].price().as_u128(), 102);
    assert_eq!(result.executed_value().expect("executed value"), 3_030);
}

/// A sell walk steps over a bid it cannot afford and keeps descending,
/// on a plain book with self-trade prevention left off (#222). The walk
/// direction, not the STP mode, decides whether a zero per-level cap ends
/// the sweep, so this path is shared by every book.
///
/// Bids 100, 75 and 50, one unit each; sell a quote amount of 150.
/// Derivation, bids descending:
/// - 100: cap = 150 / 100 = 1, one unit executes, budget 150 - 100 = 50.
/// - 75: cap = 50 / 75 = 0. Nothing executes, but the bids still ahead are
///   cheaper, so the level is skipped rather than ending the sweep.
/// - 50: cap = 50 / 50 = 1, one unit executes, budget reaches 0.
///
/// Expected: 2 trades for 150 spent, at 100 and at 50, with the bid at 75
/// left resting untouched. Before the fix this stopped at 75 with a single
/// trade and 50 of the budget unspent.
#[test]
fn test_sell_skips_an_unaffordable_level_and_reaches_a_cheaper_bid() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    let skipped = new_id();
    for (id, price) in [(new_id(), 100u128), (skipped, 75), (new_id(), 50)] {
        book.add_limit_order(id, price, 1, Side::Buy, TimeInForce::Gtc, None)
            .expect("seed bid");
    }

    let result = book
        .match_market_order_by_amount(new_id(), 150, Side::Sell)
        .expect("notional sell must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 2, "one trade at 100 and one at 50");
    assert_eq!(trades[0].price().as_u128(), 100);
    assert_eq!(trades[0].quantity().as_u64(), 1);
    assert_eq!(trades[1].price().as_u128(), 50);
    assert_eq!(trades[1].quantity().as_u64(), 1);
    assert_eq!(result.executed_value().expect("executed value"), 150);
    assert_eq!(
        book.get_order(skipped)
            .expect("the unaffordable bid at 75 is skipped, not consumed")
            .visible_quantity()
            .as_u64(),
        1
    );
    assert_eq!(
        book.best_bid(),
        Some(75),
        "only the skipped level is left on the bid side"
    );
}

/// The lot-size bound on the same walk: once the remaining notional cannot
/// fund one whole lot at a price of 1 — the cheapest level that could
/// exist — no level still ahead can execute and the sweep ends (#222).
///
/// Lot 10, bids 100, 75 and 50 holding 10 units each; sell 1_100.
/// Derivation, bids descending:
/// - 100: cap = 1_100 / 100 = 11, rounded down to 10. Ten units execute
///   and the budget falls to 1_100 - 1_000 = 100.
/// - 75: cap = 100 / 75 = 1, rounded down to a lot of 10 gives 0. The
///   budget of 100 is still at or above one lot, so the walk continues.
/// - 50: cap = 100 / 50 = 2, rounded down to 0. Still at or above one lot,
///   so the walk continues and then runs out of levels.
///
/// Expected: a single trade of 10 at 100 for 1_000 spent, with both
/// cheaper bids left resting in full.
#[test]
fn test_sell_lot_rounding_leaves_cheaper_levels_untouched_when_no_lot_fits() {
    let book: OrderBook<()> = OrderBook::with_lot_size("TEST", 10);
    let at_75 = new_id();
    let at_50 = new_id();
    for (id, price) in [(new_id(), 100u128), (at_75, 75), (at_50, 50)] {
        book.add_limit_order(id, price, 10, Side::Buy, TimeInForce::Gtc, None)
            .expect("seed bid");
    }

    let result = book
        .match_market_order_by_amount(new_id(), 1_100, Side::Sell)
        .expect("notional sell must succeed");

    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 1, "no cheaper bid can fund a whole lot");
    assert_eq!(trades[0].price().as_u128(), 100);
    assert_eq!(trades[0].quantity().as_u64(), 10);
    assert_eq!(result.executed_value().expect("executed value"), 1_000);
    for id in [at_75, at_50] {
        assert_eq!(
            book.get_order(id)
                .expect("cheaper bids still rest")
                .visible_quantity()
                .as_u64(),
            10
        );
    }
}

#[test]
fn test_sell_empty_book_errors_with_notional_variant() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    let err = book
        .match_market_order_by_amount(new_id(), 1_000, Side::Sell)
        .expect_err("empty book must error");
    match err {
        OrderBookError::InsufficientLiquidityNotional { side, .. } => {
            assert_eq!(side, Side::Sell);
        }
        other => panic!("expected InsufficientLiquidityNotional, got {other:?}"),
    }
}

#[test]
fn test_quote_notional_carried_through_to_listener() {
    let mut book: OrderBook<()> = OrderBook::new("TEST");
    let captured_notional = Arc::new(AtomicU64::new(0));
    let captured_count = Arc::new(AtomicUsize::new(0));
    let n_clone = captured_notional.clone();
    let c_clone = captured_count.clone();
    let listener: TradeListener = Arc::new(move |tr: &TradeResult| {
        n_clone.store(tr.quote_notional as u64, Ordering::SeqCst);
        c_clone.fetch_add(1, Ordering::SeqCst);
    });
    book.set_trade_listener(listener);

    seed_asks(&book, &[100], 100);
    book.match_market_order_by_amount(new_id(), 5_000, Side::Buy)
        .expect("notional buy");

    assert_eq!(
        captured_count.load(Ordering::SeqCst),
        1,
        "listener invoked exactly once"
    );
    assert_eq!(
        captured_notional.load(Ordering::SeqCst),
        5_000,
        "listener saw quote_notional = sum(price * qty)"
    );
}

#[test]
fn test_fee_schedule_applies_to_notional_path() {
    let mut book: OrderBook<()> = OrderBook::new("TEST");
    book.set_fee_schedule(Some(FeeSchedule::new(0, 5))); // 5 bps taker
    let captured_taker_fee = Arc::new(AtomicU64::new(0));
    let f_clone = captured_taker_fee.clone();
    let listener: TradeListener = Arc::new(move |tr: &TradeResult| {
        f_clone.store(tr.total_taker_fees as u64, Ordering::SeqCst);
    });
    book.set_trade_listener(listener);

    seed_asks(&book, &[100], 1_000);
    book.match_market_order_by_amount(new_id(), 100_000, Side::Buy)
        .expect("notional buy with fees");

    // 5 bps on notional = 100_000 * 5 / 10_000 = 50.
    assert_eq!(captured_taker_fee.load(Ordering::SeqCst), 50);
}

#[test]
fn test_submit_market_order_by_amount_runs_kill_switch_gate() {
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 100);
    book.engage_kill_switch();

    let err = book
        .submit_market_order_by_amount(new_id(), 1_000, Side::Buy)
        .expect_err("kill-switch must reject");
    assert!(matches!(err, OrderBookError::KillSwitchActive));
}

#[test]
fn test_existing_base_qty_path_unaffected_by_refactor() {
    // Smoke test that the unified inner loop still drives base-qty
    // semantics correctly. Single-level fill, single trade, exact qty.
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 50);

    let result = book
        .submit_market_order(new_id(), 30, Side::Buy)
        .expect("base-qty market buy");
    let trades = result.trades().as_vec();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].quantity().as_u64(), 30);
    assert_eq!(trades[0].price().as_u128(), 100);
}

#[test]
fn test_quote_notional_populated_on_base_qty_path() {
    // The new `quote_notional` field is populated for both market-order
    // paths so consumers see it uniformly.
    let mut book: OrderBook<()> = OrderBook::new("TEST");
    let captured = Arc::new(AtomicU64::new(0));
    let c_clone = captured.clone();
    let listener: TradeListener = Arc::new(move |tr: &TradeResult| {
        c_clone.store(tr.quote_notional as u64, Ordering::SeqCst);
    });
    book.set_trade_listener(listener);

    seed_asks(&book, &[100], 50);
    book.submit_market_order(new_id(), 30, Side::Buy)
        .expect("base-qty market buy");

    assert_eq!(captured.load(Ordering::SeqCst), 100 * 30);
}

#[test]
fn test_buy_partial_fill_when_book_too_thin() {
    // budget covers more than the book — we should fill what's there
    // and return Ok (not an error) since at least one fill happened.
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 50);

    let result = book
        .match_market_order_by_amount(new_id(), 1_000_000, Side::Buy)
        .expect("partial fill must return Ok");
    assert_eq!(result.executed_value().expect("executed value"), 50 * 100);
    assert_eq!(
        result.executed_quantity().expect("executed qty"),
        Quantity::new(50)
    );
}

#[test]
fn test_buy_amount_zero_returns_no_fills_error() {
    // Zero notional cannot fund anything ⇒ InsufficientLiquidityNotional
    // (consistent with the existing base-qty `quantity = 0` semantics
    // being treated as a degenerate market order).
    let book: OrderBook<()> = OrderBook::new("TEST");
    seed_asks(&book, &[100], 50);
    let err = book
        .match_market_order_by_amount(new_id(), 0, Side::Buy)
        .expect_err("zero notional must error");
    assert!(matches!(
        err,
        OrderBookError::InsufficientLiquidityNotional { .. }
    ));
}
