//! `pohunek doctor` — environment health checks.
//!
//! Per `docs/plan-phase-1.md` "CLI Grammar": check agent binaries, git,
//! socket-dir perms, and state-dir writability. (Schema-version check is part of
//! the `SQLite` milestone and is therefore reported as not-yet-available rather
//! than faked.)
//!
//! Exit status: non-zero if any *required* check fails. Agent binaries are
//! reported but their absence is a warning, not a hard failure, because a user
//! may run only one of the two agents.

use hostcheck::StandardCheckInputs;
use protocol::{DoctorReport as Report, DoctorStatus as Status};

use crate::commands::render_json;
use crate::error::CliError;
use crate::paths::Paths;

/// Run `doctor`. Returns `true` if the environment is healthy (no fatal checks).
///
/// # Errors
///
/// Only returns an error if paths cannot be resolved at all; individual failed
/// checks are reported in the output, not returned as errors.
pub(crate) fn run(paths: &Paths, json: bool) -> Result<bool, CliError> {
    let launcher_bin_dir = paths.launcher_bin_dir();
    let sway_config_dir = paths.sway_config_dir();
    let checks = hostcheck::standard_checks(StandardCheckInputs {
        socket_dir: &paths.runtime_dir,
        state_dir: &paths.data_dir,
        log_dir: &paths.log_dir,
        launcher_bin_dir: &launcher_bin_dir,
        sway_config_dir: &sway_config_dir,
    });

    let report = Report::from_checks(checks);

    if json {
        print!("{}", render_json(&report)?);
    } else {
        print_human(&report);
    }

    Ok(report.overall != Status::Fail)
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
            c.status.as_str(),
            c.detail,
            width = width
        );
    }
    println!();
    println!("overall: {}", report.overall.as_str());
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Per-test unique temp dir, namespaced by pid + a monotonic counter so
    /// parallel tests never collide.
    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("pohunek-doctor-test-{pid}-{n}"))
    }

    /// Build a `Paths` rooted at `base` (same-crate `pub(crate)` fields).
    fn paths_at(base: &Path) -> Paths {
        Paths {
            runtime_dir: base.join("runtime"),
            socket: base.join("runtime").join("daemon.sock"),
            data_dir: base.join("data"),
            log_dir: base.join("logs"),
            cache_dir: base.join("cache"),
            config_home: base.join("config"),
            config_dir: base.join("config").join("pohunek"),
        }
    }

    /// The CLI doctor wiring assembles a report (the individual host probes are
    /// covered in the `hostcheck` crate). Writable temp dirs make the
    /// directory-writability checks pass; the result is rendered as JSON without
    /// error.
    #[test]
    fn run_assembles_report_without_error() {
        let base = unique_temp_dir();
        let paths = paths_at(&base);

        let healthy = run(&paths, true).expect("doctor run resolves");
        // We assert only that the run completed and produced a boolean verdict;
        // the exact value depends on which binaries exist on the test host.
        let _ = healthy;

        let _ = std::fs::remove_dir_all(&base);
    }
}
