//! Fixture (issue #242, PR #266 review): `std::panic::resume_unwind` in
//! production must fail the gate — it resumes an in-flight unwind (usually
//! one captured by `catch_unwind`, itself already forbidden), so allowing
//! it would just be a laundering path around the same ban.

pub fn bad(payload: Box<dyn std::any::Any + Send>) -> ! {
    std::panic::resume_unwind(payload)
}
