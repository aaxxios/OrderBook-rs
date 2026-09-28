//! Fixture (issue #242, PR #266 review): `catch_unwind` inside a co-located
//! `mod tests { ... }` block is real test code (deliberately probing a
//! panic path under test, e.g. to assert a poison/kill-switch behavior) and
//! must NOT be flagged.

#[cfg(test)]
mod tests {
    #[test]
    fn deliberately_panics_and_catches_it() {
        let result = std::panic::catch_unwind(|| {
            panic!("intentional test-only panic");
        });
        assert!(result.is_err());
    }
}
