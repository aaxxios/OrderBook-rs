//! Fixture (issue #242 review): indexing inside a non-test-shaped
//! `#[cfg(test)] mod test_seam { ... }` (a production-adjacent seam module —
//! a singular `test_`-prefixed MODULE name, unlike this crate's real
//! `src/orderbook/tests/*.rs` co-located test modules, is deliberately NOT
//! treated as a test module by `_TEST_MODULE_NAME`) must also fail the
//! gate, not just a standalone `fn`.

#[cfg(test)]
pub(crate) mod test_seam {
    pub(crate) fn first_byte(buf: &[u8]) -> u8 {
        buf[0]
    }
}
