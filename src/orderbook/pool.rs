use pricelevel::Id;
use std::cell::RefCell;
use std::sync::Arc;

/// A memory pool for reusing vectors to reduce allocations in hot paths.
///
/// Every access is a fallible runtime borrow (#246): the pool is a
/// thread-local cache, never a source of truth, so a pool that is already
/// borrowed (a reentrant call on the same thread) degrades to a fresh
/// buffer on `get_*` and to dropping the buffer on `return_*` instead of
/// panicking. The caller sees the same empty `Vec` either way; only the
/// allocation reuse is lost for that call.
#[derive(Debug)]
pub struct MatchingPool {
    /// Reusable buffers of `(fully_consumed_maker_id, filled_quantity)` collected
    /// during a match so terminal `Filled` events carry the true fill (#104).
    filled_orders_pool: RefCell<Vec<Vec<(Id, u64)>>>,
    price_vec_pool: RefCell<Vec<Vec<u128>>>,
    /// Reusable buffers for the per-level STP scan snapshot. Each match fills
    /// one of these via `PriceLevel::snapshot_by_seq_into` instead of allocating
    /// a fresh `Vec<Arc<OrderType<()>>>` per conflicting level (#107).
    order_snapshot_pool: RefCell<Vec<Vec<Arc<pricelevel::OrderType<()>>>>>,
}

/// Pop a pooled buffer, or build a fresh one with `capacity` when the pool is
/// empty or already borrowed (#246).
#[inline]
fn take_or_fresh<V>(pool: &RefCell<Vec<Vec<V>>>, capacity: usize) -> Vec<V> {
    pool.try_borrow_mut()
        .ok()
        .and_then(|mut buffers| buffers.pop())
        .unwrap_or_else(|| Vec::with_capacity(capacity))
}

/// Clear `vec` and park it in the pool. When the pool is already borrowed
/// the buffer is simply dropped (#246): the next `get_*` allocates afresh.
#[inline]
fn park<V>(pool: &RefCell<Vec<Vec<V>>>, mut vec: Vec<V>) {
    vec.clear();
    if let Ok(mut buffers) = pool.try_borrow_mut() {
        buffers.push(vec);
    }
}

impl MatchingPool {
    /// Creates a new, empty matching pool.
    pub fn new() -> Self {
        MatchingPool {
            filled_orders_pool: RefCell::new(Vec::with_capacity(4)),
            price_vec_pool: RefCell::new(Vec::with_capacity(4)),
            order_snapshot_pool: RefCell::new(Vec::new()),
        }
    }

    /// Retrieves a vector for filled orders (with their filled quantities) from
    /// the pool, or a fresh one when the pool is empty or already borrowed.
    pub fn get_filled_orders_vec(&self) -> Vec<(Id, u64)> {
        take_or_fresh(&self.filled_orders_pool, 16)
    }

    /// Returns a filled orders vector to the pool for reuse (dropped when the
    /// pool is already borrowed).
    pub fn return_filled_orders_vec(&self, vec: Vec<(Id, u64)>) {
        park(&self.filled_orders_pool, vec);
    }

    /// Retrieves a vector for the per-level STP scan snapshot from the pool.
    ///
    /// Mirrors [`Self::get_filled_orders_vec`]: pops a reusable buffer or
    /// allocates a fresh one. The caller fills it with
    /// `PriceLevel::snapshot_by_seq_into` and returns it via
    /// [`Self::return_order_snapshot_vec`] (#107).
    pub fn get_order_snapshot_vec(&self) -> Vec<Arc<pricelevel::OrderType<()>>> {
        take_or_fresh(&self.order_snapshot_pool, 16)
    }

    /// Returns a STP scan snapshot vector to the pool for reuse.
    ///
    /// Clears the buffer **first** so the `Arc<OrderType<()>>` clones are
    /// dropped immediately — leaving them in place would pin resting orders
    /// alive across reuse. Mirrors [`Self::return_filled_orders_vec`] (#107).
    pub fn return_order_snapshot_vec(&self, vec: Vec<Arc<pricelevel::OrderType<()>>>) {
        park(&self.order_snapshot_pool, vec);
    }

    /// Retrieves a vector for prices from the pool, or a fresh one when the
    /// pool is empty or already borrowed.
    pub fn get_price_vec(&self) -> Vec<u128> {
        take_or_fresh(&self.price_vec_pool, 32)
    }

    /// Returns a price vector to the pool for reuse (dropped when the pool is
    /// already borrowed).
    pub fn return_price_vec(&self, vec: Vec<u128>) {
        park(&self.price_vec_pool, vec);
    }
}

impl Default for MatchingPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_reuses_returned_buffer() {
        let pool = MatchingPool::new();
        let mut prices = pool.get_price_vec();
        prices.push(7);
        let capacity = prices.capacity();
        pool.return_price_vec(prices);

        let reused = pool.get_price_vec();
        assert!(reused.is_empty(), "a parked buffer comes back cleared");
        assert_eq!(reused.capacity(), capacity, "the parked buffer is reused");
    }

    /// #246: a reentrant access while the pool is borrowed degrades to a
    /// fresh buffer / a dropped buffer instead of a `BorrowMutError` panic,
    /// and the pool is intact once the outer borrow ends.
    #[test]
    fn test_pool_falls_back_when_already_borrowed() {
        let pool = MatchingPool::new();
        let mut parked = pool.get_filled_orders_vec();
        parked.reserve(1_000);
        let parked_capacity = parked.capacity();
        pool.return_filled_orders_vec(parked);

        {
            let _outer_filled = pool.filled_orders_pool.borrow_mut();
            let _outer_prices = pool.price_vec_pool.borrow_mut();
            let _outer_snapshot = pool.order_snapshot_pool.borrow_mut();

            let fresh = pool.get_filled_orders_vec();
            assert!(fresh.is_empty());
            assert_eq!(fresh.capacity(), 16, "fresh fallback buffer");
            // Returning while borrowed drops the buffer instead of panicking.
            pool.return_filled_orders_vec(fresh);

            let prices = pool.get_price_vec();
            assert_eq!(prices.capacity(), 32);
            pool.return_price_vec(prices);

            let snapshot = pool.get_order_snapshot_vec();
            assert_eq!(snapshot.capacity(), 16);
            pool.return_order_snapshot_vec(snapshot);
        }

        // The buffer parked before the borrow is still there and nothing
        // returned during the borrow was parked.
        assert_eq!(pool.filled_orders_pool.borrow().len(), 1);
        assert!(pool.price_vec_pool.borrow().is_empty());
        assert!(pool.order_snapshot_pool.borrow().is_empty());
        let reused = pool.get_filled_orders_vec();
        assert_eq!(reused.capacity(), parked_capacity);
    }
}
