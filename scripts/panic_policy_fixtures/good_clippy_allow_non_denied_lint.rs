//! Fixture (issue #260): allowing a clippy lint that `[lints.clippy]` does
//! NOT deny is a style choice, not a panic-policy escape.

#[allow(clippy::too_many_arguments)]
pub fn many(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8, g: u8, h: u8) -> u8 {
    a.max(b).max(c).max(d).max(e).max(f).max(g).max(h)
}

#[allow(clippy::type_complexity)]
pub type Nested = Option<Option<Option<Vec<(u8, u8)>>>>;
