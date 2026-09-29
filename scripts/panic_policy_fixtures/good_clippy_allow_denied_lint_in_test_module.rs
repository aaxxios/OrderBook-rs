//! Fixture (issue #260): a co-located test module may allow the lints that
//! have no `clippy.toml` "in tests" toggle; comments and strings that merely
//! mention `#[allow(clippy::unwrap_used)]` are not attributes.

pub const DOC: &str = "#[allow(clippy::unwrap_used)]";

// #[allow(clippy::arithmetic_side_effects)]
pub fn noop() {}

#[cfg(test)]
// tests may panic: rules/global_rules.md § Testing
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
mod tests {
    #[allow(clippy::panic_in_result_fn, clippy::manual_assert)]
    #[test]
    fn it_adds() {
        let x = 1u64 + 1;
        assert_eq!(x as u8, 2);
    }
}
