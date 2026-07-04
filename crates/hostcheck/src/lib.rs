//! Host environment probes shared by `pohunek doctor` (CLI-local) and the
//! `daemon.doctor` RPC.
//!
//! Both the CLI doctor and the daemon need to probe the same things — binaries
//! on `PATH`, directory writability, `NetBird` state, the configured terminal,
//! and the optional sway/rofi launcher assets — but on potentially different
//! hosts (the CLI describes the local host; `daemon.doctor` describes the host
//! that owns the agent runtime). The probe logic is identical, so it lives here
//! once and produces [`protocol::DoctorCheck`] values that both callers embed
//! into a [`protocol::DoctorReport`].
//!
//! Functions take concrete directory paths rather than a `Paths` struct so this
//! crate does not depend on either binary's path resolution (the CLI and daemon
//! deliberately resolve paths separately).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use protocol::{DoctorCheck, DoctorStatus};

const PROBE_FILE: &str = ".pohunek-doctor-probe";

/// Inputs for the standard pohunek host checks.
///
/// Callers keep owning path resolution because the CLI and daemon intentionally
/// resolve paths in their own crates. This type carries only the concrete
/// directories needed by the shared probe list.
#[derive(Debug, Clone, Copy)]
pub struct StandardCheckInputs<'a> {
    /// Directory where the daemon binds its control socket.
    pub socket_dir: &'a Path,
    /// Directory where persistent state is written.
    pub state_dir: &'a Path,
    /// Directory where logs are written.
    pub log_dir: &'a Path,
    /// Directory where launcher entrypoints are installed.
    pub launcher_bin_dir: &'a Path,
    /// Directory containing the user's sway configuration.
    pub sway_config_dir: &'a Path,
}

/// Build the standard pohunek host probe list.
///
/// The CLI-local doctor command and `daemon.doctor` RPC use this same ordered
/// list so drift in warnings, required checks, and launcher probes is visible in
/// one place.
#[must_use]
pub fn standard_checks(inputs: StandardCheckInputs<'_>) -> Vec<DoctorCheck> {
    vec![
        binary("git", true),
        binary("codex", false),
        binary("claude", false),
        dir_writable(
            "socket_dir_writable",
            inputs.socket_dir,
            "control socket directory",
        ),
        dir_writable(
            "state_dir_writable",
            inputs.state_dir,
            "state data directory",
        ),
        dir_writable("log_dir_writable", inputs.log_dir, "log directory"),
        netbird(),
        DoctorCheck::new(
            "schema_version",
            DoctorStatus::Warn,
            "not available yet (SQLite store is a later milestone)",
        ),
        binary("rofi", false),
        binary("swaymsg", false),
        binary("python3", false),
        binary("timeout", false),
        terminal(),
        launcher_scripts(inputs.launcher_bin_dir),
        sway_include(inputs.sway_config_dir),
    ]
}

/// Resolve a binary name against the `PATH` environment variable.
///
/// A small dependency-free `which`: splits `PATH`, joins the name, and returns
/// the first entry that exists and is a regular file.
#[must_use]
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Check whether a binary is resolvable on `PATH`.
///
/// `required` controls whether absence is reported as `fail` or `warn`.
#[must_use]
pub fn binary(name: &str, required: bool) -> DoctorCheck {
    if let Some(path) = which_on_path(name) {
        DoctorCheck::new(
            format!("bin:{name}"),
            DoctorStatus::Ok,
            format!("found at {}", path.display()),
        )
    } else {
        let status = if required {
            DoctorStatus::Fail
        } else {
            DoctorStatus::Warn
        };
        DoctorCheck::new(
            format!("bin:{name}"),
            status,
            format!("'{name}' not found on PATH"),
        )
    }
}

/// Check `NetBird` availability.
///
/// `NetBird` is *optional*: remote hosts need it, but local-only use is fully
/// valid, so its absence is a `warn`, never a `fail`. When the CLI is present we
/// additionally probe local state — a resolvable self `NetBird` IP yields `ok`;
/// an unreadable state (daemon down / not logged in) is a `warn`.
#[must_use]
pub fn netbird() -> DoctorCheck {
    if which_on_path("netbird").is_none() {
        return DoctorCheck::new(
            "netbird_cli",
            DoctorStatus::Warn,
            "'netbird' not found on PATH; NetBird is optional (remote hosts need it)",
        );
    }

    match netbird::run_status() {
        Ok(status) => match status.self_netbird_ip() {
            Some(ip) => DoctorCheck::new(
                "netbird_cli",
                DoctorStatus::Ok,
                format!("found; this host's NetBird IP is {ip}"),
            ),
            None => DoctorCheck::new(
                "netbird_cli",
                DoctorStatus::Warn,
                "found, but no NetBird IP resolved (not logged in or daemon down)",
            ),
        },
        Err(err) => DoctorCheck::new(
            "netbird_cli",
            DoctorStatus::Warn,
            format!("found, but local state is unavailable: {err}"),
        ),
    }
}

/// Check that a terminal emulator is configured for the rofi launcher.
///
/// `ok` when `$TERMINAL` is set and non-empty; otherwise `warn` (the launcher
/// also reads a `terminal=` key from `launcher.conf`, so this is optional).
#[must_use]
pub fn terminal() -> DoctorCheck {
    match std::env::var("TERMINAL") {
        Ok(value) if !value.is_empty() => {
            DoctorCheck::new("terminal", DoctorStatus::Ok, format!("TERMINAL={value}"))
        }
        _ => DoctorCheck::new(
            "terminal",
            DoctorStatus::Warn,
            "set $TERMINAL or 'terminal=' in launcher.conf (the rofi launcher needs a terminal)",
        ),
    }
}

/// Check that a directory exists (or can be created) and is writable, by
/// creating it and writing a probe file.
#[must_use]
pub fn dir_writable(name: &str, dir: &Path, label: &str) -> DoctorCheck {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!("cannot create {label} {}: {err}", dir.display()),
        );
    }
    let probe = dir.join(PROBE_FILE);
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            // Best-effort cleanup; a leftover probe is harmless.
            let _ = std::fs::remove_file(&probe);
            DoctorCheck::new(
                name,
                DoctorStatus::Ok,
                format!("writable: {}", dir.display()),
            )
        }
        Err(err) => DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!("{label} {} is not writable: {err}", dir.display()),
        ),
    }
}

/// Check whether the launcher scripts have been materialized by
/// `pohunek setup scripts`. `ok` when the `pohunek-rofi` entrypoint is present
/// in `bin_dir`; otherwise `warn` (the launcher is optional).
#[must_use]
pub fn launcher_scripts(bin_dir: &Path) -> DoctorCheck {
    if bin_dir.join("pohunek-rofi").is_file() {
        DoctorCheck::new(
            "launcher_scripts",
            DoctorStatus::Ok,
            format!("installed at {}", bin_dir.display()),
        )
    } else {
        DoctorCheck::new(
            "launcher_scripts",
            DoctorStatus::Warn,
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
#[must_use]
pub fn sway_include(sway_config_dir: &Path) -> DoctorCheck {
    let config = sway_config_dir.join("config");
    match std::fs::read_to_string(&config) {
        Ok(contents) => {
            // A "non-comment line mentioning config.d": trim each line, skip
            // comment lines, and look for the `config.d` token.
            let includes = contents.lines().any(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with('#') && trimmed.contains("config.d")
            });
            if includes {
                DoctorCheck::new(
                    "sway_include",
                    DoctorStatus::Ok,
                    "sway config includes config.d",
                )
            } else {
                DoctorCheck::new(
                    "sway_include",
                    DoctorStatus::Warn,
                    format!(
                        "add 'include {}/config.d/*' to your sway config (see 'pohunek setup sway')",
                        sway_config_dir.display()
                    ),
                )
            }
        }
        Err(_) => DoctorCheck::new(
            "sway_include",
            DoctorStatus::Warn,
            format!("sway config not found at {}", config.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn unique_temp_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("pohunek-hostcheck-test-{pid}-{n}"))
    }

    #[test]
    fn sway_include_warns_when_config_absent() {
        let base = unique_temp_dir();

        let check = sway_include(&base.join("sway"));
        assert_eq!(check.status, DoctorStatus::Warn);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sway_include_ok_when_config_includes_config_d() {
        let base = unique_temp_dir();
        let sway_dir = base.join("sway");
        std::fs::create_dir_all(&sway_dir).unwrap();
        std::fs::write(
            sway_dir.join("config"),
            "include ~/.config/sway/config.d/*\n",
        )
        .unwrap();

        let check = sway_include(&sway_dir);
        assert_eq!(check.status, DoctorStatus::Ok);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sway_include_warns_when_only_commented_config_d() {
        let base = unique_temp_dir();
        let sway_dir = base.join("sway");
        std::fs::create_dir_all(&sway_dir).unwrap();
        std::fs::write(
            sway_dir.join("config"),
            "# include ~/.config/sway/config.d/*\n",
        )
        .unwrap();

        let check = sway_include(&sway_dir);
        assert_eq!(check.status, DoctorStatus::Warn);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dir_writable_ok_for_fresh_dir_and_cleans_probe() {
        let base = unique_temp_dir();

        let check = dir_writable("probe_dir", &base, "probe directory");
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(!base.join(PROBE_FILE).exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn standard_checks_keeps_single_ordered_probe_contract() {
        let base = unique_temp_dir();
        let socket_dir = base.join("runtime");
        let state_dir = base.join("data");
        let log_dir = base.join("logs");
        let launcher_bin_dir = base.join("bin");
        let sway_config_dir = base.join("sway");

        let checks = standard_checks(StandardCheckInputs {
            socket_dir: &socket_dir,
            state_dir: &state_dir,
            log_dir: &log_dir,
            launcher_bin_dir: &launcher_bin_dir,
            sway_config_dir: &sway_config_dir,
        });
        let names = checks
            .iter()
            .map(|check| check.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "bin:git",
                "bin:codex",
                "bin:claude",
                "socket_dir_writable",
                "state_dir_writable",
                "log_dir_writable",
                "netbird_cli",
                "schema_version",
                "bin:rofi",
                "bin:swaymsg",
                "bin:python3",
                "bin:timeout",
                "terminal",
                "launcher_scripts",
                "sway_include",
            ]
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn binary_reports_missing_required_as_fail() {
        let check = binary("definitely-not-a-real-binary-xyz", true);
        assert_eq!(check.status, DoctorStatus::Fail);
    }
}
