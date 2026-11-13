//! The Claude OSC 7501 canary: compare captured status records against the
//! expected table.
//!
//! `compare` reads a `capture.json` (schema [`CANARY_SCHEMA`]) and the
//! expected table `compat/claude/canary/expected.json` and prints one row per
//! scenario with a verdict. A row whose expected value is `null` is
//! UNVERIFIED by definition and never a pass. Expectations change only by a
//! reviewed hand edit with its evidence, never from a run.
//!
//! Rust guideline compliant 2026-11-13

use std::fs;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::Value;

use crate::{XtaskError, FAILURE_EXIT_CODE};

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

/// The exit code of the `compare` command.
///
/// The `Usage` case completes the exit-code table of the command (a usage
/// error or an unreadable input exits 2); the implementation reports it as
/// [`XtaskError::Usage`], whose exit code is the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareOutcome {
    AllVerified,
    Mismatch,
    Unverified,
    #[expect(
        dead_code,
        reason = "completes the exit-code table; usage errors are reported as XtaskError::Usage"
    )]
    Usage,
}

impl CompareOutcome {
    /// The process exit code.
    #[must_use]
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::AllVerified => 0,
            Self::Mismatch => 1,
            Self::Usage => FAILURE_EXIT_CODE,
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

/// What `compare` prints: the rows and one diff line per differing leaf.
#[derive(Debug)]
struct CompareReport {
    rows: Vec<(String, RowVerdict)>,
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
    let mut rows = Vec::new();
    let mut diff_lines = Vec::new();
    let mut mismatched = 0_u32;
    let mut unverified = 0_u32;
    for scenario in SCENARIOS {
        let Some(expected_row) = expected_scenarios.get(scenario) else {
            continue;
        };
        let Some(observed_row) = capture_scenarios.get(scenario) else {
            rows.push((scenario.to_owned(), RowVerdict::Missing));
            diff_lines.push(format!(
                "- {scenario}: present in expected, absent in capture"
            ));
            mismatched += 1;
            continue;
        };
        // A row with a `null` value is UNVERIFIED by definition and never a
        // pass. A row that omits a compared field stores no expectation for
        // it, so it is UNVERIFIED as well.
        let expected_final = expected_row.get("final_record");
        let expected_attention = expected_row.get("attention");
        let complete = expected_final.is_some_and(Value::is_string)
            && expected_attention.is_some_and(Value::is_array);
        if !complete {
            rows.push((scenario.to_owned(), RowVerdict::Unverified));
            unverified += 1;
            continue;
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
        if row_diff.is_empty() {
            rows.push((scenario.to_owned(), RowVerdict::Verified));
        } else {
            rows.push((scenario.to_owned(), RowVerdict::Mismatch));
            diff_lines.extend(row_diff);
            mismatched += 1;
        }
    }
    let outcome = if mismatched > 0 {
        CompareOutcome::Mismatch
    } else if unverified > 0 {
        CompareOutcome::Unverified
    } else {
        CompareOutcome::AllVerified
    };
    Ok(CompareReport {
        rows,
        diff_lines,
        outcome,
    })
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

/// Refuses a capture that stores a message (`msg`/`title`/`progress` or raw
/// base64): the schema has no message field, and a stored message is a
/// policy breach. Canonical records never carry those fields.
fn policy_scan(capture: &Value) -> Result<(), String> {
    /// Keys a capture document must never store.
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
    Ok(())
}

/// Prints the table and the per-leaf diffs to stdout.
fn print_report(report: &CompareReport) {
    println!("scenario | expected | observed | verdict");
    for (scenario, verdict) in &report.rows {
        println!("{scenario} | expected | observed | {}", verdict.label());
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
