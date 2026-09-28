#!/usr/bin/env python3
r"""Clippy-side Production Panic Policy ratchet gate (issue #242 follow-up,
review of PR #266).

`scripts/check_panic_policy.py` gates the syntax-level forms clippy cannot
see at all (the `assert!` family, `saturating_*`/`wrapping_*`) with a
counted allowlist (`scripts/panic_policy_allowlist.txt`): a file's finding
count must match its ledger entry exactly, so both a NEW violation and a
STALE (shrinking) entry fail the build.

The per-file `// panic-policy-ratchet: see #242, removed by the fix issue`
`#![allow(clippy::...)]` markers `check_panic_policy.py --ratchet-report`
lists do not have that property: they are a plain clippy `allow`, so ANY
number of additional violations of an already-listed lint in that same file
— including a brand new one added well after the marker was written — is
silently allowed by `cargo clippy`. This script closes that gap the same
way: a counted ledger (`scripts/clippy_ratchet.txt`, `path:lint:count`) that
fails the build if a ratcheted file's count for a lint grows past its
ledger value (new violation) or falls below it (stale entry, must shrink).

## How it measures a "real" count despite the file-level `allow`

`cargo clippy` cannot report a lint's violations in a scope that has an
`#![allow(...)]` for it — that is what `allow` means. So this script:

1. Copies the whole crate (`Cargo.toml`, `Cargo.lock`, `clippy.toml`,
   `rust-toolchain.toml`, `src/`, `tests/`, `benches/`, `examples/`) to a
   temporary directory. A plain recursive copy of on-disk files (not a git
   worktree / diff-apply) is deliberate: it reflects whatever is actually on
   disk, committed or not, identically in CI (a clean checkout) and locally
   (uncommitted edits) — there is no separate "clean" and "dirty" code path
   to keep in sync.
2. Strips every `// panic-policy-ratchet: ... \n#![allow(clippy::...)]`
   block from the copied `.rs` files (the SAME regex
   `--ratchet-report` above uses to find them), recording which files were
   stripped. Any OTHER `#[allow(...)]` (the `// tests may panic: ...`
   markers on co-located test modules, the standard 9-lint test/bench
   crate-root markers) is left untouched — those are legitimate, permanent
   exemptions, not part of this ratchet.
3. Runs `cargo clippy --all-targets --all-features --message-format=json`
   against the copy with `RUSTFLAGS=--cap-lints=warn`. `--cap-lints=warn`
   caps every lint's EFFECTIVE level at "warn", regardless of the crate's
   own `[lints.clippy]` `"deny"` entries (Cargo.toml is copied unmodified) —
   so with the file-level `allow` gone, clippy now actually evaluates and
   reports each ratcheted lint at that location, as a warning, without
   aborting the build the way a real `deny` would. `--target-dir` points at
   this crate's own `target/` (not a fresh one): the copy's dependencies are
   content-addressed by version, not by the consuming crate's path, so
   external dependency artifacts are fully reused; only `orderbook-rs` (and
   the `examples` workspace member) itself needs a fresh compile against
   the copy's different absolute path. See the Makefile's `lint-clippy-
   ratchet` target for the measured runtime cost of that fresh compile, and
   the caveat that it can invalidate the main tree's own cached `orderbook-
   rs` build artifacts (a `cargo build`/`clippy` run right after this one
   may need to recompile `orderbook-rs` itself, though not its
   dependencies).
4. Counts primary-span `clippy::<lint>` diagnostics per (file, lint),
   restricted to files this run actually stripped a marker from (so a
   completely unmarked file's diagnostics — which are also caught, and
   already denied, by the crate's own normal `cargo clippy -D warnings` in
   `make lint` — are never double-counted here).

Usage:
    scripts/check_clippy_ratchet.py                    # gate mode
    scripts/check_clippy_ratchet.py --write-clippy-ratchet

Exit status is non-zero if any ratcheted file/lint pair's count exceeds or
falls short of `scripts/clippy_ratchet.txt`, or on a clippy/cargo failure
unrelated to the lints under test. `--write-clippy-ratchet` exits 0 on
success and always regenerates the full ledger from a fresh scan.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
LEDGER_PATH = REPO_ROOT / "scripts" / "clippy_ratchet.txt"
RATCHET_MARKER = "panic-policy-ratchet: see #242, removed by the fix issue"

# Same shape `check_panic_policy.py --ratchet-report` parses: the marker
# comment immediately followed by an `#![allow(clippy::a, clippy::b, ...)]`
# (single- or multi-line — `[^)]*` matches embedded newlines).
_RATCHET_ALLOW_RE = re.compile(
    r"//\s*" + re.escape(RATCHET_MARKER) + r"\s*\n\s*#!?\[allow\(\s*(?P<lints>[^)]*)\)\]",
    re.MULTILINE,
)

# What to copy into the scratch build: exactly what `cargo clippy --all-
# targets --all-features` for this workspace (`orderbook-rs` + the
# `examples` member) needs to resolve and compile. Nothing under `scripts/`,
# `doc/`, `Draws/`, `Docker/` is needed for a build.
COPY_ENTRIES = [
    "Cargo.toml",
    "Cargo.lock",
    "clippy.toml",
    "rust-toolchain.toml",
    "src",
    "tests",
    "benches",
    "examples",
]


def _rel(path: Path) -> str:
    p = path.relative_to(REPO_ROOT) if path.is_absolute() else path
    return p.as_posix()


def load_ratchet_lints() -> list[str]:
    """Parses the `name = "deny"` entries out of Cargo.toml's
    `[lints.clippy]` table — the exact lint set the file-level ratchet
    markers are drawn from. Regex-based, not a TOML parser: the pinned
    toolchain's Python (3.9 in CI/local dev at the time this was written)
    predates the `tomllib` stdlib module, and this table's shape (one
    `name = "value"` assignment per line) does not need a real parser.
    """
    text = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r"^\[lints\.clippy\]\n(.*?)(?=^\[|\Z)", text, re.MULTILINE | re.DOTALL)
    if not match:
        return []
    lints: list[str] = []
    for line in match.group(1).splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        name_match = re.match(r'([A-Za-z_][A-Za-z0-9_]*)\s*=\s*"deny"', stripped)
        if name_match:
            lints.append(name_match.group(1))
    return lints


def _blank_replacement(match: re.Match[str]) -> str:
    """Replaces a stripped ratchet block with the same number of blank
    lines, so line numbers in the scratch copy still line up with the
    original file (not load-bearing for correctness — only file+lint counts
    are compared — but it keeps a diagnostic's reported line meaningful for
    a human reading gate output).
    """
    return "\n" * match.group(0).count("\n")


def copy_and_strip(tmp_dir: Path) -> set[str]:
    """Copies the crate into `tmp_dir` and strips every ratchet `allow`
    block from the copy. Returns the set of repo-relative `.rs` paths that
    had at least one block stripped — the only files this run's counts
    apply to.
    """
    for entry in COPY_ENTRIES:
        src = REPO_ROOT / entry
        if not src.exists():
            continue
        dst = tmp_dir / entry
        if src.is_dir():
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)

    stripped: set[str] = set()
    for rs_file in tmp_dir.rglob("*.rs"):
        text = rs_file.read_text(encoding="utf-8")
        new_text, count = _RATCHET_ALLOW_RE.subn(_blank_replacement, text)
        if count:
            rs_file.write_text(new_text, encoding="utf-8")
            stripped.add(rs_file.relative_to(tmp_dir).as_posix())
    return stripped


def run_clippy_scan(
    tmp_dir: Path, target_dir: Path, ratchet_lints: set[str]
) -> tuple[list[tuple[str, str]], int]:
    """Runs clippy against the stripped copy and returns one `(file, lint)`
    pair per primary-span diagnostic for a lint in `ratchet_lints`. Every
    lint is capped to at most `warn` (`--cap-lints=warn`) so the crate's own
    `[lints.clippy]` `"deny"` entries (still present, unmodified, in the
    copied `Cargo.toml`) report instead of aborting the build.
    """
    env = os.environ.copy()
    extra_flags = "--cap-lints=warn"
    env["RUSTFLAGS"] = f"{env['RUSTFLAGS']} {extra_flags}".strip() if env.get("RUSTFLAGS") else extra_flags

    cmd = [
        "cargo",
        "clippy",
        "--manifest-path",
        str(tmp_dir / "Cargo.toml"),
        "--all-targets",
        "--all-features",
        "--target-dir",
        str(target_dir),
        "--message-format=json",
    ]
    result = subprocess.run(cmd, cwd=tmp_dir, env=env, capture_output=True, text=True, check=False)

    findings: list[tuple[str, str]] = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if payload.get("reason") != "compiler-message":
            continue
        message = payload.get("message", {})
        code = (message.get("code") or {}).get("code")
        if not code or not code.startswith("clippy::"):
            continue
        lint = code[len("clippy::") :]
        if lint not in ratchet_lints:
            continue
        for span in message.get("spans", []):
            if span.get("is_primary"):
                findings.append((Path(span["file_name"]).as_posix(), lint))
    return findings, result.returncode


def load_ledger() -> dict[tuple[str, str], int]:
    entries: dict[tuple[str, str], int] = {}
    if not LEDGER_PATH.exists():
        return entries
    for line_no, raw_line in enumerate(LEDGER_PATH.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(":")
        if len(parts) != 3:
            print(f"scripts/clippy_ratchet.txt:{line_no}: malformed line (want path:lint:count): {raw_line}")
            continue
        path_str, lint, count_str = parts
        try:
            count = int(count_str)
        except ValueError:
            print(f"scripts/clippy_ratchet.txt:{line_no}: non-integer count: {raw_line}")
            continue
        entries[(path_str, lint)] = count
    return entries


def scan(*, verbose_cmd_failures: bool) -> tuple[dict[tuple[str, str], int], int]:
    """Runs the full copy/strip/clippy pipeline once. Returns `(counts,
    clippy_returncode)`. A non-zero `clippy_returncode` with no useful
    diagnostics (a real compile error unrelated to the ratchet lints) is the
    caller's problem to surface.
    """
    ratchet_lints = set(load_ratchet_lints())
    with tempfile.TemporaryDirectory(prefix="orderbook-rs-clippy-ratchet-") as tmp:
        tmp_dir = Path(tmp)
        stripped_files = copy_and_strip(tmp_dir)
        target_dir = REPO_ROOT / "target"
        findings, returncode = run_clippy_scan(tmp_dir, target_dir, ratchet_lints)
        counts: dict[tuple[str, str], int] = {}
        for file_name, lint in findings:
            if file_name not in stripped_files:
                continue
            key = (file_name, lint)
            counts[key] = counts.get(key, 0) + 1
        if verbose_cmd_failures and returncode not in (0, 101):
            print(f"check_clippy_ratchet: cargo clippy exited {returncode} unexpectedly")
        return counts, returncode


def write_clippy_ratchet() -> int:
    counts, _ = scan(verbose_cmd_failures=True)
    lines = [
        "# Clippy-side Production Panic Policy ratchet ledger (issue #242 follow-up).",
        "#",
        "# One `path:lint:count` line per ratchet-marked file/lint pair with its",
        "# CURRENT clippy finding count once that file's `// panic-policy-ratchet`",
        "# `#![allow(...)]` is stripped (see scripts/check_clippy_ratchet.py). A file",
        "# not listed here, or listed with a lower count than it now has, means a",
        "# new violation was added inside an already-ratcheted `allow` and must be",
        "# fixed, not papered over by raising the count. Regenerated by",
        "# `scripts/check_clippy_ratchet.py --write-clippy-ratchet`.",
        "",
    ]
    for key in sorted(counts):
        path_str, lint = key
        lines.append(f"{path_str}:{lint}:{counts[key]}")
    LEDGER_PATH.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"check_clippy_ratchet --write-clippy-ratchet: wrote {len(counts)} entr{'y' if len(counts) == 1 else 'ies'} to {_rel(LEDGER_PATH)}")
    return 0


def run_gate() -> int:
    counts, returncode = scan(verbose_cmd_failures=True)
    ledger = load_ledger()

    status = 0
    all_keys = sorted(set(counts) | set(ledger))
    for key in all_keys:
        path_str, lint = key
        actual = counts.get(key, 0)
        allowed = ledger.get(key, 0)
        if actual > allowed:
            status = 1
            print(f"{path_str}: {actual} clippy::{lint} finding(s), {allowed} allowed — new violation(s)")
        elif actual < allowed:
            status = 1
            print(
                f"{path_str}: {actual} clippy::{lint} finding(s), {allowed} allowed — "
                "stale ledger entry, shrink it (run --write-clippy-ratchet)"
            )

    if status == 0:
        print("check_clippy_ratchet: every ratcheted file matches its ledger count exactly.")
    else:
        print(
            "check_clippy_ratchet: ratchet mismatch(es) found. See doc/panic-boundaries.md "
            "and scripts/clippy_ratchet.txt."
        )
    if returncode not in (0, 101) and not counts:
        # cargo clippy failed for a reason unrelated to the ratchet lints
        # (e.g. a genuine compile error in the scratch copy) and produced no
        # usable diagnostics at all: surface that distinctly from a clean
        # "0 findings" ratchet pass.
        print("check_clippy_ratchet: cargo clippy produced no diagnostics; see stderr above for a build failure.")
        status = 1
    return status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write-clippy-ratchet",
        action="store_true",
        help="Regenerate scripts/clippy_ratchet.txt from a fresh scan instead of gating on it.",
    )
    args = parser.parse_args()
    if args.write_clippy_ratchet:
        return write_clippy_ratchet()
    return run_gate()


if __name__ == "__main__":
    sys.exit(main())
