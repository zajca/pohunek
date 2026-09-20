//! Provides owner-private, descriptor-relative filesystem operations.

use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RenameFlags, Stat};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

// Rust guideline compliant 2026-09-20

/// Permission and special-mode bits reported by `stat`.
const MODE_MASK: u32 = 0o7777;
/// Bounded attempts for collision-resistant internal staging names.
const STAGING_NAME_ATTEMPTS: usize = 16;
/// Random bytes encoded in internal staging names.
const STAGING_RANDOM_BYTES: usize = 8;

/// A filesystem operation result.
pub type FsResult<T> = Result<T, FsError>;

/// A filesystem entry type accepted by trusted operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryKind {
    /// A regular file.
    RegularFile,
    /// A directory.
    Directory,
    /// A Unix-domain socket.
    Socket,
    /// A symbolic link inspected without following its target.
    Symlink,
}

impl EntryKind {
    fn matches(self, actual: FileType) -> bool {
        matches!(
            (self, actual),
            (Self::RegularFile, FileType::RegularFile)
                | (Self::Directory, FileType::Directory)
                | (Self::Socket, FileType::Socket)
                | (Self::Symlink, FileType::Symlink)
        )
    }
}

/// Stable identity captured from a trusted filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryIdentity {
    device: u64,
    inode: u64,
    kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntryGeneration {
    identity: EntryIdentity,
    size: i128,
    change_seconds: i128,
    change_nanoseconds: i128,
}

impl EntryIdentity {
    /// Returns the device number containing this entry.
    #[must_use]
    pub fn device(self) -> u64 {
        self.device
    }

    /// Returns the inode number of this entry.
    #[must_use]
    pub fn inode(self) -> u64 {
        self.inode
    }

    /// Returns the validated entry type.
    #[must_use]
    pub fn kind(self) -> EntryKind {
        self.kind
    }
}

/// The result of an atomic move that cannot replace its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoveOutcome {
    /// The source was moved and directory metadata was synchronized.
    Moved,
    /// The destination already existed and neither entry was changed.
    DestinationExists,
}

/// The result of removing an inode-bound staged entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoveOutcome {
    /// The staged entry was removed and its directory was synchronized.
    Removed,
    /// The staged name no longer existed.
    Missing,
    /// The staged name referred to a different inode and was preserved.
    IdentityChanged,
}

/// The result of moving an entry to an inode-bound staging name.
#[derive(Debug)]
#[non_exhaustive]
pub enum StageOutcome {
    /// The expected inode is retained under the staging name.
    Staged(StagedEntry),
    /// The source name did not exist.
    Missing,
    /// The staging name already existed and neither entry was changed.
    DestinationExists,
    /// The source identity changed before staging and was preserved.
    IdentityChanged,
}

/// A failure from a trusted filesystem operation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FsError {
    /// An operation requiring an absolute path received another path form.
    #[error("trusted directory path is not a normalized absolute path: {path}", path = .path.display())]
    InvalidAbsolutePath {
        /// The rejected path.
        path: PathBuf,
    },
    /// A descriptor-relative operation received more than one path component.
    #[error("trusted filesystem name is not one normal path component: {name:?}")]
    InvalidComponent {
        /// The rejected name.
        name: OsString,
    },
    /// A requested mode contained bits outside Unix permission and special bits.
    #[error("trusted filesystem mode {mode:#o} contains unsupported bits")]
    InvalidMode {
        /// The rejected raw mode.
        mode: u32,
    },
    /// A bounded read encountered more bytes than its configured limit.
    #[error("trusted file exceeds the {max_bytes}-byte read limit: {path}", path = .path.display())]
    FileTooLarge {
        /// The rejected file.
        path: PathBuf,
        /// Maximum accepted byte count.
        max_bytes: usize,
    },
    /// An entry had a different filesystem type than required.
    #[error("trusted entry has type {actual}, expected {expected:?}: {path}", path = .path.display())]
    UnsafeType {
        /// The inspected entry.
        path: PathBuf,
        /// The required entry type.
        expected: EntryKind,
        /// A stable description of the actual entry type.
        actual: &'static str,
    },
    /// An entry was not owned by the effective user.
    #[error("trusted entry is owned by uid {actual}, expected uid {expected}: {path}", path = .path.display())]
    UnsafeOwner {
        /// The inspected entry.
        path: PathBuf,
        /// The reported owner.
        actual: u32,
        /// The required effective owner.
        expected: u32,
    },
    /// An entry did not have the exact required mode.
    #[error("trusted entry has mode {actual:#o}, expected {expected:#o}: {path}", path = .path.display())]
    UnsafeMode {
        /// The inspected entry.
        path: PathBuf,
        /// The reported permission and special bits.
        actual: u32,
        /// The required permission and special bits.
        expected: u32,
    },
    /// An entry's link count could not establish the required binding.
    #[error("trusted entry has unsafe link count {actual}: {path}", path = .path.display())]
    UnsafeLinkCount {
        /// The inspected entry.
        path: PathBuf,
        /// The reported hard-link count.
        actual: u64,
    },
    /// Another process holds the descriptor-bound advisory lock.
    #[error("trusted filesystem lock is already held: {path}", path = .path.display())]
    LockContended {
        /// The lock marker path.
        path: PathBuf,
    },
    /// The platform or filesystem cannot provide atomic no-replace rename.
    #[error("atomic no-replace move is unsupported for {path}", path = .path.display())]
    AtomicMoveUnsupported {
        /// The move destination.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: io::Error,
    },
    /// A recovery move could not restore its original name.
    #[error("staged entry could not be restored because its original name is occupied: {path}", path = .path.display())]
    RecoveryConflict {
        /// The occupied original path.
        path: PathBuf,
    },
    /// A pathname stopped referring to the expected inode.
    #[error("trusted entry identity changed during operation: {path}", path = .path.display())]
    IdentityChanged {
        /// The changed pathname.
        path: PathBuf,
    },
    /// A mutation committed, but its containing directory could not be synchronized.
    #[error("{operation} committed but durability is uncertain for {path}: {source}", path = .path.display())]
    CommittedDurabilityUncertain {
        /// The operation whose namespace mutation committed.
        operation: &'static str,
        /// The committed entry path.
        path: PathBuf,
        /// The synchronization failure.
        #[source]
        source: io::Error,
    },
    /// An operating-system operation failed before its mutation committed.
    #[error("failed to {operation} at {path}: {source}", path = .path.display())]
    Io {
        /// The failed operation.
        operation: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: io::Error,
    },
}

impl FsError {
    /// Returns the underlying I/O kind when this failure came from the OS.
    #[must_use]
    pub fn io_kind(&self) -> Option<io::ErrorKind> {
        match self {
            Self::Io { source, .. }
            | Self::CommittedDurabilityUncertain { source, .. }
            | Self::AtomicMoveUnsupported { source, .. } => Some(source.kind()),
            _ => None,
        }
    }

    /// Returns the underlying raw operating-system error when available.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Io { source, .. }
            | Self::CommittedDurabilityUncertain { source, .. }
            | Self::AtomicMoveUnsupported { source, .. } => source.raw_os_error(),
            _ => None,
        }
    }
}

/// A failure while atomically replacing one file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AtomicReplaceError {
    /// The destination was not changed.
    #[error("atomic replacement failed before commit: {0}")]
    BeforeCommit(#[source] FsError),
    /// Rename committed, but the containing directory was not durably synchronized.
    #[error("atomic replacement committed but durability is uncertain: {0}")]
    CommittedDurabilityUncertain(#[source] FsError),
}

/// A failure while moving an inode-bound staged entry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StagedMoveError {
    /// The destination was unchanged and the entry was restored to its staging name.
    #[error("staged move failed before commit: {0}")]
    BeforeCommit(#[source] FsError),
    /// The entry reached the destination, but final validation or durability failed.
    #[error("staged move committed at {}: {source}", path.display())]
    Committed {
        /// The current path naming the moved entry.
        path: PathBuf,
        /// The failure observed after the namespace commit.
        #[source]
        source: FsError,
    },
    /// Recovery could not restore the entry to its tracked staging name.
    #[error("staged move recovery failed with the entry at {}: {source}", path.display())]
    RecoveryRequired {
        /// The last known path naming the staged entry.
        path: PathBuf,
        /// The recovery failure.
        #[source]
        source: FsError,
    },
}

/// A validated owner-private directory retained by descriptor.
#[derive(Debug)]
pub struct TrustedDir {
    path: PathBuf,
    file: File,
}

/// A descriptor-bound exclusive advisory lock.
#[derive(Debug)]
pub struct AdvisoryLock {
    _directory: File,
    _marker: File,
}

/// An entry moved aside and bound to its original inode identity.
#[derive(Debug)]
pub struct StagedEntry {
    directory: File,
    directory_path: PathBuf,
    name: OsString,
    identity: EntryIdentity,
    generation: EntryGeneration,
    entry_directory: Option<File>,
}

impl TrustedDir {
    /// Returns the diagnostic absolute path associated with the retained descriptor.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Duplicates the retained directory descriptor for a specialized trusted
    /// operation that cannot be expressed by the higher-level API.
    ///
    /// The duplicate remains bound to the already validated directory inode;
    /// callers must continue to perform all child operations descriptor-relatively.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the descriptor cannot be duplicated.
    pub fn try_clone_descriptor(&self) -> FsResult<File> {
        self.file.try_clone().map_err(|source| {
            io_error("duplicate trusted directory descriptor", &self.path, source)
        })
    }

    /// Opens and validates an existing absolute directory without following symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the path is invalid, unsafe, or cannot be opened.
    pub fn open_absolute(path: impl AsRef<Path>, mode: u32) -> FsResult<Self> {
        Self::open_absolute_inner(path.as_ref(), mode, false)
    }

    /// Opens an existing absolute directory owned by the effective user and
    /// rejects caller-selected unsafe mode bits.
    ///
    /// This is intended for externally managed owner configuration roots that
    /// may legitimately be owner-readable by other accounts while remaining
    /// non-writable to them. Pohunek-owned roots should use [`Self::open_absolute`]
    /// with an exact private mode instead.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the path, owner, type, link count, or mode is unsafe.
    pub fn open_absolute_owner_safe(
        path: impl AsRef<Path>,
        forbidden_mode_bits: u32,
    ) -> FsResult<Self> {
        validate_mode(forbidden_mode_bits)?;
        let path = path.as_ref();
        let components = absolute_components(path)?;
        let root_fd = fs::open(
            Path::new("/"),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| io_error("open filesystem root", Path::new("/"), source))?;
        let mut directory = Self {
            path: PathBuf::from("/"),
            file: File::from(root_fd),
        };
        let root_stat = fs::fstat(&directory.file)
            .map_err(|source| io_error("inspect filesystem root", Path::new("/"), source))?;
        let system_uid = stat_uid(&root_stat);
        let effective_uid = rustix::process::geteuid().as_raw();
        for component in components {
            directory = directory.open_child_inner(component, 0o700, false, false)?;
            validate_ancestor(
                &directory.file,
                &directory.path,
                0o700,
                effective_uid,
                system_uid,
            )?;
        }
        let stat = fs::fstat(&directory.file)
            .map_err(|source| io_error("inspect owner-safe directory", &directory.path, source))?;
        let actual = stat_mode(&stat);
        if actual & forbidden_mode_bits != 0 {
            return Err(FsError::UnsafeMode {
                path: directory.path.clone(),
                actual,
                expected: actual & !forbidden_mode_bits,
            });
        }
        validate_stat(&stat, &directory.path, EntryKind::Directory, None)?;
        Ok(directory)
    }

    /// Opens or creates and validates an absolute directory without following symlinks.
    ///
    /// Missing path components and the final directory use exact `mode`. Existing
    /// effective-user-owned ancestors must not be group/world-writable; system
    /// ancestors must be non-writable or sticky.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the path is invalid, unsafe, or cannot be created.
    pub fn open_or_create_absolute(path: impl AsRef<Path>, mode: u32) -> FsResult<Self> {
        Self::open_absolute_inner(path.as_ref(), mode, true)
    }

    /// Opens and validates one existing child directory without following symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the name is invalid or the child is unsafe.
    pub fn open_child(&self, name: impl AsRef<OsStr>, mode: u32) -> FsResult<Self> {
        self.open_child_inner(name.as_ref(), mode, false, true)
    }

    /// Opens an existing owner-managed child directory while rejecting selected
    /// unsafe mode bits.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the child is missing, replaced, foreign-owned,
    /// or has any forbidden mode bit.
    pub fn open_child_owner_safe(
        &self,
        name: impl AsRef<OsStr>,
        forbidden_mode_bits: u32,
    ) -> FsResult<Self> {
        validate_mode(forbidden_mode_bits)?;
        let child = self.open_child_inner(name.as_ref(), 0o700, false, false)?;
        let stat = fs::fstat(&child.file).map_err(|source| {
            io_error("inspect owner-safe child directory", &child.path, source)
        })?;
        validate_stat(&stat, &child.path, EntryKind::Directory, None)?;
        let actual = stat_mode(&stat);
        if actual & forbidden_mode_bits != 0 {
            return Err(FsError::UnsafeMode {
                path: child.path.clone(),
                actual,
                expected: actual & !forbidden_mode_bits,
            });
        }
        Ok(child)
    }

    /// Opens or creates and validates one child directory without following symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the name is invalid or the child is unsafe.
    pub fn open_or_create_child(&self, name: impl AsRef<OsStr>, mode: u32) -> FsResult<Self> {
        self.open_child_inner(name.as_ref(), mode, true, true)
    }

    /// Creates one child directory exclusively and returns its retained descriptor.
    ///
    /// The inode is prepared under a random staging name and moved atomically,
    /// without replacement, to the requested name.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the destination exists or creation is unsafe.
    pub fn create_child_exclusive(&self, name: impl AsRef<OsStr>, mode: u32) -> FsResult<Self> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        let path = self.path.join(name);
        for _ in 0..STAGING_NAME_ATTEMPTS {
            let staging = random_staging_name(".pohunek-dir-")?;
            let staging_path = self.path.join(&staging);
            match fs::mkdirat(&self.file, &staging, mode_from_raw(mode)) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => continue,
                Err(source) => {
                    return Err(io_error(
                        "create trusted directory staging",
                        &staging_path,
                        source,
                    ));
                }
            }
            prepare_random_staging_directory_mode(&self.file, &staging, &staging_path, mode)?;
            let child = Self {
                path: path.clone(),
                file: File::from(open_directory_at(&self.file, &staging).map_err(|source| {
                    io_error("open created trusted directory", &staging_path, source)
                })?),
            };
            fs::fchmod(&child.file, mode_from_raw(mode))
                .map_err(|source| io_error("set trusted directory mode", &staging_path, source))?;
            let identity =
                validate_fd(&child.file, &staging_path, EntryKind::Directory, Some(mode))?;
            child
                .sync()
                .map_err(|error| committed_error("create directory", &staging_path, error))?;
            match self.move_no_replace(&staging, self, name)? {
                MoveOutcome::Moved => {
                    if inspect_entry(&self.file, &path, name, EntryKind::Directory, Some(mode))?
                        != Some(identity)
                    {
                        return Err(FsError::IdentityChanged { path });
                    }
                    return Ok(child);
                }
                MoveOutcome::DestinationExists => {
                    let generation = require_entry_generation(
                        &self.file,
                        &staging_path,
                        &staging,
                        EntryKind::Directory,
                        Some(mode),
                        identity,
                    )?;
                    let residue = StagedEntry {
                        directory: self.try_clone_descriptor()?,
                        directory_path: self.path.clone(),
                        name: staging,
                        identity,
                        generation,
                        entry_directory: None,
                    };
                    let _ = residue.remove()?;
                    return Err(FsError::Io {
                        operation: "create trusted directory exclusively",
                        path,
                        source: io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "trusted directory destination already exists",
                        ),
                    });
                }
            }
        }
        Err(FsError::Io {
            operation: "allocate trusted directory staging name",
            path,
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "trusted directory staging collision budget exhausted",
            ),
        })
    }

    /// Acquires a nonblocking exclusive advisory lock retained by descriptors.
    ///
    /// The directory lock makes cooperating users of this API immune to lock-marker
    /// replacement. The marker is also opened without following symlinks and locked.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::LockContended`] when another holder owns the lock.
    pub fn acquire_lock(&self, name: impl AsRef<OsStr>, mode: u32) -> FsResult<AdvisoryLock> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        let path = self.path.join(name);
        // `dup` would share one open-file description with `self.file`, causing
        // the lock to outlive `AdvisoryLock`. Reopen `.` for an independent
        // description whose final close reliably releases the directory lock.
        let directory = File::from(
            open_directory_at(&self.file, OsStr::new("."))
                .map_err(|source| io_error("reopen lock directory", &path, source))?,
        );
        let authority = fs::fstat(&self.file)
            .map_err(|source| io_error("inspect lock authority", &path, source))?;
        let reopened = fs::fstat(&directory)
            .map_err(|source| io_error("inspect reopened lock authority", &path, source))?;
        if authority.st_dev != reopened.st_dev || authority.st_ino != reopened.st_ino {
            return Err(FsError::Io {
                operation: "reopen the same lock directory inode",
                path,
                source: io::Error::other("directory identity changed while reopening"),
            });
        }
        lock_nonblocking(&directory, &path)?;

        let (fd, created) = match fs::openat(
            &self.file,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode_from_raw(mode),
        ) {
            Ok(fd) => (fd, true),
            Err(rustix::io::Errno::EXIST) => (
                fs::openat(
                    &self.file,
                    name,
                    OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|source| io_error("open lock marker", &path, source))?,
                false,
            ),
            Err(source) => return Err(io_error("create lock marker", &path, source)),
        };
        let marker = File::from(fd);
        if created {
            fs::fchmod(&marker, mode_from_raw(mode))
                .map_err(|source| io_error("set lock marker mode", &path, source))?;
        }
        lock_nonblocking(&marker, &path)?;
        validate_fd(&marker, &path, EntryKind::RegularFile, Some(mode))?;
        Ok(AdvisoryLock {
            _directory: directory,
            _marker: marker,
        })
    }

    /// Captures a trusted entry's stable device and inode identity.
    ///
    /// A missing name returns `Ok(None)`. Regular files must have exactly one
    /// hard link; directories and sockets must remain linked.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the name or existing entry is unsafe.
    pub fn entry_identity(
        &self,
        name: impl AsRef<OsStr>,
        kind: EntryKind,
    ) -> FsResult<Option<EntryIdentity>> {
        let name = validate_component(name.as_ref())?;
        inspect_entry(&self.file, &self.path.join(name), name, kind, None)
    }

    /// Captures a trusted entry identity after validating its exact mode.
    ///
    /// A missing name returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the mode, name, or existing entry is unsafe.
    pub fn entry_identity_with_mode(
        &self,
        name: impl AsRef<OsStr>,
        kind: EntryKind,
        mode: u32,
    ) -> FsResult<Option<EntryIdentity>> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        inspect_entry(&self.file, &self.path.join(name), name, kind, Some(mode))
    }

    /// Lists names from the retained directory descriptor.
    ///
    /// The returned names are untrusted until passed to another descriptor-relative
    /// operation such as [`Self::open_child`] or [`Self::read_file`].
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when descriptor-bound enumeration fails.
    pub fn entry_names(&self) -> FsResult<Vec<OsString>> {
        let entries = fs::Dir::read_from(&self.file)
            .map_err(|source| io_error("open trusted directory enumeration", &self.path, source))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|source| io_error("enumerate trusted directory", &self.path, source))?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsStr::from_bytes(bytes).to_os_string());
        }
        Ok(names)
    }

    /// Binds a Unix listener under a collision-resistant staging name with an
    /// exact mode established before publication.
    ///
    /// The socket is created under a collision-resistant name in this verified
    /// owner-private directory. Its identity is checked around mode setup and
    /// again before the caller can publish it under a stable name.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when randomness, binding, validation, or mode setup
    /// fails, or when the bounded collision budget is exhausted.
    pub fn bind_unix_listener_staged(
        &self,
        prefix: &str,
        mode: u32,
    ) -> FsResult<(OsString, std::os::unix::net::UnixListener, EntryIdentity)> {
        validate_mode(mode)?;
        if prefix.is_empty()
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(FsError::InvalidComponent {
                name: OsString::from(prefix),
            });
        }
        for _ in 0..STAGING_NAME_ATTEMPTS {
            let name = random_staging_name(prefix)?;
            let path = self.path.join(&name);
            match bind_unix_listener_with_mode(&path, mode) {
                Ok(listener) => {
                    let identity = self
                        .entry_identity(&name, EntryKind::Socket)?
                        .ok_or_else(|| FsError::IdentityChanged { path: path.clone() })?;
                    let mode_result = (|| {
                        set_random_staging_mode_at(&self.file, &name, &path, identity, mode)?;
                        if self.entry_identity_with_mode(&name, EntryKind::Socket, mode)?
                            != Some(identity)
                        {
                            return Err(FsError::IdentityChanged { path: path.clone() });
                        }
                        Ok(())
                    })();
                    if let Err(error) = mode_result {
                        drop(listener);
                        if let Ok(StageOutcome::Staged(staged)) =
                            self.stage_random(&name, ".pohunek-socket-stale-", identity)
                        {
                            let _ = staged.remove();
                        }
                        return Err(error);
                    }
                    return Ok((name, listener, identity));
                }
                Err(source) if source.kind() == io::ErrorKind::AddrInUse => {}
                Err(source) => return Err(io_error("bind staged Unix socket", &path, source)),
            }
        }
        Err(FsError::Io {
            operation: "allocate staged Unix socket name",
            path: self.path.clone(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "staged Unix socket collision budget exhausted",
            ),
        })
    }

    /// Sets an expected owner-private entry's exact mode without following symlinks.
    ///
    /// The pathname identity is checked before and after the mode change, while
    /// the mutation itself is applied through a retained descriptor. Darwin does
    /// not permit opening pathname sockets for descriptor-bound chmod, so socket
    /// callers use [`Self::bind_unix_listener_staged`] inside a verified private
    /// directory instead.
    ///
    /// # Errors
    ///
    /// Returns [`FsError::IdentityChanged`] if the pathname does not retain
    /// `expected`, or another [`FsError`] when validation or chmod fails.
    pub fn set_entry_mode(
        &self,
        name: impl AsRef<OsStr>,
        expected: EntryIdentity,
        mode: u32,
    ) -> FsResult<()> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        let path = self.path.join(name);
        let before = inspect_entry(&self.file, &path, name, expected.kind, None)?;
        if before != Some(expected) {
            return Err(FsError::IdentityChanged { path });
        }
        set_entry_mode_at(&self.file, name, &path, expected, mode)?;
        let after = inspect_entry(&self.file, &path, name, expected.kind, Some(mode))?;
        if after != Some(expected) {
            return Err(FsError::IdentityChanged { path });
        }
        Ok(())
    }

    /// Reads an owner-private regular file through the trusted directory descriptor.
    ///
    /// The open is no-follow, the descriptor is validated before reading, and
    /// `max_bytes` bounds memory use even if the file changes during the read.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the entry is unsafe, too large, or cannot be read.
    pub fn read_file(
        &self,
        name: impl AsRef<OsStr>,
        mode: u32,
        max_bytes: usize,
    ) -> FsResult<Vec<u8>> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        let path = self.path.join(name);
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| io_error("open trusted file", &path, source))?;
        let mut file = File::from(fd);
        validate_fd(&file, &path, EntryKind::RegularFile, Some(mode))?;
        let limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(|source| io_error("read trusted file", &path, source))?;
        if bytes.len() > max_bytes {
            return Err(FsError::FileTooLarge { path, max_bytes });
        }
        Ok(bytes)
    }

    /// Creates, writes, and durably synchronizes one regular file exclusively.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the name exists or the file cannot be created,
    /// validated, written, or synchronized.
    pub fn create_file(
        &self,
        name: impl AsRef<OsStr>,
        contents: &[u8],
        mode: u32,
    ) -> FsResult<EntryIdentity> {
        validate_mode(mode)?;
        let name = validate_component(name.as_ref())?;
        let path = self.path.join(name);
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode_from_raw(mode),
        )
        .map_err(|source| io_error("create trusted file exclusively", &path, source))?;
        let mut file = File::from(fd);
        let mut committed = false;
        let result = (|| {
            fs::fchmod(&file, mode_from_raw(mode))
                .map_err(|source| io_error("set trusted file mode", &path, source))?;
            let identity = validate_fd(&file, &path, EntryKind::RegularFile, Some(mode))?;
            file.write_all(contents)
                .map_err(|source| io_error("write trusted file", &path, source))?;
            sync_file(&file)?;
            if inspect_entry(&self.file, &path, name, EntryKind::RegularFile, Some(mode))?
                != Some(identity)
            {
                return Err(FsError::IdentityChanged { path: path.clone() });
            }
            committed = true;
            self.sync()
                .map_err(|error| committed_error("create file", &path, error))?;
            Ok(identity)
        })();
        if result.is_err() && !committed {
            if let Ok(identity) = validate_fd(&file, &path, EntryKind::RegularFile, Some(mode)) {
                if let Ok(StageOutcome::Staged(entry)) =
                    self.stage_random(name, ".pohunek-file-stale-", identity)
                {
                    let _ = entry.remove();
                }
            }
        }
        result
    }

    /// Atomically replaces one owner-private regular file.
    ///
    /// The temporary name is created exclusively, written and durably synchronized,
    /// then renamed over the destination. A post-rename directory-sync failure is
    /// reported separately because the new contents are already authoritative.
    ///
    /// # Errors
    ///
    /// Returns [`AtomicReplaceError::BeforeCommit`] before rename, or
    /// [`AtomicReplaceError::CommittedDurabilityUncertain`] after rename.
    #[expect(
        clippy::too_many_lines,
        reason = "the write, identity revalidation, commit boundary, and inode-safe cleanup form one auditable transaction"
    )]
    pub fn replace_file(
        &self,
        destination: impl AsRef<OsStr>,
        temporary: impl AsRef<OsStr>,
        contents: &[u8],
        mode: u32,
    ) -> Result<(), AtomicReplaceError> {
        validate_mode(mode).map_err(AtomicReplaceError::BeforeCommit)?;
        let destination =
            validate_component(destination.as_ref()).map_err(AtomicReplaceError::BeforeCommit)?;
        let temporary =
            validate_component(temporary.as_ref()).map_err(AtomicReplaceError::BeforeCommit)?;
        if destination == temporary {
            return Err(AtomicReplaceError::BeforeCommit(
                FsError::InvalidComponent {
                    name: temporary.to_os_string(),
                },
            ));
        }
        let destination_path = self.path.join(destination);
        let temporary_path = self.path.join(temporary);
        let destination_identity = inspect_entry(
            &self.file,
            &destination_path,
            destination,
            EntryKind::RegularFile,
            Some(mode),
        )
        .map_err(AtomicReplaceError::BeforeCommit)?;

        let fd = fs::openat(
            &self.file,
            temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode_from_raw(mode),
        )
        .map_err(|source| {
            AtomicReplaceError::BeforeCommit(io_error(
                "create atomic replacement temporary file",
                &temporary_path,
                source,
            ))
        })?;
        let mut temporary_file = File::from(fd);
        let prepared = (|| {
            fs::fchmod(&temporary_file, mode_from_raw(mode)).map_err(|source| {
                io_error("set atomic replacement mode", &temporary_path, source)
            })?;
            let temporary_identity = validate_fd(
                &temporary_file,
                &temporary_path,
                EntryKind::RegularFile,
                Some(mode),
            )?;
            temporary_file
                .write_all(contents)
                .map_err(|source| io_error("write atomic replacement", &temporary_path, source))?;
            sync_file(&temporary_file)?;
            if inspect_entry(
                &self.file,
                &temporary_path,
                temporary,
                EntryKind::RegularFile,
                Some(mode),
            )? != Some(temporary_identity)
            {
                return Err(FsError::IdentityChanged {
                    path: temporary_path.clone(),
                });
            }
            Ok(temporary_identity)
        })();
        let temporary_identity = match prepared {
            Ok(identity) => identity,
            Err(error) => {
                if let Ok(identity) = validate_fd(
                    &temporary_file,
                    &temporary_path,
                    EntryKind::RegularFile,
                    Some(mode),
                ) {
                    if let Ok(StageOutcome::Staged(entry)) =
                        self.stage_random(temporary, ".pohunek-replace-stale-", identity)
                    {
                        let _ = entry.remove();
                    }
                }
                return Err(AtomicReplaceError::BeforeCommit(error));
            }
        };
        let activation = if let Some(expected) = destination_identity {
            match inspect_entry(
                &self.file,
                &destination_path,
                destination,
                EntryKind::RegularFile,
                Some(mode),
            ) {
                Ok(Some(actual)) if actual == expected => {
                    fs::renameat(&self.file, temporary, &self.file, destination).map_err(|source| {
                        io_error("atomically replace file", &destination_path, source)
                    })
                }
                Ok(_) => Err(FsError::IdentityChanged {
                    path: destination_path.clone(),
                }),
                Err(error) => Err(error),
            }
        } else {
            match fs::renameat_with(
                &self.file,
                temporary,
                &self.file,
                destination,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => Ok(()),
                Err(rustix::io::Errno::EXIST) => Err(FsError::IdentityChanged {
                    path: destination_path.clone(),
                }),
                Err(source)
                    if matches!(
                        source,
                        rustix::io::Errno::NOSYS
                            | rustix::io::Errno::NOTSUP
                            | rustix::io::Errno::INVAL
                    ) =>
                {
                    Err(FsError::AtomicMoveUnsupported {
                        path: destination_path.clone(),
                        source: source.into(),
                    })
                }
                Err(source) => Err(io_error(
                    "atomically install file without replacement",
                    &destination_path,
                    source,
                )),
            }
        };
        if let Err(error) = activation {
            if let Ok(StageOutcome::Staged(entry)) =
                self.stage_random(temporary, ".pohunek-replace-stale-", temporary_identity)
            {
                let _ = entry.remove();
            }
            return Err(AtomicReplaceError::BeforeCommit(error));
        }
        if inspect_entry(
            &self.file,
            &destination_path,
            destination,
            EntryKind::RegularFile,
            Some(mode),
        )
        .map_err(|error| {
            AtomicReplaceError::CommittedDurabilityUncertain(committed_error(
                "validate replaced file",
                &destination_path,
                error,
            ))
        })? != Some(temporary_identity)
        {
            return Err(AtomicReplaceError::CommittedDurabilityUncertain(
                FsError::CommittedDurabilityUncertain {
                    operation: "validate replaced file",
                    path: destination_path.clone(),
                    source: io::Error::other(
                        "replacement destination changed after the atomic rename",
                    ),
                },
            ));
        }
        self.sync().map_err(|error| {
            AtomicReplaceError::CommittedDurabilityUncertain(committed_error(
                "replace file",
                &destination_path,
                error,
            ))
        })
    }

    /// Atomically moves one entry without replacing an existing destination.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] for invalid names, unsupported kernel primitives, or I/O.
    pub fn move_no_replace(
        &self,
        source: impl AsRef<OsStr>,
        destination_dir: &TrustedDir,
        destination: impl AsRef<OsStr>,
    ) -> FsResult<MoveOutcome> {
        let source = validate_component(source.as_ref())?;
        let destination = validate_component(destination.as_ref())?;
        let destination_path = destination_dir.path.join(destination);
        if let Some(source) = injected_move_failure() {
            return Err(io_error(
                "move entry without replacement",
                &destination_path,
                source,
            ));
        }
        match fs::renameat_with(
            &self.file,
            source,
            &destination_dir.file,
            destination,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => return Ok(MoveOutcome::DestinationExists),
            Err(source)
                if matches!(
                    source,
                    rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP | rustix::io::Errno::INVAL
                ) =>
            {
                return Err(FsError::AtomicMoveUnsupported {
                    path: destination_path,
                    source: source.into(),
                });
            }
            Err(source) => {
                return Err(io_error(
                    "move entry without replacement",
                    &destination_path,
                    source,
                ));
            }
        }
        self.sync()
            .map_err(|error| committed_error("move entry", &self.path.join(source), error))?;
        destination_dir
            .sync()
            .map_err(|error| committed_error("move entry", &destination_path, error))?;
        Ok(MoveOutcome::Moved)
    }

    /// Moves an expected inode to a staging name without replacement.
    ///
    /// A raced identity is restored to the source name before
    /// [`StageOutcome::IdentityChanged`] is returned.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when inspection, movement, or recovery fails.
    pub fn stage(
        &self,
        source: impl AsRef<OsStr>,
        staging: impl AsRef<OsStr>,
        expected: EntryIdentity,
    ) -> FsResult<StageOutcome> {
        let source = validate_component(source.as_ref())?;
        let staging = validate_component(staging.as_ref())?;
        let Some(current) = inspect_entry(
            &self.file,
            &self.path.join(source),
            source,
            expected.kind,
            None,
        )?
        else {
            return Ok(StageOutcome::Missing);
        };
        if current != expected {
            return Ok(StageOutcome::IdentityChanged);
        }
        let directory = self.file.try_clone().map_err(|source_error| {
            io_error(
                "duplicate staging directory descriptor",
                &self.path,
                source_error,
            )
        })?;
        if self.move_no_replace(source, self, staging)? == MoveOutcome::DestinationExists {
            return Ok(StageOutcome::DestinationExists);
        }
        let staged_path = self.path.join(staging);
        let staged =
            inspect_entry_generation(&self.file, &staged_path, staging, expected.kind, None)?;
        if staged.map(|generation| generation.identity) != Some(expected) {
            match self.move_no_replace(staging, self, source)? {
                MoveOutcome::Moved => return Ok(StageOutcome::IdentityChanged),
                MoveOutcome::DestinationExists => {
                    return Err(FsError::RecoveryConflict {
                        path: self.path.join(source),
                    });
                }
            }
        }
        let entry_directory = if expected.kind == EntryKind::Directory {
            let descriptor = File::from(
                open_directory_at(&self.file, staging)
                    .map_err(|source| io_error("open staged directory", &staged_path, source))?,
            );
            if validate_fd_base(&descriptor, &staged_path, EntryKind::Directory)? != expected
                || inspect_entry(&self.file, &staged_path, staging, expected.kind, None)?
                    != Some(expected)
            {
                return Err(FsError::IdentityChanged { path: staged_path });
            }
            Some(descriptor)
        } else {
            None
        };
        Ok(StageOutcome::Staged(StagedEntry {
            directory,
            directory_path: self.path.clone(),
            name: staging.to_os_string(),
            identity: expected,
            generation: staged.expect("validated staged generation exists"),
            entry_directory,
        }))
    }

    /// Moves an expected inode to a collision-resistant quarantine name.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when staging fails or the bounded collision budget
    /// is exhausted.
    pub fn stage_random(
        &self,
        source: impl AsRef<OsStr>,
        prefix: &str,
        expected: EntryIdentity,
    ) -> FsResult<StageOutcome> {
        for _ in 0..STAGING_NAME_ATTEMPTS {
            let staging = random_staging_name(prefix)?;
            match self.stage(source.as_ref(), staging, expected)? {
                StageOutcome::DestinationExists => {}
                outcome => return Ok(outcome),
            }
        }
        Err(FsError::Io {
            operation: "allocate staging quarantine name",
            path: self.path.join(source.as_ref()),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "staging quarantine collision budget exhausted",
            ),
        })
    }

    /// Durably synchronizes this directory descriptor.
    ///
    /// On Apple platforms this uses `F_FULLFSYNC`; other Unix platforms use `fsync`.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the operating system cannot complete synchronization.
    pub fn sync(&self) -> FsResult<()> {
        durable_sync(&self.file)
            .map_err(|source| io_error("durably synchronize directory", &self.path, source))
    }

    fn open_absolute_inner(path: &Path, mode: u32, create: bool) -> FsResult<Self> {
        validate_mode(mode)?;
        let components = absolute_components(path)?;
        let root_fd = fs::open(
            Path::new("/"),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| io_error("open filesystem root", Path::new("/"), source))?;
        let mut directory = Self {
            path: PathBuf::from("/"),
            file: File::from(root_fd),
        };
        let root_stat = fs::fstat(&directory.file)
            .map_err(|source| io_error("inspect filesystem root", Path::new("/"), source))?;
        let system_uid = stat_uid(&root_stat);
        let effective_uid = rustix::process::geteuid().as_raw();
        if components.is_empty() {
            validate_fd(&directory.file, path, EntryKind::Directory, Some(mode))?;
            return Ok(directory);
        }
        let final_index = components.len() - 1;
        for (index, component) in components.into_iter().enumerate() {
            let enforce_policy = index == final_index;
            directory = directory.open_child_inner(component, mode, create, enforce_policy)?;
            if index != final_index {
                validate_ancestor(
                    &directory.file,
                    &directory.path,
                    mode,
                    effective_uid,
                    system_uid,
                )?;
            }
        }
        Ok(directory)
    }

    fn open_child_inner(
        &self,
        name: &OsStr,
        mode: u32,
        create: bool,
        enforce_policy: bool,
    ) -> FsResult<Self> {
        validate_mode(mode)?;
        let name = validate_component(name)?;
        let path = self.path.join(name);
        match open_directory_at(&self.file, name) {
            Ok(fd) => {
                let child = Self {
                    path: path.clone(),
                    file: File::from(fd),
                };
                validate_fd_base(&child.file, &path, EntryKind::Directory)?;
                if enforce_policy {
                    validate_fd(&child.file, &path, EntryKind::Directory, Some(mode))?;
                }
                return Ok(child);
            }
            Err(rustix::io::Errno::NOENT) if create => {}
            Err(source) => return Err(io_error("open trusted directory", &path, source)),
        }

        for _ in 0..STAGING_NAME_ATTEMPTS {
            let staging = random_staging_name(".pohunek-dir-")?;
            let staging_path = self.path.join(&staging);
            match fs::mkdirat(&self.file, &staging, mode_from_raw(mode)) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => continue,
                Err(source) => {
                    return Err(io_error(
                        "create trusted directory staging",
                        &staging_path,
                        source,
                    ));
                }
            }
            prepare_random_staging_directory_mode(&self.file, &staging, &staging_path, mode)?;
            let child = Self {
                path: path.clone(),
                file: File::from(open_directory_at(&self.file, &staging).map_err(|source| {
                    io_error("open created trusted directory", &staging_path, source)
                })?),
            };
            validate_fd_owner(&child.file, &staging_path)?;
            fs::fchmod(&child.file, mode_from_raw(mode))
                .map_err(|source| io_error("set trusted directory mode", &staging_path, source))?;
            let identity =
                validate_fd(&child.file, &staging_path, EntryKind::Directory, Some(mode))?;
            child
                .sync()
                .map_err(|error| committed_error("create directory", &staging_path, error))?;
            match self.move_no_replace(&staging, self, name)? {
                MoveOutcome::Moved => {
                    if inspect_entry(&self.file, &path, name, EntryKind::Directory, Some(mode))?
                        != Some(identity)
                    {
                        return Err(FsError::IdentityChanged { path });
                    }
                    return Ok(child);
                }
                MoveOutcome::DestinationExists => {
                    let generation = require_entry_generation(
                        &self.file,
                        &staging_path,
                        &staging,
                        EntryKind::Directory,
                        Some(mode),
                        identity,
                    )?;
                    let residue = StagedEntry {
                        directory: self.file.try_clone().map_err(|source| {
                            io_error("duplicate trusted directory descriptor", &self.path, source)
                        })?,
                        directory_path: self.path.clone(),
                        name: staging,
                        identity,
                        generation,
                        entry_directory: None,
                    };
                    let _ = residue.remove()?;
                    let fd = open_directory_at(&self.file, name).map_err(|source| {
                        io_error("open raced trusted directory", &path, source)
                    })?;
                    let existing = Self {
                        path: path.clone(),
                        file: File::from(fd),
                    };
                    validate_fd(&existing.file, &path, EntryKind::Directory, Some(mode))?;
                    return Ok(existing);
                }
            }
        }
        Err(FsError::Io {
            operation: "allocate trusted directory staging name",
            path,
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "trusted directory staging collision budget exhausted",
            ),
        })
    }
}

fn validate_ancestor(
    file: &File,
    path: &Path,
    private_mode: u32,
    effective_uid: u32,
    system_uid: u32,
) -> FsResult<()> {
    let stat =
        fs::fstat(file).map_err(|source| io_error("inspect trusted ancestor", path, source))?;
    let mode = stat_mode(&stat);
    let owner = stat_uid(&stat);
    if owner == system_uid && mode & 0o1000 != 0 {
        return Ok(());
    }
    if owner == effective_uid {
        if mode & 0o022 != 0 {
            return Err(FsError::UnsafeMode {
                path: path.to_path_buf(),
                actual: mode,
                expected: private_mode,
            });
        }
        return Ok(());
    }
    if owner == system_uid && mode & 0o022 == 0 {
        return Ok(());
    }
    Err(FsError::UnsafeOwner {
        path: path.to_path_buf(),
        actual: owner,
        expected: effective_uid,
    })
}

impl StagedEntry {
    /// Returns the diagnostic path currently naming the staged entry.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.directory_path.join(&self.name)
    }

    /// Returns the identity captured before the entry was staged.
    #[must_use]
    pub fn identity(&self) -> EntryIdentity {
        self.identity
    }

    /// Removes the staged entry only if its filesystem identity and generation
    /// are unchanged.
    ///
    /// The caller must retain the lifecycle, daemon-instance, or worker
    /// authority that serialized the staging operation. Pohunek's owner-only
    /// filesystem boundary treats processes running as the same effective UID
    /// as trusted; the retained parent descriptor prevents ancestor retargeting.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when inspection, removal, or directory sync fails.
    pub fn remove(self) -> FsResult<RemoveOutcome> {
        let original_path = self.directory_path.join(&self.name);
        let current = inspect_entry_generation(
            &self.directory,
            &original_path,
            &self.name,
            self.identity.kind,
            None,
        )?;
        match current {
            None => return Ok(RemoveOutcome::Missing),
            Some(current) if current != self.generation => {
                return Ok(RemoveOutcome::IdentityChanged);
            }
            Some(_) => {}
        }
        let quarantine = move_to_random_quarantine(
            &self.directory,
            &self.directory_path,
            &self.name,
            ".pohunek-remove-",
        )?;
        let Some(quarantine) = quarantine else {
            return Ok(RemoveOutcome::Missing);
        };
        let path = self.directory_path.join(&quarantine);
        if inspect_entry(
            &self.directory,
            &path,
            &quarantine,
            self.identity.kind,
            None,
        )? != Some(self.identity)
        {
            let directory = TrustedDir {
                path: self.directory_path.clone(),
                file: self.directory.try_clone().map_err(|source| {
                    io_error(
                        "duplicate staging directory descriptor",
                        &self.directory_path,
                        source,
                    )
                })?,
            };
            match directory.move_no_replace(&quarantine, &directory, &self.name)? {
                MoveOutcome::Moved => {}
                MoveOutcome::DestinationExists => {
                    return Err(FsError::RecoveryConflict {
                        path: original_path,
                    });
                }
            }
            return Ok(RemoveOutcome::IdentityChanged);
        }
        let flags = if self.identity.kind == EntryKind::Directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        match fs::unlinkat(&self.directory, &quarantine, flags) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => {
                return Err(FsError::IdentityChanged {
                    path: original_path,
                });
            }
            Err(source) => return Err(io_error("remove staged entry", &path, source)),
        }
        durable_sync(&self.directory).map_err(|source| FsError::CommittedDurabilityUncertain {
            operation: "remove staged entry",
            path: path.clone(),
            source,
        })?;
        Ok(RemoveOutcome::Removed)
    }

    /// Removes a staged directory tree through its retained directory descriptor.
    ///
    /// The caller must retain the transaction or instance authority that made
    /// the staging rename exclusive. Descendants are never followed through
    /// symlinks, and the final parent unlink is attempted only while the staged
    /// name still identifies the retained root inode.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the staged entry is not a directory, its identity
    /// changes, an unsafe descendant is encountered, or synchronization fails.
    pub fn remove_tree(mut self) -> FsResult<RemoveOutcome> {
        let path = self.path();
        if self.identity.kind != EntryKind::Directory {
            return Err(FsError::UnsafeType {
                path,
                expected: EntryKind::Directory,
                actual: file_type_name(match self.identity.kind {
                    EntryKind::RegularFile => FileType::RegularFile,
                    EntryKind::Directory => FileType::Directory,
                    EntryKind::Socket => FileType::Socket,
                    EntryKind::Symlink => FileType::Symlink,
                }),
            });
        }
        let entry = self
            .entry_directory
            .take()
            .ok_or_else(|| FsError::IdentityChanged { path: path.clone() })?;
        if validate_fd_base(&entry, &path, EntryKind::Directory)? != self.identity {
            return Ok(RemoveOutcome::IdentityChanged);
        }
        remove_directory_contents(&entry, &path)?;
        if inspect_entry(
            &self.directory,
            &path,
            &self.name,
            EntryKind::Directory,
            None,
        )? != Some(self.identity)
        {
            return Ok(RemoveOutcome::IdentityChanged);
        }
        match fs::unlinkat(&self.directory, &self.name, AtFlags::REMOVEDIR) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => return Ok(RemoveOutcome::IdentityChanged),
            Err(source) => return Err(io_error("remove staged directory tree", &path, source)),
        }
        durable_sync(&self.directory).map_err(|source| FsError::CommittedDurabilityUncertain {
            operation: "remove staged directory tree",
            path,
            source,
        })?;
        Ok(RemoveOutcome::Removed)
    }

    /// Restores the staged entry without replacing an occupied destination.
    ///
    /// # Errors
    ///
    /// Returns [`FsError`] when the atomic move or synchronization fails.
    pub fn restore(&self, destination: impl AsRef<OsStr>) -> FsResult<MoveOutcome> {
        let destination = validate_component(destination.as_ref())?;
        let directory = TrustedDir {
            path: self.directory_path.clone(),
            file: self.directory.try_clone().map_err(|source| {
                io_error(
                    "duplicate staging directory descriptor",
                    &self.directory_path,
                    source,
                )
            })?,
        };
        let staged_path = self.directory_path.join(&self.name);
        let quarantine = move_to_random_quarantine(
            &self.directory,
            &self.directory_path,
            &self.name,
            ".pohunek-restore-",
        )?
        .ok_or_else(|| FsError::IdentityChanged {
            path: staged_path.clone(),
        })?;
        let quarantine_path = self.directory_path.join(&quarantine);
        if inspect_entry(
            &self.directory,
            &quarantine_path,
            &quarantine,
            self.identity.kind,
            None,
        )? != Some(self.identity)
        {
            match directory.move_no_replace(&quarantine, &directory, &self.name)? {
                MoveOutcome::Moved => {}
                MoveOutcome::DestinationExists => {
                    return Err(FsError::RecoveryConflict { path: staged_path });
                }
            }
            return Err(FsError::IdentityChanged { path: staged_path });
        }
        let outcome = directory.move_no_replace(&quarantine, &directory, destination)?;
        if outcome == MoveOutcome::Moved {
            let destination_path = self.directory_path.join(destination);
            if inspect_entry(
                &self.directory,
                &destination_path,
                destination,
                self.identity.kind,
                None,
            )? != Some(self.identity)
            {
                return Err(FsError::IdentityChanged {
                    path: destination_path,
                });
            }
        } else {
            match directory.move_no_replace(&quarantine, &directory, &self.name)? {
                MoveOutcome::Moved => {}
                MoveOutcome::DestinationExists => {
                    return Err(FsError::RecoveryConflict { path: staged_path });
                }
            }
        }
        Ok(outcome)
    }

    /// Moves the staged inode to another trusted directory without replacing
    /// an occupied destination.
    ///
    /// A destination collision leaves the entry under its current staging name,
    /// so the caller can restore the original source name explicitly.
    ///
    /// # Errors
    ///
    /// Returns [`StagedMoveError`] with the namespace commit state when identity
    /// validation, movement, durability, or recovery fails.
    #[expect(
        clippy::too_many_lines,
        reason = "the staged-move commit and recovery states remain contiguous for auditability"
    )]
    pub fn move_to(
        &mut self,
        destination_dir: &TrustedDir,
        destination: impl AsRef<OsStr>,
    ) -> Result<MoveOutcome, StagedMoveError> {
        let destination =
            validate_component(destination.as_ref()).map_err(StagedMoveError::BeforeCommit)?;
        let source_dir = TrustedDir {
            path: self.directory_path.clone(),
            file: self
                .directory
                .try_clone()
                .map_err(|source| {
                    io_error(
                        "duplicate staging directory descriptor",
                        &self.directory_path,
                        source,
                    )
                })
                .map_err(StagedMoveError::BeforeCommit)?,
        };
        let original_name = self.name.clone();
        let staged_path = self.directory_path.join(&self.name);
        let quarantine = rename_to_random_quarantine(
            &self.directory,
            &self.directory_path,
            &self.name,
            ".pohunek-move-",
        )
        .map_err(StagedMoveError::BeforeCommit)?
        .ok_or_else(|| FsError::IdentityChanged {
            path: staged_path.clone(),
        })
        .map_err(StagedMoveError::BeforeCommit)?;
        let quarantine_path = self.directory_path.join(&quarantine);
        self.name.clone_from(&quarantine);
        if let Err(source) = durable_sync(&self.directory) {
            let error = FsError::CommittedDurabilityUncertain {
                operation: "move entry to quarantine",
                path: quarantine_path.clone(),
                source,
            };
            return Err(self.restore_move_staging(&source_dir, original_name, error));
        }
        let current = inspect_entry(
            &self.directory,
            &quarantine_path,
            &quarantine,
            self.identity.kind,
            None,
        );
        let current = match current {
            Ok(current) => current,
            Err(error) => {
                return Err(self.restore_move_staging(&source_dir, original_name, error));
            }
        };
        if current != Some(self.identity) {
            match source_dir
                .move_no_replace(&self.name, &source_dir, &original_name)
                .map_err(|source| StagedMoveError::RecoveryRequired {
                    path: quarantine_path.clone(),
                    source,
                })? {
                MoveOutcome::Moved => self.name = original_name,
                MoveOutcome::DestinationExists => {
                    return Err(StagedMoveError::RecoveryRequired {
                        path: quarantine_path,
                        source: FsError::RecoveryConflict { path: staged_path },
                    });
                }
            }
            return Err(StagedMoveError::BeforeCommit(FsError::IdentityChanged {
                path: staged_path,
            }));
        }
        let outcome = match source_dir.move_no_replace(&self.name, destination_dir, destination) {
            Ok(outcome) => outcome,
            Err(source @ FsError::CommittedDurabilityUncertain { .. }) => {
                return Err(StagedMoveError::Committed {
                    path: destination_dir.path.join(destination),
                    source,
                });
            }
            Err(error) => {
                return Err(self.restore_move_staging(&source_dir, original_name, error));
            }
        };
        if outcome == MoveOutcome::DestinationExists {
            match source_dir
                .move_no_replace(&self.name, &source_dir, &original_name)
                .map_err(|source| StagedMoveError::RecoveryRequired {
                    path: self.path(),
                    source,
                })? {
                MoveOutcome::Moved => self.name = original_name,
                MoveOutcome::DestinationExists => {
                    return Err(StagedMoveError::RecoveryRequired {
                        path: self.path(),
                        source: FsError::RecoveryConflict { path: staged_path },
                    });
                }
            }
            return Ok(outcome);
        }
        let destination_path = destination_dir.path.join(destination);
        if inspect_entry(
            &destination_dir.file,
            &destination_path,
            destination,
            self.identity.kind,
            None,
        )
        .map_err(|source| StagedMoveError::Committed {
            path: destination_path.clone(),
            source,
        })? != Some(self.identity)
        {
            return Err(StagedMoveError::Committed {
                path: destination_path.clone(),
                source: FsError::IdentityChanged {
                    path: destination_path,
                },
            });
        }
        Ok(MoveOutcome::Moved)
    }

    fn restore_move_staging(
        &mut self,
        source_dir: &TrustedDir,
        original_name: OsString,
        original_error: FsError,
    ) -> StagedMoveError {
        let quarantine_path = self.path();
        let staged_path = self.directory_path.join(&original_name);
        match source_dir.move_no_replace(&self.name, source_dir, &original_name) {
            Ok(MoveOutcome::Moved) => {
                self.name = original_name;
                StagedMoveError::BeforeCommit(original_error)
            }
            Ok(MoveOutcome::DestinationExists) => StagedMoveError::RecoveryRequired {
                path: quarantine_path,
                source: FsError::RecoveryConflict { path: staged_path },
            },
            Err(source @ FsError::CommittedDurabilityUncertain { .. }) => {
                match inspect_entry(
                    &source_dir.file,
                    &staged_path,
                    &original_name,
                    self.identity.kind,
                    None,
                ) {
                    Ok(Some(identity)) if identity == self.identity => {
                        self.name = original_name;
                        StagedMoveError::BeforeCommit(source)
                    }
                    _ => StagedMoveError::RecoveryRequired {
                        path: quarantine_path,
                        source,
                    },
                }
            }
            Err(source) => StagedMoveError::RecoveryRequired {
                path: quarantine_path,
                source,
            },
        }
    }
}

fn remove_directory_contents(directory: &File, directory_path: &Path) -> FsResult<()> {
    let entries = fs::Dir::read_from(directory).map_err(|source| {
        io_error(
            "open staged directory tree enumeration",
            directory_path,
            source,
        )
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            io_error("enumerate staged directory tree", directory_path, source)
        })?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(bytes).to_os_string());
        }
    }
    for name in names {
        let path = directory_path.join(&name);
        let stat = fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|source| io_error("inspect staged tree entry", &path, source))?;
        let actual = FileType::from_raw_mode(stat.st_mode);
        let kind = match actual {
            FileType::RegularFile => EntryKind::RegularFile,
            FileType::Directory => EntryKind::Directory,
            FileType::Socket => EntryKind::Socket,
            FileType::Symlink => EntryKind::Symlink,
            _ => {
                return Err(FsError::UnsafeType {
                    path,
                    expected: EntryKind::RegularFile,
                    actual: file_type_name(actual),
                });
            }
        };
        let identity = validate_stat(&stat, &path, kind, None)?;
        if kind == EntryKind::Directory {
            let child = File::from(
                open_directory_at(directory, &name)
                    .map_err(|source| io_error("open staged tree directory", &path, source))?,
            );
            if validate_fd_base(&child, &path, EntryKind::Directory)? != identity {
                return Err(FsError::IdentityChanged { path });
            }
            remove_directory_contents(&child, &path)?;
        }
        if inspect_entry(directory, &path, &name, kind, None)? != Some(identity) {
            return Err(FsError::IdentityChanged { path });
        }
        let flags = if kind == EntryKind::Directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        fs::unlinkat(directory, &name, flags)
            .map_err(|source| io_error("remove staged tree entry", &path, source))?;
    }
    durable_sync(directory)
        .map_err(|source| io_error("synchronize staged directory tree", directory_path, source))
}

fn move_to_random_quarantine(
    directory: &File,
    directory_path: &Path,
    source_name: &OsStr,
    prefix: &str,
) -> FsResult<Option<OsString>> {
    let quarantine = rename_to_random_quarantine(directory, directory_path, source_name, prefix)?;
    if let Some(name) = quarantine.as_ref() {
        durable_sync(directory).map_err(|source| FsError::CommittedDurabilityUncertain {
            operation: "move entry to quarantine",
            path: directory_path.join(name),
            source,
        })?;
    }
    Ok(quarantine)
}

fn rename_to_random_quarantine(
    directory: &File,
    directory_path: &Path,
    source_name: &OsStr,
    prefix: &str,
) -> FsResult<Option<OsString>> {
    for _ in 0..STAGING_NAME_ATTEMPTS {
        let quarantine = random_staging_name(prefix)?;
        match fs::renameat_with(
            directory,
            source_name,
            directory,
            &quarantine,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => return Ok(Some(quarantine)),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(rustix::io::Errno::EXIST) => {}
            Err(source) => {
                return Err(io_error(
                    "move entry to quarantine",
                    &directory_path.join(source_name),
                    source,
                ));
            }
        }
    }
    Err(FsError::Io {
        operation: "allocate quarantine name",
        path: directory_path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "quarantine name collision budget exhausted",
        ),
    })
}

/// Durably synchronizes an open file descriptor.
///
/// On Apple platforms this uses `F_FULLFSYNC`; other Unix platforms use `fsync`.
///
/// # Errors
///
/// Returns [`FsError`] when the operating system cannot complete synchronization.
pub fn sync_file(file: &File) -> FsResult<()> {
    durable_sync(file).map_err(|source| FsError::Io {
        operation: "durably synchronize file",
        path: PathBuf::from("<open file descriptor>"),
        source,
    })
}

fn absolute_components(path: &Path) -> FsResult<Vec<&OsStr>> {
    let raw = path.as_os_str().as_bytes();
    if raw == b"/"
        || raw.windows(2).any(|window| window == b"//")
        || raw.windows(3).any(|window| window == b"/./")
        || raw.ends_with(b"/.")
        || raw.ends_with(b"/")
    {
        return Err(FsError::InvalidAbsolutePath {
            path: path.to_path_buf(),
        });
    }
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(FsError::InvalidAbsolutePath {
            path: path.to_path_buf(),
        });
    }
    components
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(FsError::InvalidAbsolutePath {
                path: path.to_path_buf(),
            }),
        })
        .collect()
}

fn validate_component(name: &OsStr) -> FsResult<&OsStr> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        Ok(name)
    } else {
        Err(FsError::InvalidComponent {
            name: name.to_os_string(),
        })
    }
}

fn random_staging_name(prefix: &str) -> FsResult<OsString> {
    let mut random = [0_u8; STAGING_RANDOM_BYTES];
    getrandom::getrandom(&mut random).map_err(|source| FsError::Io {
        operation: "generate trusted staging name",
        path: PathBuf::from("<random staging name>"),
        source: io::Error::other(source.to_string()),
    })?;
    let mut name = String::with_capacity(prefix.len() + random.len() * 2);
    name.push_str(prefix);
    for byte in random {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing hexadecimal to String cannot fail");
    }
    Ok(name.into())
}

fn validate_mode(mode: u32) -> FsResult<()> {
    if mode & !MODE_MASK == 0 {
        Ok(())
    } else {
        Err(FsError::InvalidMode { mode })
    }
}

#[cfg(target_os = "linux")]
fn mode_from_raw(mode: u32) -> Mode {
    Mode::from_bits_truncate(mode)
}

#[cfg(not(target_os = "linux"))]
fn mode_from_raw(mode: u32) -> Mode {
    let native = mode
        .try_into()
        .expect("validated Unix permission bits fit the native mode type");
    Mode::from_bits_truncate(native)
}

fn open_directory_at(
    parent: &File,
    name: &OsStr,
) -> Result<rustix::fd::OwnedFd, rustix::io::Errno> {
    fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
}

#[cfg(target_os = "linux")]
fn bind_unix_listener_with_mode(
    path: &Path,
    _mode: u32,
) -> io::Result<std::os::unix::net::UnixListener> {
    std::os::unix::net::UnixListener::bind(path)
}

#[cfg(not(target_os = "linux"))]
fn bind_unix_listener_with_mode(
    path: &Path,
    _mode: u32,
) -> io::Result<std::os::unix::net::UnixListener> {
    std::os::unix::net::UnixListener::bind(path)
}

#[cfg(target_os = "linux")]
fn set_random_staging_mode_at(
    directory: &File,
    name: &OsStr,
    path: &Path,
    expected: EntryIdentity,
    mode: u32,
) -> FsResult<()> {
    set_entry_mode_at(directory, name, path, expected, mode)
}

#[cfg(not(target_os = "linux"))]
fn set_random_staging_mode_at(
    directory: &File,
    name: &OsStr,
    path: &Path,
    expected: EntryIdentity,
    mode: u32,
) -> FsResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if inspect_entry(directory, path, name, EntryKind::Socket, None)? != Some(expected) {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    // Darwin cannot open a pathname socket for fchmod. The random name lives
    // inside a retained owner-private directory, and descriptor-relative
    // identity checks fence the pathname mutation before atomic publication.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| io_error("set staged Unix socket mode", path, source))?;
    if inspect_entry(directory, path, name, EntryKind::Socket, Some(mode))? != Some(expected) {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_random_staging_directory_mode(
    directory: &File,
    name: &OsStr,
    path: &Path,
    mode: u32,
) -> FsResult<()> {
    let identity =
        inspect_entry(directory, path, name, EntryKind::Directory, None)?.ok_or_else(|| {
            FsError::IdentityChanged {
                path: path.to_path_buf(),
            }
        })?;
    set_random_staging_mode_at(directory, name, path, identity, mode)
}

#[cfg(not(target_os = "linux"))]
fn prepare_random_staging_directory_mode(
    _directory: &File,
    _name: &OsStr,
    _path: &Path,
    _mode: u32,
) -> FsResult<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_entry_mode_at(
    directory: &File,
    name: &OsStr,
    path: &Path,
    expected: EntryIdentity,
    mode: u32,
) -> FsResult<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::PermissionsExt as _;

    let descriptor = File::from(
        fs::openat(
            directory,
            name,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| io_error("open inode-bound entry for chmod", path, source))?,
    );
    let opened = validate_fd(&descriptor, path, expected.kind, None)?;
    if opened != expected {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    // The descriptor remains open, so its procfs link cannot be redirected by
    // pathname replacement or descriptor-number reuse during this operation.
    let proc_path = PathBuf::from("/proc/self/fd").join(descriptor.as_raw_fd().to_string());
    std::fs::set_permissions(&proc_path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| io_error("set inode-bound entry mode", path, source))?;
    let changed = validate_fd(&descriptor, path, expected.kind, Some(mode))?;
    if changed != expected {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_entry_mode_at(
    directory: &File,
    name: &OsStr,
    path: &Path,
    expected: EntryIdentity,
    mode: u32,
) -> FsResult<()> {
    if expected.kind == EntryKind::Socket {
        return Err(FsError::Io {
            operation: "set socket mode through a retained descriptor",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::Unsupported,
                "socket mode must be established atomically at bind time",
            ),
        });
    }
    let flags = OFlags::RDONLY
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | if expected.kind == EntryKind::Directory {
            OFlags::DIRECTORY
        } else {
            OFlags::empty()
        };
    let descriptor = File::from(
        fs::openat(directory, name, flags, Mode::empty())
            .map_err(|source| io_error("open inode-bound entry for chmod", path, source))?,
    );
    let opened = validate_fd(&descriptor, path, expected.kind, None)?;
    if opened != expected {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    fs::fchmod(&descriptor, mode_from_raw(mode))
        .map_err(|source| io_error("set inode-bound entry mode", path, source))?;
    let changed = validate_fd(&descriptor, path, expected.kind, Some(mode))?;
    if changed != expected {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn inspect_entry(
    directory: &File,
    path: &Path,
    name: &OsStr,
    kind: EntryKind,
    mode: Option<u32>,
) -> FsResult<Option<EntryIdentity>> {
    let stat = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(source) => return Err(io_error("inspect trusted entry", path, source)),
    };
    validate_stat(&stat, path, kind, mode).map(Some)
}

fn inspect_entry_generation(
    directory: &File,
    path: &Path,
    name: &OsStr,
    kind: EntryKind,
    mode: Option<u32>,
) -> FsResult<Option<EntryGeneration>> {
    let stat = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(source) => return Err(io_error("inspect trusted entry generation", path, source)),
    };
    let identity = validate_stat(&stat, path, kind, mode)?;
    Ok(Some(EntryGeneration {
        identity,
        size: i128::from(stat.st_size),
        change_seconds: i128::from(stat.st_ctime),
        change_nanoseconds: i128::from(stat.st_ctime_nsec),
    }))
}

fn require_entry_generation(
    directory: &File,
    path: &Path,
    name: &OsStr,
    kind: EntryKind,
    mode: Option<u32>,
    expected: EntryIdentity,
) -> FsResult<EntryGeneration> {
    let generation =
        inspect_entry_generation(directory, path, name, kind, mode)?.ok_or_else(|| {
            FsError::IdentityChanged {
                path: path.to_path_buf(),
            }
        })?;
    if generation.identity != expected {
        return Err(FsError::IdentityChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(generation)
}

fn validate_fd(
    file: &File,
    path: &Path,
    kind: EntryKind,
    mode: Option<u32>,
) -> FsResult<EntryIdentity> {
    let stat =
        fs::fstat(file).map_err(|source| io_error("inspect trusted descriptor", path, source))?;
    validate_stat(&stat, path, kind, mode)
}

fn validate_fd_base(file: &File, path: &Path, kind: EntryKind) -> FsResult<EntryIdentity> {
    let stat =
        fs::fstat(file).map_err(|source| io_error("inspect trusted descriptor", path, source))?;
    let actual = FileType::from_raw_mode(stat.st_mode);
    if !kind.matches(actual) {
        return Err(FsError::UnsafeType {
            path: path.to_path_buf(),
            expected: kind,
            actual: file_type_name(actual),
        });
    }
    if stat.st_nlink == 0 {
        return Err(FsError::UnsafeLinkCount {
            path: path.to_path_buf(),
            actual: 0,
        });
    }
    Ok(identity_from_stat(&stat, kind))
}

fn validate_fd_owner(file: &File, path: &Path) -> FsResult<()> {
    let stat =
        fs::fstat(file).map_err(|source| io_error("inspect trusted descriptor", path, source))?;
    let expected = rustix::process::geteuid().as_raw();
    let actual = stat_uid(&stat);
    if actual == expected {
        Ok(())
    } else {
        Err(FsError::UnsafeOwner {
            path: path.to_path_buf(),
            actual,
            expected,
        })
    }
}

fn validate_stat(
    stat: &Stat,
    path: &Path,
    kind: EntryKind,
    mode: Option<u32>,
) -> FsResult<EntryIdentity> {
    let actual_type = FileType::from_raw_mode(stat.st_mode);
    if !kind.matches(actual_type) {
        return Err(FsError::UnsafeType {
            path: path.to_path_buf(),
            expected: kind,
            actual: file_type_name(actual_type),
        });
    }
    let expected_uid = rustix::process::geteuid().as_raw();
    let actual_uid = stat_uid(stat);
    if actual_uid != expected_uid {
        return Err(FsError::UnsafeOwner {
            path: path.to_path_buf(),
            actual: actual_uid,
            expected: expected_uid,
        });
    }
    if let Some(expected) = mode {
        let actual = stat_mode(stat);
        if actual != expected {
            return Err(FsError::UnsafeMode {
                path: path.to_path_buf(),
                actual,
                expected,
            });
        }
    }
    let links = stat_links(stat);
    let links_are_safe = if kind == EntryKind::RegularFile {
        links == 1
    } else {
        links != 0
    };
    if !links_are_safe {
        return Err(FsError::UnsafeLinkCount {
            path: path.to_path_buf(),
            actual: links,
        });
    }
    Ok(identity_from_stat(stat, kind))
}

fn identity_from_stat(stat: &Stat, kind: EntryKind) -> EntryIdentity {
    EntryIdentity {
        device: stat_device(stat),
        inode: stat_inode(stat),
        kind,
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn stat_uid(stat: &Stat) -> u32 {
    stat.st_uid
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn stat_uid(stat: &Stat) -> u32 {
    stat.st_uid
        .try_into()
        .expect("native uid fits the portable identity type")
}

#[cfg(target_os = "linux")]
fn stat_mode(stat: &Stat) -> u32 {
    Mode::from_raw_mode(stat.st_mode).as_raw_mode() & MODE_MASK
}

#[cfg(not(target_os = "linux"))]
fn stat_mode(stat: &Stat) -> u32 {
    u32::from(Mode::from_raw_mode(stat.st_mode).as_raw_mode()) & MODE_MASK
}

#[cfg(target_os = "linux")]
fn stat_links(stat: &Stat) -> u64 {
    stat.st_nlink
}

#[cfg(not(target_os = "linux"))]
fn stat_links(stat: &Stat) -> u64 {
    stat.st_nlink.into()
}

#[cfg(target_os = "linux")]
fn stat_device(stat: &Stat) -> u64 {
    stat.st_dev
}

#[cfg(not(target_os = "linux"))]
fn stat_device(stat: &Stat) -> u64 {
    stat.st_dev
        .try_into()
        .expect("native device number fits the portable identity type")
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn stat_inode(stat: &Stat) -> u64 {
    stat.st_ino
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn stat_inode(stat: &Stat) -> u64 {
    stat.st_ino.into()
}

fn file_type_name(kind: FileType) -> &'static str {
    match kind {
        FileType::RegularFile => "regular file",
        FileType::Directory => "directory",
        FileType::Symlink => "symbolic link",
        FileType::Fifo => "FIFO",
        FileType::Socket => "socket",
        FileType::CharacterDevice => "character device",
        FileType::BlockDevice => "block device",
        FileType::Unknown => "unknown entry",
    }
}

fn lock_nonblocking(file: &File, path: &Path) -> FsResult<()> {
    match fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(source) if source == rustix::io::Errno::AGAIN => Err(FsError::LockContended {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(io_error("acquire advisory lock", path, source)),
    }
}

#[cfg(target_vendor = "apple")]
fn durable_sync(file: &File) -> io::Result<()> {
    if let Some(error) = injected_sync_failure() {
        return Err(error);
    }
    fs::fcntl_fullfsync(file).map_err(Into::into)
}

#[cfg(not(target_vendor = "apple"))]
fn durable_sync(file: &File) -> io::Result<()> {
    if let Some(error) = injected_sync_failure() {
        return Err(error);
    }
    fs::fsync(file).map_err(Into::into)
}

#[cfg(test)]
std::thread_local! {
    static NEXT_SYNC_FAILURE: std::cell::RefCell<Option<(usize, io::ErrorKind)>> = const {
        std::cell::RefCell::new(None)
    };
    static NEXT_MOVE_FAILURE: std::cell::RefCell<Option<io::ErrorKind>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn injected_move_failure() -> Option<io::Error> {
    NEXT_MOVE_FAILURE.with(|slot| slot.borrow_mut().take().map(io::Error::from))
}

#[cfg(not(test))]
const fn injected_move_failure() -> Option<io::Error> {
    None
}

#[cfg(test)]
fn injected_sync_failure() -> Option<io::Error> {
    NEXT_SYNC_FAILURE.with(|slot| {
        let mut pending = slot.borrow_mut();
        let (remaining, kind) = pending.as_mut()?;
        if *remaining == 0 {
            let kind = *kind;
            pending.take();
            Some(io::Error::from(kind))
        } else {
            *remaining -= 1;
            None
        }
    })
}

#[cfg(not(test))]
const fn injected_sync_failure() -> Option<io::Error> {
    None
}

fn io_error(operation: &'static str, path: &Path, source: impl Into<io::Error>) -> FsError {
    FsError::Io {
        operation,
        path: path.to_path_buf(),
        source: source.into(),
    }
}

fn committed_error(operation: &'static str, path: &Path, error: FsError) -> FsError {
    let source = match error {
        FsError::Io { source, .. } | FsError::CommittedDurabilityUncertain { source, .. } => source,
        other => io::Error::other(other),
    };
    FsError::CommittedDurabilityUncertain {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::sync::{Arc, Barrier};

    const DIRECTORY_MODE: u32 = 0o700;
    const FILE_MODE: u32 = 0o600;

    fn trusted_root() -> (tempfile::TempDir, TrustedDir) {
        let temporary = tempfile::tempdir().expect("create temporary directory");
        let canonical_path = fs::canonicalize(temporary.path()).expect("canonicalize test root");
        fs::set_permissions(&canonical_path, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set temporary directory permissions");
        let trusted = TrustedDir::open_absolute(&canonical_path, DIRECTORY_MODE)
            .expect("open trusted temporary directory");
        (temporary, trusted)
    }

    #[test]
    fn absolute_and_child_directories_reject_symlinks_and_wrong_modes() {
        let (temporary, trusted) = trusted_root();
        let child = trusted
            .open_or_create_child("child", DIRECTORY_MODE)
            .expect("create trusted child");
        assert_eq!(
            fs::metadata(temporary.path().join("child"))
                .expect("inspect child")
                .mode()
                & MODE_MASK,
            DIRECTORY_MODE
        );
        drop(child);

        std::os::unix::fs::symlink("child", temporary.path().join("link"))
            .expect("create directory symlink");
        trusted
            .open_child("link", DIRECTORY_MODE)
            .expect_err("directory symlink must be rejected");
        TrustedDir::open_absolute(temporary.path().join("link"), DIRECTORY_MODE)
            .expect_err("absolute symlink component must be rejected");

        trusted
            .open_or_create_child("space and Unicode ž", DIRECTORY_MODE)
            .expect("create portable non-ASCII child");

        fs::set_permissions(
            temporary.path().join("child"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("widen child mode");
        assert!(matches!(
            trusted.open_child("child", DIRECTORY_MODE),
            Err(FsError::UnsafeMode { .. })
        ));
    }

    #[test]
    fn absolute_tree_rejects_an_unsafe_application_owned_ancestor() {
        let (_temporary, trusted) = trusted_root();
        let ancestor = trusted.path().join("ancestor");
        let leaf = ancestor.join("leaf");
        fs::create_dir(&ancestor).expect("create ancestor");
        fs::create_dir(&leaf).expect("create leaf");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o775))
            .expect("make ancestor group-writable");
        fs::set_permissions(&leaf, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set private leaf");

        assert!(matches!(
            TrustedDir::open_absolute(&leaf, DIRECTORY_MODE),
            Err(FsError::UnsafeMode { path, .. }) if path == ancestor
        ));
    }

    #[test]
    fn absolute_tree_accepts_safe_shared_intermediates_below_a_private_home() {
        let (_temporary, trusted) = trusted_root();
        let home = trusted.path().join("home");
        let local = home.join(".local");
        let share = local.join("share");
        let application = share.join("pohunek");
        fs::create_dir(&home).expect("create private home");
        fs::create_dir(&local).expect("create shared local directory");
        fs::create_dir(&share).expect("create shared data directory");
        fs::create_dir(&application).expect("create private application directory");
        fs::set_permissions(&home, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set private home mode");
        fs::set_permissions(&local, fs::Permissions::from_mode(0o755))
            .expect("set local directory mode");
        fs::set_permissions(&share, fs::Permissions::from_mode(0o755))
            .expect("set data directory mode");
        fs::set_permissions(&application, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set private application mode");

        TrustedDir::open_absolute(&application, DIRECTORY_MODE)
            .expect("safe XDG intermediates below a private home are accepted");
    }

    #[test]
    fn directory_sync_failure_is_explicit_before_the_final_name_is_published() {
        let (temporary, trusted) = trusted_root();
        NEXT_SYNC_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some((0, io::ErrorKind::StorageFull));
        });

        assert!(matches!(
            trusted.open_or_create_child("child", DIRECTORY_MODE),
            Err(FsError::CommittedDurabilityUncertain { .. })
        ));
        assert!(!temporary.path().join("child").exists());
    }

    #[test]
    fn lock_is_exclusive_across_separate_directory_descriptors() {
        let (_temporary, first) = trusted_root();
        let second = TrustedDir::open_absolute(&first.path, DIRECTORY_MODE)
            .expect("open second directory descriptor");
        let lock = first
            .acquire_lock("state.lock", FILE_MODE)
            .expect("acquire first lock");
        assert!(matches!(
            second.acquire_lock("state.lock", FILE_MODE),
            Err(FsError::LockContended { .. })
        ));
        drop(lock);
        second
            .acquire_lock("state.lock", FILE_MODE)
            .expect("acquire released lock");
    }

    #[test]
    fn atomic_replace_is_durable_and_rejects_untrusted_destination() {
        let (temporary, trusted) = trusted_root();
        trusted
            .replace_file("record", ".record.tmp", b"first", FILE_MODE)
            .expect("install first record");
        trusted
            .replace_file("record", ".record.next", b"second", FILE_MODE)
            .expect("replace record");
        assert_eq!(
            fs::read(temporary.path().join("record")).expect("read record"),
            b"second"
        );
        assert_eq!(
            fs::metadata(temporary.path().join("record"))
                .expect("inspect record")
                .mode()
                & MODE_MASK,
            FILE_MODE
        );

        fs::set_permissions(
            temporary.path().join("record"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("widen record mode");
        assert!(matches!(
            trusted.replace_file("record", ".record.bad", b"bad", FILE_MODE),
            Err(AtomicReplaceError::BeforeCommit(FsError::UnsafeMode { .. }))
        ));
        assert!(!temporary.path().join(".record.bad").exists());
    }

    #[test]
    fn atomic_replace_reports_post_commit_sync_failure_without_losing_destination() {
        let (temporary, trusted) = trusted_root();
        trusted
            .replace_file("record", ".record.initial", b"old", FILE_MODE)
            .expect("install initial record");
        NEXT_SYNC_FAILURE.with(|slot| {
            // The prepared file sync succeeds; the containing-directory sync
            // immediately after the one atomic rename fails.
            *slot.borrow_mut() = Some((1, io::ErrorKind::StorageFull));
        });

        assert!(matches!(
            trusted.replace_file("record", ".record.next", b"new", FILE_MODE),
            Err(AtomicReplaceError::CommittedDurabilityUncertain(_))
        ));
        assert_eq!(
            fs::read(temporary.path().join("record")).expect("read committed record"),
            b"new"
        );
        assert!(!temporary.path().join(".record.next").exists());
    }

    #[test]
    fn trusted_read_is_bounded_and_rejects_symlinks() {
        let (temporary, trusted) = trusted_root();
        let record = temporary.path().join("record");
        fs::write(&record, b"bounded").expect("write record");
        fs::set_permissions(&record, fs::Permissions::from_mode(FILE_MODE))
            .expect("set record mode");
        assert_eq!(
            trusted
                .read_file("record", FILE_MODE, 7)
                .expect("read bounded record"),
            b"bounded"
        );
        assert!(matches!(
            trusted.read_file("record", FILE_MODE, 6),
            Err(FsError::FileTooLarge { max_bytes: 6, .. })
        ));
        std::os::unix::fs::symlink("record", temporary.path().join("record-link"))
            .expect("create record symlink");
        trusted
            .read_file("record-link", FILE_MODE, 7)
            .expect_err("symlink read must fail closed");
    }

    #[test]
    fn move_no_replace_reports_collision_without_overwrite() {
        let (temporary, trusted) = trusted_root();
        fs::write(temporary.path().join("source"), b"source").expect("write source");
        fs::write(temporary.path().join("destination"), b"destination").expect("write destination");
        assert_eq!(
            trusted
                .move_no_replace("source", &trusted, "destination")
                .expect("attempt no-replace move"),
            MoveOutcome::DestinationExists
        );
        assert_eq!(
            fs::read(temporary.path().join("source")).expect("read source"),
            b"source"
        );
        assert_eq!(
            fs::read(temporary.path().join("destination")).expect("read destination"),
            b"destination"
        );
    }

    #[test]
    fn concurrent_no_replace_move_has_exactly_one_winner() {
        let (temporary, trusted) = trusted_root();
        for (name, bytes) in [("source-a", b"alpha"), ("source-b", b"bravo")] {
            fs::write(temporary.path().join(name), bytes).expect("write competing source");
        }
        let barrier = Arc::new(Barrier::new(2));
        let handles = ["source-a", "source-b"].map(|source| {
            let barrier = Arc::clone(&barrier);
            let root = trusted.path().to_path_buf();
            std::thread::spawn(move || {
                let directory = TrustedDir::open_absolute(&root, DIRECTORY_MODE)
                    .expect("open competing directory descriptor");
                barrier.wait();
                directory
                    .move_no_replace(source, &directory, "winner")
                    .expect("run competing no-replace move")
            })
        });
        let outcomes = handles.map(|handle| handle.join().expect("join competing move"));
        assert_eq!(
            outcomes
                .into_iter()
                .filter(|outcome| *outcome == MoveOutcome::Moved)
                .count(),
            1
        );
        let winner = fs::read(temporary.path().join("winner")).expect("read winning entry");
        assert!(winner == b"alpha" || winner == b"bravo");
        assert_eq!(
            ["source-a", "source-b"]
                .into_iter()
                .filter(|source| temporary.path().join(source).exists())
                .count(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_case_alias_cannot_replace_an_existing_destination() {
        let (temporary, trusted) = trusted_root();
        fs::write(temporary.path().join("source ž"), b"source").expect("write Unicode source");
        fs::write(temporary.path().join("CaseTarget"), b"destination")
            .expect("write mixed-case destination");
        assert_eq!(
            trusted
                .move_no_replace("source ž", &trusted, "casetarget")
                .expect("attempt case-alias no-replace move"),
            MoveOutcome::DestinationExists,
            "native macOS CI must exercise a case-insensitive APFS volume"
        );
        assert_eq!(
            fs::read(temporary.path().join("CaseTarget")).expect("read preserved destination"),
            b"destination"
        );
    }

    #[test]
    fn staged_removal_is_bound_to_the_expected_inode() {
        let (temporary, trusted) = trusted_root();
        let source = temporary.path().join("source");
        fs::write(&source, b"source").expect("write source");
        fs::set_permissions(&source, fs::Permissions::from_mode(FILE_MODE))
            .expect("set source mode");
        let identity = trusted
            .entry_identity("source", EntryKind::RegularFile)
            .expect("inspect source")
            .expect("source exists");
        let StageOutcome::Staged(staged) = trusted
            .stage("source", ".source.staged", identity)
            .expect("stage source")
        else {
            panic!("source must be staged");
        };
        fs::remove_file(temporary.path().join(".source.staged")).expect("remove staged inode");
        fs::write(temporary.path().join(".source.staged"), b"replacement")
            .expect("write replacement");
        fs::set_permissions(
            temporary.path().join(".source.staged"),
            fs::Permissions::from_mode(FILE_MODE),
        )
        .expect("set replacement mode");
        assert_eq!(
            staged.remove().expect("protect replacement"),
            RemoveOutcome::IdentityChanged
        );
        assert_eq!(
            fs::read(temporary.path().join(".source.staged")).expect("read replacement"),
            b"replacement"
        );
    }

    #[test]
    fn staged_move_restores_its_tracked_name_after_pre_commit_failure() {
        let (source_root, source_directory) = trusted_root();
        let (destination_root, destination_directory) = trusted_root();
        let source = source_root.path().join("source");
        fs::write(&source, b"source").expect("write source");
        fs::set_permissions(&source, fs::Permissions::from_mode(FILE_MODE))
            .expect("set source mode");
        let identity = source_directory
            .entry_identity("source", EntryKind::RegularFile)
            .expect("inspect source")
            .expect("source exists");
        let StageOutcome::Staged(mut staged) = source_directory
            .stage("source", ".source.staged", identity)
            .expect("stage source")
        else {
            panic!("source must be staged");
        };
        NEXT_MOVE_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some(io::ErrorKind::CrossesDevices);
        });

        assert!(matches!(
            staged.move_to(&destination_directory, "destination"),
            Err(StagedMoveError::BeforeCommit(FsError::Io { source, .. }))
                if source.kind() == io::ErrorKind::CrossesDevices
        ));
        assert_eq!(
            fs::read(staged.path()).expect("read restored staging entry"),
            b"source"
        );
        assert!(!destination_root.path().join("destination").exists());
        assert!(source_root
            .path()
            .read_dir()
            .expect("enumerate source root")
            .all(|entry| !entry
                .expect("source entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".pohunek-move-")));
    }

    #[test]
    fn staged_move_reports_the_destination_after_post_commit_failure() {
        let (source_root, source_directory) = trusted_root();
        let (_destination_root, destination_directory) = trusted_root();
        let source = source_root.path().join("source");
        fs::write(&source, b"source").expect("write source");
        fs::set_permissions(&source, fs::Permissions::from_mode(FILE_MODE))
            .expect("set source mode");
        let identity = source_directory
            .entry_identity("source", EntryKind::RegularFile)
            .expect("inspect source")
            .expect("source exists");
        let StageOutcome::Staged(mut staged) = source_directory
            .stage("source", ".source.staged", identity)
            .expect("stage source")
        else {
            panic!("source must be staged");
        };
        NEXT_SYNC_FAILURE.with(|slot| {
            // Quarantine synchronization succeeds; source-directory
            // synchronization after the destination rename fails.
            *slot.borrow_mut() = Some((1, io::ErrorKind::StorageFull));
        });
        let destination = destination_directory.path().join("destination");

        assert!(matches!(
            staged.move_to(&destination_directory, "destination"),
            Err(StagedMoveError::Committed { path, .. }) if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("read committed entry"),
            b"source"
        );
        assert!(!staged.path().exists());
    }

    #[test]
    fn hard_linked_regular_file_is_not_trusted() {
        let (temporary, trusted) = trusted_root();
        let first = temporary.path().join("first");
        fs::write(&first, b"record").expect("write first link");
        fs::hard_link(&first, temporary.path().join("second")).expect("create second link");
        assert!(matches!(
            trusted.entry_identity("first", EntryKind::RegularFile),
            Err(FsError::UnsafeLinkCount { actual: 2, .. })
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_path_mode_change_is_bound_to_the_pathname_inode() {
        let (temporary, trusted) = trusted_root();
        let socket_path = temporary.path().join("control.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind pathname Unix socket");
        let identity = trusted
            .entry_identity("control.sock", EntryKind::Socket)
            .expect("inspect socket pathname")
            .expect("socket pathname exists");
        trusted
            .set_entry_mode("control.sock", identity, FILE_MODE)
            .expect("set socket pathname mode");
        assert_eq!(
            fs::symlink_metadata(&socket_path)
                .expect("inspect socket pathname")
                .mode()
                & MODE_MASK,
            FILE_MODE
        );
    }

    #[test]
    fn hostile_regular_file_is_rejected_as_a_socket_object() {
        let (temporary, trusted) = trusted_root();
        let path = temporary.path().join("control.sock");
        fs::write(&path, b"not a socket").expect("write hostile socket object");
        fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE))
            .expect("set hostile object mode");

        assert!(matches!(
            trusted.entry_identity_with_mode("control.sock", EntryKind::Socket, FILE_MODE),
            Err(FsError::UnsafeType { .. })
        ));
        assert_eq!(
            fs::read(path).expect("hostile object preserved"),
            b"not a socket"
        );
    }

    #[test]
    fn staged_socket_bind_uses_random_names_and_exact_mode() {
        let (temporary, trusted) = trusted_root();
        let (first_name, first_listener, first_identity) = trusted
            .bind_unix_listener_staged(".socket-", FILE_MODE)
            .expect("bind first staged socket");
        let (second_name, second_listener, second_identity) = trusted
            .bind_unix_listener_staged(".socket-", FILE_MODE)
            .expect("bind second staged socket");
        assert_ne!(first_name, second_name);
        for name in [&first_name, &second_name] {
            assert_eq!(
                fs::symlink_metadata(temporary.path().join(name))
                    .expect("inspect staged socket")
                    .mode()
                    & MODE_MASK,
                FILE_MODE
            );
        }
        drop(first_listener);
        drop(second_listener);
        for (name, identity) in [(first_name, first_identity), (second_name, second_identity)] {
            let StageOutcome::Staged(staged) = trusted
                .stage_random(&name, ".socket-cleanup-", identity)
                .expect("stage socket cleanup")
            else {
                panic!("socket must remain bound to its expected inode");
            };
            assert_eq!(
                staged.remove().expect("remove staged socket"),
                RemoveOutcome::Removed
            );
        }
    }

    #[test]
    fn staged_tree_cleanup_is_descriptor_anchored_and_does_not_follow_symlinks() {
        let (temporary, trusted) = trusted_root();
        let tree = temporary.path().join("tree");
        let nested = tree.join("nested");
        let outside = temporary.path().join("outside");
        fs::create_dir(&tree).expect("create tree");
        fs::set_permissions(&tree, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set tree mode");
        fs::create_dir(&nested).expect("create nested directory");
        fs::set_permissions(&nested, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set nested mode");
        fs::write(nested.join("record"), b"managed").expect("write nested record");
        fs::set_permissions(nested.join("record"), fs::Permissions::from_mode(FILE_MODE))
            .expect("set nested record mode");
        fs::write(&outside, b"preserve").expect("write outside sentinel");
        std::os::unix::fs::symlink(&outside, tree.join("outside-link"))
            .expect("link outside sentinel");
        let identity = trusted
            .entry_identity("tree", EntryKind::Directory)
            .expect("inspect tree")
            .expect("tree exists");
        let StageOutcome::Staged(staged) = trusted
            .stage_random("tree", ".tree-cleanup-", identity)
            .expect("stage tree")
        else {
            panic!("tree must stage");
        };

        assert_eq!(
            staged.remove_tree().expect("remove staged tree"),
            RemoveOutcome::Removed
        );
        assert_eq!(
            fs::read(outside).expect("read outside sentinel"),
            b"preserve"
        );
    }

    #[test]
    fn staged_tree_cleanup_preserves_a_replacement_at_the_staging_name() {
        let (temporary, trusted) = trusted_root();
        let tree = temporary.path().join("tree");
        fs::create_dir(&tree).expect("create tree");
        fs::set_permissions(&tree, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set tree mode");
        let identity = trusted
            .entry_identity("tree", EntryKind::Directory)
            .expect("inspect tree")
            .expect("tree exists");
        let StageOutcome::Staged(staged) = trusted
            .stage_random("tree", ".tree-cleanup-", identity)
            .expect("stage tree")
        else {
            panic!("tree must stage");
        };
        let staging_path = staged.path();
        let captured = temporary.path().join("captured-tree");
        fs::rename(&staging_path, &captured).expect("move captured tree aside");
        fs::create_dir(&staging_path).expect("create replacement tree");
        fs::set_permissions(&staging_path, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set replacement mode");
        fs::write(staging_path.join("sentinel"), b"replacement")
            .expect("write replacement sentinel");

        assert_eq!(
            staged.remove_tree().expect("reject staged replacement"),
            RemoveOutcome::IdentityChanged
        );
        assert_eq!(
            fs::read(staging_path.join("sentinel")).expect("read replacement sentinel"),
            b"replacement"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "native APFS CI runs this test as root to create a foreign-owned fixture"]
    fn apfs_foreign_owned_private_directory_is_rejected() {
        assert_eq!(rustix::process::geteuid().as_raw(), 0, "test requires root");
        let temporary = tempfile::tempdir().expect("create APFS fixture");
        let canonical_path = fs::canonicalize(temporary.path()).expect("canonicalize APFS fixture");
        fs::set_permissions(&canonical_path, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set fixture mode");
        let foreign = canonical_path.join("foreign");
        fs::create_dir(&foreign).expect("create foreign directory");
        fs::set_permissions(&foreign, fs::Permissions::from_mode(DIRECTORY_MODE))
            .expect("set foreign mode");
        let status = std::process::Command::new("/usr/sbin/chown")
            .args(["1", foreign.to_str().expect("UTF-8 fixture path")])
            .status()
            .expect("run chown");
        assert!(status.success(), "chown foreign fixture");
        assert_eq!(
            fs::symlink_metadata(&foreign)
                .expect("inspect foreign fixture")
                .uid(),
            1,
            "fixture must be foreign-owned"
        );
        assert!(matches!(
            TrustedDir::open_absolute(&foreign, DIRECTORY_MODE),
            Err(FsError::UnsafeOwner {
                path,
                actual: 1,
                expected: 0
            }) if path == foreign
        ));
    }

    #[test]
    fn file_sync_flushes_real_descriptor() {
        let temporary = tempfile::NamedTempFile::new().expect("create temporary file");
        sync_file(temporary.as_file()).expect("sync temporary file");
        assert_eq!(
            temporary
                .as_file()
                .metadata()
                .expect("inspect file")
                .nlink(),
            1
        );
    }
}
