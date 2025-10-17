//! OrderBook implementation for managing multiple price levels and order matching.

pub mod book;
pub mod error;
/// Functional-style iterators for order book analysis.
pub mod iterators;
/// Multi-book management with centralized trade event routing.
pub mod manager;
/// Market impact simulation and liquidity analysis.
pub mod market_impact;
pub mod matching;
/// Aggregate statistics for order book analysis.
pub mod statistics;

mod cache;
/// Contains the core logic for modifying the order book state, such as adding, canceling, or updating orders.
pub mod modifications;
pub mod operations;
mod pool;
mod private;
pub mod snapshot;
mod tests;
/// Trade-related types including TradeResult and TradeListener for monitoring order executions.
pub mod trade;

pub use book::OrderBook;
pub use error::OrderBookError;
pub use iterators::LevelInfo;
pub use market_impact::{MarketImpact, OrderSimulation};
pub use snapshot::{
    EnrichedSnapshot, MetricFlags, ORDERBOOK_SNAPSHOT_FORMAT_VERSION, OrderBookSnapshot,
    OrderBookSnapshotPackage,
};
pub use statistics::{DepthStats, DistributionBin};
