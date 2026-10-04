//! Re-verification of an installed package root against its manifest.
//!
//! The daemon calls [`verify_root`] before it loads a package, launches a
//! session from it, or mutates an integration. Every access goes through the
//! descriptor of the `packages` directory: names are never resolved as paths,
//! symbolic links are never followed, and a mode, type or owner that differs
//! from what install wrote is an error. The returned [`VerifiedRoot`] reads
//! file contents that are hashed against the manifest, so a caller never
//! consumes bytes the manifest does not describe.

// Rust guideline compliant 2026-10-04

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use pohunek_platform::filesystem::{EntryKind, FsError, TrustedDir};
use protocol::PackageDigest;
use thiserror::Error;

use crate::hash::stream_sha256;
use crate::layout::{
    digest_hex, DIRECTORY_MODE, EXECUTABLE_MODE, FILES_DIR, FILE_MODE, MANIFEST_NAME,
    MAX_MANIFEST_BYTES,
};
use crate::limits::Limits;
use crate::manifest::{Manifest, ManifestDigest, ManifestFile, Tree};

/// Why an installed package root failed verification.
///
/// No variant carries a path, a file name or file bytes: entries are
/// identified by their position in the manifest, so on-disk or manifest text
/// cannot reach a log or a terminal through an error.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum VerifyError {
    /// The package root directory does not exist.
    #[error("package root is missing")]
    RootMissing,
    /// The root has no manifest.
    #[error("package manifest is missing")]
    ManifestMissing,
    /// The manifest is not valid under the schema, path or limit rules.
    #[error("package manifest is invalid")]
    ManifestInvalid,
    /// The manifest names a different archive than the root is stored under.
    #[error("package manifest belongs to another archive")]
    ManifestArchiveMismatch,
    /// The manifest differs from the one recorded at install time.
    #[error("package manifest differs from the recorded manifest")]
    ManifestChanged,
    /// A file or directory of the manifest is absent.
    #[error("a package entry is missing")]
    Missing {
        /// Manifest position of the file; `None` for a directory.
        entry: Option<usize>,
    },
    /// The root holds a file or directory the manifest does not list.
    #[error("the package root holds an unexpected entry")]
    Added,
    /// A file has other contents than the manifest records.
    #[error("a package file was modified")]
    Modified {
        /// Manifest position of the file.
        entry: usize,
    },
    /// An entry has a mode other than the one install set.
    #[error("a package entry has the wrong mode")]
    WrongMode {
        /// Manifest position of the file; `None` for a directory.
        entry: Option<usize>,
    },
    /// An entry is not of the expected type, for example a symbolic link
    /// where a file belongs.
    #[error("a package entry has the wrong type")]
    WrongType {
        /// Manifest position of the file; `None` for a directory.
        entry: Option<usize>,
    },
    /// An entry is not owned by the current user.
    #[error("a package entry has the wrong owner")]
    WrongOwner {
        /// Manifest position of the file; `None` for a directory.
        entry: Option<usize>,
    },
    /// A file has more than one hard link.
    #[error("a package file has extra hard links")]
    Hardlinked {
        /// Manifest position of the file; `None` for a directory.
        entry: Option<usize>,
    },
    /// The requested path is not a file of the manifest.
    #[error("the path is not a file of the package")]
    UnknownPath,
    /// The filesystem failed while the root was read. The root is treated as
    /// unverified.
    #[error("the package root could not be read: {kind}")]
    Unreadable {
        /// Kind of the operating-system failure.
        kind: ErrorKind,
    },
}

/// A package root whose tree matched its manifest when it was verified.
#[derive(Debug)]
pub struct VerifiedRoot {
    digest: PackageDigest,
    manifest_digest: ManifestDigest,
    manifest: Manifest,
    files: TrustedDir,
}

impl VerifiedRoot {
    /// Digest of the archive this root was extracted from.
    #[must_use]
    pub fn digest(&self) -> &PackageDigest {
        &self.digest
    }

    /// Digest of the manifest bytes that were verified.
    #[must_use]
    pub fn manifest_digest(&self) -> &ManifestDigest {
        &self.manifest_digest
    }

    /// Files of the package in ascending path order.
    #[must_use]
    pub fn files(&self) -> &[ManifestFile] {
        &self.manifest.files
    }

    /// Location of the extracted tree, for diagnostics and for the path of an
    /// executable the daemon launches.
    #[must_use]
    pub fn files_path(&self) -> &Path {
        self.files.path()
    }

    /// Reads one file of the package through the retained descriptors.
    ///
    /// The returned bytes have the size and SHA-256 the manifest records.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::UnknownPath`] for a path the manifest does not
    /// list, otherwise the error that names how the file differs.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, VerifyError> {
        let (position, file) = self.manifest.find(path).ok_or(VerifyError::UnknownPath)?;
        let segments: Vec<&str> = path.split('/').collect();
        let bytes = read_in(&self.files, &segments, position, file)?;
        let actual = crate::hash::sha256_hex(&bytes);
        if u64::try_from(bytes.len()).ok() != Some(file.size) || actual != file.sha256 {
            return Err(VerifyError::Modified { entry: position });
        }
        Ok(bytes)
    }
}

fn read_in(
    directory: &TrustedDir,
    segments: &[&str],
    position: usize,
    file: &ManifestFile,
) -> Result<Vec<u8>, VerifyError> {
    match segments {
        [] => Err(VerifyError::UnknownPath),
        [name] => {
            let limit = usize::try_from(file.size)
                .map_err(|_cause| VerifyError::Modified { entry: position })?;
            directory
                .read_file(name, file_mode(file), limit)
                .map_err(|error| map_fs(&error, Some(position)))
        }
        [name, rest @ ..] => {
            let child = open_directory(directory, name)?.ok_or(VerifyError::Missing {
                entry: Some(position),
            })?;
            read_in(&child, rest, position, file)
        }
    }
}

/// Opens the child directory `name`, `None` when the name does not exist.
///
/// The entry is inspected without following links first, so a symbolic link or
/// a file in its place is reported as a wrong type rather than as an opaque
/// open failure.
fn open_directory(parent: &TrustedDir, name: &str) -> Result<Option<TrustedDir>, VerifyError> {
    if parent
        .entry_identity(name, EntryKind::Directory)
        .map_err(|error| map_fs(&error, None))?
        .is_none()
    {
        return Ok(None);
    }
    parent
        .open_child(name, DIRECTORY_MODE)
        .map(Some)
        .map_err(|error| map_fs(&error, None))
}

/// Mode an extracted file has: owner-only, executable by the canonical bit.
pub(crate) fn file_mode(file: &ManifestFile) -> u32 {
    if file.executable {
        EXECUTABLE_MODE
    } else {
        FILE_MODE
    }
}

/// Verifies the root of `digest` below `packages` against its manifest.
///
/// `packages` is the retained descriptor of the `packages` directory.
///
/// # Errors
///
/// Returns [`VerifyError::RootMissing`] when the root is absent, otherwise the
/// first difference between the tree and the manifest. Any failure means the
/// root must not be used.
pub fn verify_root(
    packages: &TrustedDir,
    digest: &PackageDigest,
    limits: &Limits,
) -> Result<VerifiedRoot, VerifyError> {
    let root = open_directory(packages, digest_hex(digest))?.ok_or(VerifyError::RootMissing)?;
    verify_open_root(&root, digest, limits)
}

/// Verifies an already opened root directory; shared by install, which checks
/// the staging directory before it is published.
pub(crate) fn verify_open_root(
    root: &TrustedDir,
    digest: &PackageDigest,
    limits: &Limits,
) -> Result<VerifiedRoot, VerifyError> {
    let bytes = match root.read_file(MANIFEST_NAME, FILE_MODE, MAX_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(FsError::FileTooLarge { .. }) => return Err(VerifyError::ManifestInvalid),
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => {
            return Err(VerifyError::ManifestMissing);
        }
        Err(error) => return Err(map_fs(&error, None)),
    };
    let manifest =
        Manifest::from_bytes(&bytes, limits).map_err(|_cause| VerifyError::ManifestInvalid)?;
    if manifest.archive != *digest {
        return Err(VerifyError::ManifestArchiveMismatch);
    }

    let (names, truncated) = root
        .entry_names_limited(2)
        .map_err(|error| map_fs(&error, None))?;
    let expected = [MANIFEST_NAME, FILES_DIR];
    if truncated
        || names
            .iter()
            .any(|name| !expected.iter().any(|want| name == want))
    {
        return Err(VerifyError::Added);
    }
    if names.len() != expected.len() {
        return Err(VerifyError::Missing { entry: None });
    }
    let files = open_directory(root, FILES_DIR)?.ok_or(VerifyError::Missing { entry: None })?;
    verify_directory(&files, &Tree::of(&manifest), &manifest)?;
    Ok(VerifiedRoot {
        digest: digest.clone(),
        manifest_digest: ManifestDigest::of(&bytes),
        manifest,
        files,
    })
}

fn verify_directory(
    directory: &TrustedDir,
    node: &Tree,
    manifest: &Manifest,
) -> Result<(), VerifyError> {
    let (names, truncated) = directory
        .entry_names_limited(node.child_count())
        .map_err(|error| map_fs(&error, None))?;
    if truncated {
        return Err(VerifyError::Added);
    }
    let mut present = HashSet::with_capacity(names.len());
    for name in &names {
        let known = name
            .to_str()
            .filter(|name| node.files.contains_key(*name) || node.directories.contains_key(*name));
        match known {
            Some(name) => {
                present.insert(name);
            }
            None => return Err(VerifyError::Added),
        }
    }
    for (name, &position) in &node.files {
        if !present.contains(name.as_str()) {
            return Err(VerifyError::Missing {
                entry: Some(position),
            });
        }
        verify_file(directory, name, position, manifest)?;
    }
    for (name, subtree) in &node.directories {
        if !present.contains(name.as_str()) {
            return Err(VerifyError::Missing { entry: None });
        }
        let child = open_directory(directory, name)?.ok_or(VerifyError::Missing { entry: None })?;
        verify_directory(&child, subtree, manifest)?;
    }
    Ok(())
}

fn verify_file(
    directory: &TrustedDir,
    name: &str,
    position: usize,
    manifest: &Manifest,
) -> Result<(), VerifyError> {
    let file = &manifest.files[position];
    let mut handle = directory
        .open_file(name, file_mode(file))
        .map_err(|error| map_fs(&error, Some(position)))?
        .ok_or(VerifyError::Missing {
            entry: Some(position),
        })?;
    let hash = stream_sha256(&mut handle, file.size)
        .map_err(|error| VerifyError::Unreadable { kind: error.kind() })?;
    if hash.len != file.size || hash.hex != file.sha256 {
        return Err(VerifyError::Modified { entry: position });
    }
    Ok(())
}

/// Classifies a filesystem failure without carrying its path or text.
pub(crate) fn map_fs(error: &FsError, entry: Option<usize>) -> VerifyError {
    match error {
        FsError::UnsafeType { .. } => VerifyError::WrongType { entry },
        FsError::UnsafeMode { .. } | FsError::UnsafeAcl { .. } => VerifyError::WrongMode { entry },
        FsError::UnsafeOwner { .. } => VerifyError::WrongOwner { entry },
        FsError::UnsafeLinkCount { .. } => VerifyError::Hardlinked { entry },
        FsError::FileTooLarge { .. } => match entry {
            Some(entry) => VerifyError::Modified { entry },
            None => VerifyError::ManifestInvalid,
        },
        other => VerifyError::Unreadable {
            kind: other.io_kind().unwrap_or(ErrorKind::Other),
        },
    }
}
