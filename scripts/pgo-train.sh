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

SCRATCH="$(mktemp -d -t rtk-pgo-XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT
export RTK_DB_PATH="$SCRATCH/rtk.db"
export XDG_CONFIG_HOME="$SCRATCH/config"
mkdir -p "$XDG_CONFIG_HOME/rtk"

CORPUS="$REPO_ROOT/target/pgo-corpus"
mkdir -p "$CORPUS"

echo "pgo-train: capturing corpus from rtk's own repo state ..."

capture() {
  local name="$1"; shift
  if "$@" > "$CORPUS/$name.txt" 2>&1; then
    echo "  captured $name ($(wc -c < "$CORPUS/$name.txt") bytes)"
  else
    echo "  skipped $name (command failed)"
    rm -f "$CORPUS/$name.txt"
  fi
}

capture git_log      git log -20 --oneline
capture git_status   git status
capture git_diff     bash -c 'git diff HEAD~5 HEAD 2>/dev/null || git diff HEAD 2>/dev/null || git diff --cached'
capture cargo_build  bash -c 'cargo build --message-format=short 2>&1'
capture cargo_clippy bash -c 'cargo clippy --all-targets --message-format=short 2>&1'
capture cargo_test   bash -c 'cargo test --no-run --message-format=short 2>&1'

echo "pgo-train: running instrumented workload ..."

run_filter() {
  local cmd_label="$1"
  local fixture="$2"
  shift 2
  if [[ -s "$CORPUS/$fixture.txt" ]]; then
    "$BIN" "$@" < "$CORPUS/$fixture.txt" > /dev/null 2>&1 || true
    echo "  ran   rtk $cmd_label"
  else
    echo "  skip  rtk $cmd_label (no corpus)"
  fi
}

run_filter "git log"      git_log      git log
run_filter "git status"   git_status   git status
run_filter "git diff"     git_diff     git diff
run_filter "cargo build"  cargo_build  cargo build
run_filter "cargo clippy" cargo_clippy cargo clippy
run_filter "cargo test"   cargo_test   cargo test

"$BIN" --version > /dev/null 2>&1 || true
"$BIN" gain > /dev/null 2>&1 || true
"$BIN" gain --history > /dev/null 2>&1 || true
"$BIN" proxy echo hello > /dev/null 2>&1 || true
echo "  ran   rtk --version / gain / gain --history / proxy echo"

captured=$(ls -1 "$CORPUS" 2>/dev/null | wc -l | tr -d ' ')
echo "pgo-train: corpus files captured: $captured"
if [[ "$captured" -lt 4 ]]; then
  echo "pgo-train: WARNING only $captured/6 fixtures captured; profile may be thin" >&2
fi

echo "pgo-train: done"
