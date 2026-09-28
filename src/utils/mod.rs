mod time;

#[cfg(test)]
mod tests;

pub use time::{TimeError, current_time_millis, try_current_time_millis};

#[cfg(feature = "alloc-counters")]
pub mod counting_allocator;

#[cfg(feature = "alloc-counters")]
pub use counting_allocator::{AllocSnapshot, CountingAllocator};
