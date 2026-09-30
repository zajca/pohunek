//! macOS platform readiness checks.
//!
//! The checks split into two layers. [`MacosFacts`] holds everything read from
//! the host that is not a plain directory probe (environment, `/etc/shells`,
//! application presence, the `launchd` domain probe); [`MacosFacts::collect`]
//! gathers it from the real host, while tests construct it directly. The
//! `check_*` functions turn facts and directories into
//! [`protocol::DoctorCheck`] values and never touch the process environment.
//!
//! `name` is the stable check code and `detail` carries the remediation. The
//! statuses follow one rule: a check is `fail` only when pohunek cannot work
//! without it (runtime directory, worker executable, `launchd` domain,
//! required filesystem access); optional capabilities are at most `warn`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pohunek_paths::{
    validate_staged_socket_path, worker_socket_path, Platform, SocketKind, SOCKET_NAME,
    WORKERS_SUBDIR,
};
use pohunek_platform::filesystem::{StageOutcome, TrustedDir};
use protocol::{DoctorCheck, DoctorStatus};

use crate::executable::{is_executable_file, resolve_executable};
use crate::{binary_with_path, netbird, StandardCheckInputs};

// Rust guideline compliant 2026-09-30

/// The only `launchctl` executable the domain probe runs.
///
/// An absolute system path so neither `PATH` nor the working directory can
/// substitute another program.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// Deadline of the `launchctl print gui/<uid>` probe.
///
/// `print` answers in milliseconds; ten seconds matches the service installer's
/// `launchctl` deadline and only guards against a wedged `launchd`. A longer
/// value makes `pohunek doctor` hang on such a host.
pub const LAUNCHCTL_DEADLINE: Duration = Duration::from_secs(10);

/// Interval between exit-status polls of the running probe.
///
/// Short enough that a healthy probe adds no visible latency, long enough not
/// to spin a core while waiting for the deadline.
const LAUNCHCTL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `launchctl` exit status for a domain that does not exist.
///
/// Recorded on GitHub-hosted macOS 14 and 15 runners for `print gui/<unknown
/// uid>`; the status table lives with the service supervisor's `launchctl`
/// runner. Output text is localized and never matched.
const LAUNCHCTL_NO_SUCH_DOMAIN: i32 = 112;

/// Stable name of the worker executable check.
///
/// The CLI doctor prefers the daemon's own result for this check, because the
/// daemon knows which worker its supervision launches.
pub const WORKER_EXECUTABLE_CHECK: &str = "worker_executable";

/// Shell registry consulted for the login-shell check.
const ETC_SHELLS: &str = "/etc/shells";

/// Stock macOS terminal application.
const TERMINAL_APP: &str = "/System/Applications/Utilities/Terminal.app";

/// Stock macOS scripting host the notification path is built on.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// Stock macOS Keychain command-line tool.
const SECURITY_TOOL: &str = "/usr/bin/security";

/// Login keychain file relative to the home directory.
const LOGIN_KEYCHAIN_RELATIVE: [&str; 3] = ["Library", "Keychains", "login.keychain-db"];

/// Home-relative folders macOS protects with Privacy & Security (TCC) consent.
///
/// Documents, Desktop and Downloads are the user-facing protected folders;
/// `Library/Mobile Documents` is iCloud Drive and `Library/CloudStorage` holds
/// File Provider stores such as third-party sync clients.
const TCC_PROTECTED_HOME_SUBDIRS: [&[&str]; 5] = [
    &["Documents"],
    &["Desktop"],
    &["Downloads"],
    &["Library", "Mobile Documents"],
    &["Library", "CloudStorage"],
];

/// Volumes root; removable and network volumes are TCC-protected as well.
const VOLUMES_ROOT: &str = "/Volumes";

/// `errno` value of `EPERM` ("Operation not permitted").
///
/// macOS reports a Privacy & Security denial as `EPERM`, distinct from the
/// `EACCES` of ordinary file permissions.
const EPERM: i32 = 1;

/// `errno` value of `EACCES` ("Permission denied").
const EACCES: i32 = 13;

/// Directory mode of the runtime root: owner-only, the mode the daemon
/// requires at startup.
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Mode of the writability probe file inside the runtime root.
const PROBE_FILE_MODE: u32 = 0o600;

/// Prefix of the quarantine name a probe file is moved to before removal.
const PROBE_QUARANTINE_PREFIX: &str = ".pohunek-doctor-stale-";

/// Where a resolved worker executable path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSource {
    /// The worker the running daemon's supervision launches.
    Supervision,
    /// The `worker_executable` recorded in the installed `service.toml`.
    ServiceConfig,
    /// The `POHUNEK_WORKER_BIN` override.
    Environment,
    /// `pohunek-sessiond` next to the running executable.
    Sibling,
}

impl WorkerSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Supervision => "the daemon's active supervision",
            Self::ServiceConfig => "service.toml",
            Self::Environment => "POHUNEK_WORKER_BIN",
            Self::Sibling => "next to the running executable",
        }
    }
}

/// The worker executable a caller expects the daemon to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCandidate {
    /// Path of the worker executable.
    pub path: PathBuf,
    /// How the path was derived.
    pub source: WorkerSource,
}

/// Resolve the worker executable the way the daemon does.
///
/// An installed `service.toml` wins (native supervision), then the
/// `POHUNEK_WORKER_BIN` override, then `pohunek-sessiond` in
/// `executable_dir`. Like the daemon's `resolve_worker_binary`, any present
/// override is used as is, an empty value included; the worker check then
/// rejects a path that is empty or not absolute instead of falling back.
#[must_use]
pub fn resolve_worker_candidate(
    service_worker: Option<PathBuf>,
    env_override: Option<OsString>,
    executable_dir: Option<&Path>,
) -> Option<WorkerCandidate> {
    if let Some(path) = service_worker {
        return Some(WorkerCandidate {
            path,
            source: WorkerSource::ServiceConfig,
        });
    }
    if let Some(value) = env_override {
        return Some(WorkerCandidate {
            path: PathBuf::from(value),
            source: WorkerSource::Environment,
        });
    }
    executable_dir.map(|dir| WorkerCandidate {
        path: dir.join(pohunek_paths::WORKER_EXECUTABLE_NAME),
        source: WorkerSource::Sibling,
    })
}

/// A directory the caller needs to read, with the reason it matters.
#[derive(Debug, Clone, Copy)]
pub struct AccessDir<'a> {
    /// Human label used in the check detail, for example `current directory`.
    pub label: &'a str,
    /// Directory to probe.
    pub path: &'a Path,
}

/// Outcome of the `launchctl print gui/<uid>` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainProbe {
    /// The domain exists.
    Ready,
    /// `launchctl` reported that the domain does not exist.
    Missing,
    /// `launchctl` exited with another status, or was killed by a signal.
    Failed(Option<i32>),
    /// The probe outlived its deadline and was killed.
    TimedOut,
    /// `launchctl` could not be started.
    Unavailable,
}

/// Result of running an external program under a deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The program exited; `None` means it was killed by a signal.
    Exited(Option<i32>),
    /// The deadline passed and the program was killed.
    TimedOut,
    /// The program could not be started.
    Unavailable,
}

/// Runs a fixed executable with an argument vector under a deadline.
pub trait Runner {
    /// Run the program with `arguments`, discarding its output.
    fn run(&self, arguments: &[&str], deadline: Duration) -> RunOutcome;
}

/// [`Runner`] that spawns a real process.
///
/// Arguments are passed as an argv array, never through a shell; standard
/// streams are detached so a chatty child cannot block on a full pipe.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    program: PathBuf,
}

impl ProcessRunner {
    /// Runner for the fixed `/bin/launchctl`.
    #[must_use]
    pub fn launchctl() -> Self {
        Self {
            program: PathBuf::from(LAUNCHCTL),
        }
    }

    /// Runner for an explicit program, used by tests with a scripted stand-in.
    #[must_use]
    pub fn for_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Runner for ProcessRunner {
    fn run(&self, arguments: &[&str], deadline: Duration) -> RunOutcome {
        let Ok(mut child) = Command::new(&self.program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return RunOutcome::Unavailable;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return RunOutcome::Exited(status.code()),
                Ok(None) if started.elapsed() < deadline => {
                    std::thread::sleep(LAUNCHCTL_POLL_INTERVAL);
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return RunOutcome::TimedOut;
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return RunOutcome::Unavailable;
                }
            }
        }
    }
}

/// Probe whether the `gui/<uid>` launchd domain exists.
#[must_use]
pub fn probe_launchd_domain(runner: &dyn Runner, uid: u32, deadline: Duration) -> DomainProbe {
    let domain = format!("gui/{uid}");
    match runner.run(&["print", &domain], deadline) {
        RunOutcome::Exited(Some(0)) => DomainProbe::Ready,
        RunOutcome::Exited(Some(LAUNCHCTL_NO_SUCH_DOMAIN)) => DomainProbe::Missing,
        RunOutcome::Exited(code) => DomainProbe::Failed(code),
        RunOutcome::TimedOut => DomainProbe::TimedOut,
        RunOutcome::Unavailable => DomainProbe::Unavailable,
    }
}

/// Host facts the macOS checks evaluate.
#[derive(Debug, Clone)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent host observations, each consumed by exactly one check"
)]
pub struct MacosFacts {
    /// Value of `$SHELL`, when set.
    pub shell: Option<OsString>,
    /// Contents of `/etc/shells`; `None` when unreadable.
    pub etc_shells: Option<String>,
    /// Whether the stock Terminal application bundle exists.
    pub terminal_app_present: bool,
    /// `terminal=` value from `launcher.conf`, when set.
    pub launcher_terminal: Option<String>,
    /// Value of `PATH`.
    pub path_var: Option<OsString>,
    /// Result of the `gui/<uid>` domain probe.
    pub launchd_domain: DomainProbe,
    /// Whether `/usr/bin/osascript` is executable.
    pub osascript_present: bool,
    /// Whether `/usr/bin/security` is executable.
    pub security_tool_present: bool,
    /// Whether the login keychain file exists.
    pub login_keychain_present: bool,
}

impl MacosFacts {
    /// Read the facts from the running host.
    ///
    /// Runs the bounded `launchctl` domain probe through `runner`; every other
    /// fact is a plain file or environment read.
    #[must_use]
    pub fn collect(inputs: &StandardCheckInputs<'_>, runner: &dyn Runner) -> Self {
        let login_keychain_present = inputs.home_dir.is_some_and(|home| {
            LOGIN_KEYCHAIN_RELATIVE
                .iter()
                .fold(home.to_path_buf(), |path, part| path.join(part))
                .is_file()
        });
        Self {
            shell: std::env::var_os("SHELL"),
            etc_shells: std::fs::read_to_string(ETC_SHELLS).ok(),
            terminal_app_present: Path::new(TERMINAL_APP).is_dir(),
            launcher_terminal: read_launcher_terminal(inputs.config_dir),
            path_var: std::env::var_os("PATH"),
            launchd_domain: probe_launchd_domain(runner, inputs.effective_uid, LAUNCHCTL_DEADLINE),
            osascript_present: is_executable_file(Path::new(OSASCRIPT)),
            security_tool_present: is_executable_file(Path::new(SECURITY_TOOL)),
            login_keychain_present,
        }
    }
}

/// Read the `terminal=` value from `<config_dir>/launcher.conf`.
///
/// Mirrors the launcher's `pohunek_config_get` (`scripts/lib.sh`): lines are
/// stripped, blank and `#` lines are skipped, keys and values are stripped, and
/// the last `terminal=` assignment wins even when empty (an empty value means
/// unset, so the launcher falls back to `$TERMINAL`). A non-comment line
/// without `=` makes the launcher's lookup fail, which reads as unset. An
/// unreadable or absent file yields `None`.
#[must_use]
pub fn read_launcher_terminal(config_dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(config_dir.join("launcher.conf")).ok()?;
    parse_launcher_terminal(&contents)
}

fn parse_launcher_terminal(contents: &str) -> Option<String> {
    let mut value = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, item) = line.split_once('=')?;
        if key.trim() == "terminal" {
            value = Some(item.trim());
        }
    }
    value.filter(|value| !value.is_empty()).map(str::to_owned)
}

/// The ordered macOS probe list.
///
/// Optional Linux capabilities (rofi, swaymsg, `timeout`, `$TERMINAL`, the
/// launcher scripts, the sway include) and the launcher-only `python3` probe
/// are omitted: they are not macOS capabilities, and hook interpreter
/// readiness is reported by `pohunek integration doctor`.
#[must_use]
pub fn standard_checks(inputs: &StandardCheckInputs<'_>, facts: &MacosFacts) -> Vec<DoctorCheck> {
    let state_root = state_root(inputs.log_dir);
    vec![
        binary_with_path("git", true, facts.path_var.as_deref(), ""),
        agent_binary("codex", facts),
        agent_binary("claude", facts),
        check_runtime_dir_private(inputs.socket_dir),
        check_socket_path_length(inputs.socket_dir),
        check_private_dir(
            "socket_dir_writable",
            "control socket directory",
            inputs.socket_dir,
        ),
        check_private_dir(
            "state_dir_writable",
            "state data directory",
            inputs.state_dir,
        ),
        check_log_dir(inputs.log_dir),
        check_private_dir(
            "worker_runtime_root",
            "worker runtime root",
            &inputs.socket_dir.join(WORKERS_SUBDIR),
        ),
        check_private_dir(
            "worker_state_root",
            "worker state root",
            &state_root.join(WORKERS_SUBDIR),
        ),
        check_filesystem_access(inputs),
        netbird(),
        DoctorCheck::new(
            "schema_version",
            DoctorStatus::Warn,
            "not available yet (SQLite store is a later milestone)",
        ),
        check_worker_executable(inputs.worker),
        check_login_shell(facts),
        check_terminal(facts),
        check_launchd_domain(facts.launchd_domain, inputs.effective_uid),
        check_desktop_notifications(facts),
        check_keychain(facts),
    ]
}

/// Optional agent program probe with a launchd-aware hint.
fn agent_binary(name: &str, facts: &MacosFacts) -> DoctorCheck {
    binary_with_path(
        name,
        false,
        facts.path_var.as_deref(),
        "; optional. Install it, or set an absolute 'program' in an agent profile. \
         A daemon started by launchd does not read shell startup files, so a PATH set only \
         in ~/.zshrc is not visible to it",
    )
}

/// Check the runtime directory with the validation the daemon applies at
/// startup, without creating it.
///
/// The default root lives in the world-writable `/private/tmp`, so another
/// account could pre-create it. [`TrustedDir::open_absolute`] rejects symlinked
/// path components, a foreign owner, any mode other than `0700`, and (on macOS)
/// an ACL that grants access beyond the mode bits. An absent directory is `ok`:
/// the daemon creates it owner-private.
#[must_use]
pub fn check_runtime_dir_private(dir: &Path) -> DoctorCheck {
    const NAME: &str = "runtime_dir_private";
    match TrustedDir::open_absolute(dir, RUNTIME_DIR_MODE) {
        Ok(_) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!(
                "{} passes the daemon's owner-private directory validation",
                dir.display()
            ),
        ),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!(
                "{} does not exist yet; the daemon creates it with mode 0700",
                dir.display()
            ),
        ),
        Err(error) => DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{} fails the owner-private directory validation the daemon applies at startup: \
                 {error}. It must be a real directory you own with mode 0700 and no symlinked \
                 path components; remove it or set XDG_RUNTIME_DIR to such a directory",
                dir.display()
            ),
        ),
    }
}

/// The state root: the parent of the log directory.
///
/// `BasePaths` places the log directory directly below the state root
/// (`<state>/logs`), and the daemon also keeps the worker journals there.
fn state_root(log_dir: &Path) -> &Path {
    log_dir.parent().unwrap_or(log_dir)
}

/// Check a directory the daemon creates owner-private, without creating it.
///
/// The doctor never creates a directory: a directory made here with the
/// default umask would be `0755`, which startup then refuses. An existing
/// directory must pass [`TrustedDir::open_absolute`] with mode `0700`, the same
/// validation startup applies (real directory, owned by you, exact mode, no
/// ACL beyond the mode, no symlinked component), and is then write-probed with
/// a randomly named exclusive file relative to the opened descriptor. A missing
/// directory is `ok` when startup could create it (see [`creatable_below`]).
#[must_use]
pub fn check_private_dir(name: &str, label: &str, dir: &Path) -> DoctorCheck {
    match TrustedDir::open_absolute(dir, RUNTIME_DIR_MODE) {
        Ok(trusted) => probe_trusted_dir(name, label, dir, &trusted),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            match creatable_below(dir) {
                Ok(ancestor) => DoctorCheck::new(
                    name,
                    DoctorStatus::Ok,
                    format!(
                        "{label} {} does not exist yet; startup creates it with mode 0700 below {}",
                        dir.display(),
                        ancestor.display()
                    ),
                ),
                Err(reason) => DoctorCheck::new(
                    name,
                    DoctorStatus::Fail,
                    format!("{label} {} cannot be created: {reason}", dir.display()),
                ),
            }
        }
        Err(error) => DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!(
                "{label} {} fails the owner-private directory validation the daemon applies at \
                 startup: {error}. It must be a real directory you own with mode 0700 and no \
                 symlinked path components; nothing was written to it",
                dir.display()
            ),
        ),
    }
}

/// Check the log directory without creating anything.
///
/// Startup creates the state root owner-private and then the log directory
/// below it, tolerating an existing real log directory (it resets its mode).
/// The state root is validated like any private directory; an existing log
/// directory must be a real directory and is write-probed.
#[must_use]
pub fn check_log_dir(log_dir: &Path) -> DoctorCheck {
    const NAME: &str = "log_dir_writable";
    const LABEL: &str = "log directory";
    let root = state_root(log_dir);
    let root_check = check_private_dir(NAME, "state root", root);
    if root_check.status != DoctorStatus::Ok || !root.exists() {
        return root_check;
    }
    match std::fs::symlink_metadata(log_dir) {
        Ok(meta) if meta.file_type().is_dir() => match crate::write_probe(log_dir) {
            Ok(()) => DoctorCheck::new(
                NAME,
                DoctorStatus::Ok,
                format!("writable: {}", log_dir.display()),
            ),
            Err(error) => DoctorCheck::new(
                NAME,
                DoctorStatus::Fail,
                format!("{LABEL} {} is not writable: {error}", log_dir.display()),
            ),
        },
        Ok(_) => DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{LABEL} {} is not a real directory (a symlink or file is refused); remove it",
                log_dir.display()
            ),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!(
                "{LABEL} {} does not exist yet; startup creates it",
                log_dir.display()
            ),
        ),
        Err(error) => DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!("cannot inspect {LABEL} {}: {error}", log_dir.display()),
        ),
    }
}

/// Decide, without creating anything, whether startup could create `dir`.
///
/// Startup creates missing components with `open_or_create_absolute`, which
/// needs the nearest existing ancestor to pass the ancestor policy
/// ([`TrustedDir::open_absolute_ancestor`]) and to be writable and searchable
/// by the effective user. Returns that ancestor, or the reason it fails.
fn creatable_below(dir: &Path) -> Result<PathBuf, String> {
    let ancestor = dir
        .ancestors()
        .skip(1)
        .find(|candidate| std::fs::symlink_metadata(candidate).is_ok())
        .ok_or_else(|| "no existing ancestor directory".to_owned())?;
    TrustedDir::open_absolute_ancestor(ancestor).map_err(|error| error.to_string())?;
    rustix::fs::accessat(
        rustix::fs::CWD,
        ancestor,
        rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS,
    )
    .map_err(|error| {
        format!(
            "its nearest existing ancestor {} is not writable and searchable by you ({error})",
            ancestor.display()
        )
    })?;
    Ok(ancestor.to_path_buf())
}

/// Write-probe an already validated directory through its descriptor.
fn probe_trusted_dir(name: &str, label: &str, dir: &Path, trusted: &TrustedDir) -> DoctorCheck {
    for _ in 0..crate::PROBE_NAME_ATTEMPTS {
        let probe = crate::probe_file_name();
        match trusted.create_file(&probe, b"probe", PROBE_FILE_MODE) {
            Ok(identity) => {
                // Best-effort cleanup; a leftover probe is harmless.
                if let Ok(StageOutcome::Staged(entry)) =
                    trusted.stage_random(&probe, PROBE_QUARANTINE_PREFIX, identity)
                {
                    let _ = entry.remove();
                }
                return DoctorCheck::new(
                    name,
                    DoctorStatus::Ok,
                    format!("writable: {}", dir.display()),
                );
            }
            Err(error) if error.io_kind() == Some(std::io::ErrorKind::AlreadyExists) => {}
            Err(error) => {
                return DoctorCheck::new(
                    name,
                    DoctorStatus::Fail,
                    format!("{label} {} is not writable: {error}", dir.display()),
                );
            }
        }
    }
    DoctorCheck::new(
        name,
        DoctorStatus::Fail,
        format!(
            "{label} {} rejected every probe file name; inspect it for unexpected entries",
            dir.display()
        ),
    )
}

/// Check that the daemon socket and the longest worker socket fit Darwin's
/// `sockaddr_un` limit, including staged bind names.
#[must_use]
pub fn check_socket_path_length(runtime_dir: &Path) -> DoctorCheck {
    const NAME: &str = "socket_path_length";
    let remediation = "set XDG_RUNTIME_DIR to a shorter absolute directory";
    let daemon_socket = runtime_dir.join(SOCKET_NAME);
    if let Err(error) =
        validate_staged_socket_path(&daemon_socket, Platform::MacOs, SocketKind::Daemon)
    {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!("daemon socket path is unusable: {error}; {remediation}"),
        );
    }
    let longest_session = pohunek_paths::longest_worker_session_id();
    let worker_root = runtime_dir.join(WORKERS_SUBDIR);
    match worker_socket_path(&worker_root, &longest_session, Platform::MacOs) {
        Ok(_) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!(
                "daemon and worker socket paths fit the {}-byte Darwin limit",
                Platform::MacOs.socket_path_max_bytes()
            ),
        ),
        Err(error) => DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!("a worker socket path would exceed the platform limit: {error}; {remediation}"),
        ),
    }
}

/// Check that the worker executable exists, is absolute and is executable.
///
/// The worker is required: without it no session can start.
#[must_use]
pub fn check_worker_executable(candidate: Option<&WorkerCandidate>) -> DoctorCheck {
    const NAME: &str = WORKER_EXECUTABLE_CHECK;
    let Some(candidate) = candidate else {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            "no worker executable is configured; install the service with 'pohunek service install', \
             or start the daemon with '--service-config' or '--dev-subprocess' (POHUNEK_WORKER_BIN \
             overrides the path)",
        );
    };
    let source = candidate.source.describe();
    if !candidate.path.is_absolute() {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "worker path '{}' (from {source}) is not absolute (an empty value is rejected too); \
                 use an absolute path",
                candidate.path.display()
            ),
        );
    }
    if is_executable_file(&candidate.path) {
        DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("{} (from {source})", candidate.path.display()),
        )
    } else {
        DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{} (from {source}) is missing or not executable; reinstall with 'pohunek service \
                 install' or fix POHUNEK_WORKER_BIN",
                candidate.path.display()
            ),
        )
    }
}

/// Check `$SHELL`: an absolute executable listed in `/etc/shells`.
///
/// Interactive attach uses the login shell; a problem here is a `warn` because
/// pohunek does not need the shell to supervise agents.
#[must_use]
pub fn check_login_shell(facts: &MacosFacts) -> DoctorCheck {
    const NAME: &str = "login_shell";
    let Some(shell) = facts.shell.as_deref().filter(|shell| !shell.is_empty()) else {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            "$SHELL is not set; set it to your login shell, for example /bin/zsh",
        );
    };
    let path = Path::new(shell);
    if !path.is_absolute() {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("$SHELL={} is not an absolute path", path.display()),
        );
    }
    if !is_executable_file(path) {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("$SHELL={} is not an executable file", path.display()),
        );
    }
    match facts.etc_shells.as_deref() {
        Some(listing) if shell_listed(listing, path) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("{} is listed in {ETC_SHELLS}", path.display()),
        ),
        Some(_) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!(
                "{} is not listed in {ETC_SHELLS}; add it there or run 'chsh -s' with a listed shell",
                path.display()
            ),
        ),
        None => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("{ETC_SHELLS} could not be read, so {} is unverified", path.display()),
        ),
    }
}

fn shell_listed(listing: &str, shell: &Path) -> bool {
    listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|line| Path::new(line) == shell)
}

/// Check the attach terminal: the stock Terminal application and the optional
/// `terminal=` command from `launcher.conf`.
///
/// The configured terminal is optional. The launcher runs the whole `terminal=`
/// value as one executable name (`"$terminal_bin" -e ...`), so the value is
/// resolved as a single program, never split into words; a value such as
/// `kitty -e` is unresolvable. A configured but unresolvable command is a
/// `warn`, as is a host with neither the stock app nor a working configured
/// command.
#[must_use]
pub fn check_terminal(facts: &MacosFacts) -> DoctorCheck {
    const NAME: &str = "terminal";
    let configured = facts.launcher_terminal.as_deref().map(|command| {
        let resolved = resolve_executable(command, facts.path_var.as_deref());
        (command, resolved.is_some())
    });
    match (facts.terminal_app_present, configured) {
        (true, None) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("stock terminal available at {TERMINAL_APP}; 'terminal=' in launcher.conf is optional"),
        ),
        (true, Some((command, true))) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("configured terminal '{command}' resolves; stock {TERMINAL_APP} is also available"),
        ),
        (_, Some((command, false))) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!(
                "configured terminal '{command}' (launcher.conf) does not resolve to one executable; \
                 the launcher runs the whole value as a single program name, so put arguments in a \
                 wrapper script, or fix or remove the 'terminal=' key"
            ),
        ),
        (false, Some((command, true))) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("configured terminal '{command}' resolves"),
        ),
        (false, None) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!(
                "{TERMINAL_APP} not found and no 'terminal=' set in launcher.conf; set 'terminal=' \
                 to a terminal command"
            ),
        ),
    }
}

/// Report the outcome of the `gui/<uid>` launchd domain probe.
///
/// A missing domain is a `fail` (native supervision needs a graphical login
/// session); an inconclusive probe is a `warn`.
#[must_use]
pub fn check_launchd_domain(probe: DomainProbe, uid: u32) -> DoctorCheck {
    const NAME: &str = "launchd_domain";
    match probe {
        DomainProbe::Ready => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("launchd domain gui/{uid} is reachable"),
        ),
        DomainProbe::Missing => DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "launchd domain gui/{uid} does not exist; log in to the Mac's graphical session \
                 as this user (a bare SSH session without a console login has no gui domain) and retry"
            ),
        ),
        DomainProbe::Failed(code) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            match code {
                Some(code) => format!("'launchctl print gui/{uid}' exited with status {code}; launchd readiness is unconfirmed"),
                None => format!("'launchctl print gui/{uid}' was terminated by a signal; launchd readiness is unconfirmed"),
            },
        ),
        DomainProbe::TimedOut => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("'launchctl print gui/{uid}' did not answer within {} seconds; launchd may be wedged", LAUNCHCTL_DEADLINE.as_secs()),
        ),
        DomainProbe::Unavailable => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("{LAUNCHCTL} could not be started; launchd readiness is unconfirmed"),
        ),
    }
}

/// Report whether the notification path's host tool exists.
///
/// Delivery and user denial cannot be observed from a CLI process for an
/// unbundled binary, so a present tool is reported `ok` with that caveat.
#[must_use]
pub fn check_desktop_notifications(facts: &MacosFacts) -> DoctorCheck {
    const NAME: &str = "desktop_notifications";
    if facts.osascript_present {
        DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!(
                "{OSASCRIPT} is available; delivery cannot be confirmed for an unbundled binary. \
                 If banners do not appear, allow notifications for the sending app in System \
                 Settings > Notifications"
            ),
        )
    } else {
        DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("{OSASCRIPT} is missing; desktop notifications are unavailable (optional)"),
        )
    }
}

/// Report whether the Keychain prerequisites exist.
///
/// Only the presence of `/usr/bin/security` and the login keychain file is
/// checked. No secret is read and no Keychain API is called, so the check
/// cannot prompt or block; lock state is not probed.
#[must_use]
pub fn check_keychain(facts: &MacosFacts) -> DoctorCheck {
    const NAME: &str = "keychain";
    match (facts.security_tool_present, facts.login_keychain_present) {
        (true, true) => DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            "login keychain file and security tool are present; lock state is not probed, a \
             locked keychain is reported when a provider credential is first requested",
        ),
        (false, _) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            format!("{SECURITY_TOOL} is missing; provider credentials cannot use the Keychain (optional)"),
        ),
        (true, false) => DoctorCheck::new(
            NAME,
            DoctorStatus::Warn,
            "login keychain file not found under ~/Library/Keychains; provider credentials need a \
             login keychain (optional)",
        ),
    }
}

/// Check the directories pohunek must read, classifying macOS privacy denials.
///
/// A denial is a `fail`. `EPERM`, or any denial below a Privacy & Security
/// protected folder, is explained as a TCC consent problem with the exact grant
/// to make; Full Disk Access is deliberately not the default remedy.
#[must_use]
pub fn check_filesystem_access(inputs: &StandardCheckInputs<'_>) -> DoctorCheck {
    const NAME: &str = "filesystem_access";
    let dirs = [
        AccessDir {
            label: "config directory",
            path: inputs.config_dir,
        },
        AccessDir {
            label: "data directory",
            path: inputs.state_dir,
        },
    ];
    let mut denials = Vec::new();
    let mut probed = 0_usize;
    for dir in dirs.iter().chain(inputs.access_dirs.iter()) {
        match std::fs::read_dir(dir.path) {
            Ok(_) => probed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => denials.push(describe_denial(dir, &err, inputs.home_dir)),
        }
    }
    if denials.is_empty() {
        DoctorCheck::new(
            NAME,
            DoctorStatus::Ok,
            format!("{probed} required directories are readable"),
        )
    } else {
        DoctorCheck::new(NAME, DoctorStatus::Fail, denials.join("; "))
    }
}

fn describe_denial(dir: &AccessDir<'_>, err: &std::io::Error, home: Option<&Path>) -> String {
    let denied = err.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(err.raw_os_error(), Some(EPERM | EACCES));
    if !denied {
        return format!("{} {} cannot be read: {err}", dir.label, dir.path.display());
    }
    if err.raw_os_error() == Some(EPERM) || is_tcc_protected(dir.path, home) {
        format!(
            "{label} {path} was denied ({err}), which on macOS is a Privacy & Security consent \
             denial for a protected location. Grant the app that started the failing process access \
             in System Settings > Privacy & Security > Files and Folders: your terminal app when \
             pohunek runs from a terminal, or the pohunekd and pohunek-sessiond executables when \
             launchd runs them. Alternatively keep projects outside Documents, Desktop, Downloads, \
             iCloud Drive and external volumes (for example in ~/Code). Full Disk Access is not \
             required and is not recommended by default",
            label = dir.label,
            path = dir.path.display(),
        )
    } else {
        format!(
            "{label} {path} is not readable ({err}); fix its ownership or mode (chown/chmod)",
            label = dir.label,
            path = dir.path.display(),
        )
    }
}

fn is_tcc_protected(path: &Path, home: Option<&Path>) -> bool {
    if path.starts_with(VOLUMES_ROOT) {
        return true;
    }
    let Some(home) = home else {
        return false;
    };
    TCC_PROTECTED_HOME_SUBDIRS.iter().any(|parts| {
        let root = parts
            .iter()
            .fold(home.to_path_buf(), |dir, part| dir.join(part));
        path.starts_with(root)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::sync::atomic::{AtomicU32, Ordering};

    use protocol::DoctorReport;

    use super::*;

    /// Scratch directory under the symlink-free, short fixture root.
    ///
    /// On macOS the root is `/private/tmp`: `/tmp` and the per-user temp dir
    /// are symlinked, and the latter overflows Darwin's 103-byte socket limit.
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = pohunek_test_support::temp_root()
            .join(format!("pohunek-hc-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_exec(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn facts() -> MacosFacts {
        MacosFacts {
            shell: None,
            etc_shells: Some("# comment\n/bin/zsh\n/bin/bash\n".to_owned()),
            terminal_app_present: true,
            launcher_terminal: None,
            path_var: None,
            launchd_domain: DomainProbe::Ready,
            osascript_present: true,
            security_tool_present: true,
            login_keychain_present: true,
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    // --- worker candidate --------------------------------------------------

    #[test]
    fn worker_candidate_precedence_is_service_then_env_then_sibling() {
        let dir = Path::new("/opt/pohunek/bin");
        let service = resolve_worker_candidate(
            Some(PathBuf::from("/svc/worker")),
            Some(OsString::from("/env/worker")),
            Some(dir),
        )
        .unwrap();
        assert_eq!(service.source, WorkerSource::ServiceConfig);
        assert_eq!(service.path, PathBuf::from("/svc/worker"));

        let env =
            resolve_worker_candidate(None, Some(OsString::from("/env/worker")), Some(dir)).unwrap();
        assert_eq!(env.source, WorkerSource::Environment);

        // The daemon uses any present override, an empty one included, and then
        // rejects a non-absolute path; it never falls back to the sibling.
        let empty = resolve_worker_candidate(None, Some(OsString::new()), Some(dir)).unwrap();
        assert_eq!(empty.source, WorkerSource::Environment);
        assert_eq!(empty.path, PathBuf::new());
        assert_eq!(
            check_worker_executable(Some(&empty)).status,
            DoctorStatus::Fail
        );

        let sibling = resolve_worker_candidate(None, None, Some(dir)).unwrap();
        assert_eq!(sibling.source, WorkerSource::Sibling);
        assert_eq!(sibling.path, dir.join("pohunek-sessiond"));

        assert_eq!(resolve_worker_candidate(None, None, None), None);
    }

    #[test]
    fn worker_executable_fails_when_absent_relative_missing_or_not_executable() {
        assert_eq!(check_worker_executable(None).status, DoctorStatus::Fail);

        let relative = WorkerCandidate {
            path: PathBuf::from("pohunek-sessiond"),
            source: WorkerSource::Environment,
        };
        assert_eq!(
            check_worker_executable(Some(&relative)).status,
            DoctorStatus::Fail
        );

        let dir = temp_dir("worker");
        let missing = WorkerCandidate {
            path: dir.join("missing"),
            source: WorkerSource::Sibling,
        };
        assert_eq!(
            check_worker_executable(Some(&missing)).status,
            DoctorStatus::Fail
        );

        let plain = WorkerCandidate {
            path: write_exec(&dir, "plain", 0o644),
            source: WorkerSource::ServiceConfig,
        };
        let check = check_worker_executable(Some(&plain));
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("service.toml"), "{}", check.detail);

        let good = WorkerCandidate {
            path: write_exec(&dir, "good", 0o755),
            source: WorkerSource::ServiceConfig,
        };
        assert_eq!(
            check_worker_executable(Some(&good)).status,
            DoctorStatus::Ok
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- runtime directory ---------------------------------------------------

    /// Every path below `dir` with its mode, sorted, to prove nothing changed.
    fn tree(dir: &Path) -> Vec<(PathBuf, u32)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u32)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                out.push((path.clone(), meta.permissions().mode() & 0o7777));
                if meta.is_dir() {
                    walk(&path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    fn entry_names(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn runtime_dir_absent_is_ok_without_being_created() {
        let base = temp_dir("rt-absent");
        let dir = base.join("pohunek-1");

        let check = check_runtime_dir_private(&dir);

        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(!dir.exists(), "the check must not create the directory");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_needs_exactly_mode_0700() {
        let base = temp_dir("rt-mode");
        let dir = base.join("rt");
        std::fs::create_dir(&dir).unwrap();

        for loose in [0o755, 0o770, 0o750, 0o777] {
            set_mode(&dir, loose);
            let check = check_runtime_dir_private(&dir);
            assert_eq!(check.status, DoctorStatus::Fail, "mode {loose:o}");
            assert!(check.detail.contains("0700"), "{}", check.detail);
        }
        set_mode(&dir, 0o700);
        assert_eq!(check_runtime_dir_private(&dir).status, DoctorStatus::Ok);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_symlink_or_file_fails() {
        let base = temp_dir("rt-link");
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        set_mode(&target, 0o700);
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();

        assert_eq!(check_runtime_dir_private(&link).status, DoctorStatus::Fail);
        assert_eq!(check_runtime_dir_private(&file).status, DoctorStatus::Fail);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_ancestor_is_refused_like_at_daemon_startup() {
        // Startup opens the runtime root with `TrustedDir::open_or_create_absolute`,
        // which rejects a symlinked path component; the doctor applies the same rule.
        let real = temp_dir("rt-parent-real");
        let holder = temp_dir("rt-parent-link");
        let link_root = holder.join("root");
        std::os::unix::fs::symlink(&real, &link_root).unwrap();
        let dir = real.join("rt");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o700);

        assert_eq!(check_runtime_dir_private(&dir).status, DoctorStatus::Ok);
        let through_link = link_root.join("rt");
        let refused = check_runtime_dir_private(&through_link);
        assert_eq!(refused.status, DoctorStatus::Fail, "{}", refused.detail);
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&holder);
    }

    #[test]
    fn a_symlinked_ancestor_is_still_readable_for_filesystem_access() {
        let real = temp_dir("fa-real");
        let holder = temp_dir("fa-link");
        let link_root = holder.join("root");
        std::os::unix::fs::symlink(&real, &link_root).unwrap();
        let dirs = [AccessDir {
            label: "current directory",
            path: &link_root,
        }];

        let access = check_filesystem_access(&access_inputs(&real, None, &dirs));

        assert_eq!(access.status, DoctorStatus::Ok, "{}", access.detail);
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&holder);
    }

    #[test]
    fn a_missing_directory_is_reported_but_never_created() {
        let base = temp_dir("rt-create");
        let dir = base.join("a").join("b");
        let before = tree(&base);

        let check = check_private_dir("socket_dir_writable", "control socket directory", &dir);

        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        assert!(
            check.detail.contains("startup creates it"),
            "{}",
            check.detail
        );
        assert_eq!(tree(&base), before, "the doctor must not create anything");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_directory_below_an_unsafe_ancestor_cannot_be_created() {
        let base = temp_dir("rt-unsafe-parent");
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        set_mode(&open, 0o777);
        let link = base.join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();

        for parent in [&open, &link] {
            let check = check_private_dir(
                "state_dir_writable",
                "state data directory",
                &parent.join("state"),
            );
            assert_eq!(check.status, DoctorStatus::Fail, "{}", check.detail);
        }
        assert!(!open.join("state").exists());
        set_mode(&open, 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_directory_below_a_read_only_ancestor_cannot_be_created() {
        let base = temp_dir("rt-readonly-parent");
        let parent = base.join("readonly");
        std::fs::create_dir(&parent).unwrap();
        set_mode(&parent, 0o500);

        let check = check_private_dir(
            "state_dir_writable",
            "state data directory",
            &parent.join("state"),
        );

        set_mode(&parent, 0o700);
        if !rustix::process::geteuid().is_root() {
            assert_eq!(check.status, DoctorStatus::Fail, "{}", check.detail);
            assert!(
                check.detail.contains("cannot be created"),
                "{}",
                check.detail
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_dir_that_fails_startup_validation_is_never_written() {
        let base = temp_dir("rt-planted");
        let victim = base.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let dir = base.join("rt");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o777);
        std::os::unix::fs::symlink(&victim, dir.join(crate::PROBE_FILE)).unwrap();
        let before = tree(&base);

        assert_eq!(check_runtime_dir_private(&dir).status, DoctorStatus::Fail);
        let refused = check_private_dir("socket_dir_writable", "control socket directory", &dir);

        assert_eq!(refused.status, DoctorStatus::Fail, "{}", refused.detail);
        assert!(
            refused.detail.contains("nothing was written"),
            "{}",
            refused.detail
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert_eq!(tree(&base), before, "nothing was created or removed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_planted_probe_symlink_in_a_private_dir_is_not_written_through() {
        let base = temp_dir("rt-planted-ok");
        let victim = base.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let dir = base.join("rt");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o700);
        std::os::unix::fs::symlink(&victim, dir.join(crate::PROBE_FILE)).unwrap();

        let check = check_private_dir("socket_dir_writable", "control socket directory", &dir);

        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert_eq!(entry_names(&dir), [crate::PROBE_FILE]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worker_roots_need_a_real_owner_private_directory() {
        // A directory owned by another account cannot be constructed without
        // chown, so the foreign-owner rule is covered by the platform crate's
        // `TrustedDir` tests this check delegates to.
        let base = temp_dir("worker-roots");
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let check =
            |dir: &Path| check_private_dir("worker_runtime_root", "worker runtime root", dir);

        set_mode(&real, 0o755);
        assert_eq!(check(&real).status, DoctorStatus::Fail, "wrong mode");
        set_mode(&real, 0o700);
        assert_eq!(check(&real).status, DoctorStatus::Ok);
        assert_eq!(check(&link).status, DoctorStatus::Fail, "symlink");
        assert_eq!(check(&base.join("absent")).status, DoctorStatus::Ok);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_log_dir_check_validates_the_state_root_and_never_creates() {
        let base = temp_dir("log-dir");
        let root = base.join("state");
        let logs = root.join("logs");

        // Absent state root and log dir: creatable, nothing made.
        let before = tree(&base);
        assert_eq!(check_log_dir(&logs).status, DoctorStatus::Ok);
        assert_eq!(tree(&base), before);

        // A state root with the default umask mode is refused, as at startup.
        std::fs::create_dir(&root).unwrap();
        set_mode(&root, 0o755);
        assert_eq!(check_log_dir(&logs).status, DoctorStatus::Fail);

        set_mode(&root, 0o700);
        assert_eq!(check_log_dir(&logs).status, DoctorStatus::Ok);
        std::fs::create_dir(&logs).unwrap();
        assert_eq!(check_log_dir(&logs).status, DoctorStatus::Ok);
        assert!(entry_names(&logs).is_empty(), "the probe file is removed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_daemon_free_process_fixture_layout_passes_every_host_check() {
        // The layout `crates/cli/tests/session_process_api.rs` builds: XDG
        // directories pre-created with the default umask, then the ones the
        // daemon creates privately chmod'ed to 0700.
        let root = temp_dir("fx");
        let bin = temp_dir("fx-bin");
        write_exec(&bin, "git", 0o755);
        let worker = WorkerCandidate {
            path: write_exec(&bin, "pohunek-sessiond", 0o755),
            source: WorkerSource::Environment,
        };
        for dir in [
            "run/pohunek",
            "state/pohunek/logs",
            "data",
            "config",
            "cache",
            "home",
            "logs",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for dir in ["run/pohunek", "state/pohunek", "state/pohunek/logs"] {
            set_mode(&root.join(dir), 0o700);
        }
        let mut f = facts();
        f.path_var = Some(OsString::from(bin.as_os_str()));
        let config_dir = root.join("config").join("pohunek");
        let runtime = root.join("run/pohunek");
        let data = root.join("data/pohunek");
        let logs = root.join("state/pohunek/logs");
        let home = root.join("home");
        let inputs = StandardCheckInputs {
            socket_dir: &runtime,
            state_dir: &data,
            log_dir: &logs,
            launcher_bin_dir: &root,
            sway_config_dir: &root,
            config_dir: &config_dir,
            home_dir: Some(&home),
            effective_uid: std::fs::metadata(&root).unwrap().uid(),
            worker: Some(&worker),
            access_dirs: &[],
        };

        let checks = standard_checks(&inputs, &f);

        let failing = failing_names(&checks);
        assert!(failing.is_empty(), "{checks:?}");
        for name in [
            "runtime_dir_private",
            "socket_path_length",
            "socket_dir_writable",
            "state_dir_writable",
            "log_dir_writable",
            "worker_runtime_root",
            "worker_state_root",
            "worker_executable",
        ] {
            let check = checks.iter().find(|c| c.name == name).expect(name);
            assert_eq!(check.status, DoctorStatus::Ok, "{name}: {}", check.detail);
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[test]
    fn the_full_list_creates_no_directory_even_with_a_default_umask_tree() {
        let base = temp_dir("no-create");
        let bin = temp_dir("no-create-bin");
        write_exec(&bin, "git", 0o755);
        let before = tree(&base);

        let checks = assembled(&base, OsString::from(bin.as_os_str()), None);

        assert_eq!(tree(&base), before, "the doctor must not create anything");
        for name in [
            "socket_dir_writable",
            "state_dir_writable",
            "log_dir_writable",
            "worker_runtime_root",
            "worker_state_root",
        ] {
            let check = checks.iter().find(|c| c.name == name).expect(name);
            assert_eq!(check.status, DoctorStatus::Ok, "{name}: {}", check.detail);
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&bin);
    }

    // --- socket path length --------------------------------------------------

    #[test]
    fn socket_path_length_accepts_the_default_darwin_runtime_dir() {
        let check = check_socket_path_length(Path::new("/private/tmp/pohunek-501"));
        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
    }

    #[test]
    fn socket_path_length_fails_for_overlong_runtime_dir() {
        let long = format!("/private/tmp/{}", "x".repeat(90));

        let check = check_socket_path_length(Path::new(&long));

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("XDG_RUNTIME_DIR"), "{}", check.detail);
    }

    #[test]
    fn socket_path_length_fails_when_only_the_worker_path_overflows() {
        // The daemon socket fits, but the workers/<id>/control.sock path does not.
        let dir = format!("/private/tmp/{}", "y".repeat(40));
        validate_staged_socket_path(
            Path::new(&dir).join(SOCKET_NAME),
            Platform::MacOs,
            SocketKind::Daemon,
        )
        .expect("the daemon socket fits");

        let check = check_socket_path_length(Path::new(&dir));

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("worker"), "{}", check.detail);
    }

    // --- login shell / terminal ----------------------------------------------

    #[test]
    fn login_shell_reports_each_failure_mode_as_warn() {
        let dir = temp_dir("shell");
        let zsh = write_exec(&dir, "zsh", 0o755);
        let plain = write_exec(&dir, "plain", 0o644);
        let listing = |path: &Path| format!("# shells\n{}\n", path.display());

        let mut f = facts();
        assert_eq!(check_login_shell(&f).status, DoctorStatus::Warn);

        f.shell = Some(OsString::from("zsh"));
        assert!(check_login_shell(&f).detail.contains("not an absolute"));

        f.shell = Some(plain.clone().into_os_string());
        assert!(check_login_shell(&f).detail.contains("not an executable"));

        f.shell = Some(zsh.clone().into_os_string());
        f.etc_shells = Some(listing(&plain));
        let unlisted = check_login_shell(&f);
        assert_eq!(unlisted.status, DoctorStatus::Warn);
        assert!(
            unlisted.detail.contains("not listed"),
            "{}",
            unlisted.detail
        );

        f.etc_shells = None;
        assert!(check_login_shell(&f).detail.contains("could not be read"));

        f.etc_shells = Some(listing(&zsh));
        assert_eq!(check_login_shell(&f).status, DoctorStatus::Ok);

        // A commented-out entry does not count as listed.
        f.etc_shells = Some(format!("#{}\n", zsh.display()));
        assert_eq!(check_login_shell(&f).status, DoctorStatus::Warn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launcher_terminal_mirrors_the_launcher_parser() {
        assert_eq!(parse_launcher_terminal("# terminal=x\n"), None);
        assert_eq!(parse_launcher_terminal("terminal=\n"), None);
        assert_eq!(
            parse_launcher_terminal("terminal=kitty\n  terminal = wezterm start \n"),
            Some("wezterm start".to_owned())
        );
        // The last assignment wins even when empty: the launcher then falls
        // back to $TERMINAL, so the earlier value is not in effect.
        assert_eq!(
            parse_launcher_terminal("host=local\nterminal=alacritty\nterminal=\n"),
            None
        );
        // A non-comment line without `=` makes the launcher's lookup fail.
        assert_eq!(
            parse_launcher_terminal("terminal=kitty\nbroken line\n"),
            None
        );
    }

    #[test]
    fn a_terminal_value_with_arguments_is_not_one_executable() {
        let dir = temp_dir("term-args");
        write_exec(&dir, "kitty", 0o755);
        let mut f = facts();
        f.path_var = Some(OsString::from(dir.as_os_str()));

        f.launcher_terminal = Some("kitty".to_owned());
        assert_eq!(check_terminal(&f).status, DoctorStatus::Ok);

        // The launcher runs the whole value as one program name.
        f.launcher_terminal = Some("kitty -e".to_owned());
        let with_args = check_terminal(&f);
        assert_eq!(with_args.status, DoctorStatus::Warn, "{}", with_args.detail);
        assert!(
            with_args.detail.contains("single program"),
            "{}",
            with_args.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_check_covers_stock_and_configured_terminals() {
        let dir = temp_dir("term");
        write_exec(&dir, "kitty", 0o755);
        let path = OsString::from(dir.as_os_str());

        let mut f = facts();
        f.path_var = Some(path);
        assert_eq!(check_terminal(&f).status, DoctorStatus::Ok);

        f.launcher_terminal = Some("kitty".to_owned());
        let configured = check_terminal(&f);
        assert_eq!(configured.status, DoctorStatus::Ok);
        assert!(configured.detail.contains("kitty"));

        f.launcher_terminal = Some("no-such-terminal --flag".to_owned());
        let broken = check_terminal(&f);
        assert_eq!(broken.status, DoctorStatus::Warn);
        assert!(broken.detail.contains("launcher.conf"), "{}", broken.detail);

        f.terminal_app_present = false;
        f.launcher_terminal = None;
        assert_eq!(check_terminal(&f).status, DoctorStatus::Warn);

        f.launcher_terminal = Some("kitty".to_owned());
        assert_eq!(check_terminal(&f).status, DoctorStatus::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- launchd -------------------------------------------------------------

    struct FakeRunner {
        outcome: RunOutcome,
        seen: RefCell<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(outcome: RunOutcome) -> Self {
            Self {
                outcome,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, arguments: &[&str], _deadline: Duration) -> RunOutcome {
            self.seen
                .borrow_mut()
                .push(arguments.iter().map(|arg| (*arg).to_owned()).collect());
            self.outcome
        }
    }

    #[test]
    fn domain_probe_maps_exit_statuses_and_passes_argv_only() {
        let cases = [
            (RunOutcome::Exited(Some(0)), DomainProbe::Ready),
            (RunOutcome::Exited(Some(112)), DomainProbe::Missing),
            (RunOutcome::Exited(Some(5)), DomainProbe::Failed(Some(5))),
            (RunOutcome::Exited(None), DomainProbe::Failed(None)),
            (RunOutcome::TimedOut, DomainProbe::TimedOut),
            (RunOutcome::Unavailable, DomainProbe::Unavailable),
        ];
        for (outcome, expected) in cases {
            let runner = FakeRunner::new(outcome);
            assert_eq!(
                probe_launchd_domain(&runner, 501, Duration::from_secs(1)),
                expected
            );
            assert_eq!(
                runner.seen.borrow().as_slice(),
                [vec!["print".to_owned(), "gui/501".to_owned()]]
            );
        }
    }

    #[test]
    fn launchd_domain_check_fails_only_for_a_missing_domain() {
        let status = |probe| check_launchd_domain(probe, 501).status;
        assert_eq!(status(DomainProbe::Ready), DoctorStatus::Ok);
        assert_eq!(status(DomainProbe::Missing), DoctorStatus::Fail);
        assert_eq!(status(DomainProbe::Failed(Some(9))), DoctorStatus::Warn);
        assert_eq!(status(DomainProbe::Failed(None)), DoctorStatus::Warn);
        assert_eq!(status(DomainProbe::TimedOut), DoctorStatus::Warn);
        assert_eq!(status(DomainProbe::Unavailable), DoctorStatus::Warn);
        assert!(check_launchd_domain(DomainProbe::Missing, 501)
            .detail
            .contains("gui/501"));
    }

    #[test]
    fn process_runner_reports_exit_status_timeout_and_unavailable() {
        let sh = ProcessRunner::for_program("/bin/sh");
        let generous = Duration::from_secs(30);

        assert_eq!(
            sh.run(&["-c", "exit 112"], generous),
            RunOutcome::Exited(Some(112))
        );
        assert_eq!(
            sh.run(&["-c", "exit 0"], generous),
            RunOutcome::Exited(Some(0))
        );
        assert_eq!(
            sh.run(&["-c", "kill -9 $$"], generous),
            RunOutcome::Exited(None)
        );
        assert_eq!(
            sh.run(&["-c", "exec sleep 60"], Duration::from_millis(50)),
            RunOutcome::TimedOut
        );
        assert_eq!(
            ProcessRunner::for_program("/nonexistent/launchctl").run(&["print"], generous),
            RunOutcome::Unavailable
        );
    }

    // --- filesystem access -----------------------------------------------------

    fn access_inputs<'a>(
        base: &'a Path,
        home: Option<&'a Path>,
        access_dirs: &'a [AccessDir<'a>],
    ) -> StandardCheckInputs<'a> {
        StandardCheckInputs {
            socket_dir: base,
            state_dir: base,
            log_dir: base,
            launcher_bin_dir: base,
            sway_config_dir: base,
            config_dir: base,
            home_dir: home,
            effective_uid: 0,
            worker: None,
            access_dirs,
        }
    }

    #[test]
    fn filesystem_access_is_ok_for_readable_and_absent_directories() {
        let base = temp_dir("fs-ok");
        let missing = base.join("missing");
        let dirs = [AccessDir {
            label: "current directory",
            path: &missing,
        }];

        let check = check_filesystem_access(&access_inputs(&base, None, &dirs));

        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn filesystem_access_explains_a_denied_protected_folder_without_full_disk_access() {
        let base = temp_dir("fs-tcc");
        let project = base.join("Documents").join("project");
        std::fs::create_dir_all(&project).unwrap();
        set_mode(&project, 0o000);
        if std::fs::read_dir(&project).is_ok() {
            // Running with privileges that bypass mode bits: nothing to assert.
            set_mode(&project, 0o700);
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let dirs = [AccessDir {
            label: "current directory",
            path: &project,
        }];

        let check = check_filesystem_access(&access_inputs(&base, Some(&base), &dirs));

        set_mode(&project, 0o700);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(
            check.detail.contains("Privacy & Security"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("Files and Folders"));
        assert!(check.detail.contains("terminal app"));
        assert!(check.detail.contains("Full Disk Access is not required"));
        assert!(!check.detail.contains("grant Full Disk Access"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn filesystem_access_reports_plain_permissions_outside_protected_folders() {
        let base = temp_dir("fs-plain");
        let project = base.join("Code").join("project");
        std::fs::create_dir_all(&project).unwrap();
        set_mode(&project, 0o000);
        if std::fs::read_dir(&project).is_ok() {
            set_mode(&project, 0o700);
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let dirs = [AccessDir {
            label: "current directory",
            path: &project,
        }];

        let check = check_filesystem_access(&access_inputs(&base, Some(&base), &dirs));

        set_mode(&project, 0o700);
        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("chown/chmod"), "{}", check.detail);
        assert!(!check.detail.contains("Privacy"), "{}", check.detail);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn eperm_is_always_a_privacy_denial() {
        let dir = AccessDir {
            label: "current directory",
            path: Path::new("/Volumes/External/project"),
        };
        let err = std::io::Error::from_raw_os_error(EPERM);

        let detail = describe_denial(&dir, &err, None);

        assert!(detail.contains("Privacy & Security"), "{detail}");

        let elsewhere = AccessDir {
            label: "current directory",
            path: Path::new("/srv/project"),
        };
        assert!(describe_denial(&elsewhere, &err, None).contains("Privacy & Security"));
    }

    #[test]
    fn protected_locations_cover_documents_desktop_downloads_icloud_and_volumes() {
        let home = Path::new("/Users/me");
        for path in [
            "/Users/me/Documents/a",
            "/Users/me/Desktop",
            "/Users/me/Downloads/x/y",
            "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/p",
            "/Users/me/Library/CloudStorage/Dropbox/p",
            "/Volumes/Disk/p",
        ] {
            assert!(is_tcc_protected(Path::new(path), Some(home)), "{path}");
        }
        for path in ["/Users/me/Code/p", "/Users/me/DocumentsOld/p", "/tmp/p"] {
            assert!(!is_tcc_protected(Path::new(path), Some(home)), "{path}");
        }
    }

    // --- optional capabilities ---------------------------------------------------

    #[test]
    fn notification_and_keychain_entries_are_never_fatal() {
        let mut f = facts();
        let notify = check_desktop_notifications(&f);
        assert_eq!(notify.status, DoctorStatus::Ok);
        assert!(notify.detail.contains("cannot be confirmed"));
        assert_eq!(check_keychain(&f).status, DoctorStatus::Ok);

        f.osascript_present = false;
        f.security_tool_present = false;
        f.login_keychain_present = false;
        assert_eq!(check_desktop_notifications(&f).status, DoctorStatus::Warn);
        assert_eq!(check_keychain(&f).status, DoctorStatus::Warn);

        f.security_tool_present = true;
        assert_eq!(check_keychain(&f).status, DoctorStatus::Warn);
    }

    // --- assembled list --------------------------------------------------------------

    fn assembled(
        base: &Path,
        path_var: OsString,
        worker: Option<&WorkerCandidate>,
    ) -> Vec<DoctorCheck> {
        let mut f = facts();
        f.path_var = Some(path_var);
        let config_dir = base.join("config");
        let inputs = StandardCheckInputs {
            socket_dir: &base.join("runtime"),
            state_dir: &base.join("data"),
            // `BasePaths` puts the log directory directly below the state root.
            log_dir: &base.join("state").join("logs"),
            launcher_bin_dir: &base.join("bin"),
            sway_config_dir: &base.join("sway"),
            config_dir: &config_dir,
            home_dir: Some(base),
            effective_uid: std::fs::metadata(base).unwrap().uid(),
            worker,
            access_dirs: &[],
        };
        standard_checks(&inputs, &f)
    }

    #[test]
    fn macos_list_is_ordered_and_omits_linux_only_capabilities() {
        let base = temp_dir("list");
        let bin = temp_dir("list-bin");
        write_exec(&bin, "git", 0o755);
        let worker = WorkerCandidate {
            path: write_exec(&bin, "pohunek-sessiond", 0o755),
            source: WorkerSource::Sibling,
        };

        let checks = assembled(&base, OsString::from(bin.as_os_str()), Some(&worker));
        let names = checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "bin:git",
                "bin:codex",
                "bin:claude",
                "runtime_dir_private",
                "socket_path_length",
                "socket_dir_writable",
                "state_dir_writable",
                "log_dir_writable",
                "worker_runtime_root",
                "worker_state_root",
                "filesystem_access",
                "netbird_cli",
                "schema_version",
                "worker_executable",
                "login_shell",
                "terminal",
                "launchd_domain",
                "desktop_notifications",
                "keychain",
            ]
        );
        for linux_only in [
            "bin:rofi",
            "bin:swaymsg",
            "bin:python3",
            "bin:timeout",
            "launcher_scripts",
            "sway_include",
        ] {
            assert!(!names.contains(&linux_only), "{linux_only} leaked");
        }
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[test]
    fn missing_optional_agents_warn_but_a_missing_worker_fails_overall() {
        let base = temp_dir("overall");
        let bin = temp_dir("overall-bin");
        write_exec(&bin, "git", 0o755);
        let worker = WorkerCandidate {
            path: write_exec(&bin, "pohunek-sessiond", 0o755),
            source: WorkerSource::Sibling,
        };
        let path = OsString::from(bin.as_os_str());

        let healthy = assembled(&base, path.clone(), Some(&worker));
        let by_name = |name: &str| healthy.iter().find(|c| c.name == name).unwrap().status;
        assert_eq!(by_name("bin:codex"), DoctorStatus::Warn);
        assert_eq!(by_name("bin:claude"), DoctorStatus::Warn);
        let fails: Vec<_> = healthy
            .iter()
            .filter(|c| c.status == DoctorStatus::Fail)
            .collect();
        assert!(fails.is_empty(), "{fails:?}");

        let broken = assembled(&base, path, None);
        assert_eq!(
            DoctorReport::from_checks(broken).overall,
            DoctorStatus::Fail
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&bin);
    }

    fn failing_names(checks: &[DoctorCheck]) -> Vec<&str> {
        checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Fail)
            .map(|check| check.name.as_str())
            .collect()
    }

    #[test]
    fn an_isolated_fixture_with_a_default_umask_runtime_dir_fails_and_is_not_probed() {
        let base = temp_dir("umask");
        let bin = temp_dir("umask-bin");
        write_exec(&bin, "git", 0o755);
        let worker = WorkerCandidate {
            path: write_exec(&bin, "pohunek-sessiond", 0o755),
            source: WorkerSource::Environment,
        };
        let runtime = base.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let path = OsString::from(bin.as_os_str());

        set_mode(&runtime, 0o755);
        let loose = assembled(&base, path.clone(), Some(&worker));
        assert_eq!(
            failing_names(&loose),
            ["runtime_dir_private", "socket_dir_writable"]
        );

        set_mode(&runtime, 0o700);
        let private = assembled(&base, path, Some(&worker));
        assert!(failing_names(&private).is_empty(), "{private:?}");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[test]
    fn a_long_isolated_root_overflows_the_worker_socket_limit() {
        // Shape of a test fixture root on macOS: canonical /private/tmp plus a
        // descriptive directory name, with the runtime dir at run/pohunek.
        let long = Path::new("/private/tmp/pohunek-cli-process-api-12345-0/run/pohunek");
        let short = Path::new("/private/tmp/pcpa-12345-0/run/pohunek");

        let overflow = check_socket_path_length(long);
        assert_eq!(overflow.status, DoctorStatus::Fail);
        assert!(overflow.detail.contains("worker"), "{}", overflow.detail);
        assert_eq!(check_socket_path_length(short).status, DoctorStatus::Ok);
    }

    #[test]
    fn agent_hint_explains_launchd_path_without_naming_a_homebrew_prefix() {
        let mut f = facts();
        f.path_var = Some(OsString::new());

        let check = agent_binary("codex", &f);

        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("launchd"), "{}", check.detail);
        assert!(!check.detail.contains("/opt/homebrew"));
    }
}
