//! Reading the Ed25519 key files of the catalog tooling.
//!
//! A signing key file holds the 32-byte Ed25519 seed as 64 lowercase
//! hexadecimal characters and an optional final newline. The tool never takes
//! a key from an argument or an environment variable, never prints or logs key
//! material, and refuses a file another account could read or replace: the
//! file must be a regular file owned by the invoking user, with no group or
//! other permission bits, no macOS extended ACL beyond deny entries, and one
//! link, judged on the opened handle. The seed
//! lives in zeroizing buffers.
//!
//! A public key file holds the 64-hex public key and is read with the same
//! no-follow, bounded rules but needs no permission policy.

// Rust guideline compliant 2026-10-05

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use ed25519_dalek::SigningKey;
use nix::fcntl::OFlag;
use pohunek_platform::filesystem::acl_grants_access;
use zeroize::Zeroizing;

use crate::XtaskError;

/// Largest accepted key file: 128 bytes.
///
/// A key is 64 hexadecimal characters plus a newline; the ceiling bounds what
/// is read from an arbitrary path.
const MAX_KEY_FILE_BYTES: usize = 128;

/// Length of an Ed25519 key in bytes.
const KEY_BYTES: usize = 32;

/// Permission bits no secret key file may carry: any group or other access.
const GROUP_OTHER_ACCESS: u32 = 0o077;

/// Permission and special mode bits of `st_mode`.
const MODE_BITS: u32 = 0o7777;

/// Why a key file was refused. Never carries key material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyFileFault {
    /// The file could not be opened or read, or a final symbolic link was
    /// refused.
    Unreadable,
    /// The path is a symbolic link.
    Symlink,
    /// The path is not a regular file.
    NotRegular,
    /// The file is owned by another account.
    ForeignOwner,
    /// The file grants group or other access.
    UnsafePermissions {
        /// The permission bits found.
        mode: u32,
    },
    /// An extended ACL entry grants some principal access to the file.
    UnsafeAcl,
    /// The file has more than one hard link.
    HardLinked,
    /// The file is longer than a key file can be.
    TooLarge,
    /// The content is not 64 lowercase hexadecimal characters.
    Malformed,
}

impl fmt::Display for KeyFileFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable => f.write_str("it cannot be opened or read"),
            Self::Symlink => f.write_str("it is a symbolic link"),
            Self::NotRegular => f.write_str("it is not a regular file"),
            Self::ForeignOwner => f.write_str("it is owned by another account"),
            Self::UnsafePermissions { mode } => write!(
                f,
                "its mode {mode:04o} grants group or other access; run `chmod 600` on it"
            ),
            Self::UnsafeAcl => {
                f.write_str("an extended ACL entry grants access to it; remove it with `chmod -N`")
            }
            Self::HardLinked => f.write_str("it has more than one hard link"),
            Self::TooLarge => f.write_str("it is too large for a key file"),
            Self::Malformed => f.write_str("it does not hold 64 lowercase hexadecimal characters"),
        }
    }
}

/// What the policy judges about an opened key file.
#[derive(Clone, Copy, Debug)]
struct FileFacts {
    regular: bool,
    owner: u32,
    mode: u32,
    links: u64,
    /// An extended ACL entry other than a deny entry is present.
    acl_grants_access: bool,
}

/// Applies the secret key file policy.
fn check_secret(facts: &FileFacts, effective_uid: u32) -> Result<(), KeyFileFault> {
    if !facts.regular {
        Err(KeyFileFault::NotRegular)
    } else if facts.owner != effective_uid {
        Err(KeyFileFault::ForeignOwner)
    } else if facts.mode & GROUP_OTHER_ACCESS != 0 {
        Err(KeyFileFault::UnsafePermissions {
            mode: facts.mode & GROUP_OTHER_ACCESS,
        })
    } else if facts.acl_grants_access {
        Err(KeyFileFault::UnsafeAcl)
    } else if facts.links != 1 {
        Err(KeyFileFault::HardLinked)
    } else {
        Ok(())
    }
}

/// Opens `path` without following a final symbolic link and without blocking.
fn open_no_follow(path: &Path) -> Result<File, KeyFileFault> {
    let flags = OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits())
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
                KeyFileFault::Symlink
            } else {
                KeyFileFault::Unreadable
            }
        })
}

/// Reads at most [`MAX_KEY_FILE_BYTES`] from `file` into a zeroizing buffer.
fn read_bounded(file: File) -> Result<Zeroizing<Vec<u8>>, KeyFileFault> {
    let limit = u64::try_from(MAX_KEY_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_error| KeyFileFault::Unreadable)?;
    if bytes.len() > MAX_KEY_FILE_BYTES {
        return Err(KeyFileFault::TooLarge);
    }
    Ok(bytes)
}

/// Decodes 64 lowercase hex characters with one optional final newline.
fn decode_key_text(text: &[u8]) -> Result<Zeroizing<[u8; KEY_BYTES]>, KeyFileFault> {
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    if text.len() != KEY_BYTES * 2 {
        return Err(KeyFileFault::Malformed);
    }
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(KeyFileFault::Malformed),
    };
    let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
    let (pairs, _remainder) = text.as_chunks::<2>();
    for (slot, [high, low]) in key.iter_mut().zip(pairs) {
        *slot = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Ok(key)
}

fn refuse(path: &Path, fault: KeyFileFault) -> XtaskError {
    XtaskError::KeyFile {
        path: path.to_path_buf(),
        fault,
    }
}

/// Reads the signing key at `path`.
///
/// # Errors
///
/// Returns [`XtaskError::KeyFile`] naming the path and the fault, never the
/// content.
pub(crate) fn read_signing_key(path: &Path) -> Result<SigningKey, XtaskError> {
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    let file = open_no_follow(path).map_err(|fault| refuse(path, fault))?;
    let metadata = file
        .metadata()
        .map_err(|_error| refuse(path, KeyFileFault::Unreadable))?;
    // An ACL that cannot be read is unknown, so the key is refused as unreadable.
    let acl_grants_access =
        acl_grants_access(&file).map_err(|_error| refuse(path, KeyFileFault::Unreadable))?;
    check_secret(
        &FileFacts {
            regular: metadata.is_file(),
            owner: metadata.uid(),
            mode: metadata.mode() & MODE_BITS,
            links: metadata.nlink(),
            acl_grants_access,
        },
        effective_uid,
    )
    .map_err(|fault| refuse(path, fault))?;
    let text = read_bounded(file).map_err(|fault| refuse(path, fault))?;
    let seed = decode_key_text(&text).map_err(|fault| refuse(path, fault))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Reads the public key at `path`: 64 lowercase hex characters.
///
/// # Errors
///
/// Returns [`XtaskError::KeyFile`] when the file cannot be read or is not a
/// public key.
pub(crate) fn read_public_key(path: &Path) -> Result<[u8; KEY_BYTES], XtaskError> {
    let file = open_no_follow(path).map_err(|fault| refuse(path, fault))?;
    let is_file = file
        .metadata()
        .map_err(|_error| refuse(path, KeyFileFault::Unreadable))?
        .is_file();
    if !is_file {
        return Err(refuse(path, KeyFileFault::NotRegular));
    }
    let text = read_bounded(file).map_err(|fault| refuse(path, fault))?;
    let key = decode_key_text(&text).map_err(|fault| refuse(path, fault))?;
    Ok(*key)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    use super::*;

    const SEED_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const USER: u32 = 1000;

    fn key_file(dir: &Path, text: &[u8], mode: u32) -> std::path::PathBuf {
        let path = dir.join("signing.key");
        fs::write(&path, text).expect("write key");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("mode");
        path
    }

    fn fault_of(result: Result<SigningKey, XtaskError>) -> KeyFileFault {
        match result {
            Err(XtaskError::KeyFile { fault, .. }) => fault,
            Err(other) => panic!("expected a key file fault, got {other}"),
            Ok(_) => panic!("expected a key file fault, got a key"),
        }
    }

    #[test]
    fn an_owner_private_key_file_is_read_with_or_without_the_final_newline() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let expected = SigningKey::from_bytes(&[1; KEY_BYTES]).verifying_key();
        for (text, mode) in [
            (format!("{SEED_HEX}\n"), 0o600),
            (SEED_HEX.to_owned(), 0o400),
        ] {
            let path = key_file(dir.path(), text.as_bytes(), mode);
            let key = read_signing_key(&path).expect("key");
            assert_eq!(key.verifying_key(), expected, "mode {mode:o}");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        }
    }

    #[test]
    fn a_file_with_group_or_other_access_is_refused_before_it_is_read() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        for mode in [0o640, 0o604, 0o644, 0o660, 0o666, 0o700 | 0o010] {
            let path = key_file(dir.path(), SEED_HEX.as_bytes(), mode);
            assert!(
                matches!(
                    fault_of(read_signing_key(&path)),
                    KeyFileFault::UnsafePermissions { .. }
                ),
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn the_refusal_message_names_the_path_and_never_the_key() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let path = key_file(dir.path(), SEED_HEX.as_bytes(), 0o644);
        let message = read_signing_key(&path).expect_err("refused").to_string();
        assert!(message.contains("signing.key"), "{message}");
        assert!(message.contains("chmod 600"), "{message}");
        assert!(!message.contains(SEED_HEX), "{message}");
        assert!(!message.contains("01010101"), "{message}");
    }

    #[test]
    fn a_symbolic_link_is_refused_even_to_a_private_key() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let real = key_file(dir.path(), SEED_HEX.as_bytes(), 0o600);
        let link = dir.path().join("link.key");
        symlink(&real, &link).expect("symlink");
        assert_eq!(fault_of(read_signing_key(&link)), KeyFileFault::Symlink);
    }

    #[test]
    fn a_hard_linked_key_file_is_refused() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let path = key_file(dir.path(), SEED_HEX.as_bytes(), 0o600);
        fs::hard_link(&path, dir.path().join("second")).expect("hard link");
        assert_eq!(fault_of(read_signing_key(&path)), KeyFileFault::HardLinked);
    }

    #[test]
    fn a_directory_and_a_missing_path_are_refused() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        assert!(matches!(
            fault_of(read_signing_key(dir.path())),
            KeyFileFault::NotRegular | KeyFileFault::Unreadable
        ));
        assert_eq!(
            fault_of(read_signing_key(&dir.path().join("absent"))),
            KeyFileFault::Unreadable
        );
    }

    #[test]
    fn malformed_and_oversized_content_is_refused() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let upper = SEED_HEX.replace('0', "A");
        let too_long = format!("{SEED_HEX}{SEED_HEX}");
        let cases: [(&[u8], KeyFileFault); 6] = [
            (b"", KeyFileFault::Malformed),
            (b"zz", KeyFileFault::Malformed),
            (&SEED_HEX.as_bytes()[1..], KeyFileFault::Malformed),
            (upper.as_bytes(), KeyFileFault::Malformed),
            (b"01010101\n\n", KeyFileFault::Malformed),
            (&[b'0'; MAX_KEY_FILE_BYTES + 1], KeyFileFault::TooLarge),
        ];
        for (text, expected) in cases {
            let path = key_file(dir.path(), text, 0o600);
            assert_eq!(fault_of(read_signing_key(&path)), expected, "{text:?}");
        }
        let path = key_file(dir.path(), too_long.as_bytes(), 0o600);
        assert_eq!(fault_of(read_signing_key(&path)), KeyFileFault::Malformed);
    }

    /// Adds `acl` to `path` with the native macOS `chmod +a`, from an empty
    /// environment.
    #[cfg(target_os = "macos")]
    fn add_acl(path: &Path, acl: &str) {
        let status = std::process::Command::new("/bin/chmod")
            .env_clear()
            .current_dir(path.parent().expect("a key file has a parent"))
            .args(["+a", acl])
            .arg(path)
            .status()
            .expect("run native ACL fixture command");
        assert!(status.success(), "native ACL fixture command must succeed");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_macos_acl_granting_access_to_a_private_key_file_is_refused() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        for acl in ["everyone allow read", "everyone allow read,write"] {
            let path = key_file(dir.path(), SEED_HEX.as_bytes(), 0o600);
            add_acl(&path, acl);
            assert_eq!(
                fault_of(read_signing_key(&path)),
                KeyFileFault::UnsafeAcl,
                "{acl}"
            );
            fs::remove_file(&path).expect("remove key");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_deny_only_macos_acl_on_a_private_key_file_is_accepted() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let path = key_file(dir.path(), SEED_HEX.as_bytes(), 0o600);
        add_acl(&path, "everyone deny delete");
        read_signing_key(&path).expect("a deny-only ACL grants nothing");
    }

    #[test]
    fn the_policy_admits_only_a_private_regular_single_link_file_of_the_caller() {
        let facts = |regular, owner, mode, links| FileFacts {
            regular,
            owner,
            mode,
            links,
            acl_grants_access: false,
        };
        assert_eq!(check_secret(&facts(true, USER, 0o600, 1), USER), Ok(()));
        assert_eq!(
            check_secret(
                &FileFacts {
                    acl_grants_access: true,
                    ..facts(true, USER, 0o600, 1)
                },
                USER
            ),
            Err(KeyFileFault::UnsafeAcl)
        );
        assert_eq!(
            check_secret(&facts(true, USER + 1, 0o600, 1), USER),
            Err(KeyFileFault::ForeignOwner)
        );
        assert_eq!(
            check_secret(&facts(true, 0, 0o600, 1), USER),
            Err(KeyFileFault::ForeignOwner),
            "a root-owned file is another account's file"
        );
        assert_eq!(
            check_secret(&facts(false, USER, 0o600, 1), USER),
            Err(KeyFileFault::NotRegular)
        );
        assert_eq!(
            check_secret(&facts(true, USER, 0o600, 2), USER),
            Err(KeyFileFault::HardLinked)
        );
        assert_eq!(
            check_secret(&facts(true, USER, 0o640, 1), USER),
            Err(KeyFileFault::UnsafePermissions { mode: 0o040 })
        );
    }

    #[test]
    fn a_public_key_file_needs_no_private_mode_but_is_still_strict() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let path = key_file(dir.path(), SEED_HEX.as_bytes(), 0o644);
        assert_eq!(read_public_key(&path).expect("public key"), [1; KEY_BYTES]);
        let path = key_file(dir.path(), b"not a key", 0o644);
        assert!(matches!(
            read_public_key(&path),
            Err(XtaskError::KeyFile {
                fault: KeyFileFault::Malformed,
                ..
            })
        ));
    }
}
