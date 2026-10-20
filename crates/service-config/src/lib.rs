//! Loads, validates, and writes the installation's `service.toml`.
//!
//! `pohunek service install` records every value the native supervision path
//! needs in `<config>/pohunek/service.toml`. The daemon (`pohunekd
//! --service-config <abs>`) and each worker generation (`pohunek-sessiond
//! --service-config <abs>`) load the same file and fail fast when it is
//! missing or invalid. There are no defaults: every key is required, unknown
//! keys are rejected, and each value is checked against a documented bound.
//!
//! # Schema
//!
//! ```toml
//! schema_version = 6
//! prefix = "/home/u/.local"                       # absolute install prefix
//! active_version = "0.31.6"                       # <prefix>/libexec/pohunek/<version>
//!
//! [namespace]                                      # inputs of the job namespace
//! uid = 1000
//! state_root = "/home/u/.local/state/pohunek"     # canonical application state dir
//! runtime_root = "/run/user/1000/pohunek"         # canonical application runtime dir
//!
//! [deadlines]                                      # milliseconds, 1..=600000
//! worker_connect_ms = 10000
//! worker_initialize_ms = 45000
//! launchctl_command_ms = 10000
//! worker_exit_timeout_ms = 30000
//! daemon_exit_timeout_ms = 30000
//! daemon_restart_throttle_ms = 5000
//!
//! [environment]
//! search_path = ["/opt/homebrew/bin", "/usr/bin"]  # absolute normalized directories; may be empty
//! allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]   # names or trailing-`*` prefixes
//!
//! [sweep]
//! grace_ms = 5000                                  # 1..=600000
//!
//! [input]                                         # milliseconds, 1..=600000
//! initial_startup_grace_ms = 5000
//! submit_delay_ms = 150
//! write_stall_timeout_ms = 3000
//!
//! [waits]                                          # milliseconds, 5000..=3600000
//! turn_default_ms = 300000                         # at most turn_max_ms
//! turn_max_ms = 1800000
//!
//! [hooks]
//! stderr_tail_bytes = 4096                         # 0..=32768; 0 disables the capture
//!
//! [limits]
//! open_files = 8192                                # 256..=1048576
//! ```
//!
//! This crate validates allowlist patterns only syntactically; which names
//! the installer allowlists by default is decided by its caller.
//!
//! `[waits]` bounds `session.wait_turn`: `turn_default_ms` applies when a
//! caller omits its timeout, and `turn_max_ms` is the ceiling a requested
//! timeout may not exceed. See [`WaitTiming`] and [`DEFAULT_WAIT_TIMING`].
//!
//! `[hooks]` bounds how much of a failing worktree-create hook's stderr the
//! daemon keeps for the `session.new` warning. See
//! [`DEFAULT_HOOK_STDERR_TAIL_BYTES`] and [`MAX_HOOK_STDERR_TAIL_BYTES`].
//!
//! # Upgrades
//!
//! A normal load accepts only [`SCHEMA_VERSION`]. The installer reads the
//! previous schemas through [`ServiceConfig::load_upgrade_source`], which
//! assigns the documented defaults to the keys those schemas lack: schema 2
//! has neither `[input]` nor `[waits]`, schema 3 has no `[waits]` and no
//! `input.write_stall_timeout_ms`, schema 4 has no
//! `input.write_stall_timeout_ms`, and none of them nor schema 5 has
//! `[hooks]`. Every value a previous file records is kept.
//!
//! `environment.search_path` is the executable search path the installer
//! resolved (see `pohunek_platform::shell_env`) and hands to the daemon job as
//! its `PATH`. Each entry is an absolute, normalized directory without `:` or
//! control characters, no entry repeats, and the joined value stays within
//! [`MAX_SEARCH_PATH_BYTES`]. An empty list means the job keeps the service
//! manager's own `PATH`. Upgrades preserve the recorded list. The daemon does
//! not read it from the file: it reaches the job definition only when the
//! installer writes one (install, or an upgrade to a different version).
//!
//! # File trust
//!
//! The file must be a regular `0600` file owned by the effective user, with a
//! single link and no ACL granting access beyond its mode bits. Its directory
//! must be owned by the effective user and must not be group- or
//! world-writable, so both `0700` and the common `0755` config directory
//! pass. Integrity rests on that directory: nobody else can replace or rename
//! the file. Confidentiality rests on the `0600` file alone, which is why a
//! readable directory is acceptable. Symlinks and foreign owners are rejected
//! before any byte is parsed. Reads are bounded by [`MAX_CONFIG_BYTES`].
//! [`ServiceConfig::write`] replaces the file atomically through the same
//! trusted directory, creates a missing directory as `0700`, and never changes
//! the mode of an existing one.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//!
//! use pohunek_service_config::ServiceConfig;
//!
//! let config = ServiceConfig::load(Path::new("/home/u/.config/pohunek/service.toml"))?;
//! config.verify_installation(
//!     1000,
//!     Path::new("/home/u/.local/state/pohunek"),
//!     Path::new("/run/user/1000/pohunek"),
//! )?;
//! println!("daemon label: {}", config.namespace().daemon_label());
//! # Ok::<(), pohunek_service_config::ConfigError>(())
//! ```

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_paths::{
    is_normalized_absolute, valid_install_version, BasePaths, InstallLayout,
    MAX_INSTALL_VERSION_BYTES,
};
use pohunek_platform::filesystem::{FsError, TrustedDir};
use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::{
    Namespace, MAX_JOB_TIMEOUT, MAX_JOB_VALUE_BYTES, MAX_OPEN_FILES, MIN_OPEN_FILES,
};
use serde::{Deserialize, Serialize};

mod error;
pub mod preflight;
pub mod runtimes;

#[doc(inline)]
pub use error::ConfigError;

/// The only schema version this crate reads and writes.
///
/// A file with any other value is rejected before its keys are interpreted,
/// so an older binary never half-understands a newer installation and a newer
/// binary never reads a file that lacks keys it requires. Version 3 holds
/// `[input]`; version 4 adds `[waits]`; version 5 adds
/// `input.write_stall_timeout_ms`; version 6 adds `[hooks]`. Schemas 2, 3, 4
/// and 5 are read only through [`ServiceConfig::load_upgrade_source`].
pub const SCHEMA_VERSION: u32 = 6;

/// Input timings written on fresh installs and assigned explicitly when
/// upgrading a schema-2, schema-3 or schema-4 service file, which lack some
/// input keys.
///
/// The write stall bound is 3000 ms, below the 5 s default request timeout of
/// the client (`pohunek-client`). A fire-and-forget `session.input` caller then
/// receives the typed `session_input_stalled` error before its own timeout
/// fires. A reading agent drains its input within microseconds, so 3 s without
/// a single accepted byte means the reader is stuck, not slow.
pub const DEFAULT_INPUT_TIMING: InputTiming = InputTiming {
    initial_startup_grace: Duration::from_secs(5),
    submit_delay: Duration::from_millis(150),
    write_stall_timeout: Duration::from_millis(3000),
};

/// Turn-wait timings written on fresh installs, assigned when upgrading a
/// schema-2 or schema-3 file, and used by a daemon without `service.toml`.
///
/// The five-minute default lets an agent that omits a timeout follow a
/// typical agent turn in one call while still getting a progress report often
/// enough to notice a stuck session. The thirty-minute ceiling covers long
/// turns (large builds, test suites) in one wait, and bounds how long one
/// waiter slot stays taken: a client that disappears without closing its
/// connection, such as a vanished overlay host, holds its slot until the
/// timeout. Raising the ceiling lengthens that worst case; lowering the
/// default makes callers that omit a timeout poll more often.
pub const DEFAULT_WAIT_TIMING: WaitTiming = WaitTiming {
    turn_default: Duration::from_mins(5),
    turn_max: Duration::from_mins(30),
};

/// Stderr tail size written on fresh installs, assigned when upgrading a
/// schema-2 to schema-5 file, and used by a daemon without `service.toml`.
///
/// 4096 bytes hold the last few dozen lines of a typical tool error (a failed
/// `npm install`, `git` or compiler run), which is what tells the owner why
/// project setup failed, while the warning stays a small JSON value.
pub const DEFAULT_HOOK_STDERR_TAIL_BYTES: u32 = 4096;

/// Largest accepted `hooks.stderr_tail_bytes`.
///
/// One `session.new` result can carry four tails (`pre-create` and
/// `post-create`, each from the host-global and the in-repo layer). Lossy
/// UTF-8 decoding turns one invalid byte into a three-byte replacement
/// character, so four 32 KiB tails stay below 384 KiB of text, well within the
/// 1 MiB control line together with the rest of the session. 32 KiB is also
/// several hundred lines, more than anyone reads in a warning.
pub const MAX_HOOK_STDERR_TAIL_BYTES: u32 = 32 * 1024;

/// Shortest accepted `[waits]` value.
///
/// Turn events are counted only after the attention debounce (5 s by
/// default), so a shorter wait would time out before a turn that ends during
/// it could count. A smaller value is most likely seconds written as
/// milliseconds.
pub const MIN_TURN_WAIT: Duration = Duration::from_secs(5);

/// Longest accepted `[waits]` value.
///
/// One hour is twice the default ceiling, room for an operator whose agents
/// run longer turns, while a waiter slot of a client that vanished without
/// closing its connection is still released within the hour. It is not
/// [`MAX_DEADLINE`]: turn waits are not native job timeouts.
pub const MAX_TURN_WAIT: Duration = Duration::from_hours(1);

/// File name of the configuration inside the application config directory.
pub const FILE_NAME: &str = "service.toml";

/// Largest accepted configuration file in bytes.
///
/// A complete file with a 256-entry allowlist stays below 8 KiB; the bound
/// only keeps a corrupted or hostile file from being read into memory whole.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Longest accepted deadline or grace period.
///
/// Equal to the supervisor's job timeout ceiling because the exit timeouts
/// and restart throttle flow into native job definitions that reject longer
/// values; the other deadlines share it since anything longer would only hide
/// a wedged operation from the operator.
pub const MAX_DEADLINE: Duration = MAX_JOB_TIMEOUT;

/// Longest silent-reader wait for clients that omit the extended-wait opt-in.
///
/// A five-second client response deadline needs a short fallback so a silent
/// reader does not consume the entire budget before input is delivered.
pub const COMPAT_INITIAL_INPUT_STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Default PTY stop grace of daemon and session workers.
///
/// The grace is how long a worker waits for the PTY to reach EOF after it
/// signals the session's process group with `SIGTERM`, and then once more after
/// `SIGKILL`, before it force-closes the PTY; only a root still alive then is
/// awaited longer, up to [`DEFAULT_KILL_REAP_DEADLINE`]. 500 ms is long enough for
/// a shell or an agent CLI on a loaded host to handle `SIGTERM` and flush its
/// final output, and short enough that an explicit stop returns in about a
/// second. Raising it delays every stop of a stuck process; lowering it risks a
/// final screen cut short by the force-close.
///
/// This is not a `service.toml` key. The daemon is the single authority: it
/// sends its grace to each worker in the `Initialize` stop policy (the wire
/// value is a non-zero number of milliseconds), and a worker only falls back to
/// this value before it has been initialized. Both sides take their default from
/// this constant, so they cannot drift apart, and tests that need another value
/// set the `stop_grace` field of the registry or worker configuration. The value
/// stays below [`MAX_DEADLINE`].
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_millis(500);

/// Default deadline of daemon and session workers for the death of a stopped
/// session's root process after `SIGKILL`.
///
/// `SIGKILL` cannot be caught or ignored, so a root that has not exited one
/// [`DEFAULT_STOP_GRACE`] after it is only held by the kernel: by uninterruptible
/// I/O, by tearing down a large address space, or by a host too loaded to
/// schedule the exit. A real agent CLI at a prompt can need more than one grace
/// for that on a busy CI runner. Three seconds covers such a host many times
/// over; a root still alive after it is stuck in the kernel, and the stop
/// reports it with the state it is in. Together with the grace before
/// `SIGKILL` that report arrives well inside the daemon's bound on a worker's
/// stop answer.
///
/// Like the grace, this is not a `service.toml` key: the daemon sends it to
/// each worker in the `Initialize` stop policy, and a worker falls back to this
/// value when the policy does not carry it. It is never shorter than
/// [`DEFAULT_STOP_GRACE`] and stays below [`MAX_DEADLINE`].
pub const DEFAULT_KILL_REAP_DEADLINE: Duration = Duration::from_secs(3);

/// Default wait of a session worker for the durable commit of a stopped
/// session's terminal outcome before it answers a stop with a retryable
/// refusal.
///
/// The daemon bounds its own wait for the worker's stop answer by this value
/// ([`DEFAULT_WORKER_STOP_HEADROOM`] on top of the worker's grace, kill reap
/// and sweep waits), so the two sides share one constant. Like the grace, it is
/// not a `service.toml` key.
pub const DEFAULT_TERMINAL_COMMIT_WAIT: Duration = Duration::from_secs(10);

/// Margin the daemon adds to the worker's own stop bounds before it gives up on
/// a worker that has not answered a stop.
///
/// It covers the control-connection round trip and scheduling on a loaded
/// host; a worker that is still silent after every bound of its own plus this
/// margin is frozen, not slow.
pub const DEFAULT_WORKER_STOP_HEADROOM: Duration = Duration::from_secs(2);

/// Longest accepted path in bytes.
///
/// Equal to the supervisor's job value limit, because the prefix and roots
/// become executable paths, arguments, and bootstrap environment values.
pub const MAX_PATH_BYTES: usize = MAX_JOB_VALUE_BYTES;

/// Most entries the environment allowlist may hold.
///
/// Matches the worker protocol's bound on base-environment entries, so an
/// allowlist can never admit more variables than one `Initialize` carries.
pub const MAX_ALLOWLIST_ENTRIES: usize = 256;

/// Longest accepted allowlist pattern in bytes, including a trailing `*`.
///
/// Matches the worker protocol's bound on base-environment key length.
pub const MAX_PATTERN_BYTES: usize = 128;

/// Longest accepted joined `environment.search_path` in bytes.
///
/// Equal to the supervisor's job value limit, because the list becomes the
/// daemon job's `PATH` environment value.
pub const MAX_SEARCH_PATH_BYTES: usize = pohunek_platform::shell_env::MAX_SEARCH_PATH_BYTES;

/// Owner-only mode required of the configuration file.
const FILE_MODE: u32 = 0o600;

/// Mode of a configuration directory that [`ServiceConfig::write`] creates.
const DIRECTORY_MODE: u32 = 0o700;

/// Directory mode bits that would let another account replace the file.
///
/// Read and search bits for others are allowed: the `0600` file alone keeps
/// its contents private, while the absence of these bits keeps it intact.
const FORBIDDEN_DIRECTORY_BITS: u32 = 0o022;

/// Reserved `(uid_t)-1`, which `setuid`-family calls treat as "unchanged".
const INVALID_UID: u32 = u32::MAX;

/// Nanoseconds in one millisecond, the resolution of every recorded duration.
const NANOS_PER_MILLI: u32 = 1_000_000;

/// Header written above the generated TOML.
const FILE_HEADER: &str =
    "# Pohunek service configuration, written by `pohunek service install`.\n\
# Every key is required; unknown keys are rejected.\n\n";

/// Per-process sequence making temporary file names unique between writes.
///
/// It only disambiguates names: the temporary file is created exclusively, so
/// a collision fails the write instead of corrupting another writer's file.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Returns `<config>/pohunek/service.toml` for resolved base paths.
#[must_use]
pub fn file_path(paths: &BasePaths) -> PathBuf {
    paths.config_dir.join(FILE_NAME)
}

/// Deadlines recorded in `[deadlines]`, each a whole number of milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Deadlines {
    /// How long the daemon waits for a new worker generation's socket.
    pub worker_connect: Duration,
    /// How long the daemon waits for a worker to answer `Initialize`.
    pub worker_initialize: Duration,
    /// How long one `launchctl` invocation may run.
    pub launchctl_command: Duration,
    /// How long a worker job may take to exit after a stop request.
    pub worker_exit_timeout: Duration,
    /// How long the daemon job may take to exit after a stop request.
    pub daemon_exit_timeout: Duration,
    /// Minimum interval between native restarts of a failed daemon.
    pub daemon_restart_throttle: Duration,
}

/// Host input timings recorded in `[input]`, in whole milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InputTiming {
    /// Upper bound before initial input uses the startup fallback.
    pub initial_startup_grace: Duration,
    /// Delay between Claude's input body and separate submit write.
    pub submit_delay: Duration,
    /// Longest time one control input write may wait without the PTY accepting a byte.
    ///
    /// Restarted after every accepted byte, so it bounds a stalled reader, not
    /// the total duration of a long input. A reader that never drains its
    /// terminal would otherwise block the write, and every request queued
    /// behind it, indefinitely.
    pub write_stall_timeout: Duration,
}

/// Turn-wait timings recorded in `[waits]`, in whole milliseconds.
///
/// [`ServiceConfig::new`] keeps both within
/// [`MIN_TURN_WAIT`]`..=`[`MAX_TURN_WAIT`] and `turn_default` at most
/// `turn_max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaitTiming {
    /// Timeout of a `session.wait_turn` call that names none.
    pub turn_default: Duration,
    /// Longest timeout a `session.wait_turn` call may request.
    pub turn_max: Duration,
}

/// A validated service file read for an installer upgrade.
///
/// The source schema must be recorded with the upgrade transaction so a
/// rollback can restore the format understood by the previous daemon.
pub struct UpgradeSource {
    /// Current typed values, with the documented defaults assigned to the
    /// tables a previous schema lacks.
    pub config: ServiceConfig,
    /// Schema of the file before the upgrade.
    pub source_schema_version: u32,
    original_text: String,
}

impl std::fmt::Debug for UpgradeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeSource")
            .field("config", &self.config)
            .field("source_schema_version", &self.source_schema_version)
            .finish_non_exhaustive()
    }
}

impl UpgradeSource {
    /// Original validated bytes for binding a private backup to a transaction.
    ///
    /// The installer must not log these bytes and must verify their digest on
    /// resume before restoring a previous daemon's configuration.
    #[must_use]
    pub fn original_bytes(&self) -> &[u8] {
        self.original_text.as_bytes()
    }

    /// Atomically writes the original schema and values to a trusted path.
    ///
    /// Use this first to save the pre-upgrade file, then on rollback to
    /// restore it before starting the previous daemon.
    ///
    /// # Errors
    ///
    /// Returns the same path, trust, and write errors as [`ServiceConfig::write`].
    pub fn write_original(&self, path: &Path) -> Result<(), ConfigError> {
        write_config_text(path, &self.original_text)
    }
}

/// Unvalidated values for [`ServiceConfig::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSpec {
    /// Absolute installation prefix.
    pub prefix: PathBuf,
    /// Version directory name the daemon and new workers run from.
    pub active_version: String,
    /// User owning the installation.
    pub uid: u32,
    /// Canonical application state directory (`<XDG_STATE_HOME>/pohunek`).
    pub state_root: PathBuf,
    /// Canonical application runtime directory (`<runtime>/pohunek`).
    pub runtime_root: PathBuf,
    /// Supervision deadlines.
    pub deadlines: Deadlines,
    /// Environment names or trailing-`*` prefixes a worker may forward.
    pub environment_allowlist: Vec<String>,
    /// Executable search path handed to the daemon job; empty keeps the
    /// service manager's own `PATH`.
    pub search_path: SearchPath,
    /// Delay between SIGTERM and SIGKILL when sweeping orphaned processes.
    pub sweep_grace: Duration,
    /// Initial input readiness and Claude submit timings.
    pub input_timing: InputTiming,
    /// Default and ceiling of a turn wait.
    pub wait_timing: WaitTiming,
    /// Bytes of a failing worktree-create hook's stderr kept for its warning;
    /// 0 disables the capture.
    pub hook_stderr_tail_bytes: u32,
    /// Open-file limit applied to supervised jobs.
    pub open_files: u64,
}

/// A validated service configuration.
///
/// Every value satisfies the bounds documented on the crate constants, and
/// [`namespace`](Self::namespace) is derived from the recorded namespace inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    layout: InstallLayout,
    active_version: String,
    uid: u32,
    state_root: PathBuf,
    runtime_root: PathBuf,
    namespace: Namespace,
    deadlines: Deadlines,
    environment_allowlist: Vec<String>,
    search_path: SearchPath,
    sweep_grace: Duration,
    input_timing: InputTiming,
    wait_timing: WaitTiming,
    hook_stderr_tail_bytes: u32,
    open_files: u64,
}

impl ServiceConfig {
    /// Validates `spec` and derives its namespace.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidPath`] for a relative, unnormalized,
    /// non-UTF-8, or oversized path; [`ConfigError::InvalidVersion`],
    /// [`ConfigError::InvalidUid`], [`ConfigError::OutOfRange`],
    /// [`ConfigError::Precision`], [`ConfigError::AllowlistSize`],
    /// [`ConfigError::InvalidPattern`], [`ConfigError::DuplicatePattern`], or
    /// [`ConfigError::WaitDefaultAboveMax`] for the respective invalid value.
    pub fn new(spec: ConfigSpec) -> Result<Self, ConfigError> {
        let layout = validate_prefix(&spec.prefix)?;
        if valid_install_version(&spec.active_version).is_none() {
            return Err(ConfigError::InvalidVersion {
                length: spec.active_version.len(),
                max_bytes: MAX_INSTALL_VERSION_BYTES,
            });
        }
        if spec.uid == INVALID_UID {
            return Err(ConfigError::InvalidUid { uid: spec.uid });
        }
        validate_path("namespace.state_root", &spec.state_root)?;
        validate_path("namespace.runtime_root", &spec.runtime_root)?;
        let deadlines = spec.deadlines;
        validate_duration("deadlines.worker_connect_ms", deadlines.worker_connect)?;
        validate_duration(
            "deadlines.worker_initialize_ms",
            deadlines.worker_initialize,
        )?;
        validate_duration(
            "deadlines.launchctl_command_ms",
            deadlines.launchctl_command,
        )?;
        validate_duration(
            "deadlines.worker_exit_timeout_ms",
            deadlines.worker_exit_timeout,
        )?;
        validate_duration(
            "deadlines.daemon_exit_timeout_ms",
            deadlines.daemon_exit_timeout,
        )?;
        validate_duration(
            "deadlines.daemon_restart_throttle_ms",
            deadlines.daemon_restart_throttle,
        )?;
        validate_allowlist(&spec.environment_allowlist)?;
        validate_duration("sweep.grace_ms", spec.sweep_grace)?;
        validate_duration(
            "input.initial_startup_grace_ms",
            spec.input_timing.initial_startup_grace,
        )?;
        validate_duration("input.submit_delay_ms", spec.input_timing.submit_delay)?;
        validate_duration(
            "input.write_stall_timeout_ms",
            spec.input_timing.write_stall_timeout,
        )?;
        validate_wait_timing(spec.wait_timing)?;
        if spec.hook_stderr_tail_bytes > MAX_HOOK_STDERR_TAIL_BYTES {
            return Err(ConfigError::OutOfRange {
                key: "hooks.stderr_tail_bytes",
                value: u64::from(spec.hook_stderr_tail_bytes),
                min: 0,
                max: u64::from(MAX_HOOK_STDERR_TAIL_BYTES),
            });
        }
        if !(MIN_OPEN_FILES..=MAX_OPEN_FILES).contains(&spec.open_files) {
            return Err(ConfigError::OutOfRange {
                key: "limits.open_files",
                value: spec.open_files,
                min: MIN_OPEN_FILES,
                max: MAX_OPEN_FILES,
            });
        }
        let namespace = Namespace::derive(spec.uid, &spec.state_root, &spec.runtime_root);
        Ok(Self {
            layout,
            active_version: spec.active_version,
            uid: spec.uid,
            state_root: spec.state_root,
            runtime_root: spec.runtime_root,
            namespace,
            deadlines,
            environment_allowlist: spec.environment_allowlist,
            search_path: spec.search_path,
            sweep_grace: spec.sweep_grace,
            input_timing: spec.input_timing,
            wait_timing: spec.wait_timing,
            hook_stderr_tail_bytes: spec.hook_stderr_tail_bytes,
            open_files: spec.open_files,
        })
    }

    /// Loads and validates the configuration at an absolute `path`.
    ///
    /// The file is read through a trusted directory descriptor without
    /// following symlinks and parsed only after its type, owner, and mode are
    /// verified.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ConfigPath`] for a relative or unnormalized path,
    /// [`ConfigError::Untrusted`] for a symlink, special file, foreign owner,
    /// any file mode other than `0600`, or a group- or world-writable directory,
    /// [`ConfigError::TooLarge`] above [`MAX_CONFIG_BYTES`],
    /// [`ConfigError::Io`] when the file is missing or unreadable,
    /// [`ConfigError::Parse`] for invalid TOML, a missing, unknown, or mistyped
    /// key, [`ConfigError::UnsupportedSchema`] for another schema version, and
    /// every validation error of [`ServiceConfig::new`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        parse(path, &read_config_text(path)?)
    }

    /// Reads a current, schema-5, schema-4, schema-3, or schema-2 service file for an installer upgrade.
    ///
    /// Schema 2 has no `[input]` or `[waits]` table, schema 3 no `[waits]`
    /// and no `input.write_stall_timeout_ms`, schema 4 no
    /// `input.write_stall_timeout_ms`, and none of them nor schema 5 a
    /// `[hooks]` table. This method supplies the documented
    /// [`DEFAULT_INPUT_TIMING`], [`DEFAULT_WAIT_TIMING`] and
    /// [`DEFAULT_HOOK_STDERR_TAIL_BYTES`] explicitly for
    /// the missing keys and tables and keeps every recorded value; normal
    /// daemon and worker loads still require every key of the current schema. The caller must preserve the source
    /// schema across the transaction and restore it if the old daemon is
    /// restarted on rollback.
    ///
    /// # Errors
    ///
    /// Returns the same trust and validation errors as [`Self::load`], and
    /// [`ConfigError::UnsupportedSchema`] for schemas other than 2, 3, 4, 5, or
    /// [`SCHEMA_VERSION`].
    pub fn load_upgrade_source(path: &Path) -> Result<UpgradeSource, ConfigError> {
        Self::load_upgrade_source_bytes(path, read_config_text(path)?.as_bytes())
    }

    /// Parses already-read bytes as a service file for an installer upgrade.
    ///
    /// The bytes carry no trust on their own: the caller binds them first, for
    /// example to the digest a transaction record journals for the exact
    /// pre-upgrade file, and refuses a mismatch before using the result.
    /// `path` names the bytes in every error. `load_upgrade_source` is this
    /// function over a trusted read.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] for bytes that are not UTF-8, invalid
    /// TOML, or a missing, unknown, or mistyped key, and
    /// [`ConfigError::UnsupportedSchema`] for schemas other than 2, 3, 4, 5, or
    /// [`SCHEMA_VERSION`].
    pub fn load_upgrade_source_bytes(
        path: &Path,
        bytes: &[u8],
    ) -> Result<UpgradeSource, ConfigError> {
        let original_text =
            String::from_utf8(bytes.to_vec()).map_err(|error| ConfigError::Parse {
                path: path.to_path_buf(),
                message: format!("file is not UTF-8: {error}"),
            })?;
        let (config, source_schema_version) = parse_upgrade_source(path, &original_text)?;
        Ok(UpgradeSource {
            config,
            source_schema_version,
            original_text,
        })
    }

    /// Atomically writes this configuration to an absolute `path` with mode `0600`.
    ///
    /// A missing parent directory is created with mode `0700`. An existing one
    /// must be owned by the effective user and not group- or world-writable;
    /// its mode is never changed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ConfigPath`] for a relative or unnormalized path,
    /// [`ConfigError::Untrusted`] when the directory is foreign-owned or
    /// group- or world-writable,
    /// [`ConfigError::Io`] when it cannot be opened or created, and
    /// [`ConfigError::Write`] when the replacement fails.
    pub fn write(&self, path: &Path) -> Result<(), ConfigError> {
        write_config_text(path, &self.to_toml())
    }

    /// Renders the configuration as the TOML document [`load`](Self::load) accepts.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let raw = RawConfig {
            schema_version: SCHEMA_VERSION,
            prefix: utf8(self.layout.prefix()).to_owned(),
            active_version: self.active_version.clone(),
            namespace: RawNamespace {
                uid: self.uid,
                state_root: utf8(&self.state_root).to_owned(),
                runtime_root: utf8(&self.runtime_root).to_owned(),
            },
            deadlines: RawDeadlines {
                worker_connect_ms: millis(self.deadlines.worker_connect),
                worker_initialize_ms: millis(self.deadlines.worker_initialize),
                launchctl_command_ms: millis(self.deadlines.launchctl_command),
                worker_exit_timeout_ms: millis(self.deadlines.worker_exit_timeout),
                daemon_exit_timeout_ms: millis(self.deadlines.daemon_exit_timeout),
                daemon_restart_throttle_ms: millis(self.deadlines.daemon_restart_throttle),
            },
            environment: RawEnvironment {
                search_path: self
                    .search_path
                    .entries()
                    .iter()
                    .map(|entry| utf8(entry).to_owned())
                    .collect(),
                allowlist: self.environment_allowlist.clone(),
            },
            sweep: RawSweep {
                grace_ms: millis(self.sweep_grace),
            },
            input: RawInput {
                initial_startup_grace_ms: millis(self.input_timing.initial_startup_grace),
                submit_delay_ms: millis(self.input_timing.submit_delay),
                write_stall_timeout_ms: millis(self.input_timing.write_stall_timeout),
            },
            waits: RawWaits::from(self.wait_timing),
            hooks: RawHooks {
                stderr_tail_bytes: self.hook_stderr_tail_bytes,
            },
            limits: RawLimits {
                open_files: self.open_files,
            },
        };
        let body = toml::to_string(&raw)
            .expect("validated strings, bounded integers, and tables always serialize as TOML");
        format!("{FILE_HEADER}{body}")
    }

    /// Returns the values this configuration was built from.
    ///
    /// Useful to change one value, such as the active version on upgrade, and
    /// revalidate through [`ServiceConfig::new`].
    #[must_use]
    pub fn to_spec(&self) -> ConfigSpec {
        ConfigSpec {
            prefix: self.layout.prefix().to_path_buf(),
            active_version: self.active_version.clone(),
            uid: self.uid,
            state_root: self.state_root.clone(),
            runtime_root: self.runtime_root.clone(),
            deadlines: self.deadlines,
            environment_allowlist: self.environment_allowlist.clone(),
            search_path: self.search_path.clone(),
            sweep_grace: self.sweep_grace,
            input_timing: self.input_timing,
            wait_timing: self.wait_timing,
            hook_stderr_tail_bytes: self.hook_stderr_tail_bytes,
            open_files: self.open_files,
        }
    }

    /// Fails unless the running installation matches the recorded namespace inputs.
    ///
    /// `state_root` and `runtime_root` are the application state and runtime
    /// directories of the running process; both are canonicalized before the
    /// comparison, so they must exist. The daemon calls this at startup so a
    /// configuration written for another user or installation is never used to
    /// name, adopt, or retire jobs.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Canonicalize`] when a root cannot be resolved and
    /// [`ConfigError::NamespaceMismatch`] naming the first differing key.
    pub fn verify_installation(
        &self,
        uid: u32,
        state_root: &Path,
        runtime_root: &Path,
    ) -> Result<(), ConfigError> {
        if uid != self.uid {
            return Err(ConfigError::NamespaceMismatch {
                key: "namespace.uid",
                recorded: self.uid.to_string(),
                actual: uid.to_string(),
            });
        }
        verify_root("namespace.state_root", &self.state_root, state_root)?;
        verify_root("namespace.runtime_root", &self.runtime_root, runtime_root)
    }

    /// Returns the installation prefix.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        self.layout.prefix()
    }

    /// Returns the versioned executable layout below the prefix.
    #[must_use]
    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    /// Returns the active version directory name.
    #[must_use]
    pub fn active_version(&self) -> &str {
        &self.active_version
    }

    /// Returns `<prefix>/libexec/pohunek/<active_version>`.
    #[must_use]
    pub fn version_dir(&self) -> PathBuf {
        self.layout
            .version_dir(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the daemon executable of the active version.
    #[must_use]
    pub fn daemon_executable(&self) -> PathBuf {
        self.layout
            .daemon_executable(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the session-worker executable of the active version.
    #[must_use]
    pub fn worker_executable(&self) -> PathBuf {
        self.layout
            .worker_executable(&self.active_version)
            .expect("active_version was validated by ServiceConfig::new")
    }

    /// Returns the recorded user id.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Returns the recorded canonical application state directory.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Returns the recorded canonical application runtime directory.
    #[must_use]
    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    /// Returns the job namespace derived from the recorded namespace inputs.
    #[must_use]
    pub fn namespace(&self) -> Namespace {
        self.namespace.clone()
    }

    /// Returns the supervision deadlines.
    #[must_use]
    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    /// Returns the environment allowlist patterns in file order.
    #[must_use]
    pub fn environment_allowlist(&self) -> &[String] {
        &self.environment_allowlist
    }

    /// Returns the executable search path recorded for the daemon job.
    ///
    /// Empty when the job keeps the service manager's own `PATH`.
    #[must_use]
    pub fn search_path(&self) -> &SearchPath {
        &self.search_path
    }

    /// Returns the orphan-sweep grace period between SIGTERM and SIGKILL.
    #[must_use]
    pub fn sweep_grace(&self) -> Duration {
        self.sweep_grace
    }

    /// Returns the host input timings.
    #[must_use]
    pub fn input_timing(&self) -> InputTiming {
        self.input_timing
    }

    /// Returns the default and ceiling of a turn wait.
    #[must_use]
    pub fn wait_timing(&self) -> WaitTiming {
        self.wait_timing
    }

    /// Returns how many bytes of a failing worktree-create hook's stderr the
    /// daemon keeps for its warning; 0 disables the capture.
    #[must_use]
    pub fn hook_stderr_tail_bytes(&self) -> u32 {
        self.hook_stderr_tail_bytes
    }

    /// Returns the open-file limit for supervised jobs.
    #[must_use]
    pub fn open_files(&self) -> u64 {
        self.open_files
    }
}

/// Reads a trusted service file while keeping parsing policy at the caller.
fn read_config_text(path: &Path) -> Result<String, ConfigError> {
    let (parent, name) = split_config_path(path)?;
    let directory = TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS)
        .map_err(|source| classify_read_error(path, source))?;
    let bytes = directory
        .read_file(name, FILE_MODE, MAX_CONFIG_BYTES)
        .map_err(|source| classify_read_error(path, source))?;
    String::from_utf8(bytes).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: format!("file is not UTF-8: {error}"),
    })
}

/// Replaces a trusted service configuration or transaction backup atomically.
fn write_config_text(path: &Path, text: &str) -> Result<(), ConfigError> {
    let (parent, name) = split_config_path(path)?;
    let directory =
        open_or_create_directory(parent).map_err(|source| classify_read_error(path, source))?;
    let temporary = format!(
        ".{FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    directory
        .replace_file(name, temporary, text.as_bytes(), FILE_MODE)
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// On-disk form of the whole file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInput,
    waits: RawWaits,
    hooks: RawHooks,
    limits: RawLimits,
}

/// Exact on-disk schema-5 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV5 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInput,
    waits: RawWaits,
    limits: RawLimits,
}

/// Exact on-disk schema-3 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV3 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInputWithoutStall,
    limits: RawLimits,
}

/// Exact on-disk schema-4 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV4 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    input: RawInputWithoutStall,
    waits: RawWaits,
    limits: RawLimits,
}

/// Exact on-disk schema-2 shape accepted only for an explicit upgrade.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfigV2 {
    schema_version: u32,
    prefix: String,
    active_version: String,
    namespace: RawNamespace,
    deadlines: RawDeadlines,
    environment: RawEnvironment,
    sweep: RawSweep,
    limits: RawLimits,
}

/// On-disk `[namespace]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNamespace {
    uid: u32,
    state_root: String,
    runtime_root: String,
}

/// On-disk `[deadlines]` table in milliseconds.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_field_names,
    reason = "the `_ms` suffix is the documented on-disk unit of every key"
)]
struct RawDeadlines {
    worker_connect_ms: u64,
    worker_initialize_ms: u64,
    launchctl_command_ms: u64,
    worker_exit_timeout_ms: u64,
    daemon_exit_timeout_ms: u64,
    daemon_restart_throttle_ms: u64,
}

/// On-disk `[environment]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvironment {
    search_path: Vec<String>,
    allowlist: Vec<String>,
}

/// On-disk `[sweep]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSweep {
    grace_ms: u64,
}

/// On-disk `[input]` table in milliseconds.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_field_names,
    reason = "the `_ms` suffix is the documented on-disk unit of every key"
)]
struct RawInput {
    initial_startup_grace_ms: u64,
    submit_delay_ms: u64,
    write_stall_timeout_ms: u64,
}

/// On-disk `[input]` table of schemas 3 and 4, which has no write stall bound.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInputWithoutStall {
    initial_startup_grace_ms: u64,
    submit_delay_ms: u64,
}

/// On-disk `[waits]` table in milliseconds.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWaits {
    turn_default_ms: u64,
    turn_max_ms: u64,
}

impl From<WaitTiming> for RawWaits {
    fn from(timing: WaitTiming) -> Self {
        Self {
            turn_default_ms: millis(timing.turn_default),
            turn_max_ms: millis(timing.turn_max),
        }
    }
}

/// On-disk `[hooks]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHooks {
    stderr_tail_bytes: u32,
}

impl RawHooks {
    /// The documented value assigned to a previous schema, which has no `[hooks]`.
    const DEFAULT: Self = Self {
        stderr_tail_bytes: DEFAULT_HOOK_STDERR_TAIL_BYTES,
    };
}

/// On-disk `[limits]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    open_files: u64,
}

/// Parses and validates the text of a configuration read from `path`.
fn parse(path: &Path, text: &str) -> Result<ServiceConfig, ConfigError> {
    let parse_error = |error: toml::de::Error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: diagnostic(text, &error),
    };
    // The schema version is checked before the typed parse, so a newer file
    // reports its version instead of its first key this schema does not know.
    let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
    if let Some(toml::Value::Integer(found)) = table.get("schema_version") {
        if *found != i64::from(SCHEMA_VERSION) {
            return Err(ConfigError::UnsupportedSchema { found: *found });
        }
    }
    let raw: RawConfig = toml::from_str(text).map_err(parse_error)?;
    if raw.schema_version != SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchema {
            found: i64::from(raw.schema_version),
        });
    }
    config_from_raw(raw)
}

/// Parses the current or an exact previous schema at the installer upgrade boundary.
///
/// A previous schema gets the documented defaults for the tables it lacks
/// and keeps every value it records.
fn parse_upgrade_source(path: &Path, text: &str) -> Result<(ServiceConfig, u32), ConfigError> {
    let parse_error = |error: toml::de::Error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: diagnostic(text, &error),
    };
    let table: toml::Table = toml::from_str(text).map_err(parse_error)?;
    match table.get("schema_version") {
        Some(toml::Value::Integer(2)) => {
            let previous: RawConfigV2 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: millis(DEFAULT_INPUT_TIMING.initial_startup_grace),
                    submit_delay_ms: millis(DEFAULT_INPUT_TIMING.submit_delay),
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: RawWaits::from(DEFAULT_WAIT_TIMING),
                hooks: RawHooks::DEFAULT,
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 2);
            Ok((config_from_raw(raw)?, 2))
        }
        Some(toml::Value::Integer(4)) => {
            let previous: RawConfigV4 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: previous.input.initial_startup_grace_ms,
                    submit_delay_ms: previous.input.submit_delay_ms,
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: previous.waits,
                hooks: RawHooks::DEFAULT,
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 4);
            Ok((config_from_raw(raw)?, 4))
        }
        Some(toml::Value::Integer(5)) => {
            let previous: RawConfigV5 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: previous.input,
                waits: previous.waits,
                hooks: RawHooks::DEFAULT,
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 5);
            Ok((config_from_raw(raw)?, 5))
        }
        Some(toml::Value::Integer(3)) => {
            let previous: RawConfigV3 = toml::from_str(text).map_err(parse_error)?;
            let raw = RawConfig {
                schema_version: SCHEMA_VERSION,
                prefix: previous.prefix,
                active_version: previous.active_version,
                namespace: previous.namespace,
                deadlines: previous.deadlines,
                environment: previous.environment,
                sweep: previous.sweep,
                input: RawInput {
                    initial_startup_grace_ms: previous.input.initial_startup_grace_ms,
                    submit_delay_ms: previous.input.submit_delay_ms,
                    write_stall_timeout_ms: millis(DEFAULT_INPUT_TIMING.write_stall_timeout),
                },
                waits: RawWaits::from(DEFAULT_WAIT_TIMING),
                hooks: RawHooks::DEFAULT,
                limits: previous.limits,
            };
            debug_assert_eq!(previous.schema_version, 3);
            Ok((config_from_raw(raw)?, 3))
        }
        _ => Ok((parse(path, text)?, SCHEMA_VERSION)),
    }
}

/// Converts the parsed current shape into validated typed values.
fn config_from_raw(raw: RawConfig) -> Result<ServiceConfig, ConfigError> {
    let search_path = SearchPath::new(
        raw.environment
            .search_path
            .into_iter()
            .map(PathBuf::from)
            .collect(),
    )
    .map_err(|source| ConfigError::InvalidSearchPath {
        detail: source.to_string(),
    })?;
    ServiceConfig::new(ConfigSpec {
        prefix: PathBuf::from(raw.prefix),
        active_version: raw.active_version,
        uid: raw.namespace.uid,
        state_root: PathBuf::from(raw.namespace.state_root),
        runtime_root: PathBuf::from(raw.namespace.runtime_root),
        deadlines: Deadlines {
            worker_connect: Duration::from_millis(raw.deadlines.worker_connect_ms),
            worker_initialize: Duration::from_millis(raw.deadlines.worker_initialize_ms),
            launchctl_command: Duration::from_millis(raw.deadlines.launchctl_command_ms),
            worker_exit_timeout: Duration::from_millis(raw.deadlines.worker_exit_timeout_ms),
            daemon_exit_timeout: Duration::from_millis(raw.deadlines.daemon_exit_timeout_ms),
            daemon_restart_throttle: Duration::from_millis(
                raw.deadlines.daemon_restart_throttle_ms,
            ),
        },
        environment_allowlist: raw.environment.allowlist,
        search_path,
        sweep_grace: Duration::from_millis(raw.sweep.grace_ms),
        input_timing: InputTiming {
            initial_startup_grace: Duration::from_millis(raw.input.initial_startup_grace_ms),
            submit_delay: Duration::from_millis(raw.input.submit_delay_ms),
            write_stall_timeout: Duration::from_millis(raw.input.write_stall_timeout_ms),
        },
        wait_timing: WaitTiming {
            turn_default: Duration::from_millis(raw.waits.turn_default_ms),
            turn_max: Duration::from_millis(raw.waits.turn_max_ms),
        },
        hook_stderr_tail_bytes: raw.hooks.stderr_tail_bytes,
        open_files: raw.limits.open_files,
    })
}

/// Sanitized diagnostic for one TOML parse failure over `text`.
///
/// The parser's `Display` renders the source line the error span points at,
/// which would print arbitrary file contents — including a hand-edited line
/// such as an unknown `token = "…"` key — into sticky destinations: journald,
/// the CLI error envelope, and operator logs. Only the parser's message text
/// and a byte-offset-derived position survive, never source text: argument
/// values quoted in parser messages are replaced by a fixed placeholder, and
/// the line and column are computed from the error's span.
fn diagnostic(text: &str, error: &toml::de::Error) -> String {
    let mut message = redact_quoted_values(error.message());
    if message.is_empty() {
        message.push_str("TOML parse error");
    }
    if let Some(span) = error.span() {
        let (line, column) = position(text, span.start);
        std::fmt::Write::write_fmt(
            &mut message,
            format_args!(" (line {line}, column {column})"),
        )
        .expect("writing to String is infallible");
    }
    message
}

/// Replaces the content of every `"`-delimited run with a fixed placeholder.
///
/// Parser messages double-quote argument values only; schema identifiers,
/// key names, and error descriptors use backticks or plain words. A `\`
/// escapes exactly one following character, so an escaped `\"` never closes
/// a run that its `Debug`-escaped token did open. An unterminated run also
/// renders as the placeholder: replaying that tail could print file text.
fn redact_quoted_values(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut inside_value = false;
    let mut escaped = false;
    for character in message.chars() {
        if inside_value {
            if escaped {
                escaped = false;
            } else if character == '"' {
                inside_value = false;
                redacted.push('"');
                redacted.push_str(REDACTED_PLACEHOLDER);
                redacted.push('"');
            } else if character == '\\' {
                escaped = true;
            }
            continue;
        }
        if character == '"' {
            inside_value = true;
        } else {
            redacted.push(character);
        }
    }
    if inside_value {
        // Fail closed: what followed the open quote may be file text.
        redacted.push('"');
        redacted.push_str(REDACTED_PLACEHOLDER);
        redacted.push('"');
    }
    redacted
}

/// Placeholder shown for parser-quoted file contents.
const REDACTED_PLACEHOLDER: &str = "<redacted>";

/// Returns the 1-based line and character column of a byte offset in `text`.
///
/// A non-character-boundary offset backs up to the nearest boundary, so an
/// offset into a multi-byte character reports the line the character starts on.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut line = 1;
    let mut column = 1;
    for (index, character) in text.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Splits an absolute normalized configuration path into directory and name.
fn split_config_path(path: &Path) -> Result<(&Path, &OsStr), ConfigError> {
    let invalid = || ConfigError::ConfigPath {
        path: path.to_path_buf(),
    };
    if !is_normalized_absolute(path) {
        return Err(invalid());
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, name)),
        _ => Err(invalid()),
    }
}

/// Opens an existing owner-safe directory, or creates a missing one as `0700`.
///
/// Creation also validates the result, so a directory another process
/// creates in between with a different mode is rejected, not adopted.
fn open_or_create_directory(parent: &Path) -> Result<TrustedDir, FsError> {
    match TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS) {
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            TrustedDir::open_or_create_absolute(parent, DIRECTORY_MODE)
        }
        result => result,
    }
}

/// Maps a trusted-filesystem failure to the matching configuration error.
fn classify_read_error(path: &Path, source: FsError) -> ConfigError {
    let path = path.to_path_buf();
    match source {
        FsError::FileTooLarge { max_bytes, .. } => ConfigError::TooLarge { path, max_bytes },
        FsError::UnsafeType { .. }
        | FsError::UnsafeOwner { .. }
        | FsError::UnsafeMode { .. }
        | FsError::UnsafeAcl { .. }
        | FsError::UnsafeLinkCount { .. } => ConfigError::Untrusted { path, source },
        source => ConfigError::Io { path, source },
    }
}

/// Validates an installation prefix exactly as [`ServiceConfig::new`] does.
///
/// Callers that act on a prefix before building the configuration check it
/// here first, so a prefix the configuration would reject is refused before
/// any of their effects.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidPath`] with key `prefix` for a relative,
/// unnormalized, non-UTF-8, or oversized path.
pub fn validate_prefix(prefix: &Path) -> Result<InstallLayout, ConfigError> {
    validate_path("prefix", prefix)?;
    InstallLayout::new(prefix).map_err(|_invalid_prefix| ConfigError::InvalidPath {
        key: "prefix",
        length: prefix.as_os_str().len(),
        reason: "is not a usable installation prefix",
        max_bytes: MAX_PATH_BYTES,
    })
}

/// Validates one recorded path key.
fn validate_path(key: &'static str, path: &Path) -> Result<(), ConfigError> {
    match path_error(path) {
        None => Ok(()),
        Some(reason) => Err(ConfigError::InvalidPath {
            key,
            length: path.as_os_str().len(),
            reason,
            max_bytes: MAX_PATH_BYTES,
        }),
    }
}

/// Explains why `path` is not an absolute normalized UTF-8 path within [`MAX_PATH_BYTES`].
fn path_error(path: &Path) -> Option<&'static str> {
    if path.as_os_str().len() > MAX_PATH_BYTES {
        Some("is too long")
    } else if path.to_str().is_none() {
        Some("is not UTF-8")
    } else if !path.is_absolute() {
        Some("is not absolute")
    } else if path.as_os_str().as_encoded_bytes().contains(&0) {
        Some("contains a NUL byte")
    } else if !is_normalized_absolute(path) {
        Some("is not normalized (`.`, `..`, or a repeated or trailing `/`)")
    } else {
        None
    }
}

/// Validates one recorded duration key.
fn validate_duration(key: &'static str, value: Duration) -> Result<(), ConfigError> {
    if !value.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
        return Err(ConfigError::Precision { key });
    }
    if value.is_zero() || value > MAX_DEADLINE {
        return Err(ConfigError::OutOfRange {
            key,
            value: u64::try_from(value.as_millis()).unwrap_or(u64::MAX),
            min: 1,
            max: millis(MAX_DEADLINE),
        });
    }
    Ok(())
}

/// Validates each `[waits]` bound and that the default stays within the ceiling.
fn validate_wait_timing(timing: WaitTiming) -> Result<(), ConfigError> {
    for (key, value) in [
        ("waits.turn_default_ms", timing.turn_default),
        ("waits.turn_max_ms", timing.turn_max),
    ] {
        if !value.subsec_nanos().is_multiple_of(NANOS_PER_MILLI) {
            return Err(ConfigError::Precision { key });
        }
        if !(MIN_TURN_WAIT..=MAX_TURN_WAIT).contains(&value) {
            return Err(ConfigError::OutOfRange {
                key,
                value: u64::try_from(value.as_millis()).unwrap_or(u64::MAX),
                min: millis(MIN_TURN_WAIT),
                max: millis(MAX_TURN_WAIT),
            });
        }
    }
    if timing.turn_default > timing.turn_max {
        return Err(ConfigError::WaitDefaultAboveMax {
            default_ms: millis(timing.turn_default),
            max_ms: millis(timing.turn_max),
        });
    }
    Ok(())
}

/// Validates the environment allowlist syntax, size, and uniqueness.
fn validate_allowlist(patterns: &[String]) -> Result<(), ConfigError> {
    if patterns.is_empty() || patterns.len() > MAX_ALLOWLIST_ENTRIES {
        return Err(ConfigError::AllowlistSize {
            count: patterns.len(),
            max: MAX_ALLOWLIST_ENTRIES,
        });
    }
    let mut seen = BTreeMap::new();
    for (index, pattern) in patterns.iter().enumerate() {
        if let Some(reason) = pattern_error(pattern) {
            return Err(ConfigError::InvalidPattern {
                index,
                length: pattern.len(),
                reason,
            });
        }
        if let Some(&first) = seen.get(pattern.as_str()) {
            return Err(ConfigError::DuplicatePattern { index, first });
        }
        seen.insert(pattern.as_str(), index);
    }
    Ok(())
}

/// Explains why `pattern` is not `[A-Za-z_][A-Za-z0-9_]*` with an optional trailing `*`.
fn pattern_error(pattern: &str) -> Option<&'static str> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Some("longer than 128 bytes");
    }
    let name = pattern.strip_suffix('*').unwrap_or(pattern);
    let mut bytes = name.bytes();
    match bytes.next() {
        None => Some("a pattern needs at least one name character before any `*`"),
        Some(first) if !(first.is_ascii_alphabetic() || first == b'_') => {
            Some("a name must start with an ASCII letter or `_`")
        }
        Some(_) if !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') => Some(
            "a name may contain only ASCII letters, digits, and `_`, with `*` only as the last character",
        ),
        Some(_) => None,
    }
}

/// Compares a recorded root with the canonical form of the running root.
fn verify_root(key: &'static str, recorded: &Path, actual: &Path) -> Result<(), ConfigError> {
    let canonical = std::fs::canonicalize(actual).map_err(|source| ConfigError::Canonicalize {
        key,
        path: actual.to_path_buf(),
        source,
    })?;
    if canonical == recorded {
        Ok(())
    } else {
        Err(ConfigError::NamespaceMismatch {
            key,
            recorded: recorded.display().to_string(),
            actual: canonical.display().to_string(),
        })
    }
}

/// Returns a validated duration in whole milliseconds.
///
/// # Panics
///
/// Panics above `u64::MAX` milliseconds, which no validated duration reaches:
/// the longest is [`MAX_TURN_WAIT`].
fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).expect("validated durations are at most MAX_TURN_WAIT")
}

/// Returns a validated path as UTF-8.
fn utf8(path: &Path) -> &str {
    path.to_str()
        .expect("recorded paths were validated as UTF-8 by ServiceConfig::new")
}
