//! Fixture (issue #242, PR #266 review): `std::panic::resume_unwind` inside
//! a co-located `mod tests { ... }` block is real test code (re-propagating
//! a caught panic to assert its payload) and must NOT be flagged.

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn recaptures_and_resumes_the_unwind() {
        let result = std::panic::catch_unwind(|| {
            panic!("boom");
        });
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
