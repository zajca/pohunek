//! `zagentmesh doctor` — environment health checks.
//!
//! Per `docs/plan-phase-1.md` "CLI Grammar": check Codex/Claude binaries, git,
//! socket-dir perms, and state-dir writability. (Schema-version check is part of
//! the SQLite milestone and is therefore reported as not-yet-available rather
//! than faked.)
//!
//! Exit status: non-zero if any *required* check fails. Agent binaries are
//! reported but their absence is a warning, not a hard failure, because a user
//! may run only one of the two agents.

use std::path::Path;

use serde::Serialize;

use crate::error::CliError;
use crate::paths::Paths;

/// Outcome of a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    /// Check passed.
    Ok,
    /// Non-fatal: a capability is missing but the tool still works.
    Warn,
    /// Fatal: the tool cannot function correctly.
    Fail,
}

impl Status {
    fn symbol(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

/// One reported check.
#[derive(Debug, Clone, Serialize)]
struct Check {
    name: String,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

/// Aggregated doctor report.
#[derive(Debug, Serialize)]
struct Report {
    checks: Vec<Check>,
    /// Overall: `ok` if no failures, else `fail`.
    overall: Status,
}

/// Run `doctor`. Returns `true` if the environment is healthy (no fatal checks).
///
/// # Errors
///
/// Only returns an error if paths cannot be resolved at all; individual failed
/// checks are reported in the output, not returned as errors.
pub(crate) fn run(paths: &Paths, json: bool) -> Result<bool, CliError> {
    let checks = vec![
        // Required binaries on PATH.
        check_binary("git", true),
        // Agent binaries: reported, but missing is a warning, not fatal (a user
        // may run only one of the two agents).
        check_binary("codex", false),
        check_binary("claude", false),
        // Socket dir writability (where the daemon binds the socket).
        check_dir_writable(
            "socket_dir_writable",
            &paths.runtime_dir,
            "control socket directory",
        ),
        // State dir writability (state.db / events / worktrees live here).
        check_dir_writable("state_dir_writable", &paths.data_dir, "state data directory"),
        // Log dir writability.
        check_dir_writable("log_dir_writable", &paths.log_dir, "log directory"),
        // NetBird: optional. Its absence is a warning (local-only use is valid),
        // never a hard failure.
        check_netbird(),
        // Schema version: deferred to the SQLite milestone. Reported honestly as
        // unavailable rather than invented.
        Check::new(
            "schema_version",
            Status::Warn,
            "not available yet (SQLite store is a later milestone)",
        ),
    ];

    let overall = if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else {
        Status::Ok
    };
    let report = Report { checks, overall };

    if json {
        let line = serde_json::to_string_pretty(&report)?;
        println!("{line}");
    } else {
        print_human(&report);
    }

    Ok(report.overall != Status::Fail)
}

/// Check whether a binary is resolvable on `PATH`.
///
/// `required` controls whether absence is reported as `fail` or `warn`.
fn check_binary(name: &str, required: bool) -> Check {
    match which_on_path(name) {
        Some(path) => Check::new(
            format!("bin:{name}"),
            Status::Ok,
            format!("found at {}", path.display()),
        ),
        None => {
            let status = if required { Status::Fail } else { Status::Warn };
            Check::new(
                format!("bin:{name}"),
                status,
                format!("'{name}' not found on PATH"),
            )
        }
    }
}

/// Check NetBird availability.
///
/// NetBird is *optional*: remote hosts need it, but local-only use is fully
/// valid, so its absence is a `warn`, never a `fail`. When the CLI is present we
/// additionally probe local state — a resolvable self NetBird IP yields `ok`
/// (this host is on the mesh); an unreadable state (daemon down / not logged in)
/// is a `warn`, not a failure.
fn check_netbird() -> Check {
    if which_on_path("netbird").is_none() {
        return Check::new(
            "netbird_cli",
            Status::Warn,
            "'netbird' not found on PATH; NetBird is optional (remote hosts need it)",
        );
    }

    match netbird::run_status() {
        Ok(status) => match status.self_netbird_ip() {
            Some(ip) => Check::new(
                "netbird_cli",
                Status::Ok,
                format!("found; this host's NetBird IP is {ip}"),
            ),
            None => Check::new(
                "netbird_cli",
                Status::Warn,
                "found, but no NetBird IP resolved (not logged in or daemon down)",
            ),
        },
        Err(err) => Check::new(
            "netbird_cli",
            Status::Warn,
            format!("found, but local state is unavailable: {err}"),
        ),
    }
}

/// Resolve a binary name against the `PATH` environment variable.
///
/// A small dependency-free `which`: splits `PATH`, joins the name, and returns
/// the first entry that exists and is a regular file. Avoids adding a crate for
/// a one-call need.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Check that a directory exists (or can be created) and is writable, by
/// creating it and writing a probe file.
fn check_dir_writable(name: &str, dir: &Path, label: &str) -> Check {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Check::new(
            name,
            Status::Fail,
            format!("cannot create {label} {}: {err}", dir.display()),
        );
    }
    let probe = dir.join(".zagentmesh-doctor-probe");
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            // Best-effort cleanup; a leftover probe is harmless.
            let _ = std::fs::remove_file(&probe);
            Check::new(name, Status::Ok, format!("writable: {}", dir.display()))
        }
        Err(err) => Check::new(
            name,
            Status::Fail,
            format!("{label} {} is not writable: {err}", dir.display()),
        ),
    }
}

/// Render the report as an aligned human table.
fn print_human(report: &Report) {
    let width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0)
        .max("CHECK".len());

    println!("{:<width$}  STATUS  DETAIL", "CHECK", width = width);
    for c in &report.checks {
        println!(
            "{:<width$}  {:<6}  {}",
            c.name,
            c.status.symbol(),
            c.detail,
            width = width
        );
    }
    println!();
    println!("overall: {}", report.overall.symbol());
}
