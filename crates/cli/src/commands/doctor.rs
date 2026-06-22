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
        check_dir_writable(
            "state_dir_writable",
            &paths.data_dir,
            "state data directory",
        ),
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
        // Launcher prerequisites: the sway/rofi launcher (materialized by
        // `pohunek setup`) is OPTIONAL, so every check below is `warn` at worst
        // and never flips `overall` to fail.
        check_binary("rofi", false),
        check_binary("swaymsg", false),
        check_binary("python3", false),
        check_binary("timeout", false),
        check_terminal(),
        check_launcher_scripts(paths),
        check_sway_include(paths),
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

/// Check that a terminal emulator is configured for the rofi launcher.
///
/// The launcher needs a terminal to spawn agent sessions in. `ok` when
/// `$TERMINAL` is set and non-empty; otherwise `warn` (the launcher also reads
/// a `terminal=` key from `launcher.conf`, so this is optional).
fn check_terminal() -> Check {
    match std::env::var("TERMINAL") {
        Ok(value) if !value.is_empty() => {
            Check::new("terminal", Status::Ok, format!("TERMINAL={value}"))
        }
        _ => Check::new(
            "terminal",
            Status::Warn,
            "set $TERMINAL or 'terminal=' in launcher.conf (the rofi launcher needs a terminal)",
        ),
    }
}

/// Check whether the launcher scripts have been materialized by
/// `pohunek setup scripts`. `ok` when the `pohunek-rofi` entrypoint is present;
/// otherwise `warn` (the launcher is optional).
fn check_launcher_scripts(paths: &Paths) -> Check {
    let bin_dir = paths.launcher_bin_dir();
    if bin_dir.join("pohunek-rofi").is_file() {
        Check::new(
            "launcher_scripts",
            Status::Ok,
            format!("installed at {}", bin_dir.display()),
        )
    } else {
        Check::new(
            "launcher_scripts",
            Status::Warn,
            "not installed; run 'pohunek setup scripts'",
        )
    }
}

/// Check whether the user's sway config includes a `config.d` drop-in dir, which
/// is where `pohunek setup sway` writes its launcher keybinding drop-in.
///
/// `ok` when the main config exists and has a non-comment line mentioning
/// `config.d`; `warn` when the config exists without such a line, or when it is
/// absent entirely. The launcher is optional, so this is never fatal.
fn check_sway_include(paths: &Paths) -> Check {
    let config = paths.sway_config_dir().join("config");
    match std::fs::read_to_string(&config) {
        Ok(contents) => {
            // A "non-comment line mentioning config.d": trim each line, skip
            // comment lines, and look for the `config.d` token.
            let includes = contents.lines().any(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with('#') && trimmed.contains("config.d")
            });
            if includes {
                Check::new(
                    "sway_include",
                    Status::Ok,
                    "sway config includes config.d",
                )
            } else {
                Check::new(
                    "sway_include",
                    Status::Warn,
                    format!(
                        "add 'include {}/config.d/*' to your sway config (see 'pohunek setup sway')",
                        paths.sway_config_dir().display()
                    ),
                )
            }
        }
        Err(_) => Check::new(
            "sway_include",
            Status::Warn,
            format!("sway config not found at {}", config.display()),
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
    let probe = dir.join(".pohunek-doctor-probe");
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

    /// Build a `Paths` rooted at `base` (same-crate `pub(crate)` fields). Only
    /// `config_home` matters for `check_sway_include`, but all fields are filled
    /// with sensible temp paths.
    fn paths_at(base: &Path) -> Paths {
        Paths {
            runtime_dir: base.join("runtime"),
            socket: base.join("runtime").join("daemon.sock"),
            data_dir: base.join("data"),
            log_dir: base.join("logs"),
            config_home: base.join("config"),
            config_dir: base.join("config").join("pohunek"),
        }
    }

    /// Write the sway main config under `<config_home>/sway/config`.
    fn write_sway_config(paths: &Paths, contents: &str) {
        let dir = paths.sway_config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), contents).unwrap();
    }

    #[test]
    fn sway_include_warns_when_config_absent() {
        let base = unique_temp_dir();
        let paths = paths_at(&base);

        let check = check_sway_include(&paths);
        assert_eq!(check.status, Status::Warn);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sway_include_ok_when_config_includes_config_d() {
        let base = unique_temp_dir();
        let paths = paths_at(&base);
        write_sway_config(&paths, "include ~/.config/sway/config.d/*\n");

        let check = check_sway_include(&paths);
        assert_eq!(check.status, Status::Ok);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sway_include_warns_when_only_commented_config_d() {
        let base = unique_temp_dir();
        let paths = paths_at(&base);
        write_sway_config(&paths, "# include ~/.config/sway/config.d/*\n");

        let check = check_sway_include(&paths);
        assert_eq!(check.status, Status::Warn);

        let _ = std::fs::remove_dir_all(&base);
    }
}
