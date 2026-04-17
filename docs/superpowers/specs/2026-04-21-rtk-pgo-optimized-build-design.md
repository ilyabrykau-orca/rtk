# rtk PGO-Optimized Local Build — Design

**Status:** Approved, ready for implementation plan
**Date:** 2026-04-21
**Target:** `rtk` Rust CLI (`~/src/rtk`), local-developer-install scope

## Goal

Produce a `cargo install`-compatible optimized build path for rtk that adds
Profile-Guided Optimization (PGO), `target-cpu=native`, and explicit link-time
tuning on top of the existing release profile, without disturbing the
portable distributable profile used by deb/rpm packaging.

Non-goals (explicitly out of scope for this spec):

- BOLT post-link optimization (bad fit: macOS-first development, experimental
  tooling, no gain on rtk's hot path)
- Multi-target CI matrix with per-target PGO profiles (deferred — distributed
  release binaries continue to use the plain `release` profile unchanged)
- `-Z build-std` / nightly-only flags (too much friction for a contributor
  tool)

## Success Criteria

- One-command PGO build: `make release-pgo` (or `./scripts/build-pgo.sh`)
- One-command native build: `make release-native`
- Baseline `cargo build --release` still produces a portable binary
  suitable for `cargo deb` / `cargo generate-rpm` — no regression in the
  distribution pipeline
- PGO-optimized binary passes verification gate:
  - startup time (`rtk --version`) ≥ baseline (no regression)
  - hot-path filter (`rtk git log -20` on fixture) ≥ 3% faster than baseline
  - binary size within ±10% of baseline
  - full `cargo test --all` passes under the PGO profile

## Current State

Existing `[profile.release]` in `rtk/Cargo.toml`:

```toml
opt-level = 3
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

Foundation is solid (fat-LTO, one codegen unit, panic=abort, symbols stripped).
Missing: PGO, per-profile rustflags for native CPU, explicit build-dep
opt-level, and a PGO-safe thin-LTO sibling profile.

## Architecture

Five small components, each independently testable:

1. **Profile config** (`Cargo.toml`) — declares three release profiles
2. **Rustflag config** (`.cargo/config.toml`) — scopes `target-cpu=native` per
   profile, never globally
3. **Training script** (`scripts/pgo-train.sh`) — feeds the instrumented
   binary a fixed, fixture-driven workload
4. **Build orchestrator** (`scripts/build-pgo.sh`) — drives `cargo-pgo`
   end-to-end
5. **Verification gate** (`scripts/bench-pgo.sh`) — hyperfine before/after
   plus size and test-suite checks

Each consumer-facing entry point is a Makefile target. The user-facing
contract is "run `make release-pgo`, get an optimized binary at
`target/release/rtk-pgo`."

## Component 1 — Profile Config

**File:** `rtk/Cargo.toml` (modify)

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

**Rationale for each knob:**

| Knob | Value | Why |
|------|-------|-----|
| `opt-level` | `3` | Max speed; size cost is acceptable given rtk is <5MB |
| `lto` | `fat` (release), `thin` (pgo) | Fat maximizes inlining across crates; thin avoids the documented LLVM PGO+fat-LTO miscompile bug |
| `codegen-units` | `1` | Enables full cross-function inlining |
| `panic` | `abort` | Saves ~200KB unwinding tables; rtk uses `std::process::exit` anyway |
| `strip` | `true` | Removes debug symbols; rtk doesn't ship debug builds |
| `overflow-checks` | `false` | Explicit (default); documents intent |
| `debug` | `false` | Explicit; ensures PGO instrumentation data is the only profiling info |
| `package."*" opt-level` | `3` | Forces build-dep code paths (e.g. proc-macro outputs) to full opt |

**Why three profiles:**

- `release` — portable, used by deb/rpm and CI. Unchanged behavior.
- `release-native` — local install with CPU-specific codegen. Fast
  alternative to full PGO.
- `release-pgo` — PGO-safe sibling; thin LTO avoids the known PGO+fat-LTO
  bug. Only used by the PGO script; users shouldn't invoke it directly.

## Component 2 — Rustflag Config

**File:** `rtk/.cargo/config.toml` (new)

```toml
[profile.release-native]
rustflags = ["-C", "target-cpu=native"]

[profile.release-pgo]
rustflags = ["-C", "target-cpu=native"]
```

Per-profile rustflags in `.cargo/config.toml` are stable as of Rust 1.74
(2023-11). If the toolchain predates that, the build script falls back to
exporting `RUSTFLAGS` for the duration of the `cargo` invocation.

**Why not global `RUSTFLAGS`:** would leak `target-cpu=native` into the
plain `release` build, poisoning distributable artifacts.

## Component 3 — Training Script

**File:** `rtk/scripts/pgo-train.sh` (new)

Executes ~25 rtk invocations driven entirely by committed test fixtures.
No live network, no live git state, no live gh auth.

**Workload composition:**

| Ecosystem | Invocations | Fixture |
|-----------|-------------|---------|
| Git | `git log -20`, `git status`, `git diff`, `gh pr view` | `tests/fixtures/git_*_raw.txt`, `gh_pr_view_raw.txt` |
| Rust | `cargo test`, `cargo clippy`, `cargo build` | `tests/fixtures/cargo_*_raw.txt` |
| JS | `pnpm list` | `tests/fixtures/pnpm_list_raw.txt` |
| Python | `pytest` | `tests/fixtures/pytest_raw.txt` |
| Go | `go test` | `tests/fixtures/go_test_raw.txt` |
| Core | `gain`, `gain --history`, `--version`, `discover` | n/a (no stdin) |
| Proxy | `proxy echo hello` | n/a |

**Isolation guarantees:**

- `RTK_DB_PATH` points at a scratch SQLite file in `$(mktemp -d)`, cleaned up
  on exit
- `XDG_CONFIG_HOME` points at a scratch dir so the training run can't read or
  mutate the developer's real `~/.config/rtk/config.toml`
- Script `set -euo pipefail`; any missing fixture is logged and skipped with
  a warning, never silently (so the training corpus can't drift without
  notice)

**Explicit exclusion:** `cargo test --profile release-pgo` is NOT part of
training. Rationale: (a) the test suite uses synthetic short inputs that
would bias the profile toward short-string code paths, (b) adds ~30s for
marginal profile coverage, (c) the curated fixture set already covers the
real hot-path shape.

## Component 4 — Build Orchestrator

**File:** `rtk/scripts/build-pgo.sh` (new)

End-to-end PGO build, idempotent, safe to re-run:

1. Preflight: ensure `cargo-pgo` installed; ensure `llvm-tools-preview`
   rustup component present
2. Clean `target/pgo-profiles/` to avoid stale profile contamination
3. `cargo pgo build -- --bin rtk --profile release-pgo`
4. Run `./scripts/pgo-train.sh` against the instrumented binary
5. `cargo pgo optimize build -- --bin rtk --profile release-pgo`
6. Copy the result to `target/release/rtk-pgo` for a stable, documented path

**Makefile additions** (`rtk/Makefile`, new file — no existing Makefile in the repo):

```make
.PHONY: release-native release-pgo bench-pgo

release-native:
	cargo install --path . --profile release-native --force

release-pgo:
	./scripts/build-pgo.sh

bench-pgo:
	./scripts/bench-pgo.sh
```

The existing `cargo install --path .` workflow documented in CLAUDE.md
continues to work (uses plain `release` profile) — the new targets are
purely additive.

## Component 5 — Verification Gate

**File:** `rtk/scripts/bench-pgo.sh` (new)

Runs after `build-pgo.sh` produces `target/release/rtk-pgo`.

**Checks:**

1. Startup benchmark via hyperfine:
   - `target/release/rtk --version` vs `target/release/rtk-pgo --version`
   - warmup 5, min-runs 30, markdown export
2. Hot-path benchmark via hyperfine:
   - `rtk git log -20` piped from `tests/fixtures/git_log_raw.txt`
   - baseline vs PGO binary
3. Binary size: `ls -l target/release/rtk target/release/rtk-pgo`
4. Token-savings sanity: `cargo test --profile release-pgo` — verifies PGO
   didn't miscompile filter logic

Results are reported; the script does not hard-fail on regression to avoid
blocking the user when PGO gains are noisy. Pass criteria are documented
in the output so the user can eyeball it.

**Pass criteria (eyeball):**

- Startup: PGO ≤ baseline + 0ms (no regression)
- Hot path: PGO ≥ 3% faster on at least one filter
- Binary size: PGO within ±10% of baseline
- `cargo test`: all pass

## File Inventory

```
rtk/
├── Cargo.toml                          # MODIFY: expand [profile.release], add 2 profiles
├── .cargo/config.toml                  # NEW: per-profile rustflags
├── Makefile                            # NEW: 3 targets
├── scripts/
│   ├── pgo-train.sh                    # NEW: ~25-invocation training workload
│   ├── build-pgo.sh                    # NEW: cargo-pgo end-to-end driver
│   └── bench-pgo.sh                    # NEW: hyperfine verification gate
├── CLAUDE.md                           # MODIFY: add "Optimized Builds" subsection
└── docs/superpowers/specs/
    └── 2026-04-21-rtk-pgo-optimized-build-design.md   # this spec
```

Six file touches total. No source code in `src/` changes.

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Missing fixture → partial training → biased profile | Script logs & counts skipped fixtures; fail build if <80% of planned invocations ran |
| `target-cpu=native` leaks into distributable build | Per-profile rustflags scoped to `release-native` and `release-pgo` only |
| PGO + fat-LTO miscompile (known LLVM bug) | `release-pgo` profile uses `lto = "thin"` |
| `cargo-pgo` not installed | `build-pgo.sh` preflight runs `cargo install cargo-pgo` on first use |
| `llvm-tools-preview` missing | Preflight runs `rustup component add` |
| Training pollutes real tracking DB | Scratch `RTK_DB_PATH` in `mktemp` dir, cleaned on EXIT trap |
| Training reads real config | Scratch `XDG_CONFIG_HOME`, same cleanup |
| Toolchain pre-dates 1.74 per-profile rustflags | Fallback: `build-pgo.sh` exports `RUSTFLAGS` around the cargo calls |

## Open Questions

None. All decisions made and captured inline.

## Implementation Plan

Next step: invoke the `superpowers:writing-plans` skill to break this design
into an ordered, testable implementation plan.
