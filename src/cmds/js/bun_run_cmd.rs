//! Filters bun run output — strips boilerplate, keeps script output.

use crate::core::runner;
use crate::core::utils::resolved_command;
use anyhow::Result;

/// Known bun subcommands that should NOT get "run" injected.
/// Shared between production code and tests to avoid drift.
const BUN_SUBCOMMANDS: &[&str] = &[
    "install",
    "i",
    "add",
    "remove",
    "rm",
    "update",
    "upgrade",
    "link",
    "unlink",
    "pm",
    "create",
    "init",
    "build",
    "dev",
    "test",
    "repl",
    "completions",
    "help",
    "version",
    "-v",
    "--version",
];

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("bun");

    // Determine if this is "bun run <script>" or another bun subcommand (install, add, etc.)
    // Only inject "run" when args look like a script name, not a known bun subcommand.
    let first_arg = args.first().map(|s| s.as_str());
    let is_run_explicit = first_arg == Some("run");
    let is_bun_subcommand = first_arg
        .map(|a| BUN_SUBCOMMANDS.contains(&a) || a.starts_with('-'))
        .unwrap_or(false);

    let effective_args = if is_run_explicit {
        // "rtk bunx run build" → "bun run build"
        cmd.arg("run");
        &args[1..]
    } else if is_bun_subcommand {
        // "rtk bun run add express" → "bun add express"
        args
    } else {
        // "rtk bun run build" → "bun run build" (assume script name)
        cmd.arg("run");
        args
    };

    for arg in effective_args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: bun {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "bun",
        &args.join(" "),
        filter_bun_output,
        runner::RunOptions::default(),
    )
}

/// Filter bun run output - strip boilerplate, progress bars, command echoes
fn filter_bun_output(output: &str) -> String {
    let mut result = Vec::new();

    for line in output.lines() {
        // Skip bun command echo (lines starting with $)
        if line.starts_with('$') {
            continue;
        }
        // Skip bun boilerplate (> pkg@version)
        if line.starts_with('>') && line.contains('@') {
            continue;
        }
        // Skip warn/WARN lines
        if line.trim_start().starts_with("warn") || line.trim_start().starts_with("WARN") {
            continue;
        }
        // Skip progress indicators
        if line.contains('⸩') || line.contains('⸨') {
            continue;
        }
        // Skip empty lines
        if line.trim().is_empty() {
            continue;
        }

        result.push(line.to_string());
    }

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_bun_output_strips_boilerplate() {
        let output = r#"$ bun run build
> project@1.0.0 build
> next build

warn deprecated package inflight@1.0.6
WARN some other warning
⸩ loading...

   Creating an optimized production build...
   Build completed successfully
"#;
        let result = filter_bun_output(output);
        assert!(!result.contains("$ bun run"), "Should strip $ command echo");
        assert!(!result.contains("> project@"), "Should strip > pkg@version");
        assert!(!result.contains("warn deprecated"), "Should strip warn lines");
        assert!(!result.contains("WARN some"), "Should strip WARN lines");
        assert!(!result.contains('⸩'), "Should strip progress indicators");
        assert!(result.contains("Build completed"), "Should keep real output");
    }

    #[test]
    fn test_filter_bun_output_empty_returns_ok() {
        let output = "\n\n\n";
        let result = filter_bun_output(output);
        assert_eq!(result, "ok");
    }

    #[test]
    fn test_filter_bun_output_preserves_real_output() {
        let output = r#"$ bun run lint
Checking 42 files...
Found 0 errors. Linting completed.
"#;
        let result = filter_bun_output(output);
        assert!(!result.contains("$ bun run"), "Should strip command echo");
        assert!(result.contains("Checking 42 files"), "Should preserve real output");
        assert!(result.contains("Linting completed"), "Should preserve result line");
    }

    #[test]
    fn test_bun_subcommand_routing() {
        // Uses the shared BUN_SUBCOMMANDS constant — no drift between prod and test
        fn needs_run_injection(args: &[&str]) -> bool {
            let first = args.first().copied();
            let is_run_explicit = first == Some("run");
            let is_subcommand = first
                .map(|a| BUN_SUBCOMMANDS.contains(&a) || a.starts_with('-'))
                .unwrap_or(false);
            !is_run_explicit && !is_subcommand
        }

        // Known subcommands should NOT get "run" injected
        for subcmd in BUN_SUBCOMMANDS {
            assert!(
                !needs_run_injection(&[subcmd]),
                "'bun {}' should NOT inject 'run'",
                subcmd
            );
        }

        // Script names SHOULD get "run" injected (not listed in BUN_SUBCOMMANDS)
        for script in &["lint", "typecheck", "deploy", "start", "compile"] {
            assert!(
                needs_run_injection(&[script]),
                "'bun {}' SHOULD inject 'run'",
                script
            );
        }

        // Flags should NOT get "run" injected
        assert!(!needs_run_injection(&["--help"]));
        assert!(!needs_run_injection(&["-h"]));

        // Explicit "run" should NOT inject another "run"
        assert!(!needs_run_injection(&["run", "build"]));
    }
}
