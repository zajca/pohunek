//! Numeric facts the docs state about the protocol and the store, held to the
//! constants that define them.
//!
//! The constants are read from the source text of the checkout, so a fixture
//! can change one. Four statement shapes are recognised, each in a paragraph
//! (a block of non-blank lines) of a page the command checks read:
//!
//! - the public protocol window written as a backticked `N..=M` range, in a
//!   paragraph about the protocol whose text just before the range names the
//!   window ([`WINDOW_CONTEXT`]);
//! - `` `PROTOCOL_VERSION` (N) `` and `` `MIN_PROTOCOL_VERSION` is `N` ``;
//! - the current store schema version;
//! - the worker protocol window `PREVIOUS_VERSION..=CURRENT_VERSION`.
//!
//! Historical mentions ("store schema 20"), config ranges and the window of an
//! older protocol do not match any shape. A source declaration the parsers do
//! not recognise is a failure that names the source line, never a silent pass.

// Rust guideline compliant 2026-11-20

use std::path::Path;

use regex::Regex;

use super::{is_exempt_page, is_retired, markdown_files, read_page, COMMAND_SOURCES};
use crate::XtaskError;

/// Source of `PROTOCOL_VERSION` and `MIN_PROTOCOL_VERSION`.
const PROTOCOL_SOURCE: &str = "crates/protocol/src/version.rs";

/// Source of the worker protocol `CURRENT_VERSION` and `PREVIOUS_VERSION`.
const WORKER_SOURCE: &str = "crates/worker-protocol/src/version.rs";

/// Source of `STORE_SCHEMA_VERSION`.
const STORE_SOURCE: &str = "crates/daemon/src/store/schema.rs";

/// `pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(<int>);`
const PROTOCOL_DECLARATION: &str =
    r"(?m)^pub const PROTOCOL_VERSION:\s*ProtocolVersion\s*=\s*ProtocolVersion\((\d+)\);";

/// `MIN_PROTOCOL_VERSION` as an integer, as `PROTOCOL_VERSION.0`, or as
/// `PROTOCOL_VERSION.0 - <int>`.
const MIN_PROTOCOL_DECLARATION: &str = r"(?m)^pub const MIN_PROTOCOL_VERSION:\s*ProtocolVersion\s*=\s*ProtocolVersion\((?:(\d+)|PROTOCOL_VERSION\.0(?:\s*-\s*(\d+))?)\);";

/// `pub const CURRENT_VERSION: Version = Version(<int>);`
const WORKER_CURRENT_DECLARATION: &str =
    r"(?m)^pub const CURRENT_VERSION:\s*Version\s*=\s*Version\((\d+)\);";

/// `pub const PREVIOUS_VERSION: Version = Version(<int>);`
const WORKER_PREVIOUS_DECLARATION: &str =
    r"(?m)^pub const PREVIOUS_VERSION:\s*Version\s*=\s*Version\((\d+)\);";

/// `pub const STORE_SCHEMA_VERSION: u32 = <int>;`
const STORE_DECLARATION: &str = r"(?m)^pub const STORE_SCHEMA_VERSION:\s*u32\s*=\s*(\d+);";

/// A backticked public protocol range.
const WINDOW_STATEMENT: &str = r"`(\d+)\.\.=(\d+)`";

/// Words that mark a protocol paragraph's range as the current window when
/// they appear in the [`WINDOW_CONTEXT_BYTES`] before the range.
const WINDOW_CONTEXT: [&str; 7] = [
    "window",
    "SUPPORTED_PROTOCOL_VERSIONS",
    "CLIENT_PROTOCOL_VERSIONS",
    "accepts",
    "today",
    "CLI's",
    "client or CLI",
];

/// Bytes before a range searched for [`WINDOW_CONTEXT`].
const WINDOW_CONTEXT_BYTES: usize = 60;

/// A paragraph must contain this word (case-insensitive) for its ranges to
/// count as protocol windows.
const WINDOW_TOPIC: &str = "protocol";

/// `` `PROTOCOL_VERSION` (N) `` or `` `MIN_PROTOCOL_VERSION` is `N` ``.
const CONSTANT_STATEMENT: &str =
    r"`(MIN_PROTOCOL_VERSION|PROTOCOL_VERSION)`\s*(?:is\s*`(\d+)`|\((\d+)\))";

/// The current store schema version, named by its constant or in words.
const STORE_STATEMENT: &str =
    r"(?i)(?:STORE_SCHEMA_VERSION`?.{0,90}?currently\s+|current store schema is\s+)(\d+)";

/// The worker window with its two current values.
const WORKER_STATEMENT: &str =
    r"PREVIOUS_VERSION\.\.=CURRENT_VERSION`?.{0,90}?currently\s+(\d+)\s+and\s+(\d+)";

/// The values the sources declare.
struct Facts {
    protocol_min: u64,
    protocol_max: u64,
    worker_previous: u64,
    worker_current: u64,
    store_schema: u64,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid numeric fact regex")
}

/// The first capture group of `pattern` in `source` as a number, or a problem
/// naming the source.
fn declared(source: &str, content: &str, name: &str, pattern: &str) -> Result<u64, String> {
    let captures = regex(pattern)
        .captures(content)
        .ok_or_else(|| format!("{source}: no declaration of {name} in a recognised shape"))?;
    let digits = captures
        .get(1)
        .ok_or_else(|| format!("{source}: {name} has no integer value"))?;
    digits
        .as_str()
        .parse()
        .map_err(|_overflow| format!("{source}: {name} is out of range"))
}

/// `MIN_PROTOCOL_VERSION` given the declared `PROTOCOL_VERSION`.
fn declared_minimum(content: &str, maximum: u64) -> Result<u64, String> {
    let captures = regex(MIN_PROTOCOL_DECLARATION)
        .captures(content)
        .ok_or_else(|| {
            format!(
                "{PROTOCOL_SOURCE}: no declaration of MIN_PROTOCOL_VERSION in a recognised shape"
            )
        })?;
    if let Some(value) = captures.get(1) {
        return value.as_str().parse().map_err(|_overflow| {
            format!("{PROTOCOL_SOURCE}: MIN_PROTOCOL_VERSION is out of range")
        });
    }
    let behind: u64 = match captures.get(2) {
        Some(value) => value.as_str().parse().map_err(|_overflow| {
            format!("{PROTOCOL_SOURCE}: MIN_PROTOCOL_VERSION is out of range")
        })?,
        None => 0,
    };
    maximum
        .checked_sub(behind)
        .ok_or_else(|| format!("{PROTOCOL_SOURCE}: MIN_PROTOCOL_VERSION counts back past zero"))
}

/// Reads the constants; a declaration that cannot be parsed is returned as the
/// list of problems.
fn read_facts(repo: &Path) -> Result<Result<Facts, Vec<String>>, XtaskError> {
    let protocol = read_page(repo, Path::new(PROTOCOL_SOURCE))?;
    let worker = read_page(repo, Path::new(WORKER_SOURCE))?;
    let store = read_page(repo, Path::new(STORE_SOURCE))?;
    let mut problems = Vec::new();
    let mut take = |value: Result<u64, String>| match value {
        Ok(value) => value,
        Err(problem) => {
            problems.push(problem);
            0
        }
    };
    let protocol_max = take(declared(
        PROTOCOL_SOURCE,
        &protocol,
        "PROTOCOL_VERSION",
        PROTOCOL_DECLARATION,
    ));
    let protocol_min = take(declared_minimum(&protocol, protocol_max));
    let worker_current = take(declared(
        WORKER_SOURCE,
        &worker,
        "CURRENT_VERSION",
        WORKER_CURRENT_DECLARATION,
    ));
    let worker_previous = take(declared(
        WORKER_SOURCE,
        &worker,
        "PREVIOUS_VERSION",
        WORKER_PREVIOUS_DECLARATION,
    ));
    let store_schema = take(declared(
        STORE_SOURCE,
        &store,
        "STORE_SCHEMA_VERSION",
        STORE_DECLARATION,
    ));
    if !problems.is_empty() {
        return Ok(Err(problems));
    }
    Ok(Ok(Facts {
        protocol_min,
        protocol_max,
        worker_previous,
        worker_current,
        store_schema,
    }))
}

/// A page's paragraphs as (first line number, text with newlines replaced by
/// spaces, so offsets stay byte offsets of the page paragraph).
fn paragraphs(content: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            if let Some(done) = current.take() {
                found.push(done);
            }
            continue;
        }
        match current.as_mut() {
            Some((_, text)) => {
                text.push(' ');
                text.push_str(line);
            }
            None => current = Some((index + 1, line.to_owned())),
        }
    }
    if let Some(done) = current {
        found.push(done);
    }
    found
}

/// The line of byte `offset` of a paragraph that starts at `first_line`; the
/// paragraph's lines are as long as they were before joining plus one space,
/// so the line is found from the original lines.
fn line_of(content_lines: &[&str], first_line: usize, offset: usize) -> usize {
    let mut remaining = offset;
    let mut line = first_line;
    for text in &content_lines[first_line - 1..] {
        if remaining <= text.len() {
            return line;
        }
        remaining -= text.len() + 1;
        line += 1;
    }
    line
}

/// One statement found on a page: its line, what it states and the value the
/// sources declare.
struct Statement {
    line: usize,
    stated: String,
    declared: String,
    constant: &'static str,
    source: &'static str,
    matches: bool,
}

fn statements(content: &str, facts: &Facts) -> Vec<Statement> {
    let window = regex(WINDOW_STATEMENT);
    let constant = regex(CONSTANT_STATEMENT);
    let store = regex(STORE_STATEMENT);
    let worker = regex(WORKER_STATEMENT);
    let lines: Vec<&str> = content.lines().collect();
    let mut found = Vec::new();
    for (first_line, text) in paragraphs(content) {
        let lower = text.to_lowercase();
        for range in window.captures_iter(&text) {
            let whole = range.get(0).expect("whole match");
            let before = &text[..whole.start()];
            let preceding_start = (0..=before.len().saturating_sub(WINDOW_CONTEXT_BYTES))
                .rev()
                .find(|index| before.is_char_boundary(*index))
                .unwrap_or(0);
            let preceding = before[preceding_start..].to_lowercase();
            if !lower.contains(WINDOW_TOPIC)
                || !WINDOW_CONTEXT
                    .iter()
                    .any(|word| preceding.contains(&word.to_lowercase()))
            {
                continue;
            }
            let stated: (u64, u64) = (
                range[1].parse().unwrap_or(u64::MAX),
                range[2].parse().unwrap_or(u64::MAX),
            );
            found.push(Statement {
                line: line_of(&lines, first_line, whole.start()),
                stated: format!("the protocol window {}..={}", stated.0, stated.1),
                declared: format!("{}..={}", facts.protocol_min, facts.protocol_max),
                constant: "MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION",
                source: PROTOCOL_SOURCE,
                matches: stated == (facts.protocol_min, facts.protocol_max),
            });
        }
        for statement in constant.captures_iter(&text) {
            let whole = statement.get(0).expect("whole match");
            let value: u64 = statement
                .get(2)
                .or_else(|| statement.get(3))
                .and_then(|digits| digits.as_str().parse().ok())
                .unwrap_or(u64::MAX);
            let (name, declared) = if &statement[1] == "MIN_PROTOCOL_VERSION" {
                ("MIN_PROTOCOL_VERSION", facts.protocol_min)
            } else {
                ("PROTOCOL_VERSION", facts.protocol_max)
            };
            found.push(Statement {
                line: line_of(&lines, first_line, whole.start()),
                stated: format!("{name} {value}"),
                declared: declared.to_string(),
                constant: name,
                source: PROTOCOL_SOURCE,
                matches: value == declared,
            });
        }
        for statement in store.captures_iter(&text) {
            let whole = statement.get(0).expect("whole match");
            let value: u64 = statement[1].parse().unwrap_or(u64::MAX);
            found.push(Statement {
                line: line_of(&lines, first_line, whole.start()),
                stated: format!("the current store schema {value}"),
                declared: facts.store_schema.to_string(),
                constant: "STORE_SCHEMA_VERSION",
                source: STORE_SOURCE,
                matches: value == facts.store_schema,
            });
        }
        for statement in worker.captures_iter(&text) {
            let whole = statement.get(0).expect("whole match");
            let previous: u64 = statement[1].parse().unwrap_or(u64::MAX);
            let current: u64 = statement[2].parse().unwrap_or(u64::MAX);
            found.push(Statement {
                line: line_of(&lines, first_line, whole.start()),
                stated: format!("the worker window {previous} and {current}"),
                declared: format!("{} and {}", facts.worker_previous, facts.worker_current),
                constant: "PREVIOUS_VERSION and CURRENT_VERSION",
                source: WORKER_SOURCE,
                matches: (previous, current) == (facts.worker_previous, facts.worker_current),
            });
        }
    }
    found
}

/// Wrong statements as `<page>:<line> states ...` and the number of statements
/// checked. A source declaration that cannot be parsed is one problem per
/// declaration and nothing is checked.
fn scan(repo: &Path) -> Result<(Vec<String>, usize), XtaskError> {
    let facts = match read_facts(repo)? {
        Ok(facts) => facts,
        Err(problems) => return Ok((problems, 0)),
    };
    let mut problems = Vec::new();
    let mut checked = 0usize;
    for source in COMMAND_SOURCES {
        for relative in markdown_files(repo, source)? {
            if is_exempt_page(&relative) {
                continue;
            }
            let content = read_page(repo, &relative)?;
            if is_retired(&content) {
                continue;
            }
            for statement in statements(&content, &facts) {
                checked += 1;
                if !statement.matches {
                    problems.push(format!(
                        "{}:{} states {}, but {} in {} is {}",
                        relative.display(),
                        statement.line,
                        statement.stated,
                        statement.constant,
                        statement.source,
                        statement.declared,
                    ));
                }
            }
        }
    }
    Ok((problems, checked))
}

/// The statements that differ from their constants, one string each.
pub(super) fn problems(repo: &Path) -> Result<Vec<String>, XtaskError> {
    Ok(scan(repo)?.0)
}

/// Prints the `numeric-facts` result line and the problems; returns whether
/// every statement matches.
pub(super) fn check(repo: &Path) -> Result<bool, XtaskError> {
    let (problems, checked) = scan(repo)?;
    if problems.is_empty() {
        println!("[PASS] numeric-facts: {checked} statement(s) match their constants");
        return Ok(true);
    }
    println!("[FAIL] numeric-facts: the docs state numbers that differ from their constants:");
    for problem in &problems {
        println!("        {problem}");
    }
    Ok(false)
}
