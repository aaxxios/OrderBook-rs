Rules for writing Rust in a production, lock-free limit-order-book library
(`orderbook-rs`). All code, comments, docs, commit messages, and PR
descriptions in English.

---

## Compiler Attributes

### #[must_use]
- All pure functions (price math, quantity math, validation, snapshot /
  statistics queries, iterator constructors).
- Builder pattern methods — message:
  `"builders do nothing unless .build() is called"`.
- `Result`-returning functions whose outcome the caller MUST handle:
  `Result<TradeResult, OrderBookError>`,
  `Result<MassCancelResult, OrderBookError>`, journal / replay returns.

### #[inline] / #[inline(always)] / #[inline(never)]
- `#[inline]`: small frequent functions — newtype accessors, enum
  conversions, cache best-bid / best-ask lookups, comparison helpers on
  `Price` / `Quantity`.
- `#[inline(always)]`: ONLY proven hot paths — inner matching loop steps.
  Needs a Criterion benchmark before it goes in.
- `#[inline(never)]`: error construction, logging helpers, snapshot
  restoration, mass-cancel traversal, NATS publish paths.
- No attribute: mid-size functions (10–50 lines).

### #[cold]
- Error construction helpers, validation failures, unreachable branches kept
  for safety, journal-corruption paths, replay-divergence paths.

### #[repr]
- `#[repr(u8)]`: small enums with stable values (`OrderStatus`,
  `CancelReason`, `STPMode`, `Side`, `MetricFlags` variants where applicable).
- `#[repr(C)]`: only if a struct crosses FFI / `cdylib` boundaries — flag
  explicitly if introduced.

### #[derive] — exact order
```
Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default
```
Derive only what is needed. No `Ord` if ordering is meaningless. No `Default`
if no sensible default exists. `Copy` only on small POD types (newtype
wrappers around `u64` / `f64`).

Serde conventions:
- `#[serde(rename_all = "snake_case")]` on public-facing DTOs unless a
  downstream consumer (NATS topic schema, recorded fixture) requires a
  specific casing.
- `#[serde(transparent)]` for single-field newtypes.
- `#[serde(deny_unknown_fields)]` on deserialized *config* / input DTOs and
  on internal journal entries (schema drift there IS a bug). **Do NOT** set
  it on snapshot-restoration DTOs that must remain forward-compatible.

---

## Type Safety

- Use the `pricelevel` newtypes at every domain boundary: `Id`, `Price`,
  `Quantity`, `TimestampMs`, `OrderType`, `Side`, `TimeInForce`. Do not pass
  raw `u64` / `f64` for those concepts across module boundaries.
- Crate-local newtypes (`OrderId` alias, `DefaultOrderBook`, etc.) — inner
  fields private where the type enforces an invariant.
- Constructors: `new()` for infallible, `try_new()` returning
  `Result<Self, OrderBookError>` for validation. No `pub` fields that can
  violate invariants.
- Monetary math stays on `Price` / `Quantity` (fixed-point inside
  `pricelevel`). Do NOT drop to raw `f64` in the matching path.
- Use `u64` for counts and non-negative integer parameters. `NonZeroU64` /
  `NonZeroUsize` where zero is structurally invalid (batch sizes, segment
  sizes, replay intervals).

---

## Arithmetic

- Integer arithmetic in production, including offsets, lengths, capacities,
  counters and journal / wire offsets: use `checked_add`, `checked_sub`,
  `checked_mul`, `checked_div`, `checked_rem`, `checked_shl`, `checked_shr`
  and `checked_neg` as applicable. Handle overflow, zero divisors, signed
  minimum divided by `-1`, and invalid shift counts as a typed error; never
  rely on debug checks or release wrapping. Handle fallible integer
  conversions with `TryFrom` / `TryInto` rather than narrowing `as` casts or
  unwrapping a conversion result.
- Never `saturating_*` or `wrapping_*` on state: sequencer ids, journal
  sequence numbers, NATS retry counters, mass-cancel totals, book quantity
  and depth aggregates, statistics. Silently clamping protocol state hides a
  bug.
- Price / quantity arithmetic goes through `pricelevel` APIs when they
  exist — do not reimplement tick / lot rounding here.
- `f64` arithmetic in analytics (VWAP, imbalance, IV solver): guard against
  NaN / Inf at the boundary. Return a typed error rather than propagating
  NaN.
- Guard floating-point division explicitly (zero volume means no VWAP).
  Integer division and remainder follow the checked rules above. `f64` to
  integer conversions validate range and finiteness first.

---

## Error Handling

- `thiserror` for typed, domain-specific errors. Canonical enums:
  `OrderBookError`, `ManagerError`, `IVError`, `JournalError`, `ReplayError`,
  `SerializationError`. New subsystems add a module-scoped enum and
  aggregate into `OrderBookError` / `ManagerError` via `#[from]`.
- No `anyhow`. All public functions return concrete error types.
- Error messages: lowercase, human-readable, include the offending value
  (`price`, `order_id`, `user_id`, `segment_id`) when possible.
- Wrap lower-level errors with `#[from]` where the mapping is unambiguous
  (`serde_json::Error`, `bincode::Error`, `std::io::Error`,
  `async_nats::Error`).
- Follow the Production Panic Policy below on every error and invariant
  path. Use `?`, pattern matching and `.ok_or_else()` to propagate typed
  failures. A replay divergence or a corrupted journal is reported as
  `ReplayError` / `JournalError`, never asserted.
- Trade / match results MUST be mapped to typed error variants, never
  returned as opaque `serde_json::Value` on the public surface.

---

## Production Panic Policy

Production code must not initiate a panic, including on invalid input, failed
invariants, dependency errors or exceptional branches. This applies in debug
and release builds, with every feature combination, and when production
functions are compiled or called by tests. Calling a branch "unreachable" does
not exempt it. Tests and fixtures that exist exclusively for testing may panic,
assert and unwrap freely; see Testing below.

- Never use `.unwrap()`, `.expect()`, `.unwrap_err()` or `.expect_err()` in
  production, even for a value believed to be valid.
- Never use `panic!`, `todo!`, `unimplemented!`, `unreachable!`, `panic_any`
  or `resume_unwind`. All `assert!`, `assert_eq!`, `assert_ne!` and
  `debug_assert!` / `debug_assert_eq!` / `debug_assert_ne!` calls are forbidden
  in production, including checks intended only for development builds.
- Use checked access such as `.get()` / `.get_mut()` instead of indexing or
  slicing expressions on arrays, slices, collections or strings. Validate
  ranges and UTF-8 boundaries before string operations; return an error for
  invalid offsets. This includes decoding journal segments, bincode payloads
  and `wire` frames: every length, offset and header field read from bytes is
  untrusted until checked. Apply the Arithmetic rules to index and range
  calculations.
- Handle capacity arithmetic and conversions with checked operations. Use
  `try_reserve` / `try_reserve_exact` for fallible collection growth where
  applicable, and map capacity/allocation errors to a typed failure. Do not
  assume a length hint, a decoded length prefix or an input-derived capacity
  is safe.
- Handle lock poisoning: `std::sync::Mutex` / `RwLock` `lock()`, `read()` and
  `write()` return `Result`; map `PoisonError` to a typed failure or recover
  explicitly and document why the protected state is still consistent. Use
  fallible runtime borrow operations such as `RefCell::try_borrow` and
  `try_borrow_mut`.
- Async surface (`BookManagerTokio`, NATS publishers): a `JoinHandle` await
  returns `Result<_, JoinError>`; handle it, including a task that panicked or
  was cancelled. Handle channel send / receive errors (closed receiver, lagged
  `broadcast`). Do not call APIs that panic outside a runtime (for example
  `tokio::spawn` or `Handle::current()` from a thread without one) unless the
  runtime is established by the type; prefer `Handle::try_current()`.
- Time: `Duration` and `Instant` / `SystemTime` arithmetic uses the `checked_*`
  forms; `SystemTime::duration_since` returns `Result` and must be handled.
- Review collection operations, atomic orderings, time and iterator
  arithmetic, serialization and dependency calls for their documented panic
  conditions. This list is not exhaustive: every reachable operation,
  including error formatting and cleanup, must be reviewed. Apart from the
  explicitly forbidden forms above, an API with documented panic conditions
  may be called only when type invariants or preceding checked validation
  establish all required preconditions; otherwise use a fallible alternative
  or redesign the path.
- Review implicit trait calls and callbacks, including `Clone`, `Drop`,
  formatting, hashing and comparison. Crate-owned implementations must follow
  this policy. Caller-supplied code cannot be globally certified: the generic
  `T` on `OrderBook<T>`, `TradeListener`, `OrderStateListener`,
  `PriceLevelChangedListener`, custom event serializers and journal
  implementations. Document their obligation not to panic, audit where they
  execute relative to locks and mutations (never inside a lock or mid-mutation
  when avoidable), and state the boundary's limits instead of promising to
  prevent every external panic.
- Return the API's typed error (`OrderBookError`, `ManagerError`,
  `JournalError`, `ReplayError`, `SerializationError`, ...) or its documented
  typed failure outcome. Preserve book, price-level cache, sequencer and
  journal invariants on failure: validate and reserve before committing
  mutations, or provide a consistent rollback. Do not hide invariant failures
  by silently defaulting, dropping state or returning success.
- Errors returned by `pricelevel` are part of this contract: map every
  `PriceLevelError` and every failure slot of a `pricelevel` result (for
  example `MatchResult`'s attached error) to a typed `OrderBookError`; never
  unwrap or ignore them.
- Never evade this policy with `unsafe` (already denied), `catch_unwind`,
  `panic = "abort"`, deliberate process termination (`std::process::exit` /
  `abort` in library code), or disabled overflow checks. An allocator's
  unrecoverable OOM abort is distinct from a Rust panic; this policy does not
  promise recovery from process-wide resource exhaustion. Handle allocation
  failures through fallible APIs where they are available.

---

## Concurrency

The core engine is lock-free and synchronous. The async surface is limited
to `BookManagerTokio` and the NATS publishers.

- Lock-free primitives first: `dashmap::DashMap`,
  `crossbeam-skiplist::SkipMap`, `crossbeam::queue::SegQueue`, atomics
  (`AtomicU64`, `AtomicBool`, `AtomicPtr`). Reach for a mutex only when no
  lock-free data structure fits.
- If a lock is required, prefer `std::sync::RwLock` for read-heavy state.
  Keep the guard short. Never hold a guard across expensive work.
- On the async side: `tokio::sync` primitives only — `RwLock`, `Mutex`,
  `mpsc`, `broadcast`, `watch`, `oneshot`. Never hold a `std::sync::Mutex`
  guard across `.await`.
- Never call blocking I/O (`std::fs`, `std::thread::sleep`, CPU-heavy loops)
  from an async context without `tokio::task::spawn_blocking`.
- No `.await` inside the matching hot path. Trade notifications go through
  a `TradeListener` that pushes into a channel or returns quickly.
- No `unsafe` for concurrency (or anywhere — `#![deny(unsafe_code)]` is
  enforced on `lib.rs`).
- Spawned tasks MUST have a clear shutdown path: either a `JoinHandle` that
  is awaited, or a cancellation signal propagated via `watch` /
  `CancellationToken`. No fire-and-forget.
- NATS retry / backoff uses exponential backoff with jitter — hard-coded
  sleeps are banned.

---

## Minimize Copies

- Move ownership when storing.
- References for read-only access.
- `Arc<T>` for shared immutable state (fee schedule, STP config, manager
  book registries). `Arc<OrderBook<T>>` is the expected handle shape inside
  the manager.
- Pre-allocate collections with `Vec::with_capacity` whenever the size is
  known or estimable (match batches, mass-cancel results, NATS publish
  batches).
- `MatchingPool` is the allocation-reuse point for matching — do not
  allocate fresh `Vec`s per match call on the hot path.
- Avoid cloning large snapshots. Prefer enriched snapshots with metric
  flags for read paths.

---

## Code Organization

- One concern per file: `orderbook/matching.rs`, `orderbook/mass_cancel.rs`,
  `orderbook/book.rs`. Preserve the current layout.
- Group by domain concept, not by mechanism: `orderbook/fees.rs` rather than
  `utils/money_helpers.rs`.
- Re-export the user-facing API from `lib.rs` and `prelude.rs`. Keep the
  prelude minimal and stable. Any change to the prelude is a public-API
  change.
- Constants: module-level `const` for book-wide values (default tick size,
  default lot size, snapshot format version), associated `const` for
  type-scoped values. No magic numbers in matching / sequencer logic — name
  them.

---

## Logging & Observability

- Use `tracing` for all logging. Never `println!`, `eprintln!`, `dbg!`, or
  the `log` crate.
- Examples, benches, and `tests/` may initialize `tracing_subscriber::fmt()`
  with an `EnvFilter`; library code must NOT install a global subscriber.
- Structured fields, not string interpolation:
  `tracing::debug!(order_id = %id, side = ?side, price = %price, "submitted")`.
- Annotate expensive or top-level public functions with
  `#[tracing::instrument(skip(self, large_arg))]` — skip large / non-Debug
  arguments.
- Log levels:
  - `ERROR`: unrecoverable failures — journal corruption detected, replay
    divergence, NATS publisher permanent failure.
  - `WARN`: recoverable issues — NATS retry, journal segment rotation fail
    with fallback, replay skipped entry.
  - `INFO`: high-level lifecycle events — book created, manager bound,
    publisher connected, journal opened, replay completed.
  - `DEBUG`: per-operation internals — match steps, cancellations, state
    transitions.
  - `TRACE`: per-price-level traversal, per-frame dispatch — disabled by
    default.
- Do NOT log user-supplied secrets if any appear in extra fields. The
  current crate has none by default, but the generic `T` on `OrderBook<T>`
  can carry arbitrary data — use a redacting `Debug` on `T` if you plan to
  log it.

---

## Documentation

- Every `pub` item: `///` doc comment with a one-line summary, then details.
  For `Result`-returning functions add a `# Errors` section. Production
  functions do not panic (see Production Panic Policy), so there is no
  `# Panics` section; a function that runs caller-supplied code documents
  that code's obligation not to panic instead.
- Include units: "ticks", "price ticks", "basis points", "milliseconds",
  "quantity units".
- Examples should compile. Use `?`, not `.unwrap()`. Gate examples on
  features where applicable (`/// # #[cfg(feature = "journal")]`).
- `README.md` is generated from `src/lib.rs` module-level docs via
  `make readme` (cargo-readme). Update `lib.rs` first, then regenerate.
- `#![warn(missing_docs)]` on `lib.rs` is expected — fix missing-doc
  warnings before merging.
- Any `unsafe` block: forbidden by `#![deny(unsafe_code)]`. If absolutely
  required, raise the exception with the user first.

---

## Testing

- Unit tests in the same file (`#[cfg(test)] mod tests`).
- Cross-module and feature-gated tests under `tests/unit/` — this is the
  canonical integration-test tree wired in `Cargo.toml` (`[[test]] name = "tests"`).
- Every test covers both the happy path and all documented error cases.
- Tests for feature-gated code: `#[cfg(feature = "nats")]`,
  `#[cfg(feature = "journal")]`, `#[cfg(feature = "bincode")]`,
  `#[cfg(feature = "special_orders")]`.
- Name tests `test_<unit>_<scenario>_<expected>`, e.g.,
  `test_submit_limit_order_crosses_book_emits_trade`.
- Panics, assertions, `debug_assert!`, `unwrap` / `expect` and their error
  variants are allowed in tests and their test-only fixtures/helpers,
  including deliberate panic tests. This permission does not extend to
  production functions, including their `cfg(test)` branches, or to helpers
  shared with production. Compiling or calling production code from a test
  does not exempt it from the Production Panic Policy.
- Tests of production failure paths must check typed failures and preserved
  state; an expected production panic is not an acceptable error contract.
- Concurrency tests: seed RNGs deterministically; use
  `std::thread::Barrier` to start workers in lockstep; `sleep` only for
  time-based expirations, never for synchronization.

---

## Matching / Book Discipline

The matching engine IS the product. Wire-level correctness of trade events
and book state matters as much as throughput.

- Every new order type, matching rule, or STP mode ships with unit tests
  covering: empty book, partial fill, full fill, self-trade paths, and the
  cancel-on-STP branch.
- Snapshots MUST round-trip: `OrderBookSnapshot` →
  `restore_from_snapshot_package()` must preserve fee schedule, STP mode,
  tick / lot size, order size limits, and aggregate state. Every new config
  field added to the book adds a corresponding preservation path — this is
  non-negotiable.
- `snapshots_match` is the equality oracle for replay correctness. Do not
  downgrade it to a subset check.
- Trade emission order within a single submit call MUST be deterministic.
  A non-deterministic trade stream breaks replay.
- `TradeResult` carries fees (`maker_fee`, `taker_fee`). Do not omit them
  from NATS payloads or journal entries.

---

## Sequencer / Journal / Replay

- Journal entries are append-only and versioned. Bumping
  `ORDERBOOK_SNAPSHOT_FORMAT_VERSION` forces a migration note in
  `CHANGELOG.md` and a compatibility test covering the previous version.
- CRC32 checksums on journal segments — `FileJournal` verifies before
  replay. Replay failure returns `ReplayError`, never panics.
- `ReplayEngine` rebuilds state deterministically from a journal prefix.
  Non-determinism in the book implementation IS a bug.
- Snapshot-then-journal replay must equal journal-only replay on the same
  prefix. `snapshots_match` is the oracle.
- In-memory journal (`InMemoryJournal`) is the reference for testing and
  benchmarking — keep it in parity with `FileJournal` surface.

---

## Performance Discipline

Hot paths: the matching inner loop, price-level cache lookups, iterator
traversals (`levels_with_cumulative_depth`, `levels_until_depth`), mass
cancel, NATS publish batching.

- Zero heap allocation inside inner matching loops where feasible.
  `MatchingPool` reuses buffers — use it.
- `PriceLevelCache` is the fast path for best bid / ask. Do not traverse
  the skiplist when the cache is valid.
- Prefer monomorphized generics over `dyn Trait` on the hot path. Use enum
  dispatch for bounded sets (order type, side, STP mode).
- Benchmark with Criterion before claiming a speedup. No perf PR without
  numbers. `make bench` is the entry point; compare with
  `make bench-compare`.
- Iterator-based analytics are zero-alloc O(1) memory. Do not regress them
  to `Vec<_>` returns.
- NATS publisher batches and throttles — never publish one event per trade
  on the hot path.

---

## Security / Safety

- `#![deny(unsafe_code)]` on `lib.rs`. Do not introduce `unsafe` without
  explicit user approval.
- If the generic `T` on `OrderBook<T>` carries user-identifying data in
  production, ensure `Debug` does not leak it — use a redacting wrapper.
- No hard-coded credentials anywhere. NATS connection strings go through
  configuration; `.env` files are git-ignored.

---

## Pre-Submission Checklist

All must pass — failing any means not ready:

- `make pre-push` (runs `fix`, `fmt`, `lint-fix`, `test`, `readme`, `doc`)
  OR the explicit four:
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo fmt --all --check`
  - `cargo test --all-features`
  - `cargo build --release` (zero warnings)
- Production Panic Policy reviewed in debug and release configurations and
  for every feature combination touched: no explicit panic/assertion forms or
  panicking extraction/indexing; checked arithmetic, capacities and
  dependency preconditions; lock poisoning, `JoinError` and channel errors
  handled; callback boundaries documented; failure paths preserve state.
  Test-only code may panic. Automated enforcement (clippy restriction lints
  plus a panic-policy scan, as in `pricelevel`) does not prove the crate is
  panic-free; the manual review still applies.
- `#[must_use]` on all pure functions and builders
- `#[inline]` on small hot-path helpers, `#[cold]` on error paths
- No `unsafe` introduced
- Module boundaries respected (core engine does NOT depend on `manager`,
  `nats*`, `sequencer`)
- Tests cover happy path AND all error cases; feature-gated paths have
  feature-gated tests
- Doc comments on all `pub` items; `# Errors` on fallible functions
- `README.md` regenerated via `make readme` if the public surface or
  `lib.rs` module docs changed
- `CHANGELOG.md` entry for every user-visible change
- Perf-relevant change includes Criterion numbers before / after

---

## DO NOT

- Introduce a production panic, assertion or unchecked failure path, or
  bypass the Production Panic Policy. Test-only panic/assertion code is
  permitted.
- Add dependencies without explicit approval.
- Use `anyhow` — all errors must be concrete typed enums.
- Use `println!`, `eprintln!`, `dbg!`, or `log` crate — use `tracing`.
- Skip any pre-submission check.
- Call blocking I/O from async context without `spawn_blocking`.
- Use `saturating_*` or `wrapping_*` on state (sequencer / journal / retry
  counters, quantity and depth aggregates, statistics).
- Install a global `tracing` subscriber from library code.
- Allocate on the matching hot path when `MatchingPool` fits.
- Traverse the skiplist when `PriceLevelCache` would answer the query.
- Drop fee fields from `TradeResult`, NATS payloads, or journal entries.
- Bump `ORDERBOOK_SNAPSHOT_FORMAT_VERSION` without a migration test.
- Break replay determinism.
- Introduce `unsafe` — `#![deny(unsafe_code)]` is on.
