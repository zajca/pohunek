//! Descriptor-relative owner-private host-state persistence.
//!
//! The host directory is opened once without following symlinks. All record and
//! lock operations then use that directory descriptor, preventing path swapping
//! after validation. Every written record is a complete same-directory temporary
//! file, synced before rename; a parent-sync failure is reported separately
//! because the replacement may already be visible.

// Rust guideline compliant 2026-09-03

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};
use zeroize::Zeroizing;

/// Exact mode for every managed host-state directory.
const DIRECTORY_MODE: u32 = 0o700;
/// Exact mode for every managed host-state file.
const FILE_MODE: u32 = 0o600;
/// Maximum serialized size of one host-state record.
///
/// Host identity, governance, and key records are compact bounded metadata. A
/// one-mebibyte ceiling keeps corrupt or adversarial files from allocating an
/// unbounded startup buffer while leaving ample room for future signed outcomes.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Maximum bytes read into a record buffer, including the overflow sentinel.
///
/// Reading one byte beyond the accepted record ceiling detects a concurrent
/// grow-after-stat without reallocating a secret buffer that already contains
/// private key material.
const MAX_RECORD_READ_BYTES: usize = MAX_RECORD_BYTES + 1;
/// Bounded attempts to reserve a unique temporary filename.
const TEMP_NAME_ATTEMPTS: u64 = 128;

/// Durable host-state filesystem failure.
#[derive(Debug, thiserror::Error)]
pub enum HostStateError {
    /// A path component was not a safe regular file or directory.
    #[error("unsafe host-state path: {path}")]
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
    },
    /// A managed component was not owned by the daemon effective UID or exact mode.
    #[error("unsafe host-state ownership or permissions: {path}")]
    UnsafePermissions {
        /// Rejected path.
        path: PathBuf,
    },
    /// A caller supplied a record name with path components.
    #[error("invalid host-state record name")]
    InvalidRecordName,
    /// A data-record API attempted to access the reserved process lock file.
    #[error("host-state record name is reserved")]
    ReservedRecordName,
    /// A record exceeded the bounded persistence limit.
    #[error("host-state record exceeds {max_bytes} bytes: {path}")]
    RecordTooLarge {
        /// Record path.
        path: PathBuf,
        /// Maximum accepted serialized size.
        max_bytes: usize,
    },
    /// Another process currently owns the host-state lock.
    #[error("host-state lock is held: {path}")]
    LockContended {
        /// Lock path.
        path: PathBuf,
    },
    /// An operation failed before the atomic rename committed a replacement.
    #[error("host-state {operation} failed at {path}: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Affected safe path.
        path: PathBuf,
        /// Underlying operating-system cause.
        #[source]
        source: io::Error,
    },
    /// Rename succeeded but parent-directory durability could not be confirmed.
    #[error("host-state replacement committed but durability is uncertain at {path}: {source}")]
    CommittedDurabilityUncertain {
        /// Replaced record path.
        path: PathBuf,
        /// Parent-directory synchronization cause.
        #[source]
        source: io::Error,
    },
}

/// Open descriptor for one validated owner-private host-state directory.
#[derive(Debug)]
pub struct HostStateDir {
    state_path: PathBuf,
    path: PathBuf,
    dir: File,
    #[cfg(test)]
    faults: Faults,
}

/// Cross-process lock bound to one host-directory inode.
///
/// The retained directory descriptor is independently reopened and flocked for
/// the lock lifetime. The `state.lock` marker remains owner-private and is
/// bound to its current directory entry before use, but the directory flock is
/// authoritative: replacing only the marker cannot split cooperating daemon
/// processes into independent writers. This does not defend against a hostile
/// same-UID replacement of the complete XDG state root.
#[derive(Debug)]
pub struct HostStateLock {
    directory: File,
    marker: File,
    path: PathBuf,
}

struct RecordFile {
    file: File,
    path: PathBuf,
    length: usize,
}

impl Drop for HostStateLock {
    fn drop(&mut self) {
        let _ = self.marker.unlock();
        let _ = self.directory.unlock();
    }
}

impl HostStateDir {
    /// Opens or creates `<state_dir>/host` without following managed path components.
    ///
    /// `state_dir` must be an absolute XDG-derived application state path. Its
    /// existing ancestors are opened descriptor-by-descriptor without symlink
    /// traversal; the application and host directories must be exact `0700` and
    /// owned by the daemon effective UID.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError`] when a component is unsafe, permissions are too
    /// broad, or an operating-system operation fails.
    pub fn open_or_create(state_dir: &Path) -> Result<Self, HostStateError> {
        let state_path = state_dir.to_path_buf();
        let state = open_or_create_absolute_dir(state_dir, true)?;
        let host = open_or_create_child_dir(&state, pohunek_paths::HOST_STATE_SUBDIR)?;
        Ok(Self {
            state_path: state_path.clone(),
            path: state_path.join(pohunek_paths::HOST_STATE_SUBDIR),
            dir: host,
            #[cfg(test)]
            faults: Faults::default(),
        })
    }

    /// Returns the validated host-state directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Revalidates the current application and host-state directory bindings.
    ///
    /// This read-only check opens the current names without following links and
    /// proves that they still identify the descriptors retained by this
    /// repository. It never creates a missing path while diagnosing state.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError`] when either directory is missing, unsafe, or
    /// no longer names the originally validated directory.
    pub(crate) fn validate_current_layout(&self) -> Result<(), HostStateError> {
        let state = open_existing_absolute_dir(&self.state_path, true)?;
        let host = open_existing_child_dir(&state, pohunek_paths::HOST_STATE_SUBDIR, true)?;
        let current = host.metadata().map_err(|source| {
            io_error("inspect current host-state directory", &self.path, source)
        })?;
        let retained = self.dir.metadata().map_err(|source| {
            io_error("inspect retained host-state directory", &self.path, source)
        })?;
        if current.dev() != retained.dev() || current.ino() != retained.ino() {
            return Err(HostStateError::UnsafePath {
                path: self.path.clone(),
            });
        }
        Ok(())
    }

    /// Reads one bounded owner-private regular record without following symlinks.
    ///
    /// A missing record returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError`] for unsafe files, oversized data, or I/O failure.
    pub fn read_record(&self, name: &str) -> Result<Option<Vec<u8>>, HostStateError> {
        let Some(record) = self.open_record(name)? else {
            return Ok(None);
        };
        let mut bytes = Vec::with_capacity(record.length.min(MAX_RECORD_BYTES));
        #[cfg(test)]
        self.faults.grow_after_stat(&record.path)?;
        Self::read_record_bytes(record, &mut bytes)?;
        Ok(Some(bytes))
    }

    /// Reads one bounded owner-private secret record into zeroizing ownership.
    ///
    /// This is intentionally separate from [`Self::read_record`]: callers of
    /// private-key codecs receive a buffer that zeroizes the complete on-disk
    /// record on every success and error return after the read begins.
    ///
    /// A missing record returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError`] for unsafe files, oversized data, or I/O failure.
    pub(crate) fn read_secret_record(
        &self,
        name: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, HostStateError> {
        let Some(record) = self.open_record(name)? else {
            return Ok(None);
        };
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_RECORD_READ_BYTES));
        #[cfg(test)]
        self.faults.grow_after_stat(&record.path)?;
        Self::read_record_bytes(record, &mut bytes)?;
        Ok(Some(bytes))
    }

    fn open_record(&self, name: &str) -> Result<Option<RecordFile>, HostStateError> {
        validate_data_record_name(name)?;
        let path = self.record_path(name);
        let fd = match fs::openat(
            &self.dir,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error("open record", &path, source)),
        };
        let file = File::from(fd);
        validate_private_file(&file, &path)?;
        let length = usize::try_from(
            file.metadata()
                .map_err(|source| io_error("inspect record", &path, source))?
                .len(),
        )
        .map_err(|_conversion_error| HostStateError::RecordTooLarge {
            path: path.clone(),
            max_bytes: MAX_RECORD_BYTES,
        })?;
        if length > MAX_RECORD_BYTES {
            return Err(HostStateError::RecordTooLarge {
                path,
                max_bytes: MAX_RECORD_BYTES,
            });
        }
        Ok(Some(RecordFile { file, path, length }))
    }

    fn read_record_bytes(record: RecordFile, bytes: &mut Vec<u8>) -> Result<(), HostStateError> {
        let RecordFile { file, path, .. } = record;
        file.take(u64::try_from(MAX_RECORD_READ_BYTES).expect("record bound fits u64"))
            .read_to_end(bytes)
            .map_err(|source| io_error("read record", &path, source))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(HostStateError::RecordTooLarge {
                path,
                max_bytes: MAX_RECORD_BYTES,
            });
        }
        Ok(())
    }

    /// Atomically replaces one bounded owner-private record.
    ///
    /// The temporary file is created exclusively in this directory, flushed to
    /// disk, atomically renamed through the held directory descriptor, and then
    /// followed by directory synchronization.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError::CommittedDurabilityUncertain`] only after rename
    /// succeeded. Every other error leaves the previous record authoritative.
    pub fn replace_record(&self, name: &str, bytes: &[u8]) -> Result<(), HostStateError> {
        validate_data_record_name(name)?;
        let path = self.record_path(name);
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(HostStateError::RecordTooLarge {
                path,
                max_bytes: MAX_RECORD_BYTES,
            });
        }
        self.validate_existing_record(name)?;
        let (temp_name, temp_fd) = self.create_temp(name)?;
        let temp_path = self.record_path(&temp_name);
        let mut temp = File::from(temp_fd);
        let mut committed = false;
        let write_result = (|| {
            fs::fchmod(&temp, Mode::RUSR | Mode::WUSR).map_err(|source| {
                io_error("set temporary record permissions", &temp_path, source)
            })?;
            validate_private_file(&temp, &temp_path)?;
            #[cfg(test)]
            self.faults.write(&temp_path)?;
            temp.write_all(bytes)
                .map_err(|source| io_error("write temporary record", &temp_path, source))?;
            #[cfg(test)]
            self.faults.file_sync(&temp_path)?;
            temp.sync_all()
                .map_err(|source| io_error("sync temporary record", &temp_path, source))?;
            drop(temp);
            #[cfg(test)]
            self.faults.rename(&path)?;
            fs::renameat(&self.dir, &temp_name, &self.dir, name)
                .map_err(|source| io_error("rename record", &path, source))?;
            committed = true;
            #[cfg(test)]
            self.faults
                .reoccupy_temp_after_rename(&self.dir, &temp_name, &path)?;
            #[cfg(test)]
            self.faults.dir_sync(&path)?;
            self.dir
                .sync_all()
                .map_err(|source| HostStateError::CommittedDurabilityUncertain {
                    path: path.clone(),
                    source,
                })
        })();
        if write_result.is_err() && !committed {
            let _ = fs::unlinkat(&self.dir, &temp_name, fs::AtFlags::empty());
        }
        write_result
    }

    /// Acquires the owner-private cross-process host-state lock.
    ///
    /// This flocks the retained host-state directory inode before opening and
    /// validating the reserved marker, so marker replacement cannot create a
    /// second writer within the same authoritative directory.
    ///
    /// # Errors
    ///
    /// Returns [`HostStateError::LockContended`] while another holder is alive.
    pub fn acquire_lock(&self) -> Result<HostStateLock, HostStateError> {
        let name = pohunek_paths::HOST_STATE_LOCK_NAME;
        let path = self.record_path(name);
        let directory = self.open_lock_directory()?;
        match directory.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(HostStateError::LockContended { path });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(io_error("lock host-state directory", &path, source));
            }
        }
        let (fd, created) = match fs::openat(
            &self.dir,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => (fd, true),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => (
                fs::openat(
                    &self.dir,
                    name,
                    OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|source| io_error("open state lock", &path, source))?,
                false,
            ),
            Err(source) => return Err(io_error("create state lock", &path, source)),
        };
        let marker = File::from(fd);
        if created {
            fs::fchmod(&marker, Mode::RUSR | Mode::WUSR)
                .map_err(|source| io_error("set state lock permissions", &path, source))?;
        }
        validate_lock_marker_binding(&self.dir, &marker, &path)?;
        match marker.try_lock() {
            Ok(()) => Ok(HostStateLock {
                directory,
                marker,
                path,
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(HostStateError::LockContended { path }),
            Err(std::fs::TryLockError::Error(source)) => {
                Err(io_error("lock host state", &path, source))
            }
        }
    }

    fn open_lock_directory(&self) -> Result<File, HostStateError> {
        let fd = fs::openat(
            &self.dir,
            Path::new("."),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| io_error("open host-state lock directory", &self.path, source))?;
        let directory = File::from(fd);
        validate_directory(&directory, &self.path, true)?;
        let retained = self.dir.metadata().map_err(|source| {
            io_error("inspect retained host-state directory", &self.path, source)
        })?;
        let locked = directory
            .metadata()
            .map_err(|source| io_error("inspect lock host-state directory", &self.path, source))?;
        if retained.dev() != locked.dev() || retained.ino() != locked.ino() {
            return Err(HostStateError::UnsafePath {
                path: self.path.clone(),
            });
        }
        Ok(directory)
    }

    fn create_temp(&self, record_name: &str) -> Result<(String, OwnedFd), HostStateError> {
        for _ in 0..TEMP_NAME_ATTEMPTS {
            #[cfg(test)]
            let suffix = self
                .faults
                .take_temp_suffix()
                .map_or_else(random_temp_suffix, Ok)?;
            #[cfg(not(test))]
            let suffix = random_temp_suffix()?;
            let name = format!(".{record_name}.tmp.{suffix}");
            let path = self.record_path(&name);
            match fs::openat(
                &self.dir,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => return Ok((name, fd)),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                Err(source) => return Err(io_error("create temporary record", &path, source)),
            }
        }
        Err(io_error(
            "reserve unique temporary record",
            &self.path,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary name collision limit reached",
            ),
        ))
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn validate_existing_record(&self, name: &str) -> Result<(), HostStateError> {
        let path = self.record_path(name);
        match fs::openat(
            &self.dir,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => validate_private_file(&File::from(fd), &path),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error("inspect existing record", &path, source)),
        }
    }

    #[cfg(test)]
    fn inject_write(&self, kind: io::ErrorKind) {
        self.faults.set_write(kind);
    }

    #[cfg(test)]
    fn inject_file_sync(&self, kind: io::ErrorKind) {
        self.faults.set_file_sync(kind);
    }

    #[cfg(test)]
    pub(super) fn inject_rename(&self, kind: io::ErrorKind) {
        self.faults.set_rename(kind);
    }

    #[cfg(test)]
    pub(super) fn inject_directory_sync(&self, kind: io::ErrorKind) {
        self.faults.set_dir_sync(kind);
    }

    #[cfg(test)]
    fn inject_next_temp_suffix(&self, suffix: &str) {
        self.faults.set_temp_suffix(suffix);
    }

    #[cfg(test)]
    fn inject_temp_suffixes(&self, suffixes: impl IntoIterator<Item = String>) {
        self.faults.set_temp_suffixes(suffixes);
    }

    #[cfg(test)]
    fn inject_grow_after_stat(&self) {
        self.faults.set_grow_after_stat();
    }

    #[cfg(test)]
    fn inject_reoccupy_temp_after_rename(&self) {
        self.faults.set_reoccupy_temp_after_rename();
    }
}

fn validate_lock_marker_binding(
    directory: &File,
    marker: &File,
    path: &Path,
) -> Result<(), HostStateError> {
    validate_private_file(marker, path)?;
    let fd = fs::openat(
        directory,
        pohunek_paths::HOST_STATE_LOCK_NAME,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("bind state lock marker", path, source))?;
    let current = File::from(fd);
    validate_private_file(&current, path)?;
    let held = marker
        .metadata()
        .map_err(|source| io_error("inspect held state lock marker", path, source))?;
    let current = current
        .metadata()
        .map_err(|source| io_error("inspect current state lock marker", path, source))?;
    if held.dev() != current.dev() || held.ino() != current.ino() {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

impl HostStateLock {
    /// Returns the safe lock path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_or_create_absolute_dir(path: &Path, managed_final: bool) -> Result<File, HostStateError> {
    let raw = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || raw == b"/"
        || raw.windows(2).any(|window| window == b"//")
        || raw.windows(3).any(|window| window == b"/./")
        || raw.ends_with(b"/.")
        || raw.ends_with(b"/")
    {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let root = fs::open(
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|source| io_error("open filesystem root", Path::new("/"), source))?;
    let mut current = File::from(root);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let names: Vec<_> = components.collect();
    if names.is_empty()
        || names
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    for (index, component) in names.iter().enumerate() {
        if let Component::Normal(name) = component {
            current = open_or_create_dir_component(
                &current,
                Path::new(name),
                index + 1 == names.len() && managed_final,
            )?;
        } else {
            return Err(HostStateError::UnsafePath {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(current)
}

fn open_existing_absolute_dir(path: &Path, managed_final: bool) -> Result<File, HostStateError> {
    let raw = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || raw == b"/"
        || raw.windows(2).any(|window| window == b"//")
        || raw.windows(3).any(|window| window == b"/./")
        || raw.ends_with(b"/.")
        || raw.ends_with(b"/")
    {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let root = fs::open(
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|source| io_error("open filesystem root", Path::new("/"), source))?;
    let mut current = File::from(root);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    let names: Vec<_> = components.collect();
    if names.is_empty()
        || names
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    for (index, component) in names.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(HostStateError::UnsafePath {
                path: path.to_path_buf(),
            });
        };
        current = open_existing_dir_component(
            &current,
            Path::new(name),
            index + 1 == names.len() && managed_final,
        )?;
    }
    Ok(current)
}

fn open_or_create_child_dir(parent: &File, name: &str) -> Result<File, HostStateError> {
    open_or_create_dir_component(parent, Path::new(name), true)
}

fn open_existing_child_dir(
    parent: &File,
    name: &str,
    managed: bool,
) -> Result<File, HostStateError> {
    open_existing_dir_component(parent, Path::new(name), managed)
}

fn open_existing_dir_component(
    parent: &File,
    name: &Path,
    managed: bool,
) -> Result<File, HostStateError> {
    let display = name.to_path_buf();
    let fd = fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|source| io_error("open existing host-state directory", &display, source))?;
    let file = File::from(fd);
    validate_directory(&file, &display, managed)?;
    Ok(file)
}

fn open_or_create_dir_component(
    parent: &File,
    name: &Path,
    managed: bool,
) -> Result<File, HostStateError> {
    let display = name.to_path_buf();
    let fd = match fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let mut created = false;
            if let Err(source) = fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                if source.kind() != io::ErrorKind::AlreadyExists {
                    return Err(io_error("create host-state directory", &display, source));
                }
            } else {
                created = true;
            }
            if created {
                #[cfg(test)]
                fault_parent_sync_after_mkdir(&display)?;
                parent.sync_all().map_err(|source| {
                    io_error("sync parent directory after create", &display, source)
                })?;
            }
            fs::openat(
                parent,
                name,
                OFlags::RDONLY
                    | OFlags::DIRECTORY
                    | OFlags::CLOEXEC
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|source| io_error("open created host-state directory", &display, source))?
        }
        Err(source) => return Err(io_error("open host-state directory", &display, source)),
    };
    let file = File::from(fd);
    validate_directory(&file, &display, managed)?;
    Ok(file)
}

fn validate_directory(file: &File, path: &Path, managed: bool) -> Result<(), HostStateError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect directory", path, source))?;
    if !metadata.is_dir() {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    if managed && (metadata.mode() & 0o777 != DIRECTORY_MODE || metadata.uid() != effective_uid()) {
        return Err(HostStateError::UnsafePermissions {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_private_file(file: &File, path: &Path) -> Result<(), HostStateError> {
    let metadata = file
        .metadata()
        .map_err(|source| io_error("inspect record", path, source))?;
    if !metadata.is_file() {
        return Err(HostStateError::UnsafePath {
            path: path.to_path_buf(),
        });
    }
    if metadata.mode() & 0o777 != FILE_MODE
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
    {
        return Err(HostStateError::UnsafePermissions {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

fn validate_data_record_name(name: &str) -> Result<(), HostStateError> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        if name == pohunek_paths::HOST_STATE_LOCK_NAME {
            Err(HostStateError::ReservedRecordName)
        } else {
            Ok(())
        }
    } else {
        Err(HostStateError::InvalidRecordName)
    }
}

fn io_error(operation: &'static str, path: &Path, source: impl Into<io::Error>) -> HostStateError {
    HostStateError::Io {
        operation,
        path: path.to_path_buf(),
        source: source.into(),
    }
}

fn random_temp_suffix() -> Result<String, HostStateError> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|source| {
            io_error(
                "read secure temporary-name entropy",
                Path::new("/dev/urandom"),
                source,
            )
        })?;
    Ok(u128::from_le_bytes(bytes).to_string())
}

#[cfg(test)]
#[derive(Debug, Default)]
struct Faults {
    write: std::sync::Mutex<Option<io::ErrorKind>>,
    file_sync: std::sync::Mutex<Option<io::ErrorKind>>,
    rename: std::sync::Mutex<Option<io::ErrorKind>>,
    dir_sync: std::sync::Mutex<Option<io::ErrorKind>>,
    temp_suffixes: std::sync::Mutex<std::collections::VecDeque<String>>,
    grow_after_stat: std::sync::Mutex<bool>,
    reoccupy_temp_after_rename: std::sync::Mutex<bool>,
}

#[cfg(test)]
impl Faults {
    fn write(&self, path: &Path) -> Result<(), HostStateError> {
        Self::take_precommit(&self.write, "write temporary record", path)
    }

    fn file_sync(&self, path: &Path) -> Result<(), HostStateError> {
        Self::take_precommit(&self.file_sync, "sync temporary record", path)
    }

    fn rename(&self, path: &Path) -> Result<(), HostStateError> {
        Self::take_precommit(&self.rename, "rename record", path)
    }

    fn dir_sync(&self, path: &Path) -> Result<(), HostStateError> {
        match self
            .dir_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(kind) => Err(HostStateError::CommittedDurabilityUncertain {
                path: path.to_path_buf(),
                source: io::Error::from(kind),
            }),
            None => Ok(()),
        }
    }

    fn take_precommit(
        slot: &std::sync::Mutex<Option<io::ErrorKind>>,
        operation: &'static str,
        path: &Path,
    ) -> Result<(), HostStateError> {
        let kind = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match kind {
            Some(kind) => Err(io_error(operation, path, io::Error::from(kind))),
            None => Ok(()),
        }
    }

    fn set_write(&self, kind: io::ErrorKind) {
        *self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kind);
    }

    fn set_file_sync(&self, kind: io::ErrorKind) {
        *self
            .file_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kind);
    }

    fn set_rename(&self, kind: io::ErrorKind) {
        *self
            .rename
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kind);
    }

    fn set_dir_sync(&self, kind: io::ErrorKind) {
        *self
            .dir_sync
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(kind);
    }

    fn set_temp_suffix(&self, suffix: &str) {
        *self
            .temp_suffixes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            std::collections::VecDeque::from([suffix.to_owned()]);
    }

    fn set_temp_suffixes(&self, suffixes: impl IntoIterator<Item = String>) {
        *self
            .temp_suffixes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = suffixes.into_iter().collect();
    }

    fn take_temp_suffix(&self) -> Option<String> {
        self.temp_suffixes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    fn grow_after_stat(&self, path: &Path) -> Result<(), HostStateError> {
        let grow = std::mem::take(
            &mut *self
                .grow_after_stat
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if grow {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(&vec![0_u8; MAX_RECORD_BYTES + 1]))
                .map_err(|source| io_error("grow record after stat", path, source))?;
        }
        Ok(())
    }

    fn set_grow_after_stat(&self) {
        *self
            .grow_after_stat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    fn set_reoccupy_temp_after_rename(&self) {
        *self
            .reoccupy_temp_after_rename
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    fn reoccupy_temp_after_rename(
        &self,
        dir: &File,
        name: &str,
        record_path: &Path,
    ) -> Result<(), HostStateError> {
        if !std::mem::take(
            &mut *self
                .reoccupy_temp_after_rename
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) {
            return Ok(());
        }
        let fd = fs::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|source| HostStateError::CommittedDurabilityUncertain {
            path: record_path.to_path_buf(),
            source: source.into(),
        })?;
        File::from(fd)
            .write_all(b"occupied-after-rename")
            .map_err(|source| HostStateError::CommittedDurabilityUncertain {
                path: record_path.to_path_buf(),
                source,
            })
    }
}

#[cfg(test)]
std::thread_local! {
    static PARENT_SYNC_AFTER_MKDIR: std::cell::RefCell<Option<io::ErrorKind>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn fault_parent_sync_after_mkdir(path: &Path) -> Result<(), HostStateError> {
    match PARENT_SYNC_AFTER_MKDIR.with(|slot| slot.borrow_mut().take()) {
        Some(kind) => Err(io_error(
            "sync parent directory after create",
            path,
            io::Error::from(kind),
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
fn inject_parent_sync_after_mkdir(kind: io::ErrorKind) {
    PARENT_SYNC_AFTER_MKDIR.with(|slot| *slot.borrow_mut() = Some(kind));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn state_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pohunek-host-state-unit-{tag}-{}-{nanos}/state/pohunek",
            std::process::id()
        ));
        path
    }

    #[test]
    fn precommit_faults_preserve_the_previous_record() {
        let host = HostStateDir::open_or_create(&state_dir("precommit")).expect("open host state");
        host.replace_record("identity.json", b"old")
            .expect("seed record");
        host.inject_write(io::ErrorKind::StorageFull);
        assert!(matches!(
            host.replace_record("identity.json", b"new"),
            Err(HostStateError::Io { .. })
        ));
        assert_eq!(
            host.read_record("identity.json").expect("read record"),
            Some(b"old".to_vec())
        );

        host.inject_file_sync(io::ErrorKind::ReadOnlyFilesystem);
        assert!(matches!(
            host.replace_record("identity.json", b"new"),
            Err(HostStateError::Io { .. })
        ));
        assert_eq!(
            host.read_record("identity.json").expect("read record"),
            Some(b"old".to_vec())
        );

        host.inject_rename(io::ErrorKind::ReadOnlyFilesystem);
        assert!(matches!(
            host.replace_record("identity.json", b"new"),
            Err(HostStateError::Io { .. })
        ));
        assert_eq!(
            host.read_record("identity.json").expect("read record"),
            Some(b"old".to_vec())
        );
    }

    #[test]
    fn postrename_fault_is_reported_as_durability_uncertain() {
        let host = HostStateDir::open_or_create(&state_dir("postrename")).expect("open host state");
        host.replace_record("governance.json", b"old")
            .expect("seed record");
        host.inject_directory_sync(io::ErrorKind::Other);
        assert!(matches!(
            host.replace_record("governance.json", b"new"),
            Err(HostStateError::CommittedDurabilityUncertain { .. })
        ));
        assert_eq!(
            host.read_record("governance.json").expect("read record"),
            Some(b"new".to_vec())
        );
    }

    #[test]
    fn temporary_symlink_is_never_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let host =
            HostStateDir::open_or_create(&state_dir("temporary-symlink")).expect("open host state");
        let target = host.path().join("target");
        std::fs::write(&target, b"keep").expect("write target");
        host.inject_next_temp_suffix("predictable");
        let temporary = host.path().join(".identity.json.tmp.predictable");
        symlink(&target, &temporary).expect("create temporary symlink");

        host.replace_record("identity.json", b"record")
            .expect("collision retries with secure entropy");
        assert_eq!(std::fs::read(&target).expect("read target"), b"keep");
        assert!(std::fs::symlink_metadata(&temporary)
            .expect("inspect temporary symlink")
            .file_type()
            .is_symlink());
    }

    #[test]
    fn secret_read_is_bounded_even_when_a_record_grows_after_metadata_inspection() {
        let host = HostStateDir::open_or_create(&state_dir("read-bound")).expect("open host state");
        host.replace_record("approval.key", b"small")
            .expect("seed record");
        host.inject_grow_after_stat();

        assert!(matches!(
            host.read_secret_record("approval.key"),
            Err(HostStateError::RecordTooLarge { .. })
        ));
    }

    #[test]
    fn secret_read_growth_never_reallocates_its_zeroizing_buffer() {
        let host = HostStateDir::open_or_create(&state_dir("secret-read-growth"))
            .expect("open host state");
        host.replace_record("approval.key", b"small")
            .expect("seed secret record");
        let record = host
            .open_record("approval.key")
            .expect("open secret record")
            .expect("secret record exists");
        let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_RECORD_READ_BYTES));
        let capacity = bytes.capacity();
        assert_eq!(capacity, MAX_RECORD_READ_BYTES);

        host.inject_grow_after_stat();
        host.faults
            .grow_after_stat(&record.path)
            .expect("grow secret record after stat");
        assert!(matches!(
            HostStateDir::read_record_bytes(record, &mut bytes),
            Err(HostStateError::RecordTooLarge { .. })
        ));
        assert_eq!(bytes.len(), MAX_RECORD_READ_BYTES);
        assert_eq!(bytes.capacity(), capacity);
    }

    #[test]
    fn secret_read_returns_zeroizing_record_ownership() {
        let host =
            HostStateDir::open_or_create(&state_dir("secret-read")).expect("open host state");
        host.replace_record("approval.key", b"private record")
            .expect("seed secret record");

        let bytes: Zeroizing<Vec<u8>> = host
            .read_secret_record("approval.key")
            .expect("read secret record")
            .expect("secret record exists");
        assert_eq!(&*bytes, b"private record");
        assert_eq!(bytes.capacity(), MAX_RECORD_READ_BYTES);
    }

    #[test]
    fn rejects_existing_oversized_record_before_allocating_its_contents() {
        let host =
            HostStateDir::open_or_create(&state_dir("oversized-record")).expect("open host state");
        let record = host.path().join("identity.json");
        std::fs::write(&record, vec![0_u8; MAX_RECORD_BYTES + 1]).expect("write oversized record");
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(FILE_MODE))
            .expect("set record mode");

        assert!(matches!(
            host.read_record("identity.json"),
            Err(HostStateError::RecordTooLarge { .. })
        ));
    }

    #[test]
    fn parent_sync_failure_stops_before_child_directory_creation() {
        let path = state_dir("parent-sync");
        std::fs::create_dir_all(path.parent().expect("state parent")).expect("create state parent");
        inject_parent_sync_after_mkdir(io::ErrorKind::StorageFull);

        assert!(matches!(
            HostStateDir::open_or_create(&path),
            Err(HostStateError::Io {
                operation: "sync parent directory after create",
                ..
            })
        ));
        assert!(
            !path.join(pohunek_paths::HOST_STATE_SUBDIR).exists(),
            "a failed parent fsync must prevent child host-directory creation"
        );
    }

    #[test]
    fn postrename_failure_never_removes_a_reoccupied_temporary_name() {
        let host =
            HostStateDir::open_or_create(&state_dir("postrename-temp")).expect("open host state");
        host.inject_next_temp_suffix("reoccupied");
        host.inject_reoccupy_temp_after_rename();
        host.inject_directory_sync(io::ErrorKind::Other);

        assert!(matches!(
            host.replace_record("identity.json", b"record"),
            Err(HostStateError::CommittedDurabilityUncertain { .. })
        ));
        assert_eq!(
            std::fs::read(host.path().join(".identity.json.tmp.reoccupied"))
                .expect("reoccupied name remains"),
            b"occupied-after-rename"
        );
    }

    #[test]
    fn temporary_name_collision_limit_preserves_the_old_record_and_collision_files() {
        let host = HostStateDir::open_or_create(&state_dir("temporary-collisions"))
            .expect("open host state");
        host.replace_record("identity.json", b"old")
            .expect("seed record");
        let suffixes: Vec<_> = (0..TEMP_NAME_ATTEMPTS)
            .map(|index| format!("collision-{index}"))
            .collect();
        for suffix in &suffixes {
            std::fs::write(
                host.path().join(format!(".identity.json.tmp.{suffix}")),
                b"keep",
            )
            .expect("create collision");
        }
        host.inject_temp_suffixes(suffixes.clone());

        assert!(matches!(
            host.replace_record("identity.json", b"new"),
            Err(HostStateError::Io { .. })
        ));
        assert_eq!(
            host.read_record("identity.json").expect("read old record"),
            Some(b"old".to_vec())
        );
        assert_eq!(
            std::fs::read(host.path().join(".identity.json.tmp.collision-0"))
                .expect("read collision"),
            b"keep"
        );
        assert_eq!(
            std::fs::read(host.path().join(format!(
                ".identity.json.tmp.collision-{}",
                TEMP_NAME_ATTEMPTS - 1
            )))
            .expect("read last collision"),
            b"keep"
        );
    }

    #[test]
    fn data_record_apis_reserve_the_state_lock_name() {
        let host =
            HostStateDir::open_or_create(&state_dir("reserved-lock")).expect("open host state");
        let lock = host.acquire_lock().expect("hold state lock");

        assert!(matches!(
            host.read_record(pohunek_paths::HOST_STATE_LOCK_NAME),
            Err(HostStateError::ReservedRecordName)
        ));
        assert!(matches!(
            host.replace_record(pohunek_paths::HOST_STATE_LOCK_NAME, b"forbidden"),
            Err(HostStateError::ReservedRecordName)
        ));
        assert!(matches!(
            host.acquire_lock(),
            Err(HostStateError::LockContended { .. })
        ));
        drop(lock);
    }

    #[test]
    fn replacing_the_lock_marker_cannot_split_directory_lock_authority() {
        let state = state_dir("replaced-lock-marker");
        let holder = HostStateDir::open_or_create(&state).expect("open lock holder directory");
        let first = holder.acquire_lock().expect("acquire first lock");
        let marker = holder.path().join(pohunek_paths::HOST_STATE_LOCK_NAME);
        std::fs::remove_file(&marker).expect("unlink held lock marker");
        std::fs::write(&marker, b"replacement").expect("recreate lock marker");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(FILE_MODE))
            .expect("restore replacement lock marker mode");

        let challenger = HostStateDir::open_or_create(&state).expect("open challenger directory");
        assert!(matches!(
            challenger.acquire_lock(),
            Err(HostStateError::LockContended { .. })
        ));

        drop(first);
        drop(
            challenger
                .acquire_lock()
                .expect("reacquire after original directory lock releases"),
        );
    }

    #[test]
    fn absolute_state_paths_reject_noncanonical_components() {
        for path in [
            Path::new("/tmp/../pohunek-host-state"),
            Path::new("/"),
            Path::new("/tmp//pohunek-host-state"),
            Path::new("/tmp/./pohunek-host-state"),
            Path::new("/tmp/pohunek-host-state/"),
        ] {
            assert!(matches!(
                open_or_create_absolute_dir(path, true),
                Err(HostStateError::UnsafePath { .. })
            ));
        }
    }

    #[test]
    fn errors_do_not_render_record_contents() {
        let host =
            HostStateDir::open_or_create(&state_dir("error-redaction")).expect("open host state");
        let sentinel = b"host-state-secret-sentinel";
        let bytes = sentinel
            .iter()
            .copied()
            .cycle()
            .take(MAX_RECORD_BYTES + 1)
            .collect::<Vec<_>>();
        let error = host
            .replace_record("identity.json", &bytes)
            .expect_err("oversized record must fail");
        assert!(!error.to_string().contains("host-state-secret-sentinel"));
        assert!(!format!("{error:?}").contains("host-state-secret-sentinel"));
    }
}
