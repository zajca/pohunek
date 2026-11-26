//! Replays every fixture through every model and builds the comparison report.
//!
//! The report is deterministic: no timestamps, host data or paths.

// Rust guideline compliant 2026-06-26

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::ModelConfig;
use crate::expect::{Basis, Expect, ExpectError, Verdict};
use crate::model::{GridDump, GridModel, ModelError};
use crate::replay::{apply, replay, ReplayError};
use crate::trace::{Record, Trace, TraceError};

/// One fixture: a trace and its sidecar.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// File stem of the trace.
    pub name: String,
    /// The recorded bytes.
    pub trace: Trace,
    /// The spec-derived assertions.
    pub expect: Expect,
}

/// Builds a model at a size.
pub type ModelFactory = Box<dyn Fn(u16, u16) -> Result<Box<dyn GridModel>, ModelError>>;

/// A model the report covers.
pub struct ModelSpec {
    /// Name used in the report.
    pub name: &'static str,
    /// Creates the model at the trace's initial size.
    pub make: ModelFactory,
}

impl std::fmt::Debug for ModelSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSpec")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The report could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// A fixture file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file or directory.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// A trace file is malformed.
    #[error("{path}: {source}")]
    Trace {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: TraceError,
    },
    /// A sidecar is missing or invalid.
    #[error(transparent)]
    Expect(#[from] ExpectError),
    /// A trace has no sidecar.
    #[error("{0} has no .expect.toml next to it")]
    MissingSidecar(PathBuf),
    /// A replay failed.
    #[error("{trace} / {model}: {source}")]
    Replay {
        /// Trace name.
        trace: String,
        /// Model name.
        model: &'static str,
        /// The failure.
        source: ReplayError,
    },
    /// A model could not be created or could not repaint.
    #[error("{trace} / {model}: {source}")]
    Model {
        /// Trace name.
        trace: String,
        /// Model name.
        model: &'static str,
        /// The failure.
        source: ModelError,
    },
    /// A model reports a size other than the trace's last size.
    #[error("{trace} / {model}: dump is {found} but the trace ends at {expected}")]
    Dimensions {
        /// Trace name.
        trace: String,
        /// Model name.
        model: &'static str,
        /// Size the trace ends at.
        expected: String,
        /// Size the dump has.
        found: String,
    },
    /// The report could not be encoded.
    #[error("cannot encode the report: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Reads every `*.trace` and its `*.expect.toml` from `dir`, sorted by name.
///
/// # Errors
///
/// Returns a [`ReportError`] when a file cannot be read, a trace or sidecar is
/// invalid, or a trace has no sidecar.
pub fn load_fixtures(dir: &Path, config: &ModelConfig) -> Result<Vec<Fixture>, ReportError> {
    let read_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ReportError::Read { path, source }
    };
    let mut traces = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(read_err(dir))? {
        let path = entry.map_err(read_err(dir))?.path();
        if path.extension().is_some_and(|ext| ext == "trace") {
            traces.push(path);
        }
    }
    traces.sort();
    let limits = config.limits();
    traces
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).map_err(read_err(&path))?;
            let trace = Trace::parse(&bytes, &limits).map_err(|source| ReportError::Trace {
                path: path.clone(),
                source,
            })?;
            let sidecar = path.with_extension("expect.toml");
            if !sidecar.is_file() {
                return Err(ReportError::MissingSidecar(path));
            }
            let text = std::fs::read_to_string(&sidecar).map_err(read_err(&sidecar))?;
            let expect = Expect::parse(&text, &sidecar, config)?;
            let name = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            Ok(Fixture {
                name,
                trace,
                expect,
            })
        })
        .collect()
}

/// Outcome of one assertion for one model.
#[derive(Debug, Clone, Serialize)]
pub struct AssertionRow {
    /// Assertion id.
    pub id: String,
    /// Where the expected value comes from: `spec` or `pohunek-contract`.
    pub basis: String,
    /// The cited source.
    pub source: String,
    /// Width-method group, when the assertion depends on one.
    pub width_method: Option<String>,
    /// Model name to `pass` or the failure reason.
    pub outcome: BTreeMap<String, String>,
}

/// One difference between two models.
#[derive(Debug, Clone, Serialize)]
pub struct Difference {
    /// Records applied when the screens were compared.
    pub record: usize,
    /// What differs, such as `cell text` or `cursor`.
    pub what: String,
    /// Row, for a per-row or per-cell difference.
    pub row: Option<usize>,
    /// Column, for a per-cell difference.
    pub col: Option<usize>,
    /// Value in the first model of the pair.
    pub left: String,
    /// Value in the second model of the pair.
    pub right: String,
}

/// The differences of one model pair on one trace.
#[derive(Debug, Clone, Serialize)]
pub struct PairDiff {
    /// First model.
    pub left: String,
    /// Second model.
    pub right: String,
    /// Every difference, in comparison order.
    pub differences: Vec<Difference>,
}

/// Result of feeding a model's repaint to a fresh instance of itself.
#[derive(Debug, Clone, Serialize)]
pub struct RepaintRow {
    /// Model name.
    pub model: String,
    /// Bytes in the repaint, or `None` when the model offers none.
    pub repaint_bytes: Option<usize>,
    /// Differences between the original and the repainted screen.
    pub differences: Vec<Difference>,
}

/// Everything the report states about one trace.
#[derive(Debug, Clone, Serialize)]
pub struct TraceReport {
    /// Trace name.
    pub trace: String,
    /// Assertion outcomes.
    pub assertions: Vec<AssertionRow>,
    /// Pairwise screen differences.
    pub pairs: Vec<PairDiff>,
    /// Repaint round trips.
    pub repaint: Vec<RepaintRow>,
}

/// The whole report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Model names in column order.
    pub models: Vec<String>,
    /// One entry per fixture, sorted by name.
    pub traces: Vec<TraceReport>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat list of field comparisons reads better than helpers"
)]
fn diff_dumps(record: usize, left: &GridDump, right: &GridDump) -> Vec<Difference> {
    let mut out = Vec::new();
    let mut push = |what: &str, row, col, l: String, r: String| {
        if l != r {
            out.push(Difference {
                record,
                what: what.to_owned(),
                row,
                col,
                left: l,
                right: r,
            });
        }
    };
    push(
        "size",
        None,
        None,
        format!("{}x{}", left.rows, left.cols),
        format!("{}x{}", right.rows, right.cols),
    );
    push(
        "cursor",
        None,
        None,
        format!("{},{}", left.cursor_row, left.cursor_col),
        format!("{},{}", right.cursor_row, right.cursor_col),
    );
    push(
        "cursor visible",
        None,
        None,
        left.cursor_visible.to_string(),
        right.cursor_visible.to_string(),
    );
    push(
        "alternate screen",
        None,
        None,
        left.alternate_screen.to_string(),
        right.alternate_screen.to_string(),
    );
    push(
        "title",
        None,
        None,
        format!("{:?}", left.title),
        format!("{:?}", right.title),
    );
    push(
        "progress",
        None,
        None,
        format!("{:?}", left.progress),
        format!("{:?}", right.progress),
    );
    push(
        "scrollback rows",
        None,
        None,
        left.scrollback_rows.to_string(),
        right.scrollback_rows.to_string(),
    );
    push(
        "kitty keyboard flags",
        None,
        None,
        format!("{:?}", left.kitty_keyboard_flags),
        format!("{:?}", right.kitty_keyboard_flags),
    );
    push(
        "clipboard writes",
        None,
        None,
        format!("{:?}", left.clipboard_writes),
        format!("{:?}", right.clipboard_writes),
    );
    for row in 0..left.cells.len().min(right.cells.len()) {
        push(
            "row wrapped",
            Some(row),
            None,
            left.row_wrapped[row].to_string(),
            right.row_wrapped[row].to_string(),
        );
        for col in 0..left.cells[row].len().min(right.cells[row].len()) {
            let (l, r) = (&left.cells[row][col], &right.cells[row][col]);
            push(
                "cell text",
                Some(row),
                Some(col),
                format!("{:?}", l.text),
                format!("{:?}", r.text),
            );
            push(
                "cell width",
                Some(row),
                Some(col),
                l.width.to_string(),
                r.width.to_string(),
            );
            push(
                "spacer tail",
                Some(row),
                Some(col),
                l.spacer_tail.to_string(),
                r.spacer_tail.to_string(),
            );
            if let (Some(l), Some(r)) = (&left.hyperlinks, &right.hyperlinks) {
                push(
                    "hyperlink",
                    Some(row),
                    Some(col),
                    format!("{:?}", l[row][col]),
                    format!("{:?}", r[row][col]),
                );
            }
        }
    }
    out
}

/// A replay of one trace through one model, with a dump at every resize and at
/// the end.
struct Run {
    snapshots: Vec<(usize, GridDump)>,
    /// The screen after the last record.
    last: GridDump,
}

fn run_snapshots(
    fixture: &Fixture,
    spec: &ModelSpec,
) -> Result<(Run, Box<dyn GridModel>), ReportError> {
    let model_err = |source| ReportError::Model {
        trace: fixture.name.clone(),
        model: spec.name,
        source,
    };
    let replay_err = |source| ReportError::Replay {
        trace: fixture.name.clone(),
        model: spec.name,
        source: ReplayError::Model(source),
    };
    let mut model = (spec.make)(fixture.trace.rows, fixture.trace.cols).map_err(model_err)?;
    let mut snapshots = Vec::new();
    for (index, record) in fixture.trace.records.iter().enumerate() {
        apply(model.as_mut(), record);
        if matches!(record, Record::Resize { .. }) {
            snapshots.push((index + 1, model.dump().map_err(replay_err)?));
        }
    }
    let last = model.dump().map_err(replay_err)?;
    snapshots.push((fixture.trace.records.len(), last.clone()));
    Ok((Run { snapshots, last }, model))
}

/// Size the trace ends at.
fn final_size(trace: &Trace) -> (u16, u16) {
    trace
        .records
        .iter()
        .rev()
        .find_map(|record| match record {
            Record::Resize { rows, cols } => Some((*rows, *cols)),
            Record::Output(_) => None,
        })
        .unwrap_or((trace.rows, trace.cols))
}

/// Replays every fixture through every model.
///
/// # Errors
///
/// Returns a [`ReportError`] when a model cannot be built or dumped, a replay
/// names a moment the trace lacks, or a model's dump has a size other than the
/// size the trace ends at.
pub fn build_report(fixtures: &[Fixture], models: &[ModelSpec]) -> Result<Report, ReportError> {
    let mut traces = Vec::new();
    for fixture in fixtures {
        let (rows, cols) = final_size(&fixture.trace);
        let mut assertions: Vec<AssertionRow> = fixture
            .expect
            .assertion
            .iter()
            .map(|assertion| AssertionRow {
                id: assertion.id.clone(),
                basis: match assertion.basis {
                    Basis::Spec => "spec",
                    Basis::PohunekContract => "pohunek-contract",
                }
                .to_owned(),
                source: assertion.source.clone(),
                width_method: assertion.width_method.clone(),
                outcome: BTreeMap::new(),
            })
            .collect();
        let mut runs = Vec::new();
        let mut repaint = Vec::new();
        for spec in models {
            let (run, model) = run_snapshots(fixture, spec)?;
            let last = &run.last;
            if (last.rows, last.cols) != (rows, cols) {
                return Err(ReportError::Dimensions {
                    trace: fixture.name.clone(),
                    model: spec.name,
                    expected: format!("{rows}x{cols}"),
                    found: format!("{}x{}", last.rows, last.cols),
                });
            }
            let mut fresh =
                (spec.make)(fixture.trace.rows, fixture.trace.cols).map_err(|source| {
                    ReportError::Model {
                        trace: fixture.name.clone(),
                        model: spec.name,
                        source,
                    }
                })?;
            let replayed = replay(fresh.as_mut(), &fixture.trace, Some(&fixture.expect)).map_err(
                |source| ReportError::Replay {
                    trace: fixture.name.clone(),
                    model: spec.name,
                    source,
                },
            )?;
            for (row, outcome) in assertions.iter_mut().zip(&replayed.outcomes) {
                row.outcome.insert(
                    spec.name.to_owned(),
                    match &outcome.verdict {
                        Verdict::Pass => "pass".to_owned(),
                        Verdict::Fail(reason) => format!("FAIL: {reason}"),
                    },
                );
            }
            repaint.push(repaint_row(fixture, spec, model.as_ref(), last)?);
            runs.push((spec.name, run));
        }
        let mut pairs = Vec::new();
        for (i, (left_name, left)) in runs.iter().enumerate() {
            for (right_name, right) in &runs[i + 1..] {
                let mut differences = Vec::new();
                for ((record, l), (_, r)) in left.snapshots.iter().zip(&right.snapshots) {
                    differences.extend(diff_dumps(*record, l, r));
                }
                pairs.push(PairDiff {
                    left: (*left_name).to_owned(),
                    right: (*right_name).to_owned(),
                    differences,
                });
            }
        }
        traces.push(TraceReport {
            trace: fixture.name.clone(),
            assertions,
            pairs,
            repaint,
        });
    }
    Ok(Report {
        models: models.iter().map(|spec| spec.name.to_owned()).collect(),
        traces,
    })
}

fn repaint_row(
    fixture: &Fixture,
    spec: &ModelSpec,
    model: &dyn GridModel,
    original: &GridDump,
) -> Result<RepaintRow, ReportError> {
    let model_err = |source| ReportError::Model {
        trace: fixture.name.clone(),
        model: spec.name,
        source,
    };
    let Some(bytes) = model.repaint().map_err(model_err)? else {
        return Ok(RepaintRow {
            model: spec.name.to_owned(),
            repaint_bytes: None,
            differences: Vec::new(),
        });
    };
    let mut fresh = (spec.make)(original.rows, original.cols).map_err(model_err)?;
    fresh.feed(&bytes);
    let after = fresh.dump().map_err(model_err)?;
    Ok(RepaintRow {
        model: spec.name.to_owned(),
        repaint_bytes: Some(bytes.len()),
        differences: diff_dumps(fixture.trace.records.len(), original, &after),
    })
}

impl Report {
    /// The report as pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError::Encode`] when serialization fails.
    pub fn to_json(&self) -> Result<String, ReportError> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    /// The report as Markdown.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("# Spike report: screen models over PTY traces\n\n");
        let _ = writeln!(out, "Models: {}\n", self.models.join(", "));
        out.push_str("## Assertion outcomes\n\n");
        let mut totals: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for trace in &self.traces {
            for row in &trace.assertions {
                for (model, outcome) in &row.outcome {
                    let entry = totals.entry(model).or_default();
                    entry.1 += 1;
                    if outcome == "pass" {
                        entry.0 += 1;
                    }
                }
            }
        }
        out.push_str("| model | passed | total |\n|---|---|---|\n");
        for (model, (passed, total)) in &totals {
            let _ = writeln!(out, "| {model} | {passed} | {total} |");
        }
        for group in ["spec", "pohunek-contract"] {
            let _ = writeln!(out, "\n### Basis `{group}`\n");
            self.assertion_table(&mut out, |row| {
                row.basis == group && row.width_method.is_none()
            });
        }
        out.push_str("\n### Width-method assertions (not a plain correctness count)\n\n");
        self.assertion_table(&mut out, |row| row.width_method.is_some());
        out.push_str("\n## Differences between model pairs\n\n");
        for trace in &self.traces {
            for pair in &trace.pairs {
                let _ = writeln!(
                    out,
                    "### {} : {} vs {} ({} differences)\n",
                    trace.trace,
                    pair.left,
                    pair.right,
                    pair.differences.len()
                );
                for diff in &pair.differences {
                    let _ = writeln!(
                        out,
                        "- record {} {}{}: {} vs {}",
                        diff.record,
                        diff.what,
                        position(diff),
                        diff.left,
                        diff.right
                    );
                }
                out.push('\n');
            }
        }
        out.push_str("## Repaint round trips\n\n| trace | model | repaint bytes | differences |\n|---|---|---|---|\n");
        for trace in &self.traces {
            for row in &trace.repaint {
                let bytes = row
                    .repaint_bytes
                    .map_or("none".to_owned(), |b| b.to_string());
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} |",
                    trace.trace,
                    row.model,
                    bytes,
                    row.differences.len()
                );
            }
        }
        out
    }

    fn assertion_table(&self, out: &mut String, keep: impl Fn(&AssertionRow) -> bool) {
        let _ = writeln!(out, "| trace | assertion | {} |", self.models.join(" | "));
        let _ = writeln!(out, "|---|---|{}", "---|".repeat(self.models.len()));
        for trace in &self.traces {
            for row in trace.assertions.iter().filter(|row| keep(row)) {
                let cells: Vec<String> = self
                    .models
                    .iter()
                    .map(|model| {
                        row.outcome
                            .get(model)
                            .cloned()
                            .unwrap_or_default()
                            .replace('|', "\\|")
                    })
                    .collect();
                let _ = writeln!(
                    out,
                    "| {} | {} | {} |",
                    trace.trace,
                    row.id,
                    cells.join(" | ")
                );
            }
        }
    }
}

fn position(diff: &Difference) -> String {
    match (diff.row, diff.col) {
        (Some(row), Some(col)) => format!(" at row {row} col {col}"),
        (Some(row), None) => format!(" at row {row}"),
        _ => String::new(),
    }
}
