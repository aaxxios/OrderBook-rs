//! Fixture (issue #260): with the ratchet ledgers gone the gate is absolute,
//! so a production `#[allow(clippy::<lint>)]` naming a lint that Cargo.toml's
//! `[lints.clippy]` denies is an escape hatch and must fail.

#[allow(clippy::too_many_arguments, clippy::arithmetic_side_effects)]
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}
