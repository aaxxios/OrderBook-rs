[![Dual License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)
[![Crates.io](https://img.shields.io/crates/v/orderbook-rs.svg)](https://crates.io/crates/orderbook-rs)
[![Downloads](https://img.shields.io/crates/d/orderbook-rs.svg)](https://crates.io/crates/orderbook-rs)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/OrderBook-rs.svg)](https://github.com/joaquinbejar/OrderBook-rs/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/OrderBook-rs.svg)](https://github.com/joaquinbejar/OrderBook-rs/issues)
[![PRs](https://img.shields.io/github/issues-pr/joaquinbejar/OrderBook-rs.svg)](https://github.com/joaquinbejar/OrderBook-rs/pulls)

[![Build Status](https://img.shields.io/github/actions/workflow/status/joaquinbejar/OrderBook-rs/build.yml)](https://github.com/joaquinbejar/OrderBook-rs/actions)
[![Coverage](https://img.shields.io/codecov/c/github/joaquinbejar/OrderBook-rs)](https://codecov.io/gh/joaquinbejar/OrderBook-rs)
[![Dependencies](https://img.shields.io/librariesio/github/joaquinbejar/OrderBook-rs)](https://libraries.io/github/joaquinbejar/OrderBook-rs)
[![Documentation](https://img.shields.io/badge/docs-latest-blue.svg)](https://docs.rs/orderbook-rs)



## High-Performance Lock-Free Order Book Engine

A high-performance, thread-safe limit order book implementation written in Rust. This project provides a comprehensive order matching engine designed for low-latency trading systems, with a focus on concurrent access patterns and lock-free data structures.

### Key Features

- **Lock-Free Architecture**: Built using atomics and lock-free data structures to minimize contention and maximize throughput in high-frequency trading scenarios.

- **Multiple Order Types**: Support for various order types including standard limit orders, iceberg orders, post-only, fill-or-kill, immediate-or-cancel, good-till-date, trailing stop, pegged, market-to-limit, and reserve orders with custom replenishment logic.

- **Thread-Safe Price Levels**: Each price level can be independently and concurrently modified by multiple threads without blocking.

- **Advanced Order Matching**: Efficient matching algorithm for both market and limit orders, correctly handling complex order types and partial fills.

- **Performance Metrics**: Built-in statistics tracking for benchmarking and monitoring system performance.

- **Memory Efficient**: Designed to scale to millions of orders with minimal memory overhead.

### Design Goals

This order book engine is built with the following design principles:

1. **Correctness**: Ensure that all operations maintain the integrity of the order book, even under high concurrency.
2. **Performance**: Optimize for low latency and high throughput in both write-heavy and read-heavy workloads.
3. **Scalability**: Support for millions of orders and thousands of price levels without degradation.
4. **Flexibility**: Easily extendable to support additional order types and matching algorithms.

### Use Cases

- **Trading Systems**: Core component for building trading systems and exchanges
- **Market Simulation**: Tool for back-testing trading strategies with realistic market dynamics
- **Research**: Platform for studying market microstructure and order flow
- **Educational**: Reference implementation for understanding modern exchange architecture

### What's New in Version 0.14.0 (unreleased)

- Dependency floors raised to the latest semver-compatible releases
  (#237). `bincode` stays on 2.0.1.
- The Production Panic Policy is enforced mechanically: a clippy deny set
  plus `scripts/check_panic_policy.py` in `make lint` (#242). See
  `doc/panic-boundaries.md`.
- **pricelevel 0.10 (#239).** Level snapshots, queue views and
  match-result growth are fallible upstream; the book now propagates
  those errors instead of ignoring them. `create_snapshot`,
  `enriched_snapshot`, `enriched_snapshot_with_metrics` and
  `evict_expired_orders` return `Result`.
- **Mass cancels report failures.** `MassCancelResult::failures()` /
  `has_failures()` and the new `MassCancelFailure`: a mass cancel whose
  price level cannot be read cancels nothing and says so.
- **Snapshot format v4.** Level statistics carry a `u128`
  `value_executed`; v2 and v3 packages still restore.
- **Wire break for bincode `TradeResult`.** pricelevel's `MatchResult`
  gained a positional `error` field; JSON payloads and journals stay
  compatible.
- `BincodeEventSerializer` bounds decoding of untrusted payloads (#251):
  a string length prefix is checked against the remaining input before
  anything is allocated (`SerializationError::Truncated`), so allocations
  are bounded by the input length, and payloads over `DEFAULT_MAX_BINCODE_PAYLOAD_BYTES` (8 MiB,
  configurable via `BincodeEventSerializer::with_max_payload_bytes`) are
  rejected with `SerializationError::PayloadTooLarge`.
- Pre-trade risk uses checked notional arithmetic (#243): two orders whose
  notional sum overflows `u128` can no longer wrap the account counter and
  bypass `max_notional_per_account`, and the price band no longer passes
  at extreme prices. Such admissions are now rejected with the existing
  typed risk errors. Release-side underflows are logged and counted in
  `OrderBook::risk_accounting_anomalies`.
- **Implied-volatility inputs are validated (#256).** `SolverConfig::validate`
  and `IVConfig::validate` run at every solve entry point, so an inverted
  or NaN IV bound, a zero tolerance or a bad `price_scale` returns
  `IVError::InvalidConfig` instead of panicking in `f64::clamp`.
  Black-Scholes and the Greeks return `Result<f64, IVError>` and never hand
  back NaN or infinity. `IVError` is `#[non_exhaustive]` and gains
  `InvalidConfig`, `NonFiniteResult`, `ArithmeticOverflow` and
  `PriceLevel`.
- **Aborted sweeps (#240).** A price level that fails mid-sweep stops the
  sweep: the committed prefix is published like a partial fill, the
  remainder never rests, and the submit returns
  `OrderBookError::MatchAborted` (taker state
  `Cancelled { MatchAborted }`). New reject codes `MatchAborted` (15),
  `CapacityExceeded` (16), `CounterExhausted` (17).
- **Journaling aborted submits.** `add_order_with_committed`,
  `submit_market_order_with_committed` and
  `submit_market_order_by_amount_with_committed` return a `SubmitFailure`
  carrying the committed `TradeResult`;
  `SequencerResult::from_submit_failure` records it as the new
  `SequencerResult::MatchAborted`, and replay requires the same prefix
  (`ReplayError::OutcomeMismatch` otherwise).
- **Fill-or-kill preflight.** A FOK checks trade-id headroom and reserves
  its result buffers before any mutation; a shortfall rejects it
  untouched.
- **Dead-book signal.** `OrderBook::match_aborts()`,
  `match_fold_failures()` and the latched `trade_ids_exhausted()` (plus
  `metrics` counters). With an exhausted trade-id generator every
  crossing submit / modify is rejected untouched (code 16); a failed
  post-only probe is also a clean `Rejected`, not an abort.
- **Limitations.** A journal holding a resource-exhaustion abort replays
  at best from genesis, never from a mid-stream snapshot (the trade-id
  generator is not in the snapshot); the committed-prefix check only
  applies to submits recorded through `*_with_committed` /
  `SequencerResult::from_submit_failure`, and aborted updates are
  reconciled by code only. See `doc/panic-boundaries.md`.
- **NATS publishers validate their configuration (#253).** Builder values
  are clamped with a `warn!` instead of panicking later (batch window and
  publish interval at 60 s, batch size to `1..=65_536`, channel capacity
  to Tokio's limit); retries use capped exponential backoff (5 s) with
  jitter; `shutdown()` returns `Result<(), NatsPublisherError>` so a
  panicked or cancelled background task is reported.

- **Checked time helpers (#257).** `try_current_time_millis()` returns
  `Result<u64, TimeError>` for a pre-epoch clock or a `u64` overflow;
  `current_time_millis()` stays infallible with a documented, logged
  fallback instead of a silent `0` / truncating cast.
  `AllocSnapshot::since` (feature `alloc-counters`) returns `Option` and
  rejects out-of-order snapshots instead of clamping.
- **Wire codec is panic-free on untrusted bytes (#254).** Decoders read
  through checked offsets instead of `copy_from_slice` and raw offset
  arithmetic. `encode_exec_report`, `encode_trade_print` and
  `encode_book_update` reserve with `Vec::try_reserve` and return
  `Result<(), WireError>` (new `WireError::CapacityOverflow`); the wire
  format is unchanged.
- **Default trade-id namespace without OS entropy (#265).** Constructors
  that are not given a namespace derive a UUIDv5 from the symbol, process
  id, wall-clock nanoseconds and a process-wide checked counter instead of
  calling the panicking `Uuid::new_v4()`. Namespaces are unique per book
  within a process and are designed to differ across restarts; a restart
  that reuses the same process id with the wall clock stepped back to the
  same nanosecond can repeat one (see `default_trade_id_namespace`), so
  inject a namespace when cross-restart uniqueness must be guaranteed.
  Trade-id format and namespace injection for replay are unchanged.


## 🛠 Makefile Commands

This project includes a `Makefile` with common tasks to simplify development. Here's a list of useful commands:

### 🔧 Build & Run

```sh
make build         # Compile the project
make release       # Build in release mode
make run           # Run the main binary
```

### 🧪 Test & Quality

```sh
make test          # Run all tests
make fmt           # Format code
make fmt-check     # Check formatting without applying
make lint          # Run clippy with warnings as errors
make lint-fix      # Auto-fix lint issues
make fix           # Auto-fix Rust compiler suggestions
make check         # Run fmt-check + lint + test
```

### 📦 Packaging & Docs

```sh
make doc           # Check for missing docs via clippy
make doc-open      # Build and open Rust documentation
make create-doc    # Generate internal docs
make readme        # Regenerate README using cargo-readme
make publish       # Prepare and publish crate to crates.io
```

### 📈 Coverage & Benchmarks

```sh
make coverage            # Generate code coverage report (XML)
make coverage-html       # Generate HTML coverage report
make open-coverage       # Open HTML report
make bench               # Run benchmarks using Criterion
make bench-show          # Open benchmark report
make bench-save          # Save benchmark history snapshot
make bench-compare       # Compare benchmark runs
make bench-json          # Output benchmarks in JSON
make bench-clean         # Remove benchmark data
```

### 🧪 Git & Workflow Helpers

```sh
make git-log             # Show commits on current branch vs main
make check-spanish       # Check for Spanish words in code
make zip                 # Create zip without target/ and temp files
make tree                # Visualize project tree (excludes common clutter)
```

### 🤖 GitHub Actions (via act)

```sh
make workflow-build      # Simulate build workflow
make workflow-lint       # Simulate lint workflow
make workflow-test       # Simulate test workflow
make workflow-coverage   # Simulate coverage workflow
make workflow            # Run all workflows
```

ℹ️ Requires act for local workflow simulation and cargo-tarpaulin for coverage.

## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Make your changes and ensure that the project still builds and all tests pass.
4. Commit your changes and push your branch to your forked repository.
5. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project
maintainer:

### **Contact Information**
- **Author**: Joaquín Béjar García
- **Email**: jb@taunais.com
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/OrderBook-rs>
- **Documentation**: <https://docs.rs/orderbook-rs>


We appreciate your interest and look forward to your contributions!

**License**: MIT

<!-- related-projects:start -->
## Related projects

Repositories by the same author that this project depends on, and repositories that depend on it.

### Depends on

| Repository | Description |
|------------|-------------|
| [PriceLevel](https://github.com/joaquinbejar/PriceLevel) · [crates.io](https://crates.io/crates/pricelevel) | Lock-free price level implementation for limit order books. |

### Used by

| Repository | Description |
|------------|-------------|
| [hydra-amm](https://github.com/joaquinbejar/hydra-amm) · [crates.io](https://crates.io/crates/hydra-amm) | Universal AMM engine: build, configure and operate any Automated Market Maker through one interface. |
| [market-maker-rs](https://github.com/joaquinbejar/market-maker-rs) | Quantitative market making strategies, starting with the Avellaneda-Stoikov model. |
| [Option-Chain-OrderBook](https://github.com/joaquinbejar/Option-Chain-OrderBook) · [crates.io](https://crates.io/crates/option-chain-orderbook) | Option chain order book system (underlying, expiration, strike) built on OrderBook-rs, PriceLevel and OptionStratLib. |
| [Option-Chain-OrderBook-Backend](https://github.com/joaquinbejar/Option-Chain-OrderBook-Backend) | REST and WebSocket backend service exposing Option-Chain-OrderBook. |

<!-- related-projects:end -->
