//! CLI entry point for the A/B comparison harness (issues #258 / #259).
//!
//! Prints one JSON line per scenario to stdout:
//! `{"scenario":"add_only","samples":N,"p50":..,"p99":..,"p999":..,"p9999":..,"min":..,"max":..}`
//! — `scripts/bench_compare.sh` parses this with `python3`'s stdlib
//! `json` module (no new dependency). Diagnostics go to stderr so stdout
//! stays clean JSON-lines.
//!
//! Usage: `compare [--quick] [SCENARIO...]` — with no scenario names,
//! runs all three; `--quick` shrinks every op count for a fast
//! pipeline smoke test (see `scripts/bench_compare.sh --quick`), not a
//! real measurement.

mod adapter;
mod rng;
mod workloads;

use hdrhistogram::Histogram;

struct Sizes {
    add_only_warmup: u64,
    add_only_measured: u64,
    cancel_only_preload: u64,
    walk_resting_per_level: u64,
    walk_num_levels: u64,
    walk_measured: u64,
}

const FULL: Sizes = Sizes {
    add_only_warmup: 200_000,
    add_only_measured: 1_000_000,
    cancel_only_preload: 1_000_000,
    walk_resting_per_level: 100,
    walk_num_levels: 50,
    walk_measured: 100_000,
};

// Tiny op counts — only meant to prove the pipeline runs end to end
// (issue #258 task 6), never to produce a measurement. #259 uses `FULL`.
const QUICK: Sizes = Sizes {
    add_only_warmup: 200,
    add_only_measured: 2_000,
    cancel_only_preload: 2_000,
    walk_resting_per_level: 5,
    walk_num_levels: 5,
    walk_measured: 2_000,
};

fn emit(scenario: &str, hist: &Histogram<u64>) {
    println!(
        "{{\"scenario\":\"{scenario}\",\"samples\":{},\"p50\":{},\"p99\":{},\"p999\":{},\"p9999\":{},\"min\":{},\"max\":{}}}",
        hist.len(),
        hist.value_at_quantile(0.50),
        hist.value_at_quantile(0.99),
        hist.value_at_quantile(0.999),
        hist.value_at_quantile(0.9999),
        hist.min(),
        hist.max(),
    );
}

fn run_one(scenario: &str, sizes: &Sizes) {
    let hist = match scenario {
        "add_only" => workloads::add_only(sizes.add_only_warmup, sizes.add_only_measured),
        "cancel_only" => workloads::cancel_only(sizes.cancel_only_preload),
        "aggressive_walk" => workloads::aggressive_walk(
            sizes.walk_resting_per_level,
            sizes.walk_num_levels,
            sizes.walk_measured,
        ),
        other => {
            eprintln!(
                "unknown scenario {other:?} — expected one of: add_only, cancel_only, aggressive_walk"
            );
            std::process::exit(2);
        }
    };
    emit(scenario, &hist);
}

fn main() {
    let mut quick = false;
    let mut scenarios: Vec<String> = Vec::new();
    for arg in std::env::args().skip(1) {
        if arg == "--quick" {
            quick = true;
        } else {
            scenarios.push(arg);
        }
    }
    if scenarios.is_empty() {
        scenarios = vec![
            "add_only".to_string(),
            "cancel_only".to_string(),
            "aggressive_walk".to_string(),
        ];
    }

    let sizes = if quick { &QUICK } else { &FULL };
    for scenario in &scenarios {
        run_one(scenario, sizes);
    }
}
