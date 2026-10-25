//! Ratchet on wall-clock budgets in the TypeScript SDK tests.
//!
//! The TypeScript twin of `timing_deadline_ratchet`: a tight real-time budget
//! handed to the client under test races the host's scheduling, and widening it
//! never held (#494, #590). A test whose deadline is the behavior under test
//! drives it with fake timers (`jest.useFakeTimers()`) or a gate the fixture
//! holds; an incidental bound derives from the suite's hang guard.
//!
//! This test counts, per file under `sdk/ts/**/test/**/*.ts`, a numeric literal
//! below `pohunek_test_support::wait::HANG_GUARD` in milliseconds, or a
//! module-level `const NAME = <literal>` naming one, when it is:
//! - the value of a `requestTimeoutMs:` or `timeoutMs:` property;
//! - the value of `ms:` in an object literal that also holds `kind: "delay"`
//!   (the mock daemon's delayed reply step);
//! - the second argument of `setTimeout(..)`.
//!
//! A literal is an integer with `_` separators. A computed value, a parameter
//! and a constant that is not a plain integer literal are not literals.
//!
//! Counts are compared with `sdk_timing_baseline.txt` next to this file (one
//! `path count` pair per line). A file whose count rises above its baseline
//! fails, and so does a file whose count fell below it: the baseline only goes
//! down.
//!
//! Not counted:
//! - a literal inside a `test(..)`/`it(..)` callback between a `useFakeTimers(`
//!   call and the next `useRealTimers(` call (or the end of the callback): the
//!   clock is virtual there. A literal before the call, such as a budget given
//!   to a client that is built first, and one after the restore are counted;
//! - a literal on a line that carries, or follows a line that carries, a line
//!   comment starting `// timing-allowed: #<issue> <reason>`. A malformed
//!   marker is itself a failure. Text inside a block comment or string is never
//!   a marker.
//!
//! After lowering counts, rewrite the baseline with
//! `cargo nextest run -p xtask --run-ignored only -E 'test(regenerate_sdk_timing_baseline)'`.
//! The regeneration test refuses to raise any file above the committed
//! baseline; it only creates a baseline from scratch when none exists.

// Rust guideline compliant 2026-10-01

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use pohunek_test_support::wait::HANG_GUARD;
use regex::Regex;
use xtask::ratchet::{
    end_of_ident, format_baseline, is_allowed, is_valid_marker, matching_close, parse_baseline,
    starts_word, top_level_arguments,
};

/// Workspace-relative path of the committed baseline.
const BASELINE_PATH: &str = "crates/xtask/tests/sdk_timing_baseline.txt";

/// Workspace-relative directory whose TypeScript tests are scanned.
const SDK_ROOT: &str = "sdk/ts";

/// Directory component that marks a test file.
const TEST_DIR: &str = "test";

/// Directories the scan never enters.
const SKIPPED_DIRS: [&str; 2] = ["node_modules", "dist"];

/// Comment keyword that exempts a budget (after `// `).
const MARKER: &str = "timing-allowed";

/// Properties whose numeric value is a request budget in milliseconds.
const BUDGET_PROPERTIES: [&str; 2] = ["requestTimeoutMs", "timeoutMs"];

/// Header written at the top of the baseline file.
const BASELINE_HEADER: &str =
    "# Ratchet baseline for tests/sdk_timing_ratchet.rs: wall-clock budgets below\n\
# the hang guard in the TypeScript SDK tests, per file.\n\
# Format: `<workspace-relative path> <count>`. Counts may only go down;\n\
# regenerate with the ignored `regenerate_sdk_timing_baseline` test.\n";

/// Which construct carries the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    RequestBudget,
    DelayFixture,
    SetTimeout,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::RequestBudget => "request budget",
            Self::DelayFixture => "delay fixture",
            Self::SetTimeout => "setTimeout",
        }
    }
}

/// One counted literal.
#[derive(Debug, PartialEq, Eq)]
struct Occurrence {
    path: String,
    line: usize,
    kind: Kind,
}

/// A `timing-allowed` comment that does not name an issue and a reason.
#[derive(Debug, PartialEq, Eq)]
struct MalformedMarker {
    path: String,
    line: usize,
}

/// Result of scanning a set of files.
#[derive(Debug, Default)]
struct Scan {
    counted: Vec<Occurrence>,
    malformed: Vec<MalformedMarker>,
}

/// TypeScript text with comments and string contents blanked.
struct Lexed {
    /// Same length and newlines as the text; comments blanked, string and
    /// template contents blanked, quotes kept.
    skeleton: String,
    /// Byte offsets of the `//` that begin a line comment.
    line_comments: Vec<usize>,
}

/// Whether a `/` after `previous` (the last significant byte) starts a regular
/// expression literal rather than a division.
fn starts_regex(previous: Option<u8>) -> bool {
    previous.is_none_or(|byte| b"(,=:[!&|?{};+-*%<>~^".contains(&byte))
}

/// Blanks comments, strings, templates and regular expressions of `text`.
fn lex(text: &str) -> Lexed {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut line_comments = Vec::new();
    let blank = |out: &mut [u8], from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut previous: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if bytes[i..].starts_with(b"//") {
            line_comments.push(i);
            let end = bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |n| i + n);
            blank(&mut out, i, end);
            i = end;
        } else if bytes[i..].starts_with(b"/*") {
            let end = text[i + 2..]
                .find("*/")
                .map_or(bytes.len(), |n| i + 2 + n + 2);
            blank(&mut out, i, end);
            i = end;
        } else if byte == b'\'' || byte == b'"' {
            let mut end = i + 1;
            while end < bytes.len() && bytes[end] != byte && bytes[end] != b'\n' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            let end = end.min(bytes.len());
            blank(&mut out, i + 1, end);
            i = (end + 1).min(bytes.len());
            previous = Some(byte);
        } else if byte == b'`' {
            let mut end = i + 1;
            let mut depth = 0_usize;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end += 1,
                    b'$' if bytes.get(end + 1) == Some(&b'{') => {
                        depth += 1;
                        end += 1;
                    }
                    b'}' if depth > 0 => depth -= 1,
                    b'`' if depth == 0 => break,
                    _ => {}
                }
                end += 1;
            }
            let end = end.min(bytes.len());
            blank(&mut out, i + 1, end);
            i = (end + 1).min(bytes.len());
            previous = Some(b'`');
        } else if byte == b'/' && starts_regex(previous) {
            let mut end = i + 1;
            let mut in_class = false;
            while end < bytes.len() && bytes[end] != b'\n' {
                match bytes[end] {
                    b'\\' => end += 1,
                    b'[' => in_class = true,
                    b']' => in_class = false,
                    b'/' if !in_class => break,
                    _ => {}
                }
                end += 1;
            }
            let end = end.min(bytes.len());
            blank(&mut out, i + 1, end);
            i = (end + 1).min(bytes.len());
            previous = Some(b'/');
        } else {
            if !byte.is_ascii_whitespace() {
                previous = Some(byte);
            }
            i += 1;
        }
    }
    Lexed {
        skeleton: String::from_utf8(out).unwrap_or_default(),
        line_comments,
    }
}

/// One-based line of the byte `offset`.
fn line_of(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(1, |head| head.matches('\n').count() + 1)
}

/// Value of an integer literal with `_` separators.
fn int_literal(text: &str) -> Option<u128> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None;
    }
    if !text.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    text.replace('_', "").parse().ok()
}

/// Module-level `const NAME = <integer literal>;` values of the skeleton.
fn literal_constants(skeleton: &str) -> BTreeMap<String, u128> {
    let pattern = Regex::new(
        r"(?m)^(?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*(?::\s*number\s*)?=\s*([0-9][0-9_]*)\s*;",
    )
    .expect("constant pattern compiles");
    pattern
        .captures_iter(skeleton)
        .filter_map(|capture| {
            let value = int_literal(&capture[2])?;
            Some((capture[1].to_owned(), value))
        })
        .collect()
}

/// Millisecond value of `expression` when it is a literal or a literal
/// constant, positive and below the hang guard.
fn budget_ms(expression: &str, constants: &BTreeMap<String, u128>) -> Option<u128> {
    let expression = expression.trim();
    let value = int_literal(expression).or_else(|| constants.get(expression).copied())?;
    (value > 0 && value < HANG_GUARD.as_millis()).then_some(value)
}

/// End of the value expression that starts at `start`: the first top-level
/// `,`, `;` or closing bracket.
fn value_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(start) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b',' | b';' if depth == 0 => return i,
            _ => {}
        }
    }
    bytes.len()
}

/// Offset of the first non-space byte of `expression`, which starts at `start`.
fn value_start(expression: &str, start: usize) -> usize {
    start + (expression.len() - expression.trim_start().len())
}

/// `<property>: <literal>` occurrences of `property`, as (offset of the value,
/// offset of the property name, value text).
fn property_values<'a>(skeleton: &'a str, property: &str) -> Vec<(usize, usize, &'a str)> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for (offset, _) in skeleton.match_indices(property) {
        let end = offset + property.len();
        if !starts_word(bytes, offset)
            || end_of_ident(bytes, offset) != end
            || (offset > 0 && bytes[offset - 1] == b'.')
        {
            continue;
        }
        let mut colon = end;
        while bytes.get(colon).is_some_and(u8::is_ascii_whitespace) {
            colon += 1;
        }
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let start = colon + 1;
        let expression = &skeleton[start..value_end(bytes, start)];
        found.push((value_start(expression, start), offset, expression));
    }
    found
}

/// Start of the innermost `{` that encloses `offset`, and the offset of its
/// matching `}`.
fn enclosing_object(skeleton: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = skeleton.as_bytes();
    let mut depth = 0_usize;
    for open in (0..offset).rev() {
        match bytes[open] {
            b'}' => depth += 1,
            b'{' if depth == 0 => return Some((open, matching_close(bytes, open))),
            b'{' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Byte ranges of the `test(..)`/`it(..)` callbacks in which the clock is fake:
/// from each `useFakeTimers(` call to the next `useRealTimers(` call, or to the
/// end of the callback.
fn fake_clock_ranges(skeleton: &str) -> Vec<(usize, usize)> {
    let bytes = skeleton.as_bytes();
    let call =
        Regex::new(r"\b(?:test|it)(?:\.[A-Za-z]+)*\s*\(").expect("test call pattern compiles");
    let mut ranges = Vec::new();
    for found in call.find_iter(skeleton) {
        let open = found.end() - 1;
        let close = matching_close(bytes, open).min(skeleton.len());
        let body = &skeleton[open..close];
        for (fake, _) in body.match_indices("useFakeTimers(") {
            let end = body[fake..]
                .find("useRealTimers(")
                .map_or(close, |real| open + fake + real);
            ranges.push((open + fake, end));
        }
    }
    ranges
}

/// Scans `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> Scan {
    let mut scan = Scan::default();
    let delay_kind = Regex::new(r#"kind\s*:\s*["']delay["']"#).expect("delay pattern compiles");
    for (path, text) in files {
        let lexed = lex(text);
        let skeleton = &lexed.skeleton;
        let bytes = skeleton.as_bytes();

        let mut valid = Vec::new();
        for &at in &lexed.line_comments {
            let rest = &text[at + 2..];
            let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
            if rest.starts_with(['/', '!']) {
                continue;
            }
            if let Some(body) = rest.trim_start().strip_prefix(MARKER) {
                let line = line_of(text, at);
                if is_valid_marker(body) {
                    valid.push(line);
                } else {
                    scan.malformed.push(MalformedMarker {
                        path: path.clone(),
                        line,
                    });
                }
            }
        }

        let constants = literal_constants(skeleton);
        let mut candidates: Vec<(usize, Kind)> = Vec::new();
        for property in BUDGET_PROPERTIES {
            for (value, _, expression) in property_values(skeleton, property) {
                if budget_ms(expression, &constants).is_some() {
                    candidates.push((value, Kind::RequestBudget));
                }
            }
        }
        for (value, name, expression) in property_values(skeleton, "ms") {
            let in_delay = enclosing_object(skeleton, name).is_some_and(|(open, close)| {
                delay_kind.is_match(&text[open..close.min(text.len())])
            });
            if in_delay && budget_ms(expression, &constants).is_some() {
                candidates.push((value, Kind::DelayFixture));
            }
        }
        for (offset, _) in skeleton.match_indices("setTimeout") {
            let end = offset + "setTimeout".len();
            if !starts_word(bytes, offset) || end_of_ident(bytes, offset) != end {
                continue;
            }
            let mut open = end;
            while bytes.get(open).is_some_and(u8::is_ascii_whitespace) {
                open += 1;
            }
            if bytes.get(open) != Some(&b'(') {
                continue;
            }
            let close = matching_close(bytes, open);
            let body = &skeleton[open + 1..close];
            let arguments = top_level_arguments(body);
            if let Some(delay) = arguments.get(1) {
                let start = open + 1 + arguments[0].len() + 1;
                if budget_ms(delay, &constants).is_some() {
                    candidates.push((value_start(delay, start), Kind::SetTimeout));
                }
            }
        }

        let fake = fake_clock_ranges(skeleton);
        candidates.sort_by_key(|&(offset, _)| offset);
        let mut seen = BTreeSet::new();
        for (offset, kind) in candidates {
            if !seen.insert(offset) {
                continue;
            }
            let line = line_of(text, offset);
            let on_fake_clock = fake
                .iter()
                .any(|&(open, close)| open < offset && offset < close);
            if !on_fake_clock && !is_allowed(&valid, line) {
                scan.counted.push(Occurrence {
                    path: path.clone(),
                    line,
                    kind,
                });
            }
        }
    }
    scan
}

/// Counted occurrences per file.
fn counts(scan: &Scan) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for occurrence in &scan.counted {
        *counts.entry(occurrence.path.clone()).or_insert(0) += 1;
    }
    counts
}

/// Failure messages for `scan` measured against `baseline`; empty when the
/// ratchet holds.
fn evaluate(scan: &Scan, baseline: &BTreeMap<String, usize>) -> Vec<String> {
    let mut failures = Vec::new();
    for marker in &scan.malformed {
        failures.push(format!(
            "{}:{}: malformed timing marker; write `// {MARKER}: #<issue> <reason>` with an \
             issue number and a non-empty reason",
            marker.path, marker.line
        ));
    }
    let current = counts(scan);
    for (path, &count) in &current {
        let allowed = baseline.get(path).copied().unwrap_or(0);
        if count > allowed {
            let mut message = format!(
                "{path}: {count} wall-clock budget(s) below the hang guard in SDK tests, baseline allows {allowed}:\n"
            );
            for occurrence in scan.counted.iter().filter(|o| &o.path == path) {
                let _ = writeln!(
                    message,
                    "  {}:{}: {}",
                    occurrence.path,
                    occurrence.line,
                    occurrence.kind.label()
                );
            }
            message.push_str(
                "  Fix: drive the deadline with `jest.useFakeTimers()` and an explicit \
                 `advanceTimersByTime`, hold the reply behind a `gate` step the test releases, or \
                 derive an incidental bound from the suite's hang guard. Never widen the budget. \
                 A budget that is the policy under test takes a \
                 `// timing-allowed: #<issue> <reason>` comment on or above the line.",
            );
            failures.push(message);
        }
    }
    for (path, &allowed) in baseline {
        let count = current.get(path).copied().unwrap_or(0);
        if count < allowed {
            failures.push(format!(
                "{path}: {count} budget(s) found but the baseline allows {allowed}; lower the \
                 baseline to {count} (the ratchet only goes down). Rewrite it with `cargo \
                 nextest run -p xtask --run-ignored only -E \
                 'test(regenerate_sdk_timing_baseline)'`."
            ));
        }
    }
    failures
}

fn baseline_file() -> std::path::PathBuf {
    pohunek_test_support::workspace_root().join(BASELINE_PATH)
}

/// Reads every `.ts` file below `dir` that sits under a `test` directory.
fn collect_sources(
    root: &Path,
    dir: &Path,
    in_test_dir: bool,
    out: &mut BTreeMap<String, String>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if SKIPPED_DIRS.contains(&name) {
                continue;
            }
            collect_sources(root, &path, in_test_dir || name == TEST_DIR, out)?;
        } else if in_test_dir && path.extension().is_some_and(|ext| ext == "ts") {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(relative, fs::read_to_string(&path)?);
        }
    }
    Ok(())
}

fn scan_workspace() -> Scan {
    let root = pohunek_test_support::workspace_root();
    let mut files = BTreeMap::new();
    collect_sources(&root, &root.join(SDK_ROOT), false, &mut files)
        .unwrap_or_else(|e| panic!("read {SDK_ROOT}: {e}"));
    scan_files(&files)
}

#[test]
fn sdk_tests_stay_within_the_timing_baseline() {
    let text =
        fs::read_to_string(baseline_file()).unwrap_or_else(|e| panic!("read {BASELINE_PATH}: {e}"));
    let failures = evaluate(&scan_workspace(), &parse_baseline(&text));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "rewrites the committed baseline; run it after removing wall-clock budgets"]
fn regenerate_sdk_timing_baseline() {
    let scan = scan_workspace();
    assert!(
        scan.malformed.is_empty(),
        "fix malformed timing markers first: {:?}",
        scan.malformed
    );
    let current = counts(&scan);
    if let Ok(text) = fs::read_to_string(baseline_file()) {
        let committed = parse_baseline(&text);
        let raised: Vec<_> = current
            .iter()
            .filter(|&(path, &count)| count > committed.get(path).copied().unwrap_or(0))
            .map(|(path, _)| path.as_str())
            .collect();
        assert!(
            raised.is_empty(),
            "the baseline only goes down; remove the new budgets in {raised:?}"
        );
    }
    fs::write(baseline_file(), format_baseline(BASELINE_HEADER, &current))
        .unwrap_or_else(|e| panic!("write {BASELINE_PATH}: {e}"));
}

/// Scan of one file at `path`.
fn scan_one(path: &str, text: &str) -> Scan {
    scan_files(&BTreeMap::from([(path.to_owned(), text.to_owned())]))
}

const DEMO: &str = "sdk/ts/demo/test/demo.test.ts";

fn lines_and_kinds(scan: &Scan) -> Vec<(usize, Kind)> {
    scan.counted.iter().map(|o| (o.line, o.kind)).collect()
}

/// A test callback whose body is `statements`.
fn in_test(statements: &str) -> String {
    format!("test(\"case\", async () => {{\n{statements}\n}});\n")
}

#[test]
fn a_new_request_budget_fails_the_ratchet_with_a_fix_hint() {
    let text =
        in_test("  const client = await connect(daemon, undefined, { requestTimeoutMs: 20 });");
    let failures = evaluate(&scan_one(DEMO, &text), &BTreeMap::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("sdk/ts/demo/test/demo.test.ts:2: request budget"));
    assert!(failures[0].contains("useFakeTimers"));
    assert!(failures[0].contains("Never widen"));
    assert!(failures[0].contains("timing-allowed"));
}

#[test]
fn request_budget_properties_take_literals_and_literal_constants() {
    let counted = [
        "connect(d, undefined, { requestTimeoutMs: 20 });",
        "connect(d, undefined, { timeoutMs: 1_000 });",
        "connect(d, undefined, { requestTimeoutMs: SHORT_MS });",
        "connect(d, undefined, {\n    requestTimeoutMs:\n      119_999,\n  });",
        "connect(d, undefined, { requestTimeoutMs : 5 });",
    ];
    for statement in counted {
        let text = format!(
            "const SHORT_MS = 250;\n{}",
            in_test(&format!("  {statement}"))
        );
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{statement}");
        assert_eq!(scan.counted[0].kind, Kind::RequestBudget, "{statement}");
    }
    let ignored = [
        "connect(d, undefined, { requestTimeoutMs: 120_000 });",
        "connect(d, undefined, { requestTimeoutMs: LONG_MS });",
        "connect(d, undefined, { requestTimeoutMs: COMPUTED_MS });",
        "connect(d, undefined, { requestTimeoutMs: base * 2 });",
        "connect(d, undefined, { requestTimeoutMs });",
        "connect(d, undefined, { requestTimeoutMs: options.timeout });",
        "connect(d, undefined, { otherTimeoutMs: 20 });",
        "connect(d, undefined, { timeout_ms: 20 });",
        "const options: { timeoutMs?: number } = {};",
        "client.timeoutMs = 20;",
        "const s = \"{ requestTimeoutMs: 20 }\";",
        "// connect(d, undefined, { requestTimeoutMs: 20 });",
        "/* connect(d, undefined, { requestTimeoutMs: 20 }); */",
    ];
    for statement in ignored {
        let text = format!(
            "const LONG_MS = 600_000;\nconst COMPUTED_MS = 2 * 10;\n{}",
            in_test(&format!("  {statement}"))
        );
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn a_delay_fixture_counts_only_inside_a_delay_step() {
    let counted = [
        "const steps = [{ kind: \"delay\", ms: 60, line: reply }];",
        "const steps = [{ ms: 60, kind: 'delay', line: reply }];",
        "const steps = [{\n    kind: \"delay\",\n    ms: DELAY_MS,\n    line: reply,\n  }];",
    ];
    for statement in counted {
        let text = format!(
            "const DELAY_MS = 1_500;\n{}",
            in_test(&format!("  {statement}"))
        );
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{statement}");
        assert_eq!(scan.counted[0].kind, Kind::DelayFixture, "{statement}");
    }
    let ignored = [
        "const steps = [{ kind: \"delay\", ms: 0, line: reply }];",
        "const steps = [{ kind: \"delay\", ms: 120_000, line: reply }];",
        "const steps = [{ kind: \"reply\", ms: 60, line: reply }];",
        "const steps = [{ kind: \"delay\", line: reply }]; const other = { ms: 60 };",
        "const window = { ms: 60 };",
        "const steps = [{ kind: \"gate\", ready, line: reply }];",
    ];
    for statement in ignored {
        let text = in_test(&format!("  {statement}"));
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn the_second_argument_of_set_timeout_is_counted() {
    let counted = [
        "setTimeout(resolve, 50);",
        "setTimeout(() => { done(); }, 1_000);",
        "setTimeout(resolve, WAIT_MS);",
        "globalThis.setTimeout(resolve, 50);",
        "setTimeout(\n    resolve,\n    50,\n  );",
    ];
    for statement in counted {
        let text = format!(
            "const WAIT_MS = 75;\n{}",
            in_test(&format!("  {statement}"))
        );
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{statement}");
        assert_eq!(scan.counted[0].kind, Kind::SetTimeout, "{statement}");
    }
    let ignored = [
        "setTimeout(resolve, 0);",
        "setTimeout(resolve, 120_000);",
        "setTimeout(resolve, ms);",
        "setTimeout(resolve);",
        "setTimeout(() => done(50), delay);",
        "mySetTimeout(resolve, 50);",
        "clearTimeout(timer);",
        "const s = \"setTimeout(resolve, 50)\";",
        "// setTimeout(resolve, 50);",
    ];
    for statement in ignored {
        let text = in_test(&format!("  {statement}"));
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn a_test_on_fake_timers_is_exempt() {
    let fake = "test(\"case\", async () => {\n  jest.useFakeTimers();\n  const c = await connect(d, undefined, { requestTimeoutMs: 1_000 });\n  setTimeout(resolve, 50);\n});\n";
    assert!(scan_one(DEMO, fake).counted.is_empty());
    let it = fake.replace("test(", "it(");
    assert!(scan_one(DEMO, &it).counted.is_empty());
    let neighbour = format!(
        "{fake}test(\"real\", async () => {{\n  const c = await connect(d, undefined, {{ requestTimeoutMs: 1_000 }});\n}});\n"
    );
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, &neighbour)),
        [(7, Kind::RequestBudget)]
    );
    let outside = "const c = await connect(d, undefined, { requestTimeoutMs: 1_000 });\ntest(\"case\", async () => {\n  jest.useFakeTimers();\n});\n";
    assert_eq!(scan_one(DEMO, outside).counted.len(), 1);
    let helper = "function setup() {\n  jest.useFakeTimers();\n  return connect(d, undefined, { requestTimeoutMs: 1_000 });\n}\ntest(\"case\", async () => {\n  await setup();\n});\n";
    assert_eq!(scan_one(DEMO, helper).counted.len(), 1);
}

#[test]
fn only_the_stretch_between_fake_and_real_timers_is_exempt() {
    let before = "test(\"case\", async () => {\n  const c = await connect(d, undefined, { requestTimeoutMs: 20 });\n  jest.useFakeTimers();\n});\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, before)),
        [(2, Kind::RequestBudget)]
    );
    let after = "test(\"case\", async () => {\n  jest.useFakeTimers();\n  try {\n    await first();\n  } finally {\n    jest.useRealTimers();\n  }\n  const c = await connect(d, undefined, { requestTimeoutMs: 20 });\n});\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, after)),
        [(8, Kind::RequestBudget)]
    );
    let between = "test(\"case\", async () => {\n  jest.useFakeTimers();\n  try {\n    const c = await connect(d, undefined, { requestTimeoutMs: 20 });\n  } finally {\n    jest.useRealTimers();\n  }\n});\n";
    assert!(scan_one(DEMO, between).counted.is_empty());
    let again = "test(\"case\", async () => {\n  jest.useFakeTimers();\n  jest.useRealTimers();\n  connect(d, undefined, { requestTimeoutMs: 20 });\n  jest.useFakeTimers();\n  connect(d, undefined, { requestTimeoutMs: 30 });\n});\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, again)),
        [(4, Kind::RequestBudget)]
    );
}

#[test]
fn a_valid_marker_on_or_above_the_line_exempts_the_literal() {
    let forms = [
        "test(\"c\", async () => {\n  connect(d, undefined, { requestTimeoutMs: 20 }); // timing-allowed: #590 the policy under test\n});\n",
        "test(\"c\", async () => {\n  // timing-allowed: #590 the policy under test\n  connect(d, undefined, { requestTimeoutMs: 20 });\n});\n",
        "test(\"c\", async () => {\n  //timing-allowed:#590 reason\n  connect(d, undefined, { requestTimeoutMs: 20 });\n});\n",
    ];
    for text in forms {
        let scan = scan_one(DEMO, text);
        assert!(
            scan.counted.is_empty() && scan.malformed.is_empty(),
            "{text}"
        );
    }
    let two_lines_above = "test(\"c\", async () => {\n  // timing-allowed: #590 reason\n\n  connect(d, undefined, { requestTimeoutMs: 20 });\n});\n";
    assert_eq!(scan_one(DEMO, two_lines_above).counted.len(), 1);
}

#[test]
fn a_malformed_marker_is_a_failure_and_does_not_exempt() {
    let forms = [
        "// timing-allowed\n",
        "// timing-allowed:\n",
        "// timing-allowed: #590\n",
        "// timing-allowed: 590 reason\n",
        "// timing-allowed: #abc reason\n",
        "// timing-allowed #590 reason\n",
    ];
    for marker in forms {
        let text = format!(
            "test(\"c\", async () => {{\n  {marker}  connect(d, undefined, {{ requestTimeoutMs: 20 }});\n}});\n"
        );
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{marker}");
        let failures = evaluate(&scan, &BTreeMap::from([(DEMO.to_owned(), 1)]));
        assert_eq!(failures.len(), 1, "{marker}: {failures:?}");
        assert!(failures[0].contains("malformed timing marker"), "{marker}");
        assert!(failures[0].contains("demo.test.ts:2"), "{marker}");
    }
}

#[test]
fn marker_text_in_docs_strings_and_block_comments_is_not_a_marker() {
    let text = concat!(
        "/* // timing-allowed: broken */\n",
        "/// timing-allowed: broken\n",
        "const s = \"// timing-allowed: broken\";\n",
        "const t = `\n// timing-allowed: broken\n`;\n",
        "// see the timing-allowed: broken marker format\n",
    );
    assert!(scan_one(DEMO, text).malformed.is_empty());
    let inside = "test(\"c\", async () => {\n  /* // timing-allowed: #590 reason */\n  connect(d, undefined, { requestTimeoutMs: 20 });\n});\n";
    let scan = scan_one(DEMO, inside);
    assert!(scan.malformed.is_empty());
    assert_eq!(scan.counted.len(), 1);
}

#[test]
fn strings_templates_and_regular_expressions_do_not_confuse_the_scan() {
    let text = concat!(
        "const quote = /[\"']/;\n",
        "const url = `http://x/${a}//${b}`;\n",
        "const apostrophe = \"it's\";\n",
        "test(\"c\", async () => {\n",
        "  connect(d, undefined, { requestTimeoutMs: 20 });\n",
        "});\n",
    );
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, text)),
        [(5, Kind::RequestBudget)]
    );
}

#[test]
fn counts_below_the_baseline_fail_with_a_lower_the_baseline_message() {
    let text = in_test("  connect(d, undefined, { requestTimeoutMs: 20 });");
    let scan = scan_one(DEMO, &text);
    let equal = BTreeMap::from([(DEMO.to_owned(), 1)]);
    assert!(evaluate(&scan, &equal).is_empty());

    let above = BTreeMap::from([(DEMO.to_owned(), 2)]);
    let failures = evaluate(&scan, &above);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("lower the baseline to 1"),
        "{failures:?}"
    );
    assert!(failures[0].contains("only goes down"), "{failures:?}");

    let gone = BTreeMap::from([("sdk/ts/demo/test/gone.test.ts".to_owned(), 3)]);
    let failures = evaluate(&scan, &gone);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures
        .iter()
        .any(|f| f.contains("gone.test.ts") && f.contains("lower the baseline to 0")));
    assert!(failures.iter().any(|f| f.contains("baseline allows 0")));
}

#[test]
fn a_file_missing_from_the_baseline_may_not_gain_budgets() {
    let text =
        in_test("  connect(d, undefined, { requestTimeoutMs: 20 });\n  setTimeout(resolve, 50);");
    let scan = scan_one(DEMO, &text);
    let other = BTreeMap::from([("sdk/ts/other/test/other.test.ts".to_owned(), 0)]);
    let failures = evaluate(&scan, &other);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("demo.test.ts:2"), "{failures:?}");
    assert!(failures[0].contains("demo.test.ts:3"), "{failures:?}");
    assert!(failures[0].contains("baseline allows 0"), "{failures:?}");
}

#[test]
fn only_typescript_files_under_a_test_directory_are_read() {
    let root = pohunek_test_support::tempdir().expect("temp dir");
    for (path, text) in [
        ("sdk/test/a.ts", "x"),
        ("sdk/test/nested/b.ts", "x"),
        ("sdk/src/c.ts", "x"),
        ("sdk/test/d.js", "x"),
        ("sdk/test/node_modules/e.ts", "x"),
    ] {
        let file = root.path().join(path);
        fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
        fs::write(file, text).expect("write file");
    }
    let mut files = BTreeMap::new();
    collect_sources(root.path(), &root.path().join("sdk"), false, &mut files).expect("collect");
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        ["sdk/test/a.ts", "sdk/test/nested/b.ts"]
    );
}

#[test]
fn the_committed_baseline_is_sorted_and_has_no_zero_entries() {
    let text = fs::read_to_string(baseline_file()).expect("baseline exists");
    let baseline = parse_baseline(&text);
    assert!(baseline.values().all(|&count| count > 0), "zero entry");
    assert_eq!(
        text,
        format_baseline(BASELINE_HEADER, &baseline),
        "baseline is canonical"
    );
}
