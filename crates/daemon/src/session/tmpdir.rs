//! The private temporary directory of each session.
//!
//! Every launched session gets `TMPDIR=<root>/<session id>`, where `root` is
//! `<cache_dir>/session-tmp` below the daemon's private cache directory, so
//! concurrent agents never share temporary file names. A profile that sets
//! `TMPDIR` in its `[env]` keeps its own value. The directory survives a stop
//! and a resume of the same session and is deleted when the session is
//! removed.

// Rust guideline compliant 2026-10-14

use std::ffi::OsString;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use pohunek_platform::filesystem::{EntryKind, FsError, StageOutcome, TrustedDir};
use protocol::{ErrorClass, ProtocolError, RuntimeRef, SessionId};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use tracing::warn;

use super::SessionRegistry;
use crate::agent::LaunchEnv;

/// Name of the variable that carries the directory to the session.
pub(super) const TMPDIR: &str = "TMPDIR";

/// Mode of the root and of every session directory: the owner's only.
const PRIVATE_DIR_MODE: u32 = 0o700;

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
        if let Some(root) = &self.inner.config.session_tmp_root {
            env.extend([(
                TMPDIR.to_owned(),
                session_tmpdir(root, id).to_string_lossy().into_owned(),
            )]);
        }
        env.extend(profile_env.iter().cloned());
        env.extend(self.session_pty_env(agent, id));
        env
    }

    /// Creates the private temporary directory of `id` when `env` exports it.
    ///
    /// # Errors
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
    let root_dir = match TrustedDir::open_absolute(root, PRIVATE_DIR_MODE) {
        Ok(root_dir) => root_dir,
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let Some(identity) = root_dir
        .entry_identity(&id.0, EntryKind::Directory)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let staged = match root_dir
        .stage_random(&id.0, REMOVAL_PREFIX, identity)
        .map_err(|error| error.to_string())?
    {
        StageOutcome::Staged(staged) => staged,
        StageOutcome::Missing => return Ok(()),
        _ => return Err("the directory changed while it was being removed".to_owned()),
    };
    let staged_path = staged.path();
    drop(staged);
    if std::fs::remove_dir_all(&staged_path).is_ok() {
        return Ok(());
    }
    // An agent may leave read-only directories (a Go module cache, a Nix
    // build tree); their entries can be unlinked once the owner may write.
    grant_owner_access(&staged_path).map_err(|error| error.to_string())?;
    std::fs::remove_dir_all(&staged_path)
        .map_err(|error| format!("{}: {error}", staged_path.display()))
}

/// Grants the owner `rwx` on every directory of the daemon's user in the tree
/// at `top`, without following symlinks and without touching any other entry.
fn grant_owner_access(top: &Path) -> std::io::Result<()> {
    let uid = rustix::process::geteuid().as_raw();
    let open_dir = |parent: Option<&OwnedFd>, name: &Path| -> std::io::Result<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match parent {
            Some(parent) => rustix::fs::openat(parent, name, flags, Mode::empty()),
            None => rustix::fs::open(name, flags, Mode::empty()),
        };
        fd.map_err(std::io::Error::from)
    };
    let make_accessible = |fd: &OwnedFd| -> std::io::Result<()> {
        let stat = rustix::fs::fstat(fd)?;
        if stat.st_uid == uid {
            let mode = Mode::from_raw_mode(stat.st_mode) | Mode::RWXU;
            rustix::fs::fchmod(fd, mode)?;
        }
        Ok(())
    };
    let top_fd = open_dir(None, top)?;
    make_accessible(&top_fd)?;
    let mut pending = vec![top_fd];
    while let Some(dir) = pending.pop() {
        for name in child_names(&dir)? {
            let stat = rustix::fs::statat(&dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::Directory || stat.st_uid != uid {
                continue;
            }
            let child = open_dir(Some(&dir), Path::new(&name))?;
            make_accessible(&child)?;
            pending.push(child);
        }
    }
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
    let code = match error {
        FsError::Io { .. } => "session_tmpdir_failed",
        _ => "session_tmpdir_unsafe",
    };
    ProtocolError::new(
        ErrorClass::Runtime,
        code,
        format!("the session's private temporary directory is not usable: {error}"),
        None,
    )
}
