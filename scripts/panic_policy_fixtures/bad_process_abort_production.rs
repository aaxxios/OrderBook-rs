//! Fixture (issue #242): `std::process::abort` in production must fail the
//! gate — the same deliberate-process-termination ban as `std::process::exit`
//! (`rules/global_rules.md`'s Production Panic Policy: "deliberate process
//! termination (`std::process::exit` / `abort` in library code)").

pub fn bad() {
    std::process::abort();
}
