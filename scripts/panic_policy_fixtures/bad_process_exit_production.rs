//! Fixture (issue #242): `std::process::exit` (and `clippy::exit`'s target)
//! in production must fail the gate.

pub fn bad(code: i32) {
    std::process::exit(code);
}
