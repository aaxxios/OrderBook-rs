//! Fixture (issue #242, PR #266 review): `std::panic::panic_any` inside a
//! co-located `mod tests { ... }` block is real test code and must NOT be
//! flagged.

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn panics_with_a_non_str_payload() {
        std::panic::panic_any(42_i32)
    }
}
