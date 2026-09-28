//! Fixture (issue #242): a `#[cfg(test)]`-attributed struct FIELD (this
//! crate's `src/orderbook/book.rs` `stp_interleave_hook` /
//! `level_interleave_hook` shape) is bounded by its own trailing comma, not
//! by the enclosing struct's closing brace. Without that comma stop, the
//! struct's own closing `}` is never recognized by
//! `find_item_terminator` (it only tracks `{`/`;`/`,` at depth 0, not a
//! bare closing `}`), so the scan would run past it into the next
//! unrelated item — here `impl Widget`'s opening brace — and wrongly
//! widen the production-adjacent `#[cfg(test)]` scope to cover
//! `first()`'s real, safe indexing below. With the comma stop, the scope
//! ends at `hook: Option<u8>,` and `first()`'s indexing is never inside
//! it, so this fixture must NOT be flagged.

pub struct Widget {
    #[cfg(test)]
    hook: Option<u8>,
    data: u8,
}

impl Widget {
    pub fn first(&self) -> u8 {
        let buf = [self.data, 0, 0, 0];
        buf[0]
    }
}
