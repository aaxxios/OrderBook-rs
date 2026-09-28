# Panic boundaries

Issue #242. Companion to the Production Panic Policy in
`rules/global_rules.md`. Modelled on PriceLevel's `doc/panic-boundaries.md`
(that crate's issue #172/#173), adapted to this crate's dependency set and
public surface.

**Status: skeleton.** This document enumerates what the Production Panic
Policy gate (`[lints.clippy]` in `Cargo.toml`, `scripts/check_panic_policy.py`)
cannot see — irreducible dependency panic surface, the one documented
`unsafe` exception, and caller-supplied code obligations — and records the
current ratchet. Sections marked **to be completed by #260** need the
per-call-site inventory PriceLevel's document has (guard held / partial
mutation / unwind effect per call site); that audit is out of scope for
issue #242, which lands the mechanical gate.

## Contract

The Production Panic Policy in `rules/global_rules.md` requires that
crate-owned code not initiate panics. That is the required policy, not a
completed state: `scripts/panic_policy_allowlist.txt` and each production
file's own `// panic-policy-ratchet: see #242, removed by the fix issue`
marker enumerate exactly what remains (`python3
scripts/check_panic_policy.py --ratchet-report` lists the clippy-side half;
the allowlist file is the script-side half). Removing them is issues
#243-#257, not this one.

This document covers what is out of scope for that ratchet entirely: panic
surface the crate does not own (dependency internals, `unsafe`) and panic
surface the crate cannot certify (caller-supplied generic code).

- **What the library guarantees, once the ratchet above is empty.** It never
  panics from crate-owned code on invalid input, a failed invariant, a
  dependency error, or an exceptional branch, in debug or release, for every
  feature combination. It installs no panic hook, uses no `catch_unwind`,
  and never aborts on purpose (`std::process::exit` / `abort` are denied,
  see `scripts/check_panic_policy.py`).
- **What it does not guarantee.** It does not recover from a caller panic
  (in the generic `T` on `OrderBook<T>`, or a listener/serializer/journal/
  clock implementation supplied by the caller) and does not certify
  third-party code. An allocator's OOM abort is a process-wide failure and
  is not reported as a typed error (`rules/global_rules.md`'s Production
  Panic Policy is explicit about this).

## Irreducible dependency panic surface

Code this crate calls but does not own. Each row is either "no known panic
path reachable with valid crate-internal usage" (the crate's own
responsibility is to keep using it that way) or a specific documented
exception.

| Dependency | Surface used | Panic surface | Notes |
|---|---|---|---|
| `dashmap::DashMap` | Order index, symbol registries (`manager.rs`) | Internal `RandomState` hasher panics are not part of its public contract; growth (`RawTable` resize) aborts the process on allocator OOM, not a Rust panic | No known panic path from crate-internal usage (keys are `Id`/`String`, never attacker-controlled hash-flooding input in the trusted-input model this crate assumes) |
| `crossbeam-skiplist::SkipMap` | Price-level index (`PriceLevelCache`, book side maps) | Node allocation aborts the process on allocator OOM, not a Rust panic | Same allocator-OOM caveat as `DashMap` |
| `crossbeam::queue::SegQueue` | (if used on a hot path) | Allocator OOM only | — |
| `std::collections::hash_map::RandomState` | Default hasher for the above | Does not panic in normal operation | — |
| `tokio` (`BookManagerTokio`, NATS publishers) | `tokio::sync::{RwLock, Mutex, mpsc, broadcast, watch, oneshot}`, `tokio::spawn`, `tokio::time` | `tokio::spawn` / `Handle::current()` panic when called outside a runtime; `broadcast::Receiver::recv` can return `Lagged`; a `JoinHandle` can return `Err(JoinError)` for a cancelled or panicked task | `rules/global_rules.md`'s Production Panic Policy requires `Handle::try_current()` over `Handle::current()` and explicit handling of `JoinError` / lagged receivers — tracked per call site by the ratchet where not yet done |
| `async-nats` (`nats` feature) | JetStream publish, connection management | Network/protocol errors are typed (`async_nats::Error`); no known panic path in the publish path this crate calls | — |
| `serde_json` | JSON encoding (`EventSerializer` default, journal fallback) | No panic on well-formed input via the `Result`-returning API this crate uses (`to_string`/`from_str`, never the panicking `Value` index operators) | Crate code must never use `serde_json::Value`'s `Index`/`IndexMut` (panics on a missing key) — reviewed at each call site |
| `bincode` (`bincode` feature) | Zero-copy event serialization | No panic on the `Result`-returning encode/decode API this crate uses | — |
| Global allocator | Every collection growth | OOM aborts the process; this is not a Rust panic and is explicitly out of scope (`rules/global_rules.md`) | `try_reserve`/`try_reserve_exact` convert a growth failure into a typed error where the allocation is caller-sized (e.g. a decoded journal/wire length prefix); ordinary amortized `Vec`/`HashMap` growth is not wrapped, matching the OOM-is-not-a-panic carve-out |

**To be completed by #260:** a per-call-site table (guard held / partial
mutation / unwind effect) for the `DashMap`/`SkipMap` write paths in
`book.rs`, `cache.rs` and `manager.rs`, matching PriceLevel's inventory
format.

## The one documented `unsafe` exception

`#![deny(unsafe_code)]` is on `lib.rs`. The single exception:

- **`memmap2`, feature `journal`, `src/orderbook/sequencer/file_journal.rs`.**
  `FileJournal` memory-maps append-only segment files
  (`memmap2::MmapMut`/`Mmap`). The `unsafe` is confined to `memmap2`'s own
  `map`/`map_mut` constructors (memory-mapping a file is inherently unsafe:
  the kernel can deliver `SIGBUS` on a truncated/concurrently-modified
  backing file, which `memmap2` cannot prevent). This crate does not add its
  own `unsafe` on top; it mitigates by pre-allocating (not truncating)
  segment files and by not sharing the mapped file with another writer.
  `SIGBUS` from a corrupted external truncation is a process-level fault,
  not a Rust panic, and is explicitly out of the Production Panic Policy's
  scope (irreducible OS-level risk of memory-mapped I/O). CRC32 checksums
  detect a truncated/corrupted segment on the read/replay path and return
  `ReplayError`, never panic, for anything short of the kernel-level fault
  above.

**To be completed by #260:** confirm there is no second `unsafe` introduced
by a dependency's own `build.rs`/proc-macro that this crate re-exercises
(informational only — not a crate-owned `unsafe` block either way).

## Caller-supplied code obligations

`rules/global_rules.md`'s Production Panic Policy: "Caller-supplied code
cannot be globally certified... Document their obligation not to panic,
audit where they execute relative to locks and mutations..., and state the
boundary's limits instead of promising to prevent every external panic."

| Caller-supplied surface | Where it runs | Obligation | Notes |
|---|---|---|---|
| Generic `T` on `OrderBook<T>` (`Clone`, `Default`, `Debug`, and any trait bound the caller's `T` carries) | Order storage, snapshot/clone paths, `Debug` formatting | Must not panic; `Debug` must not leak caller-identifying data if `T` carries user-identifying fields (`rules/global_rules.md`'s Security/Safety section) | No engine lock is held across a `T::clone()`/`T::fmt()` call on the matching hot path (matching operates on `pricelevel`'s `OrderType<()>` internally; `T` is only touched at the book's own boundary, not inside `pricelevel`'s matcher) |
| `TradeListener` (`Arc<dyn Fn(&TradeResult) + Send + Sync>`, `src/orderbook/trade.rs`) | Invoked synchronously from the matching path after a trade is committed | Must not panic; must return quickly (no blocking I/O) — push into a channel, do not do work inline | Invoked after book/queue mutation for that trade is committed, never mid-mutation; an unwind here does not corrupt book state but does abort the remainder of that `submit`/`match` call's listener fan-out |
| `OrderStateListener` (`Arc<dyn Fn(Id, &OrderStatus, &OrderStatus) + Send + Sync>`, `src/orderbook/order_state.rs`) | Invoked on order lifecycle transitions | Same as `TradeListener` | Same commit-then-notify discipline |
| `PriceLevelChangedListener` (`Arc<dyn Fn(PriceLevelChangedEvent) + Send + Sync>`, `src/orderbook/book_change_event.rs`) | Invoked on book-level change events (feeds `NatsBookChangePublisher`) | Same as `TradeListener` | Same commit-then-notify discipline |
| `EventSerializer` impls (`src/orderbook/serialization.rs`) | Journal entry encoding, NATS payload encoding | Must return a typed error rather than panicking on an unencodable value | Crate-provided JSON/Bincode impls follow this; a caller-supplied impl is not re-certified |
| `Journal<T>` impls (`src/orderbook/sequencer/journal.rs`) | Append/read of sequencer events (`InMemoryJournal`, `FileJournal`, or a caller's own impl) | Must return `JournalError`/`ReplayError` rather than panicking; must not silently drop or reorder entries | Crate-provided impls follow this end-to-end; a caller-supplied `Journal<T>` is not re-certified |
| `Clock` impls (`src/orderbook/clock.rs`) | Timestamp generation for the book and sequencer | Must not panic; must be monotonic if used with `ReplayEngine`'s determinism guarantee | `MonotonicClock` is crate-provided and compliant; a caller-supplied `Clock` breaking monotonicity is a correctness bug in the caller, not a crate panic |
| Replay progress callbacks (`replay_from_with_progress`, `replay_from_with_clock_and_progress`, `src/orderbook/sequencer/replay.rs`) | Invoked per applied journal entry during replay | Must not panic; must return quickly | Runs after each entry is applied to the in-memory book, not while any lock is held |
| `metrics` recorder (feature `metrics`, `src/orderbook/metrics.rs`) | The process-installed global `metrics` recorder | Must not panic on a recorded metric | The crate never installs its own recorder (`rules/global_rules.md`'s Logging & Observability rule against installing a global subscriber applies by the same reasoning to a metrics recorder) |
| `tracing` subscriber | Every `tracing::{trace,debug,info,warn,error}!` call site | Must not panic | The crate never installs its own subscriber |

**To be completed by #260:** the per-call-site guard/partial-mutation/unwind
table PriceLevel's document has for each row above (which lock, if any, is
held across the call; what state is already committed if the callback
unwinds).

## Matching sweep failures (#240)

pricelevel 0.10 reports a failure inside `PriceLevel::match_order` through
`MatchResult::error()` while keeping the prefix the level committed. The
book stops the sweep at that level, never trades at a worse price, publishes
the committed prefix exactly like a partial fill (trade listener, price-level
listener, risk `on_fill`, maker states, location cleanup), never rests the
remainder, and returns `OrderBookError::MatchAborted` (taker state
`Cancelled { MatchAborted }`).

Before each level is matched, the aggregate result and the pooled
filled-maker buffer are reserved for that level's worst case,
`min(resting makers, quantity cap)`; a refused reservation aborts the sweep
**before** the level is touched, with the prefix of the earlier levels.
Fill-or-kill takers additionally reserve for their whole predicted sweep and
check trade-id headroom before any mutation.

Residuals, stated precisely:

- **Fold beyond the reserved bound.** A replenishing iceberg / reserve maker
  can trade again at the same level, and a maker admitted to the level by a
  concurrent submit on the shared submit gate while it is being swept can be
  consumed too. Those trades exceed `min(resting makers, quantity cap)`, so
  their slots grow during the fold. Only if the allocator refuses that growth
  are the level's committed trades left out of the aggregate `MatchResult`
  (and so out of the `TradeResult` / journal), while the level, the makers'
  risk counters and order states reflect them. The sweep then aborts and the
  gap is logged at `ERROR` ("committed trades of a price level could not be
  folded into the taker's result").
- **Fill-or-kill preflight.** The preflight covers what the book can
  observe: the trade-id generator's `remaining()` against a conservative
  upper bound on the sweep's trades, and the result buffers. pricelevel's
  internal counters (queue insertion sequence, topology / mutation epochs,
  statistics sequence) have no public headroom query, and replenishment
  trades beyond the reserved maker steps grow the buffers. Either can still
  stop a FOK mid-sweep; it then follows the abort rules above (a partial
  fill reported as `MatchAborted`, never rested). The id bound is
  conservative, so a FOK within that many ids of the sequence's exhaustion
  can be refused although it would fit.
- **Poisoned levels.** A pricelevel level poisoned by an earlier panic
  refuses to match by returning an empty result **without** an error, so the
  book cannot tell it from an empty level and the sweep walks on to the next
  price. This is an upstream limitation, tracked in pricelevel.
- **Replay of recorded aborts.** Replay re-executes a journaled
  `SequencerResult::MatchAborted` and requires the same committed prefix.
  An abort caused by trade-id generator exhaustion or by an allocator
  refusal depends on state the journal does not carry (the generator's
  counter is not part of the snapshot package or `ReplayBookConfig`, and
  allocation outcomes are not deterministic), so it usually does not
  reproduce on a fresh replay book: the replayed submit fills further and
  replay stops with `ReplayError::OutcomeMismatch`. That is by design —
  loud, never a silent divergence. Such a journal is replayable at best
  from genesis onto a book whose generator state matches; it is never
  replayable from a mid-stream snapshot, because the snapshot package does
  not carry the trade-id generator.
- **Prefix reconciliation coverage.** Replay compares the committed prefix
  only for submits the sequencer recorded through the `*_with_committed`
  entry points and `SequencerResult::from_submit_failure`. An abort recorded
  through `From<&OrderBookError>` (`RejectedWithCode`, code 15), and every
  aborted `UpdateOrder`, is reconciled by reject code only.
- **Cross-stream disagreement.** The fold-beyond-the-bound case above is
  the only path on which the trade stream can disagree with the book, risk
  and order-state streams. It is counted by `OrderBook::match_fold_failures`
  and the `orderbook_match_fold_failures_total` metric.
- **Exhausted trade-id generator.** Once the generator is exhausted the
  book latches `OrderBook::trade_ids_exhausted` (logged once at `ERROR`,
  `orderbook_trade_ids_exhausted_total`) and rejects every crossing submit,
  crossing modify (before the original is cancelled) and publishing
  `match_*` call untouched with `CapacityExceeded`. The raw `match_order*`
  family still aborts with an empty prefix. A limit taker counts as
  crossing only when its price crosses the best opposite price. No kill
  switch is engaged automatically; replace the generator with
  `set_trade_id_namespace`. The check runs under the submit gate the sweep
  holds, before any mutation, and is exact under the exclusive gate or a
  single writer. Under the shared gate it is best-effort: pricelevel 0.10
  exposes no public atomic id reservation, so concurrent takers racing for
  the last ids can all pass it, and the losers abort inside the sweep with
  `MatchAborted` (their prefix published, possibly empty) instead of the
  untouched rejection.

## Ratchet

Three ledgers, all mechanically enforced (`make lint`), all shrink-only:

1. `scripts/check_panic_policy.py --ratchet-report` lists every clippy-side
   `#![allow(clippy::...)] // panic-policy-ratchet: see #242, removed by the
   fix issue` currently in the tree — but a plain per-file `allow` is not
   itself a count-based ratchet: `cargo clippy` cannot tell a violation that
   existed when the marker was written from a brand new one added later in
   the same file, for an already-listed lint. Ledger 3 below closes that.
2. `scripts/panic_policy_allowlist.txt` lists every `assert!`/
   `debug_assert!`-family, `saturating_*`/`wrapping_*`, `catch_unwind`,
   `panic_any` and `resume_unwind` finding `scripts/check_panic_policy.py`'s
   own syntax scan currently tolerates, one `path:rule:count` line per
   file/rule pair (`check_panic_policy.py --write-allowlist` regenerates
   it).
3. `scripts/clippy_ratchet.txt` (PR #266 review) is the count-based
   companion to ledger 1: `scripts/check_clippy_ratchet.py` copies the
   crate to a scratch directory, strips every `panic-policy-ratchet`
   `#![allow(...)]` block from the copy only, and re-runs `cargo clippy`
   there with `RUSTFLAGS=--cap-lints=warn` (so the crate's own
   `[lints.clippy]` `"deny"` entries report instead of aborting the
   scratch build). Each ratcheted file/lint pair's finding count in the
   ledger must match exactly; `check_clippy_ratchet.py
   --write-clippy-ratchet` regenerates it. `make lint-clippy-ratchet` runs
   it standalone; `make lint` runs it last (see the Makefile for the
   measured cost).

All three fail the build if a count grows past its ledger value (a new
violation) **or** falls below it (a stale, over-generous entry) — so a fix
PR is forced to shrink them, never to widen them. A fix PR (issues
#243-#257) that removes a production violation:

1. Deletes or narrows the file's `#![allow(clippy::...)]` ratchet line (or
   removes the whole marker once that file has none left).
2. Regenerates `scripts/panic_policy_allowlist.txt` via `python3
   scripts/check_panic_policy.py --write-allowlist` and
   `scripts/clippy_ratchet.txt` via `python3
   scripts/check_clippy_ratchet.py --write-clippy-ratchet`, and confirms
   each diff only removes/lowers entries for files it touched.

When all three are empty, delete the two ledger files (an absent file is an
empty ledger, matching a from-scratch audit — `--write-allowlist` /
`--write-clippy-ratchet` still regenerate a header-only file, which the
final cleanup PR then removes) and delete this ratchet section.
