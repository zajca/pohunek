//! Host environment probes shared by `pohunek doctor` (CLI-local) and the
//! `daemon.doctor` RPC.
//!
//! Both the CLI doctor and the daemon need to probe the same things: binaries
//! on `PATH`, directory writability, and `NetBird` state, but on potentially
//! different hosts (the CLI describes the local host; `daemon.doctor` describes the host
//! that owns the agent runtime). The probe logic is identical, so it lives here
//! once and produces [`protocol::DoctorCheck`] values that both callers embed
//! into a [`protocol::DoctorReport`].
//!
//! The probe list is platform specific. macOS adds runtime-path, worker,
//! `launchd`, filesystem-privacy and optional desktop probes (see [`macos`]).
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
    apply_supervision, doctor_request_timeout, LAUNCHD_CHECKS, PROBE_BUDGET, REPLY_HEADROOM,
};
pub use macos::{
    resolve_worker_candidate, AccessDir, DomainProbe, MacosFacts, ProcessRunner, RunOutcome,
    Runner, Supervision, WorkerCandidate, WorkerSource, WORKER_EXECUTABLE_CHECK,
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
    /// pohunek's config directory (macOS filesystem-access probe).
    pub config_dir: &'a Path,
    /// The user's home directory, used to recognize privacy-protected folders.
    pub home_dir: Option<&'a Path>,
    /// Effective user id of the probing process.
    pub effective_uid: u32,
    /// Worker executable the daemon is expected to launch (macOS worker probe).
    pub worker: Option<&'a WorkerCandidate>,
    /// Extra directories that must be readable, such as the working directory.
    pub access_dirs: &'a [AccessDir<'a>],
    /// How the daemon supervises workers (macOS launchd checks).
    pub supervision: Supervision,
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
/// list so drift in warnings and required checks is visible in
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

/// The Linux probe list: agents, directories, and `NetBird`.
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
/// valid, so its absence is a `warn`, never a `fail`. The CLI is located by the
/// same resolver the daemon and client use to run it, so the verdict cannot
/// disagree with the spawn. When it is present we additionally probe local
/// state: a resolvable self `NetBird` IP yields `ok`; an unreadable state
/// (daemon down / not logged in) is a `warn`.
#[must_use]
pub fn netbird() -> DoctorCheck {
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
        Err(netbird::NetbirdError::CliMissing) => DoctorCheck::new(
            "netbird_cli",
            DoctorStatus::Warn,
            "'netbird' not found on PATH; NetBird is optional (remote hosts need it)",
        ),
        Err(err) => DoctorCheck::new(
            "netbird_cli",
            DoctorStatus::Warn,
            format!("found, but local state is unavailable: {err}"),
        ),
    }
}

/// Check that a directory exists (or can be created) and is writable, by
/// creating it and writing a probe file (see [`write_probe`]).
#[must_use]
pub fn dir_writable(name: &str, dir: &Path, label: &str) -> DoctorCheck {
    if let Err(err) = create_private_dirs(dir) {
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

/// Mode of a directory created by the writability probe.
///
/// The daemon requires its state and runtime directories to be exactly
/// owner-private, so a directory created here with the default umask (`0755`)
/// would be refused at startup.
const CREATED_DIR_MODE: u32 = 0o700;

/// Create `dir` and any missing parents owner-private.
fn create_private_dirs(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(CREATED_DIR_MODE)
        .create(dir)
}

/// Create, write and remove one probe file in `dir`.
///
/// The file has a random name and is created with `O_CREAT | O_EXCL`, which
/// never follows a symlink at the final component, so a pre-planted link cannot
/// redirect the write. Only the file this call created is removed.
pub(crate) fn write_probe(dir: &Path) -> std::io::Result<()> {
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
