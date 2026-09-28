//! Fixture (issue #242, PR #266 review): `catch_unwind` in production must
//! fail the gate. `catch_unwind` itself never panics, but
//! `rules/global_rules.md`'s Production Panic Policy explicitly forbids it
//! ("Never evade this policy with... `catch_unwind`"): catching a panic and
//! continuing is exactly the "recover and keep going" behavior this crate's
//! typed-error contract replaces.

pub fn bad(f: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    std::panic::catch_unwind(f).is_ok()
}
