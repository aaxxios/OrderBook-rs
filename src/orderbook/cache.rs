/******************************************************************************
   Author: Joaquín Béjar García
   Email: jb@taunais.com
   Date: 15/7/25
******************************************************************************/

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Serialize, Default)]
pub struct PriceLevelCache {
    best_bid_price: AtomicU64,
    best_ask_price: AtomicU64,
    cache_valid: AtomicBool,
}

impl PriceLevelCache {
    pub fn new() -> Self {
        Self {
            best_bid_price: AtomicU64::new(0),
            best_ask_price: AtomicU64::new(0),
            cache_valid: AtomicBool::new(false),
        }
    }

    pub fn invalidate(&self) {
        self.cache_valid.store(false, Ordering::Relaxed);
    }

    pub fn get_cached_best_bid(&self) -> Option<u64> {
        if self.cache_valid.load(Ordering::Relaxed) {
            let price = self.best_bid_price.load(Ordering::Relaxed);
            if price > 0 { Some(price) } else { None }
        } else {
            None
        }
    }

    pub fn get_cached_best_ask(&self) -> Option<u64> {
        if self.cache_valid.load(Ordering::Relaxed) {
            let price = self.best_ask_price.load(Ordering::Relaxed);
            if price > 0 { Some(price) } else { None }
        } else {
            None
        }
    }

    pub fn update_best_prices(&self, best_bid: Option<u64>, best_ask: Option<u64>) {
        if let Some(bid) = best_bid {
            self.best_bid_price.store(bid, Ordering::Relaxed);
        } else {
            self.best_bid_price.store(0, Ordering::Relaxed);
        }

        if let Some(ask) = best_ask {
            self.best_ask_price.store(ask, Ordering::Relaxed);
        } else {
            self.best_ask_price.store(0, Ordering::Relaxed);
        }

        self.cache_valid.store(true, Ordering::Relaxed);
    }
}
