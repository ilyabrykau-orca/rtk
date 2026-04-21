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
hyperfine --warmup 10 --min-runs 50 -N \
  'target/release/rtk --version' \
  'target/release/rtk-pgo --version' \
  --export-markdown target/bench/startup.md

echo ""
echo "=== Hot-path benchmark (rtk git log) ==="
hyperfine --warmup 5 --min-runs 20 -N -i \
  'target/release/rtk git log -20' \
  'target/release/rtk-pgo git log -20' \
  --export-markdown target/bench/gitlog.md

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
