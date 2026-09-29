//! Fixture (issue #260): `#[expect(...)]` is an `allow` that also warns when
//! unused; nesting it in `cfg_attr` does not hide it from the gate.

#[cfg_attr(not(feature = "x"), expect(clippy::unwrap_used))]
pub fn get(v: Option<u8>) -> u8 {
    v.unwrap_or_default()
}
