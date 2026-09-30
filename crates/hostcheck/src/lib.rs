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
//! The probe list is platform specific. Linux keeps the optional sway/rofi
//! launcher probes; macOS replaces them with runtime-path, worker, `launchd`,
//! terminal, filesystem-privacy and optional desktop probes (see [`macos`]).
//! Platform selection is explicit input to the pure builders ([`linux_checks`],
//! [`macos::standard_checks`]) so both are testable on any host;
//! [`standard_checks`] passes the current platform.
//!
//! Functions take concrete directory paths rather than a `Paths` struct so this
//! crate does not depend on either binary's path resolution (the CLI and daemon
//! deliberately resolve paths separately).

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use protocol::{DoctorCheck, DoctorStatus};

pub mod executable;
pub mod macos;

// Rust guideline compliant 2026-09-30

pub use executable::{is_executable_file, resolve_executable};
pub use macos::{
    resolve_worker_candidate, AccessDir, DomainProbe, MacosFacts, ProcessRunner, RunOutcome,
    Runner, WorkerCandidate, WorkerSource, WORKER_EXECUTABLE_CHECK,
};
pub use pohunek_paths::Platform;

/// Name prefix of the writability probe file.
///
/// A random suffix follows it, so a name planted in advance cannot collide
/// with a probe.
const PROBE_FILE: &str = ".pohunek-doctor-probe";

/// How many random probe names are tried when one already exists.
///
/// A collision needs a 128-bit random name to repeat, so one retry only guards
/// against a hostile pre-created file; eight bounds the loop.
const PROBE_NAME_ATTEMPTS: usize = 8;

/// Mode of the writability probe file: owner-only.
const PROBE_FILE_MODE: u32 = 0o600;

/// A fresh unpredictable probe file name.
///
/// Built from two randomly keyed `SipHash` outputs; the probe is also created
/// exclusively, so predictability is never the only defense.
pub(crate) fn probe_file_name() -> String {
    use std::hash::{BuildHasher as _, Hasher as _, RandomState};

    let high = RandomState::new().build_hasher().finish();
    let low = RandomState::new().build_hasher().finish();
    format!("{PROBE_FILE}-{high:016x}{low:016x}")
}

/// Inputs for the standard pohunek host checks.
///
/// Callers keep owning path resolution because the CLI and daemon intentionally
/// resolve paths in their own crates. This type carries the concrete
/// directories and process facts the shared probe lists need; fields used only
/// by one platform's list are ignored by the other.
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
    /// pohunek's config directory holding `launcher.conf` (macOS terminal probe).
    pub config_dir: &'a Path,
    /// The user's home directory, used to recognize privacy-protected folders.
    pub home_dir: Option<&'a Path>,
    /// Effective user id of the probing process.
    pub effective_uid: u32,
    /// Worker executable the daemon is expected to launch (macOS worker probe).
    pub worker: Option<&'a WorkerCandidate>,
    /// Extra directories that must be readable, such as the working directory.
    pub access_dirs: &'a [AccessDir<'a>],
}

/// The platform the binary was compiled for.
///
/// Anything other than macOS uses the Linux probe list.
#[must_use]
pub const fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    }
}

/// Build the standard pohunek host probe list.
///
/// The CLI-local doctor command and `daemon.doctor` RPC use this same ordered
/// list so drift in warnings, required checks, and launcher probes is visible in
/// one place. The list is selected by [`current_platform`]; on macOS this runs
/// the bounded `launchctl` domain probe.
#[must_use]
pub fn standard_checks(inputs: StandardCheckInputs<'_>) -> Vec<DoctorCheck> {
    match current_platform() {
        Platform::MacOs => {
            let facts = MacosFacts::collect(&inputs, &ProcessRunner::launchctl());
            macos::standard_checks(&inputs, &facts)
        }
        _ => linux_checks(inputs),
    }
}

/// The Linux probe list: agents, directories, `NetBird`, and the optional
/// sway/rofi launcher assets.
#[must_use]
pub fn linux_checks(inputs: StandardCheckInputs<'_>) -> Vec<DoctorCheck> {
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
/// the first entry that is a regular file with an execute bit
/// ([`resolve_executable`]).
#[must_use]
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    resolve_executable(name, std::env::var_os("PATH").as_deref())
}

/// Check whether a binary is resolvable on `PATH`.
///
/// `required` controls whether absence is reported as `fail` or `warn`.
#[must_use]
pub fn binary(name: &str, required: bool) -> DoctorCheck {
    binary_with_path(name, required, std::env::var_os("PATH").as_deref(), "")
}

/// [`binary`] against an explicit `PATH` value.
///
/// `missing_hint` is appended to the not-found detail (for example a
/// platform-specific remediation); it is empty for the Linux list.
#[must_use]
pub fn binary_with_path(
    name: &str,
    required: bool,
    path_var: Option<&OsStr>,
    missing_hint: &str,
) -> DoctorCheck {
    if let Some(path) = resolve_executable(name, path_var) {
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
            format!("'{name}' not found on PATH{missing_hint}"),
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
/// creating it and writing a probe file (see [`write_probe`]).
#[must_use]
pub fn dir_writable(name: &str, dir: &Path, label: &str) -> DoctorCheck {
    if let Err(err) = std::fs::create_dir_all(dir) {
        return DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!("cannot create {label} {}: {err}", dir.display()),
        );
    }
    match write_probe(dir) {
        Ok(()) => DoctorCheck::new(
            name,
            DoctorStatus::Ok,
            format!("writable: {}", dir.display()),
        ),
        Err(err) => DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!("{label} {} is not writable: {err}", dir.display()),
        ),
    }
}

/// Create, write and remove one probe file in `dir`.
///
/// The file has a random name and is created with `O_CREAT | O_EXCL`, which
/// never follows a symlink at the final component, so a pre-planted link cannot
/// redirect the write. Only the file this call created is removed.
fn write_probe(dir: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut last = None;
    for _ in 0..PROBE_NAME_ATTEMPTS {
        let probe = dir.join(probe_file_name());
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PROBE_FILE_MODE)
            .open(&probe)
        {
            Ok(mut file) => {
                let written = file.write_all(b"probe");
                drop(file);
                // Best-effort cleanup; a leftover probe is harmless.
                let _ = std::fs::remove_file(&probe);
                return written;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no probe name attempts")))
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
        pohunek_test_support::temp_root().join(format!("pohunek-hostcheck-test-{pid}-{n}"))
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
        assert_eq!(
            std::fs::read_dir(&base).unwrap().count(),
            0,
            "the probe file is removed"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dir_writable_never_writes_through_a_planted_probe_symlink() {
        let base = unique_temp_dir();
        std::fs::create_dir_all(&base).unwrap();
        let victim = base.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let dir = base.join("probed");
        std::fs::create_dir(&dir).unwrap();
        // The fixed name an earlier release used, and a guess at a random one.
        std::os::unix::fs::symlink(&victim, dir.join(PROBE_FILE)).unwrap();

        let check = dir_writable("probe_dir", &dir, "probe directory");

        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        let entries = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(entries, 1, "only the planted link remains");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn probe_names_are_unique_and_prefixed() {
        let first = probe_file_name();
        let second = probe_file_name();

        assert_ne!(first, second);
        assert!(first.starts_with(PROBE_FILE));
        assert!(first.len() > PROBE_FILE.len() + 16);
    }

    #[test]
    fn linux_checks_keeps_single_ordered_probe_contract() {
        let base = unique_temp_dir();
        let socket_dir = base.join("runtime");
        let state_dir = base.join("data");
        let log_dir = base.join("logs");
        let launcher_bin_dir = base.join("bin");
        let sway_config_dir = base.join("sway");

        let config_dir = base.join("config");
        let checks = linux_checks(StandardCheckInputs {
            socket_dir: &socket_dir,
            state_dir: &state_dir,
            log_dir: &log_dir,
            launcher_bin_dir: &launcher_bin_dir,
            sway_config_dir: &sway_config_dir,
            config_dir: &config_dir,
            home_dir: None,
            effective_uid: 0,
            worker: None,
            access_dirs: &[],
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
