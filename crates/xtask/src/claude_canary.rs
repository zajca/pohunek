//! The Claude OSC 7501 canary: compare captured status records against the
//! expected table.
//!
//! `compare` reads a `capture.json` (schema [`CANARY_SCHEMA`]) and the
//! expected table `compat/claude/canary/expected.json` and prints one row per
//! scenario with the expected and observed `final_record` and `attention`
//! values and a verdict. A row whose expected value is `null` is
//! UNVERIFIED by definition and never a pass. Expectations change only by a
//! reviewed hand edit with its evidence, never from a run.
//!
//! Rust guideline compliant 2026-11-13

use std::fs;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::Value;

use crate::XtaskError;

/// Schema of `capture.json` and of the expected table.
///
/// The runbook `docs/runbooks/claude-canary.md` documents the schema.
const CANARY_SCHEMA: u32 = 1;

/// The runtime the canary compares.
const CANARY_RUNTIME: &str = "claude";

/// The scenarios, in table order.
const SCENARIOS: [&str; 5] = [
    "idle_after_turn",
    "permission_prompt",
    "mcp_elicitation",
    "login_screen",
    "running_turn",
];

/// Exit code of a compare that verified every row except UNVERIFIED ones.
const UNVERIFIED_EXIT_CODE: i32 = 3;

/// The top-level `claude canary` command.
#[derive(Debug, Subcommand)]
pub(crate) enum ClaudeAction {
    /// The Claude OSC 7501 canary.
    Canary {
        #[command(subcommand)]
        action: CanaryAction,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum CanaryAction {
    /// Compare a capture against the expected table.
    ///
    /// Exit codes: 0 all rows match, 1 at least one mismatch, 2 an
    /// unreadable input, 3 at least one row is UNVERIFIED and none mismatch.
    Compare {
        /// The expected table file (`compat/claude/canary/expected.json`).
        #[arg(long, value_name = "FILE")]
        expected: PathBuf,
        /// The capture file with the schema documented in the runbook.
        #[arg(long, value_name = "FILE")]
        capture: PathBuf,
    },
}

/// The exit code of the `compare` command, as a typed outcome of a run that
/// read both inputs. A usage error or an unreadable input is reported with
/// [`XtaskError::Usage`], whose exit code is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareOutcome {
    AllVerified,
    Mismatch,
    Unverified,
}

impl CompareOutcome {
    /// The process exit code.
    #[must_use]
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::AllVerified => 0,
            Self::Mismatch => 1,
            Self::Unverified => UNVERIFIED_EXIT_CODE,
        }
    }
}

/// The verdict of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowVerdict {
    Verified,
    Mismatch,
    Missing,
    Unverified,
}

impl RowVerdict {
    /// The table label.
    fn label(&self) -> &'static str {
        match self {
            Self::Verified => "VERIFIED",
            Self::Mismatch => "MISMATCH",
            Self::Missing => "MISSING",
            Self::Unverified => "UNVERIFIED",
        }
    }
}

/// One scenario row of the report: the expected and observed values and the
/// verdict. `expected` and `observed` are the `final_record` and `attention`
/// renderings shown in the row; `null` or `-` marks a row with no value
/// (UNVERIFIED or MISSING).
#[derive(Debug)]
struct Row {
    scenario: String,
    expected: String,
    observed: String,
    verdict: RowVerdict,
}

/// What `compare` prints: the rows and one diff line per differing leaf.
#[derive(Debug)]
struct CompareReport {
    claude_version: String,
    rows: Vec<Row>,
    diff_lines: Vec<String>,
    outcome: CompareOutcome,
}

/// Dispatch of the `claude canary` command.
pub(crate) fn run(action: ClaudeAction) -> Result<CompareOutcome, XtaskError> {
    match action {
        ClaudeAction::Canary { action } => match action {
            CanaryAction::Compare { expected, capture } => match compare(&expected, &capture) {
                Ok(report) => {
                    print_report(&report);
                    Ok(report.outcome)
                }
                Err(error) => Err(error),
            },
        },
    }
}

/// Compares a capture against the expected table and reports the outcome.
fn compare(expected_path: &Path, capture_path: &Path) -> Result<CompareReport, XtaskError> {
    let expected = read_json(expected_path)?;
    let capture = read_json(capture_path)?;
    check_schema(&expected, "the expected table")?;
    check_schema(&capture, "the capture")?;
    if let Err(message) = policy_scan(&capture) {
        return Err(XtaskError::Usage(message));
    }
    let expected_scenarios = expected
        .get("scenarios")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            XtaskError::Usage("the expected table has no `scenarios` object".to_owned())
        })?;
    let capture_scenarios = capture
        .get("scenarios")
        .and_then(Value::as_object)
        .ok_or_else(|| XtaskError::Usage("the capture has no `scenarios` object".to_owned()))?;
    // The expected table must name exactly the known scenarios: a hand edit
    // that drops or renames a row fails loudly here instead of a scenario
    // going unchecked. Extra names appear in the usage error.
    let missing: Vec<&str> = SCENARIOS
        .iter()
        .copied()
        .filter(|scenario| !expected_scenarios.contains_key(*scenario))
        .collect();
    let unknown: Vec<&str> = expected_scenarios
        .keys()
        .map(String::as_str)
        .filter(|name| !SCENARIOS.contains(name))
        .collect();
    if !missing.is_empty() || !unknown.is_empty() {
        return Err(XtaskError::Usage(format!(
            "the expected table must list exactly the known scenarios (missing: [{}]; unknown: [{}])",
            missing.join(", "),
            unknown.join(", ")
        )));
    }
    // `claude_version` is a required field of the capture schema: without it
    // the table pasted as evidence carries no version, so compare refuses
    // the capture instead of printing a placeholder.
    let claude_version = match capture.get("claude_version") {
        Some(Value::String(version)) => version.clone(),
        _ => {
            return Err(XtaskError::Usage(
                "the capture must carry a string `claude_version`".to_owned(),
            ));
        }
    };
    let mut rows = Vec::new();
    let mut diff_lines = Vec::new();
    let mut mismatched = 0_u32;
    let mut unverified = 0_u32;
    for scenario in SCENARIOS {
        let expected_row = expected_scenarios
            .get(scenario)
            .expect("exactly the known scenarios");
        let Some(observed_row) = capture_scenarios.get(scenario) else {
            rows.push(Row {
                scenario: scenario.to_owned(),
                expected: render_pair(expected_row),
                observed: "-".to_owned(),
                verdict: RowVerdict::Missing,
            });
            diff_lines.push(format!(
                "- {scenario}: present in expected, absent in capture"
            ));
            mismatched += 1;
            continue;
        };
        let (row, row_diff) = compare_row(scenario, expected_row, observed_row);
        match row.verdict {
            RowVerdict::Verified => {}
            RowVerdict::Unverified => unverified += 1,
            RowVerdict::Mismatch | RowVerdict::Missing => mismatched += 1,
        }
        diff_lines.extend(row_diff);
        rows.push(row);
    }
    let outcome = if mismatched > 0 {
        CompareOutcome::Mismatch
    } else if unverified > 0 {
        CompareOutcome::Unverified
    } else {
        CompareOutcome::AllVerified
    };
    Ok(CompareReport {
        claude_version,
        rows,
        diff_lines,
        outcome,
    })
}

/// Compares one scenario row and returns the row and its diff lines.
///
/// A row with a `null` value is UNVERIFIED by definition and never a pass.
/// A row that omits a compared field stores no expectation for it, so it is
/// UNVERIFIED as well.
fn compare_row(scenario: &str, expected_row: &Value, observed_row: &Value) -> (Row, Vec<String>) {
    let expected_final = expected_row.get("final_record");
    let expected_attention = expected_row.get("attention");
    let complete = expected_final.is_some_and(Value::is_string)
        && expected_attention.is_some_and(Value::is_array);
    if !complete {
        return (
            Row {
                scenario: scenario.to_owned(),
                expected: "null".to_owned(),
                observed: render_pair(observed_row),
                verdict: RowVerdict::Unverified,
            },
            Vec::new(),
        );
    }
    let mut row_diff = Vec::new();
    collect_leaf_diffs(
        scenario,
        expected_final,
        observed_row.get("final_record"),
        "final_record",
        &mut row_diff,
    );
    collect_leaf_diffs(
        scenario,
        expected_attention,
        observed_row.get("attention"),
        "attention",
        &mut row_diff,
    );
    let verdict = if row_diff.is_empty() {
        RowVerdict::Verified
    } else {
        RowVerdict::Mismatch
    };
    (
        Row {
            scenario: scenario.to_owned(),
            expected: render_pair(expected_row),
            observed: render_pair(observed_row),
            verdict,
        },
        row_diff,
    )
}

/// Requires `schema` 1 and the `claude` runtime of a canary document.
fn check_schema(document: &Value, what: &str) -> Result<(), XtaskError> {
    let schema = document.get("schema").and_then(Value::as_u64);
    if schema != Some(u64::from(CANARY_SCHEMA)) {
        return Err(XtaskError::Usage(format!(
            "{what} must carry \"schema\": {CANARY_SCHEMA}"
        )));
    }
    if document.get("runtime").and_then(Value::as_str) != Some(CANARY_RUNTIME) {
        return Err(XtaskError::Usage(format!(
            "{what} must carry \"runtime\": \"{CANARY_RUNTIME}\""
        )));
    }
    Ok(())
}

/// Appends one `-`/`+` line per differing leaf below `name`.
fn collect_leaf_diffs(
    scenario: &str,
    expected: Option<&Value>,
    observed: Option<&Value>,
    name: &str,
    lines: &mut Vec<String>,
) {
    match (expected, observed) {
        (Some(expected), Some(observed)) => {
            collect_values(scenario, expected, observed, name, lines);
        }
        (Some(expected), None) => lines.push(format!("- {scenario}/{name}: {expected}")),
        (None, Some(observed)) => lines.push(format!("+ {scenario}/{name}: {observed}")),
        (None, None) => {}
    }
}

/// Walks two JSON values and appends one `-`/`+` line per differing leaf.
/// Arrays compare by index. A key present on one side prints only its side.
fn collect_values(
    scenario: &str,
    expected: &Value,
    observed: &Value,
    name: &str,
    lines: &mut Vec<String>,
) {
    match (expected, observed) {
        (Value::Object(expected_map), Value::Object(observed_map)) => {
            let mut keys: Vec<&String> = expected_map.keys().chain(observed_map.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let child = format!("{name}/{key}");
                collect_leaf_diffs(
                    scenario,
                    expected_map.get(key),
                    observed_map.get(key),
                    &child,
                    lines,
                );
            }
        }
        (Value::Array(expected_items), Value::Array(observed_items)) => {
            let span = expected_items.len().max(observed_items.len());
            for index in 0..span {
                let child = format!("{name}/{index}");
                collect_leaf_diffs(
                    scenario,
                    expected_items.get(index),
                    observed_items.get(index),
                    &child,
                    lines,
                );
            }
        }
        _ => {
            if expected != observed {
                lines.push(format!("- {scenario}/{name}: {expected}"));
                lines.push(format!("+ {scenario}/{name}: {observed}"));
            }
        }
    }
}

/// `state` values of a canonical record.
const STATES: [&str; 6] = ["working", "blocked", "done", "idle", "error", "clear"];

/// `kind` values of a blocked record.
const KINDS: [&str; 3] = ["permission", "question", "auth"];

/// Renders the compared fields of one scenario row, for example
/// `state=done []`. A missing or non-JSON value renders as `null`.
fn render_pair(row: &Value) -> String {
    let final_record = match row.get("final_record") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => "null".to_owned(),
        Some(other) => other.to_string(),
    };
    let attention = match row.get("attention") {
        Some(Value::Array(items)) => {
            let names: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect();
            format!("[{}]", names.join(", "))
        }
        Some(Value::Null) | None => "null".to_owned(),
        Some(other) => other.to_string(),
    };
    format!("{final_record} {attention}")
}

/// Refuses a capture that stores a message (`msg`/`title`/`progress` or a raw
/// base64 message tail). The schema has no message field, and a stored
/// message is a policy breach. Every `records[]` and `final_record` value
/// must match the canonical record grammar
/// `[app=<app>:]state=<state>[:kind=<kind>]`; a `msg=` segment or any other
/// unknown segment fails the validation. A `records` that is not an array of
/// strings, a `final_record` that is neither a string nor `null` and an
/// `attention` that is not an array of strings are refused as well, so no
/// other shape can slip a message past the scan.
fn policy_scan(capture: &Value) -> Result<(), String> {
    /// Keys a capture document must never carry.
    const FORBIDDEN_KEYS: [&str; 3] = ["msg", "title", "progress"];
    let mut stack = vec![capture];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if FORBIDDEN_KEYS.contains(&key.as_str()) {
                        return Err(format!(
                            "the capture stores a `{key}` field: the capture schema has no message; a stored message is a policy breach"
                        ));
                    }
                    stack.push(child);
                }
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    if let Some(scenarios) = capture.get("scenarios").and_then(Value::as_object) {
        for (name, row) in scenarios {
            if let Some(records_value) = row.get("records") {
                let Some(records) = records_value.as_array() else {
                    return Err(format!(
                        "the capture scenario `{name}` carries a `records` that is not an array of records"
                    ));
                };
                for record in records {
                    if let Some(text) = record.as_str() {
                        check_record(text, name)?;
                    } else {
                        return Err(format!(
                            "the capture scenario `{name}` carries a non-string record"
                        ));
                    }
                }
            }
            if let Some(final_value) = row.get("final_record") {
                match final_value {
                    Value::String(text) => check_record(text, name)?,
                    Value::Null => {}
                    _ => {
                        return Err(format!(
                            "the capture scenario `{name}` carries a `final_record` that is neither a string nor null"
                        ));
                    }
                }
            }
            if let Some(attention_value) = row.get("attention") {
                let Some(attention) = attention_value.as_array() else {
                    return Err(format!(
                        "the capture scenario `{name}` carries an `attention` that is not an array of strings"
                    ));
                };
                if attention.iter().any(|item| !item.is_string()) {
                    return Err(format!(
                        "the capture scenario `{name}` carries a non-string attention entry"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Checks one record string against the canonical grammar
/// `[app=<app>:]state=<state>[:kind=<kind>]`.
fn check_record(record: &str, scenario: &str) -> Result<(), String> {
    let mut state = false;
    let mut kind = false;
    for pair in record.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!(
                "the capture scenario `{scenario}` carries an unkeyed record segment `{pair}`"
            ));
        };
        if key.is_empty() {
            return Err(format!(
                "the capture scenario `{scenario}` carries a record segment with an empty key"
            ));
        }
        match key {
            "app" => {
                if !is_valid_app(value) {
                    return Err(format!(
                        "the capture scenario `{scenario}` carries an invalid `app` in `{record}`"
                    ));
                }
            }
            "state" => {
                if !STATES.contains(&value) {
                    return Err(format!(
                        "the capture scenario `{scenario}` carries an unknown `state` in `{record}`"
                    ));
                }
                state = true;
            }
            "kind" => {
                if !KINDS.contains(&value) {
                    return Err(format!(
                        "the capture scenario `{scenario}` carries an unknown `kind` in `{record}`"
                    ));
                }
                kind = true;
            }
            _ => {
                return Err(format!(
                    "the capture scenario `{scenario}` carries the `{key}` segment in `{record}`: a canonical record has no message"
                ));
            }
        }
    }
    if !state {
        return Err(format!(
            "the capture scenario `{scenario}` carries a record without `state=`: `{record}`"
        ));
    }
    if kind && !record.contains("state=blocked") {
        return Err(format!(
            "the capture scenario `{scenario}` carries a `kind` without `state=blocked`: `{record}`"
        ));
    }
    Ok(())
}

/// Whether `value` is a valid `app` name: 1 to 32 characters of
/// `[A-Za-z0-9_.+-]`, as the daemon's record parser accepts.
fn is_valid_app(value: &str) -> bool {
    const MAX_APP_BYTES: usize = 32;
    !value.is_empty()
        && value.len() <= MAX_APP_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
}

/// Prints the capture version, the table and the per-leaf diffs to stdout.
fn print_report(report: &CompareReport) {
    println!("claude_version: {}", report.claude_version);
    println!("scenario | expected | observed | verdict");
    for row in &report.rows {
        println!(
            "{} | {} | {} | {}",
            row.scenario,
            row.expected,
            row.observed,
            row.verdict.label()
        );
    }
    for line in &report.diff_lines {
        println!("{line}");
    }
}

/// Reads and parses a JSON file, mapping every failure to a usage error.
fn read_json(path: &Path) -> Result<Value, XtaskError> {
    let bytes = fs::read(path)
        .map_err(|error| XtaskError::Usage(format!("cannot read `{}`: {error}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| XtaskError::Usage(format!("cannot parse `{}`: {error}", path.display())))
}
