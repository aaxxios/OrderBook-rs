//! Fixture (issue #260, PR #298 review): a group allow nested in `cfg_attr`
//! is still an escape hatch.

#[cfg_attr(feature = "x", allow(clippy::too_many_arguments, clippy::pedantic))]
pub fn noop() {}
