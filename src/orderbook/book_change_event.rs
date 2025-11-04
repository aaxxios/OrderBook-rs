use std::sync::Arc;
use pricelevel::Side;

/// Event data for orderbook price level changes.
/// It is assumed that the listener is aware of the
/// order book context so we are not adding symbol here.
/// This event is sent on operations that update the order book price levels
/// e.g. adding, cancelling, updating or matching order
#[derive(Debug)]
pub struct PriceLevelChangedEvent {
    /// the order book side of the price level 
    pub side: Side,
    
    /// price level price
    pub price: u64,

    /// latest visible quantity of the order book at this price level
    pub quantity: u64,
}

pub type PriceLevelChangedListener = Arc<dyn Fn(PriceLevelChangedEvent) + Send + Sync>;
