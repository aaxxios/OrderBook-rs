//! Tiny deterministic xorshift PRNG, copied from
//! `benches/order_book/hdr_common.rs` in the main crate. Duplicated
//! rather than shared because this crate is built from a *copy* dropped
//! into two independent git worktrees (see `scripts/bench_compare.sh`)
//! and must not depend on anything under the main crate's own
//! `benches/` tree — only on the library itself, via the path
//! dependency in `Cargo.toml`.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    #[inline]
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    #[inline]
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(lo <= hi);
        let span = hi - lo + 1;
        lo + (self.next() % span)
    }
}
