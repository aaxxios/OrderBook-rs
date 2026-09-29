# Benchmark Results

orderbook-rs **0.14.0** candidate (`pricelevel` 0.10.1) against the last
release, **v0.13.1** (`pricelevel` 0.9.2), measured for #259 on
2026-09-29. Tail-latency detail per scenario, the allocation profile and
the methodology live in [`BENCH.md`](BENCH.md); the compact raw data
(per-round JSON, both resolved `Cargo.lock` files, the load log, the
summary CSV) lives in [`doc/bench/0.14.0/`](doc/bench/0.14.0/).

## System

| Item | Value |
|---|---|
| Host | Apple M5 Max, 18 cores (logical = physical), 128 GiB RAM |
| OS | macOS 27.0 (Darwin 27.0.0, `arm64`), no CPU pinning |
| Toolchain | `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1` |
| Profile | `--release`, system allocator, `RUSTFLAGS` unset |
| Load average | 5.4 to 7.6 (1 min) across the comparison run; per-run values in `doc/bench/0.14.0/load.csv` |
| Candidate | branch `issue-259-performance` at `7946e00` (main `5447250` + the #259 commits) |
| Baseline | tag `v0.13.1` (`a36218b`) |

A desktop host, not a bench rig: interleaving the two sides round by
round is what keeps background load from favouring either one.

## Methodology (summary)

- **Same bench code on both sides.** `scripts/bench_compare.sh` copies
  `benches/compare/` into two detached worktrees (one per git ref, each
  with its own `CARGO_TARGET_DIR`) and builds it against each checkout;
  `benches/compare/src/adapter.rs` absorbs the one API difference
  (`create_snapshot` returns a `Result` on 0.14.0). Each side's resolved
  `Cargo.lock` is recorded.
- **Interleaved rounds.** Seven rounds, baseline then candidate, each
  running all fourteen scenarios; the load average is logged before and
  after every run.
- **Only the operation is timed.** Seeding, refills and the drop of the
  operation's output happen outside the clock. Ops at or below the
  Apple timer tick (41.67 ns) are timed 32 per `Instant` pair (`batch`);
  the rest one per pair (`single`). Contended scenarios time batches of
  32 adds per thread and merge the per-thread histograms.
- **Verdict.** Per row: median across rounds of each round's p50. A row
  whose round-to-round p50 spread exceeds 10 pp on either side is NOISY
  (inconclusive, never a pass). Otherwise a delta above +3 %
  (uncontended) or +5 % (contended) is a REGRESSION, except a
  single-op-timed row that moved by at most one clock tick. A delta inside
  the threshold is "within threshold", not "no change".

## HEAD vs v0.13.1

p50 in ns (median of seven rounds, spread in parentheses), deltas of the
p50 and p99 medians.

| scenario | class / timer | v0.13.1 p50 | 0.14.0 p50 | p50 Δ | p99 Δ | verdict |
|---|---|---|---|---|---|---|
| `add_only` | uncontended / single | 1 125 (7.4 pp) | 625 (6.7 pp) | -44.4 % | -49.8 % | OK |
| `cancel_only` | uncontended / batch | 20 (15.0 pp) | 28 (3.6 pp) | +40.0 % | -32.1 % | REGRESSION (#249, #294; reported) |
| `aggressive_walk` | uncontended / batch | 2 993 (64.1 pp) | 2 529 (5.1 pp) | -15.5 % | +42.8 % | NOISY (baseline bimodal) |
| `mixed_70_20_10` | uncontended / single | 959 (8.7 pp) | 500 (8.2 pp) | -47.9 % | -47.7 % | OK |
| `thin_book_sweep` | uncontended / batch | 691 (64.0 pp) | 350 (4.6 pp) | -49.3 % | -25.3 % | NOISY (baseline bimodal) |
| `mass_cancel_burst` | uncontended / single | 32 047 (16.6 pp) | 29 375 (12.5 pp) | -8.3 % | -14.8 % | OK, re-measured (1.6 / 1.3 pp, -7.2 %) |
| `stp_cancel_maker` | uncontended / single | 3 333 (36.3 pp) | 2 083 (8.0 pp) | -37.5 % | -22.6 % | NOISY (faster in every round) |
| `snapshot_create_10k` | uncontended / single | 383 487 (4.2 pp) | 406 015 (7.8 pp) | +5.9 % | +8.5 % | REGRESSION (pricelevel 0.10; reported) |
| `snapshot_restore_10k` | uncontended / single | 11 165 695 (4.0 pp) | 3 997 695 (5.9 pp) | -64.2 % | -63.6 % | OK |
| `replay_10k` | uncontended / single | 9 207 807 (4.7 pp) | 4 050 943 (8.5 pp) | -56.0 % | -54.7 % | OK |
| `contended_add_4t` | contended / batch | 2 205 (5.4 pp) | 1 391 (6.0 pp) | -36.9 % | -26.5 % | OK |
| `contended_add_8t` | contended / batch | 3 739 (7.2 pp) | 3 497 (6.5 pp) | -6.5 % | -0.6 % | OK |
| `contended_add_listeners_4t` | contended / batch | 2 031 (20.4 pp) | 1 567 (4.6 pp) | -22.8 % | -8.2 % | NOISY (faster in every round) |
| `contended_add_listeners_8t` | contended / batch | 3 855 (8.8 pp) | 4 847 (11.4 pp) | +25.7 % | +32.0 % | REGRESSION (#247, #249; reported) |

**Totals: 7 OK, 3 REGRESSION, 4 NOISY.**

- **NOISY rows** are the baseline's doing: v0.13.1 untracks every filled
  maker with a scan whose cost depends on the per-process hash seed, so
  its fill-heavy scenarios swing from one process to the next; 0.14.0
  (which fixed that scan) stays within 5 to 8 pp. More rounds cannot
  shrink a per-process spread, so the rows stay inconclusive by policy.
- **The three regressions** are measured conclusively (every 0.14.0
  round is on the wrong side of every v0.13.1 round), bisected, and left
  for the maintainer: `cancel_only` +8 ns per cancel (+3 ns from #249's
  per-call emission scope, +5 ns from #294's unwind-aware gate guard),
  `snapshot_create_10k` +5.9 % (arrives with the pricelevel 0.10
  upgrade, time spent inside `PriceLevel::snapshot`), and same-price,
  same-account adds from 8 threads with listeners +25.7 % (#247 level
  stripes and #249's ordered outbox, accepted in those issues at +4.9 %
  and +6.7 %; measured larger on this host). Evidence and bisection
  points: `BENCH.md` "0.13.1 → 0.14.0 delta".

## Allocations (`alloc_count`, feature `alloc-counters`)

`allocs/op` / `bytes_alloc/op`, three runs per point. "before" is `main`
at `5447250` (every 0.14.0 change except #259), "after" is 0.14.0.

| scenario | before | after |
|---|---|---|
| mixed 70 / 20 / 10 | 16.5 to 32.4 / 9.4 to 9.8 KB | 3.35 / 3.3 KB |
| passive add, one level, with user | 6.29 / 18.3 KB | 3.28 / 0.95 KB |
| passive add, one level, no user | 6.28 / 18.2 KB | 3.28 / 0.94 KB |
| passive add, distinct levels | 8.14 / 18.3 KB | 8.14 / 18.4 KB |
| crossing add, full fill | 18.0 to 128.0 / 1.4 to 4.1 KB | 3.02 / 1.07 KB |
| crossing add, partial fill | 4.00 / 1.2 KB | 4.00 / 1.2 KB |
| crossing add, deep level, large taker | 4.00 / 88.3 KB | 4.00 / 88.3 KB |
| market sweep, one level | 44.0 to 119.0 / 1.2 to 3.0 KB | 2.03 / 0.24 KB |
| market sweep, three levels | 209 to 248 / 6.3 to 7.2 KB | 8.10 / 1.4 KB |

The "before" ranges are the hash-seed swing of the old fill-path scan;
the "after" values are identical across runs. What remains (a new level's
16 KB DashMap shard array, the pre-sized `MatchResult` of a large taker)
is inside `pricelevel`; see `BENCH.md` "0.14.0 allocation profile".

## Criterion suite (0.14.0 only)

`cargo bench --all-features --bench benches` on the 0.14.0 candidate,
same host, load average 6.2 to 7.7 during the run. Criterion's median
point estimate per benchmark (100 samples); these are single-version
numbers for regression tracking, not a comparison (the cross-version
numbers are the table above). Setup and teardown are excluded since #258
(see `BENCH.md` "Timing only the operation under test"). Same list as
CSV: `doc/bench/0.14.0/criterion.csv`.

| benchmark | median |
|---|---|
| `Basic OrderBook Operations/create_order_book` | 3.2301 µs |
| `Basic OrderBook Operations/add_single_order` | 5.4732 µs |
| `OrderBook - Add Orders/add_limit_orders` | 99.871 µs |
| `OrderBook - Add Orders/add_limit_orders_with_listeners` | 97.035 µs |
| `OrderBook - Add Orders/add_iceberg_orders` | 92.059 µs |
| `OrderBook - Add Orders/add_post_only_orders` | 91.066 µs |
| `OrderBook - Add Orders/order_count_scaling/10` | 4.8205 µs |
| `OrderBook - Add Orders/order_count_scaling/100` | 36.988 µs |
| `OrderBook - Add Orders/order_count_scaling/1000` | 373.20 µs |
| `OrderBook - Match Orders/match_market_against_limit` | 3.9096 µs |
| `OrderBook - Match Orders/match_market_against_limit_with_listeners` | 4.0592 µs |
| `OrderBook - Match Orders/match_market_against_iceberg` | 6.6770 µs |
| `OrderBook - Match Orders/match_quantity_scaling/10` | 2.0622 µs |
| `OrderBook - Match Orders/match_quantity_scaling/50` | 4.2892 µs |
| `OrderBook - Match Orders/match_quantity_scaling/100` | 5.8138 µs |
| `OrderBook - Match Orders/match_quantity_scaling/200` | 8.6745 µs |
| `OrderBook - Match Orders/match_quantity_scaling/500` | 18.855 µs |
| `OrderBook - Update Orders/cancel_orders` | 19.584 µs |
| `OrderBook - Update Orders/update_quantities` | 41.529 µs |
| `OrderBook - Update Orders/cancel_order_count_scaling/10` | 1.2236 µs |
| `OrderBook - Update Orders/cancel_order_count_scaling/100` | 12.747 µs |
| `OrderBook - Update Orders/cancel_order_count_scaling/1000` | 157.13 µs |
| `OrderBook - Mixed Operations/realistic_trading_scenario` | 242.97 µs |
| `OrderBook - Mixed Operations/high_frequency_scenario` | 771.38 µs |
| `match_order_deep_book` | 1.1004 µs |
| `OrderBook - Mass Cancel/cancel_all_orders/100` | 150.60 µs |
| `OrderBook - Mass Cancel/cancel_all_orders/1000` | 1.9801 ms |
| `OrderBook - Mass Cancel/cancel_all_orders/10000` | 3.5479 ms |
| `OrderBook - Mass Cancel/cancel_all_orders/50000` | 10.583 ms |
| `OrderBook - Mass Cancel/cancel_orders_by_side/100` | 173.33 µs |
| `OrderBook - Mass Cancel/cancel_orders_by_side/1000` | 1.3524 ms |
| `OrderBook - Mass Cancel/cancel_orders_by_side/10000` | 24.927 ms |
| `OrderBook - Mass Cancel/cancel_orders_by_user/100` | 40.950 µs |
| `OrderBook - Mass Cancel/cancel_orders_by_user/1000` | 809.29 µs |
| `OrderBook - Mass Cancel/cancel_orders_by_user/10000` | 26.881 ms |
| `OrderBook - Snapshot/create_snapshot/100` | 145.13 µs |
| `OrderBook - Snapshot/create_snapshot/1000` | 159.55 µs |
| `OrderBook - Snapshot/create_snapshot/10000` | 364.35 µs |
| `OrderBook - Snapshot/restore_from_snapshot/100` | 249.59 µs |
| `OrderBook - Snapshot/restore_from_snapshot/1000` | 438.47 µs |
| `OrderBook - Snapshot/restore_from_snapshot/10000` | 2.5228 ms |
| `OrderBook - Snapshot/enriched_snapshot_all/100` | 152.78 µs |
| `OrderBook - Snapshot/enriched_snapshot_all/1000` | 164.20 µs |
| `OrderBook - Snapshot/enriched_snapshot_all/10000` | 366.96 µs |
| `OrderBook - Snapshot/enriched_snapshot_mid_price/100` | 143.02 µs |
| `OrderBook - Snapshot/enriched_snapshot_mid_price/1000` | 154.42 µs |
| `OrderBook - Snapshot/enriched_snapshot_mid_price/10000` | 310.38 µs |
| `OrderBook - Snapshot/snapshot_json_roundtrip/100` | 85.438 µs |
| `OrderBook - Snapshot/snapshot_json_roundtrip/1000` | 446.18 µs |
| `OrderBook - Replay/journal_append/100` | 3.0352 µs |
| `OrderBook - Replay/journal_append/1000` | 29.686 µs |
| `OrderBook - Replay/journal_append/10000` | 253.91 µs |
| `OrderBook - Replay/replay_from_journal/100` | 95.529 µs |
| `OrderBook - Replay/replay_from_journal/1000` | 464.24 µs |
| `OrderBook - Replay/replay_from_journal/10000` | 4.5371 ms |
| `OrderBook - Replay/replay_verify/100` | 252.77 µs |
| `OrderBook - Replay/replay_verify/1000` | 689.34 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders/2` | 1.8961 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders_with_listeners/2` | 2.1387 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations/2` | 3.2642 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations_with_listeners/2` | 3.4483 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders/4` | 2.4104 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders_with_listeners/4` | 2.6549 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations/4` | 7.5916 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations_with_listeners/4` | 7.3607 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders/8` | 4.8629 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders_with_listeners/8` | 5.3846 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations/8` | 19.930 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations_with_listeners/8` | 19.814 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders/16` | 10.887 µs |
| `OrderBook - Concurrent Operations/concurrent_add_limit_orders_with_listeners/16` | 11.426 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations/16` | 30.576 µs |
| `OrderBook - Concurrent Operations/concurrent_mixed_operations_with_listeners/16` | 30.527 µs |
| `serialization/json_serialize_trade` | 210.24 ns |
| `serialization/json_serialize_book_change` | 44.928 ns |
| `serialization/json_deserialize_trade` | 326.96 ns |
| `serialization/json_deserialize_book_change` | 72.544 ns |
| `serialization/bincode_serialize_trade` | 82.063 ns |
| `serialization/bincode_serialize_book_change` | 25.399 ns |
| `serialization/bincode_deserialize_trade` | 112.71 ns |
| `serialization/bincode_deserialize_book_change` | 12.693 ns |

`cancel_orders_by_user/10000` and `cancel_orders_by_side/10000` grow
faster than linearly: every cancel removes its id from the owner's
`user_orders` list with an order-preserving `Vec::remove` (#252), which
shifts the rest of that list, so cancelling all of one owner's `n`
orders costs O(n²). Unchanged since v0.13.1 (which used `retain`, also
O(n) per call); a follow-up, not a 0.14.0 regression.

## Reproducing

```sh
make bench-compare-refs ARGS="--baseline v0.13.1 --candidate HEAD --rounds 7"
cargo bench --features alloc-counters --bench alloc_count
make bench-hdr
cargo bench --all-features --bench benches
```

`bench-results/` (raw output of `bench_compare.sh`) is gitignored and
never packaged; `doc/bench/` is outside `Cargo.toml`'s `include` list,
so neither ships in the published crate.
