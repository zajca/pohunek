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
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pohunek_paths::{
    validate_staged_socket_path, worker_socket_path, Platform, SocketKind, SOCKET_NAME,
    WORKERS_SUBDIR,
};
use protocol::{DoctorCheck, DoctorStatus};

use crate::executable::{is_executable_file, resolve_executable};
use crate::{binary_with_path, dir_writable_with, netbird, StandardCheckInputs};

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

/// Directory mode of the runtime root: owner-only.
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Group and other permission bits that make the runtime root unsafe.
const RUNTIME_DIR_FORBIDDEN_BITS: u32 = 0o077;

/// Where a resolved worker executable path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSource {
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
/// `executable_dir`. An empty override is ignored.
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
    if let Some(value) = env_override.filter(|value| !value.is_empty()) {
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
/// Lines are `key=value`; `#` starts a comment; the last non-empty assignment
/// wins. An unreadable or absent file yields `None`.
#[must_use]
pub fn read_launcher_terminal(config_dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(config_dir.join("launcher.conf")).ok()?;
    parse_launcher_terminal(&contents)
}

fn parse_launcher_terminal(contents: &str) -> Option<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| key.trim() == "terminal")
        .map(|(_, value)| value.trim())
        .rfind(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The ordered macOS probe list.
///
/// Optional Linux capabilities (rofi, swaymsg, `timeout`, `$TERMINAL`, the
/// launcher scripts, the sway include) and the launcher-only `python3` probe
/// are omitted: they are not macOS capabilities, and hook interpreter
/// readiness is reported by `pohunek integration doctor`.
#[must_use]
pub fn standard_checks(inputs: &StandardCheckInputs<'_>, facts: &MacosFacts) -> Vec<DoctorCheck> {
    vec![
        binary_with_path("git", true, facts.path_var.as_deref(), ""),
        agent_binary("codex", facts),
        agent_binary("claude", facts),
        check_runtime_dir_private(inputs.socket_dir, inputs.effective_uid),
        check_socket_path_length(inputs.socket_dir),
        dir_writable_with(
            "socket_dir_writable",
            inputs.socket_dir,
            "control socket directory",
            create_private_dirs,
        ),
        crate::dir_writable(
            "state_dir_writable",
            inputs.state_dir,
            "state data directory",
        ),
        crate::dir_writable("log_dir_writable", inputs.log_dir, "log directory"),
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

fn create_private_dirs(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(RUNTIME_DIR_MODE)
        .create(dir)
}

/// Check the runtime directory's type, owner and mode without creating it.
///
/// The default root lives in the world-writable `/private/tmp`, so another
/// account could pre-create it. An absent directory is `ok`: the daemon
/// creates it owner-private.
#[must_use]
pub fn check_runtime_dir_private(dir: &Path, effective_uid: u32) -> DoctorCheck {
    const NAME: &str = "runtime_dir_private";
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return DoctorCheck::new(
                NAME,
                DoctorStatus::Ok,
                format!(
                    "{} does not exist yet; the daemon creates it with mode 0700",
                    dir.display()
                ),
            );
        }
        Err(err) => {
            return DoctorCheck::new(
                NAME,
                DoctorStatus::Fail,
                format!("cannot inspect runtime directory {}: {err}", dir.display()),
            );
        }
    };
    if !meta.file_type().is_dir() {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{} is not a real directory (a symlink or file is refused); remove it so the daemon can create it",
                dir.display()
            ),
        );
    }
    if meta.uid() != effective_uid {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{} is owned by uid {} but pohunek runs as uid {effective_uid}; remove it or set \
                 XDG_RUNTIME_DIR to a directory you own",
                dir.display(),
                meta.uid()
            ),
        );
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & RUNTIME_DIR_FORBIDDEN_BITS != 0 {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "{} has mode {mode:04o}, which lets other accounts reach the control socket; run \
                 'chmod 700 {}'",
                dir.display(),
                dir.display()
            ),
        );
    }
    DoctorCheck::new(
        NAME,
        DoctorStatus::Ok,
        format!("{} is owned by you with mode {mode:04o}", dir.display()),
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
    const NAME: &str = "worker_executable";
    let Some(candidate) = candidate else {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            "no pohunek-sessiond location could be derived; run 'pohunek service install' or set \
             POHUNEK_WORKER_BIN to its absolute path",
        );
    };
    let source = candidate.source.describe();
    if !candidate.path.is_absolute() {
        return DoctorCheck::new(
            NAME,
            DoctorStatus::Fail,
            format!(
                "worker path {} (from {source}) is not absolute; use an absolute path",
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
/// The configured terminal is optional; a configured but unresolvable command
/// is a `warn`, as is a host with neither the stock app nor a working
/// configured command.
#[must_use]
pub fn check_terminal(facts: &MacosFacts) -> DoctorCheck {
    const NAME: &str = "terminal";
    let configured = facts.launcher_terminal.as_deref().map(|command| {
        let program = command.split_whitespace().next().unwrap_or(command);
        let resolved = resolve_executable(program, facts.path_var.as_deref());
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
                "configured terminal '{command}' (launcher.conf) does not resolve to an executable; \
                 fix or remove the 'terminal=' key"
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use protocol::DoctorReport;

    use super::*;

    /// Scratch directory under `/tmp`.
    ///
    /// The Darwin socket limit is 103 bytes, and macOS's per-user temp dir is
    /// long enough to overflow it, so tests that build a runtime dir stay short.
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            PathBuf::from("/tmp").join(format!("pohunek-hc-{tag}-{}-{n}", std::process::id()));
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

        let sibling = resolve_worker_candidate(None, Some(OsString::new()), Some(dir)).unwrap();
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

    #[test]
    fn runtime_dir_absent_is_ok_without_being_created() {
        let base = temp_dir("rt-absent");
        let dir = base.join("pohunek-1");

        let check = check_runtime_dir_private(&dir, 0);

        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(!dir.exists(), "the check must not create the directory");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_with_group_or_other_access_fails_with_chmod_remediation() {
        let base = temp_dir("rt-mode");
        let dir = base.join("rt");
        std::fs::create_dir(&dir).unwrap();
        let uid = std::fs::metadata(&dir).unwrap().uid();

        set_mode(&dir, 0o755);
        let loose = check_runtime_dir_private(&dir, uid);
        assert_eq!(loose.status, DoctorStatus::Fail);
        assert!(loose.detail.contains("chmod 700"), "{}", loose.detail);

        set_mode(&dir, 0o700);
        assert_eq!(
            check_runtime_dir_private(&dir, uid).status,
            DoctorStatus::Ok
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_owned_by_another_uid_fails() {
        let base = temp_dir("rt-owner");
        let dir = base.join("rt");
        std::fs::create_dir(&dir).unwrap();
        set_mode(&dir, 0o700);
        let uid = std::fs::metadata(&dir).unwrap().uid();

        let check = check_runtime_dir_private(&dir, uid.wrapping_add(1));

        assert_eq!(check.status, DoctorStatus::Fail);
        assert!(check.detail.contains("owned by uid"), "{}", check.detail);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn runtime_dir_symlink_or_file_fails() {
        let base = temp_dir("rt-link");
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();

        assert_eq!(
            check_runtime_dir_private(&link, 0).status,
            DoctorStatus::Fail
        );
        assert_eq!(
            check_runtime_dir_private(&file, 0).status,
            DoctorStatus::Fail
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn private_dir_creation_uses_mode_0700() {
        let base = temp_dir("rt-create");
        let dir = base.join("a").join("b");

        create_private_dirs(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&base);
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
    fn launcher_terminal_takes_the_last_non_empty_assignment() {
        assert_eq!(parse_launcher_terminal("# terminal=x\n"), None);
        assert_eq!(parse_launcher_terminal("terminal=\n"), None);
        assert_eq!(
            parse_launcher_terminal("terminal=kitty\n  terminal = wezterm start \n"),
            Some("wezterm start".to_owned())
        );
        assert_eq!(
            parse_launcher_terminal("host=local\nterminal=alacritty -e\nterminal=\n"),
            Some("alacritty -e".to_owned())
        );
    }

    #[test]
    fn terminal_check_covers_stock_and_configured_terminals() {
        let dir = temp_dir("term");
        write_exec(&dir, "kitty", 0o755);
        let path = OsString::from(dir.as_os_str());

        let mut f = facts();
        f.path_var = Some(path);
        assert_eq!(check_terminal(&f).status, DoctorStatus::Ok);

        f.launcher_terminal = Some("kitty -e".to_owned());
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
            state_dir: &base.join("state"),
            log_dir: &base.join("logs"),
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

    #[test]
    fn agent_hint_explains_launchd_path_without_naming_a_homebrew_prefix() {
        let mut f = facts();
        f.path_var = Some(OsString::new());

        let check = agent_binary("codex", &f);

        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("launchd"), "{}", check.detail);
        assert!(!check.detail.contains("/opt/homebrew"));
    }

    #[test]
    fn socket_dir_probe_creates_the_runtime_dir_private() {
        let base = temp_dir("private-create");
        let bin = temp_dir("private-create-bin");
        write_exec(&bin, "git", 0o755);
        let checks = assembled(&base, OsString::from(bin.as_os_str()), None);

        let socket = checks
            .iter()
            .find(|c| c.name == "socket_dir_writable")
            .unwrap();
        assert_eq!(socket.status, DoctorStatus::Ok);
        let mode = std::fs::metadata(base.join("runtime"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&bin);
    }
}
