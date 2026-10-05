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
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    use ed25519_dalek::SigningKey;
    use package::{AnchorFile, KeyId, RootKey};
    use pohunek_test_support::tempdir;

    use super::*;

    const WINDOW_START: u64 = 1;
    const WINDOW_END: u64 = 4_102_444_800;
    const USER: u32 = 1000;
    const OTHER_USER: u32 = 1001;

    fn anchor_bytes(seed: u8) -> Vec<u8> {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let root =
            RootKey::new(key.verifying_key().to_bytes(), WINDOW_START, WINDOW_END).expect("root");
        AnchorFile::new(vec![root], Vec::<KeyId>::new())
            .expect("anchor")
            .to_bytes()
            .expect("bytes")
    }

    fn scratch() -> tempfile::TempDir {
        tempdir().expect("tempdir")
    }

    fn write_anchor(directory: &Path, bytes: &[u8], mode: u32) -> PathBuf {
        let path = directory.join(ANCHOR_FILE_NAME);
        fs::write(&path, bytes).expect("write anchor");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("anchor mode");
        path
    }

    fn fault(trust: CatalogTrust) -> AnchorFault {
        match trust {
            CatalogTrust::Invalid(fault) => fault,
            other => panic!("expected an invalid anchor, got {other:?}"),
        }
    }

    #[test]
    fn a_directory_without_an_anchor_is_absent() {
        let directory = scratch();
        assert!(matches!(
            load_from_directory(directory.path()),
            CatalogTrust::Absent
        ));
    }

    #[test]
    fn a_valid_anchor_loads_with_the_modes_a_release_unpacks_to() {
        for mode in [0o600, 0o640, 0o644, 0o444] {
            let directory = scratch();
            write_anchor(directory.path(), &anchor_bytes(1), mode);
            assert!(
                matches!(
                    load_from_directory(directory.path()),
                    CatalogTrust::Loaded(_)
                ),
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn a_malformed_anchor_is_invalid_and_never_absent() {
        let directory = scratch();
        write_anchor(directory.path(), b"{ not json", 0o644);
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::Malformed(AnchorFileError::Syntax)
        );

        let directory = scratch();
        write_anchor(directory.path(), b"", 0o644);
        assert!(matches!(
            fault(load_from_directory(directory.path())),
            AnchorFault::Malformed(_)
        ));
    }

    #[test]
    fn an_anchor_with_a_key_id_that_does_not_match_is_invalid() {
        let directory = scratch();
        let text = String::from_utf8(anchor_bytes(1)).expect("utf-8");
        let other = KeyId::derive(&SigningKey::from_bytes(&[2; 32]).verifying_key());
        let id = KeyId::derive(&SigningKey::from_bytes(&[1; 32]).verifying_key());
        write_anchor(
            directory.path(),
            text.replace(id.as_str(), other.as_str()).as_bytes(),
            0o644,
        );
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::Malformed(AnchorFileError::KeyIdMismatch)
        );
    }

    #[test]
    fn an_oversized_anchor_is_too_large() {
        let directory = scratch();
        write_anchor(directory.path(), &vec![b' '; MAX_ANCHOR_BYTES + 1], 0o644);
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::TooLarge
        );
    }

    #[test]
    fn a_file_writable_by_group_or_others_is_unsafe() {
        for mode in [0o664, 0o646, 0o666, 0o620] {
            let directory = scratch();
            write_anchor(directory.path(), &anchor_bytes(1), mode);
            assert_eq!(
                fault(load_from_directory(directory.path())),
                AnchorFault::UnsafeFile,
                "mode {mode:o}"
            );
        }
    }

    /// Adds `acl` to `path` with the native macOS `chmod +a`, from an empty
    /// environment.
    #[cfg(target_os = "macos")]
    fn add_acl(path: &Path, acl: &str) {
        let status = std::process::Command::new("/bin/chmod")
            .env_clear()
            .current_dir(path.parent().expect("an anchor file has a parent"))
            .args(["+a", acl])
            .arg(path)
            .status()
            .expect("run native ACL fixture command");
        assert!(status.success(), "native ACL fixture command must succeed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_acl_letting_another_principal_change_the_file_is_unsafe() {
        for acl in [
            "everyone allow write",
            "everyone allow read,write,append,delete,writeattr,writesecurity",
        ] {
            let directory = scratch();
            let path = write_anchor(directory.path(), &anchor_bytes(1), 0o644);
            add_acl(&path, acl);
            assert_eq!(
                fault(load_from_directory(directory.path())),
                AnchorFault::UnsafeFile,
                "{acl}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_acl_that_only_denies_or_lets_others_read_is_accepted() {
        for acl in ["everyone deny delete", "everyone allow read"] {
            let directory = scratch();
            let path = write_anchor(directory.path(), &anchor_bytes(1), 0o644);
            add_acl(&path, acl);
            assert!(
                matches!(
                    load_from_directory(directory.path()),
                    CatalogTrust::Loaded(_)
                ),
                "{acl}"
            );
        }
    }

    #[test]
    fn a_directory_writable_by_group_or_others_is_unsafe() {
        for mode in [0o775, 0o757, 0o777] {
            let directory = scratch();
            write_anchor(directory.path(), &anchor_bytes(1), 0o644);
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode))
                .expect("directory mode");
            assert_eq!(
                fault(load_from_directory(directory.path())),
                AnchorFault::UnsafeDirectory,
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn a_symbolic_link_is_unsafe_even_when_it_points_at_a_valid_anchor() {
        let directory = scratch();
        let real = write_anchor(directory.path(), &anchor_bytes(1), 0o644);
        let other = scratch();
        symlink(&real, other.path().join(ANCHOR_FILE_NAME)).expect("symlink");
        assert_eq!(
            fault(load_from_directory(other.path())),
            AnchorFault::UnsafeFile
        );
        let dangling = scratch();
        symlink(
            dangling.path().join("absent"),
            dangling.path().join(ANCHOR_FILE_NAME),
        )
        .expect("symlink");
        assert_eq!(
            fault(load_from_directory(dangling.path())),
            AnchorFault::UnsafeFile,
            "a dangling link is not an absent anchor"
        );
    }

    #[test]
    fn a_hard_linked_anchor_is_unsafe() {
        let directory = scratch();
        let path = write_anchor(directory.path(), &anchor_bytes(1), 0o644);
        fs::hard_link(&path, directory.path().join("second-name")).expect("hard link");
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::UnsafeFile
        );
    }

    #[test]
    fn a_fifo_is_unsafe_and_does_not_block() {
        let directory = scratch();
        nix::unistd::mkfifo(
            &directory.path().join(ANCHOR_FILE_NAME),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )
        .expect("mkfifo");
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::UnsafeFile
        );
    }

    #[test]
    fn a_directory_in_place_of_the_file_is_unsafe() {
        let directory = scratch();
        fs::create_dir(directory.path().join(ANCHOR_FILE_NAME)).expect("mkdir");
        assert_eq!(
            fault(load_from_directory(directory.path())),
            AnchorFault::UnsafeFile
        );
    }

    #[test]
    fn the_anchor_is_read_beside_the_executable_and_a_rootless_path_is_unknown() {
        let directory = scratch();
        write_anchor(directory.path(), &anchor_bytes(1), 0o644);
        let executable = directory.path().join("pohunekd");
        assert!(matches!(load_beside(&executable), CatalogTrust::Loaded(_)));
        assert_eq!(
            fault(load_beside(Path::new("/"))),
            AnchorFault::ExecutableUnknown
        );
    }

    #[test]
    fn a_directory_that_does_not_exist_is_unreadable() {
        let directory = scratch();
        assert_eq!(
            fault(load_from_directory(&directory.path().join("absent"))),
            AnchorFault::Unreadable
        );
    }

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

    #[test]
    fn fault_messages_carry_no_path_and_no_file_content() {
        for fault in [
            AnchorFault::ExecutableUnknown,
            AnchorFault::Unreadable,
            AnchorFault::UnsafeDirectory,
            AnchorFault::UnsafeFile,
            AnchorFault::TooLarge,
            AnchorFault::Malformed(AnchorFileError::Schema),
        ] {
            let message = fault.to_string();
            assert!(!message.contains('/'), "{message}");
        }
    }
}
