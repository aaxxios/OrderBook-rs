//! Fixture (issue #260): a production-adjacent `#[cfg(test)]` seam (not a
//! `mod tests` block, not a `test_`-prefixed helper) stays in scope for the
//! allow-escape check.

#[cfg(test)]
#[allow(clippy::panic)]
pub fn seam_hook() {}
