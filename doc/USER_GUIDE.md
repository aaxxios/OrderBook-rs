# OrderBook-rs User Guide

Complete guide for using the OrderBook-rs library in your trading systems.

## Table of Contents

1. [Introduction](#introduction)
2. [Installation](#installation)
3. [Quick Start](#quick-start)
4. [Core Concepts](#core-concepts)
5. [Basic Operations](#basic-operations)
6. [Advanced Features](#advanced-features)
7. [Performance Optimization](#performance-optimization)
8. [Best Practices](#best-practices)
9. [Examples](#examples)
10. [Troubleshooting](#troubleshooting)

---

## Introduction

OrderBook-rs is a high-performance, lock-free order book implementation for financial trading systems. It provides:

- **Lock-free architecture** using crossbeam-skiplist for concurrent access
- **Multiple order types**: Limit, Market, Iceberg, FOK, IOC
- **Real-time metrics**: VWAP, spread, imbalance, depth statistics
- **Market impact simulation** for pre-trade analysis
- **Intelligent order placement** strategies
- **Enriched snapshots** with pre-calculated metrics
- **Trade notifications** with listener pattern

**Performance characteristics:**
- Single-threaded: ~1M orders/second
- 30-thread HFT simulation: ~600K orders/second
- Low latency: <1µs for order operations
- Lock-free data structures on the matching path: skiplist price levels,
  concurrent maps and atomics; no lock is taken per price level

Mutating entry points additionally pass a book-level gate that serializes
the two decisions that must not interleave with another mutation:
fill-or-kill feasibility and self-trade prevention. See
[Concurrent Access](#4-concurrent-access) for what is serialized and what
stays concurrent.

---

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
orderbook-rs = "0.4"
pricelevel = "0.4"
```

For simplified imports, use the prelude:

```rust
use orderbook_rs::prelude::*;
```

---

## Quick Start

### Creating an OrderBook

```rust
use orderbook_rs::prelude::*;

// Create order book
let book = OrderBook::<()>::new("BTC/USD");

// Add buy order
let order_id = OrderId::new();
let result = book.add_limit_order(
    order_id,
    50000,  // price
    10,     // quantity
    Side::Buy,
    TimeInForce::Gtc,
    None    // no extra data
);

// Add sell order
let order_id2 = OrderId::new();
book.add_limit_order(
    order_id2,
    50100,  // price
    10,     // quantity
    Side::Sell,
    TimeInForce::Gtc,
    None
)?;

// Get best bid/ask
if let Some(best_bid) = book.best_bid() {
    println!("Best bid: {}", best_bid);
}
if let Some(best_ask) = book.best_ask() {
    println!("Best ask: {}", best_ask);
}
```

### Executing Market Orders

```rust
// Execute market buy order
let order_id = OrderId::new();
let result = book.add_market_order(
    order_id,
    20,  // quantity
    Side::Buy,
    None
)?;

// Check execution
println!("Filled: {} units", result.filled_quantity);
println!("Average price: {}", result.average_price());
```

---

## Core Concepts

### Order Types

**Limit Orders:**
- Placed at specific price level
- Only execute at or better than limit price
- Can be partially filled

**Market Orders:**
- Execute immediately at best available price
- Consume liquidity from order book
- May experience slippage

**Iceberg and Reserve Orders (two tranches):**
- Hide large orders by showing only the visible tranche
- Replenish the visible tranche as it is filled (Reserve orders carry an
  explicit replenishment policy)
- Reduce market impact
- The tranches are independent and the order's total is `visible + hidden`.
  The quantity supplied to these modification variants addresses the
  **visible** tranche and leaves the hidden tranche untouched:
  `OrderUpdate::UpdateQuantity`, `OrderUpdate::UpdatePriceAndQuantity` and
  `OrderUpdate::Replace`. One exception: `UpdateQuantity` with a **zero**
  quantity is a removal and cancels the whole order, hidden depth
  included. `add_iceberg_order` takes the visible and hidden tranches as
  separate arguments
- On a book with a lot size the two tranches are validated individually,
  not on the total: a 15 visible / 5 hidden split is rejected on a lot-10
  book even though its total of 20 is a whole multiple. A Reserve order is
  additionally validated on the quantity its replenishment would move into
  the visible tranche, capped by the hidden tranche. That check applies only
  while `auto_replenish` is on, the single flag that decides whether anything
  is ever transferred: `min(replenish_amount, hidden)` when an explicit
  amount is set; `min(DEFAULT_RESERVE_REPLENISH_AMOUNT, hidden)` when there
  is none. With `auto_replenish` off nothing is ever transferred and no
  check applies, whatever `replenish_amount` says; nothing is checked either
  when the order carries no hidden tranche. `replenish_threshold` is
  unrestricted. Rejections return `OrderBookError::InvalidLotSize` naming the
  offending quantity
- A Reserve order with `auto_replenish` off must display a positive visible
  tranche: a `visible_quantity` of 0 behind a non-empty `hidden_quantity` is
  rejected with `OrderBookError::ZeroVisibleTranche`, on `add_order`, on
  every quantity-carrying modify (`UpdateQuantity`,
  `UpdatePriceAndQuantity`, `Replace`, all of which set the visible tranche)
  and on snapshot restore, where a snapshot holding the shape must be
  repaired before it can be restored. That shape shows no depth, never fills, and loses
  its whole hidden tranche to the first taker that reaches its level. The
  other zero-visible two-tranche shapes execute instead of vanishing and
  stay admissible: an Iceberg draws its entire hidden tranche into visible
  on match, and an auto-replenishing Reserve refreshes and re-queues
- An aggressive two-tranche order sweeps with its **total**, not with its
  visible tranche: a 10 visible / 20 hidden Reserve submitted into 20 units
  of contra liquidity executes 20. What its unmatched residual does follows
  `auto_replenish`. With it on and hidden left, a visible tranche the sweep
  left below `max(replenish_threshold, 1)` — an emptied one always is — is
  refreshed from hidden (the explicit `replenish_amount`, or
  `DEFAULT_RESERVE_REPLENISH_AMOUNT` when there is none, capped by hidden)
  and the residual rests. With it off the residual is discarded **only when
  the fill exhausted the visible tranche**: the order then ends as
  `OrderStatus::Filled { filled_quantity }` carrying only what executed,
  mirroring the resting side, where `pricelevel` removes a depleted
  non-replenishing maker and strands its hidden tranche. A shallower fill
  rests normally — that same 10 visible / 20 hidden Reserve filled for 5
  rests 5 / 20 with nothing discarded. The accounting rule
  holds in every case, and discarded quantity is never counted as executed:
  `submitted = executed + resting (visible + hidden) + discarded`
- An explicit `replenish_amount` is the **transfer**, not a target display
  size: it is added to whatever visible quantity survived the fill. With
  `replenish_amount = 10`, `replenish_threshold = 5` and a remainder of 2
  visible, the residual rests 12 visible. Without an explicit amount the
  transfer is `DEFAULT_RESERVE_REPLENISH_AMOUNT` capped by the hidden
  tranche, so that 10 visible / 20 hidden Reserve filled for 10 refreshes
  with `min(DEFAULT_RESERVE_REPLENISH_AMOUNT, 20) = 20` and rests 20 visible
  / 0 hidden — more than it first displayed

**Time-In-Force:**
- `Gtc` (Good-Till-Cancel): Remain until filled or cancelled
- `Ioc` (Immediate-Or-Cancel): Fill immediately or cancel
- `Fok` (Fill-Or-Kill): Fill completely or cancel entirely

### Sides

- `Side::Buy`: Bid side (buyers)
- `Side::Sell`: Ask side (sellers)

### Price Levels

Prices are represented as `u64` in base units (e.g., cents, satoshis).

Example: $500.00 = 50000 (in cents)

---

## Basic Operations

### Adding Orders

```rust
// Limit order
let order_id = OrderId::new();
book.add_limit_order(
    order_id,
    50000,           // price
    100,             // quantity
    Side::Buy,
    TimeInForce::Gtc,
    None
)?;

// Iceberg order (visible tranche: 10, hidden tranche: 90, total: 100)
let order_id = OrderId::new();
book.add_iceberg_order(
    order_id,
    50000,           // price
    10,              // visible quantity
    90,              // hidden quantity
    Side::Buy,
    TimeInForce::Gtc,
    None
)?;

// Market order
let order_id = OrderId::new();
book.add_market_order(
    order_id,
    50,              // quantity
    Side::Buy,
    None
)?;
```

### Modifying Orders

Modifications go through `OrderBook::update_order` with an `OrderUpdate`
variant:

```rust
use pricelevel::{OrderUpdate, Price, Quantity};

// Resize in place
book.update_order(OrderUpdate::UpdateQuantity {
    order_id,
    new_quantity: Quantity::new(80),
})?;

// Re-price only
book.update_order(OrderUpdate::UpdatePrice {
    order_id,
    new_price: Price::new(50100),
})?;

// Re-price and resize in one call
book.update_order(OrderUpdate::UpdatePriceAndQuantity {
    order_id,
    new_price: Price::new(50100),
    new_quantity: Quantity::new(80),
})?;
```

**Queue priority:** `UpdateQuantity` keeps the order's queue position when the
new size is unchanged or smaller, and moves it to the back of its price level
when the size grows. `UpdatePrice`, `UpdatePriceAndQuantity` and `Replace` are
cancel-then-add: the order always re-enters at the back of its (possibly new)
price level. `Replace` and `UpdatePriceAndQuantity` do so even when the price
is unchanged; `UpdatePrice` to the current price is rejected instead.

**Two-tranche orders (iceberg / reserve):** the quantity carried by
`UpdateQuantity`, `UpdatePriceAndQuantity` and `Replace` sets the **visible**
tranche and leaves the hidden tranche untouched, so the resulting total is
`new_quantity + hidden`; an increase is applied, not clamped. Reducing the
hidden tranche is not reachable through an update: cancel and re-submit
instead. Shape validation (tick size, lot size, `visible + hidden`
representability) and the risk gate run on the projected order, and the
min / max order size limits apply to its `visible + hidden` total, so an
update can be rejected for a size larger than the quantity you passed.
A cancel-then-add modify (`UpdatePrice`, `UpdatePriceAndQuantity`,
`Replace`) of a Reserve order with `auto_replenish` off and a non-empty
hidden tranche is additionally rejected with
`OrderBookError::ReserveResidualWouldBeDiscarded` when the projected price
would cross into at least its visible tranche but less than its total,
because the re-added order's residual would not rest and its hidden
remainder would be destroyed; a projected full fill is allowed, and so is a
re-price that crosses less than the visible tranche. These pre-admission
rejections leave the original order unchanged.

**Zero `UpdateQuantity`:** a zero quantity on `UpdateQuantity` is a removal,
not a resize. It cancels the entire order (`Cancelled { UserRequested }`),
hidden depth included, and runs no projected validation, so a configured
`min_order_size` does not reject it; only the kill switch still refuses it,
as it refuses every modify. This applies to `UpdateQuantity` alone: a zero
quantity on `Replace` or `UpdatePriceAndQuantity` re-adds the order through
validate-first, so an iceberg or auto-replenishing reserve rests with a zero
visible tranche and its hidden depth live, while a reserve with
`auto_replenish` off is rejected with `ZeroVisibleTranche` and keeps resting.

### Cancelling Orders

```rust
// Cancel specific order
book.cancel_order(order_id)?;

// Cancel all orders on one side
book.cancel_all_orders_for_side(Side::Buy)?;
```

### Querying Order Book State

```rust
// Best prices
let best_bid = book.best_bid();
let best_ask = book.best_ask();

// Spread
let spread = book.spread_absolute();
let spread_bps = book.spread_bps();

// Depth
let bid_depth = book.total_depth_at_levels(Side::Buy, 5);
let ask_depth = book.total_depth_at_levels(Side::Sell, 5);

// Check if order exists
let exists = book.has_order(&order_id);
```

---

## Advanced Features

### 1. Market Metrics

Calculate key trading metrics for decision making.

```rust
// VWAP (Volume-Weighted Average Price)
let vwap = book.vwap(Side::Buy, 10);  // Top 10 levels

// Mid price
let mid = book.mid_price();

// Spread in basis points
let spread_bps = book.spread_bps();

// Order book imbalance (-1.0 to 1.0)
let imbalance = book.order_book_imbalance(5);  // Top 5 levels

// Micro price (imbalance-adjusted)
let micro_price = book.micro_price();
```

**Use cases:**
- Trading signal generation
- Fair value calculation
- Market condition detection
- Risk assessment

### 2. Market Impact Simulation

Simulate order execution to assess pre-trade impact.

```rust
// Simulate market order
let simulation = book.simulate_market_order(Side::Buy, 1000);

println!("Average price: {}", simulation.average_price);
println!("Total cost: {}", simulation.total_cost);
println!("Price impact: {:.2}%", simulation.price_impact_percentage);
println!("Levels consumed: {}", simulation.levels_consumed);

// Decide based on impact
if simulation.price_impact_percentage < 0.5 {
    // Execute order
    book.add_market_order(order_id, 1000, Side::Buy, None)?;
} else {
    // Impact too high, use limit order instead
    book.add_limit_order(
        order_id,
        simulation.average_price as u64,
        1000,
        Side::Buy,
        TimeInForce::Gtc,
        None
    )?;
}
```

**Use cases:**
- Pre-trade risk assessment
- Order type selection
- Position sizing
- Execution strategy

### 3. Intelligent Order Placement

Optimize order placement for market makers and smart routing.

```rust
// Get queue position at specific price
let queue_ahead = book.queue_ahead_at_price(50000, Side::Buy);
println!("Orders ahead: {}", queue_ahead);

// Calculate price N ticks inside
let price = book.price_n_ticks_inside(Side::Buy, 3);  // 3 ticks inside best bid

// Find price for queue position
let target_price = book.price_for_queue_position(Side::Buy, 100);

// Get depth-adjusted price
let adjusted_price = book.price_at_depth_adjusted(Side::Buy, 1000, 0.95);
```

**Use cases:**
- Market maker order placement
- Smart order routing
- Execution optimization
- Liquidity provision

### 4. Functional Iterators

Efficient, lazy evaluation for depth analysis.

```rust
// Iterate until cumulative depth reached
let levels: Vec<_> = book
    .levels_until_depth(Side::Buy, 1000)
    .collect();

// Iterate with cumulative depth tracking
for level in book.levels_with_cumulative_depth(Side::Sell, 10) {
    println!("Price: {}, Size: {}, Cumulative: {}", 
             level.price, level.size, level.cumulative);
}

// Iterate within price range
let levels: Vec<_> = book
    .levels_in_range(Side::Buy, 49000, 50000)
    .collect();

// Combine with functional operations
let total_volume: u64 = book
    .levels_until_depth(Side::Buy, 5000)
    .map(|level| level.size)
    .sum();
```

**Benefits:**
- Zero-allocation iteration
- Lazy evaluation (compute only what's needed)
- Composable operations
- Short-circuit optimization

### 5. Aggregate Statistics

Comprehensive statistical analysis for market condition detection.

```rust
// Depth statistics
let stats = book.depth_statistics(Side::Buy, 10);

println!("Total volume: {}", stats.total_volume);
println!("Average level size: {:.2}", stats.avg_level_size);
println!("Weighted avg price: {:.2}", stats.weighted_avg_price);
println!("Std dev: {:.2}", stats.std_dev_level_size);
println!("Min/Max: {} / {}", stats.min_level_size, stats.max_level_size);

// Market pressure
let (buy_pressure, sell_pressure) = book.buy_sell_pressure();
println!("Buy pressure: {}, Sell pressure: {}", buy_pressure, sell_pressure);

// Thin book detection
let is_thin = book.is_thin_book(1000, 5);  // Threshold: 1000, levels: 5
if is_thin {
    println!("⚠️ Low liquidity detected!");
}

// Depth distribution
let distribution = book.depth_distribution(Side::Buy, 5);
for bin in distribution {
    println!("Price range: {} - {}, Volume: {}, Levels: {}", 
             bin.min_price, bin.max_price, bin.volume, bin.level_count);
}
```

**Use cases:**
- Market condition detection
- Risk management
- Strategy adaptation
- Trading decision support

### 6. Enriched Snapshots

Pre-calculated metrics in snapshots for high-frequency trading.

```rust
// Snapshot with all metrics
let snapshot = book.enriched_snapshot(10);

println!("Mid price: {:?}", snapshot.mid_price);
println!("Spread: {:?} bps", snapshot.spread_bps);
println!("Bid depth: {}", snapshot.bid_depth_total);
println!("Ask depth: {}", snapshot.ask_depth_total);
println!("Imbalance: {}", snapshot.order_book_imbalance);
println!("VWAP bid: {:?}", snapshot.vwap_bid);
println!("VWAP ask: {:?}", snapshot.vwap_ask);

// Custom metrics for performance
use orderbook_rs::MetricFlags;

let snapshot = book.enriched_snapshot_with_metrics(
    10,
    MetricFlags::MID_PRICE | MetricFlags::SPREAD
);

// Serialize for distribution
let json = serde_json::to_string(&snapshot)?;
```

**Benefits:**
- Single pass through data (vs 5+ passes)
- Better cache locality
- Lower latency
- Consistent timestamp for all metrics
- Optional metric selection

---

## Performance Optimization

### 1. Choose the Right Data Types

```rust
// Use u64 for prices and quantities (base units)
let price: u64 = 50000;  // $500.00 in cents
let quantity: u64 = 100;

// Use f64 only for calculated metrics
let vwap: f64 = book.vwap(Side::Buy, 10).unwrap_or(0.0);
```

### 2. Minimize Allocations

```rust
// Use iterators instead of collecting
let sum: u64 = book
    .levels_until_depth(Side::Buy, 1000)
    .map(|level| level.size)
    .sum();  // No allocation

// Instead of:
let levels: Vec<_> = book.levels_until_depth(Side::Buy, 1000).collect();
let sum: u64 = levels.iter().map(|level| level.size).sum();  // Allocates Vec
```

### 3. Use Enriched Snapshots for Multiple Metrics

```rust
// ❌ Inefficient: Multiple passes
let mid = book.mid_price();
let spread = book.spread_bps();
let depth = book.total_depth_at_levels(Side::Buy, 10);
let vwap = book.vwap(Side::Buy, 10);

// ✅ Efficient: Single pass
let snapshot = book.enriched_snapshot(10);
let mid = snapshot.mid_price;
let spread = snapshot.spread_bps;
let depth = snapshot.bid_depth_total;
let vwap = snapshot.vwap_bid;
```

### 4. Batch Operations

```rust
// Add multiple orders efficiently
let orders = vec![
    (OrderId::new(), 50000, 10),
    (OrderId::new(), 49990, 20),
    (OrderId::new(), 49980, 30),
];

for (id, price, qty) in orders {
    book.add_limit_order(id, price, qty, Side::Buy, TimeInForce::Gtc, None)?;
}
```

### 5. Use Appropriate Depth Limits

```rust
// Only analyze what you need
let stats = book.depth_statistics(Side::Buy, 5);  // Top 5 levels only

// Instead of:
let stats = book.depth_statistics(Side::Buy, 0);  // All levels (slower)
```

---

## Best Practices

### 1. Error Handling

```rust
use orderbook_rs::OrderBookError;

match book.add_limit_order(order_id, price, qty, Side::Buy, TimeInForce::Gtc, None) {
    Ok(result) => {
        println!("Order added successfully");
    }
    Err(OrderBookError::DuplicateOrderId { order_id }) => {
        eprintln!("Order {} already exists", order_id);
    }
    Err(e) => {
        eprintln!("Error: {}", e);
    }
}
```

### 2. Trade Notifications

A listener is installed on the book, not passed per call:

```rust
use orderbook_rs::prelude::*;
use std::sync::{Arc, mpsc};

let (tx, rx) = mpsc::channel();

let mut book = OrderBook::<()>::new("BTC/USD");
book.set_trade_listener(Arc::new(move |trade: &TradeResult| {
    // Do not touch the book from here; hand the event off and return.
    let _ = tx.send((trade.engine_seq, trade.match_result.trades().len()));
}));

let book = Arc::new(book);
book.submit_market_order(OrderId::new(), 100, Side::Buy)?;

while let Ok((seq, fills)) = rx.try_recv() {
    println!("engine_seq {seq}: {fills} fill(s)");
}
```

**Delivery and re-entrancy (0.14.0, #249).** `TradeListener`,
`PriceLevelChangedListener` and `OrderStateListener` run **after** the book
operation that produced the event has committed and released the
book-level gate described in [Concurrent Access](#4-concurrent-access),
never mid-mutation. Per book, events are delivered in one total order
consistent with commit order (`engine_seq` strictly increases across the
trade and price-level streams, also with concurrent submitters), by
whichever thread is dispatching: usually the submitter before its call
returns, but under concurrency possibly another thread after the submit
returned. A listener may call back into the same `OrderBook` (the nested
call's events are delivered after the current batch). Listeners must not
panic and must return quickly: a slow listener delays every later event
of the book, so pushing onto a channel is still the recommended shape.

### 3. State Management

```rust
// Create snapshot for persistence
let snapshot = book.create_snapshot(10);
let json = serde_json::to_string(&snapshot)?;

// Save to file/database
std::fs::write("orderbook_snapshot.json", json)?;

// Restore later
let json = std::fs::read_to_string("orderbook_snapshot.json")?;
let snapshot: OrderBookSnapshot = serde_json::from_str(&json)?;
book.restore_from_snapshot(snapshot)?;
```

### 4. Concurrent Access

```rust
use std::sync::Arc;

// Share order book across threads
let book = Arc::new(OrderBook::<()>::new("BTC/USD"));

// Clone Arc for each thread
let book_clone = Arc::clone(&book);
std::thread::spawn(move || {
    // Use book_clone in thread
    let _ = book_clone.add_limit_order(
        OrderId::new(),
        50000,
        10,
        Side::Buy,
        TimeInForce::Gtc,
        None
    );
});
```

**What the book serializes.** The price-level map, the order index and the
statistics counters are lock-free structures: skiplists, concurrent maps
and atomics, with no per-level lock. On top of those, the mutating entry
points pass a single book-level gate, which is taken in one of two modes:

| Operation | Gate mode |
|---|---|
| Ordinary submit, cancel, `UpdateQuantity` | shared |
| Mass cancel (all, by side, by user, by price range) and `evict_expired_orders` | exclusive |
| Fill-or-kill submit | exclusive |
| Identified submit (any kind except post-only) or market sweep with STP enabled | exclusive |
| Post-only submit, any STP mode (never runs the STP scan) | shared |
| `UpdatePrice` / `UpdatePriceAndQuantity` / `Replace` with STP enabled | exclusive |

The exclusive modes exist because both decisions are made in one step and
applied in a second one: a fill-or-kill checks multi-level feasibility and
then sweeps, and a self-trade-prevention submit scans a price level's queue
and then fills it. Holding the gate exclusively across both steps is what
stops another thread from admitting, cancelling or re-pricing an order in
between and having the decision applied to a book state it was never taken
on.

Everything else stays on the shared side and runs concurrently. A book left
on the default `STPMode::None` takes the exclusive side for fill-or-kill
and, from #230, while it **holds** a Reserve order with `auto_replenish` off
that carries hidden quantity: every **sweep** on such a book is exclusive —
every matching-capable submit, every cancel-then-add re-price and every
match-only entry point (`match_order`, `match_market_order*`) — as is the
admission of the first such reserve. A sweep decides once whether to capture
makers whose hidden depth it would strand, so nothing may cancel, admit or
replace an order inside that sweep's capture window; otherwise the sweep
could consume a maker it never captured, or report a captured maker's hidden
quantity after a cancel freed its id for an unrelated order. Post-only
submits, `UpdateQuantity` and single cancels keep the shared side and
never consult the count: they are excluded by the sweep's hold, not by
taking the exclusive side themselves. Those books serialize their sweeps, as
STP books do; books holding no such reserve are unchanged. Enabling STP therefore serializes every identified submit
except post-only, and every matching-capable re-price, on that book.
Post-only orders never take liquidity and never run the STP scan, so
they keep the shared side; they are excluded from an identified taker's
window by that taker's exclusive hold, not by their own. The shared path for an
anonymous taker (`Hash32::zero()`, which skips the STP scan) is reachable
only through the match-only entry points (`match_order_with_user`,
`match_market_order_with_user`, `match_market_order_by_amount_with_user`):
`add_order` rejects a zero `user_id` with `MissingUserId` whenever STP is
enabled, so mixing anonymous and identified flow does not preserve submit
concurrency on an STP book.

**Scope.** The guarantee covers every mutation: the submit, cancel,
modify, mass-cancel and market-sweep entry points listed above, plus
snapshot restores. Nothing sits outside it; two things are worth
spelling out:

- The public API hands out no level handles. `get_bids()` / `get_asks()`
  cloned the live `Arc<PriceLevel>` handles, and `PriceLevel` exposes
  `add_order`, `update_order` and `match_order` publicly, so a caller could
  mutate a level behind the gate — and behind the indices, the risk state,
  STP, the kill switch, the order-state tracker and the listeners. Both were
  removed in 0.13.0 (issue #228); every level mutation now goes through
  `OrderBook`. Read a level's contents through the value-returning APIs
  instead: `create_snapshot(depth)` for a full snapshot of every level and
  order; `levels_with_cumulative_depth`, `levels_until_depth`,
  `levels_in_range` and `find_level` for `LevelInfo` views;
  `order_count_at_price`, `get_orders_at_price`, `get_all_orders` and
  `total_depth_at_levels` for per-price / per-book order data; `best_bid` /
  `best_ask` for the top of book.
- Snapshot restores are covered: the live `restore_from_snapshot(&self)`
  takes the exclusive side of the gate for its commit phase, and the
  `&mut self` package / JSON restores are exclusive by construction.

**Read-only queries are never gated**, so `best_bid`, `best_ask`,
snapshots, statistics and the iterators can be called from any thread,
including from inside a listener callback.

### 5. Custom Extra Data

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderMetadata {
    user_id: String,
    strategy: String,
}

let book = OrderBook::<OrderMetadata>::new("BTC/USD");

let metadata = OrderMetadata {
    user_id: "user123".to_string(),
    strategy: "market_maker".to_string(),
};

book.add_limit_order(
    OrderId::new(),
    50000,
    10,
    Side::Buy,
    TimeInForce::Gtc,
    Some(metadata)
)?;
```

---

## Examples

### Example 1: Simple Market Maker

```rust
use orderbook_rs::prelude::*;

fn market_maker_strategy(book: &OrderBook) -> Result<(), OrderBookError> {
    // Get current market state
    let snapshot = book.enriched_snapshot(5);
    
    if let (Some(mid), Some(spread_bps)) = (snapshot.mid_price, snapshot.spread_bps) {
        // Only make markets if spread is tight enough
        if spread_bps < 20.0 {
            let offset = 5.0;
            let bid_price = (mid - offset) as u64;
            let ask_price = (mid + offset) as u64;
            
            // Place orders
            book.add_limit_order(
                OrderId::new(),
                bid_price,
                10,
                Side::Buy,
                TimeInForce::Gtc,
                None
            )?;
            
            book.add_limit_order(
                OrderId::new(),
                ask_price,
                10,
                Side::Sell,
                TimeInForce::Gtc,
                None
            )?;
            
            println!("Market making: bid @ {}, ask @ {}", bid_price, ask_price);
        } else {
            println!("Spread too wide: {:.2} bps", spread_bps);
        }
    }
    
    Ok(())
}
```

### Example 2: Smart Order Execution

```rust
fn execute_large_order(
    book: &OrderBook,
    quantity: u64,
    side: Side
) -> Result<(), OrderBookError> {
    // Simulate to assess impact
    let simulation = book.simulate_market_order(side, quantity);
    
    println!("Simulation results:");
    println!("  Average price: {}", simulation.average_price);
    println!("  Price impact: {:.2}%", simulation.price_impact_percentage);
    
    // Decide execution strategy
    if simulation.price_impact_percentage < 0.5 {
        // Low impact: use market order
        println!("Executing market order");
        book.add_market_order(OrderId::new(), quantity, side, None)?;
    } else if simulation.price_impact_percentage < 2.0 {
        // Medium impact: use limit order at VWAP
        println!("Executing limit order at VWAP");
        book.add_limit_order(
            OrderId::new(),
            simulation.average_price as u64,
            quantity,
            side,
            TimeInForce::Gtc,
            None
        )?;
    } else {
        // High impact: split order
        println!("Splitting order due to high impact");
        let chunk_size = quantity / 4;
        for _ in 0..4 {
            book.add_limit_order(
                OrderId::new(),
                simulation.average_price as u64,
                chunk_size,
                side,
                TimeInForce::Gtc,
                None
            )?;
        }
    }
    
    Ok(())
}
```

### Example 3: Liquidity Monitoring

```rust
fn monitor_liquidity(book: &OrderBook) {
    let stats_bid = book.depth_statistics(Side::Buy, 10);
    let stats_ask = book.depth_statistics(Side::Sell, 10);
    
    println!("Liquidity Report:");
    println!("  Bid side:");
    println!("    Total volume: {}", stats_bid.total_volume);
    println!("    Std dev: {:.2}", stats_bid.std_dev_level_size);
    
    println!("  Ask side:");
    println!("    Total volume: {}", stats_ask.total_volume);
    println!("    Std dev: {:.2}", stats_ask.std_dev_level_size);
    
    // Check for thin book
    if book.is_thin_book(1000, 5) {
        println!("⚠️ WARNING: Thin book detected!");
        println!("  Recommendation: Reduce position sizes");
    }
    
    // Check for imbalance
    let imbalance = book.order_book_imbalance(5);
    if imbalance.abs() > 0.3 {
        if imbalance > 0.0 {
            println!("📈 Strong buy pressure detected");
        } else {
            println!("📉 Strong sell pressure detected");
        }
    }
}
```

---

## Troubleshooting

### Common Issues

**Issue: Order not added**

```rust
// Check for duplicate order ID
match book.add_limit_order(order_id, price, qty, Side::Buy, TimeInForce::Gtc, None) {
    Err(OrderBookError::DuplicateOrderId { .. }) => {
        // Generate new ID
        let new_id = OrderId::new();
        book.add_limit_order(new_id, price, qty, Side::Buy, TimeInForce::Gtc, None)?;
    }
    Ok(result) => { /* success */ }
    Err(e) => eprintln!("Error: {}", e),
}
```

**Issue: Market order not filled**

```rust
// Check available liquidity first
let depth = book.total_depth_at_levels(Side::Sell, 0);  // All levels
if depth < quantity {
    println!("Insufficient liquidity: {} available, {} needed", depth, quantity);
    // Use limit order instead
    book.add_limit_order(order_id, price, quantity, Side::Buy, TimeInForce::Gtc, None)?;
} else {
    book.add_market_order(order_id, quantity, Side::Buy, None)?;
}
```

**Issue: Performance degradation**

```rust
// Use enriched snapshots instead of multiple metric calls
// ❌ Slow
let mid = book.mid_price();
let spread = book.spread_bps();
let vwap = book.vwap(Side::Buy, 10);

// ✅ Fast
let snapshot = book.enriched_snapshot(10);
```

**Issue: Memory usage**

```rust
// Limit snapshot depth
let snapshot = book.create_snapshot(10);  // Only top 10 levels

// Instead of:
let snapshot = book.create_snapshot(0);  // All levels (high memory)
```

**Issue: A listener callback is slow or blocks**

Since 0.14.0 listeners run after the gate is released, so a listener that
mutates the same book no longer deadlocks. A listener that blocks still
delays every later event of the book (one dispatcher per book delivers them
in order), so hand the event off and do the work elsewhere.

```rust
// Preferred: hand the event off and work from another context.
book.set_trade_listener(Arc::new(move |trade: &TradeResult| {
    let _ = tx.send(trade.engine_seq);
}));
```

This applies to `TradeListener`, `PriceLevelChangedListener` and
`OrderStateListener` alike. See
[Trade Notifications](#2-trade-notifications) for the full contract.

### Debug Tips

```rust
// Enable logging
std::env::set_var("RUST_LOG", "debug");
tracing_subscriber::fmt::init();

// Check order book state
println!("Bid levels: {}", book.bid_levels());
println!("Ask levels: {}", book.ask_levels());
println!("Total orders: {}", book.bid_levels() + book.ask_levels());

// Verify order exists
if book.has_order(&order_id) {
    println!("Order {} exists", order_id);
} else {
    println!("Order {} not found", order_id);
}
```

---

## Performance Benchmarks

Based on Apple M4 Max processor:

**Single-threaded operations:**
- Add limit order: ~1.2M ops/sec
- Cancel order: ~1.5M ops/sec
- Market order: ~900K ops/sec
- Best bid/ask: ~15M ops/sec

**Multi-threaded (30 threads):**
- Total throughput: ~600K orders/sec
- Per-thread: ~20K orders/sec
- No per-price-level locking; concurrency across threads depends on the
  gate mode of the ops being issued (see
  [Concurrent Access](#4-concurrent-access)). These figures do not
  characterise a book running with self-trade prevention engaged: for the
  STP gate-mode comparison see `BENCH.md`.

**Metrics calculation:**
- VWAP: ~2µs (10 levels)
- Depth statistics: ~3µs (10 levels)
- Enriched snapshot: ~5µs (all metrics)

**Memory usage:**
- Base order book: ~1KB
- Per order: ~120 bytes
- 10,000 orders: ~1.2MB

---

## Further Reading

- [API Documentation](https://docs.rs/orderbook-rs)
- [Examples Directory](../examples/README.md)
- [GitHub Repository](https://github.com/joaquinbejar/OrderBook-rs)
- [Performance Analysis](../README.md#performance-analysis)

---

## Support

For issues, questions, or contributions:
- GitHub Issues: https://github.com/joaquinbejar/OrderBook-rs/issues
- Email: jb@taunais.com

---

**Version:** 0.4.8  
**Last Updated:** October 2025  
**License:** MIT
