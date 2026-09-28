# Benchmark Results

Measured on Apple M4 Max (Darwin 25.5.0, `arm64`), Rust 1.96.0 stable,
`--release` profile, orderbook-rs **0.9.0**. Criterion reports the
median of 100 samples; absolute numbers are machine- and run-dependent.
Tail-latency (`p50`/`p99`/`p99.9`) numbers live in `BENCH.md`.

Run benchmarks locally:

```sh
cargo bench --all-features
```

> **Methodology note (issue #258).** The tables below predate a
> #258 audit that found several of these Criterion benches (`Snapshot &
> Restore`, `Journal & Replay`, most of `Order Operations`) were timing
> setup and/or teardown alongside the operation under test — see
> `BENCH.md` § "Timing only the operation under test" for the fix and
> `benches/order_book/*.rs`'s own `# Methodology (issue #258)` doc
> comments for the specifics. The absolute numbers here are stale (they
> predate that fix, several point releases, and the `pricelevel` 0.10
> upgrade) and are kept as a historical illustration of the harness's
> *shape*, not as a current baseline — #259 re-measures the corrected
> harness against `v0.13.1` and publishes fresh numbers.

## Snapshot & Restore

| Operation | 100 orders | 1,000 orders | 10,000 orders |
|---|---|---|---|
| `create_snapshot` | 94 µs | 113 µs | 302 µs |
| `restore_from_snapshot` | 151 µs | 292 µs | 1.62 ms |
| `enriched_snapshot (ALL)` | 92 µs | 113 µs | 318 µs |
| `enriched_snapshot (MID_PRICE)` | 96 µs | 112 µs | 297 µs |
| `snapshot_json_roundtrip` | 190 µs | 1.45 ms | — |

## Journal & Replay

| Operation | 100 events | 1,000 events | 10,000 events |
|---|---|---|---|
| `journal_append` | 2.85 µs | 20.1 µs | 408 µs |
| `replay_from_journal` | 84 µs | 766 µs | 7.77 ms |
| `replay_verify` | 179 µs | 886 µs | — |

## Order Operations

| Operation | Time |
|---|---|
| Add limit order (single, cold book) | ~4.1 µs |
| Add 1,000 limit orders | ~1.55 ms |
| Match market vs. populated book (batch) | ~158 µs |
| Realistic mixed scenario | ~251 µs |
| High-frequency mixed scenario | ~665 µs |
| Match market order (per-fill p50, deep book) | ~42 ns (`BENCH.md` `aggressive_walk`) |

## Serialization

Single-message encode/decode (nanoseconds).

| Format | Serialize Trade | Deserialize Trade | Serialize BookChange | Deserialize BookChange |
|---|---|---|---|---|
| JSON | ~262 ns | ~335 ns | ~55 ns | ~81 ns |
| Bincode | ~122 ns | ~97 ns | ~31 ns | ~10 ns |

## Concurrent Operations

Per-op latency under N concurrent threads (`add_limit` / `mixed`):

| Threads | `concurrent_add_limit` | `concurrent_mixed` |
|---|---|---|
| 2  | ~1.74 µs | ~1.71 µs |
| 4  | ~2.02 µs | ~2.77 µs |
| 8  | ~3.69 µs | ~7.13 µs |
| 16 | ~10.3 µs | ~16.2 µs |

## Observations

- **Snapshot creation scales sub-linearly**: 10,000 orders is only ~3.2x
  slower than 100 orders due to efficient `SkipMap` iteration.
- **Enriched snapshots add negligible overhead**: metric calculation is
  fast compared to snapshot creation itself.
- **Journal append is very fast**: ~2.85 µs for 100 events (~28 ns/event)
  thanks to the in-memory journal implementation.
- **Replay throughput**: ~1.29M events/sec at 10,000 events, suitable for
  fast disaster recovery.
- **Matching is sub-microsecond**: per-fill market-order matching on a deep
  book is ~42 ns at the median (`BENCH.md` `aggressive_walk`) — well within
  HFT latency requirements.
- **Bincode is ~2x faster than JSON** on the wire path and an order of
  magnitude cheaper on small messages (`BookChange` deserialize ~10 ns).
