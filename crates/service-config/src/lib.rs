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
//! schema_version = 1
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
//! [environment]                                    # names or trailing-`*` prefixes
//! allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]
//!
//! [sweep]
//! grace_ms = 5000                                  # 1..=600000
//!
//! [limits]
//! open_files = 8192                                # 256..=1048576
//! ```
//!
//! This crate validates allowlist patterns only syntactically; which names
//! the installer allowlists by default is decided by its caller.
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

// Rust guideline compliant 2026-09-24

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pohunek_paths::{valid_install_version, BasePaths, InstallLayout};
use pohunek_platform::filesystem::{FsError, TrustedDir};
use pohunek_platform::supervisor::{
    Namespace, MAX_JOB_TIMEOUT, MAX_JOB_VALUE_BYTES, MAX_OPEN_FILES, MIN_OPEN_FILES,
};
use serde::{Deserialize, Serialize};

mod error;

#[doc(inline)]
pub use error::ConfigError;

/// The only schema version this crate reads and writes.
///
/// A file with any other value is rejected before its keys are interpreted,
/// so an older binary never half-understands a newer installation.
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Delay between SIGTERM and SIGKILL when sweeping orphaned processes.
    pub sweep_grace: Duration,
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
    sweep_grace: Duration,
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
    /// [`ConfigError::InvalidPattern`], or [`ConfigError::DuplicatePattern`]
    /// for the respective invalid value.
    pub fn new(spec: ConfigSpec) -> Result<Self, ConfigError> {
        validate_path("prefix", &spec.prefix)?;
        let layout = InstallLayout::new(spec.prefix.clone()).map_err(|_invalid_prefix| {
            ConfigError::InvalidPath {
                key: "prefix",
                path: spec.prefix.clone(),
                max_bytes: MAX_PATH_BYTES,
            }
        })?;
        if valid_install_version(&spec.active_version).is_none() {
            return Err(ConfigError::InvalidVersion {
                value: spec.active_version,
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
            sweep_grace: spec.sweep_grace,
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
        let (parent, name) = split_config_path(path)?;
        let directory = TrustedDir::open_absolute_owner_safe(parent, FORBIDDEN_DIRECTORY_BITS)
            .map_err(|source| classify_read_error(path, source))?;
        let bytes = directory
            .read_file(name, FILE_MODE, MAX_CONFIG_BYTES)
            .map_err(|source| classify_read_error(path, source))?;
        let text = std::str::from_utf8(&bytes).map_err(|error| ConfigError::Parse {
            path: path.to_path_buf(),
            message: format!("file is not UTF-8: {error}"),
        })?;
        parse(path, text)
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
        let (parent, name) = split_config_path(path)?;
        let directory =
            open_or_create_directory(parent).map_err(|source| classify_read_error(path, source))?;
        let temporary = format!(
            ".{FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        directory
            .replace_file(name, temporary, self.to_toml().as_bytes(), FILE_MODE)
            .map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })
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
                allowlist: self.environment_allowlist.clone(),
            },
            sweep: RawSweep {
                grace_ms: millis(self.sweep_grace),
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
            sweep_grace: self.sweep_grace,
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

    /// Returns the orphan-sweep grace period between SIGTERM and SIGKILL.
    #[must_use]
    pub fn sweep_grace(&self) -> Duration {
        self.sweep_grace
    }

    /// Returns the open-file limit for supervised jobs.
    #[must_use]
    pub fn open_files(&self) -> u64 {
        self.open_files
    }
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
    allowlist: Vec<String>,
}

/// On-disk `[sweep]` table.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSweep {
    grace_ms: u64,
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
        message: error.to_string(),
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
        sweep_grace: Duration::from_millis(raw.sweep.grace_ms),
        open_files: raw.limits.open_files,
    })
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

/// Returns whether `path` is absolute and spelled exactly as its components.
///
/// Rejects `..`, `.`, repeated or trailing separators, and NUL bytes.
fn is_normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

/// Validates one recorded path key.
fn validate_path(key: &'static str, path: &Path) -> Result<(), ConfigError> {
    if is_normalized_absolute(path)
        && path.to_str().is_some()
        && path.as_os_str().len() <= MAX_PATH_BYTES
    {
        Ok(())
    } else {
        Err(ConfigError::InvalidPath {
            key,
            path: path.to_path_buf(),
            max_bytes: MAX_PATH_BYTES,
        })
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

/// Validates the environment allowlist syntax, size, and uniqueness.
fn validate_allowlist(patterns: &[String]) -> Result<(), ConfigError> {
    if patterns.is_empty() || patterns.len() > MAX_ALLOWLIST_ENTRIES {
        return Err(ConfigError::AllowlistSize {
            count: patterns.len(),
            max: MAX_ALLOWLIST_ENTRIES,
        });
    }
    let mut seen = BTreeSet::new();
    for pattern in patterns {
        if let Some(reason) = pattern_error(pattern) {
            return Err(ConfigError::InvalidPattern {
                pattern: pattern.clone(),
                reason,
            });
        }
        if !seen.insert(pattern.as_str()) {
            return Err(ConfigError::DuplicatePattern {
                pattern: pattern.clone(),
            });
        }
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
fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).expect("validated durations are at most MAX_DEADLINE")
}

/// Returns a validated path as UTF-8.
fn utf8(path: &Path) -> &str {
    path.to_str()
        .expect("recorded paths were validated as UTF-8 by ServiceConfig::new")
}
