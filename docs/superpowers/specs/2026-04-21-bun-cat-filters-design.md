# RTK: Bun + Cat Filter Support

**Date:** 2026-04-21
**Scope:** Add dedicated `bun test` and `bun run` filter modules, fix `cat` redirect handling, cleanup RTK_DISABLED bypasses, build and install.

## Overview

RTK discover shows ~650K tokens/month missed. Two high-impact gaps:

| Command | Count/mo | Est. savings | Approach |
|---------|----------|-------------|----------|
| `bun test` | 122 | ~24K tokens | New filter module (vitest pattern) |
| `bun run` | 59 | ~11K tokens | New filter module (npm pattern) |
| `bun` (bare) | 46 | ~9K tokens | Route to bun-run with auto-inject |
| `bunx` | — | — | Route to bun-run |
| `cat >` / `cat <<` | 67 | 0 (writes) | Add to IGNORED_PREFIXES |
| RTK_DISABLED bypass | 29 | ~2K tokens | Remove unnecessary bypasses |

## Architecture

Approach B: split modules. Each bun subcommand gets its own filter, matching RTK convention (jest ≠ npm).

### New Files

```
src/cmds/js/bun_test_cmd.rs   — bun test filter (3-tier: JSON → regex → passthrough)
src/cmds/js/bun_run_cmd.rs    — bun run filter (npm-style boilerplate stripping)
```

### Modified Files

```
src/cmds/js/mod.rs             — pub mod declarations
src/main.rs                    — Commands enum + dispatch
src/discover/rules.rs          — registry rules for bun test, bun run, bunx
```

## Component: `bun test` Filter

### 3-Tier Parse Strategy

Mirrors vitest_cmd.rs architecture.

**Tier 1 — Structured JSON parse:**
- Inject `--reporter=json` if user hasn't specified a reporter
- Parse bun's JSON output into `BunTestResult` struct
- Extract: total, passed, failed, skipped, duration, failure details

**Tier 2 — Regex fallback:**
- Triggered when JSON parse fails (user overrides reporter, or bun version lacks JSON reporter)
- Regex patterns for bun test text output:
  - `(\d+) pass` / `(\d+) fail` / `(\d+) skip`
  - Duration: `\((\d+\.?\d*)\s*(ms|s)\)`
- Extract failure blocks: lines after `✗` marker until next test or end

**Tier 3 — Passthrough:**
- If both tiers fail, truncate raw output to `passthrough_max_chars` (config)

### Output Format

```
Tests: 42 passed, 2 failed, 1 skipped (45 total)
Duration: 1.2s

FAILED:
  ✗ src/foo.test.ts > should handle edge case
    Expected: 42
    Received: undefined
```

All pass → one-line summary. Failures → summary + each failure with message.

### Structs

```rust
// Reuse TestResult/TestFailure from vitest_cmd for output formatting.
// BunTestJsonOutput shape deferred: must run `bun test --reporter=json`
// during implementation to capture actual schema. If JSON reporter is
// unavailable in bun, skip Tier 1 entirely — Tier 2 regex becomes primary.
```

### Implementation: Verify JSON Reporter

First implementation step: run `bun test --reporter=json` on a real test suite and capture output. This determines whether Tier 1 is viable. If bun lacks JSON reporter, remove Tier 1 and strengthen Tier 2 regex patterns. Vitest uses the same degradation path when users override `--reporter`.

## Component: `bun run` Filter

### Strategy

Copies npm_cmd.rs pattern.

**Subcommand routing:**

| Input | Action |
|-------|--------|
| `bun run <script>` | Filter output |
| `bun <script>` (not a known bun subcommand) | Auto-inject `run`, filter output |
| `bun install/add/remove/link` | Passthrough (package manager ops) |
| `bun --version` | Passthrough |
| `bunx <pkg>` | Filter output (strip boilerplate) |

**Known bun subcommands** (not auto-injected with `run`):
```
install, add, remove, link, unlink, pm, create, init,
upgrade, completions, repl, test, build, dev
```

**Filter logic:**
```rust
fn filter_bun_output(output: &str) -> String {
    // Strip:
    // - Lines starting with "$ " (bun's command echo)
    // - bun install/resolution progress lines
    // - Empty lines
    // - "bun run" prefix lines
    // Return "ok" if nothing left
}
```

### Output Format

Script output only. All bun boilerplate stripped.

## Component: `cat` Redirect Handling

**Problem:** `cat > file` and `cat << 'EOF'` are writes, not reads. They produce no stdout. Currently unhandled → show as "missed" in discover.

**Fix:** Add to `IGNORED_PREFIXES` in `src/discover/rules.rs`:
```rust
"cat >",
"cat >>",
"cat <<",
```

No changes to the `rtk read` command or its filter. `cat file` already matches the existing `^(cat|head|tail)\s+` rule and routes to `rtk read`.

## Component: Registry Rules

### New rules in `rules.rs`

```rust
RtkRule {
    pattern: r"^bun\s+test(\s|$)",
    rtk_cmd: "rtk bun-test",
    rewrite_prefixes: &["bun test"],
    category: "Tests",
    savings_pct: 95.0,
    subcmd_savings: &[],
    subcmd_status: &[],
},
RtkRule {
    pattern: r"^bun\s+(run|x|exec)(\s|$)",
    rtk_cmd: "rtk bun-run",
    rewrite_prefixes: &["bun run", "bun x", "bun exec"],
    category: "PackageManager",
    savings_pct: 70.0,
    subcmd_savings: &[],
    subcmd_status: &[],
},
RtkRule {
    pattern: r"^bunx\s+",
    rtk_cmd: "rtk bun-run",
    rewrite_prefixes: &["bunx"],
    category: "PackageManager",
    savings_pct: 70.0,
    subcmd_savings: &[],
    subcmd_status: &[],
},
// Catch-all for bare `bun <anything>` not matched above.
// MUST appear AFTER bun test rule (first-match-wins).
// bun_run_cmd.rs dispatch routes known subcommands (install, add, etc.)
// to passthrough internally.
RtkRule {
    pattern: r"^bun\s+\S",
    rtk_cmd: "rtk bun-run",
    rewrite_prefixes: &["bun"],
    category: "PackageManager",
    savings_pct: 70.0,
    subcmd_savings: &[],
    subcmd_status: &[
        ("install", RtkStatus::Passthrough),
        ("add", RtkStatus::Passthrough),
        ("remove", RtkStatus::Passthrough),
        ("link", RtkStatus::Passthrough),
        ("unlink", RtkStatus::Passthrough),
        ("pm", RtkStatus::Passthrough),
        ("create", RtkStatus::Passthrough),
        ("init", RtkStatus::Passthrough),
        ("upgrade", RtkStatus::Passthrough),
        ("completions", RtkStatus::Passthrough),
        ("repl", RtkStatus::Passthrough),
        ("dev", RtkStatus::Passthrough),
    ],
},
```

### Rule ordering

`bun test` rule MUST come before any generic `bun` rule — first match wins in the registry.

## Component: Commands Enum + Dispatch

### main.rs additions

```rust
// In Commands enum:
/// Bun test with compact output
BunTest {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
},
/// Bun run with filtered output
BunRun {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
},

// In run_cli() match:
Commands::BunTest { ref args } => {
    bun_test_cmd::run(args, cli.verbose)?
}
Commands::BunRun { args } => {
    bun_run_cmd::run(&args, cli.verbose)?
}
```

## Component: Hook Fixes

### RTK_DISABLED cleanup

29 commands ran with `RTK_DISABLED=1` unnecessarily:
- `cargo test` (20x) — RTK handles cargo test fine
- `git -C` (6x) — RTK handles git with `-C` flag
- `cargo build` (1x)
- `git --no-pager` (1x)
- `git diff` (1x)

**Root cause:** Claude Code itself or hook scripts prepend `RTK_DISABLED=1` to avoid double-rewriting in certain contexts (e.g. RTK's own test suite). These are false positives — RTK handles all these commands natively.

**Fix:** Audit `~/.claude/hooks/rtk-rewrite.sh` for `RTK_DISABLED` logic. Remove the bypass for commands that RTK already handles (cargo test/build, git -C/--no-pager/diff). Keep bypass only for commands that genuinely break under RTK (if any).

## Build + Install

```bash
cd /Users/ilyabrykau/src/rtk
cargo fmt --all && cargo clippy --all-targets && cargo test --all
cargo build --release --target aarch64-apple-darwin
cp target/aarch64-apple-darwin/release/rtk ~/.local/bin/rtk
```

## Validation

```bash
# Verify new commands work
echo '✓ test1\n✗ test2' | rtk bun-test --help
rtk bun-run --help

# Run discover to confirm gap closure
rtk discover

# Check savings
rtk gain
```

## Testing Strategy

Follow RTK's existing test patterns:
- Unit tests for filter functions with fixture data
- Integration tests via `cargo test`
- Real bun test output captured as fixtures for regression tests
- Pre-commit gate: `cargo fmt --all && cargo clippy --all-targets && cargo test --all`
