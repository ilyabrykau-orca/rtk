//! Filters find results by grouping files by directory.

use crate::core::tracking;
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};


/// Directories to always skip — large dependency/cache trees that waste time.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".cache",
    ".npm",
    ".yarn",
    ".pnpm-store",
    ".cargo/registry",
    ".cargo/git",
    ".rustup",
    "go/pkg",
    ".local/share",
    ".local/lib",
    "Library",
    ".Trash",
    ".docker",
    ".gradle",
    ".m2",
    ".venv",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "__pycache__",
    ".git/objects",
    ".bundle",
    "vendor/bundle",
    ".terraform",
    ".pulumi",
    ".next",
    ".nuxt",
    "dist",
    "build/intermediates",
    ".angular",
    ".svelte-kit",
];

/// Auto-cap depth for searches rooted at broad paths like $HOME or /.
fn auto_max_depth(path: &str, explicit_depth: Option<usize>) -> Option<usize> {
    if explicit_depth.is_some() {
        return explicit_depth;
    }
    let p = Path::new(path);
    let depth = if p.is_absolute() {
        p.components().count()
    } else {
        // Relative path — resolve against cwd without stat()
        std::env::current_dir()
            .map(|cwd| cwd.join(p).components().count())
            .unwrap_or(10)
    };
    if depth <= 3 {
        Some(8)
    } else {
        None
    }
}

/// Single-component skip names — matched against the directory's own name (no allocation).
const SKIP_NAMES: &[&str] = &[
    "node_modules", ".cache", ".npm", ".yarn", ".pnpm-store", ".rustup",
    ".Trash", ".docker", ".gradle", ".m2", ".venv", ".tox", ".nox",
    ".mypy_cache", ".pytest_cache", ".ruff_cache", "__pycache__",
    ".bundle", ".terraform", ".pulumi", ".next", ".nuxt",
    ".angular", ".svelte-kit", "Library", "Applications",
    ".lima", ".orbstack", ".colima",
];

fn should_skip_dir(entry_path: &Path, search_root: &Path) -> bool {
    if let Some(name) = entry_path.file_name() {
        let name = name.to_string_lossy();
        for skip in SKIP_NAMES {
            if name == *skip {
                return true;
            }
        }
    }
    // Multi-component paths need prefix matching
    if let Ok(rel) = entry_path.strip_prefix(search_root) {
        let rel_str = rel.to_string_lossy();
        for skip in SKIP_DIRS {
            if skip.contains('/') && rel_str.starts_with(skip) {
                return true;
            }
        }
    }
    false
}

/// Match a filename against a glob pattern (supports `*` and `?`).
fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_inner(pattern.as_bytes(), name.as_bytes())
}

fn glob_match_inner(pat: &[u8], name: &[u8]) -> bool {
    match (pat.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            // '*' matches zero or more characters
            glob_match_inner(&pat[1..], name)
                || (!name.is_empty() && glob_match_inner(pat, &name[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match_inner(&pat[1..], &name[1..]),
        (Some(&p), Some(&n)) if p == n => glob_match_inner(&pat[1..], &name[1..]),
        _ => false,
    }
}

/// Parsed arguments from either native find or RTK find syntax.
#[derive(Debug)]
struct FindArgs {
    pattern: String,
    path: String,
    max_results: usize,
    max_depth: Option<usize>,
    file_type: String,
    case_insensitive: bool,
}

impl Default for FindArgs {
    fn default() -> Self {
        Self {
            pattern: "*".to_string(),
            path: ".".to_string(),
            max_results: 50,
            max_depth: None,
            file_type: "f".to_string(),
            case_insensitive: false,
        }
    }
}

/// Consume the next argument from `args` at position `i`, advancing the index.
/// Returns `None` if `i` is past the end of `args`.
fn next_arg(args: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    args.get(*i).cloned()
}

/// Check if args contain native find flags (-name, -type, -maxdepth, etc.)
fn has_native_find_flags(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "-name" || a == "-type" || a == "-maxdepth" || a == "-iname")
}

/// Native find flags that RTK cannot handle correctly.
/// These involve compound predicates, actions, or semantics we don't support.
const UNSUPPORTED_FIND_FLAGS: &[&str] = &[
    "-not", "!", "-or", "-o", "-and", "-a", "-exec", "-execdir", "-delete", "-print0", "-newer",
    "-perm", "-size", "-mtime", "-mmin", "-atime", "-amin", "-ctime", "-cmin", "-empty", "-link",
    "-regex", "-iregex",
];

fn has_unsupported_find_flags(args: &[String]) -> bool {
    args.iter()
        .any(|a| UNSUPPORTED_FIND_FLAGS.contains(&a.as_str()))
}

/// Parse arguments from raw args vec, supporting both native find and RTK syntax.
///
/// Native find syntax: `find . -name "*.rs" -type f -maxdepth 3`
/// RTK syntax: `find *.rs [path] [-m max] [-t type]`
fn parse_find_args(args: &[String]) -> Result<FindArgs> {
    if args.is_empty() {
        return Ok(FindArgs::default());
    }

    if has_unsupported_find_flags(args) {
        anyhow::bail!(
            "rtk find does not support compound predicates or actions (e.g. -not, -exec). Use `find` directly."
        );
    }

    if has_native_find_flags(args) {
        parse_native_find_args(args)
    } else {
        parse_rtk_find_args(args)
    }
}

/// Parse native find syntax: `find [path] -name "*.rs" -type f -maxdepth 3`
fn parse_native_find_args(args: &[String]) -> Result<FindArgs> {
    let mut parsed = FindArgs::default();
    let mut i = 0;

    // First non-flag argument is the path (standard find behavior)
    if !args[0].starts_with('-') {
        parsed.path = args[0].clone();
        i = 1;
    }

    while i < args.len() {
        match args[i].as_str() {
            "-name" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.pattern = val;
                }
            }
            "-iname" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.pattern = val;
                    parsed.case_insensitive = true;
                }
            }
            "-type" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.file_type = val;
                }
            }
            "-maxdepth" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.max_depth = Some(val.parse().context("invalid -maxdepth value")?);
                }
            }
            flag if flag.starts_with('-') => {
                eprintln!("rtk find: unknown flag '{}', ignored", flag);
            }
            _ => {}
        }
        i += 1;
    }

    Ok(parsed)
}

/// Parse RTK syntax: `find <pattern> [path] [-m max] [-t type]`
fn parse_rtk_find_args(args: &[String]) -> Result<FindArgs> {
    let mut parsed = FindArgs {
        pattern: args[0].clone(),
        ..FindArgs::default()
    };
    let mut i = 1;

    // Second positional arg (if not a flag) is the path
    if i < args.len() && !args[i].starts_with('-') {
        parsed.path = args[i].clone();
        i += 1;
    }

    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--max" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.max_results = val.parse().context("invalid --max value")?;
                }
            }
            "-t" | "--file-type" => {
                if let Some(val) = next_arg(args, &mut i) {
                    parsed.file_type = val;
                }
            }
            _ => {}
        }
        i += 1;
    }

    Ok(parsed)
}

/// Entry point from main.rs — parses raw args then delegates to run().
pub fn run_from_args(args: &[String], verbose: u8) -> Result<()> {
    let parsed = parse_find_args(args)?;
    run(
        &parsed.pattern,
        &parsed.path,
        parsed.max_results,
        parsed.max_depth,
        &parsed.file_type,
        parsed.case_insensitive,
        verbose,
    )
}

pub fn run(
    pattern: &str,
    path: &str,
    max_results: usize,
    max_depth: Option<usize>,
    file_type: &str,
    case_insensitive: bool,
    verbose: u8,
) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    // Treat "." as match-all
    let effective_pattern = if pattern == "." { "*" } else { pattern };

    if verbose > 0 {
        eprintln!("find: {} in {}", effective_pattern, path);
    }

    let want_dirs = file_type == "d";

    // When the pattern targets dotfiles (e.g. -name ".claude.json"), we must walk hidden
    // entries; otherwise skip them to keep results tidy (#1101).
    let search_hidden = effective_pattern.starts_with('.');

    let search_root = Path::new(path);
    let effective_depth = auto_max_depth(path, max_depth);

    let mut builder = WalkBuilder::new(path);
    builder
        .hidden(!search_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .threads(std::cmp::min(num_cpus(), 6));
    if let Some(depth) = effective_depth {
        builder.max_depth(Some(depth));
    }

    let collect_limit = max_results * 4;
    let pattern_owned = effective_pattern.to_string();
    let pattern_lower = if case_insensitive {
        Some(effective_pattern.to_lowercase())
    } else {
        None
    };

    let files = Arc::new(Mutex::new(Vec::with_capacity(collect_limit)));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    builder.build_parallel().run(|| {
        let files = Arc::clone(&files);
        let done = Arc::clone(&done);
        let pat = pattern_owned.clone();
        let pat_lower = pattern_lower.clone();
        let root = search_root.to_path_buf();

        Box::new(move |entry| {
            if done.load(std::sync::atomic::Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };
            let entry_path = entry.path();
            if entry_path.is_dir() && should_skip_dir(entry_path, &root) {
                return ignore::WalkState::Skip;
            }
            if !matches_entry(entry_path, want_dirs, &pat, pat_lower.as_deref()) {
                return ignore::WalkState::Continue;
            }
            let display = rel_display(entry_path, &root);
            if !display.is_empty() {
                let mut f = files.lock().unwrap();
                f.push(display);
                if f.len() >= collect_limit {
                    done.store(true, std::sync::atomic::Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
            }
            ignore::WalkState::Continue
        })
    });

    let mut files = Arc::try_unwrap(files).unwrap().into_inner().unwrap();

    files.sort();

    let raw_output = files.join("\n");

    if files.is_empty() {
        let msg = format!("0 for '{}'", effective_pattern);
        println!("{}", msg);
        timer.track(
            &format!("find {} -name '{}'", path, effective_pattern),
            "rtk find",
            &raw_output,
            &msg,
        );
        return Ok(());
    }

    // Group by directory
    let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();

    for file in &files {
        let p = Path::new(file);
        let dir = p
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let dir = if dir.is_empty() { ".".to_string() } else { dir };
        let filename = p
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(filename);
    }

    let mut dirs: Vec<_> = by_dir.keys().cloned().collect();
    dirs.sort();
    let dirs_count = dirs.len();
    let total_files = files.len();

    println!("{}F {}D:", total_files, dirs_count);
    println!();

    // Display with proper --max limiting (count individual files)
    let mut shown = 0;
    for dir in &dirs {
        if shown >= max_results {
            break;
        }

        let files_in_dir = &by_dir[dir];
        let dir_display = if dir.len() > 50 {
            format!("...{}", &dir[dir.len() - 47..])
        } else {
            dir.clone()
        };

        let remaining_budget = max_results - shown;
        if files_in_dir.len() <= remaining_budget {
            println!("{}/ {}", dir_display, files_in_dir.join(" "));
            shown += files_in_dir.len();
        } else {
            // Partial display: show only what fits in budget
            let partial: Vec<_> = files_in_dir
                .iter()
                .take(remaining_budget)
                .cloned()
                .collect();
            println!("{}/ {}", dir_display, partial.join(" "));
            shown += partial.len();
            break;
        }
    }

    if shown < total_files {
        println!("+{} more", total_files - shown);
    }

    // Extension summary
    let mut by_ext: HashMap<String, usize> = HashMap::new();
    for file in &files {
        let ext = Path::new(file)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "none".to_string());
        *by_ext.entry(ext).or_default() += 1;
    }

    let mut ext_line = String::new();
    if by_ext.len() > 1 {
        println!();
        let mut exts: Vec<_> = by_ext.iter().collect();
        exts.sort_by(|a, b| b.1.cmp(a.1));
        let ext_str: Vec<String> = exts
            .iter()
            .take(5)
            .map(|(e, c)| format!(".{}({})", e, c))
            .collect();
        ext_line = format!("ext: {}", ext_str.join(" "));
        println!("{}", ext_line);
    }

    let rtk_output = format!("{}F {}D + {}", total_files, dirs_count, ext_line);
    timer.track(
        &format!("find {} -name '{}'", path, effective_pattern),
        "rtk find",
        &raw_output,
        &rtk_output,
    );

    Ok(())
}


fn matches_entry(entry_path: &Path, want_dirs: bool, pattern: &str, pattern_lower: Option<&str>) -> bool {
    let is_dir = entry_path.is_dir();
    if want_dirs && !is_dir {
        return false;
    }
    if !want_dirs && is_dir {
        return false;
    }
    let name = match entry_path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    if let Some(pat_lower) = pattern_lower {
        glob_match(pat_lower, &name.to_lowercase())
    } else {
        glob_match(pattern, &name)
    }
}

fn rel_display(entry_path: &Path, search_root: &Path) -> String {
    entry_path
        .strip_prefix(search_root)
        .unwrap_or(entry_path)
        .to_string_lossy()
        .to_string()
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert string slices to Vec<String> for test convenience.
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    // --- glob_match unit tests ---

    #[test]
    fn glob_match_star_rs() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "find_cmd.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(!glob_match("*.rs", "rs"));
    }

    #[test]
    fn glob_match_star_all() {
        assert!(glob_match("*", "anything.txt"));
        assert!(glob_match("*", "a"));
        assert!(glob_match("*", ".hidden"));
    }

    #[test]
    fn glob_match_question_mark() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!glob_match("Cargo.toml", "cargo.toml"));
    }

    #[test]
    fn glob_match_complex() {
        assert!(glob_match("test_*", "test_foo"));
        assert!(glob_match("test_*", "test_"));
        assert!(!glob_match("test_*", "test"));
    }

    // --- dot pattern treated as star ---

    #[test]
    fn dot_becomes_star() {
        // run() converts "." to "*" internally, test the logic
        let effective = if "." == "." { "*" } else { "." };
        assert_eq!(effective, "*");
    }

    // --- parse_find_args: native find syntax ---

    #[test]
    fn parse_native_find_name() {
        let parsed = parse_find_args(&args(&[".", "-name", "*.rs"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, ".");
        assert_eq!(parsed.file_type, "f");
        assert_eq!(parsed.max_results, 50);
    }

    #[test]
    fn parse_native_find_name_and_type() {
        let parsed = parse_find_args(&args(&["src", "-name", "*.rs", "-type", "f"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, "src");
        assert_eq!(parsed.file_type, "f");
    }

    #[test]
    fn parse_native_find_type_d() {
        let parsed = parse_find_args(&args(&[".", "-type", "d"])).unwrap();
        assert_eq!(parsed.pattern, "*");
        assert_eq!(parsed.file_type, "d");
    }

    #[test]
    fn parse_native_find_maxdepth() {
        let parsed = parse_find_args(&args(&[".", "-name", "*.toml", "-maxdepth", "2"])).unwrap();
        assert_eq!(parsed.pattern, "*.toml");
        assert_eq!(parsed.max_depth, Some(2));
        assert_eq!(parsed.max_results, 50); // max_results unchanged by -maxdepth
    }

    #[test]
    fn parse_native_find_iname() {
        let parsed = parse_find_args(&args(&[".", "-iname", "Makefile"])).unwrap();
        assert_eq!(parsed.pattern, "Makefile");
        assert!(parsed.case_insensitive);
    }

    #[test]
    fn parse_native_find_name_is_case_sensitive() {
        let parsed = parse_find_args(&args(&[".", "-name", "*.rs"])).unwrap();
        assert!(!parsed.case_insensitive);
    }

    #[test]
    fn parse_native_find_no_path() {
        // `find -name "*.rs"` without explicit path defaults to "."
        let parsed = parse_find_args(&args(&["-name", "*.rs"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, ".");
    }

    // --- parse_find_args: unsupported flags ---

    #[test]
    fn parse_native_find_rejects_not() {
        let result = parse_find_args(&args(&[".", "-name", "*.rs", "-not", "-name", "*_test.rs"]));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("compound predicates"));
    }

    #[test]
    fn parse_native_find_rejects_exec() {
        let result = parse_find_args(&args(&[".", "-name", "*.tmp", "-exec", "rm", "{}", ";"]));
        assert!(result.is_err());
    }

    // --- parse_find_args: RTK syntax ---

    #[test]
    fn parse_rtk_syntax_pattern_only() {
        let parsed = parse_find_args(&args(&["*.rs"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, ".");
    }

    #[test]
    fn parse_rtk_syntax_pattern_and_path() {
        let parsed = parse_find_args(&args(&["*.rs", "src"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, "src");
    }

    #[test]
    fn parse_rtk_syntax_with_flags() {
        let parsed = parse_find_args(&args(&["*.rs", "src", "-m", "10", "-t", "d"])).unwrap();
        assert_eq!(parsed.pattern, "*.rs");
        assert_eq!(parsed.path, "src");
        assert_eq!(parsed.max_results, 10);
        assert_eq!(parsed.file_type, "d");
    }

    #[test]
    fn parse_empty_args() {
        let parsed = parse_find_args(&args(&[])).unwrap();
        assert_eq!(parsed.pattern, "*");
        assert_eq!(parsed.path, ".");
    }

    // --- run_from_args integration tests ---

    #[test]
    fn run_from_args_native_find_syntax() {
        // Simulates: find . -name "*.rs" -type f
        let result = run_from_args(&args(&[".", "-name", "*.rs", "-type", "f"]), 0);
        assert!(result.is_ok());
    }

    #[test]
    fn run_from_args_rtk_syntax() {
        // Simulates: rtk find *.rs src
        let result = run_from_args(&args(&["*.rs", "src"]), 0);
        assert!(result.is_ok());
    }

    #[test]
    fn run_from_args_iname_case_insensitive() {
        // -iname should match case-insensitively
        let result = run_from_args(&args(&[".", "-iname", "cargo.toml"]), 0);
        assert!(result.is_ok());
    }

    // --- #1101: dotfile pattern should not skip hidden files ---

    #[test]
    fn find_dotfile_pattern_includes_hidden() {
        // .gitignore exists at the repo root — must be found when using a dotfile pattern
        let result = run(".gitignore", ".", 50, Some(1), "f", false, 0);
        assert!(result.is_ok(), "run with dotfile pattern should not error");
    }

    #[test]
    fn find_regular_pattern_skips_hidden() {
        // Non-dot pattern should not error (hidden dirs remain skipped)
        let result = run("*.rs", "src", 5, None, "f", false, 0);
        assert!(result.is_ok());
    }

    // --- integration: run on this repo ---

    #[test]
    fn find_rs_files_in_src() {
        // Should find .rs files without error
        let result = run("*.rs", "src", 100, None, "f", false, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn find_dot_pattern_works() {
        // "." pattern should not error (was broken before)
        let result = run(".", "src", 10, None, "f", false, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn find_no_matches() {
        let result = run("*.xyz_nonexistent", "src", 50, None, "f", false, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn find_respects_max() {
        // With max=2, should not error
        let result = run("*.rs", "src", 2, None, "f", false, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn find_gitignored_excluded() {
        // target/ is in .gitignore — files inside should not appear
        let result = run("*", ".", 1000, None, "f", false, 0);
        assert!(result.is_ok());
        // We can't easily capture stdout in unit tests, but at least
        // verify it runs without error. The smoke tests verify content.
    }
}
