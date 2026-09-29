//! Fixture (issue #260, PR #298 review): `clippy::pedantic` contains the
//! denied narrowing casts and `manual_assert`, so allowing the group is the
//! same escape hatch as allowing those lints by name.

#[allow(clippy::pedantic)]
pub fn narrow(v: u64) -> u8 {
    u8::try_from(v).unwrap_or_default()
}
