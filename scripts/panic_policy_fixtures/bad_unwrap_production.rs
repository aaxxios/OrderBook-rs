//! Fixture (issue #242): a plain production function that unwraps must fail
//! the gate. Not wired into any `mod` — never compiled, scanned as text only.

pub fn bad(v: Option<i32>) -> i32 {
    v.unwrap()
}
