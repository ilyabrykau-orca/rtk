//! Filters `bun test` output to show only failures.
//!
//! Three-tier parsing strategy:
//! - Tier 1: JUnit XML (injected via `--reporter=junit`)
//! - Tier 2: Regex on default text output
//! - Tier 3: Passthrough (truncated raw output)

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;

use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::{package_manager_exec, strip_ansi};
use crate::parser::{
    emit_degradation_warning, emit_passthrough_warning, truncate_passthrough, FormatMode,
    OutputParser, ParseResult, TestFailure, TestResult, TokenFormatter,
};

// ── XML helpers (mirrors dotnet_trx.rs) ─────────────────────────────────────

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

fn extract_attr(
    reader: &Reader<&[u8]>,
    start: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> Option<String> {
    for attr in start.attributes().flatten() {
        if local_name(attr.key.as_ref()) != key {
            continue;
        }
        if let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) {
            return Some(value.into_owned());
        }
    }
    None
}

fn parse_usize_attr(
    reader: &Reader<&[u8]>,
    start: &quick_xml::events::BytesStart<'_>,
    key: &[u8],
) -> usize {
    extract_attr(reader, start, key)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

// ── Tier 1: JUnit XML ────────────────────────────────────────────────────────

fn parse_junit_xml(xml: &str) -> Option<TestResult> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut total = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut duration_ms: Option<u64> = None;
    let mut failures: Vec<TestFailure> = Vec::new();
    let mut saw_testsuites = false;

    // State for tracking current testcase
    let mut current_case_name: Option<String> = None;
    let mut current_case_file: Option<String> = None;
    let mut in_testcase = false;
    let mut case_has_failure = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => match local_name(e.name().as_ref()) {
                b"testsuites" => {
                    saw_testsuites = true;
                    total = parse_usize_attr(&reader, e, b"tests");
                    failed = parse_usize_attr(&reader, e, b"failures");
                    skipped = parse_usize_attr(&reader, e, b"skipped");
                    // time is a float in seconds
                    if let Some(t) = extract_attr(&reader, e, b"time") {
                        if let Ok(secs) = t.parse::<f64>() {
                            duration_ms = Some((secs * 1000.0) as u64);
                        }
                    }
                }
                b"testcase" => {
                    in_testcase = true;
                    case_has_failure = false;
                    current_case_name = extract_attr(&reader, e, b"name");
                    current_case_file = extract_attr(&reader, e, b"file");
                }
                b"failure" if in_testcase => {
                    case_has_failure = true;
                }
                _ => {}
            },
            Ok(Event::Empty(ref e)) => match local_name(e.name().as_ref()) {
                b"testsuites" => {
                    saw_testsuites = true;
                    total = parse_usize_attr(&reader, e, b"tests");
                    failed = parse_usize_attr(&reader, e, b"failures");
                    skipped = parse_usize_attr(&reader, e, b"skipped");
                    if let Some(t) = extract_attr(&reader, e, b"time") {
                        if let Ok(secs) = t.parse::<f64>() {
                            duration_ms = Some((secs * 1000.0) as u64);
                        }
                    }
                }
                b"failure" if in_testcase => {
                    case_has_failure = true;
                }
                b"testcase" => {
                    // Self-closing testcase (no children) — treat as passed
                    current_case_name = extract_attr(&reader, e, b"name");
                    current_case_file = extract_attr(&reader, e, b"file");
                    // No failure child, so nothing to record
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                if local_name(e.name().as_ref()) == b"testcase" && in_testcase {
                    if case_has_failure {
                        failures.push(TestFailure {
                            test_name: current_case_name.clone().unwrap_or_default(),
                            file_path: current_case_file.clone().unwrap_or_default(),
                            error_message: String::from("AssertionError"),
                            stack_trace: None,
                        });
                    }
                    in_testcase = false;
                    case_has_failure = false;
                    current_case_name = None;
                    current_case_file = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    if !saw_testsuites {
        return None;
    }

    let passed = total.saturating_sub(failed + skipped);
    Some(TestResult {
        total,
        passed,
        failed,
        skipped,
        duration_ms,
        failures,
    })
}

// ── Tier 2: Regex on text output ─────────────────────────────────────────────

lazy_static::lazy_static! {
    static ref PASS_RE: Regex = Regex::new(r"(\d+)\s+pass").unwrap();
    static ref FAIL_RE: Regex = Regex::new(r"(\d+)\s+fail").unwrap();
    static ref RAN_RE: Regex =
        Regex::new(r"Ran\s+(\d+)\s+tests.*\[(\d+\.?\d*)(ms|s)\]").unwrap();
    static ref FAIL_NAME_RE: Regex = Regex::new(r"\(fail\)\s+(.+?)\s+\[").unwrap();
    static ref ERROR_RE: Regex = Regex::new(r"^error:").unwrap();
}

fn parse_text_output(output: &str) -> Option<TestResult> {
    let clean = strip_ansi(output);

    let passed = PASS_RE
        .captures(&clean)
        .and_then(|c| c[1].parse::<usize>().ok())
        .unwrap_or(0);

    let failed = FAIL_RE
        .captures(&clean)
        .and_then(|c| c[1].parse::<usize>().ok())
        .unwrap_or(0);

    // Must have found at least one count line to be useful
    if passed == 0 && failed == 0 && !clean.contains(" pass") {
        return None;
    }

    let (total, duration_ms) = RAN_RE
        .captures(&clean)
        .map(|c| {
            let n = c[1].parse::<usize>().unwrap_or(passed + failed);
            let val: f64 = c[2].parse().unwrap_or(0.0);
            let ms = if &c[3] == "ms" {
                val as u64
            } else {
                (val * 1000.0) as u64
            };
            (n, Some(ms))
        })
        .unwrap_or((passed + failed, None));

    let failures = extract_failures_from_text(&clean);

    Some(TestResult {
        total,
        passed,
        failed,
        skipped: 0,
        duration_ms,
        failures,
    })
}

fn extract_failures_from_text(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let len = lines.len();
    let mut i = 0;

    while i < len {
        // Detect `(fail) <name> [Xms]` marker
        if let Some(caps) = FAIL_NAME_RE.captures(lines[i]) {
            let test_name = caps[1].trim().to_string();

            // Walk backwards from current position to find the matching `error:` block.
            // The error lines precede the `(fail)` line.
            let mut error_lines: Vec<String> = Vec::new();
            let mut error_start = i.saturating_sub(1);
            while error_start > 0 {
                let candidate = lines[error_start];
                if ERROR_RE.is_match(candidate) {
                    break;
                }
                if FAIL_NAME_RE.is_match(candidate) {
                    // We've hit a previous (fail) block — don't go further
                    error_start = error_start.saturating_add(1);
                    break;
                }
                error_start = error_start.saturating_sub(1);
            }

            // Collect from error_start up to (but not including) current (fail) line
            for line in lines[error_start..i].iter() {
                let l = line.trim();
                if !l.is_empty() {
                    error_lines.push(l.to_string());
                }
            }

            failures.push(TestFailure {
                test_name,
                file_path: String::new(),
                error_message: error_lines.join("\n"),
                stack_trace: None,
            });
        }
        i += 1;
    }

    // Deduplicate and pick the best entries using FAIL_NAME_RE captures only
    // (walk once in order): already done above
    failures
}

// ── Parser struct ─────────────────────────────────────────────────────────────

pub struct BunTestParser;

impl OutputParser for BunTestParser {
    type Output = TestResult;

    fn parse(input: &str) -> ParseResult<TestResult> {
        // Tier 1: JUnit XML
        if let Some(result) = parse_junit_xml(input) {
            return ParseResult::Full(result);
        }

        // Tier 2: Text regex
        if let Some(result) = parse_text_output(input) {
            return ParseResult::Degraded(
                result,
                vec!["XML parse failed; using regex tier".to_string()],
            );
        }

        // Tier 3: Passthrough
        ParseResult::Passthrough(truncate_passthrough(input))
    }
}

// ── run() entry point ─────────────────────────────────────────────────────────

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let mut cmd = package_manager_exec("bun");
    cmd.arg("test").arg("--reporter=junit");

    for arg in args {
        // Skip args we inject or that conflict with our setup
        if arg == "test"
            || arg.starts_with("--reporter")
            || arg.starts_with("--watch")
            || arg == "--hot"
        {
            continue;
        }
        cmd.arg(arg);
    }

    let result = exec_capture(&mut cmd).context("Failed to run bun test")?;
    let combined = result.combined();

    // bun writes JUnit XML to stdout; stderr has human text
    let parse_result = BunTestParser::parse(&result.stdout);
    let mode = FormatMode::from_verbosity(verbose);

    let filtered = match parse_result {
        ParseResult::Full(data) => {
            if verbose > 0 {
                eprintln!("bun test (Tier 1: JUnit XML parse)");
            }
            data.format(mode)
        }
        ParseResult::Degraded(data, warnings) => {
            if verbose > 0 {
                emit_degradation_warning("bun test", &warnings.join(", "));
            }
            data.format(mode)
        }
        ParseResult::Passthrough(raw) => {
            emit_passthrough_warning("bun test", "All parsing tiers failed");
            raw
        }
    };

    if let Some(hint) =
        crate::core::tee::tee_and_hint(&combined, "bun_test", result.exit_code)
    {
        println!("{}\n{}", filtered, hint);
    } else {
        println!("{}", filtered);
    }

    timer.track(
        "bun test",
        "rtk bun test",
        &combined,
        &filtered,
    );

    if !result.success() {
        return Ok(result.exit_code);
    }
    Ok(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    // ── Tier 1: JUnit XML ────────────────────────────────────────────────────

    #[test]
    fn test_bun_junit_all_pass() {
        let xml = include_str!("../../../tests/fixtures/bun_test_junit_pass.xml");
        let result = BunTestParser::parse(xml);

        assert_eq!(result.tier(), 1, "Should be Tier 1 (JUnit XML)");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 3);
        assert_eq!(data.passed, 3);
        assert_eq!(data.failed, 0);
        assert_eq!(data.skipped, 0);
        assert!(data.failures.is_empty());
        // Duration: 0.008253s → ~8ms
        assert!(
            data.duration_ms.unwrap_or(0) > 0,
            "Duration should be parsed"
        );
    }

    #[test]
    fn test_bun_junit_with_failures() {
        let xml = include_str!("../../../tests/fixtures/bun_test_junit_fail.xml");
        let result = BunTestParser::parse(xml);

        assert_eq!(result.tier(), 1, "Should be Tier 1 (JUnit XML)");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 3);
        assert_eq!(data.passed, 1);
        assert_eq!(data.failed, 2);
        assert_eq!(data.failures.len(), 2);

        let names: Vec<&str> = data.failures.iter().map(|f| f.test_name.as_str()).collect();
        assert!(names.contains(&"fails - wrong value"), "Expected 'fails - wrong value'");
        assert!(names.contains(&"fails - type error"), "Expected 'fails - type error'");
    }

    // ── Tier 2: Regex text ───────────────────────────────────────────────────

    #[test]
    fn test_bun_regex_all_pass() {
        let text = include_str!("../../../tests/fixtures/bun_test_text_pass.txt");
        let result = BunTestParser::parse(text);

        assert_eq!(result.tier(), 2, "Should fall to Tier 2 (regex) for text output");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 3);
        assert_eq!(data.failed, 0);
        assert!(data.failures.is_empty());
        assert_eq!(data.duration_ms, Some(10));
    }

    #[test]
    fn test_bun_regex_with_failures() {
        let text = include_str!("../../../tests/fixtures/bun_test_text_fail.txt");
        let result = BunTestParser::parse(text);

        assert_eq!(result.tier(), 2, "Should fall to Tier 2 (regex) for text output");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 1);
        assert_eq!(data.failed, 2);
        assert_eq!(data.failures.len(), 2);

        let names: Vec<&str> = data.failures.iter().map(|f| f.test_name.as_str()).collect();
        assert!(names.contains(&"fails - wrong value"), "Missing 'fails - wrong value'");
        assert!(names.contains(&"fails - type error"), "Missing 'fails - type error'");
    }

    // ── Tier 3: Passthrough ──────────────────────────────────────────────────

    #[test]
    fn test_bun_passthrough_garbage() {
        let garbage = "totally random text that is not bun output at all!!!";
        let result = BunTestParser::parse(garbage);

        assert_eq!(result.tier(), 3, "Garbage input should be Tier 3 (passthrough)");
        assert!(!result.is_ok());
    }

    // ── Format: all-pass snapshot ────────────────────────────────────────────

    #[test]
    fn test_bun_format_all_pass() {
        let xml = include_str!("../../../tests/fixtures/bun_test_junit_pass.xml");
        let data = BunTestParser::parse(xml).unwrap();
        let output = data.format(FormatMode::Compact);

        assert!(
            output.contains("PASS (3)"),
            "Compact output must show pass count; got: {}",
            output
        );
        assert!(
            output.contains("FAIL (0)"),
            "Compact output must show fail count; got: {}",
            output
        );
        // All-pass: no extra failure detail lines
        assert!(
            !output.contains("AssertionError"),
            "All-pass output should not mention AssertionError; got: {}",
            output
        );
    }

    // ── Format: failures snapshot ────────────────────────────────────────────

    #[test]
    fn test_bun_format_with_failures() {
        let xml = include_str!("../../../tests/fixtures/bun_test_junit_fail.xml");
        let data = BunTestParser::parse(xml).unwrap();
        let output = data.format(FormatMode::Compact);

        assert!(
            output.contains("FAIL (2)"),
            "Must show 2 failures; got: {}",
            output
        );
        assert!(
            output.contains("fails - wrong value") || output.contains("fails - type error"),
            "Must name at least one failed test; got: {}",
            output
        );
    }

    // ── Token savings ────────────────────────────────────────────────────────

    #[test]
    fn test_bun_token_savings() {
        let xml = include_str!("../../../tests/fixtures/bun_test_junit_fail.xml");
        let data = BunTestParser::parse(xml).unwrap();
        let output = data.format(FormatMode::Compact);

        let input_tokens = count_tokens(xml);
        let output_tokens = count_tokens(&output);

        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "Expected ≥60% token savings, got {:.1}% (in={}, out={})",
            savings,
            input_tokens,
            output_tokens
        );
    }
}
