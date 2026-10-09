//! The catalog trust anchor of this installation.
//!
//! Official runtime packages are authorized through a signed catalog whose
//! root keys come from one file, [`package::ANCHOR_FILE_NAME`], that the
//! release bundle places beside the daemon executable. The daemon reads it
//! once at startup. A missing file is a host with no anchor
//! ([`CatalogTrust::Absent`]); a file that cannot be trusted
//! ([`CatalogTrust::Invalid`]) is never treated as absent and never partially
//! used, so every catalog install fails closed with a distinct diagnostic
//! while the rest of the daemon keeps running and `daemon.doctor` reports it.
//!
//! The file holds public keys only, so it is not owner-private. The policy is
//! integrity: the executable's directory chain and the file must not be
//! writable by anyone but the daemon's user or root, because whoever can
//! rewrite the anchor can already replace the daemon. On macOS an extended ACL
//! entry that lets another principal change the file counts as writable. No
//! environment variable or command-line option selects another file.

// Rust guideline compliant 2026-10-05

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use package::{parse_anchor, AnchorFileError, ANCHOR_FILE_NAME, MAX_ANCHOR_BYTES};
use pohunek_platform::filesystem::{acl_grants_change, FsError, TrustedDir};
use thiserror::Error;
use tracing::{info, warn};

use crate::session::HostTrustAnchor;

/// Owner id of the system account whose files a release installer may leave
/// the anchor with.
const ROOT_UID: u32 = 0;

/// Permission bits that let the group or others rewrite a file or directory.
const GROUP_OTHER_WRITE: u32 = 0o022;

/// Permission and file-type bits of `st_mode`, without the file-type bits.
const MODE_BITS: u32 = 0o7777;

/// What this installation trusts to authorize official packages.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub enum CatalogTrust {
    /// The installation ships no anchor file.
    #[default]
    Absent,
    /// The anchor file was read and validated.
    Loaded(HostTrustAnchor),
    /// The anchor file exists or its location is unsafe, and it cannot be
    /// trusted.
    Invalid(AnchorFault),
}

/// Why the anchor of an installation cannot be trusted.
///
/// The messages are fixed text: they carry no path and no file content.
#[derive(Debug, Clone, Copy, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum AnchorFault {
    /// The daemon cannot tell where its executable lives.
    #[error("the daemon executable location is unknown, so its trust anchor cannot be located")]
    ExecutableUnknown,
    /// The anchor or its directory could not be read.
    #[error("the trust anchor or its directory could not be read")]
    Unreadable,
    /// A directory above the anchor can be rewritten by another account.
    #[error("the directory holding the trust anchor is not safe: it, or an ancestor, is owned or writable by another account")]
    UnsafeDirectory,
    /// The anchor file is a link, not a regular file, owned by another
    /// account, writable by others (by mode or by an extended ACL) or
    /// hard-linked.
    #[error("the trust anchor file is not safe: it must be a regular file owned by the daemon user or root, not writable by group or others (by mode or ACL), with one link")]
    UnsafeFile,
    /// The anchor file is larger than [`MAX_ANCHOR_BYTES`].
    #[error("the trust anchor file is too large")]
    TooLarge,
    /// The anchor file is not a valid trust anchor.
    #[error("the trust anchor file is invalid: {0}")]
    Malformed(AnchorFileError),
}

impl CatalogTrust {
    /// Reads the anchor beside the running daemon executable.
    ///
    /// Logs the outcome; an invalid anchor logs a warning with the typed
    /// fault and never the file content.
    #[must_use]
    pub fn load_beside_executable() -> Self {
        let trust = match std::env::current_exe() {
            Ok(executable) => load_beside(&executable),
            Err(error) => {
                warn!(%error, "the daemon executable location is unknown");
                Self::Invalid(AnchorFault::ExecutableUnknown)
            }
        };
        match &trust {
            Self::Absent => info!("no catalog trust anchor beside the daemon executable"),
            Self::Loaded(_) => info!("loaded the catalog trust anchor"),
            Self::Invalid(fault) => warn!(%fault, "the catalog trust anchor cannot be trusted"),
        }
        trust
    }
}

/// Reads the anchor file beside `executable`.
fn load_beside(executable: &Path) -> CatalogTrust {
    match executable.parent() {
        Some(directory) => load_from_directory(directory),
        None => CatalogTrust::Invalid(AnchorFault::ExecutableUnknown),
    }
}

/// Reads the anchor file in `directory`.
///
/// `directory` is absolute and holds the daemon executable.
#[must_use]
pub fn load_from_directory(directory: &Path) -> CatalogTrust {
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    match read_anchor(directory, effective_uid) {
        Ok(None) => CatalogTrust::Absent,
        Ok(Some(bytes)) => match parse_anchor(&bytes) {
            Ok(file) => {
                let (roots, revoked) = file.into_parts();
                match HostTrustAnchor::new(roots, revoked) {
                    Ok(anchor) => CatalogTrust::Loaded(anchor),
                    Err(error) => CatalogTrust::Invalid(AnchorFault::Malformed(error.into())),
                }
            }
            Err(error) => CatalogTrust::Invalid(AnchorFault::Malformed(error)),
        },
        Err(fault) => CatalogTrust::Invalid(fault),
    }
}

/// The anchor bytes, or `None` when the directory has no anchor file.
fn read_anchor(directory: &Path, effective_uid: u32) -> Result<Option<Vec<u8>>, AnchorFault> {
    TrustedDir::open_absolute_ancestor(directory).map_err(|error| match error {
        FsError::Io { .. } => AnchorFault::Unreadable,
        _ => AnchorFault::UnsafeDirectory,
    })?;
    let path: PathBuf = directory.join(ANCHOR_FILE_NAME);
    let Some(file) = open_anchor(&path)? else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(|_error| AnchorFault::Unreadable)?;
    // An ACL that cannot be read is unknown, so the anchor is unreadable.
    let acl_grants_change = acl_grants_change(&file).map_err(|_error| AnchorFault::Unreadable)?;
    check_file(
        &FileFacts {
            regular: metadata.is_file(),
            owner: metadata.uid(),
            mode: metadata.mode() & MODE_BITS,
            links: metadata.nlink(),
            acl_grants_change,
        },
        effective_uid,
    )?;
    let limit = u64::try_from(MAX_ANCHOR_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_error| AnchorFault::Unreadable)?;
    if bytes.len() > MAX_ANCHOR_BYTES {
        return Err(AnchorFault::TooLarge);
    }
    Ok(Some(bytes))
}

/// Opens the anchor without following a final symbolic link and without
/// blocking on a FIFO. A missing file is `Ok(None)`.
fn open_anchor(path: &Path) -> Result<Option<File>, AnchorFault> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        // A final symbolic link fails the no-follow open with ELOOP.
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Err(AnchorFault::UnsafeFile),
        Err(_error) => Err(AnchorFault::Unreadable),
    }
}

/// The facts about an opened anchor file the policy judges.
#[derive(Debug, Clone, Copy)]
struct FileFacts {
    regular: bool,
    owner: u32,
    mode: u32,
    links: u64,
    /// An extended ACL entry lets some principal change the file.
    acl_grants_change: bool,
}

/// Applies the anchor file policy to what was observed on the opened handle.
fn check_file(facts: &FileFacts, effective_uid: u32) -> Result<(), AnchorFault> {
    let owner_is_trusted = facts.owner == effective_uid || facts.owner == ROOT_UID;
    if facts.regular
        && owner_is_trusted
        && facts.mode & GROUP_OTHER_WRITE == 0
        && facts.links == 1
        && !facts.acl_grants_change
    {
        Ok(())
    } else {
        Err(AnchorFault::UnsafeFile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: u32 = 1000;
    const OTHER_USER: u32 = 1001;

    #[test]
    fn the_file_policy_admits_only_the_daemon_user_and_root() {
        let facts = |owner, mode, links, regular| FileFacts {
            regular,
            owner,
            mode,
            links,
            acl_grants_change: false,
        };
        assert_eq!(check_file(&facts(USER, 0o644, 1, true), USER), Ok(()));
        assert_eq!(check_file(&facts(ROOT_UID, 0o644, 1, true), USER), Ok(()));
        for rejected in [
            facts(OTHER_USER, 0o644, 1, true),
            facts(USER, 0o664, 1, true),
            facts(USER, 0o646, 1, true),
            facts(ROOT_UID, 0o666, 1, true),
            facts(USER, 0o644, 2, true),
            facts(USER, 0o644, 0, true),
            facts(USER, 0o644, 1, false),
            FileFacts {
                acl_grants_change: true,
                ..facts(USER, 0o644, 1, true)
            },
        ] {
            assert_eq!(
                check_file(&rejected, USER),
                Err(AnchorFault::UnsafeFile),
                "{rejected:?}"
            );
        }
        assert_eq!(
            check_file(&facts(ROOT_UID, 0o644, 1, true), ROOT_UID),
            Ok(()),
            "root running the daemon trusts root-owned files"
        );
    }
}
