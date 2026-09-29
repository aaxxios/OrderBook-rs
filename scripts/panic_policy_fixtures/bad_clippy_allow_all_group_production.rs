//! Fixture (issue #260, PR #298 review): `clippy::all` is refused as a
//! conservative catch-all, inner form included.

#![expect(clippy::all)]

pub fn noop() {}
