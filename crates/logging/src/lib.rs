//! Provides bounded owner-private JSON-lines log files.
//!
//! [`Writer`] rotates before a whole tracing event would cross the configured
//! file limit. A per-family file lock serializes independent processes, so all
//! worker generations for one session share one aggregate retention bound.

#![forbid(unsafe_code)]

// Rust guideline compliant 2026-07-28

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub mod config;

/// Owner-only directory permissions for structured logs.
const DIRECTORY_MODE: u32 = 0o700;

/// Owner-only permissions for log and lock files.
const FILE_MODE: u32 = 0o600;

/// Valid JSON emitted when one event cannot fit in an empty bounded file.
///
/// The original event is dropped as one unit, so it cannot leave a malformed
/// partial JSON line or reveal any of its fields.
const OVERSIZE_NOTICE: &[u8] = b"{\"level\":\"WARN\",\"target\":\"pohunek_logging\",\"message\":\"log event dropped because it exceeded the per-file size limit\"}\n";

/// Size and count bounds for one owned log family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    max_file_bytes: u64,
    max_files: usize,
}

impl Policy {
    /// Validates and creates a retention policy.
    ///
    /// `max_files` includes the active file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPolicy`] when either limit is zero.
    pub fn new(max_file_bytes: u64, max_files: usize) -> Result<Self, Error> {
        if max_file_bytes == 0 || max_files == 0 {
            return Err(Error::InvalidPolicy {
                max_file_bytes,
                max_files,
            });
        }
        Ok(Self {
            max_file_bytes,
            max_files,
        })
    }

    /// Returns the maximum bytes in one file.
    #[must_use]
    pub const fn max_file_bytes(self) -> u64 {
        self.max_file_bytes
    }

    /// Returns the maximum file count, including the active file.
    #[must_use]
    pub const fn max_files(self) -> usize {
        self.max_files
    }
}

/// Legacy owned filenames pruned during writer initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Legacy {
    /// No separate legacy filename pattern.
    None,
    /// One exact legacy filename.
    Exact(String),
    /// Every legacy filename beginning with this prefix.
    Prefix(String),
}

impl Legacy {
    /// Creates an exact legacy filename rule.
    #[must_use]
    pub fn exact(name: impl Into<String>) -> Self {
        Self::Exact(name.into())
    }

    /// Creates a legacy filename-prefix rule.
    #[must_use]
    pub fn prefix(prefix: impl Into<String>) -> Self {
        Self::Prefix(prefix.into())
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::Exact(exact) => name == exact,
            Self::Prefix(prefix) => name.starts_with(prefix),
        }
    }
}

/// Names owned by one rotating log family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    active: String,
    legacy: Legacy,
}

impl Files {
    /// Validates owned filenames.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidName`] for empty names or path components.
    pub fn new(active: impl Into<String>, legacy: Legacy) -> Result<Self, Error> {
        let active = active.into();
        validate_name(&active)?;
        match &legacy {
            Legacy::None => {}
            Legacy::Exact(name) | Legacy::Prefix(name) => validate_name(name)?,
        }
        Ok(Self { active, legacy })
    }

    fn active_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.active)
    }

    fn lock_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.lock", self.active))
    }

    fn rotation_path(&self, dir: &Path, index: usize) -> PathBuf {
        dir.join(format!("{}.{index}", self.active))
    }

    fn rotation_index(&self, name: &str) -> Option<usize> {
        let suffix = name.strip_prefix(&format!("{}.", self.active))?;
        let index = suffix.parse::<usize>().ok()?;
        (index > 0 && suffix == index.to_string()).then_some(index)
    }
}

/// A process-safe size-bounded writer for one log family.
#[derive(Debug)]
pub struct Writer {
    dir: PathBuf,
    files: Files,
    policy: Policy,
    lock: File,
}

impl Writer {
    /// Prepares a bounded writer and prunes owned stale files.
    ///
    /// The exact log directory is rejected when it is a symlink. Existing
    /// symlinks encountered while scanning the directory are never followed or
    /// deleted.
    ///
    /// # Errors
    ///
    /// Returns a typed [`Error`] for invalid configuration, unsafe filesystem
    /// entries, or failed I/O.
    pub fn open(dir: &Path, files: Files, policy: Policy) -> Result<Self, Error> {
        prepare_dir(dir)?;
        let lock_path = files.lock_path(dir);
        reject_symlink_or_non_file(&lock_path)?;
        let lock = open_owner_file(&lock_path)?;
        lock.lock()
            .map_err(|source| Error::io("lock log family", &lock_path, source))?;

        let prune_result = prune_locked(dir, &files, policy);
        let unlock_result = lock
            .unlock()
            .map_err(|source| Error::io("unlock log family", &lock_path, source));
        prune_result?;
        unlock_result?;

        Ok(Self {
            dir: dir.to_path_buf(),
            files,
            policy,
            lock,
        })
    }

    fn write_event(&mut self, event: &[u8]) -> Result<(), Error> {
        let lock_path = self.files.lock_path(&self.dir);
        self.lock
            .lock()
            .map_err(|source| Error::io("lock log family", &lock_path, source))?;
        let result = self.write_event_locked(event);
        let unlock_result = self
            .lock
            .unlock()
            .map_err(|source| Error::io("unlock log family", &lock_path, source));
        result?;
        unlock_result
    }

    fn write_event_locked(&self, event: &[u8]) -> Result<(), Error> {
        let event_len =
            u64::try_from(event.len()).map_err(|_conversion_error| Error::EventTooLarge)?;
        if event_len > self.policy.max_file_bytes {
            let notice_len = u64::try_from(OVERSIZE_NOTICE.len())
                .map_err(|_conversion_error| Error::EventTooLarge)?;
            if notice_len <= self.policy.max_file_bytes {
                self.write_bounded_locked(OVERSIZE_NOTICE)?;
            }
            return Ok(());
        }
        self.write_bounded_locked(event)
    }

    fn write_bounded_locked(&self, event: &[u8]) -> Result<(), Error> {
        let active = self.files.active_path(&self.dir);
        let current_size = regular_file_size(&active)?;
        let event_len =
            u64::try_from(event.len()).map_err(|_conversion_error| Error::EventTooLarge)?;
        if current_size.saturating_add(event_len) > self.policy.max_file_bytes {
            rotate_locked(&self.dir, &self.files, self.policy)?;
        }

        reject_symlink_or_non_file(&active)?;
        let mut file = open_owner_file(&active)?;
        file.write_all(event)
            .map_err(|source| Error::io("write log event", &active, source))?;
        file.flush()
            .map_err(|source| Error::io("flush log event", &active, source))
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_event(buf)
            .map(|()| buf.len())
            .map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Deletes one owned log family after its producer has stopped.
///
/// Symlinks and non-files are preserved. Callers must ensure no producer can
/// recreate the family after deletion.
///
/// # Errors
///
/// Returns a typed [`Error`] when locking or deleting an owned regular file
/// fails.
pub fn remove_family(dir: &Path, files: &Files) -> Result<(), Error> {
    if !dir.exists() {
        return Ok(());
    }
    reject_dir_symlink(dir)?;
    let lock_path = files.lock_path(dir);
    reject_symlink_or_non_file(&lock_path)?;
    let lock = open_owner_file(&lock_path)?;
    lock.lock()
        .map_err(|source| Error::io("lock log family", &lock_path, source))?;
    let result = remove_family_locked(dir, files);
    let unlock_result = lock
        .unlock()
        .map_err(|source| Error::io("unlock log family", &lock_path, source));
    result?;
    unlock_result?;
    drop(lock);
    remove_regular(&lock_path)?;
    Ok(())
}

/// Bounded logging failure.
#[derive(Debug)]
pub enum Error {
    /// A retention policy contains a zero limit.
    InvalidPolicy {
        /// Maximum bytes configured for each file.
        max_file_bytes: u64,
        /// Maximum configured file count.
        max_files: usize,
    },
    /// An owned filename is empty or contains path components.
    InvalidName {
        /// Rejected filename or prefix.
        name: String,
    },
    /// A path expected to be owner-controlled is a symlink or non-file.
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
    },
    /// A platform cannot represent the incoming event length as `u64`.
    EventTooLarge,
    /// A filesystem operation failed.
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
}

impl Error {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy {
                max_file_bytes,
                max_files,
            } => write!(
                f,
                "log policy requires non-zero limits (max_file_bytes={max_file_bytes}, max_files={max_files})"
            ),
            Self::InvalidName { name } => {
                write!(f, "unsafe owned log filename {name:?}")
            }
            Self::UnsafePath { path } => {
                write!(f, "refusing unsafe log path {}", path.display())
            }
            Self::EventTooLarge => write!(f, "log event length cannot be represented"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "{operation} at {} failed: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn validate_name(name: &str) -> Result<(), Error> {
    let path = Path::new(name);
    let mut components = path.components();
    let valid = !name.is_empty()
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidName {
            name: name.to_owned(),
        })
    }
}

fn prepare_dir(dir: &Path) -> Result<(), Error> {
    if dir.exists() {
        reject_dir_symlink(dir)?;
    } else {
        fs::create_dir_all(dir).map_err(|source| Error::io("create log directory", dir, source))?;
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(DIRECTORY_MODE))
        .map_err(|source| Error::io("set log directory permissions", dir, source))
}

fn reject_dir_symlink(dir: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(dir)
        .map_err(|source| Error::io("inspect log directory", dir, source))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(Error::UnsafePath {
            path: dir.to_path_buf(),
        })
    }
}

fn reject_symlink_or_non_file(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(Error::UnsafePath {
            path: path.to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io("inspect log file", path, source)),
    }
}

fn open_owner_file(path: &Path) -> Result<File, Error> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)
        .map_err(|source| Error::io("open log file", path, source))?;
    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
        .map_err(|source| Error::io("set log file permissions", path, source))?;
    Ok(file)
}

fn regular_file_size(path: &Path) -> Result<u64, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(Error::UnsafePath {
            path: path.to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(source) => Err(Error::io("inspect log file", path, source)),
    }
}

fn prune_locked(dir: &Path, files: &Files, policy: Policy) -> Result<(), Error> {
    for entry in fs::read_dir(dir).map_err(|source| Error::io("scan log directory", dir, source))? {
        let entry = entry.map_err(|source| Error::io("read log directory entry", dir, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| Error::io("inspect log directory entry", &entry.path(), source))?;
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if files.legacy.matches(&name) {
            remove_regular(&entry.path())?;
            continue;
        }
        if let Some(index) = files.rotation_index(&name) {
            let size = entry
                .metadata()
                .map_err(|source| Error::io("inspect rotated log", &entry.path(), source))?
                .len();
            if index >= policy.max_files || size > policy.max_file_bytes {
                remove_regular(&entry.path())?;
            }
        }
    }

    let active = files.active_path(dir);
    if regular_file_size(&active)? > policy.max_file_bytes {
        remove_regular(&active)?;
    }
    Ok(())
}

fn rotate_locked(dir: &Path, files: &Files, policy: Policy) -> Result<(), Error> {
    let active = files.active_path(dir);
    if policy.max_files == 1 {
        remove_regular(&active)?;
        return Ok(());
    }

    let last = files.rotation_path(dir, policy.max_files - 1);
    reject_symlink_or_non_file(&last)?;
    remove_regular(&last)?;

    for index in (1..policy.max_files - 1).rev() {
        let source = files.rotation_path(dir, index);
        if !source.exists() {
            continue;
        }
        reject_symlink_or_non_file(&source)?;
        let destination = files.rotation_path(dir, index + 1);
        reject_symlink_or_non_file(&destination)?;
        fs::rename(&source, &destination)
            .map_err(|error| Error::io("rotate log file", &source, error))?;
    }

    if active.exists() {
        reject_symlink_or_non_file(&active)?;
        let first = files.rotation_path(dir, 1);
        reject_symlink_or_non_file(&first)?;
        fs::rename(&active, &first)
            .map_err(|source| Error::io("rotate active log", &active, source))?;
    }
    Ok(())
}

fn remove_family_locked(dir: &Path, files: &Files) -> Result<(), Error> {
    for entry in fs::read_dir(dir).map_err(|source| Error::io("scan log directory", dir, source))? {
        let entry = entry.map_err(|source| Error::io("read log directory entry", dir, source))?;
        let file_type = entry
            .file_type()
            .map_err(|source| Error::io("inspect log directory entry", &entry.path(), source))?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == files.active
            || files.rotation_index(&name).is_some()
            || files.legacy.matches(&name)
        {
            remove_regular(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_regular(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            fs::remove_file(path).map_err(|source| Error::io("remove owned log file", path, source))
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io("inspect owned log file", path, source)),
    }
}
