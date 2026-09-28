//! Fixture (issue #242, PR #266 review): `std::panic::panic_any` in
//! production must fail the gate — it initiates a panic (with a non-`&str`
//! payload) exactly like `panic!`, just via a function instead of a macro.

pub fn bad(code: i32) -> ! {
    std::panic::panic_any(code)
}
