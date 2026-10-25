//! Ratchet on detached `tokio::spawn` calls in daemon and session-worker
//! production code.
//!
//! A task spawned with a bare `tokio::spawn` is outside every `TaskTracker`, so
//! nothing can wait for it or cancel it when its owner stops, and a test that
//! observes the owner's state races whatever the task still writes. Background
//! work of the session registry is spawned through the registry's tracker
//! instead (`SessionRegistry::shutdown` awaits it).
//!
//! This test counts, per file, the calls `tokio::spawn(`,
//! `tokio::task::spawn(` and `task::spawn(` in the non-test code of
//! `crates/daemon` and `crates/session-worker`. A bare `spawn(` imported from
//! tokio is not a match; the tracker's own `spawn` methods are not either.
//!
//! Counts are compared with `detached_spawn_ratchet_baseline.txt` next to this
//! file (one `path count` pair per line). A file whose count rises above its
//! baseline fails, and so does a file whose count fell below it: the baseline
//! only goes down.
//!
//! After lowering counts, rewrite the baseline with
//! `cargo nextest run -p xtask --run-ignored only -E 'test(regenerate_detached_spawn_baseline)'`.
//! The regeneration test refuses to raise any file above the committed
//! baseline; it only creates a baseline from scratch when none exists.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;

use xtask::test_code::{classify, is_ident, read_sources, skip_whitespace};

/// Workspace-relative path of the committed baseline.
const BASELINE_PATH: &str = "crates/xtask/tests/detached_spawn_ratchet_baseline.txt";

/// Path prefixes of the production code the ratchet covers.
const SCANNED_PREFIXES: [&str; 2] = ["crates/daemon/", "crates/session-worker/"];

/// Header written at the top of the baseline file.
const BASELINE_HEADER: &str =
    "# Ratchet baseline for tests/detached_spawn_ratchet.rs: bare tokio::spawn\n\
# calls in the production code of crates/daemon and crates/session-worker, per file.\n\
# Format: `<workspace-relative path> <count>`. Counts may only go down;\n\
# regenerate with the ignored `regenerate_detached_spawn_baseline` test.\n";

/// One counted occurrence.
#[derive(Debug, PartialEq, Eq)]
struct Occurrence {
    path: String,
    line: usize,
}

/// Whether `bytes[offset..]` starts an identifier-bounded word.
fn starts_word(bytes: &[u8], offset: usize) -> bool {
    offset == 0 || !is_ident(bytes[offset - 1])
}

/// Index of the first byte of the whitespace run that ends right before `end`.
fn trim_back(bytes: &[u8], mut end: usize) -> usize {
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

/// The path segments before `end` (`a::b::` read backwards), nearest last, and
/// the index where the path starts. A leading `::` is consumed.
fn path_segments_before(bytes: &[u8], end: usize) -> (Vec<&str>, usize) {
    let mut segments = Vec::new();
    let mut cursor = end;
    loop {
        let colons = trim_back(bytes, cursor);
        if colons < 2 || &bytes[colons - 2..colons] != b"::" {
            return (segments, cursor);
        }
        let ident_end = trim_back(bytes, colons - 2);
        let mut ident_start = ident_end;
        while ident_start > 0 && is_ident(bytes[ident_start - 1]) {
            ident_start -= 1;
        }
        if ident_start == ident_end {
            // A leading `::` with no segment before it.
            return (segments, colons - 2);
        }
        segments.push(std::str::from_utf8(&bytes[ident_start..ident_end]).unwrap_or(""));
        cursor = ident_start;
    }
}

/// Whether the `spawn` at `offset` is a call of `tokio::spawn`,
/// `tokio::task::spawn` or `task::spawn` (whitespace between tokens allowed).
fn is_detached_spawn(skeleton: &str, offset: usize) -> bool {
    let bytes = skeleton.as_bytes();
    let end = offset + "spawn".len();
    if bytes.get(end).copied().is_some_and(is_ident)
        || bytes.get(skip_whitespace(bytes, end)) != Some(&b'(')
    {
        return false;
    }
    let (segments, _) = path_segments_before(bytes, offset);
    // `segments` runs from the nearest segment outwards.
    matches!(
        segments.as_slice(),
        ["tokio" | "task"] | ["task", "tokio"]
    )
}

/// Scans `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> Vec<Occurrence> {
    let mut found = Vec::new();
    for (path, file) in classify(files) {
        if !SCANNED_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            continue;
        }
        let skeleton = &file.views().skeleton;
        let bytes = skeleton.as_bytes();
        for (offset, _) in skeleton.match_indices("spawn") {
            if starts_word(bytes, offset)
                && !file.in_test_code(offset)
                && is_detached_spawn(skeleton, offset)
            {
                found.push(Occurrence {
                    path: path.clone(),
                    line: file.line_of(offset),
                });
            }
        }
    }
    found
}

/// Counted occurrences per file.
fn counts(found: &[Occurrence]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for occurrence in found {
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

/// Failure messages for `found` measured against `baseline`; empty when the
/// ratchet holds.
fn evaluate(found: &[Occurrence], baseline: &BTreeMap<String, usize>) -> Vec<String> {
    let mut failures = Vec::new();
    let current = counts(found);
    for (path, &count) in &current {
        let allowed = baseline.get(path).copied().unwrap_or(0);
        if count > allowed {
            let mut message = format!(
                "{path}: {count} detached tokio::spawn call(s) in production code, baseline allows {allowed}:\n"
            );
            for occurrence in found.iter().filter(|o| &o.path == path) {
                let _ = writeln!(message, "  {}:{}", occurrence.path, occurrence.line);
            }
            message.push_str(
                "  Fix: spawn the task through the owner's tracker (the session registry's \
                 `spawn_background`, a `TaskTracker`), so shutdown can cancel and await it.",
            );
            failures.push(message);
        }
    }
    for (path, &allowed) in baseline {
        let count = current.get(path).copied().unwrap_or(0);
        if count < allowed {
            failures.push(format!(
                "{path}: {count} detached spawn(s) found but the baseline allows {allowed}; \
                 lower the baseline to {count} (the ratchet only goes down). Rewrite it with \
                 `cargo nextest run -p xtask --run-ignored only -E \
                 'test(regenerate_detached_spawn_baseline)'`."
            ));
        }
    }
    failures
}

fn baseline_file() -> std::path::PathBuf {
    pohunek_test_support::workspace_root().join(BASELINE_PATH)
}

fn scan_workspace() -> Vec<Occurrence> {
    let files = read_sources(&pohunek_test_support::workspace_root())
        .unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    scan_files(&files)
}

#[test]
fn production_code_stays_within_the_detached_spawn_baseline() {
    let text =
        fs::read_to_string(baseline_file()).unwrap_or_else(|e| panic!("read {BASELINE_PATH}: {e}"));
    let failures = evaluate(&scan_workspace(), &parse_baseline(&text));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "rewrites the committed baseline; run it after moving spawns onto a tracker"]
fn regenerate_detached_spawn_baseline() {
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
            "the baseline only goes down; spawn the new tasks through a tracker in {raised:?}"
        );
    }
    fs::write(baseline_file(), format_baseline(&current))
        .unwrap_or_else(|e| panic!("write {BASELINE_PATH}: {e}"));
}

/// Scan of one file at `path`.
fn scan_one(path: &str, text: &str) -> Vec<Occurrence> {
    scan_files(&BTreeMap::from([(path.to_owned(), text.to_owned())]))
}

const DEMO: &str = "crates/daemon/src/demo.rs";

#[test]
fn a_new_bare_spawn_in_production_code_fails_the_ratchet() {
    let text = "async fn f() {\n    tokio::spawn(async {});\n}\n";
    let found = scan_one(DEMO, text);
    assert_eq!(found.len(), 1, "{found:?}");
    let failures = evaluate(&found, &BTreeMap::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("crates/daemon/src/demo.rs:2"));
    assert!(failures[0].contains("tracker"));
}

#[test]
fn every_detached_spawn_form_is_recognised() {
    for call in [
        "tokio::spawn(fut)",
        "tokio::task::spawn(fut)",
        "task::spawn(fut)",
        "::tokio::spawn(fut)",
        "tokio :: spawn (fut)",
        "tokio::\n        spawn(fut)",
        "match tokio::spawn(fut)",
        "let a = b; tokio::spawn(fut)",
    ] {
        let text = format!("async fn f() {{\n    {call};\n}}\n");
        assert_eq!(scan_one(DEMO, &text).len(), 1, "{call}");
    }
}

#[test]
fn tracked_spawns_lookalikes_and_other_crates_are_not_counted() {
    for call in [
        "tracker.spawn(fut)",
        "self.spawn(fut)",
        "spawn(fut)",
        "my::spawn(fut)",
        "mytask::spawn(fut)",
        "my::tokio::spawn(fut)",
        "tokio::spawn_blocking(f)",
        "tokio::task::spawn_blocking(f)",
        "tokio::spawn",
        "tokio::task::spawn_local(fut)",
    ] {
        let text = format!("async fn f() {{\n    {call};\n}}\n");
        assert!(scan_one(DEMO, &text).is_empty(), "{call}");
    }
    let text = "async fn f() {\n    tokio::spawn(fut);\n}\n";
    assert!(scan_one("crates/cli/src/demo.rs", text).is_empty());
    assert_eq!(scan_one("crates/session-worker/src/demo.rs", text).len(), 1);
}

#[test]
fn test_code_and_comments_are_not_counted() {
    let in_test = "#[cfg(test)]\nmod tests {\n    #[tokio::test]\n    async fn f() {\n        tokio::spawn(fut);\n    }\n}\n";
    assert!(scan_one(DEMO, in_test).is_empty());
    assert!(scan_one(
        "crates/daemon/tests/run.rs",
        "fn f() { tokio::spawn(fut); }\n"
    )
    .is_empty());
    let text = "fn f() {\n    // tokio::spawn(fut);\n    let s = \"tokio::spawn(fut)\";\n}\n";
    assert!(scan_one(DEMO, text).is_empty());
}

#[test]
fn counts_below_the_baseline_fail_with_a_lower_the_baseline_message() {
    let found = scan_one(DEMO, "async fn f() {\n    tokio::spawn(fut);\n}\n");
    assert!(evaluate(&found, &BTreeMap::from([(DEMO.to_owned(), 1)])).is_empty());
    let failures = evaluate(&found, &BTreeMap::from([(DEMO.to_owned(), 2)]));
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
