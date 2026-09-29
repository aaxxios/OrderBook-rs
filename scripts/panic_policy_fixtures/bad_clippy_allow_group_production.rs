//! Fixture (issue #260): allowing a whole lint group that contains the
//! denied restriction lints is the same escape hatch.

#[allow(clippy::restriction)]
pub fn noop() {}
