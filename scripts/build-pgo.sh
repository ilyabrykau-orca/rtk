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
#
# target-cpu=native is applied via RUSTFLAGS (per-profile rustflags in
# .cargo/config.toml remains unstable as of Rust 1.94).

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

export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"

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
