//! Failures while loading, validating, writing, or verifying `service.toml`.

use std::io;
use std::path::PathBuf;

use pohunek_platform::filesystem::{AtomicReplaceError, FsError};

use crate::SCHEMA_VERSION;

// Rust guideline compliant 2026-09-24

/// A failure while handling the service configuration.
///
/// Validation variants name the TOML key (`section.key`) they reject so the
/// operator can fix the file without reading source code.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The configuration file path is relative, unnormalized, or has no file name.
    #[error("service config path must be an absolute normalized file path: {}", path.display())]
    ConfigPath {
        /// The rejected path.
        path: PathBuf,
    },
    /// The file or its directory is not owner-private, not owned by the
    /// effective user, a symlink, or not a regular file.
    #[error("service config {} is not a trusted owner-private file: {source}", path.display())]
    Untrusted {
        /// The configuration file path.
        path: PathBuf,
        /// The trusted-filesystem check that failed.
        #[source]
        source: FsError,
    },
    /// The file exceeds [`MAX_CONFIG_BYTES`](crate::MAX_CONFIG_BYTES).
    #[error("service config {} exceeds the {max_bytes}-byte limit", path.display())]
    TooLarge {
        /// The configuration file path.
        path: PathBuf,
        /// The accepted maximum.
        max_bytes: usize,
    },
    /// The file or its directory could not be opened or read.
    #[error("failed to read service config {}: {source}", path.display())]
    Io {
        /// The configuration file path.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: FsError,
    },
    /// The file could not be atomically replaced.
    ///
    /// [`AtomicReplaceError::CommittedDurabilityUncertain`] means the new
    /// contents are already in place and only the directory sync failed.
    #[error("failed to write service config {}: {source}", path.display())]
    Write {
        /// The configuration file path.
        path: PathBuf,
        /// The replacement failure.
        #[source]
        source: AtomicReplaceError,
    },
    /// The file is not UTF-8 TOML matching the schema.
    ///
    /// `message` is a sanitized parser diagnostic: the parser's message text
    /// with any argument values it quoted replaced by a fixed placeholder,
    /// plus the error position as a line and column. It never quotes source
    /// text, so a hand-edited file cannot print its contents — such as an
    /// unknown `token = "…"` key's value — into journald or an error envelope.
    ///
    /// The schema holds identifiers, paths, versions, and numbers, so naming
    /// the rejected key remains safe.
    #[error("service config {} is invalid: {message}", path.display())]
    Parse {
        /// The configuration file path.
        path: PathBuf,
        /// The parser diagnostic, naming the missing, unknown, or mistyped key.
        message: String,
    },
    /// `schema_version` is not [`SCHEMA_VERSION`].
    #[error("service config schema_version {found} is unsupported; expected {SCHEMA_VERSION}")]
    UnsupportedSchema {
        /// The recorded schema version.
        found: i64,
    },
    /// A path key is not an absolute normalized UTF-8 path.
    #[error(
        "service config key {key} must be an absolute normalized UTF-8 path without NUL bytes \
         of at most {max_bytes} bytes: {}",
        path.display()
    )]
    InvalidPath {
        /// The rejected key.
        key: &'static str,
        /// The rejected path.
        path: PathBuf,
        /// The accepted maximum path length.
        max_bytes: usize,
    },
    /// `active_version` is not a safe install version directory name.
    #[error("service config key active_version is not a valid install version: {value:?}")]
    InvalidVersion {
        /// The rejected version.
        value: String,
    },
    /// `namespace.uid` is the reserved `(uid_t)-1` value.
    #[error("service config key namespace.uid {uid} is not a valid user id")]
    InvalidUid {
        /// The rejected user id.
        uid: u32,
    },
    /// A numeric key is outside its accepted range.
    #[error("service config key {key} = {value} is outside {min}..={max}")]
    OutOfRange {
        /// The rejected key.
        key: &'static str,
        /// The rejected value, saturated to `u64::MAX`.
        value: u64,
        /// The smallest accepted value.
        min: u64,
        /// The largest accepted value.
        max: u64,
    },
    /// A duration cannot be recorded as whole milliseconds.
    #[error("service config key {key} must be a whole number of milliseconds")]
    Precision {
        /// The rejected key.
        key: &'static str,
    },
    /// The environment allowlist is empty or too long.
    #[error("service config key environment.allowlist has {count} entries; expected 1..={max}")]
    AllowlistSize {
        /// The recorded entry count.
        count: usize,
        /// The accepted maximum.
        max: usize,
    },
    /// An environment allowlist entry is not a name or a trailing-`*` prefix.
    #[error("service config key environment.allowlist has invalid pattern {pattern:?}: {reason}")]
    InvalidPattern {
        /// The rejected pattern.
        pattern: String,
        /// Why the pattern was rejected.
        reason: &'static str,
    },
    /// An environment allowlist entry appears more than once.
    #[error("service config key environment.allowlist lists {pattern:?} more than once")]
    DuplicatePattern {
        /// The repeated pattern.
        pattern: String,
    },
    /// An installation root could not be canonicalized for verification.
    #[error("failed to canonicalize {key} {}: {source}", path.display())]
    Canonicalize {
        /// The namespace key the root is compared with.
        key: &'static str,
        /// The root that could not be resolved.
        path: PathBuf,
        /// The operating-system failure.
        #[source]
        source: io::Error,
    },
    /// The running installation differs from the recorded namespace inputs.
    #[error(
        "service config records {key} = {recorded}, but this process runs with {actual}; \
         it belongs to a different installation"
    )]
    NamespaceMismatch {
        /// The mismatching namespace key.
        key: &'static str,
        /// The recorded value.
        recorded: String,
        /// The value of the running installation.
        actual: String,
    },
}
