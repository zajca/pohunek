//! Ratchet on wall-clock budgets that test code passes as data.
//!
//! `timing_ratchet` sees sleeps and `.elapsed()` assertions, but not a tight
//! budget handed to the code under test: a request timeout, a stop grace, a
//! stall threshold. Such a budget races the host's scheduling exactly like a
//! sleep does, and lengthening it never held (#494, #590). When the deadline is
//! the behavior under test the test runs on virtual time or behind a barrier
//! the fixture holds, and when it is incidental the bound derives from
//! `pohunek_test_support::wait::HANG_GUARD`.
//!
//! This test counts, per file and in test code only (as classified by
//! `xtask::test_code`), a `Duration::from_{nanos,micros,millis,secs}(<literal>)`
//! below `HANG_GUARD`, or an integer literal below `HANG_GUARD` in milliseconds
//! given to a name that ends in `_ms`/`_MS`, when it is:
//! - (a) inside the first argument of `tokio::time::timeout(..)`,
//!   `time::timeout(..)`, or a bare `timeout(..)` imported from a `time` module;
//! - (b) the value of a struct field, in a struct literal (`name: value`) or an
//!   assignment (`options.name = value`), whose name contains `timeout`,
//!   `deadline`, `grace`, `budget`, `wait`, `bound`, `stall` or `limit` (any
//!   case); a `let` binding or a bare variable is not a field and is not
//!   counted;
//! - (c) the value of a `const` or `static` whose name contains one of those
//!   words.
//!
//! A literal is an integer, with `_` separators and an optional type suffix,
//! or a product of such integers (`5 * 60`); zero is not a budget. A named
//! constant, a `HANG_GUARD` expression and a computed value are not literals.
//! One literal counts once however many constructs match it.
//!
//! Counts are compared with `timing_deadline_baseline.txt` next to this file
//! (one `path count` pair per line). A file whose count rises above its
//! baseline fails, and so does a file whose count fell below it: the baseline
//! only goes down.
//!
//! Not counted:
//! - a literal inside a test item declared `#[tokio::test(.. start_paused =
//!   true ..)]`: tokio timers run on the paused clock there;
//! - a literal on a line that carries, or follows a line that carries, a line
//!   comment starting `// timing-allowed: #<issue> <reason>`. A malformed
//!   marker is reported by `timing_ratchet`; here it exempts nothing.
//!
//! After lowering counts, rewrite the baseline with
//! `cargo nextest run -p xtask --run-ignored only -E 'test(regenerate_timing_deadline_baseline)'`.
//! The regeneration test refuses to raise any file above the committed
//! baseline; it only creates a baseline from scratch when none exists.

// Rust guideline compliant 2026-10-01

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;

use pohunek_test_support::wait::HANG_GUARD;
use xtask::ratchet::{
    contains_word, end_of_ident, format_baseline, ident_before, is_allowed, markers,
    matching_close, on_virtual_time, parse_baseline, starts_word, top_level_arguments,
};
use xtask::test_code::{classify, is_ident, read_sources, skip_whitespace};

/// Workspace-relative path of the committed baseline.
const BASELINE_PATH: &str = "crates/xtask/tests/timing_deadline_baseline.txt";

/// Comment keyword that exempts a deadline literal (after `// `).
const MARKER: &str = "timing-allowed";

/// Name fragments that make a field or constant a budget (compared in lower
/// case).
const BUDGET_WORDS: [&str; 8] = [
    "timeout", "deadline", "grace", "budget", "wait", "bound", "stall", "limit",
];

/// `Duration::from_*` constructors and their length in nanoseconds.
const DURATION_UNITS: [(&str, u128); 4] = [
    ("from_nanos", 1),
    ("from_micros", 1_000),
    ("from_millis", 1_000_000),
    ("from_secs", 1_000_000_000),
];

/// Header written at the top of the baseline file.
const BASELINE_HEADER: &str =
    "# Ratchet baseline for tests/timing_deadline_ratchet.rs: wall-clock budgets\n\
# below the hang guard that test code passes as data, per file.\n\
# Format: `<workspace-relative path> <count>`. Counts may only go down;\n\
# regenerate with the ignored `regenerate_timing_deadline_baseline` test.\n";

/// Which construct carries the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    TimeoutArgument,
    BudgetField,
    BudgetConst,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::TimeoutArgument => "timeout() budget",
            Self::BudgetField => "budget field",
            Self::BudgetConst => "budget const",
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

/// Result of scanning a set of files.
#[derive(Debug, Default)]
struct Scan {
    counted: Vec<Occurrence>,
}

/// Value of an integer literal with `_` separators and an optional type
/// suffix such as `u64` or `usize`.
fn int_literal(text: &str) -> Option<u128> {
    let text = text.trim();
    let taken = text
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b'_')
        .count();
    let (digits, suffix) = text.split_at(taken);
    let digits: String = digits.chars().filter(|c| *c != '_').collect();
    let suffix_ok = suffix.is_empty()
        || (suffix.starts_with(['u', 'i'])
            && suffix[1..].bytes().all(|b| b.is_ascii_alphanumeric()));
    if !suffix_ok
        || !digits
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// Value of a product of integer literals (`5 * 60`), or `None` when any
/// factor is not a literal.
fn int_product(text: &str) -> Option<u128> {
    text.split('*').try_fold(1_u128, |product, factor| {
        product.checked_mul(int_literal(factor)?)
    })
}

/// A `Duration::from_*(<literal>)` call below the hang guard: the offset of
/// the constructor name and the end of the call.
#[derive(Debug)]
struct DurationLiteral {
    start: usize,
    end: usize,
}

/// Every `Duration::from_*(<literal>)` call of `skeleton` below the hang guard.
fn duration_literals(skeleton: &str) -> Vec<DurationLiteral> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for (offset, _) in skeleton.match_indices("from_") {
        if !starts_word(bytes, offset) {
            continue;
        }
        let end = end_of_ident(bytes, offset);
        let Some(&(_, unit)) = DURATION_UNITS
            .iter()
            .find(|(name, _)| *name == &skeleton[offset..end])
        else {
            continue;
        };
        let mut before = offset;
        while before > 0 && bytes[before - 1].is_ascii_whitespace() {
            before -= 1;
        }
        if before < 2
            || &bytes[before - 2..before] != b"::"
            || ident_before(bytes, before - 2) != "Duration"
        {
            continue;
        }
        let open = skip_whitespace(bytes, end);
        if bytes.get(open) != Some(&b'(') {
            continue;
        }
        let close = matching_close(bytes, open);
        let below_guard = int_product(&skeleton[open + 1..close])
            .and_then(|value| value.checked_mul(unit))
            .is_some_and(|nanos| 0 < nanos && nanos < HANG_GUARD.as_nanos());
        if below_guard {
            found.push(DurationLiteral {
                start: offset,
                end: close,
            });
        }
    }
    found
}

/// Whether `name` is a budget name.
fn is_budget_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BUDGET_WORDS.iter().any(|word| lower.contains(word))
}

/// Whether `name` carries a millisecond count.
fn is_millisecond_name(name: &str) -> bool {
    name.ends_with("_ms") || name.ends_with("_MS")
}

/// The literal a value expression `[start, end)` of the item `name` passes: the
/// first duration literal in it, or the whole value when it is an integer
/// below the hang guard in milliseconds and `name` counts milliseconds.
fn literal_in_value(
    skeleton: &str,
    literals: &[DurationLiteral],
    name: &str,
    start: usize,
    end: usize,
) -> Option<usize> {
    if let Some(literal) = literals
        .iter()
        .find(|literal| start <= literal.start && literal.end < end)
    {
        return Some(literal.start);
    }
    let value = &skeleton[start..end];
    let below_guard = int_product(value).is_some_and(|ms| 0 < ms && ms < HANG_GUARD.as_millis());
    (is_millisecond_name(name) && below_guard)
        .then(|| start + (value.len() - value.trim_start().len()))
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

/// Whether `time::timeout` is imported by a bare `use .. timeout ..` statement.
fn imports_bare_timeout(skeleton: &str) -> bool {
    let bytes = skeleton.as_bytes();
    skeleton.match_indices("use ").any(|(at, _)| {
        if !starts_word(bytes, at) {
            return false;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        contains_word(statement, "time") && contains_word(statement, "timeout")
    })
}

/// Literals in the first argument of a `timeout(..)` call.
fn timeout_candidates(skeleton: &str, literals: &[DurationLiteral]) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let bare = imports_bare_timeout(skeleton);
    let mut found = Vec::new();
    for (offset, _) in skeleton.match_indices("timeout") {
        let end = end_of_ident(bytes, offset);
        if !starts_word(bytes, offset) || end != offset + "timeout".len() {
            continue;
        }
        let open = skip_whitespace(bytes, end);
        if bytes.get(open) != Some(&b'(') {
            continue;
        }
        let mut before = offset;
        while before > 0 && bytes[before - 1].is_ascii_whitespace() {
            before -= 1;
        }
        let qualified = before >= 2
            && &bytes[before - 2..before] == b"::"
            && ident_before(bytes, before - 2) == "time";
        let method = before > 0 && bytes[before - 1] == b'.';
        let definition = ident_before(bytes, before) == "fn";
        let bare_call = bare && !(before >= 2 && &bytes[before - 2..before] == b"::");
        if method || definition || !(qualified || bare_call) {
            continue;
        }
        let close = matching_close(bytes, open);
        let body = &skeleton[open + 1..close];
        let first = top_level_arguments(body)[0];
        let first_end = open + 1 + first.len();
        if let Some(literal) = literals
            .iter()
            .find(|literal| open < literal.start && literal.end < first_end)
        {
            found.push((literal.start, Kind::TimeoutArgument));
        }
    }
    found
}

/// Literals given to a struct-literal field named like a budget.
fn field_candidates(skeleton: &str, literals: &[DurationLiteral]) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for (colon, _) in skeleton.match_indices(':') {
        if bytes.get(colon + 1) == Some(&b':') || (colon > 0 && bytes[colon - 1] == b':') {
            continue;
        }
        let name = ident_before(bytes, colon);
        let name_start = skeleton[..colon].trim_end().len() - name.len();
        if name.is_empty() || !is_budget_name(name) {
            continue;
        }
        if name_start > 0 && bytes[name_start - 1] == b'\'' {
            continue;
        }
        if matches!(
            ident_before(bytes, name_start),
            "let" | "mut" | "const" | "static"
        ) {
            continue;
        }
        let start = colon + 1;
        let end = value_end(bytes, start);
        if let Some(offset) = literal_in_value(skeleton, literals, name, start, end) {
            found.push((offset, Kind::BudgetField));
        }
    }
    found
}

/// Literals assigned to a field named like a budget: `options.timeout = ..;`.
fn assignment_candidates(skeleton: &str, literals: &[DurationLiteral]) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for (equals, _) in skeleton.match_indices('=') {
        let preceded_by_operator = equals > 0 && b"=!<>+-*/%&|^".contains(&bytes[equals - 1]);
        let followed_by_operator = matches!(bytes.get(equals + 1), Some(b'=' | b'>'));
        if preceded_by_operator || followed_by_operator {
            continue;
        }
        let name = ident_before(bytes, equals);
        let name_start = skeleton[..equals].trim_end().len() - name.len();
        let field_access = skeleton[..name_start].trim_end().ends_with('.');
        if name.is_empty() || !field_access || !is_budget_name(name) {
            continue;
        }
        let start = equals + 1;
        let end = value_end(bytes, start);
        if let Some(offset) = literal_in_value(skeleton, literals, name, start, end) {
            found.push((offset, Kind::BudgetField));
        }
    }
    found
}

/// Literals bound to a `const` or `static` named like a budget.
fn const_candidates(skeleton: &str, literals: &[DurationLiteral]) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for keyword in ["const", "static"] {
        for (offset, _) in skeleton.match_indices(keyword) {
            let end = offset + keyword.len();
            if !starts_word(bytes, offset) || bytes.get(end).copied().is_some_and(is_ident) {
                continue;
            }
            let mut name_start = skip_whitespace(bytes, end);
            if skeleton[name_start..].starts_with("mut ") {
                name_start = skip_whitespace(bytes, name_start + "mut".len());
            }
            let name_end = end_of_ident(bytes, name_start);
            let name = &skeleton[name_start..name_end];
            if name.is_empty() || name == "fn" || !is_budget_name(name) {
                continue;
            }
            let Some(equals) = bytes[name_end..]
                .iter()
                .position(|byte| matches!(byte, b'=' | b';' | b'{' | b'}'))
                .map(|n| name_end + n)
                .filter(|&at| bytes[at] == b'=')
            else {
                continue;
            };
            let start = equals + 1;
            let end = value_end(bytes, start);
            if let Some(literal) = literal_in_value(skeleton, literals, name, start, end) {
                found.push((literal, Kind::BudgetConst));
            }
        }
    }
    found
}

/// Scans `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> Scan {
    let mut scan = Scan::default();
    for (path, file) in classify(files) {
        let text = &files[&path];
        let (valid, _malformed) = markers(&file, text, MARKER);
        let skeleton = &file.views().skeleton;
        let literals = duration_literals(skeleton);
        let mut candidates = timeout_candidates(skeleton, &literals);
        candidates.extend(field_candidates(skeleton, &literals));
        candidates.extend(assignment_candidates(skeleton, &literals));
        candidates.extend(const_candidates(skeleton, &literals));
        candidates.sort_by_key(|&(offset, _)| offset);
        let mut seen = BTreeSet::new();
        for (offset, kind) in candidates {
            if !seen.insert(offset) {
                continue;
            }
            let line = file.line_of(offset);
            if file.in_test_code(offset)
                && !on_virtual_time(&file, offset)
                && !is_allowed(&valid, line)
            {
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
    let current = counts(scan);
    for (path, &count) in &current {
        let allowed = baseline.get(path).copied().unwrap_or(0);
        if count > allowed {
            let mut message = format!(
                "{path}: {count} wall-clock budget(s) below the hang guard in test code, baseline allows {allowed}:\n"
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
                "  Fix: run the test on virtual time (`#[tokio::test(start_paused = true)]`), hold \
                 the event behind a barrier the fixture owns, or derive an incidental bound from \
                 `pohunek_test_support::wait::HANG_GUARD`. Never lengthen the budget. A budget \
                 that is the policy under test takes a \
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
                 'test(regenerate_timing_deadline_baseline)'`."
            ));
        }
    }
    failures
}

fn baseline_file() -> std::path::PathBuf {
    pohunek_test_support::workspace_root().join(BASELINE_PATH)
}

fn scan_workspace() -> Scan {
    let files = read_sources(&pohunek_test_support::workspace_root())
        .unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    scan_files(&files)
}

#[test]
fn test_code_stays_within_the_deadline_baseline() {
    let text =
        fs::read_to_string(baseline_file()).unwrap_or_else(|e| panic!("read {BASELINE_PATH}: {e}"));
    let failures = evaluate(&scan_workspace(), &parse_baseline(&text));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "rewrites the committed baseline; run it after removing wall-clock budgets"]
fn regenerate_timing_deadline_baseline() {
    let current = counts(&scan_workspace());
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

const DEMO: &str = "crates/demo/src/lib.rs";

fn lines_and_kinds(scan: &Scan) -> Vec<(usize, Kind)> {
    scan.counted.iter().map(|o| (o.line, o.kind)).collect()
}

/// A test function whose body is `statements`.
fn in_test(statements: &str) -> String {
    format!("#[test]\nfn f() {{\n{statements}\n}}\n")
}

#[test]
fn a_new_budget_in_a_test_fails_the_ratchet_with_a_fix_hint() {
    let text = in_test(
        "    let c = Config { stop_grace: Duration::from_millis(50), ..Config::default() };",
    );
    let failures = evaluate(&scan_one(DEMO, &text), &BTreeMap::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("crates/demo/src/lib.rs:3: budget field"));
    assert!(failures[0].contains("HANG_GUARD"));
    assert!(failures[0].contains("timing-allowed"));
    assert!(failures[0].contains("Never lengthen"));
}

#[test]
fn the_first_argument_of_a_timeout_call_is_counted() {
    let counted = [
        "tokio::time::timeout(Duration::from_secs(2), fut).await;",
        "time::timeout(std::time::Duration::from_millis(250), fut).await;",
        "tokio::time::timeout(\n        Duration::from_secs(1),\n        fut,\n    ).await;",
        "tokio::time::timeout(Duration::from_secs(5 * 2), fut).await;",
        "tokio::time :: timeout (Duration::from_micros(10), fut).await;",
    ];
    for statement in counted {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert_eq!(scan.counted.len(), 1, "{statement}");
        assert_eq!(scan.counted[0].kind, Kind::TimeoutArgument, "{statement}");
    }
    let bare = format!(
        "use tokio::time::{{timeout, Duration}};\n{}",
        in_test("    timeout(Duration::from_secs(2), fut).await;")
    );
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, &bare)),
        [(4, Kind::TimeoutArgument)]
    );
    let line = in_test(
        "    tokio::time::timeout(\n        Duration::from_secs(1),\n        fut,\n    ).await;",
    );
    assert_eq!(scan_one(DEMO, &line).counted[0].line, 4);
}

#[test]
fn timeout_lookalikes_and_non_literal_budgets_are_not_counted() {
    let ignored = [
        "tokio::time::timeout(HANG_GUARD, fut).await;",
        "tokio::time::timeout(DEADLINE, fut).await;",
        "tokio::time::timeout(Duration::from_secs(120), fut).await;",
        "tokio::time::timeout(Duration::from_secs(600), fut).await;",
        "tokio::time::timeout(Duration::from_millis(budget), fut).await;",
        "tokio::time::timeout(HANG_GUARD, work(Duration::from_secs(1))).await;",
        "timeout(Duration::from_secs(1), fut).await;",
        "client.timeout(Duration::from_secs(1));",
        "other::timeout(Duration::from_secs(1), fut);",
        "let s = \"tokio::time::timeout(Duration::from_secs(1), f)\";",
        "// tokio::time::timeout(Duration::from_secs(1), f);",
        "Duration::from_secs(1);",
        "let budget = Duration::from_millis(5);",
        "let timeout: Duration = Duration::from_millis(5);",
        "tokio::time::timeout(Duration::from_secs_f64(0.5), fut).await;",
    ];
    for statement in ignored {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn the_hang_guard_boundary_and_the_unit_conversion_are_exact() {
    let seconds = HANG_GUARD.as_secs();
    let millis = HANG_GUARD.as_millis();
    let counted = [
        format!(
            "let c = C {{ timeout: Duration::from_secs({}) }};",
            seconds - 1
        ),
        format!(
            "let c = C {{ timeout: Duration::from_millis({}) }};",
            millis - 1
        ),
        "let c = C { timeout: Duration::from_micros(1) };".to_owned(),
        "let c = C { timeout: Duration::from_nanos(1) };".to_owned(),
        format!("let c = C {{ timeout: Duration::from_secs({seconds} - 1) }};"),
    ];
    let expected = [true, true, true, true, false];
    for (statement, expect_counted) in counted.iter().zip(expected) {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert_eq!(
            scan.counted.len(),
            usize::from(expect_counted),
            "{statement}"
        );
    }
    let uncounted = [
        format!("let c = C {{ timeout: Duration::from_secs({seconds}) }};"),
        format!("let c = C {{ timeout: Duration::from_millis({millis}) }};"),
        format!(
            "let c = C {{ timeout: Duration::from_millis({}) }};",
            millis + 1
        ),
        format!(
            "let c = C {{ timeout: Duration::from_micros({}) }};",
            millis * 1_000
        ),
    ];
    for statement in uncounted {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert!(scan.counted.is_empty(), "{statement}");
    }
}

#[test]
fn a_struct_field_named_like_a_budget_is_counted() {
    for name in [
        "timeout",
        "request_timeout",
        "deadline",
        "stop_grace",
        "budget",
        "session_wait",
        "output_wait",
        "poll_bound",
        "stall",
        "rate_limit",
        "TIMEOUT",
    ] {
        let text = in_test(&format!(
            "    let c = Config {{ {name}: Duration::from_millis(50), other: 1 }};"
        ));
        let scan = scan_one(DEMO, &text);
        assert_eq!(lines_and_kinds(&scan), [(3, Kind::BudgetField)], "{name}");
    }
    let wrapped = in_test("    let c = Config { timeout: Some(Duration::from_millis(50)) };");
    assert_eq!(scan_one(DEMO, &wrapped).counted.len(), 1);
    let multiline = in_test("    let c = Config {\n        a: 1,\n        stop_grace: Duration::from_millis(50),\n    };");
    assert_eq!(scan_one(DEMO, &multiline).counted[0].line, 5);
    let two = in_test(
        "    let c = Config { timeout: Duration::from_millis(1).max(Duration::from_millis(2)) };",
    );
    assert_eq!(scan_one(DEMO, &two).counted.len(), 1);
}

#[test]
fn a_millisecond_integer_given_to_a_millisecond_name_is_counted() {
    let counted = [
        "let c = Config { stall_ms: 300 };",
        "let c = Config { request_timeout_ms: 1_000 };",
        "let c = Config { wait_MS: 25u64 };",
        "let c = Config { timeout_ms: 2 * 1_000 };",
    ];
    for statement in counted {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert_eq!(
            lines_and_kinds(&scan),
            [(3, Kind::BudgetField)],
            "{statement}"
        );
    }
    let ignored = [
        "let c = Config { timeout_ms: 120_000 };",
        "let c = Config { timeout_ms: HANG_MS };",
        "let c = Config { timeout_ms: compute() };",
        "let c = Config { timeout: 300 };",
        "let c = Config { stall_ms_hint: 300 };",
        "let c = Config { retries_ms: 300 };",
        "let c = Config { max_limit: 10 };",
    ];
    for statement in ignored {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn a_field_assignment_named_like_a_budget_is_counted() {
    let counted = [
        "o.timeout = Duration::from_millis(5);",
        "self.cfg.stop_grace = Duration::from_secs(1);",
        "o.timeout = Some(Duration::from_millis(5));",
        "o.stall_ms = 300;",
        "spec.exit_timeout =\n        Duration::from_secs(1);",
        "(*o).sweep_grace = Duration::from_secs(5);",
    ];
    for statement in counted {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert_eq!(scan.counted.len(), 1, "{statement}");
        assert_eq!(scan.counted[0].kind, Kind::BudgetField, "{statement}");
    }
    let ignored = [
        "o.timeout = HANG_GUARD;",
        "o.timeout = budget;",
        "o.retries = Duration::from_millis(5);",
        "timeout = Duration::from_millis(5);",
        "if o.timeout == Duration::from_millis(5) {}",
        "let same = o.timeout >= Duration::from_millis(5);",
        "o.timeout += Duration::from_millis(5);",
        "let f = |x| o.timeout => Duration::from_millis(5);",
        "o.timeout = Duration::from_secs(120);",
        "o.timeout = Duration::ZERO;",
        "o.timeout = Duration::from_millis(0);",
        "o.stall_ms = 0;",
    ];
    for statement in ignored {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn fields_that_are_not_budgets_or_not_assignments_are_not_counted() {
    let ignored = [
        "let c = Config { retries: Duration::from_millis(50) };",
        "let c = Config { delay: Duration::from_millis(50) };",
        "let c = Config { interval: Duration::from_millis(50) };",
        "fn g(timeout: Duration) {}",
        "let timeout: Duration = HANG_GUARD;",
        "let mut timeout: Duration = Duration::from_millis(5);",
        "let c = Config { timeout: HANG_GUARD };",
        "let c = Config { timeout: budget };",
        "let c = Config { timeout: Duration::from_millis(ms) };",
        "'wait: loop { break 'wait; }",
        "let c = Config { timeout::from(Duration::from_millis(5)) };",
        "let s = \"Config { timeout: Duration::from_millis(50) }\";",
        "// let c = Config { timeout: Duration::from_millis(50) };",
    ];
    for statement in ignored {
        let scan = scan_one(DEMO, &in_test(&format!("    {statement}")));
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn a_const_named_like_a_budget_is_counted() {
    let counted = [
        "const DEADLINE: Duration = Duration::from_secs(15);",
        "const PAGE_WAIT: Duration = Duration::from_millis(100);",
        "pub const STOP_GRACE: Duration = Duration::from_millis(50);",
        "static REQUEST_TIMEOUT: Duration = Duration::from_secs(1);",
        "const SILENT_CHILD_STALL_MS: u64 = 300;",
        "const WIRE_BOUND_MS: u64 = 5 * 1_000;",
        "const Shared_Budget: Duration = Duration::from_secs(60 * 1);",
    ];
    for statement in counted {
        let text = format!("#[cfg(test)]\nmod tests {{\n    {statement}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert_eq!(
            lines_and_kinds(&scan),
            [(3, Kind::BudgetConst)],
            "{statement}"
        );
    }
    let ignored = [
        "const DEADLINE: Duration = HANG_GUARD;",
        "const DEADLINE: Duration = Duration::from_secs(120);",
        "const DEADLINE_MS: u64 = 120_000;",
        "const POLL_INTERVAL: Duration = Duration::from_millis(20);",
        "const SETTLE_MS: u64 = 300;",
        "const MAX_RETRIES: u32 = 5;",
        "const LIMIT: usize = 10;",
        "const WAIT_FN: fn() -> u32 = compute;",
        "const fn wait() -> Duration { Duration::from_secs(1) }",
        "trait T { const DEADLINE: Duration; }",
    ];
    for statement in ignored {
        let text = format!("#[cfg(test)]\nmod tests {{\n    {statement}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn one_literal_counts_once_however_many_constructs_match_it() {
    let text = in_test(
        "    let c = Config { timeout: tokio::time::timeout(Duration::from_secs(1), run()) };",
    );
    let scan = scan_one(DEMO, &text);
    assert_eq!(scan.counted.len(), 1, "{:?}", scan.counted);
    let text = in_test(
        "    tokio::time::timeout(Duration::from_secs(1), run(Config { timeout: Duration::from_secs(2) })).await;",
    );
    assert_eq!(scan_one(DEMO, &text).counted.len(), 2);
    let nested = in_test(
        "    let c = Config { stop_grace: Duration::from_millis(50), timeout: Some(Duration::from_millis(5)) };",
    );
    assert_eq!(scan_one(DEMO, &nested).counted.len(), 2);
    let text = "#[cfg(test)]\nmod tests {\n    const STOP_GRACE_MS: u64 = 50;\n}\n";
    assert_eq!(scan_one(DEMO, text).counted.len(), 1);
}

#[test]
fn only_test_code_is_scanned() {
    let body = "fn f() {\n    let c = Config { stop_grace: Duration::from_millis(50) };\n}\n";
    assert!(scan_one(DEMO, body).counted.is_empty());
    let scan = scan_one("crates/demo/tests/run.rs", body);
    assert_eq!(lines_and_kinds(&scan), [(2, Kind::BudgetField)]);
    let text = format!("#[cfg(test)]\nmod tests {{\n{body}}}\n{body}");
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, &text)),
        [(4, Kind::BudgetField)]
    );
    let production = "const DEADLINE: Duration = Duration::from_secs(15);\n";
    assert!(scan_one(DEMO, production).counted.is_empty());
}

#[test]
fn a_literal_inside_a_start_paused_test_is_virtual_time() {
    let paused = [
        "#[tokio::test(start_paused = true)]",
        "#[tokio::test(flavor = \"current_thread\", start_paused = true)]",
        "#[tokio::test( start_paused=true )]",
    ];
    let statements = [
        "tokio::time::timeout(Duration::from_secs(2), fut).await;",
        "let c = Config { stop_grace: Duration::from_millis(50) };",
    ];
    for attribute in paused {
        for statement in statements {
            let text = format!("{attribute}\nasync fn f() {{\n    {statement}\n}}\n");
            assert!(
                scan_one(DEMO, &text).counted.is_empty(),
                "{attribute} {statement}"
            );
        }
    }
    for attribute in ["#[tokio::test]", "#[tokio::test(start_paused = false)]"] {
        for statement in statements {
            let text = format!("{attribute}\nasync fn f() {{\n    {statement}\n}}\n");
            assert_eq!(
                scan_one(DEMO, &text).counted.len(),
                1,
                "{attribute} {statement}"
            );
        }
    }
    let text = "#[tokio::test(start_paused = true)]\nasync fn a() {}\n#[tokio::test]\nasync fn b() {\n    let c = Config { timeout: Duration::from_millis(5) };\n}\n";
    assert_eq!(scan_one(DEMO, text).counted.len(), 1);
}

#[test]
fn a_valid_marker_on_or_above_the_line_exempts_the_literal() {
    let forms = [
        "#[test]\nfn f() {\n    let c = C { timeout: Duration::from_millis(5) }; // timing-allowed: #1592 the policy under test\n}\n",
        "#[test]\nfn f() {\n    // timing-allowed: #1592 the policy under test\n    let c = C { timeout: Duration::from_millis(5) };\n}\n",
        "#[cfg(test)]\nmod t {\n    // timing-allowed: #1592 the policy under test\n    const STALL_MS: u64 = 300;\n}\n",
    ];
    for text in forms {
        assert!(scan_one(DEMO, text).counted.is_empty(), "{text}");
    }
    let two_lines_above = "#[test]\nfn f() {\n    // timing-allowed: #1592 reason\n\n    let c = C { timeout: Duration::from_millis(5) };\n}\n";
    assert_eq!(scan_one(DEMO, two_lines_above).counted.len(), 1);
    let malformed = [
        "// timing-allowed\n",
        "// timing-allowed: #1592\n",
        "// timing-allowed: 1592 reason\n",
        "/* // timing-allowed: #1592 reason */\n",
    ];
    for marker in malformed {
        let text = format!(
            "#[test]\nfn f() {{\n    {marker}    let c = C {{ timeout: Duration::from_millis(5) }};\n}}\n"
        );
        assert_eq!(scan_one(DEMO, &text).counted.len(), 1, "{marker}");
    }
}

#[test]
fn counts_below_the_baseline_fail_with_a_lower_the_baseline_message() {
    let text = in_test("    let c = C { timeout: Duration::from_millis(5) };");
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

    let gone = BTreeMap::from([("crates/demo/src/gone.rs".to_owned(), 3)]);
    let failures = evaluate(&scan, &gone);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures
        .iter()
        .any(|f| f.contains("gone.rs") && f.contains("lower the baseline to 0")));
    assert!(failures.iter().any(|f| f.contains("baseline allows 0")));
}

#[test]
fn a_file_missing_from_the_baseline_may_not_gain_budgets() {
    let text = in_test(
        "    let a = C { timeout: Duration::from_millis(5) };\n    let b = C { stop_grace: Duration::from_millis(5) };",
    );
    let scan = scan_one(DEMO, &text);
    let other = BTreeMap::from([("crates/other/src/lib.rs".to_owned(), 0)]);
    let failures = evaluate(&scan, &other);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("lib.rs:3"), "{failures:?}");
    assert!(failures[0].contains("lib.rs:4"), "{failures:?}");
    assert!(failures[0].contains("baseline allows 0"), "{failures:?}");
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
