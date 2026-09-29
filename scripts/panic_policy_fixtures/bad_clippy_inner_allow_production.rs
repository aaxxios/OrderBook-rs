//! Fixture (issue #260): the inner, whole-file form of the old ratchet
//! marker (`#![allow(clippy::...)]`) must fail too.

#![allow(
    clippy::indexing_slicing
)]

pub fn first(v: &[u8]) -> u8 {
    v[0]
}
