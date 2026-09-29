#!/usr/bin/env bash
# scripts/bench_compare.sh — A/B latency comparison harness (issues
# #258 / #259).
#
# Builds `benches/compare/` (a small, standalone bench crate — see its
# own doc comment) against two git refs in two separate, detached
# worktrees, each with its own `CARGO_TARGET_DIR`, then runs both
# binaries for `--rounds` interleaved rounds (A, B, A, B, ...) and
# writes raw per-round JSON plus a summary table into a fresh
# `bench-results/<timestamp>/` directory at the repo root (kept OUTSIDE
# `benches/` on purpose — `Cargo.toml`'s `include` list ships
# `benches/**/*` in the published crate; comparison results never
# should be).
#
# Usage:
#   scripts/bench_compare.sh [options]
#
# Options:
#   --baseline REF     Git ref for the baseline side (default: v0.13.1)
#   --candidate REF    Git ref for the candidate side (default: HEAD)
#   --rounds N         Interleaved rounds, >= 3 recommended (default: 3)
#   --scenarios LIST   Comma-separated scenario names (default: every
#                      scenario `benches/compare` knows; see its
#                      `src/main.rs` SCENARIOS table)
#   --max-load X       Before each side of each round, wait (up to 10
#                      min) until the 1-minute load average is below X
#                      (default: 0 = do not wait). The load average
#                      before and after every run is recorded either way
#   --quick            Smoke-test mode: tiny op counts, forces
#                      --rounds 1 unless --rounds is also given. Proves
#                      the pipeline runs end to end; never a
#                      measurement — see BENCH.md "Methodology".
#   --out-dir DIR      Results directory (default:
#                      bench-results/<UTC timestamp> at repo root)
#   --keep-worktrees   Skip cleanup of the scratch git worktrees
#   -h, --help         This message
#
# Requires: git, cargo, python3 (stdlib only — no new dependency).
set -euo pipefail

BASELINE_REF="v0.13.1"
CANDIDATE_REF="HEAD"
ROUNDS=3
ROUNDS_SET=0
SCENARIOS=""
MAX_LOAD=0
QUICK=0
OUT_DIR=""
KEEP_WORKTREES=0

usage() {
    sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --baseline) BASELINE_REF="$2"; shift 2 ;;
        --candidate) CANDIDATE_REF="$2"; shift 2 ;;
        --rounds) ROUNDS="$2"; ROUNDS_SET=1; shift 2 ;;
        --scenarios) SCENARIOS="$2"; shift 2 ;;
        --max-load) MAX_LOAD="$2"; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --keep-worktrees) KEEP_WORKTREES=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$QUICK" -eq 1 ] && [ "$ROUNDS_SET" -eq 0 ]; then
    ROUNDS=1
fi

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$REPO_ROOT/bench-results/$TIMESTAMP"
fi
mkdir -p "$OUT_DIR"
echo "results dir: $OUT_DIR" >&2

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/orderbook-bench-compare.XXXXXX")"
echo "scratch dir: $SCRATCH" >&2

cleanup() {
    if [ "$KEEP_WORKTREES" -eq 1 ]; then
        echo "keeping worktrees at $SCRATCH (--keep-worktrees)" >&2
        return
    fi
    for side in baseline candidate; do
        if [ -d "$SCRATCH/$side" ]; then
            git -C "$REPO_ROOT" worktree remove --force "$SCRATCH/$side" >/dev/null 2>&1 || true
        fi
    done
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# ─── 1. Two detached worktrees, one per side ────────────────────────
git worktree add --detach --quiet "$SCRATCH/baseline" "$BASELINE_REF"
git worktree add --detach --quiet "$SCRATCH/candidate" "$CANDIDATE_REF"

BASELINE_SHA="$(git -C "$SCRATCH/baseline" rev-parse HEAD)"
CANDIDATE_SHA="$(git -C "$SCRATCH/candidate" rev-parse HEAD)"
echo "baseline:  $BASELINE_REF -> $BASELINE_SHA" >&2
echo "candidate: $CANDIDATE_REF -> $CANDIDATE_SHA" >&2

# ─── 2. Copy the CURRENT benches/compare/ into both sides ───────────
# so byte-identical bench code runs against each checkout's own
# `orderbook-rs` (the crate the compare bin path-depends on is
# `../..` relative to `benches/compare`, i.e. that worktree's checkout
# — see benches/compare/Cargo.toml).
for side in baseline candidate; do
    rm -rf "$SCRATCH/$side/benches/compare"
    mkdir -p "$SCRATCH/$side/benches"
    cp -R "$REPO_ROOT/benches/compare" "$SCRATCH/$side/benches/compare"
done

# ─── 3. Build each side, separate CARGO_TARGET_DIR per side ─────────
#
# The build is explicitly checked and `exit 1`'d on failure rather than
# left to `set -e`: this function's result is captured via
# `VAR="$(build_side ...)"` below, and by default bash does NOT
# propagate `errexit` into a command substitution's subshell unless
# `shopt -s inherit_errexit` is set (bash >= 4.4) — relying on it here
# silently continued past a failed build in testing (the build's own
# stderr was visible, but the script carried on to build the other side
# and then tried to run a binary that was never produced). `exit 1`
# inside a function always terminates the whole process, independent of
# that quirk.
build_side() {
    local side="$1" feature="$2"
    local dir="$SCRATCH/$side/benches/compare"
    local target_dir="$SCRATCH/$side/target-compare"
    echo "building $side (--features $feature)..." >&2
    if ! (
        cd "$dir"
        CARGO_TARGET_DIR="$target_dir" cargo build --release \
            --no-default-features --features "$feature" --quiet
    ); then
        echo "FATAL: build failed for $side (--features $feature) — see cargo's" \
            "output above. Aborting before running any round." >&2
        exit 1
    fi
    cp "$dir/Cargo.lock" "$OUT_DIR/Cargo.lock.$side"
    echo "$target_dir/release/compare"
}

BASELINE_BIN="$(build_side baseline v0_13)"
CANDIDATE_BIN="$(build_side candidate head)"

# ─── 4. System info + load average before ────────────────────────────
{
    echo "# System info"
    echo
    echo "- date (UTC): $TIMESTAMP"
    echo "- baseline: $BASELINE_REF -> $BASELINE_SHA"
    echo "- candidate: $CANDIDATE_REF -> $CANDIDATE_SHA"
    echo "- rounds: $ROUNDS"
    echo "- scenarios: ${SCENARIOS:-all}"
    echo "- quick: $QUICK"
    echo "- os: $(uname -srm)"
    if command -v sysctl >/dev/null 2>&1 && sysctl -n machdep.cpu.brand_string >/dev/null 2>&1; then
        echo "- cpu: $(sysctl -n machdep.cpu.brand_string)"
        echo "- cores (logical): $(sysctl -n hw.logicalcpu)"
        echo "- cores (physical): $(sysctl -n hw.physicalcpu)"
        echo "- ram: $(( $(sysctl -n hw.memsize) / 1024 / 1024 / 1024 )) GiB"
    elif [ -r /proc/cpuinfo ]; then
        echo "- cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//')"
        echo "- cores (logical): $(nproc)"
        echo "- ram: $(awk '/MemTotal/ {printf "%.1f GiB", $2/1024/1024}' /proc/meminfo)"
    fi
    echo "- rustc: $(rustc --version)"
    echo "- cargo: $(cargo --version)"
    echo "- load average (before): $(uptime | sed 's/.*load average[s]*: *//')"
} > "$OUT_DIR/system_info.md"

# ─── 5. Interleaved rounds: A, B, A, B, ... ──────────────────────────
COMPARE_ARGS=()
if [ "$QUICK" -eq 1 ]; then
    COMPARE_ARGS+=("--quick")
fi
if [ -n "$SCENARIOS" ]; then
    IFS=',' read -r -a SCENARIO_ARR <<< "$SCENARIOS"
    COMPARE_ARGS+=("${SCENARIO_ARR[@]}")
fi

load1() {
    # 1-minute load average, portable across macOS / Linux `uptime`.
    uptime | sed 's/.*load average[s]*: *//' | tr ',' ' ' | awk '{print $1}'
}

wait_for_quiet() {
    [ "$MAX_LOAD" = "0" ] && return 0
    local waited=0
    while awk -v l="$(load1)" -v m="$MAX_LOAD" 'BEGIN { exit !(l >= m) }'; do
        if [ "$waited" -ge 600 ]; then
            echo "load still $(load1) >= $MAX_LOAD after 600 s; running anyway" >&2
            return 0
        fi
        sleep 10
        waited=$((waited + 10))
    done
}

echo "round,side,load_before,load_after" > "$OUT_DIR/load.csv"
run_side() {
    local round="$1" side="$2" bin="$3"
    wait_for_quiet
    local before
    before="$(uptime | sed 's/.*load average[s]*: *//' | tr -d ',')"
    echo "round $round/$ROUNDS: $side (load: $before)" >&2
    # `${arr[@]+"${arr[@]}"}`: an empty array under `set -u` on bash 3.2.
    "$bin" ${COMPARE_ARGS[@]+"${COMPARE_ARGS[@]}"} > "$OUT_DIR/round${round}_${side}.jsonl" \
        2> "$OUT_DIR/round${round}_${side}.stderr.log"
    local after
    after="$(uptime | sed 's/.*load average[s]*: *//' | tr -d ',')"
    echo "$round,$side,$before,$after" >> "$OUT_DIR/load.csv"
}

for round in $(seq 1 "$ROUNDS"); do
    run_side "$round" baseline "$BASELINE_BIN"
    run_side "$round" candidate "$CANDIDATE_BIN"
done

{
    echo "- load average (after): $(uptime | sed 's/.*load average[s]*: *//')"
    echo "- per-run load averages (1 / 5 / 15 min, before and after): load.csv"
} >> "$OUT_DIR/system_info.md"

# ─── 6. Summarize ─────────────────────────────────────────────────────
SUMMARIZER="$SCRATCH/summarize.py"
cat > "$SUMMARIZER" <<'PYEOF'
import json
import sys
from pathlib import Path
from statistics import median

out_dir = Path(sys.argv[1])
rounds = int(sys.argv[2])
NOISY_PP = 10.0
# Regression thresholds on the median-of-rounds p50 delta (#259).
THRESHOLD_PCT = {"uncontended": 3.0, "contended": 5.0}
# Apple silicon `Instant` tick; a single-op (`timer: single`) p50 moves
# in steps of this size, so a delta of at most one tick is not a
# measured regression.
TICK_NS = 41.67

# side -> scenario -> list of per-round rows
data = {"baseline": {}, "candidate": {}}
meta = {}
for side in ("baseline", "candidate"):
    for r in range(1, rounds + 1):
        path = out_dir / f"round{r}_{side}.jsonl"
        if not path.exists():
            continue
        for line in path.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            data[side].setdefault(row["scenario"], []).append(row)
            meta[row["scenario"]] = (row.get("class", "uncontended"), row.get("timer", "batch"))


def stats(rows, key):
    values = [row[key] for row in rows]
    med = median(values)
    spread = (max(values) - min(values)) / med * 100 if med else 0.0
    return med, spread


scenarios = sorted(set(data["baseline"]) | set(data["candidate"]))
lines = [
    "| scenario | class / timer | baseline p50 (spread) | candidate p50 (spread) | p50 delta | baseline p99 | candidate p99 | p99 delta | verdict |",
    "|---|---|---|---|---|---|---|---|---|",
]
csv_lines = [
    "scenario,class,timer,rounds_baseline,rounds_candidate,baseline_p50_median_ns,baseline_p50_spread_pp,"
    "candidate_p50_median_ns,candidate_p50_spread_pp,p50_delta_pct,baseline_p99_median_ns,"
    "candidate_p99_median_ns,p99_delta_pct,baseline_mean_median_ns,candidate_mean_median_ns,verdict"
]
counts = {}

for scenario in scenarios:
    b = data["baseline"].get(scenario, [])
    c = data["candidate"].get(scenario, [])
    cls, timer = meta.get(scenario, ("uncontended", "batch"))
    if not b or not c:
        lines.append(f"| {scenario} | {cls} / {timer} | missing | missing | - | - | - | - | MISSING |")
        csv_lines.append(f"{scenario},{cls},{timer},{len(b)},{len(c)},,,,,,,,,,,MISSING")
        counts["MISSING"] = counts.get("MISSING", 0) + 1
        continue
    b_med, b_spread = stats(b, "p50")
    c_med, c_spread = stats(c, "p50")
    b99, _ = stats(b, "p99")
    c99, _ = stats(c, "p99")
    b_mean = median([row.get("mean", 0.0) for row in b])
    c_mean = median([row.get("mean", 0.0) for row in c])
    delta = (c_med - b_med) / b_med * 100 if b_med else 0.0
    delta99 = (c99 - b99) / b99 * 100 if b99 else 0.0
    threshold = THRESHOLD_PCT.get(cls, 3.0)
    if b_spread > NOISY_PP or c_spread > NOISY_PP:
        verdict = "NOISY"
    elif delta > threshold:
        if timer == "single" and (c_med - b_med) <= TICK_NS + 0.5:
            verdict = "OK (<= 1 tick)"
        else:
            verdict = "REGRESSION"
    elif delta > 0:
        verdict = "OK (within threshold)"
    else:
        verdict = "OK"
    key = verdict.split(" ")[0]
    counts[key] = counts.get(key, 0) + 1
    lines.append(
        f"| {scenario} | {cls} / {timer} | {b_med:.0f} ns ({b_spread:.1f} pp) | {c_med:.0f} ns ({c_spread:.1f} pp) "
        f"| {delta:+.1f}% | {b99:.0f} | {c99:.0f} | {delta99:+.1f}% | {verdict} |"
    )
    csv_lines.append(
        f"{scenario},{cls},{timer},{len(b)},{len(c)},{b_med:.1f},{b_spread:.2f},{c_med:.1f},{c_spread:.2f},"
        f"{delta:.2f},{b99:.1f},{c99:.1f},{delta99:.2f},{b_mean:.1f},{c_mean:.1f},{verdict}"
    )

summary_md = out_dir / "summary.md"
summary_md.write_text(
    "# Bench comparison summary\n\n"
    f"{rounds} interleaved rounds. p50 / p99 in ns, median across rounds; \"spread\" is the "
    "round-to-round `(max - min) / median` of p50 as a percentage. A row with either side's "
    "spread > 10 pp is NOISY: inconclusive, never a pass; re-measure it. Otherwise a p50 "
    "delta above +3 % (uncontended) / +5 % (contended) is a REGRESSION, except for a "
    "single-op-timed row whose p50 moved by at most one clock tick (41.67 ns). See BENCH.md "
    "\"Methodology\".\n\n"
    + "\n".join(lines)
    + "\n\nCounts: "
    + ", ".join(f"{k} {v}" for k, v in sorted(counts.items()))
    + "\n"
)
(out_dir / "summary.csv").write_text("\n".join(csv_lines) + "\n")
print("\n".join(lines))
print("Counts: " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))
PYEOF

python3 "$SUMMARIZER" "$OUT_DIR" "$ROUNDS" | tee "$OUT_DIR/summary_stdout.txt"

echo >&2
echo "wrote $OUT_DIR/summary.md, $OUT_DIR/summary.csv, $OUT_DIR/system_info.md" >&2
echo "raw per-round JSON: $OUT_DIR/round*_{baseline,candidate}.jsonl" >&2
