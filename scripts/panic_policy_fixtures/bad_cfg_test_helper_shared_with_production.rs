//! Fixture (issue #242): a standalone `#[cfg(test)]` production helper —
//! NOT a `mod tests { ... }` block — must still fail the gate. No file in
//! this crate currently has this exact shape (this fixture is a safety net,
//! not a regression repro), but `src/orderbook/book.rs`'s per-field
//! `#[cfg(test)] stp_interleave_hook` / `level_interleave_hook` show the
//! same family of gap: clippy's own `allow-unwrap-in-tests` /
//! `allow-panic-in-tests` (`clippy.toml`) wrongly exempts ANY
//! `#[cfg(test)]`-attributed item, regardless of whether it is a `mod
//! tests` block. `rules/global_rules.md`'s Testing section is explicit
//! that this permission "does not extend to production functions,
//! including their `cfg(test)` branches, or to helpers shared with
//! production."

#[cfg(test)]
pub(crate) fn helper_shared_with_production(n: usize) -> usize {
    debug_assert!(n > 0, "n must be positive");
    n.checked_sub(1).unwrap()
}
