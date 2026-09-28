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
#   --scenarios LIST   Comma-separated scenario names (default:
#                      add_only,cancel_only,aggressive_walk)
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
SCENARIOS="add_only,cancel_only,aggressive_walk"
QUICK=0
OUT_DIR=""
KEEP_WORKTREES=0

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --baseline) BASELINE_REF="$2"; shift 2 ;;
        --candidate) CANDIDATE_REF="$2"; shift 2 ;;
        --rounds) ROUNDS="$2"; ROUNDS_SET=1; shift 2 ;;
        --scenarios) SCENARIOS="$2"; shift 2 ;;
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
    echo "- scenarios: $SCENARIOS"
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
IFS=',' read -r -a SCENARIO_ARR <<< "$SCENARIOS"
COMPARE_ARGS=("${SCENARIO_ARR[@]}")
if [ "$QUICK" -eq 1 ]; then
    COMPARE_ARGS=("--quick" "${COMPARE_ARGS[@]}")
fi

for round in $(seq 1 "$ROUNDS"); do
    echo "round $round/$ROUNDS: baseline" >&2
    "$BASELINE_BIN" "${COMPARE_ARGS[@]}" > "$OUT_DIR/round${round}_baseline.jsonl" \
        2> "$OUT_DIR/round${round}_baseline.stderr.log"
    echo "round $round/$ROUNDS: candidate" >&2
    "$CANDIDATE_BIN" "${COMPARE_ARGS[@]}" > "$OUT_DIR/round${round}_candidate.jsonl" \
        2> "$OUT_DIR/round${round}_candidate.stderr.log"
done

{
    echo "- load average (after): $(uptime | sed 's/.*load average[s]*: *//')"
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
noisy_threshold_pp = 10.0

# side -> scenario -> [p50 per round]
data = {"baseline": {}, "candidate": {}}
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
            data[side].setdefault(row["scenario"], []).append(row["p50"])

scenarios = sorted(set(data["baseline"]) | set(data["candidate"]))
lines = []
lines.append("| scenario | baseline p50 (median/spread) | candidate p50 (median/spread) | delta | verdict |")
lines.append("|---|---|---|---|---|")

csv_lines = ["scenario,baseline_median_ns,baseline_spread_pp,candidate_median_ns,candidate_spread_pp,delta_pct,verdict"]

for scenario in scenarios:
    b = data["baseline"].get(scenario, [])
    c = data["candidate"].get(scenario, [])
    if not b or not c:
        lines.append(f"| {scenario} | missing | missing | - | NOISY (missing data) |")
        csv_lines.append(f"{scenario},,,,,MISSING")
        continue
    b_med = median(b)
    c_med = median(c)
    b_spread = (max(b) - min(b)) / b_med * 100 if b_med else 0.0
    c_spread = (max(c) - min(c)) / c_med * 100 if c_med else 0.0
    delta_pct = (c_med - b_med) / b_med * 100 if b_med else 0.0
    noisy = b_spread > noisy_threshold_pp or c_spread > noisy_threshold_pp
    verdict = "NOISY (inconclusive)" if noisy else ("REGRESSION" if delta_pct > 3.0 else "OK")
    lines.append(
        f"| {scenario} | {b_med:.0f} ns ({b_spread:.1f} pp) | {c_med:.0f} ns ({c_spread:.1f} pp) "
        f"| {delta_pct:+.1f}% | {verdict} |"
    )
    csv_lines.append(
        f"{scenario},{b_med:.1f},{b_spread:.2f},{c_med:.1f},{c_spread:.2f},{delta_pct:.2f},{verdict}"
    )

summary_md = out_dir / "summary.md"
summary_md.write_text(
    "# Bench comparison summary\n\n"
    "p50, ns; \"spread\" is round-to-round `(max - min) / median` as a "
    "percentage. A row with either side's spread > 10 pp is NOISY: "
    "inconclusive, never a pass, and should be re-measured with more "
    "rounds before drawing any conclusion (see BENCH.md \"Methodology\").\n\n"
    + "\n".join(lines)
    + "\n"
)
(out_dir / "summary.csv").write_text("\n".join(csv_lines) + "\n")
print("\n".join(lines))
PYEOF

python3 "$SUMMARIZER" "$OUT_DIR" "$ROUNDS" | tee "$OUT_DIR/summary_stdout.txt"

echo >&2
echo "wrote $OUT_DIR/summary.md, $OUT_DIR/summary.csv, $OUT_DIR/system_info.md" >&2
echo "raw per-round JSON: $OUT_DIR/round*_{baseline,candidate}.jsonl" >&2
