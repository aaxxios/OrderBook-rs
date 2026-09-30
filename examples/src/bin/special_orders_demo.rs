//! Demonstration of special order types: PeggedOrder and TrailingStop
//!
//! # Order Types Demonstrated:
//!
//! ## PeggedOrder
//! Orders that track a reference price (best bid, best ask, mid price, or last trade)
//! with an optional offset. When the reference price changes, the order price
//! can be re-priced with `reprice_pegged_orders`.
//!
//! ## TrailingStop (#286)
//! Pending **off-book** stop orders driven by the book's prints:
//! - they are never liquidity (no level, no depth);
//! - the watermark (`last_reference_price`) follows the prints in the
//!   stop's favour and the stop price trails it by `trail_amount`
//!   (sell: below the highest print, buy: above the lowest);
//! - a print at or through the stop price (along each sweep's price path,
//!   not only its last print) elects the stop: it records
//!   `OrderStatus::Triggered` and executes as an unpriced
//!   immediate-or-cancel market child order (or a collared limit child,
//!   below), automatically, inside the call whose trade crossed it; its
//!   trades carry `origin_stop_id`; stops can cascade;
//! - a stop the last trade already crosses is rejected at admission
//!   (`StopWouldTrigger`).
//!
//! ## Stop protection collar (#302)
//! `OrderBook::set_stop_protection` bounds an elected stop: its child
//! becomes an immediate-or-cancel limit at `stop - collar` (sell) or
//! `stop + collar` (buy) and whatever does not fill within the band is
//! cancelled (not rested, unlike CME protection points), so a thin book is
//! not swept to its last level; a remainder the collar cut ends
//! `Cancelled { StopProtectionBand }`.
//!
//! # Usage:
//! ```bash
//! cargo run -p examples --features special_orders --bin special_orders_demo
//! ```

use orderbook_rs::StopProtection;
use orderbook_rs::orderbook::repricing::RepricingOperations;
use orderbook_rs::prelude::*;
use pricelevel::{Hash32, OrderType, PegReferenceType, Price, Quantity, TimestampMs, setup_logger};
use tracing::info;

type OrderBook = orderbook_rs::OrderBook<()>;

fn main() {
    let _ = setup_logger();

    info!("=== Special Orders Demo ===");
    info!("Demonstrating PeggedOrder and TrailingStop order types\n");

    demo_pegged_orders();
    demo_trailing_stop_orders();
    demo_stop_protection_collar();
    demo_combined_repricing();

    info!("\n=== Demo Complete ===");
}

fn demo_pegged_orders() {
    info!("\n--- Pegged Orders Demo ---");
    info!("Pegged orders track a reference price with an optional offset.\n");

    let book = OrderBook::new("BTC/USD");

    // First, establish market liquidity
    info!("Step 1: Establishing market liquidity...");

    // Add buy orders (bids)
    for i in 0u64..5 {
        let price: u128 = 50000 - (i as u128 * 100); // 50000, 49900, 49800, ...
        let id = Id::from_u64(i + 1);
        let _ = book.add_limit_order(id, price, 10, Side::Buy, TimeInForce::Gtc, None);
    }

    // Add sell orders (asks)
    for i in 0u64..5 {
        let price: u128 = 50100 + (i as u128 * 100); // 50100, 50200, 50300, ...
        let id = Id::from_u64(i + 100);
        let _ = book.add_limit_order(id, price, 10, Side::Sell, TimeInForce::Gtc, None);
    }

    info!(
        "  Best Bid: {} | Best Ask: {}",
        book.best_bid().unwrap_or(0),
        book.best_ask().unwrap_or(0)
    );
    info!("  Mid Price: {:.2}", book.mid_price().unwrap_or(0.0));

    // Add a pegged order that tracks best bid + 50
    info!("\nStep 2: Adding pegged order (tracks Best Bid + 50)...");

    let pegged_id = Id::from_u64(1000);
    let pegged_order = OrderType::PeggedOrder {
        id: pegged_id,
        price: Price::new(49000), // Initial price (will be re-priced)
        quantity: Quantity::new(5),
        side: Side::Buy,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        reference_price_offset: 50, // +50 from reference
        reference_price_type: PegReferenceType::BestBid,
        extra_fields: (),
    };

    book.add_order(pegged_order).unwrap();
    info!("  Pegged order added with initial price: 49000");
    info!("  Tracked pegged orders: {}", book.pegged_order_count());

    // Re-price the pegged order
    info!("\nStep 3: Re-pricing pegged order...");
    let repriced = book.reprice_pegged_orders().unwrap();
    info!("  Orders re-priced: {}", repriced);

    if let Some(order) = book.get_order(pegged_id) {
        info!(
            "  New price: {} (Best Bid {} + offset 50)",
            order.price(),
            book.best_bid().unwrap_or(0)
        );
    }

    // Demonstrate different reference types
    info!("\n--- Pegged Order Reference Types ---");
    info!("  BestBid: Tracks the highest buy price");
    info!("  BestAsk: Tracks the lowest sell price");
    info!("  MidPrice: Tracks the midpoint between best bid and ask");
    info!("  LastTrade: Tracks the last executed trade price");
}

fn demo_trailing_stop_orders() {
    info!("\n--- Trailing Stop Orders Demo ---");
    info!("Trailing stops are held off book and follow the last trade price.\n");

    let mut book = OrderBook::new("ETH/USD");
    book.set_order_state_tracker(OrderStateTracker::new());
    book.set_trade_listener(std::sync::Arc::new(|trade: &TradeResult| {
        let fills: Vec<String> = trade
            .match_result
            .trades()
            .as_vec()
            .iter()
            .map(|t| format!("{} @ {}", t.quantity().as_u64(), t.price().as_u128()))
            .collect();
        match trade.origin_stop_id {
            Some(stop) => info!(
                "  [trade] taker {} (elected stop {}) filled {}",
                trade.match_result.order_id(),
                stop,
                fills.join(", ")
            ),
            None => info!(
                "  [trade] taker {} filled {}",
                trade.match_result.order_id(),
                fills.join(", ")
            ),
        }
    }));

    // Establish an uncrossed market: bids 3000..2960, asks 3010..3050.
    info!("Step 1: Establishing market (best bid 3000, best ask 3010)...");
    for i in 0u64..5 {
        let bid_price: u128 = 3000 - (i as u128 * 10);
        let ask_price: u128 = 3010 + (i as u128 * 10);
        let _ = book.add_limit_order(
            Id::from_u64(i + 1),
            bid_price,
            100,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        );
        let _ = book.add_limit_order(
            Id::from_u64(i + 100),
            ask_price,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
    }
    // A first trade at 3010 sets the last trade price.
    let _ = book.submit_market_order(Id::from_u64(500), 1, Side::Buy);
    info!(
        "  Best Bid: {} | Best Ask: {} | Last trade: {}",
        book.best_bid().unwrap_or(0),
        book.best_ask().unwrap_or(0),
        book.last_trade_price().unwrap_or(0)
    );

    // A protective sell stop 50 below a watermark of 3010: stop price 2960.
    info!("\nStep 2: Adding SELL trailing stop (trail amount: 50)...");
    let stop_id = Id::from_u64(2000);
    let trailing_sell = OrderType::TrailingStop {
        id: stop_id,
        price: Price::new(2960),
        quantity: Quantity::new(150),
        side: Side::Sell,
        user_id: Hash32::new([7u8; 32]),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        trail_amount: Quantity::new(50),
        last_reference_price: Price::new(3010),
        extra_fields: (),
    };
    book.add_order(trailing_sell)
        .expect("pending trailing stop admitted");
    info!("  Pending trailing stops: {:?}", book.trailing_stop_ids());
    info!(
        "  It is not liquidity: best ask still {}, depth at 2960 on the ask side: {:?}",
        book.best_ask().unwrap_or(0),
        book.visible_quantity_at_price(2960, Side::Sell)
    );

    // The market rises: buyers lift the asks up to 3040.
    info!("\nStep 3: Market rises; buyers lift the offers up to 3040...");
    let _ = book.submit_market_order(Id::from_u64(501), 18, Side::Buy);
    show_stop(&book, stop_id);

    // The market falls: sellers hit the bids down to 2990.
    info!("\nStep 4: Market falls; sellers take the whole bid at 3000...");
    let _ = book.submit_market_order(Id::from_u64(502), 100, Side::Sell);
    show_stop(&book, stop_id);
    info!("  The stop did not move down with the market (a sell stop only tightens).");

    // The next print at or below the stop price elects it.
    info!("\nStep 5: A trade at 2990 reaches the stop price 2990...");
    let _ = book.submit_market_order(Id::from_u64(503), 60, Side::Sell);
    info!(
        "  Stop {} elected and executed as market order {}",
        stop_id,
        book.stop_trigger_order_id(stop_id)
    );
    info!(
        "  Stop status: {:?} | pending stops left: {}",
        book.order_status(stop_id),
        book.trailing_stop_count()
    );
    if let Some(history) = book
        .order_state_tracker()
        .and_then(|tracker| tracker.get_history(stop_id))
    {
        for (_, status) in history {
            info!("  Stop history: {status}");
        }
    }
    info!(
        "  Best Bid after the stop's sale: {}",
        book.best_bid().unwrap_or(0)
    );

    // A stop the last trade already crosses is rejected untouched.
    let Some(last) = book.last_trade_price() else {
        return;
    };
    let (Some(stop), Some(watermark)) = (last.checked_sub(50), last.checked_sub(100)) else {
        return;
    };
    info!("\nStep 6: A BUY stop at {stop} with the last trade at {last}...");
    let crossed = OrderType::TrailingStop {
        id: Id::from_u64(2001),
        price: Price::new(stop),
        quantity: Quantity::new(10),
        side: Side::Buy,
        user_id: Hash32::new([7u8; 32]),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        trail_amount: Quantity::new(50),
        last_reference_price: Price::new(watermark),
        extra_fields: (),
    };
    match book.add_order(crossed) {
        Ok(_) => info!("  Unexpectedly admitted"),
        Err(err) => info!("  Rejected: {err}"),
    }
}

fn demo_stop_protection_collar() {
    info!("\n--- Stop Protection Collar Demo (#302) ---");
    info!(
        "The same sell stop (3000, quantity 8) elected in a thin book, without and with a collar.\n"
    );

    for collar in [None, Some(20u128)] {
        let mut book = OrderBook::new("SOL/USD");
        book.set_order_state_tracker(OrderStateTracker::new());
        let protection = collar.and_then(|units| StopProtection::try_new(units).ok());
        if let Err(err) = book.set_stop_protection(protection) {
            info!("  Collar refused: {err}");
            return;
        }
        // A thin bid ladder: 1 @ 3000, 1 @ 2990, 1 @ 2980, 5 @ 2950, 10 @ 2900.
        for (raw, price, qty) in [
            (1u64, 3000u128, 1u64),
            (2, 2990, 1),
            (3, 2980, 1),
            (4, 2950, 5),
            (5, 2900, 10),
        ] {
            let _ = book.add_limit_order(
                Id::from_u64(raw),
                price,
                qty,
                Side::Buy,
                TimeInForce::Gtc,
                None,
            );
        }
        let stop_id = Id::from_u64(3000);
        let stop = OrderType::TrailingStop {
            id: stop_id,
            price: Price::new(3000),
            quantity: Quantity::new(8),
            side: Side::Sell,
            user_id: Hash32::new([9u8; 32]),
            timestamp: TimestampMs::new(current_time_millis()),
            time_in_force: TimeInForce::Gtc,
            trail_amount: Quantity::new(50),
            last_reference_price: Price::new(3050),
            extra_fields: (),
        };
        book.add_order(stop)
            .expect("pending trailing stop admitted");
        // One unit sold at 3000 elects the stop.
        let _ = book.submit_market_order(Id::from_u64(10), 1, Side::Sell);
        match protection {
            None => info!("Without a collar (market child):"),
            Some(protection) => info!(
                "With a collar of {} (IOC limit at {}):",
                protection.collar(),
                protection.limit_price(Side::Sell, Price::new(3000))
            ),
        }
        // `Triggered` carries the child's limit (none for a market child);
        // a remainder the collar cut ends `StopProtectionBand`.
        if let Some(history) = book
            .order_state_tracker()
            .and_then(|tracker| tracker.get_history(stop_id))
        {
            for (_, status) in history {
                info!("  Stop history: {status}");
            }
        }
        info!(
            "  Last trade {} | best bid left {}",
            book.last_trade_price().unwrap_or(0),
            book.best_bid().unwrap_or(0)
        );
    }
}

fn show_stop(book: &OrderBook, stop_id: Id) {
    match book.get_order(stop_id).as_deref() {
        Some(OrderType::TrailingStop {
            price,
            last_reference_price,
            ..
        }) => info!(
            "  Last trade {} | watermark {} | stop price {}",
            book.last_trade_price().unwrap_or(0),
            last_reference_price.as_u128(),
            price.as_u128()
        ),
        _ => info!("  Stop {stop_id} is no longer pending"),
    }
}

fn demo_combined_repricing() {
    info!("\n--- Combined Re-pricing Demo ---");
    info!("Re-pricing all special orders at once.\n");

    let book = OrderBook::new("SOL/USD");

    // Establish market
    for i in 0u64..3 {
        let bid_price: u128 = 100 - (i as u128 * 5);
        let ask_price: u128 = 105 + (i as u128 * 5);
        let _ = book.add_limit_order(
            Id::from_u64(i + 1),
            bid_price,
            100,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        );
        let _ = book.add_limit_order(
            Id::from_u64(i + 100),
            ask_price,
            100,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        );
    }

    info!(
        "Market: Best Bid {} | Best Ask {}",
        book.best_bid().unwrap_or(0),
        book.best_ask().unwrap_or(0)
    );

    // Add multiple special orders
    let pegged1 = OrderType::PeggedOrder {
        id: Id::from_u64(1000),
        price: Price::new(90),
        quantity: Quantity::new(10),
        side: Side::Buy,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        reference_price_offset: 2,
        reference_price_type: PegReferenceType::BestBid,
        extra_fields: (),
    };

    let pegged2 = OrderType::PeggedOrder {
        id: Id::from_u64(1001),
        price: Price::new(110),
        quantity: Quantity::new(10),
        side: Side::Sell,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        reference_price_offset: -2,
        reference_price_type: PegReferenceType::BestAsk,
        extra_fields: (),
    };

    // A pending sell stop 5 below a watermark of 100: stop price 95. It is
    // off book, so it never interacts with the pegged orders' re-pricing.
    let trailing = OrderType::TrailingStop {
        id: Id::from_u64(2000),
        price: Price::new(95),
        quantity: Quantity::new(10),
        side: Side::Sell,
        user_id: Hash32::zero(),
        timestamp: TimestampMs::new(current_time_millis()),
        time_in_force: TimeInForce::Gtc,
        trail_amount: Quantity::new(5),
        last_reference_price: Price::new(100),
        extra_fields: (),
    };

    book.add_order(pegged1).unwrap();
    book.add_order(pegged2).unwrap();
    book.add_order(trailing).unwrap();

    info!("\nAdded special orders:");
    info!("  Pegged orders: {}", book.pegged_order_count());
    info!("  Trailing stops: {}", book.trailing_stop_count());

    // Re-price all at once
    info!("\nRe-pricing all special orders...");
    let result = book.reprice_special_orders().unwrap();

    info!("Results:");
    info!(
        "  Pegged orders re-priced: {}",
        result.pegged_orders_repriced
    );
    info!(
        "  Trailing stops trailed by this call: {} (stops trail automatically on every trade)",
        result.trailing_stops_repriced
    );

    // Show final prices
    info!("\nFinal order prices:");
    for id in book.pegged_order_ids() {
        if let Some(order) = book.get_order(id) {
            info!("  Pegged {}: price = {}", id, order.price());
        }
    }
    for id in book.trailing_stop_ids() {
        if let Some(order) = book.get_order(id) {
            info!("  Trailing {}: pending stop price = {}", id, order.price());
        }
    }

    // Best practices
    info!("\n--- Best Practices ---");
    info!("1. Call reprice_pegged_orders() after significant market changes");
    info!("2. Use price_level_changed_listener to trigger re-pricing automatically");
    info!("3. Trailing stops need no calls: they trail and trigger on every trade");
    info!("4. Correlate a stop with its fills through stop_trigger_order_id()");
}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
