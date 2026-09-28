//! Thin adapter over the `orderbook-rs` API surface this harness calls.
//!
//! `workloads.rs` is byte-identical whichever side of the comparison it
//! is compiled for; every version-specific detail is meant to live
//! here, isolated behind the `v0_13` / `head` Cargo features selected by
//! `scripts/bench_compare.sh`. As of the #258 audit the three workloads
//! this crate drives (`add_only`, `cancel_only`, `aggressive_walk`) call
//! nothing that differs between the `v0.13.1` tag and HEAD — both
//! feature arms below have the same body — so this module currently
//! documents the seam more than it uses it. Add a scenario that touches
//! an API which *has* changed across versions this harness targets
//! (`create_snapshot` going from a plain return value pre-`v0.14.0` to a
//! `Result`, or a future `pricelevel::Id` constructor rename) by adding
//! a diverging match arm here, not by branching inside `workloads.rs`.
//!
//! Every scenario submits and cancels with a `user_id` (never the
//! userless `add_limit_order` / `submit_market_order` paths), matching
//! the headline `benches/order_book/*_hdr.rs` shape exactly — a PR
//! review on #258 caught `cancel_only` diverging from
//! `cancel_only_hdr`'s `submit_gtc` (which always attaches an owner),
//! which meant this crate was comparing a different book/index shape
//! than the scenario it claims to mirror.
//!
//! # `owner` returns `[u8; 32]`, not `pricelevel::Hash32`
//!
//! `add_limit_order_with_user` / `submit_market_order_with_user` take
//! `user: [u8; 32]` and convert with `.into()` at the call site, letting
//! type inference resolve to *whichever* `Hash32` the active
//! `orderbook-rs` path dependency's own `pricelevel` edge provides.
//! This crate tried a direct `pricelevel` dependency during #258 review
//! to name `Hash32` explicitly and reverted it (see `Cargo.toml`'s doc
//! comment): `v0.13.1` and HEAD pin semver-incompatible `pricelevel`
//! minors (`0.9` / `0.10`), so a version range wide enough to match
//! both does not make Cargo unify on one of them — it resolves this
//! crate's own direct edge independently (to the newest match) from
//! `orderbook-rs`'s transitive edge, yielding two incompatible copies
//! of the crate and a type-mismatch on every `Hash32` argument.
//! `From<[u8; 32]> for Hash32` is present in both `0.9.2` and `0.10.0`,
//! so passing a plain array and converting through the callee's own
//! resolved type sidesteps the whole problem — this file never needs
//! to name `Hash32` at all.

#[cfg(feature = "head")]
mod imp {
    use orderbook_rs::{Id, OrderBook, Side, TimeInForce};

    pub type Book = OrderBook<()>;

    #[inline]
    pub fn new_book(symbol: &str) -> Book {
        OrderBook::new(symbol)
    }

    #[inline]
    pub fn owner(byte: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = byte;
        bytes
    }

    #[inline]
    pub fn add_limit_order_with_user(
        book: &Book,
        id: Id,
        price: u128,
        qty: u64,
        side: Side,
        user: [u8; 32],
    ) {
        let _ = book.add_limit_order_with_user(
            id,
            price,
            qty,
            side,
            TimeInForce::Gtc,
            user.into(),
            None,
        );
    }

    #[inline]
    pub fn cancel_order(book: &Book, id: Id) {
        let _ = book.cancel_order(id);
    }

    #[inline]
    pub fn submit_market_order_with_user(
        book: &Book,
        id: Id,
        qty: u64,
        side: Side,
        user: [u8; 32],
    ) {
        let _ = book.submit_market_order_with_user(id, qty, side, user.into());
    }
}

#[cfg(feature = "v0_13")]
mod imp {
    // Identical to the `head` arm today (verified during the #258
    // audit) — kept as a separate module, not a shared one, so a future
    // divergence is a one-file edit here instead of a rewrite of every
    // call site in `workloads.rs`.
    use orderbook_rs::{Id, OrderBook, Side, TimeInForce};

    pub type Book = OrderBook<()>;

    #[inline]
    pub fn new_book(symbol: &str) -> Book {
        OrderBook::new(symbol)
    }

    #[inline]
    pub fn owner(byte: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = byte;
        bytes
    }

    #[inline]
    pub fn add_limit_order_with_user(
        book: &Book,
        id: Id,
        price: u128,
        qty: u64,
        side: Side,
        user: [u8; 32],
    ) {
        let _ = book.add_limit_order_with_user(
            id,
            price,
            qty,
            side,
            TimeInForce::Gtc,
            user.into(),
            None,
        );
    }

    #[inline]
    pub fn cancel_order(book: &Book, id: Id) {
        let _ = book.cancel_order(id);
    }

    #[inline]
    pub fn submit_market_order_with_user(
        book: &Book,
        id: Id,
        qty: u64,
        side: Side,
        user: [u8; 32],
    ) {
        let _ = book.submit_market_order_with_user(id, qty, side, user.into());
    }
}

#[cfg(not(any(feature = "head", feature = "v0_13")))]
compile_error!(
    "orderbook-rs-bench-compare requires exactly one of the `head` / `v0_13` \
     features — pass `--features head` or `--features v0_13` (scripts/bench_compare.sh \
     does this for you per side)."
);

#[cfg(all(feature = "head", feature = "v0_13"))]
compile_error!(
    "orderbook-rs-bench-compare requires exactly one of the `head` / `v0_13` \
     features, not both — `--no-default-features --features <one>`."
);

pub use imp::*;
