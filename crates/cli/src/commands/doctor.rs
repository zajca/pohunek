//! `pohunek doctor` — environment health checks.
//!
//! Per `docs/plan-phase-1.md` "CLI Grammar": check Codex/Claude binaries, git,
//! socket-dir perms, and state-dir writability. (Schema-version check is part of
//! the SQLite milestone and is therefore reported as not-yet-available rather
//! than faked.)
//!
//! Exit status: non-zero if any *required* check fails. Agent binaries are
//! reported but their absence is a warning, not a hard failure, because a user
//! may run only one of the two agents.

use protocol::{DoctorCheck as Check, DoctorReport as Report, DoctorStatus as Status};

use crate::error::CliError;
use crate::paths::Paths;

/// Run `doctor`. Returns `true` if the environment is healthy (no fatal checks).
///
/// # Errors
///
/// Only returns an error if paths cannot be resolved at all; individual failed
/// checks are reported in the output, not returned as errors.
pub(crate) fn run(paths: &Paths, json: bool) -> Result<bool, CliError> {
    let checks = vec![
        // Required binaries on PATH.
        hostcheck::binary("git", true),
        // Agent binaries: reported, but missing is a warning, not fatal (a user
        // may run only one of the two agents).
        hostcheck::binary("codex", false),
        hostcheck::binary("claude", false),
        // Socket dir writability (where the daemon binds the socket).
        hostcheck::dir_writable(
            "socket_dir_writable",
            &paths.runtime_dir,
            "control socket directory",
        ),
        // State dir writability (state.db / events / worktrees live here).
        hostcheck::dir_writable(
            "state_dir_writable",
            &paths.data_dir,
            "state data directory",
        ),
        // Log dir writability.
        hostcheck::dir_writable("log_dir_writable", &paths.log_dir, "log directory"),
        // NetBird: optional. Its absence is a warning (local-only use is valid),
        // never a hard failure.
        hostcheck::netbird(),
        // Schema version: deferred to the SQLite milestone. Reported honestly as
        // unavailable rather than invented.
        Check::new(
            "schema_version",
            Status::Warn,
            "not available yet (SQLite store is a later milestone)",
        ),
        // Launcher prerequisites: the sway/rofi launcher (materialized by
        // `pohunek setup`) is OPTIONAL, so every check below is `warn` at worst
        // and never flips `overall` to fail.
        hostcheck::binary("rofi", false),
        hostcheck::binary("swaymsg", false),
        hostcheck::binary("python3", false),
        hostcheck::binary("timeout", false),
        hostcheck::terminal(),
        hostcheck::launcher_scripts(&paths.launcher_bin_dir()),
        hostcheck::sway_include(&paths.sway_config_dir()),
    ];

    let report = Report::from_checks(checks);

    if json {
        let line = serde_json::to_string_pretty(&report)?;
        println!("{line}");
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
