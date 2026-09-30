# Architecture

This document is the canonical description of how `orderbook-rs` is put
together: its modules, the layering between them, the data flow of a
submit, the feature-flag matrix and the decisions behind them. Decisions
with their own trade-offs are recorded as ADRs under [`adr/`](adr/).
Binding coding rules live in `rules/global_rules.md`; panic and lock
boundaries in [`panic-boundaries.md`](panic-boundaries.md); the matching
algorithm in [`matching.md`](matching.md).

## Layers

```
            prelude.rs / lib.rs          (public surface: re-exports only)
                      |
     +----------------+-----------------+----------------+
     |                |                 |                |
 manager.rs       nats*.rs         sequencer/          wire/
 (std + tokio)    (`nats`)         (journal, replay)   (`wire` codec)
     |                |                 |
     +----------------+--------+--------+
                               |
                 core engine: book.rs + matching, operations,
                 modifications, mass_cancel, stp, fees, risk,
                 stop_orders, stop_protection, repricing, cache,
                 pool, iterators, snapshot, statistics,
                 market_impact, trade, book_change_event,
                 order_state, emission, clock, reject_reason
                               |
                 error.rs (leaf)        pricelevel (per-level engine)
```

Rules that keep the layering intact:

- The core engine never depends on `manager`, `nats*` or `sequencer/`.
- `sequencer/` depends on `book.rs` (replay) and `serialization`
  (journal encoding); never on `nats*` or `manager`.
- `nats*.rs` and `manager.rs` sit on top of the core engine.
- `error.rs` is a leaf: only `std`, `thiserror` and upstream error types.
- Nothing under `src/` imports from `prelude.rs`.
- `#![deny(unsafe_code)]` on `lib.rs`; the only exceptions are the
  `memmap2` calls in `sequencer/file_journal.rs` (`journal`) and
  `utils/counting_allocator.rs` (`alloc-counters`), both listed in
  `panic-boundaries.md`.

## Core engine

`OrderBook<T>` (`book.rs`) owns the two sides as
`crossbeam_skiplist::SkipMap<u128, Arc<PriceLevel>>`, the order-location
index as a `DashMap`, and the book's configuration (tick / lot size, order
size limits, `STPMode`, `FeeSchedule`, `RiskConfig`, kill switch,
`StopProtection`). Per-level queues, order types and the per-level
matcher come from the `pricelevel` crate. `T` is the caller's extra order
data; the engine never hard-codes `()`.

Calls that can trade take the book's submit gate (shared, or exclusive
while STP or pending stops require serialization); see
`panic-boundaries.md` for the lock inventory.

### Submit data flow

1. Validation: tick / lot / size limits, kill switch, pre-trade risk
   (`risk.rs`), trade-id and fee / notional arithmetic preflights.
2. Matching (`matching.rs`): the taker sweeps the opposite side in
   price-time priority, applying STP (`stp.rs`) per level and fees
   (`fees.rs`) per trade, into a `MatchResult`.
3. Publication: the `TradeResult`, level-change events and order-state
   transitions go to the call's emission scope (`emission.rs`), stamped
   with a strictly increasing `engine_seq`, and are delivered to
   listeners after the gate is released.
4. Resting: an unfilled limit remainder is rested on its level; IOC /
   FOK / market remainders are cancelled.
5. Stop evaluation (`special_orders`): while a trailing stop is pending,
   the prints of the call trail and elect stops (`stop_orders.rs`); each
   elected stop runs its child order inside the same call, see below.

### Off-book trailing stops and the protection collar

Pending trailing stops (`special_orders`) live in `PendingStops`, never on
a level. An elected stop leaves the store and executes an
immediate-or-cancel child order with `origin_stop_id` on its trades. The
child's shape is set per book by `StopProtection` (`stop_protection.rs`,
#302, [ADR 0001](adr/0001-stop-protection-collar.md)):

| Book configuration | Child order |
|---|---|
| `stop_protection() == None` (default) | unpriced IOC market order (0.14 behaviour) |
| `Some(collar)` | IOC limit at `stop - collar` (sell) or `stop + collar` (buy), `stop` being the trailed stop price at election |

Either way the child never rests: unlike CME protection points, a
collared remainder is cancelled (`Cancelled { StopProtectionBand }` when
the collar cut it), so a stop whose band is exhausted leaves its position
unprotected. `Triggered { limit_price }` records the child's limit. The `StopProtection` type is compiled in
every build (with and without `special_orders`) so snapshot packages and
`ReplayBookConfig` have one shape everywhere; it only acts where trailing
stops exist, and only the election path reads it.

## Persistence and replay

- **Snapshot package** (`snapshot.rs`): the levels, pending stops and
  last trade price (checksummed payload) plus the book configuration
  (not checksummed). `restore_from_snapshot_package` validates everything
  before touching the live book. Format history:

  | Version | Written by | Change |
  |---|---|---|
  | 1 | earlier releases | no `engine_seq`; rejected on read |
  | 2 | 0.11 (pricelevel 0.8) | `engine_seq` |
  | 3 | 0.12, 0.13 | pricelevel 0.9 `stats_degraded` |
  | 4 | 0.14.0 development builds | pricelevel 0.10 `u128` `value_executed` |
  | 5 | 0.14.0 | pending trailing stops and last trade price, covered by the checksum (#286) |
  | 6 | 0.15.0 | `stop_protection` config field; payload and checksum unchanged from 5 (#302) |

  Reads accept `2..=6`. A package below 5 carrying stops, or below 6
  carrying a collar, is rejected. Version bumps for config-only fields
  exist so that an older reader refuses a package whose meaning it would
  silently change (a 0.14 reader would otherwise restore stops without
  their collar).
- **Sequencer** (`sequencer/`): commands are journaled (`Journal`,
  `InMemoryJournal`, `FileJournal` behind `journal`) and replayed by
  `ReplayEngine` into a fresh book configured by `ReplayBookConfig`.
  Book configuration is not journaled: `ReplayBookConfig` must carry the
  source book's fees, STP mode, shape rules, trade-id namespace and stop
  protection collar (constant over the replayed range) for the replay to
  reproduce its trades. A collar mismatch is not an `OutcomeMismatch`,
  and `snapshots_match` catches it only when it changed an outcome (the
  snapshot does not carry the collar); verify the configuration by
  comparing `stop_protection()` of the replayed and source books.
  `snapshots_match` is the equality oracle.

## Feature matrix

| Feature | Enables | Extra dependencies |
|---|---|---|
| (default) | core engine, sequencer types, `InMemoryJournal`, replay, JSON serialization, `StopProtection` config | none |
| `special_orders` | pegged orders and off-book trailing stops (repricing, election, collar enforcement) | none |
| `nats` | NATS JetStream trade and book-change publishers | `async-nats`, `bytes` |
| `bincode` | `BincodeEventSerializer` | `bincode` |
| `journal` | memory-mapped `FileJournal` | `crc32fast`, `memmap2` |
| `alloc-counters` | `CountingAllocator` | none |
| `metrics` | `metrics` facade instrumentation | `metrics` |
| `wire` | binary wire codec under `src/wire/` | `zerocopy` |

`tokio` is a mandatory dependency, used only by `BookManagerTokio` and the
NATS publishers; the matching path is synchronous and never awaits.

## Key decisions

- Lock-free structures first (`SkipMap`, `DashMap`, atomics); a std lock
  only where contention is rare and the shape does not fit.
- `thiserror` errors per module, aggregated in `OrderBookError`; every
  error maps to a stable `RejectReason` wire code (configuration errors
  map to `Other(0)`).
- Listener delivery is deferred through the emission outbox so listeners
  never run under the submit gate (#249).
- Protocol adapters (FIX, SBE, ...) live in separate bridge crates, not in
  this crate.
- ADRs: [0001 stop protection collar](adr/0001-stop-protection-collar.md).
