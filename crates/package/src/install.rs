//! Descriptor-relative extraction of a verified archive into a package root.
//!
//! The archive tree is written below a fresh owner-private staging directory
//! of the `packages` directory, with every create, mode change and sync done
//! through directory descriptors. The staged tree is verified against its
//! manifest and only then moved, without replacement, to
//! `packages/<hex>/`. A crash at any point leaves either no root or a complete
//! verified one, plus staging directories that [`collect_staging`] removes.
//!
//! Installs of one `packages` directory must be serialized by the caller (the
//! registry holds an exclusive lock for it); the staging name is derived from
//! the digest, so two concurrent installers of the same digest collide on it
//! instead of corrupting each other.

// Rust guideline compliant 2026-10-04

use std::io::ErrorKind;

use pohunek_platform::filesystem::{EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir};
use thiserror::Error;

use crate::layout::{
    digest_hex, COLLECT_PREFIX, DIRECTORY_MODE, EXECUTABLE_MODE, FILES_DIR, FILE_MODE,
    MANIFEST_NAME, STAGING_PREFIX,
};
use crate::limits::Limits;
use crate::manifest::Manifest;
use crate::verify::{verify_open_root, verify_root, VerifiedRoot, VerifyError};
use crate::VerifiedArchive;

/// Why extracting an archive into the package store failed.
///
/// Like [`VerifyError`], no variant carries a path or content.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum InstallError {
    /// The archive breaks a limit the store enforces on extraction.
    #[error("archive exceeds a package store limit")]
    LimitsExceeded,
    /// A root for this digest exists but does not hold this archive's files.
    #[error("an installed root for this digest holds different files")]
    RootConflict,
    /// A root for this digest exists and fails verification; it is not
    /// replaced.
    #[error("an installed root for this digest fails verification: {0}")]
    ExistingRootInvalid(#[source] VerifyError),
    /// A staging directory for this digest already exists; another install is
    /// running or an interrupted one was not collected.
    #[error("a staging directory for this digest already exists")]
    StagingExists,
    /// The extracted tree failed verification before it was published.
    #[error("the staged tree failed verification: {0}")]
    StagedTreeInvalid(#[source] VerifyError),
    /// The root was moved into place but the directory sync did not confirm
    /// it; verify the root before relying on it.
    #[error("the package root was published but durability is uncertain")]
    DurabilityUncertain,
    /// A staging residue is not a directory the store can safely remove.
    #[error("a staging residue is not a removable directory")]
    UnsafeResidue,
    /// The filesystem failed.
    #[error("filesystem failure while installing: {kind}")]
    Filesystem {
        /// Kind of the operating-system failure.
        kind: ErrorKind,
    },
}

/// What an install did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InstallOutcome {
    /// The root was extracted and published by this call.
    Created,
    /// A verified root with the same files was already installed.
    AlreadyPresent,
}

/// A published, verified package root.
#[derive(Debug)]
pub struct Installation {
    /// What the call did.
    pub outcome: InstallOutcome,
    /// The verified root.
    pub root: VerifiedRoot,
}

/// Extracts `archive` into `packages/<hex>/` and verifies the result.
///
/// Idempotent: when a root for the digest already exists and verifies against
/// the archive's files, it is returned unchanged. `packages` is the retained
/// descriptor of the `packages` directory.
///
/// # Errors
///
/// Returns [`InstallError`] for a limit breach, a conflicting or invalid
/// existing root, a staging collision or a filesystem failure. On every error
/// the staging directory this call created is removed on a best-effort basis;
/// [`collect_staging`] removes any that remain.
pub fn install_archive(
    packages: &TrustedDir,
    archive: &VerifiedArchive,
    limits: &Limits,
) -> Result<Installation, InstallError> {
    let manifest = Manifest::from_archive(archive);
    manifest
        .validate(limits)
        .map_err(|_cause| InstallError::LimitsExceeded)?;
    let digest = archive.digest();

    match verify_root(packages, digest, limits) {
        Ok(root) => return adopt_existing(root, &manifest),
        Err(VerifyError::RootMissing) => {}
        Err(error) => return Err(InstallError::ExistingRootInvalid(error)),
    }

    let staging_name = format!("{STAGING_PREFIX}{}", digest_hex(digest));
    let staging = packages
        .create_child_exclusive(&staging_name, DIRECTORY_MODE)
        .map_err(|error| match error.io_kind() {
            Some(ErrorKind::AlreadyExists) => InstallError::StagingExists,
            _ => fs_failure(&error),
        })?;

    let published = stage_and_publish(
        packages,
        &staging,
        &staging_name,
        archive,
        &manifest,
        limits,
    );
    drop(staging);
    match published {
        Ok(MoveOutcome::Moved) => {}
        Ok(_) => {
            // Another installer published this digest first; its root is
            // accepted only when it holds the same files.
            let _ = remove_directory(packages, &staging_name);
            return match verify_root(packages, digest, limits) {
                Ok(root) => adopt_existing(root, &manifest),
                Err(error) => Err(InstallError::ExistingRootInvalid(error)),
            };
        }
        Err(error) => {
            let _ = remove_directory(packages, &staging_name);
            return Err(error);
        }
    }

    let root = verify_root(packages, digest, limits).map_err(InstallError::StagedTreeInvalid)?;
    Ok(Installation {
        outcome: InstallOutcome::Created,
        root,
    })
}

/// Accepts an existing root only when it holds exactly this archive's files.
fn adopt_existing(root: VerifiedRoot, manifest: &Manifest) -> Result<Installation, InstallError> {
    if root.files() == manifest.files.as_slice() {
        Ok(Installation {
            outcome: InstallOutcome::AlreadyPresent,
            root,
        })
    } else {
        Err(InstallError::RootConflict)
    }
}

fn stage_and_publish(
    packages: &TrustedDir,
    staging: &TrustedDir,
    staging_name: &str,
    archive: &VerifiedArchive,
    manifest: &Manifest,
    limits: &Limits,
) -> Result<MoveOutcome, InstallError> {
    write_tree(staging, archive)?;
    staging
        .create_file(MANIFEST_NAME, &manifest.to_bytes(), FILE_MODE)
        .map_err(|error| fs_failure(&error))?;
    staging.sync().map_err(|error| fs_failure(&error))?;
    verify_open_root(staging, archive.digest(), limits).map_err(InstallError::StagedTreeInvalid)?;

    match packages.move_no_replace(staging_name, packages, digest_hex(archive.digest())) {
        Ok(outcome) => Ok(outcome),
        Err(FsError::CommittedDurabilityUncertain { .. }) => Err(InstallError::DurabilityUncertain),
        Err(error) => Err(fs_failure(&error)),
    }
}

/// Creates every directory and file of the archive below `staging/files`,
/// then syncs every directory.
fn write_tree(staging: &TrustedDir, archive: &VerifiedArchive) -> Result<(), InstallError> {
    let files = staging
        .create_child_exclusive(FILES_DIR, DIRECTORY_MODE)
        .map_err(|error| fs_failure(&error))?;
    // Entries arrive sorted by path, so the files of one directory are
    // contiguous. The open descriptors form the path of the current entry: a
    // directory is synced and closed as soon as the walk leaves it, which
    // bounds the descriptors held by the path depth, not by the directory
    // count.
    let mut open: Vec<(&str, TrustedDir)> = Vec::new();
    for entry in archive.entries() {
        let mut segments: Vec<&str> = entry.path.split('/').collect();
        let name = segments.pop().ok_or(InstallError::LimitsExceeded)?;
        let shared = open
            .iter()
            .zip(&segments)
            .take_while(|((open_name, _), segment)| open_name == *segment)
            .count();
        while open.len() > shared {
            let (_, finished) = open.pop().ok_or(InstallError::LimitsExceeded)?;
            finished.sync().map_err(|error| fs_failure(&error))?;
        }
        for segment in &segments[shared..] {
            let parent = open.last().map_or(&files, |(_, directory)| directory);
            let child = parent
                .open_or_create_child(segment, DIRECTORY_MODE)
                .map_err(|error| fs_failure(&error))?;
            open.push((segment, child));
        }
        let parent = open.last().map_or(&files, |(_, directory)| directory);
        let mode = if entry.executable {
            EXECUTABLE_MODE
        } else {
            FILE_MODE
        };
        parent
            .create_file(name, &entry.contents, mode)
            .map_err(|error| fs_failure(&error))?;
    }
    while let Some((_, finished)) = open.pop() {
        finished.sync().map_err(|error| fs_failure(&error))?;
    }
    files.sync().map_err(|error| fs_failure(&error))
}

/// Removes every staging residue of `packages` and returns how many it
/// removed.
///
/// The caller must hold the lock that serializes installs: a staging directory
/// seen under it belongs to an interrupted install, never to a running one.
///
/// # Errors
///
/// Returns [`InstallError::UnsafeResidue`] when a staging name is not a
/// directory, and [`InstallError::Filesystem`] when listing or removal fails.
pub fn collect_staging(packages: &TrustedDir) -> Result<usize, InstallError> {
    let names = packages.entry_names().map_err(|error| fs_failure(&error))?;
    let mut removed = 0;
    for name in names {
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(STAGING_PREFIX) || name.starts_with(COLLECT_PREFIX)) {
            continue;
        }
        if remove_directory(packages, name)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Removes one directory of `packages` by identity, through the platform
/// quarantine; `false` when it was already gone.
pub(crate) fn remove_directory(packages: &TrustedDir, name: &str) -> Result<bool, InstallError> {
    let identity = match packages.entry_identity(name, EntryKind::Directory) {
        Ok(Some(identity)) => identity,
        Ok(None) => return Ok(false),
        Err(FsError::UnsafeType { .. }) => return Err(InstallError::UnsafeResidue),
        Err(error) => return Err(fs_failure(&error)),
    };
    match packages
        .stage_random(name, COLLECT_PREFIX, identity)
        .map_err(|error| fs_failure(&error))?
    {
        StageOutcome::Staged(entry) => {
            entry.remove_tree().map_err(|error| fs_failure(&error))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn fs_failure(error: &FsError) -> InstallError {
    InstallError::Filesystem {
        kind: error.io_kind().unwrap_or(ErrorKind::Other),
    }
}
