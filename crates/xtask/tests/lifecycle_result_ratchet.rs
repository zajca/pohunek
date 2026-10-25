//! Ratchet on discarded lifecycle results in test code.
//!
//! A test reads state that a lifecycle call made final only after it has
//! checked that the call succeeded: `let _ = registry.stop(..).await;` hides a
//! failed stop, and the assertions after it then race whatever the failed call
//! left running. This test counts, per file and in test code only (as
//! classified by `xtask::test_code`), the statements of the shape
//! `let _ = <expr>.<lifecycle method>(..).await;` where the awaited result is
//! bound to `_` and dropped. The lifecycle methods are `stop`, `remove`,
//! `resume`, `restart`, `kill`, `shutdown` and `retire`.
//!
//! Counts are compared with `lifecycle_result_ratchet_baseline.txt` next to
//! this file (one `path count` pair per line). A file whose count rises above
//! its baseline fails, and so does a file whose count fell below it: the
//! baseline only goes down.
//!
//! Not counted: occurrences on a line that carries, or follows a line that
//! carries, a line comment starting
//! `// lifecycle-result-allowed: #<issue> <reason>`. The marker must name an
//! issue number and a non-empty reason; a malformed marker is itself a
//! failure. Text inside a block comment or string is never a marker.
//!
//! After lowering counts, rewrite the baseline with
//! `cargo nextest run -p xtask --run-ignored only -E 'test(regenerate_lifecycle_result_baseline)'`.
//! The regeneration test refuses to raise any file above the committed
//! baseline; it only creates a baseline from scratch when none exists.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;

use xtask::test_code::{classify, is_ident, read_sources, skip_whitespace, SourceFile};

/// Workspace-relative path of the committed baseline.
const BASELINE_PATH: &str = "crates/xtask/tests/lifecycle_result_ratchet_baseline.txt";

/// Comment keyword that exempts a discarded result (after `// `).
const MARKER: &str = "lifecycle-result-allowed";

/// Method names whose awaited result a test must check.
const LIFECYCLE_METHODS: [&str; 7] = [
    "stop", "remove", "resume", "restart", "kill", "shutdown", "retire",
];

/// Header written at the top of the baseline file.
const BASELINE_HEADER: &str =
    "# Ratchet baseline for tests/lifecycle_result_ratchet.rs: discarded lifecycle\n\
# results (`let _ = <x>.stop|remove|resume|restart|kill|shutdown|retire(..).await;`)\n\
# in test code, per file.\n\
# Format: `<workspace-relative path> <count>`. Counts may only go down;\n\
# regenerate with the ignored `regenerate_lifecycle_result_baseline` test.\n";

/// One counted occurrence.
#[derive(Debug, PartialEq, Eq)]
struct Occurrence {
    path: String,
    line: usize,
    method: &'static str,
}

/// A marker comment that does not name an issue and a reason.
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

/// Whether `bytes[offset..]` starts an identifier-bounded word.
fn starts_word(bytes: &[u8], offset: usize) -> bool {
    offset == 0 || !is_ident(bytes[offset - 1])
}

/// Index of the `;` that ends the statement starting at `from`, or the length.
fn statement_end(bytes: &[u8], from: usize) -> usize {
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b';' if depth == 0 => return i,
            _ => {}
        }
    }
    bytes.len()
}

/// The lifecycle method a discarded `let _ = ..await;` statement calls, when
/// the offset starts one.
fn discarded_lifecycle_call(skeleton: &str, let_at: usize) -> Option<&'static str> {
    let bytes = skeleton.as_bytes();
    let after_let = skip_whitespace(bytes, let_at + "let".len());
    if bytes.get(after_let) != Some(&b'_')
        || bytes.get(after_let + 1).copied().is_some_and(is_ident)
    {
        return None;
    }
    let equals = skip_whitespace(bytes, after_let + 1);
    if bytes.get(equals) != Some(&b'=') || bytes.get(equals + 1) == Some(&b'=') {
        return None;
    }
    let end = statement_end(bytes, equals + 1);
    let expression = skeleton[equals + 1..end].trim_end();
    let awaited = expression.strip_suffix(".await")?;
    LIFECYCLE_METHODS.into_iter().find(|method| {
        let needle = format!(".{method}");
        awaited.match_indices(&needle).any(|(at, _)| {
            let after = at + needle.len();
            let tail = awaited.as_bytes();
            !tail.get(after).copied().is_some_and(is_ident)
                && tail.get(skip_whitespace(tail, after)) == Some(&b'(')
        })
    })
}

/// Byte offsets of the `//` that begin a line comment, outside block comments,
/// strings and char literals.
fn line_comment_starts(text: &str, code: &str) -> Vec<usize> {
    let (text, code) = (text.as_bytes(), code.as_bytes());
    let mut starts = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if code[i] != b' ' || text[i].is_ascii_whitespace() {
            i += 1;
        } else if text[i..].starts_with(b"//") {
            starts.push(i);
            i += text[i..]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(text.len() - i);
        } else {
            let mut depth = 0_usize;
            while i < text.len() {
                if text[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if text[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    starts
}

/// Lines (one-based) carrying a valid marker and the malformed marker lines.
fn markers(file: &SourceFile, text: &str) -> (Vec<usize>, Vec<usize>) {
    let mut valid = Vec::new();
    let mut malformed = Vec::new();
    for at in line_comment_starts(text, &file.views().code) {
        let rest = &text[at + 2..];
        let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
        if rest.starts_with(['/', '!']) {
            continue;
        }
        if let Some(body) = rest.trim_start().strip_prefix(MARKER) {
            let line = file.line_of(at);
            if is_valid_marker(body) {
                valid.push(line);
            } else {
                malformed.push(line);
            }
        }
    }
    (valid, malformed)
}

/// Whether the text after the marker keyword is `: #<digits> <reason>`.
fn is_valid_marker(body: &str) -> bool {
    let Some(rest) = body.strip_prefix(':') else {
        return false;
    };
    let Some(rest) = rest.trim().strip_prefix('#') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest[digits..].starts_with(char::is_whitespace)
        && !rest[digits..].trim().is_empty()
}

/// Scans `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> Scan {
    let mut scan = Scan::default();
    for (path, file) in classify(files) {
        let text = &files[&path];
        let (valid, malformed) = markers(&file, text);
        scan.malformed
            .extend(malformed.into_iter().map(|line| MalformedMarker {
                path: path.clone(),
                line,
            }));
        let skeleton = &file.views().skeleton;
        let bytes = skeleton.as_bytes();
        for (offset, _) in skeleton.match_indices("let") {
            if !starts_word(bytes, offset) || !file.in_test_code(offset) {
                continue;
            }
            let Some(method) = discarded_lifecycle_call(skeleton, offset) else {
                continue;
            };
            let line = file.line_of(offset);
            let allowed = valid.contains(&line) || valid.contains(&line.saturating_sub(1));
            if !allowed {
                scan.counted.push(Occurrence {
                    path: path.clone(),
                    line,
                    method,
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

/// Parses the baseline text: `<path> <count>` per line, `#` starts a comment
/// line.
fn parse_baseline(text: &str) -> BTreeMap<String, usize> {
    let mut baseline = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (path, count) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("baseline line {}: expected `<path> <count>`", index + 1));
        let count = count
            .parse()
            .unwrap_or_else(|e| panic!("baseline line {}: bad count: {e}", index + 1));
        assert!(
            baseline.insert(path.trim().to_owned(), count).is_none(),
            "baseline line {}: duplicate path {path}",
            index + 1
        );
    }
    baseline
}

/// Renders `counts` as baseline file text.
fn format_baseline(counts: &BTreeMap<String, usize>) -> String {
    let mut text = BASELINE_HEADER.to_owned();
    for (path, count) in counts {
        writeln!(text, "{path} {count}").expect("writing to a String cannot fail");
    }
    text
}

/// Failure messages for `scan` measured against `baseline`; empty when the
/// ratchet holds.
fn evaluate(scan: &Scan, baseline: &BTreeMap<String, usize>) -> Vec<String> {
    let mut failures = Vec::new();
    for marker in &scan.malformed {
        failures.push(format!(
            "{}:{}: malformed lifecycle marker; write `// {MARKER}: #<issue> <reason>` with an \
             issue number and a non-empty reason",
            marker.path, marker.line
        ));
    }
    let current = counts(scan);
    for (path, &count) in &current {
        let allowed = baseline.get(path).copied().unwrap_or(0);
        if count > allowed {
            let mut message = format!(
                "{path}: {count} discarded lifecycle result(s) in test code, baseline allows {allowed}:\n"
            );
            for occurrence in scan.counted.iter().filter(|o| &o.path == path) {
                let _ = writeln!(
                    message,
                    "  {}:{}: `let _ = ..{}(..).await`",
                    occurrence.path, occurrence.line, occurrence.method
                );
            }
            message.push_str(
                "  Fix: check the result (`.expect(\"..\")` or an assertion on the error), or \
                 justify discarding it with a `// lifecycle-result-allowed: #<issue> <reason>` \
                 comment on or above the line.",
            );
            failures.push(message);
        }
    }
    for (path, &allowed) in baseline {
        let count = current.get(path).copied().unwrap_or(0);
        if count < allowed {
            failures.push(format!(
                "{path}: {count} occurrence(s) found but the baseline allows {allowed}; lower \
                 the baseline to {count} (the ratchet only goes down). Rewrite it with `cargo \
                 nextest run -p xtask --run-ignored only -E \
                 'test(regenerate_lifecycle_result_baseline)'`."
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
fn test_code_stays_within_the_lifecycle_result_baseline() {
    let text =
        fs::read_to_string(baseline_file()).unwrap_or_else(|e| panic!("read {BASELINE_PATH}: {e}"));
    let failures = evaluate(&scan_workspace(), &parse_baseline(&text));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "rewrites the committed baseline; run it after checking discarded lifecycle results"]
fn regenerate_lifecycle_result_baseline() {
    let scan = scan_workspace();
    assert!(
        scan.malformed.is_empty(),
        "fix malformed lifecycle markers first: {:?}",
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
            "the baseline only goes down; check the new lifecycle results in {raised:?}"
        );
    }
    fs::write(baseline_file(), format_baseline(&current))
        .unwrap_or_else(|e| panic!("write {BASELINE_PATH}: {e}"));
}

/// Scan of one file at `path`.
fn scan_one(path: &str, text: &str) -> Scan {
    scan_files(&BTreeMap::from([(path.to_owned(), text.to_owned())]))
}

const DEMO: &str = "crates/demo/src/lib.rs";

#[test]
fn a_new_discarded_lifecycle_result_fails_the_ratchet() {
    let text = "#[tokio::test]\nasync fn f() {\n    let _ = registry.stop(id).await;\n}\n";
    let scan = scan_one(DEMO, text);
    assert_eq!(scan.counted.len(), 1, "{:?}", scan.counted);
    let failures = evaluate(&scan, &BTreeMap::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("crates/demo/src/lib.rs:3"));
    assert!(failures[0].contains("lifecycle-result-allowed"));
}

#[test]
fn every_lifecycle_method_and_layout_is_recognised() {
    for method in LIFECYCLE_METHODS {
        let text = format!("#[test]\nfn f() {{\n    let _ = x.{method}(a, b).await;\n}}\n");
        assert_eq!(scan_one(DEMO, &text).counted.len(), 1, "{method}");
    }
    let multiline =
        "#[test]\nfn f() {\n    let _ = rig\n        .registry\n        .stop(id, opts)\n        .await;\n}\n";
    let scan = scan_one(DEMO, multiline);
    assert_eq!(scan.counted.len(), 1);
    assert_eq!(scan.counted[0].line, 3);
}

#[test]
fn checked_results_and_other_calls_are_not_counted() {
    let ignored = [
        "let _ = x.stop(id).await.expect(\"stop\");",
        "let _ = x.stop(id).await?;",
        "let _guard = x.stop(id).await;",
        "let result = x.stop(id).await;",
        "let _ = x.stop_all(id).await;",
        "let _ = x.restopped(id).await;",
        "let _ = x.start(id).await;",
        "let _ = x.stop(id);",
        "let _ = stop(id).await;",
        "let _ = x.resume(id).await.is_ok();",
        "let _ = \"x.stop(id).await\";",
        "// let _ = x.stop(id).await;",
    ];
    for statement in ignored {
        let text = format!("#[test]\nfn f() {{\n    {statement}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn production_code_is_not_scanned() {
    let text = "async fn f() {\n    let _ = x.stop(id).await;\n}\n";
    assert!(scan_one(DEMO, text).counted.is_empty());
    let in_test_file = scan_one("crates/demo/tests/run.rs", text);
    assert_eq!(in_test_file.counted.len(), 1);
}

#[test]
fn a_valid_marker_on_or_above_the_line_exempts_the_occurrence() {
    let forms = [
        "#[test]\nfn f() {\n    let _ = x.stop(id).await; // lifecycle-result-allowed: #1592 teardown\n}\n",
        "#[test]\nfn f() {\n    // lifecycle-result-allowed: #1592 teardown\n    let _ = x.stop(id).await;\n}\n",
    ];
    for text in forms {
        let scan = scan_one(DEMO, text);
        assert!(scan.counted.is_empty(), "{text}");
        assert!(scan.malformed.is_empty(), "{text}");
    }
}

#[test]
fn a_malformed_marker_is_a_failure_and_does_not_exempt() {
    let forms = [
        "// lifecycle-result-allowed\n",
        "// lifecycle-result-allowed: #1592\n",
        "// lifecycle-result-allowed: 1592 reason\n",
        "// lifecycle-result-allowed: #abc reason\n",
    ];
    for marker in forms {
        let text = format!("#[test]\nfn f() {{\n    {marker}    let _ = x.stop(id).await;\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{marker}");
        let failures = evaluate(&scan, &BTreeMap::from([(DEMO.to_owned(), 1)]));
        assert_eq!(failures.len(), 1, "{marker}: {failures:?}");
        assert!(
            failures[0].contains("malformed lifecycle marker"),
            "{marker}"
        );
    }
}

#[test]
fn counts_below_the_baseline_fail_with_a_lower_the_baseline_message() {
    let text = "#[test]\nfn f() {\n    let _ = x.stop(id).await;\n}\n";
    let scan = scan_one(DEMO, text);
    assert!(evaluate(&scan, &BTreeMap::from([(DEMO.to_owned(), 1)])).is_empty());
    let failures = evaluate(&scan, &BTreeMap::from([(DEMO.to_owned(), 2)]));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("lower the baseline to 1"),
        "{failures:?}"
    );
}

#[test]
fn the_committed_baseline_is_sorted_and_has_no_zero_entries() {
    let text = fs::read_to_string(baseline_file()).expect("baseline exists");
    let baseline = parse_baseline(&text);
    assert!(baseline.values().all(|&count| count > 0), "zero entry");
    assert_eq!(text, format_baseline(&baseline), "baseline is canonical");
}
