//! Ratchet on real-time waits and real-time duration assertions in test code.
//!
//! A test never asserts a duration against real time, and never uses a sleep
//! as synchronization: when the deadline is the behavior under test it runs on
//! virtual time (`#[tokio::test(start_paused = true)]` or an injected clock),
//! and when the deadline is incidental it waits on a readiness signal with the
//! helpers in `pohunek_test_support::wait`, bounded by `HANG_GUARD`.
//!
//! This test counts, per file and in test code only (as classified by
//! `xtask::test_code`):
//! - `thread::sleep(..)` with any path prefix, or a bare `sleep(..)` imported
//!   from `std::thread`;
//! - `time::sleep(..)` and `time::sleep_until(..)` with any path prefix, or a
//!   bare call imported from `tokio::time`;
//! - `Timer::after(..)`;
//! - `assert!`, `assert_eq!`, `assert_ne!` and their `debug_` forms whose
//!   compared operands include `.elapsed()` and a duration literal: a `from_*`
//!   call or a numeric literal. Comparison against a named constant is not
//!   counted.
//!
//! Counts are compared with `timing_ratchet_baseline.txt` next to this file
//! (one `path count` pair per line). A file whose count rises above its
//! baseline fails, and so does a file whose count fell below it: the baseline
//! only goes down, so a removed wait must also be removed from the baseline.
//!
//! Not counted:
//! - tokio `time::sleep`/`sleep_until` occurrences inside a test item declared
//!   `#[tokio::test(.. start_paused = true ..)]`: only tokio timers run on the
//!   paused clock. `thread::sleep` and `Timer::after` wait in real time there
//!   too and stay counted. An `.elapsed()` assertion in such a test is exempt
//!   only when the file imports `tokio::time::Instant` and names no other
//!   `Instant`; a lexical scan cannot otherwise tell it from `std::time::Instant`;
//! - occurrences on a line that carries, or follows a line that carries, a
//!   line comment starting `// timing-allowed: #<issue> <reason>`. The marker must
//!   name an issue number and a non-empty reason; a malformed marker is itself
//!   a failure. Text inside a block comment or string is never a marker.
//!
//! After lowering counts, rewrite the baseline with
//! `cargo nextest run -p xtask --run-ignored only -E 'test(regenerate_timing_baseline)'`.
//! The regeneration test refuses to raise any file above the committed
//! baseline; it only creates a baseline from scratch when none exists.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;

use xtask::test_code::{classify, is_ident, read_sources, skip_whitespace, SourceFile};

/// Workspace-relative path of the committed baseline.
const BASELINE_PATH: &str = "crates/xtask/tests/timing_ratchet_baseline.txt";

/// Comment keyword that exempts a timing occurrence (after `// `).
const MARKER: &str = "timing-allowed";

/// Assertion macros whose operands are inspected for real-time comparisons.
const ASSERT_MACROS: [&str; 6] = [
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
];

/// Attribute text (whitespace removed) that puts a test on virtual time.
const PAUSED_ATTRIBUTE: &str = "start_paused=true";

/// Header written at the top of the baseline file.
const BASELINE_HEADER: &str =
    "# Ratchet baseline for tests/timing_ratchet.rs: occurrences of real-time\n\
# sleeps and real-time duration assertions in test code, per file.\n\
# Format: `<workspace-relative path> <count>`. Counts may only go down;\n\
# regenerate with the ignored `regenerate_timing_baseline` test.\n";

/// What kind of real-time construct an occurrence is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    ThreadSleep,
    TokioSleep,
    TimerAfter,
    ElapsedAssert,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::ThreadSleep => "thread::sleep",
            Self::TokioSleep => "tokio sleep",
            Self::TimerAfter => "Timer::after",
            Self::ElapsedAssert => "elapsed() assertion",
        }
    }
}

/// One counted occurrence.
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

/// Index of the byte matching the opener at `open` in `bytes`, or the length.
fn matching_close(bytes: &[u8], open: usize) -> usize {
    let (opener, closer) = match bytes[open] {
        b'[' => (b'[', b']'),
        b'(' => (b'(', b')'),
        _ => (b'{', b'}'),
    };
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == opener {
            depth += 1;
        } else if byte == closer {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    bytes.len()
}

/// Index one past the identifier that starts at `start`.
fn end_of_ident(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end).copied().is_some_and(is_ident) {
        end += 1;
    }
    end
}

/// Whether `bytes[offset..]` starts an identifier-bounded word.
fn starts_word(bytes: &[u8], offset: usize) -> bool {
    offset == 0 || !is_ident(bytes[offset - 1])
}

/// The identifier that ends right before `end`, skipping whitespace first.
fn ident_before(bytes: &[u8], end: usize) -> &str {
    let mut stop = end;
    while stop > 0 && bytes[stop - 1].is_ascii_whitespace() {
        stop -= 1;
    }
    let mut start = stop;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    std::str::from_utf8(&bytes[start..stop]).unwrap_or("")
}

/// Index just past `token` when it follows `from` after optional whitespace.
fn expect_token(bytes: &[u8], from: usize, token: &str) -> Option<usize> {
    let at = skip_whitespace(bytes, from);
    bytes[at..]
        .starts_with(token.as_bytes())
        .then_some(at + token.len())
}

/// Whether `text` contains `word` bounded by non-identifier bytes.
fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(at, _)| {
        starts_word(bytes, at) && !bytes.get(at + word.len()).copied().is_some_and(is_ident)
    })
}

/// Kinds of the sleep functions that `use` statements import by bare name.
fn imported_sleeps(skeleton: &str) -> BTreeMap<&'static str, Kind> {
    let bytes = skeleton.as_bytes();
    let mut imported = BTreeMap::new();
    for (at, _) in skeleton.match_indices("use ") {
        if !starts_word(bytes, at) {
            continue;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        let kind = if contains_word(statement, "thread") {
            Kind::ThreadSleep
        } else if contains_word(statement, "time") {
            Kind::TokioSleep
        } else {
            continue;
        };
        for name in ["sleep", "sleep_until"] {
            if contains_word(statement, name) {
                imported.insert(name, kind);
            }
        }
    }
    imported
}

/// Sleep calls: `thread::sleep(`, `time::sleep(`, `time::sleep_until(` and
/// imported bare calls. Method calls and definitions are not matches.
fn sleep_candidates(skeleton: &str) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let imported = imported_sleeps(skeleton);
    let mut found = Vec::new();
    for (offset, _) in skeleton.match_indices("sleep") {
        if !starts_word(bytes, offset) {
            continue;
        }
        let end = end_of_ident(bytes, offset);
        let name = &skeleton[offset..end];
        if name != "sleep" && name != "sleep_until" {
            continue;
        }
        if bytes.get(skip_whitespace(bytes, end)) != Some(&b'(') {
            continue;
        }
        let mut before = offset;
        while before > 0 && bytes[before - 1].is_ascii_whitespace() {
            before -= 1;
        }
        if before > 0 && bytes[before - 1] == b'.' {
            continue;
        }
        let kind = if before >= 2 && &bytes[before - 2..before] == b"::" {
            match ident_before(bytes, before - 2) {
                "thread" => Kind::ThreadSleep,
                "time" => Kind::TokioSleep,
                _ => continue,
            }
        } else if ident_before(bytes, before) == "fn" {
            continue;
        } else if let Some(&kind) = imported.get(name) {
            kind
        } else {
            continue;
        };
        found.push((offset, kind));
    }
    found
}

/// `Timer::after(` calls.
fn timer_candidates(skeleton: &str) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    skeleton
        .match_indices("Timer")
        .filter(|&(offset, _)| {
            starts_word(bytes, offset) && end_of_ident(bytes, offset) == offset + "Timer".len()
        })
        .filter(|&(offset, _)| {
            expect_token(bytes, offset + "Timer".len(), "::")
                .and_then(|at| expect_token(bytes, at, "after"))
                .filter(|&at| !bytes.get(at).copied().is_some_and(is_ident))
                .and_then(|at| expect_token(bytes, at, "("))
                .is_some()
        })
        .map(|(offset, _)| (offset, Kind::TimerAfter))
        .collect()
}

/// Splits `body` at top-level commas.
fn top_level_arguments(body: &str) -> Vec<&str> {
    let mut depth = 0_usize;
    let mut start = 0;
    let mut arguments = Vec::new();
    for (i, c) in body.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                arguments.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    arguments.push(&body[start..]);
    arguments
}

/// Whether `text` holds a comparison operator (`<`, `>`, `<=`, `>=`).
fn has_comparison(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().any(|(i, &byte)| match byte {
        b'<' => true,
        b'>' => i == 0 || !matches!(bytes[i - 1], b'-' | b'='),
        _ => false,
    })
}

/// Whether `text` holds a duration literal: a `from_*` call or a number.
fn has_duration_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.contains("from_")
        || bytes
            .iter()
            .enumerate()
            .any(|(i, byte)| byte.is_ascii_digit() && starts_word(bytes, i))
}

/// Assertions comparing `.elapsed()` with a duration literal; the offset is
/// that of the `elapsed` call.
fn elapsed_candidates(skeleton: &str) -> Vec<(usize, Kind)> {
    let bytes = skeleton.as_bytes();
    let mut found = Vec::new();
    for name in ASSERT_MACROS {
        for (offset, _) in skeleton.match_indices(name) {
            let end = offset + name.len();
            if !starts_word(bytes, offset) || bytes.get(end).copied().is_some_and(is_ident) {
                continue;
            }
            let Some(bang) = expect_token(bytes, end, "!") else {
                continue;
            };
            let open = skip_whitespace(bytes, bang);
            if !matches!(bytes.get(open), Some(b'(' | b'[' | b'{')) {
                continue;
            }
            let close = matching_close(bytes, open);
            let body = &skeleton[open + 1..close];
            let arguments = top_level_arguments(body);
            let is_pair = name.ends_with("_eq") || name.ends_with("_ne");
            let compared = if is_pair {
                arguments[..arguments.len().min(2)].join(",")
            } else {
                arguments[0].to_owned()
            };
            let Some(elapsed) = compared.find(".elapsed") else {
                continue;
            };
            let call = expect_token(compared.as_bytes(), elapsed + ".elapsed".len(), "(");
            if call.is_some()
                && (is_pair || has_comparison(&compared))
                && has_duration_literal(&compared)
            {
                let at = open + 1 + elapsed + 1;
                found.push((at, Kind::ElapsedAssert));
            }
        }
    }
    found
}

/// Byte offsets of the `//` that begin a line comment, outside block comments,
/// strings and char literals.
///
/// `Views::code` blanks line and block comments alike, so a blanked,
/// non-whitespace byte starts a comment and the text there tells the kind.
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

/// Whether the text after `timing-allowed` is `: #<digits> <reason>`.
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

/// Whether the byte `offset` sits in a test item declared on virtual time.
fn on_virtual_time(file: &SourceFile, offset: usize) -> bool {
    file.test_attributes_at(offset).iter().any(|attribute| {
        let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
        squeezed.contains(PAUSED_ATTRIBUTE)
    })
}

/// Whether every `Instant` the file names is `tokio::time::Instant`, so an
/// `.elapsed()` reads the paused clock.
///
/// A lexical scan cannot type `started`; the file counts as tokio-only when it
/// imports `tokio::time::Instant` and no other `Instant` by `use` or by a
/// `time::Instant` path.
fn uses_only_tokio_instant(skeleton: &str) -> bool {
    let bytes = skeleton.as_bytes();
    let (mut tokio, mut other) = (false, false);
    for (at, _) in skeleton.match_indices("use ") {
        if !starts_word(bytes, at) {
            continue;
        }
        let end = skeleton[at..].find(';').map_or(skeleton.len(), |n| at + n);
        let statement = &skeleton[at..end];
        if contains_word(statement, "Instant") || statement.contains("time::*") {
            if contains_word(statement, "tokio") {
                tokio = true;
            } else {
                other = true;
            }
        }
    }
    let squeezed: String = skeleton.chars().filter(|c| !c.is_whitespace()).collect();
    for (at, _) in squeezed.match_indices("time::Instant") {
        if !squeezed[..at].ends_with("tokio::") {
            other = true;
        }
    }
    tokio && !other
}

/// Whether an occurrence of `kind` waits on tokio's paused clock: only tokio
/// timers are virtualized, and `.elapsed()` only when it reads a tokio `Instant`.
fn is_virtual(kind: Kind, only_tokio_instant: bool) -> bool {
    match kind {
        Kind::TokioSleep => true,
        Kind::ElapsedAssert => only_tokio_instant,
        Kind::ThreadSleep | Kind::TimerAfter => false,
    }
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
        let only_tokio_instant = uses_only_tokio_instant(skeleton);
        let mut candidates = sleep_candidates(skeleton);
        candidates.extend(timer_candidates(skeleton));
        candidates.extend(elapsed_candidates(skeleton));
        candidates.sort_by_key(|&(offset, _)| offset);
        for (offset, kind) in candidates {
            let line = file.line_of(offset);
            let allowed = valid.contains(&line) || valid.contains(&(line.saturating_sub(1)));
            let virtual_time =
                is_virtual(kind, only_tokio_instant) && on_virtual_time(&file, offset);
            if file.in_test_code(offset) && !virtual_time && !allowed {
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
                "{path}: {count} real-time timing occurrence(s) in test code, baseline allows {allowed}:\n"
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
                "  Fix: wait on a readiness signal with `pohunek_test_support::wait` (bounded \
                 by HANG_GUARD), run the test on virtual time (`#[tokio::test(start_paused = \
                 true)]` or an injected clock), or justify the wait with a \
                 `// timing-allowed: #<issue> <reason>` comment on or above the line.",
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
                 nextest run -p xtask --run-ignored only -E 'test(regenerate_timing_baseline)'`."
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
fn test_code_stays_within_the_timing_baseline() {
    let text =
        fs::read_to_string(baseline_file()).unwrap_or_else(|e| panic!("read {BASELINE_PATH}: {e}"));
    let failures = evaluate(&scan_workspace(), &parse_baseline(&text));
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
#[ignore = "rewrites the committed baseline; run it after removing real-time waits"]
fn regenerate_timing_baseline() {
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
            "the baseline only goes down; remove the new waits in {raised:?}"
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

fn lines_and_kinds(scan: &Scan) -> Vec<(usize, Kind)> {
    scan.counted.iter().map(|o| (o.line, o.kind)).collect()
}

#[test]
fn a_new_thread_sleep_in_a_test_fn_fails_the_ratchet() {
    let text = "#[test]\nfn f() {\n    std::thread::sleep(Duration::from_millis(5));\n}\n";
    let failures = evaluate(&scan_one(DEMO, text), &BTreeMap::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("crates/demo/src/lib.rs:3: thread::sleep"));
    assert!(failures[0].contains("pohunek_test_support::wait"));
    assert!(failures[0].contains("timing-allowed"));
}

#[test]
fn the_same_sleep_in_non_test_code_is_ignored() {
    let text = "fn f() {\n    std::thread::sleep(Duration::from_millis(5));\n}\n";
    assert!(evaluate(&scan_one(DEMO, text), &BTreeMap::new()).is_empty());
}

#[test]
fn a_sleep_in_a_test_file_or_cfg_test_module_is_counted() {
    let body = "fn f() {\n    thread::sleep(d);\n}\n";
    let scan = scan_one("crates/demo/tests/run.rs", body);
    assert_eq!(lines_and_kinds(&scan), [(2, Kind::ThreadSleep)]);
    let text = format!("#[cfg(test)]\nmod tests {{\n{body}}}\n{body}");
    let scan = scan_one(DEMO, &text);
    assert_eq!(lines_and_kinds(&scan), [(4, Kind::ThreadSleep)]);
}

#[test]
fn sleeps_inside_a_start_paused_test_are_virtual_time() {
    let paused = [
        "#[tokio::test(start_paused = true)]",
        "#[tokio::test(flavor = \"current_thread\", start_paused = true)]",
        "#[tokio::test( start_paused=true )]",
    ];
    for attribute in paused {
        let text = format!("{attribute}\nasync fn f() {{\n    tokio::time::sleep(d).await;\n}}\n");
        assert!(scan_one(DEMO, &text).counted.is_empty(), "{attribute}");
    }
    let text = "#[tokio::test]\nasync fn f() {\n    tokio::time::sleep(d).await;\n}\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, text)),
        [(3, Kind::TokioSleep)]
    );
    let text = "#[tokio::test(start_paused = false)]\nasync fn f() {\n    tokio::time::sleep(d).await;\n}\n";
    assert_eq!(scan_one(DEMO, text).counted.len(), 1);
    let text = "#[tokio::test(start_paused = true)]\nasync fn a() {}\n#[tokio::test]\nasync fn b() {\n    tokio::time::sleep(d).await;\n}\n";
    assert_eq!(scan_one(DEMO, text).counted.len(), 1);
}

#[test]
fn only_tokio_timers_are_exempt_in_a_paused_test() {
    let paused = "#[tokio::test(start_paused = true)]\nasync fn f() {\n";
    for statement in [
        "std::thread::sleep(d);",
        "thread::sleep(d);",
        "Timer::after(d).await;",
        "smol::Timer::after(d).await;",
    ] {
        let text = format!("{paused}    {statement}\n}}\n");
        assert_eq!(scan_one(DEMO, &text).counted.len(), 1, "{statement}");
    }
    let text = format!("use std::thread::sleep;\n{paused}    sleep(d);\n}}\n");
    assert_eq!(scan_one(DEMO, &text).counted.len(), 1);
    let text = format!("{paused}    tokio::time::sleep_until(t).await;\n}}\n");
    assert!(scan_one(DEMO, &text).counted.is_empty());
}

#[test]
fn elapsed_in_a_paused_test_is_exempt_only_for_a_tokio_instant() {
    let body = "#[tokio::test(start_paused = true)]\nasync fn f() {\n    assert!(t.elapsed() >= Duration::from_secs(1));\n}\n";
    let counted = [
        String::new(),
        "use std::time::Instant;\n".to_owned(),
        "use std::time::{Duration, Instant};\n".to_owned(),
        "use std::time::*;\n".to_owned(),
        "use tokio::time::Instant;\nuse std::time::Instant as Real;\n".to_owned(),
        "use tokio::time::Instant;\nfn g() { std::time::Instant::now(); }\n".to_owned(),
        "use tokio::time::Instant;\nuse std::time;\nfn g() { time::Instant::now(); }\n".to_owned(),
        "use tokio::time::Duration;\n".to_owned(),
    ];
    for prefix in counted {
        let scan = scan_one(DEMO, &format!("{prefix}{body}"));
        assert_eq!(scan.counted.len(), 1, "{prefix}");
        assert_eq!(scan.counted[0].kind, Kind::ElapsedAssert, "{prefix}");
    }
    for prefix in [
        "use tokio::time::Instant;\n",
        "use tokio::time::{Duration, Instant};\n",
        "use tokio::time::{self, Instant};\n",
        "use tokio::time::Instant;\nfn g() { tokio::time::Instant::now(); }\n",
    ] {
        assert!(
            scan_one(DEMO, &format!("{prefix}{body}"))
                .counted
                .is_empty(),
            "{prefix}"
        );
    }
    let unpaused = body.replace("(start_paused = true)", "");
    let text = format!("use tokio::time::Instant;\n{unpaused}");
    assert_eq!(scan_one(DEMO, &text).counted.len(), 1);
}

#[test]
fn a_valid_marker_on_or_above_the_line_exempts_the_occurrence() {
    let forms = [
        "#[test]\nfn f() {\n    thread::sleep(d); // timing-allowed: #362 asserts the OS timer\n}\n",
        "#[test]\nfn f() {\n    // timing-allowed: #362 asserts the OS timer\n    thread::sleep(d);\n}\n",
        "#[test]\nfn f() {\n    //timing-allowed:#362 reason\n    thread::sleep(d);\n}\n",
    ];
    for text in forms {
        let scan = scan_one(DEMO, text);
        assert!(scan.counted.is_empty(), "{text}");
        assert!(scan.malformed.is_empty(), "{text}");
    }
    let two_lines_above =
        "#[test]\nfn f() {\n    // timing-allowed: #362 reason\n\n    thread::sleep(d);\n}\n";
    assert_eq!(scan_one(DEMO, two_lines_above).counted.len(), 1);
}

#[test]
fn a_malformed_marker_is_a_failure_and_does_not_exempt() {
    let forms = [
        "// timing-allowed\n",
        "// timing-allowed:\n",
        "// timing-allowed: #362\n",
        "// timing-allowed: #362   \n",
        "// timing-allowed: 362 reason\n",
        "// timing-allowed: # reason\n",
        "// timing-allowed: #abc reason\n",
        "// timing-allowed #362 reason\n",
        "// timing-allowed: #362reason\n",
    ];
    for marker in forms {
        let text = format!("#[test]\nfn f() {{\n    {marker}    thread::sleep(d);\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{marker}");
        let failures = evaluate(&scan, &BTreeMap::from([(DEMO.to_owned(), 1)]));
        assert_eq!(failures.len(), 1, "{marker}: {failures:?}");
        assert!(failures[0].contains("malformed timing marker"), "{marker}");
        assert!(failures[0].contains("crates/demo/src/lib.rs:3"), "{marker}");
    }
}

#[test]
fn marker_text_in_docs_strings_and_mid_comment_is_not_a_marker() {
    let text = concat!(
        "/// // timing-allowed: broken\n",
        "//! timing-allowed: broken\n",
        "fn f() {\n",
        "    let s = \"// timing-allowed: broken\";\n",
        "    let r = r#\"\n// timing-allowed: broken\n\"#;\n",
        "    // see the timing-allowed: broken marker format\n",
        "}\n",
    );
    assert!(scan_one(DEMO, text).malformed.is_empty());
}

#[test]
fn a_marker_inside_a_block_comment_is_neither_valid_nor_malformed() {
    let inside = [
        "/* // timing-allowed: #362 reason */",
        "/* // timing-allowed */",
        "/* a\n// timing-allowed: broken\n*/",
        "/* /* nested */ // timing-allowed */",
    ];
    for comment in inside {
        let text = format!("#[test]\nfn f() {{\n    {comment}\n    thread::sleep(d);\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert!(scan.malformed.is_empty(), "{comment}");
        assert_eq!(scan.counted.len(), 1, "{comment}");
    }
}

#[test]
fn a_line_comment_marker_after_a_closed_block_comment_still_works() {
    let valid = "#[test]\nfn f() {\n    /* note */ // timing-allowed: #362 reason\n    thread::sleep(d);\n}\n";
    let scan = scan_one(DEMO, valid);
    assert!(scan.counted.is_empty() && scan.malformed.is_empty());
    let malformed =
        "#[test]\nfn f() {\n    /* note */ // timing-allowed\n    thread::sleep(d);\n}\n";
    assert_eq!(scan_one(DEMO, malformed).malformed.len(), 1);
}

#[test]
fn elapsed_assertions_against_a_duration_literal_are_counted() {
    let counted = [
        "assert!(started.elapsed() < Duration::from_secs(1));",
        "assert!(started.elapsed() >= Duration::from_millis(250), \"slow\");",
        "assert!(Duration::from_secs(3) > started.elapsed());",
        "debug_assert!(t.elapsed() <= std::time::Duration::from_secs_f64(0.5));",
        "assert!(t.elapsed().as_millis() < 500);",
        "assert_eq!(t.elapsed(), Duration::from_secs(2));",
        "assert_ne!(Duration::ZERO.max(Duration::from_secs(1)), t.elapsed());",
        "assert!(\n    started.elapsed()\n        < Duration::from_secs(1)\n);",
    ];
    for assertion in counted {
        let text = format!("#[test]\nfn f() {{\n    {assertion}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert_eq!(scan.counted.len(), 1, "{assertion}");
        assert_eq!(scan.counted[0].kind, Kind::ElapsedAssert, "{assertion}");
    }
    let text = "#[test]\nfn f() {\n    assert!(\n        started.elapsed()\n            < Duration::from_secs(1)\n    );\n}\n";
    assert_eq!(scan_one(DEMO, text).counted[0].line, 4);
}

#[test]
fn elapsed_uses_that_are_not_literal_comparisons_are_not_counted() {
    let ignored = [
        "assert!(started.elapsed() < DEADLINE, \"child never exited\");",
        "assert!(started.elapsed() >= CONNECT, \"{:?}\", started.elapsed());",
        "assert_eq!(items.len(), 3, \"after {:?}\", started.elapsed());",
        "assert!(items.len() < 3, \"after {:?}\", started.elapsed());",
        "assert!(started.elapsed().is_zero() || ok(3));",
        "assert!(done, \"waited {:?} of {}\", t.elapsed(), Duration::from_secs(1).as_secs());",
        "let spent = started.elapsed(); assert!(spent < Duration::from_secs(1));",
        "assert!(matches!(x.elapsed_hint(), 1));",
        "let s = \"assert!(t.elapsed() < Duration::from_secs(1))\";",
        "// assert!(t.elapsed() < Duration::from_secs(1));",
    ];
    for assertion in ignored {
        let text = format!("#[test]\nfn f() {{\n    {assertion}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{assertion}: {:?}", scan.counted);
    }
}

#[test]
fn every_sleep_and_timer_form_is_recognised_and_lookalikes_are_not() {
    let counted = [
        ("std::thread::sleep(d);", Kind::ThreadSleep),
        ("thread::sleep(d);", Kind::ThreadSleep),
        ("::std::thread::sleep (d);", Kind::ThreadSleep),
        ("tokio::time::sleep(d).await;", Kind::TokioSleep),
        ("time::sleep(d).await;", Kind::TokioSleep),
        ("tokio::time::sleep_until(t).await;", Kind::TokioSleep),
        ("time :: sleep_until(t).await;", Kind::TokioSleep),
        ("Timer::after(d).await;", Kind::TimerAfter),
        ("smol::Timer::after(d).await;", Kind::TimerAfter),
    ];
    for (statement, kind) in counted {
        let text = format!("#[test]\nfn f() {{\n    {statement}\n}}\n");
        assert_eq!(
            lines_and_kinds(&scan_one(DEMO, &text)),
            [(3, kind)],
            "{statement}"
        );
    }
    let ignored = [
        "clock.sleep(d);",
        "self.sleep(d).await;",
        "sleep(d);",
        "fn sleep(d: u8) {}",
        "my::sleep(d);",
        "thread::sleep_ms_hint(d);",
        "thread::sleeper(d);",
        "let sleep = 1;",
        "Timer::after_hint(d);",
        "MyTimer::after(d);",
        "Timer::at(d);",
        "let s = \"thread::sleep(d)\";",
        "// thread::sleep(d);",
        "/* tokio::time::sleep(d) */",
    ];
    for statement in ignored {
        let text = format!("#[test]\nfn f() {{\n    {statement}\n}}\n");
        let scan = scan_one(DEMO, &text);
        assert!(scan.counted.is_empty(), "{statement}: {:?}", scan.counted);
    }
}

#[test]
fn bare_sleep_calls_count_only_when_imported_from_a_time_module() {
    let tokio = "use tokio::time::{sleep, Instant};\n#[test]\nfn f() {\n    sleep(d);\n}\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, tokio)),
        [(4, Kind::TokioSleep)]
    );
    let until = "use tokio::time::sleep_until;\n#[test]\nfn f() {\n    sleep_until(t);\n}\n";
    assert_eq!(scan_one(DEMO, until).counted.len(), 1);
    let thread = "use std::thread::sleep;\n#[test]\nfn f() {\n    sleep(d);\n}\n";
    assert_eq!(
        lines_and_kinds(&scan_one(DEMO, thread)),
        [(4, Kind::ThreadSleep)]
    );
    let local = "use other::sleep;\n#[test]\nfn f() {\n    sleep(d);\n}\n";
    assert!(scan_one(DEMO, local).counted.is_empty());
}

#[test]
fn counts_below_the_baseline_fail_with_a_lower_the_baseline_message() {
    let text = "#[test]\nfn f() {\n    thread::sleep(d);\n}\n";
    let scan = scan_one(DEMO, text);
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
fn a_file_missing_from_the_baseline_may_not_gain_occurrences() {
    let text = "#[test]\nfn f() {\n    thread::sleep(d);\n    thread::sleep(d);\n}\n";
    let scan = scan_one(DEMO, text);
    let other = BTreeMap::from([("crates/other/src/lib.rs".to_owned(), 0)]);
    let failures = evaluate(&scan, &other);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("lib.rs:3"), "{failures:?}");
    assert!(failures[0].contains("lib.rs:4"), "{failures:?}");
    assert!(failures[0].contains("baseline allows 0"), "{failures:?}");
}

#[test]
fn the_baseline_text_round_trips() {
    let counts = BTreeMap::from([
        ("crates/a/src/lib.rs".to_owned(), 3),
        ("crates/b/tests/run.rs".to_owned(), 12),
    ]);
    let text = format_baseline(&counts);
    assert!(text.starts_with('#'));
    assert_eq!(parse_baseline(&text), counts);
}

#[test]
fn the_committed_baseline_is_sorted_and_has_no_zero_entries() {
    let text = fs::read_to_string(baseline_file()).expect("baseline exists");
    let baseline = parse_baseline(&text);
    assert!(baseline.values().all(|&count| count > 0), "zero entry");
    assert_eq!(text, format_baseline(&baseline), "baseline is canonical");
}
