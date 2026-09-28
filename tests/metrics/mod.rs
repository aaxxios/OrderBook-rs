//! Standalone integration-test binary for the optional `metrics`
//! feature (issue #60). Lives in its own crate test entry point so
//! the global `metrics` recorder isn't perturbed by the broader
//! integration suite under `tests/unit/`.

// This crate root is entirely integration-test code, not production
// (issue #242's Production Panic Policy gate, `[lints.clippy]` in
// `Cargo.toml`, is package-wide and would otherwise apply here too).
// Test fixtures freely `.unwrap()` / `.expect()` setup, index scratch
// buffers and do raw arithmetic on sample sizes; none of that reaches
// `src/`. Panics and assertions in tests are permitted by
// `rules/global_rules.md`'s Testing section.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

#[cfg(feature = "metrics")]
mod metrics_tests;
