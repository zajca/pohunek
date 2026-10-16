//! `cargo xtask affected ci`: the test selection of a pull request's CI run.
//!
//! The repository is the Git top level of the working directory, which must be
//! its Cargo workspace root. The changed files are the committed diff between
//! the merge base of `--base` and `HEAD`; a CI checkout has no other changes.
//! They are classified in [`Mode::Ci`](super::Mode::Ci), closed over the
//! dependency graph and checked by the dependency diff exactly like the local
//! loop does.
//!
//! The command writes three files into `--out`:
//!
//! - `selection.json`: the mode, the nextest filterset, every selected package
//!   with its reasons, and every changed file with its decision;
//! - `outputs.env`: `key=value` lines for `$GITHUB_OUTPUT`;
//! - `summary.md`: the Markdown table for `$GITHUB_STEP_SUMMARY`.
//!
//! Every failure (Git, Cargo metadata, writing) exits non-zero. The previous
//! outputs are removed before any write, each file is renamed into place only
//! once it is complete, `selection.json` comes last, and a failed write removes
//! the outputs again, so a caller never reads a selection from a failed run;
//! the CI jobs treat a failed or missing selection as a full run. A failure of the dependency diff alone is not an
//! error: it selects every test.

// Rust guideline compliant 2026-10-16

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::{
    command_output, ensure_git_toplevel, filterset, graph, merge_base, parse_nul_list, plan,
    printable, resolve, workspace, Decision, Mode, Outcome, Plan, Reason, Reminder, GIT,
};
use crate::XtaskError;

/// Version of the `selection.json` shape; a consumer refuses any other.
const SCHEMA_VERSION: u32 = 1;

/// The output files; `selection.json` is written last, so it exists only
/// beside complete companions.
const SUMMARY: &str = "summary.md";
const OUTPUTS: &str = "outputs.env";
const SELECTION: &str = "selection.json";

/// Reasons listed per package in `summary.md`; `selection.json` holds all.
const SUMMARY_REASONS: usize = 4;

/// Options for one `cargo xtask affected ci` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Base revision of the pull request; the diff starts at its merge base with HEAD.
    #[arg(long, value_name = "REV")]
    base: String,
    /// Directory that receives `selection.json`, `outputs.env` and `summary.md`.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
}

/// Writes the selection of the change between `--base` and `HEAD`.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    std::fs::create_dir_all(&options.out).map_err(|source| XtaskError::Io {
        path: options.out.clone(),
        source,
    })?;
    remove_outputs(&options.out)?;
    let (root, packages) = workspace(&cwd)?;
    ensure_git_toplevel(&root)?;
    let merge_base = merge_base(&root, &options.base)?;
    let listing = command_output(
        &root,
        GIT,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &merge_base,
            "HEAD",
            "--",
        ],
    )?;
    let changed: BTreeSet<String> = parse_nul_list(&listing).collect();
    let plan = plan(&changed, &packages, Mode::Ci)?;
    let outcome = resolve(&plan, &packages, || {
        graph::manifest_diff(&root, &merge_base)
    });

    let report = Report::new(&plan, &outcome);
    let selection =
        serde_json::to_string_pretty(&report.json(&options.base, &merge_base, &plan, &outcome))
            .map_err(XtaskError::Json)?;
    let written = [
        (SUMMARY, report.summary(&merge_base, &plan, &outcome)),
        (OUTPUTS, report.outputs(&outcome)),
        (SELECTION, selection + "\n"),
    ]
    .iter()
    .try_for_each(|(name, text)| write_atomic(&options.out.join(name), text));
    if written.is_err() {
        // The write error is the one reported; a removal failing as well
        // changes nothing about the exit status.
        let _ = remove_outputs(&options.out);
    }
    written?;
    eprintln!(
        "affected ci: mode {}, {} changed files",
        report.mode,
        changed.len()
    );
    Ok(())
}

/// The mode and the flags the CI jobs branch on.
struct Report {
    /// `full`, `packages` or `none`.
    mode: &'static str,
    /// Every changed file has no Rust tests and no gate but `docs check`.
    docs_only: bool,
    script_tests: bool,
    sdk_gates: bool,
}

impl Report {
    fn new(plan: &Plan, outcome: &Outcome) -> Self {
        let mode = match outcome {
            Outcome::Everything(_) => "full",
            Outcome::Tests(_) => "packages",
            Outcome::Nothing => "none",
        };
        // A dependency input whose diff selects nothing is still a build input.
        let docs_only = plan
            .decisions
            .iter()
            .all(|(_, decision)| matches!(decision, Decision::NoRustTests(_)))
            && plan
                .reminders
                .iter()
                .all(|reminder| *reminder == Reminder::DocsCheck);
        Self {
            mode,
            docs_only,
            script_tests: mode == "full" || plan.reminders.contains(&Reminder::ScriptTests),
            sdk_gates: mode == "full" || plan.reminders.contains(&Reminder::SdkGates),
        }
    }

    fn json(
        &self,
        base: &str,
        merge_base: &str,
        plan: &Plan,
        outcome: &Outcome,
    ) -> serde_json::Value {
        let (filter, packages, causes) = match outcome {
            Outcome::Tests(targets) => {
                let mut packages = Vec::new();
                for (name, reasons) in &targets.whole {
                    packages.push(json!({
                        "name": name,
                        "tests": "all",
                        "reasons": reasons.iter().map(Reason::describe).collect::<Vec<_>>(),
                    }));
                }
                for (name, filters) in &targets.narrowed {
                    for (filter, reasons) in filters {
                        packages.push(json!({
                            "name": name,
                            "tests": filter,
                            "reasons": reasons,
                        }));
                    }
                }
                (json!(filterset(targets)), packages, Vec::new())
            }
            Outcome::Everything(causes) => (serde_json::Value::Null, Vec::new(), causes.clone()),
            Outcome::Nothing => (serde_json::Value::Null, Vec::new(), Vec::new()),
        };
        let files: Vec<serde_json::Value> = plan
            .decisions
            .iter()
            .map(|(path, decision)| json!({"path": path, "decision": decision.describe()}))
            .collect();
        json!({
            "schema_version": SCHEMA_VERSION,
            "base": base,
            "merge_base": merge_base,
            "mode": self.mode,
            "docs_only": self.docs_only,
            "script_tests": self.script_tests,
            "sdk_gates": self.sdk_gates,
            "filterset": filter,
            "full_causes": causes,
            "packages": packages,
            "files": files,
        })
    }

    /// `key=value` lines; every value is a single line.
    fn outputs(&self, outcome: &Outcome) -> String {
        let filter = match outcome {
            Outcome::Tests(targets) => filterset(targets),
            Outcome::Everything(_) | Outcome::Nothing => String::new(),
        };
        format!(
            "mode={}\ndocs_only={}\nscript_tests={}\nsdk_gates={}\nfilterset={}\n",
            self.mode, self.docs_only, self.script_tests, self.sdk_gates, filter
        )
    }

    fn summary(&self, merge_base: &str, plan: &Plan, outcome: &Outcome) -> String {
        let mut out = String::from("### Test selection\n\n");
        let _ = writeln!(
            out,
            "Mode: **{}**{}, merge base `{}`, {} changed files.\n",
            self.mode,
            if self.docs_only { " (docs only)" } else { "" },
            cell(merge_base),
            plan.decisions.len()
        );
        match outcome {
            Outcome::Everything(causes) => {
                out.push_str("Every test runs because of:\n\n");
                for cause in causes {
                    let _ = writeln!(out, "- {}", cell(cause));
                }
                out.push('\n');
            }
            Outcome::Nothing => out.push_str("No Rust test is affected.\n\n"),
            Outcome::Tests(targets) => {
                out.push_str("| Package | Tests | Reasons |\n| --- | --- | --- |\n");
                for (name, reasons) in &targets.whole {
                    let reasons: Vec<String> = reasons.iter().map(Reason::describe).collect();
                    let _ = writeln!(out, "| {name} | all | {} |", listed(&reasons));
                }
                for (name, filters) in &targets.narrowed {
                    for (filter, reasons) in filters {
                        let reasons: Vec<String> = reasons.iter().cloned().collect();
                        let _ = writeln!(
                            out,
                            "| {name} | `{}` | {} |",
                            cell(filter),
                            listed(&reasons)
                        );
                    }
                }
                out.push('\n');
            }
        }
        let _ = writeln!(
            out,
            "<details><summary>Changed files ({})</summary>\n",
            plan.decisions.len()
        );
        for (path, decision) in &plan.decisions {
            let _ = writeln!(out, "- `{}`: {}", cell(path), cell(&decision.describe()));
        }
        out.push_str("\n</details>\n");
        out
    }
}

/// At most [`SUMMARY_REASONS`] reasons, then a count of the rest.
fn listed(reasons: &[String]) -> String {
    let shown: Vec<String> = reasons
        .iter()
        .take(SUMMARY_REASONS)
        .map(|r| cell(r))
        .collect();
    let rest = reasons.len().saturating_sub(SUMMARY_REASONS);
    if rest == 0 {
        shown.join("; ")
    } else {
        format!("{}; and {rest} more", shown.join("; "))
    }
}

/// Text made safe for one Markdown table cell or code span.
///
/// Control characters are escaped and the characters that end a cell or a code
/// span are replaced, so a hostile file name cannot add rows or markup.
fn cell(text: &str) -> String {
    printable(text).replace('|', "\\|").replace('`', "'")
}

/// Removes the output files of an earlier run from `out`.
fn remove_outputs(out: &Path) -> Result<(), XtaskError> {
    for name in [SELECTION, OUTPUTS, SUMMARY] {
        let path = out.join(name);
        match std::fs::remove_file(&path) {
            Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
                return Err(XtaskError::Io { path, source });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Writes `text` to `path` through a sibling temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> Result<(), XtaskError> {
    let temporary = path.with_extension("partial");
    std::fs::write(&temporary, text).map_err(|source| XtaskError::Io {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}
