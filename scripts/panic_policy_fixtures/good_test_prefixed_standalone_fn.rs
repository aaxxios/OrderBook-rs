//! Fixture (issue #242): a standalone `#[cfg(test)] fn test_*` — this
//! crate's convention for a helper called ONLY from test code — is
//! test-only and must NOT be flagged, even though it is not inside a `mod
//! tests { ... }` block. No file in this crate currently uses this exact
//! shape outside a `tests/` directory (this fixture is a safety net, not a
//! regression repro); ported from PriceLevel's `test_poison_guard`
//! convention (`src/price_level/level.rs` there).

#[cfg(test)]
pub(crate) fn test_deliberately_panics() {
    let _ = std::panic::catch_unwind(|| {
        panic!("intentional poison for a test");
    });
}
