//! The private temporary directory of each session.
//!
//! Every launched session gets `TMPDIR=<root>/<session id>`, where `root` is
//! `<cache_dir>/session-tmp` below the daemon's private cache directory, so
//! concurrent agents never share temporary file names. A profile that sets
//! `TMPDIR` in its `[env]` keeps its own value. The directory survives a stop
//! and a resume of the same session and is deleted when the session is
//! removed. Daemon startup deletes the directories whose session record is
//! gone, which a crash or a failed deletion can leave behind.

// Rust guideline compliant 2026-10-14

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::{EntryIdentity, EntryKind, FsError, StageOutcome, TrustedDir};
use protocol::{ErrorClass, ProtocolError, RuntimeRef, SessionId};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use tracing::{info, warn};

use super::SessionRegistry;
use crate::agent::LaunchEnv;

/// Name of the variable that carries the directory to the session.
pub(super) const TMPDIR: &str = "TMPDIR";

/// Mode of the root and of every session directory: the owner's only.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Owner permission bits a directory needs before its entries can be unlinked.
const OWNER_RWX: u32 = 0o700;

/// Prefix of a session directory moved aside for deletion.
const REMOVAL_PREFIX: &str = ".pohunek-session-tmp-";

impl SessionRegistry {
    /// Composes the environment of a launch of `id`.
    ///
    /// The session's private `TMPDIR` comes first, then `profile_env`, then
    /// the daemon handshake env: a profile `TMPDIR` replaces the private one,
    /// and every reserved `POHUNEK_*` key takes the daemon's value (the loader
    /// also strips `POHUNEK_*` from the profile env).
    pub(super) fn launch_env(
        &self,
        profile_env: &[(String, String)],
        agent: RuntimeRef,
        id: &SessionId,
    ) -> LaunchEnv {
        let mut env = LaunchEnv::default();
        // A path that is not UTF-8 cannot travel in the launch environment;
        // `prepare_session_tmpdir` then refuses the launch.
        let private = self
            .inner
            .config
            .session_tmp_root
            .as_ref()
            .and_then(|root| session_tmpdir(root, id).to_str().map(str::to_owned));
        if let Some(private) = private {
            env.extend([(TMPDIR.to_owned(), private)]);
        }
        env.extend(profile_env.iter().cloned());
        env.extend(self.session_pty_env(agent, id));
        env
    }

    /// Creates the private temporary directory of `id` when `env` exports it.
    ///
    /// # Errors
    ///
    /// Returns `session_tmpdir_failed` when the private path is not UTF-8 and
    /// no profile `TMPDIR` replaces it.
    ///
    /// Returns `session_tmpdir_unsafe` or `session_tmpdir_failed` (see
    /// [`ensure`]).
    pub(super) async fn prepare_session_tmpdir(
        &self,
        id: &SessionId,
        env: &LaunchEnv,
    ) -> Result<(), ProtocolError> {
        let Some(root) = self.inner.config.session_tmp_root.clone() else {
            return Ok(());
        };
        let private = session_tmpdir(&root, id);
        let exported = env
            .iter()
            .rev()
            .find(|(name, _)| name == TMPDIR)
            .map(|(_, value)| Path::new(value));
        if private.to_str().is_none() && exported.is_none() {
            return Err(ProtocolError::new(
                ErrorClass::Runtime,
                "session_tmpdir_failed",
                format!(
                    "the session's private temporary directory {} is not valid UTF-8",
                    private.display()
                ),
                None,
            ));
        }
        if exported != Some(private.as_path()) {
            return Ok(());
        }
        let id = id.clone();
        tokio::task::spawn_blocking(move || ensure(&root, &id).map(drop))
            .await
            .map_err(|error| {
                ProtocolError::new(
                    ErrorClass::Runtime,
                    "session_tmpdir_failed",
                    format!("creating the session's temporary directory panicked: {error}"),
                    None,
                )
            })?
    }

    /// Deletes the private temporary directory of `id`; a failure is logged.
    pub(super) async fn delete_session_tmpdir(&self, id: &SessionId) {
        let Some(root) = self.inner.config.session_tmp_root.clone() else {
            return;
        };
        let session = id.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || remove(&root, &session)).await {
            warn!(
                session_id = %id.0,
                error = %error,
                "deleting the session's temporary directory panicked"
            );
        }
    }

    /// Deletes every entry of the temporary directory root that is not the
    /// directory of a session in `known`; each deletion and failure is logged.
    pub(super) async fn delete_orphaned_session_tmpdirs(&self, known: HashSet<String>) {
        let Some(root) = self.inner.config.session_tmp_root.clone() else {
            return;
        };
        if let Err(error) = tokio::task::spawn_blocking(move || remove_orphans(&root, &known)).await
        {
            warn!(
                error = %error,
                "deleting orphaned session temporary directories panicked"
            );
        }
    }
}

/// Returns the temporary directory of `id` below `root`.
pub(super) fn session_tmpdir(root: &Path, id: &SessionId) -> PathBuf {
    root.join(&id.0)
}

/// Opens or creates the private temporary directory of `id`.
///
/// The root and the session directory are created with mode 0700 and must be
/// directories of the daemon's user with exactly that mode, reached without
/// following a symlink.
///
/// # Errors
///
/// Returns `session_tmpdir_unsafe` when an existing entry fails those checks,
/// and `session_tmpdir_failed` when the directory cannot be created.
pub(super) fn ensure(root: &Path, id: &SessionId) -> Result<PathBuf, ProtocolError> {
    let root_dir = TrustedDir::open_or_create_absolute(root, PRIVATE_DIR_MODE)
        .map_err(|error| tmpdir_error(&error))?;
    let session_dir = root_dir
        .open_or_create_child(&id.0, PRIVATE_DIR_MODE)
        .map_err(|error| tmpdir_error(&error))?;
    Ok(session_dir.path().to_path_buf())
}

/// Deletes the temporary directory of `id`, if it exists.
///
/// The directory is first moved, identity-checked, to a quarantine name inside
/// the private root, so the session id never names a half-deleted tree. A
/// failure is logged with the path and leaves the rest of the removal intact.
pub(super) fn remove(root: &Path, id: &SessionId) {
    if let Err(error) = try_remove(root, id) {
        warn!(
            session_id = %id.0,
            path = %session_tmpdir(root, id).display(),
            error = %error,
            "failed to delete the session's temporary directory"
        );
    }
}

fn try_remove(root: &Path, id: &SessionId) -> Result<(), String> {
    match open_root(root).map_err(|error| error.to_string())? {
        Some(root_dir) => remove_entry(&root_dir, OsStr::new(&id.0)),
        None => Ok(()),
    }
}

/// Deletes every entry of `root` except the directories named in `known`:
/// the directories of sessions whose record is gone and the quarantined
/// leftovers of an interrupted deletion.
///
/// An entry that is not a directory, such as a symlink, is never followed;
/// it fails the deletion and stays.
fn remove_orphans(root: &Path, known: &HashSet<String>) {
    let listed = open_root(root).and_then(|root_dir| match root_dir {
        Some(root_dir) => root_dir.entry_names().map(|names| Some((root_dir, names))),
        None => Ok(None),
    });
    let (root_dir, names) = match listed {
        Ok(Some(listed)) => listed,
        Ok(None) => return,
        Err(error) => {
            warn!(
                path = %root.display(),
                error = %error,
                "failed to list the session temporary directories"
            );
            return;
        }
    };
    for name in names {
        if name.to_str().is_some_and(|name| known.contains(name)) {
            continue;
        }
        let path = root.join(&name);
        match remove_entry(&root_dir, &name) {
            Ok(()) => info!(
                path = %path.display(),
                "deleted an orphaned session temporary directory"
            ),
            Err(error) => warn!(
                path = %path.display(),
                error = %error,
                "failed to delete an orphaned session temporary directory"
            ),
        }
    }
}

/// Opens the temporary directory root; a missing root is `None`.
fn open_root(root: &Path) -> Result<Option<TrustedDir>, FsError> {
    match TrustedDir::open_absolute(root, PRIVATE_DIR_MODE) {
        Ok(root_dir) => Ok(Some(root_dir)),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Deletes the directory `name` below `root_dir`, if it exists.
fn remove_entry(root_dir: &TrustedDir, name: &OsStr) -> Result<(), String> {
    let Some(identity) = root_dir
        .entry_identity(name, EntryKind::Directory)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let staged = match root_dir
        .stage_random(name, REMOVAL_PREFIX, identity)
        .map_err(|error| error.to_string())?
    {
        StageOutcome::Staged(staged) => staged,
        StageOutcome::Missing => return Ok(()),
        _ => return Err("the directory changed while it was being removed".to_owned()),
    };
    let staged_path = staged.path();
    let staged_identity = staged.identity();
    drop(staged);
    if std::fs::remove_dir_all(&staged_path).is_ok() {
        return Ok(());
    }
    // An agent may leave read-only directories (a Go module cache, a Nix
    // build tree); their entries can be unlinked once the owner may write.
    let staged_name = staged_path
        .file_name()
        .ok_or_else(|| format!("{}: no staged name", staged_path.display()))?;
    let root_fd = root_dir
        .try_clone_descriptor()
        .map_err(|error| error.to_string())?;
    grant_owner_access(&root_fd, staged_name, staged_identity)
        .map_err(|error| format!("{}: {error}", staged_path.display()))?;
    std::fs::remove_dir_all(&staged_path)
        .map_err(|error| format!("{}: {error}", staged_path.display()))
}

/// Grants the owner `rwx` on every directory of the daemon's user in the tree
/// `name` below `root`, which must still be the staged inode `expected`.
///
/// Every directory is opened without following a symlink, checked, and
/// changed through its descriptor. Only a directory its owner cannot read is
/// changed by name (see [`grant_unreadable`]). Only directories of the
/// daemon's user are changed, which that user could change anyway.
fn grant_owner_access(
    root: &impl AsFd,
    name: &OsStr,
    expected: EntryIdentity,
) -> std::io::Result<()> {
    let uid = rustix::process::geteuid().as_raw();
    // One frame per level of the current path, so the open descriptors are
    // bounded by the tree's depth, not by how many directories it holds.
    let top = open_accessible(root, name, uid, Some(expected))?;
    let names = child_names(&top)?.into_iter();
    let mut frames = vec![(top, names)];
    while let Some((dir, names)) = frames.last_mut() {
        let Some(name) = names.next() else {
            frames.pop();
            continue;
        };
        let stat = rustix::fs::statat(&*dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory && stat.st_uid == uid {
            let child = open_accessible(&*dir, &name, uid, None)?;
            let names = child_names(&child)?.into_iter();
            frames.push((child, names));
        }
    }
    Ok(())
}

/// Opens the directory `name` below `parent` without following a symlink and
/// grants its owner `rwx` when it belongs to `uid`; with `expected`, the
/// directory must be that inode before anything is changed.
fn open_accessible(
    parent: &impl AsFd,
    name: &OsStr,
    uid: u32,
    expected: Option<EntryIdentity>,
) -> std::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let dir = match rustix::fs::openat(parent, name, flags, Mode::empty()) {
        Ok(dir) => dir,
        // A directory its owner may not read (mode 0000) is granted access by
        // name first, still without following a symlink.
        Err(rustix::io::Errno::ACCESS) => {
            grant_unreadable(parent, name, uid, expected)?;
            rustix::fs::openat(parent, name, flags, Mode::empty())?
        }
        Err(error) => return Err(error.into()),
    };
    let dir = std::fs::File::from(dir);
    let metadata = dir.metadata()?;
    check_identity(&metadata, expected)?;
    if metadata.uid() == uid {
        dir.set_permissions(std::fs::Permissions::from_mode(metadata.mode() | OWNER_RWX))?;
    }
    Ok(OwnedFd::from(dir))
}

/// Fails when `metadata` is not the inode `expected` names.
fn check_identity(
    metadata: &std::fs::Metadata,
    expected: Option<EntryIdentity>,
) -> std::io::Result<()> {
    match expected {
        Some(expected)
            if metadata.dev() != expected.device() || metadata.ino() != expected.inode() =>
        {
            Err(std::io::Error::other(
                "the staged directory changed before its permissions were granted",
            ))
        }
        _ => Ok(()),
    }
}

/// Grants the owner `rwx` on an unreadable directory of `uid`.
///
/// Linux has no no-follow `chmodat`, so the directory is pinned with an
/// `O_PATH` descriptor, checked, and changed through its `/proc/self/fd`
/// link, which names exactly that inode.
#[cfg(target_os = "linux")]
fn grant_unreadable(
    parent: &impl AsFd,
    name: &OsStr,
    uid: u32,
    expected: Option<EntryIdentity>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let pinned = std::fs::File::from(rustix::fs::openat(parent, name, flags, Mode::empty())?);
    let metadata = pinned.metadata()?;
    check_identity(&metadata, expected)?;
    if metadata.uid() != uid {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    std::fs::set_permissions(
        format!("/proc/self/fd/{}", pinned.as_raw_fd()),
        std::fs::Permissions::from_mode(metadata.mode() | OWNER_RWX),
    )
}

/// Grants the owner `rwx` on an unreadable directory of `uid` by name.
///
/// The no-follow `chmodat` never reaches a symlink's target, but a same-user
/// process may swap in another of its own directories between the check and
/// the change; that grants the owner nothing it could not grant itself, since
/// same-UID processes are inside the owner trust boundary (docs/architecture.md,
/// "Same-UID owner trust, not a sandbox").
#[cfg(not(target_os = "linux"))]
fn grant_unreadable(
    parent: &impl AsFd,
    name: &OsStr,
    uid: u32,
    expected: Option<EntryIdentity>,
) -> std::io::Result<()> {
    let stat = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let same_inode = expected.is_none_or(|expected| {
        u64::try_from(stat.st_dev).is_ok_and(|device| device == expected.device())
            && stat.st_ino == expected.inode()
    });
    if !same_inode {
        return Err(std::io::Error::other(
            "the staged directory changed before its permissions were granted",
        ));
    }
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory || stat.st_uid != uid {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    rustix::fs::chmodat(
        parent,
        name,
        Mode::from_raw_mode(stat.st_mode) | Mode::RWXU,
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}

/// Lists the entry names of `dir`, except `.` and `..`.
fn child_names(dir: &OwnedFd) -> std::io::Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStrExt;

    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir.as_fd())? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(std::ffi::OsStr::from_bytes(bytes).to_os_string());
        }
    }
    Ok(names)
}

fn tmpdir_error(error: &FsError) -> ProtocolError {
    // A symlink or a non-directory in the path fails the no-follow directory
    // open with `ELOOP` or `ENOTDIR`; both are an unsafe entry, not an I/O
    // failure.
    let unsafe_entry = [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR]
        .iter()
        .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()));
    let code = match error {
        FsError::Io { .. } if !unsafe_entry => "session_tmpdir_failed",
        _ => "session_tmpdir_unsafe",
    };
    ProtocolError::new(
        ErrorClass::Runtime,
        code,
        format!("the session's private temporary directory is not usable: {error}"),
        None,
    )
}
