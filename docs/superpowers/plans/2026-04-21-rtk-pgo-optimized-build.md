# rtk PGO-Optimized Local Build Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `make release-pgo` one-command workflow that produces a PGO-optimized, `target-cpu=native`-tuned rtk binary for local developer installs, without changing the portable `cargo build --release` artifact used by deb/rpm packaging.

**Architecture:** Three Cargo profiles (unchanged `release`, new `release-native`, new PGO-safe `release-pgo`), a `.cargo/config.toml` scoping native-CPU flags per profile, a fixture-generating training script that exercises rtk's hot filter paths against its own repo's git/cargo output, and a `cargo-pgo`-driven orchestrator that produces `target/release/rtk-pgo`. Verification by hyperfine before/after and the existing `cargo test` suite.

**Tech Stack:** Rust 1.74+ (per-profile rustflags), `cargo-pgo` crate, `llvm-tools-preview` rustup component, `hyperfine` for benchmarking, POSIX shell.

**Spec:** `docs/superpowers/specs/2026-04-21-rtk-pgo-optimized-build-design.md`

**Amendment from spec:** Training corpus is captured live at training time from the rtk repo's own git and cargo state into `target/pgo-corpus/` (not committed). Spec section "Training Script" referenced `tests/fixtures/*.txt` files that don't exist yet — the live-capture approach honors the same "real, not synthetic" principle.

---

## File Inventory

| Path | Action | Purpose |
|------|--------|---------|
| `rtk/Cargo.toml` | MODIFY | Expand `[profile.release]`, add `release-native` and `release-pgo` |
| `rtk/.cargo/config.toml` | CREATE | Per-profile `target-cpu=native` rustflags |
| `rtk/scripts/pgo-train.sh` | CREATE | Training workload — captures corpus, runs ~20 invocations |
| `rtk/scripts/build-pgo.sh` | CREATE | End-to-end PGO orchestrator |
| `rtk/scripts/bench-pgo.sh` | CREATE | Hyperfine verification gate |
| `rtk/Makefile` | CREATE | Three phony targets: `release-native`, `release-pgo`, `bench-pgo` |
| `rtk/.gitignore` | MODIFY | Ignore `target/pgo-corpus/` and `target/pgo-profiles/` (cargo-pgo may already ignore the latter via target/) |
| `rtk/CLAUDE.md` | MODIFY | New "Optimized Builds" subsection |

Nine file touches. No source code in `src/` changes.

---

## Task 1: Expand Cargo.toml release profile and add two new profiles

**Files:**
- Modify: `rtk/Cargo.toml:45-50`

- [ ] **Step 1: Read current profile block**

Read `rtk/Cargo.toml` lines 45-68 to confirm the current `[profile.release]` block matches what the plan expects. If the release block doesn't match exactly, stop and reconcile with the spec before editing.

- [ ] **Step 2: Replace the release profile block**

Replace lines 45-50 of `rtk/Cargo.toml` (the current `[profile.release]` block) with:

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
overflow-checks = false
debug = false

[profile.release.package."*"]
opt-level = 3

[profile.release-native]
inherits = "release"

[profile.release-pgo]
inherits = "release"
lto = "thin"
```

Preserve the cargo-deb / cargo-generate-rpm metadata blocks (lines 52+) untouched.

- [ ] **Step 3: Verify Cargo.toml still parses**

Run: `cd rtk && cargo metadata --no-deps --format-version 1 > /dev/null`
Expected: exit 0, no output. If it fails, read the error — likely a TOML syntax issue in the edits.

- [ ] **Step 4: Verify portable release still builds**

Run: `cd rtk && cargo build --release --bin rtk`
Expected: exit 0, `target/release/rtk` exists.

- [ ] **Step 5: Verify release-native profile compiles**

Run: `cd rtk && cargo build --profile release-native --bin rtk`
Expected: exit 0, `target/release-native/rtk` exists. (Note: `target-cpu=native` is not yet wired; this just validates the profile name is accepted.)

- [ ] **Step 6: Commit**

```bash
cd rtk
git add Cargo.toml
git commit -m "build: expand release profile, add release-native and release-pgo"
```

---

## Task 2: Create .cargo/config.toml for per-profile rustflags

**Files:**
- Create: `rtk/.cargo/config.toml`
- Modify: `rtk/.gitignore` (if `.cargo/` is ignored — unlikely but verify)

- [ ] **Step 1: Verify rustc version supports per-profile rustflags**

Run: `rustc --version`
Expected: Rust 1.74.0 or newer. If older, stop — the plan needs a `RUSTFLAGS=` fallback in `build-pgo.sh` (see Task 5 Step 3 alternative).

- [ ] **Step 2: Check .gitignore does not exclude .cargo/**

Run: `cd rtk && git check-ignore -v .cargo/config.toml; echo "exit=$?"`
Expected: `exit=1` (not ignored). If ignored, add an exception to `.gitignore`.

- [ ] **Step 3: Create the config file**

Create `rtk/.cargo/config.toml` with exactly:

```toml
# Per-profile rustflags. target-cpu=native is intentionally NOT applied to the
# plain `release` profile because that's the profile used by cargo-deb and
# cargo-generate-rpm to produce portable distributable binaries.

[profile.release-native]
rustflags = ["-C", "target-cpu=native"]

[profile.release-pgo]
rustflags = ["-C", "target-cpu=native"]
```

- [ ] **Step 4: Verify release-native picks up the flag**

Run: `cd rtk && cargo build --profile release-native --bin rtk -v 2>&1 | grep -o 'target-cpu=[a-z]*' | head -1`
Expected: `target-cpu=native`. If empty, the per-profile rustflag config is not being read — check the rustc version again and the exact key path (`[profile.release-native]`).

- [ ] **Step 5: Verify plain release does NOT pick up native flag**

Run: `cd rtk && cargo build --release --bin rtk -v 2>&1 | grep -c 'target-cpu=native' || true`
Expected: `0`. If nonzero, rustflags are leaking — stop and fix before merging.

- [ ] **Step 6: Commit**

```bash
cd rtk
git add .cargo/config.toml
git commit -m "build: scope target-cpu=native to release-native and release-pgo profiles"
```

---

## Task 3: Verify release-native end-to-end via cargo install

**Files:** none changed

- [ ] **Step 1: Install from release-native into a scratch prefix**

Run:
```bash
cd rtk
SCRATCH=$(mktemp -d)
cargo install --path . --profile release-native --force --root "$SCRATCH"
ls -l "$SCRATCH/bin/rtk"
```
Expected: exit 0, `$SCRATCH/bin/rtk` exists, size < 10MB.

- [ ] **Step 2: Smoke-test the installed binary**

Run: `"$SCRATCH/bin/rtk" --version`
Expected: prints `rtk 0.37.0` (or the current version from `Cargo.toml`).

- [ ] **Step 3: Exercise one filter path end-to-end**

Run:
```bash
cd /tmp && git init -q smoke && cd smoke && git commit --allow-empty -m "x" -q
"$SCRATCH/bin/rtk" git log -5
```
Expected: exit 0, output includes the commit. Clean up: `rm -rf /tmp/smoke "$SCRATCH"`.

- [ ] **Step 4: No commit**

This task verifies only — no files changed. Proceed to Task 4.

---

## Task 4: Create scripts/pgo-train.sh

**Files:**
- Create: `rtk/scripts/pgo-train.sh`

- [ ] **Step 1: Verify scripts/ dir exists**

Run: `ls rtk/scripts/ 2>/dev/null && echo exists || mkdir -p rtk/scripts`
Expected: either `exists` or silent mkdir.

- [ ] **Step 2: Create the training script**

Create `rtk/scripts/pgo-train.sh` with exactly:

```bash
#!/usr/bin/env bash
# PGO training workload. Exercises rtk's hot filter paths against real command
# output captured at training time. Runs under an isolated RTK_DB_PATH and
# XDG_CONFIG_HOME so it cannot touch the developer's real tracking DB or config.
#
# Usage:
#   BIN=target/<target-triple>/release-pgo/rtk ./scripts/pgo-train.sh
#
# Required env:
#   BIN  - absolute or repo-relative path to the instrumented rtk binary

set -euo pipefail

if [[ -z "${BIN:-}" ]]; then
  echo "pgo-train: BIN env var must point at the instrumented rtk binary" >&2
  exit 2
fi
if [[ ! -x "$BIN" ]]; then
  echo "pgo-train: \$BIN ($BIN) is not executable" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# Isolation: scratch dirs, cleaned on exit
SCRATCH="$(mktemp -d -t rtk-pgo-XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT
export RTK_DB_PATH="$SCRATCH/rtk.db"
export XDG_CONFIG_HOME="$SCRATCH/config"
mkdir -p "$XDG_CONFIG_HOME/rtk"

CORPUS="$REPO_ROOT/target/pgo-corpus"
mkdir -p "$CORPUS"

echo "pgo-train: capturing corpus from rtk's own repo state ..."

# Capture real command output into the corpus. Each capture is best-effort:
# if a command fails (e.g. gh not authed), the corresponding training run is
# skipped rather than aborting the whole training phase.
capture() {
  local name="$1"; shift
  if "$@" > "$CORPUS/$name.txt" 2>&1; then
    echo "  captured $name ($(wc -c < "$CORPUS/$name.txt") bytes)"
  else
    echo "  skipped $name (command failed)"
    rm -f "$CORPUS/$name.txt"
  fi
}

capture git_log     git log -20 --oneline
capture git_status  git status
capture git_diff    bash -c 'git diff HEAD~5 HEAD || git diff HEAD'
capture cargo_build cargo build --message-format=short 2>&1
capture cargo_clippy cargo clippy --all-targets --message-format=short 2>&1
capture cargo_test  cargo test --no-run --message-format=short 2>&1

echo "pgo-train: running instrumented workload ..."

# Feed each captured fixture through the matching rtk filter, piping from the
# corpus so we train on real output. Tracking writes hit the scratch DB.
run() {
  local desc="$1"; shift
  if [[ -n "${1:-}" && "${1%<*}" == "$1" ]]; then
    # No stdin redirect sentinel; just run
    if "$@" > /dev/null 2>&1; then
      echo "  ran   $desc"
    else
      echo "  ran   $desc (non-zero exit, profile data still useful)"
    fi
  fi
}

# Filter runs (stdin-driven)
for pair in \
  "git log:git_log" \
  "git status:git_status" \
  "git diff:git_diff" \
  "cargo build:cargo_build" \
  "cargo clippy:cargo_clippy" \
  "cargo test:cargo_test" ; do
  cmd="${pair%%:*}"
  fx="${pair##*:}"
  if [[ -s "$CORPUS/$fx.txt" ]]; then
    # shellcheck disable=SC2086
    "$BIN" $cmd < "$CORPUS/$fx.txt" > /dev/null 2>&1 || true
    echo "  ran   rtk $cmd"
  else
    echo "  skip  rtk $cmd (no corpus)"
  fi
done

# Non-stdin invocations exercise routing, SQLite writes, and cold-startup paths
"$BIN" --version > /dev/null 2>&1
"$BIN" gain > /dev/null 2>&1 || true
"$BIN" gain --history > /dev/null 2>&1 || true
"$BIN" proxy echo hello > /dev/null 2>&1 || true
echo "  ran   rtk --version / gain / gain --history / proxy echo"

# Coverage check: require at least 4 of the 6 filter captures to have succeeded
captured=$(ls "$CORPUS" | wc -l | tr -d ' ')
echo "pgo-train: corpus files captured: $captured"
if [[ "$captured" -lt 4 ]]; then
  echo "pgo-train: WARNING only $captured/6 fixtures captured; profile may be thin" >&2
fi

echo "pgo-train: done"
```

- [ ] **Step 3: Make it executable**

Run: `chmod +x rtk/scripts/pgo-train.sh`

- [ ] **Step 4: Smoke-test with a non-instrumented debug binary**

Run:
```bash
cd rtk
cargo build --bin rtk
BIN="target/debug/rtk" ./scripts/pgo-train.sh
```
Expected: prints "captured ..." lines for most fixtures, "ran rtk ..." lines for each, and ends with `pgo-train: done`. Exit 0.

- [ ] **Step 5: Commit**

```bash
cd rtk
git add scripts/pgo-train.sh
git commit -m "build: add PGO training workload script"
```

---

## Task 5: Create scripts/build-pgo.sh

**Files:**
- Create: `rtk/scripts/build-pgo.sh`

- [ ] **Step 1: Create the orchestrator**

Create `rtk/scripts/build-pgo.sh` with exactly:

```bash
#!/usr/bin/env bash
# End-to-end PGO build. Produces target/release/rtk-pgo.
#
# Steps:
#   1. Ensure cargo-pgo and llvm-tools-preview are installed
#   2. Wipe stale profile data
#   3. Build instrumented binary (cargo pgo build)
#   4. Run training workload against it
#   5. Merge profiles and build optimized binary (cargo pgo optimize build)
#   6. Copy optimized binary to target/release/rtk-pgo

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "build-pgo: preflight ..."
if ! command -v cargo-pgo > /dev/null; then
  echo "  installing cargo-pgo ..."
  cargo install cargo-pgo
fi
rustup component add llvm-tools-preview

TARGET_TRIPLE="$(rustc -vV | sed -n 's|host: ||p')"
PROFILE=release-pgo
PROFILE_DIR="target/$TARGET_TRIPLE/$PROFILE"

echo "build-pgo: cleaning stale PGO profiles ..."
rm -rf "$REPO_ROOT/target/pgo-profiles"

echo "build-pgo: step 1/3 — building instrumented binary ..."
cargo pgo build -- --bin rtk --profile "$PROFILE"

INSTR_BIN="$PROFILE_DIR/rtk"
if [[ ! -x "$INSTR_BIN" ]]; then
  echo "build-pgo: instrumented binary not found at $INSTR_BIN" >&2
  exit 1
fi

echo "build-pgo: step 2/3 — running training workload ..."
BIN="$INSTR_BIN" ./scripts/pgo-train.sh

echo "build-pgo: step 3/3 — optimizing with gathered profile ..."
cargo pgo optimize build -- --bin rtk --profile "$PROFILE"

OPT_BIN="$PROFILE_DIR/rtk"
OUT_BIN="$REPO_ROOT/target/release/rtk-pgo"
mkdir -p "$(dirname "$OUT_BIN")"
cp "$OPT_BIN" "$OUT_BIN"

echo "build-pgo: done"
ls -lh "$OUT_BIN"
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x rtk/scripts/build-pgo.sh`

- [ ] **Step 3: Dry-run check (do not execute the full pipeline yet)**

Run: `bash -n rtk/scripts/build-pgo.sh`
Expected: exit 0 (shell syntax check passes).

- [ ] **Step 4: Commit**

```bash
cd rtk
git add scripts/build-pgo.sh
git commit -m "build: add PGO end-to-end orchestrator"
```

---

## Task 6: Run build-pgo end-to-end and verify artifact

**Files:** none changed

- [ ] **Step 1: Execute the full PGO build**

Run: `cd rtk && ./scripts/build-pgo.sh`
Expected: all three steps complete, final line is `ls -lh` output for `target/release/rtk-pgo`.

If `cargo pgo optimize build` reports "no profile data" — the training script ran the uninstrumented binary. Check that `$PROFILE_DIR/rtk` matches what `cargo pgo build` produced.

- [ ] **Step 2: Smoke-test the PGO binary**

Run: `rtk/target/release/rtk-pgo --version`
Expected: prints `rtk 0.37.0` (or current `Cargo.toml` version).

- [ ] **Step 3: Run the rtk test suite under the PGO profile**

Run: `cd rtk && cargo test --profile release-pgo --bin rtk`
Expected: all tests pass. A miscompile from PGO would show up here.

- [ ] **Step 4: No commit**

Verification only.

---

## Task 7: Create scripts/bench-pgo.sh

**Files:**
- Create: `rtk/scripts/bench-pgo.sh`

- [ ] **Step 1: Verify hyperfine is installed**

Run: `command -v hyperfine`
Expected: a path. If missing: `brew install hyperfine` (macOS) or `cargo install hyperfine`.

- [ ] **Step 2: Create the bench script**

Create `rtk/scripts/bench-pgo.sh` with exactly:

```bash
#!/usr/bin/env bash
# Verification gate for the PGO build. Compares target/release/rtk (portable
# baseline) against target/release/rtk-pgo (PGO-optimized) on startup and on a
# hot-path filter. Reports results and pass/fail, but does not hard-exit on
# regression to avoid blocking a developer on noisy single-run measurements.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v hyperfine > /dev/null; then
  echo "bench-pgo: hyperfine not found. Install with 'brew install hyperfine' or 'cargo install hyperfine'." >&2
  exit 2
fi

echo "bench-pgo: building baseline ..."
cargo build --release --bin rtk

if [[ ! -x target/release/rtk-pgo ]]; then
  echo "bench-pgo: target/release/rtk-pgo not found. Run ./scripts/build-pgo.sh first." >&2
  exit 2
fi

mkdir -p target/bench

CORPUS="target/pgo-corpus"
if [[ ! -s "$CORPUS/git_log.txt" ]]; then
  echo "bench-pgo: capturing git_log corpus for bench ..."
  git log -20 --oneline > "$CORPUS/git_log.txt" 2>&1 || true
fi

echo ""
echo "=== Startup benchmark (rtk --version) ==="
hyperfine --warmup 5 --min-runs 30 \
  'target/release/rtk --version' \
  'target/release/rtk-pgo --version' \
  --export-markdown target/bench/startup.md

echo ""
echo "=== Hot-path benchmark (rtk git log -20 < corpus) ==="
if [[ -s "$CORPUS/git_log.txt" ]]; then
  hyperfine --warmup 3 --min-runs 20 \
    "target/release/rtk git log -20 < $CORPUS/git_log.txt" \
    "target/release/rtk-pgo git log -20 < $CORPUS/git_log.txt" \
    --export-markdown target/bench/gitlog.md
else
  echo "bench-pgo: skipped (no git_log corpus available)"
fi

echo ""
echo "=== Binary sizes ==="
ls -l target/release/rtk target/release/rtk-pgo

echo ""
echo "=== Test suite under release-pgo profile ==="
cargo test --profile release-pgo --bin rtk 2>&1 | tail -5

echo ""
echo "bench-pgo: pass criteria (eyeball):"
echo "  - Startup:    PGO ≤ baseline + 0ms"
echo "  - Hot path:   PGO ≥ 3% faster on git log filter"
echo "  - Size:       PGO within ±10% of baseline"
echo "  - Tests:      all pass under release-pgo profile"
echo ""
echo "bench-pgo: markdown reports saved to target/bench/"
```

- [ ] **Step 3: Make it executable**

Run: `chmod +x rtk/scripts/bench-pgo.sh`

- [ ] **Step 4: Run the bench (assumes Task 6 completed)**

Run: `cd rtk && ./scripts/bench-pgo.sh`
Expected: two hyperfine tables printed, a size listing, a test-suite summary, and the four-line pass-criteria footer. Exit 0.

- [ ] **Step 5: Record baseline results in `target/bench/` (not committed)**

The `target/` directory is already gitignored. The markdown reports are for the developer's eyes, not for the repo.

- [ ] **Step 6: Commit the script**

```bash
cd rtk
git add scripts/bench-pgo.sh
git commit -m "build: add PGO vs baseline verification bench"
```

---

## Task 8: Create Makefile

**Files:**
- Create: `rtk/Makefile`

- [ ] **Step 1: Verify no existing Makefile**

Run: `ls rtk/Makefile 2>/dev/null && echo EXISTS || echo OK`
Expected: `OK`. If `EXISTS`, stop and reconcile — the plan assumed no Makefile.

- [ ] **Step 2: Create the Makefile**

Create `rtk/Makefile` with exactly:

```make
# rtk optimized-build entry points. See docs/superpowers/specs/2026-04-21-rtk-pgo-optimized-build-design.md

.PHONY: release-native release-pgo bench-pgo help

help:
	@echo "rtk build targets:"
	@echo "  release-native   cargo install with target-cpu=native (fast, no PGO)"
	@echo "  release-pgo      full PGO build -> target/release/rtk-pgo"
	@echo "  bench-pgo        hyperfine baseline vs PGO binary"

release-native:
	cargo install --path . --profile release-native --force

release-pgo:
	./scripts/build-pgo.sh

bench-pgo:
	./scripts/bench-pgo.sh
```

Note: the recipe lines must be indented with a single TAB character, not spaces.

- [ ] **Step 3: Verify make parses the file**

Run: `cd rtk && make -n help`
Expected: prints the `@echo` lines (Make dry-run). No "missing separator" error (that would indicate spaces instead of tabs).

- [ ] **Step 4: Verify each target dry-runs cleanly**

Run: `cd rtk && make -n release-pgo bench-pgo`
Expected: prints `./scripts/build-pgo.sh` and `./scripts/bench-pgo.sh`.

- [ ] **Step 5: Commit**

```bash
cd rtk
git add Makefile
git commit -m "build: add Makefile with release-native, release-pgo, bench-pgo targets"
```

---

## Task 9: Update CLAUDE.md with "Optimized Builds" section

**Files:**
- Modify: `rtk/CLAUDE.md`

- [ ] **Step 1: Locate the "Build & Run" block**

Read `rtk/CLAUDE.md` and find the "### Build & Run" section (near the top of "## Development Commands"). Confirm it matches the version already in the project before editing.

- [ ] **Step 2: Insert a new subsection directly after "### Build & Run"**

Insert the following block after the closing ``` of the "Build & Run" code block, before "### Testing":

```markdown
### Optimized Builds (local dev)

For benchmarking or a faster personal install, use the optimized profiles
(see `docs/superpowers/specs/2026-04-21-rtk-pgo-optimized-build-design.md`):

```bash
make release-native   # cargo install --profile release-native (target-cpu=native, no PGO)
make release-pgo      # full PGO pipeline -> target/release/rtk-pgo
make bench-pgo        # hyperfine baseline vs PGO binary
```

- `cargo build --release` is unchanged and remains the portable profile used by
  `cargo deb` / `cargo generate-rpm` — do not add `target-cpu=native` to it.
- `release-pgo` requires `cargo-pgo` (auto-installed on first run) and the
  `llvm-tools-preview` rustup component (also auto-added).
```

- [ ] **Step 3: Verify the edit**

Run: `grep -c "Optimized Builds" rtk/CLAUDE.md`
Expected: `1`.

- [ ] **Step 4: Commit**

```bash
cd rtk
git add CLAUDE.md
git commit -m "docs: document release-native / release-pgo / bench-pgo make targets"
```

---

## Task 10: Update .gitignore for PGO scratch directories

**Files:**
- Modify: `rtk/.gitignore`

- [ ] **Step 1: Check current .gitignore**

Run: `grep -E '^target/?$|pgo-' rtk/.gitignore || true`
Expected: likely just `target/` or `target`. If `target/` alone is ignored, Task 10 is a no-op because `target/pgo-corpus/` and `target/pgo-profiles/` are already covered — skip to Step 3.

- [ ] **Step 2: (Only if target/ is NOT globally ignored) Add explicit entries**

Append to `rtk/.gitignore`:

```
target/pgo-corpus/
target/pgo-profiles/
```

- [ ] **Step 3: Verify no PGO artifacts are tracked**

Run: `cd rtk && git status --short | grep -E 'pgo-(corpus|profiles)' || echo CLEAN`
Expected: `CLEAN`.

- [ ] **Step 4: Commit (only if Step 2 modified the file)**

```bash
cd rtk
git add .gitignore
git commit -m "build: ignore PGO scratch directories"
```

---

## Task 11: Final end-to-end verification

**Files:** none changed

- [ ] **Step 1: Clean target and rebuild everything**

Run: `cd rtk && cargo clean`
Expected: exit 0.

- [ ] **Step 2: Confirm portable release is untouched**

Run: `cd rtk && cargo build --release --bin rtk && file target/release/rtk`
Expected: builds, `file` output shows a Mach-O (macOS) or ELF (Linux) executable. Record the binary size: `ls -lh target/release/rtk`.

- [ ] **Step 3: Run the full PGO pipeline**

Run: `cd rtk && make release-pgo`
Expected: completes without error, prints final `ls -lh` for `target/release/rtk-pgo`.

- [ ] **Step 4: Run the bench**

Run: `cd rtk && make bench-pgo`
Expected: prints both hyperfine tables, size listing, test summary, pass criteria.

- [ ] **Step 5: Confirm the portable release still targets generic CPU**

Run: `cd rtk && cargo build --release --bin rtk -v 2>&1 | grep -c 'target-cpu=native' || true`
Expected: `0`. Native CPU flags MUST NOT have leaked into the portable profile.

- [ ] **Step 6: Run pre-commit gate from CLAUDE.md**

Run: `cd rtk && cargo fmt --all && cargo clippy --all-targets && cargo test --all`
Expected: all green.

- [ ] **Step 7: No commit — this task verifies only**

---

## Self-Review Notes

**Spec coverage:** Every component in the spec (Profile config, Rustflag config, Training script, Build orchestrator, Verification gate, Docs) has a dedicated task. File inventory matches the spec's file inventory plus one addition: `.gitignore` (Task 10), needed because the spec mentions scratch directories but doesn't call out that they land under `target/` (usually already ignored).

**Known deviation from spec (documented above):** Training corpus is captured live rather than read from `tests/fixtures/*.txt`. The committed fixtures referenced in the spec don't exist in the repo. The live-capture approach preserves the spec's intent (real captured output, not synthetic) and adds a best-effort coverage check ("warn if <4/6 captures succeeded").

**Type / path consistency:** All scripts reference `target/$TARGET_TRIPLE/release-pgo/rtk` via the same `rustc -vV` derivation. `BIN` env var name is consistent between `pgo-train.sh` and `build-pgo.sh`. Makefile targets (`release-native`, `release-pgo`, `bench-pgo`) are consistent between `Makefile` and CLAUDE.md.

**Risk coverage:** All eight risks from the spec map to tasks — per-profile rustflag leak check (Task 2 Step 5, Task 11 Step 5), fat-LTO + PGO bug avoided by `lto = "thin"` on release-pgo (Task 1), isolation of training DB/config (Task 4), preflight for `cargo-pgo` and `llvm-tools-preview` (Task 5), pre-1.74 toolchain gate (Task 2 Step 1).
