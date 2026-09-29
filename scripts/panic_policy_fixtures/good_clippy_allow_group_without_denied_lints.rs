//! Fixture (issue #260, PR #298 review): groups that contain no denied lint
//! (`clippy::style`, `clippy::complexity`, `clippy::nursery`, ...) are not
//! panic-policy escapes.

#[allow(clippy::style, clippy::complexity, clippy::nursery, clippy::perf)]
pub fn noop() {}
