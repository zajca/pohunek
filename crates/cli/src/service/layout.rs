//! Version directories, the installed CLI copy, and their removal.
//!
//! A version directory `<prefix>/libexec/pohunek/<version>/` holds
//! `pohunekd`, `pohunek-sessiond`, and a copy of `pohunek`. It is assembled
//! in a private staging directory next to it, each binary's `--version` is
//! probed there, and only then is it renamed into place without replacement.
//! An existing version directory is never overwritten; identical contents
//! make publishing idempotent. An existing directory is reused only when it is
//! exactly what publishing creates: owned by this user, mode `0755`, reached
//! without a symbolic link, and holding single-link `0755` regular files. The
//! service manager executes those files later, so anything another account
//! could replace between this check and that execution is refused.
//!
//! # Prefix ownership
//!
//! Version directories and `<prefix>/bin/pohunek` are keyed only by the
//! prefix, while worker journals, worker jobs, and the transaction lock are
//! per installation namespace. Garbage collection and uninstall can only see
//! the references of their own namespace, so one prefix serves exactly one
//! namespace: `<prefix>/libexec/pohunek/installation_owner` names it. Install
//! and upgrade create the record with no-replace semantics before staging
//! anything, every destructive operation verifies it first, and uninstall
//! removes it after the prefix is emptied.

// Rust guideline compliant 2026-10-01

use std::ffi::OsStr;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pohunek_paths::{
    valid_install_version, InstallLayout, DAEMON_EXECUTABLE_NAME, WORKER_EXECUTABLE_NAME,
};
use pohunek_platform::filesystem::{EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir};
use pohunek_platform::supervisor::Namespace;
use tokio::io::AsyncReadExt as _;

use super::error::{fs_error, io_error, Error};
use super::settings::{
    EXEC_BUSY_POLL, EXEC_BUSY_WAIT, VERSION_PROBE_OUTPUT, VERSION_PROBE_TIMEOUT,
};

/// File name of the CLI in a staging directory, a version directory, and `<prefix>/bin`.
pub const CLI_NAME: &str = "pohunek";

/// Every binary a version directory holds.
pub const BINARIES: [&str; 3] = [CLI_NAME, DAEMON_EXECUTABLE_NAME, WORKER_EXECUTABLE_NAME];

/// Mode of installed binaries and of the directories holding them.
///
/// Binaries are not secret; they only must not be writable by other users.
const PUBLIC_MODE: u32 = 0o755;

/// Mode of a staging directory while it is being filled.
const STAGING_MODE: u32 = 0o700;

/// Mode bits that make a directory unsafe to install into.
const FORBIDDEN_BITS: u32 = 0o022;

/// Name prefix of staging directories inside the versions directory.
const STAGING_PREFIX: &str = ".pohunek-staging-";

/// Name prefix of entries staged for removal.
const REMOVAL_PREFIX: &str = ".pohunek-removed-";

/// Name of the prefix ownership record inside the versions directory.
///
/// `_` is not allowed in a version, so the record is never listed, swept, or
/// collected as a version directory.
const OWNER_NAME: &str = "installation_owner";

/// Mode of the prefix ownership record.
const OWNER_MODE: u32 = 0o600;

/// Read bound of the prefix ownership record, which holds one namespace and a
/// newline; anything longer is not a record this module wrote.
const OWNER_MAX_BYTES: usize = pohunek_platform::supervisor::namespace::NAMESPACE_LEN + 1;

/// Disambiguates staging and temporary names created by one process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Opens an owner-managed directory, creating a missing one with `mode`.
///
/// Existing directories may have any mode that other users cannot write,
/// because `~/.local/bin` and friends are managed by the owner.
///
/// # Errors
///
/// Returns [`Error::UntrustedDirectory`] for a directory or ancestor others
/// can write, and a filesystem error otherwise.
pub fn open_owner_dir(path: &Path, mode: u32) -> Result<TrustedDir, Error> {
    match open_existing_owner_dir(path)? {
        Some(directory) => Ok(directory),
        None => TrustedDir::open_or_create_absolute(path, mode)
            .map_err(|source| fs_error("create directory", source)),
    }
}

/// Opens an existing owner-managed directory; a missing one is `None`.
///
/// # Errors
///
/// Returns [`Error::UntrustedDirectory`] for a directory or ancestor others
/// can write, and a filesystem error otherwise.
pub fn open_existing_owner_dir(path: &Path) -> Result<Option<TrustedDir>, Error> {
    match TrustedDir::open_absolute_owner_safe(path, FORBIDDEN_BITS) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => Ok(None),
        Err(source) => Err(fs_error("open directory", source)),
    }
}

/// Checks that a directory the service manager reads can be trusted.
///
/// A missing directory is fine as long as its nearest existing ancestor is,
/// because the backend creates it later. The check never changes a mode.
///
/// # Errors
///
/// Returns [`Error::UntrustedDirectory`] naming the offending directory.
pub fn check_trusted(path: &Path) -> Result<(), Error> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if open_existing_owner_dir(current)?.is_some() {
            return Ok(());
        }
        candidate = current.parent();
    }
    Ok(())
}

/// Returns the prefix ownership record's path.
#[must_use]
pub fn owner_record(layout: &InstallLayout) -> PathBuf {
    layout.versions_dir().join(OWNER_NAME)
}

/// Claims the prefix for `namespace`, creating its versions directory.
///
/// A missing record is created without replacement, so of two installations
/// claiming one prefix concurrently exactly one wins. An existing record must
/// name `namespace`. Returns whether this call created the record.
///
/// # Errors
///
/// Returns [`Error::PrefixOwned`] when another namespace owns the prefix and
/// a filesystem error for an unsafe directory or record.
pub fn claim_prefix(layout: &InstallLayout, namespace: &Namespace) -> Result<bool, Error> {
    let versions = open_owner_dir(&layout.versions_dir(), PUBLIC_MODE)?;
    claim(&versions, layout, namespace)
}

/// Claims the prefix for `namespace` when its versions directory exists.
///
/// Without a versions directory there is nothing to protect, and nothing is
/// created.
///
/// # Errors
///
/// See [`claim_prefix`].
pub fn claim_existing_prefix(layout: &InstallLayout, namespace: &Namespace) -> Result<(), Error> {
    match open_existing_owner_dir(&layout.versions_dir())? {
        Some(versions) => claim(&versions, layout, namespace).map(drop),
        None => Ok(()),
    }
}

/// Verifies that no other namespace claimed the prefix, without claiming it.
///
/// This is the read-only half of [`claim_prefix`]: a missing versions
/// directory or ownership record passes, because the claim would create it.
///
/// # Errors
///
/// Returns [`Error::PrefixOwned`] when another namespace owns the prefix and
/// a filesystem error for an unsafe directory or record.
pub fn verify_claim(layout: &InstallLayout, namespace: &Namespace) -> Result<(), Error> {
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(());
    };
    match owner(&versions, layout)? {
        Some(owner) if owner != *namespace => Err(prefix_owned(layout, &owner)),
        Some(_) | None => Ok(()),
    }
}

/// Gives up `namespace`'s claim on the prefix; returns whether a record existed.
///
/// # Errors
///
/// Returns [`Error::PrefixOwned`] when another namespace owns the prefix, and
/// a filesystem error when removal fails.
pub fn release_prefix(layout: &InstallLayout, namespace: &Namespace) -> Result<bool, Error> {
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(false);
    };
    match owner(&versions, layout)? {
        None => Ok(false),
        Some(owner) if owner == *namespace => {
            super::record::remove_file(&versions, OWNER_NAME)?;
            Ok(true)
        }
        Some(owner) => Err(prefix_owned(layout, &owner)),
    }
}

fn claim(
    versions: &TrustedDir,
    layout: &InstallLayout,
    namespace: &Namespace,
) -> Result<bool, Error> {
    let contents = format!("{}\n", namespace.as_str());
    match versions.create_file(OWNER_NAME, contents.as_bytes(), OWNER_MODE) {
        Ok(_identity) => return Ok(true),
        Err(error) if error.io_kind() == Some(io::ErrorKind::AlreadyExists) => {}
        Err(source) => return Err(fs_error("create prefix ownership record", source)),
    }
    match owner(versions, layout)? {
        Some(owner) if owner == *namespace => Ok(false),
        Some(owner) => Err(prefix_owned(layout, &owner)),
        // Removed by its owner's uninstall between the two calls; nothing
        // may be decided on a prefix whose ownership just changed.
        None => Err(Error::Io {
            operation: "claim the prefix",
            path: owner_record(layout),
            source: io::Error::other("the ownership record changed while it was being claimed"),
        }),
    }
}

/// Reads the namespace the ownership record names; a missing record is `None`.
fn owner(versions: &TrustedDir, layout: &InstallLayout) -> Result<Option<Namespace>, Error> {
    let bytes = match versions.read_file(OWNER_NAME, OWNER_MODE, OWNER_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => return Ok(None),
        Err(source) => return Err(fs_error("read prefix ownership record", source)),
    };
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
        .and_then(|text| Namespace::parse(text).ok())
        .map(Some)
        .ok_or_else(|| Error::Io {
            operation: "read prefix ownership record",
            path: owner_record(layout),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "the record does not name an installation namespace",
            ),
        })
}

fn prefix_owned(layout: &InstallLayout, owner: &Namespace) -> Error {
    Error::PrefixOwned {
        prefix: layout.prefix().to_path_buf(),
        owner: owner.as_str().to_owned(),
        record: owner_record(layout),
    }
}

/// Binaries copied into a private staging directory and verified there.
#[derive(Debug)]
pub struct Staged {
    name: String,
    path: PathBuf,
}

impl Staged {
    /// Returns the staging directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Copies the three binaries from `from` into a staging directory and probes them.
///
/// Every staged copy must answer `--version` with its own name and
/// `reported`, which is the version being installed except under the
/// `test-util` version override. The probe runs the copy, never the source,
/// so what is verified is exactly what gets installed.
///
/// # Errors
///
/// Returns [`Error::StagedBinary`] for a missing source,
/// [`Error::VersionProbe`] or [`Error::VersionMismatch`] for a failed probe,
/// and filesystem errors for unsafe directories.
pub async fn stage(layout: &InstallLayout, from: &Path, reported: &str) -> Result<Staged, Error> {
    let versions = open_owner_dir(&layout.versions_dir(), PUBLIC_MODE)?;
    sweep(&versions)?;
    let name = format!(
        "{STAGING_PREFIX}{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let staging = versions
        .create_child_exclusive(&name, STAGING_MODE)
        .map_err(|source| fs_error("create staging directory", source))?;
    let staged = Staged {
        name,
        path: staging.path().to_path_buf(),
    };
    let result = fill(&staging, from, reported).await;
    if let Err(error) = result {
        // The staging directory is private scratch; removing it cannot
        // affect an installation, and the next run sweeps it otherwise.
        let _cleanup = remove_tree(&versions, &staged.name);
        return Err(error);
    }
    Ok(staged)
}

async fn fill(staging: &TrustedDir, from: &Path, version: &str) -> Result<(), Error> {
    for binary in BINARIES {
        copy_binary(&from.join(binary), &staging.path().join(binary))?;
    }
    for binary in BINARIES {
        probe(&staging.path().join(binary), binary, version).await?;
    }
    std::fs::set_permissions(staging.path(), Permissions::from_mode(PUBLIC_MODE))
        .map_err(io_error("set the mode of", staging.path()))?;
    staging
        .sync()
        .map_err(|source| fs_error("synchronize staging directory", source))
}

/// Publishes a staged directory as `version` without replacing anything.
///
/// Returns `true` when this call created the version directory and `false`
/// when an identical one already existed.
///
/// # Errors
///
/// Returns [`Error::VersionConflict`] when an existing version directory
/// holds different binaries; the staging directory is removed either way.
pub fn publish(layout: &InstallLayout, staged: &Staged, version: &str) -> Result<bool, Error> {
    let versions = open_owner_dir(&layout.versions_dir(), PUBLIC_MODE)?;
    let outcome = versions
        .move_no_replace(&staged.name, &versions, version)
        .map_err(|source| fs_error("publish version directory", source))?;
    match outcome {
        MoveOutcome::Moved => Ok(true),
        MoveOutcome::DestinationExists => {
            let identical = same_binaries(&versions, &staged.name, version);
            remove_tree(&versions, &staged.name)?;
            if identical? {
                Ok(false)
            } else {
                Err(Error::VersionConflict {
                    path: versions.path().join(version),
                })
            }
        }
        other => Err(Error::Io {
            operation: "publish version directory",
            path: versions.path().join(version),
            source: io::Error::other(format!("unexpected move outcome {other:?}")),
        }),
    }
}

/// Checks that an existing version directory holds exactly the staged binaries.
///
/// # Errors
///
/// Returns [`Error::VersionConflict`] when contents differ or are missing, and
/// [`Error::UntrustedVersion`] when the directory or a binary is not exactly
/// what publishing creates.
pub fn verify_existing(
    layout: &InstallLayout,
    staged: &Staged,
    version: &str,
) -> Result<(), Error> {
    let path = layout
        .version_dir(version)
        .expect("callers pass validated versions");
    let identical = match open_existing_owner_dir(&layout.versions_dir())? {
        Some(versions) => same_binaries(&versions, &staged.name, version)?,
        None => false,
    };
    if identical {
        Ok(())
    } else {
        Err(Error::VersionConflict { path })
    }
}

/// Removes a staging directory that was not published.
///
/// # Errors
///
/// Returns a filesystem error when removal fails.
pub fn discard(layout: &InstallLayout, staged: &Staged) -> Result<(), Error> {
    let versions = open_owner_dir(&layout.versions_dir(), PUBLIC_MODE)?;
    remove_tree(&versions, &staged.name).map(drop)
}

/// Returns whether `version` has a version directory.
///
/// # Errors
///
/// Returns a filesystem error when the versions directory is unsafe.
pub fn version_exists(layout: &InstallLayout, version: &str) -> Result<bool, Error> {
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(false);
    };
    versions
        .entry_identity(version, EntryKind::Directory)
        .map(|identity| identity.is_some())
        .map_err(|source| version_error(versions.path().join(version), source))
}

/// Lists installed version directories in ascending name order.
///
/// # Errors
///
/// Returns a filesystem error when the versions directory is unsafe.
pub fn installed_versions(layout: &InstallLayout) -> Result<Vec<String>, Error> {
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(Vec::new());
    };
    let names = versions
        .entry_names()
        .map_err(|source| fs_error("list version directories", source))?;
    let mut found = Vec::new();
    for name in names {
        let Some(name) = name.to_str().and_then(valid_install_version) else {
            continue;
        };
        if versions
            .entry_identity(name, EntryKind::Directory)
            .map_err(|source| version_error(versions.path().join(name), source))?
            .is_some()
        {
            found.push(name.to_owned());
        }
    }
    found.sort();
    Ok(found)
}

/// Removes one version directory; a missing one is not an error.
///
/// # Errors
///
/// Returns a filesystem error when removal fails.
pub fn remove_version(layout: &InstallLayout, version: &str) -> Result<bool, Error> {
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(false);
    };
    remove_tree(&versions, version)
}

/// Returns the version whose directory contains `path`, if any.
#[must_use]
pub fn version_of(layout: &InstallLayout, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(layout.versions_dir()).ok()?;
    let first = relative.components().next()?;
    first
        .as_os_str()
        .to_str()
        .and_then(valid_install_version)
        .map(str::to_owned)
}

/// Atomically installs the version directory's CLI as `<prefix>/bin/pohunek`.
///
/// # Errors
///
/// Returns a filesystem error when `<prefix>/bin` is unsafe or the copy fails.
pub fn install_cli(layout: &InstallLayout, version: &str) -> Result<(), Error> {
    let source = layout
        .version_dir(version)
        .expect("callers pass validated versions")
        .join(CLI_NAME);
    let bin = open_owner_dir(&layout.bin_dir(), PUBLIC_MODE)?;
    let temporary = bin.path().join(format!(
        ".{CLI_NAME}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let destination = bin.path().join(CLI_NAME);
    let result = copy_binary(&source, &temporary)
        .and_then(|()| {
            std::fs::rename(&temporary, &destination)
                .map_err(io_error("install the CLI at", &destination))
        })
        .and_then(|()| {
            bin.sync()
                .map_err(|source| fs_error("synchronize bin directory", source))
        });
    if result.is_err() {
        // A failed copy leaves only this process's own temporary file.
        let _cleanup = std::fs::remove_file(&temporary);
    }
    result
}

/// Returns whether `<prefix>/bin/pohunek` is a copy of an installed version's CLI.
///
/// # Errors
///
/// Returns a filesystem error when a directory is unsafe.
pub fn installed_cli_copy(layout: &InstallLayout) -> Result<bool, Error> {
    let cli = layout.bin_dir().join(CLI_NAME);
    if !regular_file(&cli)? {
        return Ok(false);
    }
    let Some(versions) = open_existing_owner_dir(&layout.versions_dir())? else {
        return Ok(false);
    };
    for version in installed_versions(layout)? {
        let directory = open_version(&versions, &version)?;
        let Some(mut candidate) = open_binary(&directory, CLI_NAME)? else {
            continue;
        };
        let mut installed = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&cli)
            .map_err(io_error("open", &cli))?;
        let candidate_path = directory.path().join(CLI_NAME);
        if same_contents((&mut installed, &cli), (&mut candidate, &candidate_path))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes `<prefix>/bin/pohunek`.
///
/// # Errors
///
/// Returns a filesystem error when removal fails.
pub fn remove_cli(layout: &InstallLayout) -> Result<(), Error> {
    let Some(bin) = open_existing_owner_dir(&layout.bin_dir())? else {
        return Ok(());
    };
    super::record::remove_file(&bin, CLI_NAME)
}

/// Removes one directory tree below a trusted parent; absence is not an error.
///
/// # Errors
///
/// Returns a filesystem error when the entry is unsafe or removal fails.
pub fn remove_tree(parent: &TrustedDir, name: &str) -> Result<bool, Error> {
    const OPERATION: &str = "remove directory";

    let Some(identity) = parent
        .entry_identity(name, EntryKind::Directory)
        .map_err(|source| fs_error(OPERATION, source))?
    else {
        return Ok(false);
    };
    match parent
        .stage_random(name, REMOVAL_PREFIX, identity)
        .map_err(|source| fs_error(OPERATION, source))?
    {
        StageOutcome::Staged(entry) => entry
            .remove_tree()
            .map(|_outcome| true)
            .map_err(|source| fs_error(OPERATION, source)),
        StageOutcome::Missing => Ok(false),
        _ => Err(Error::Io {
            operation: OPERATION,
            path: parent.path().join(name),
            source: io::Error::other("the directory changed while it was being removed"),
        }),
    }
}

/// Removes leftovers of interrupted staging and removal.
fn sweep(versions: &TrustedDir) -> Result<(), Error> {
    let names = versions
        .entry_names()
        .map_err(|source| fs_error("list version directories", source))?;
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(STAGING_PREFIX) || name.starts_with(REMOVAL_PREFIX) {
            remove_tree(versions, name)?;
        }
    }
    Ok(())
}

/// Streams one regular file into a new `0755` file without following links.
fn copy_binary(source: &Path, destination: &Path) -> Result<(), Error> {
    let mut input = File::open(source).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Error::StagedBinary {
                path: source.to_path_buf(),
            }
        } else {
            io_error("open", source)(error)
        }
    })?;
    let metadata = input.metadata().map_err(io_error("inspect", source))?;
    if !metadata.is_file() {
        return Err(Error::StagedBinary {
            path: source.to_path_buf(),
        });
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PUBLIC_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)
        .map_err(io_error("create", destination))?;
    io::copy(&mut input, &mut output).map_err(io_error("copy into", destination))?;
    // The creation mode is filtered by the umask; set it explicitly.
    output
        .set_permissions(Permissions::from_mode(PUBLIC_MODE))
        .map_err(io_error("set the mode of", destination))?;
    output
        .sync_all()
        .map_err(io_error("synchronize", destination))
}

/// Whether `error` is an `exec` of a file that is open for writing.
pub(crate) fn is_exec_busy(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ExecutableFileBusy
}

/// Runs `attempt` until it stops failing with `ETXTBSY` or `wait` has passed.
///
/// Every other result, and the busy error once `wait` is exhausted, is
/// returned unchanged. `poll` separates the attempts.
pub(crate) async fn retry_while_exec_busy<T>(
    wait: Duration,
    poll: Duration,
    mut attempt: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match attempt() {
            Err(error) if is_exec_busy(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(poll).await;
            }
            result => return result,
        }
    }
}

/// Runs `<binary> --version` with an empty environment and checks its answer.
pub(crate) async fn probe(binary: &Path, name: &str, version: &str) -> Result<(), Error> {
    let failure = |detail: String| Error::VersionProbe {
        binary: binary.to_path_buf(),
        detail,
    };
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // The binary was written by this process moments ago, so another thread's
    // child can still hold a copy of the write descriptor.
    let mut child = retry_while_exec_busy(EXEC_BUSY_WAIT, EXEC_BUSY_POLL, || command.spawn())
        .await
        .map_err(|error| failure(format!("cannot run it: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as a pipe");
    let run = async {
        let limit = u64::try_from(VERSION_PROBE_OUTPUT).unwrap_or(u64::MAX);
        let mut output = Vec::new();
        stdout
            .take(limit)
            .read_to_end(&mut output)
            .await
            .map_err(|error| failure(format!("cannot read its output: {error}")))?;
        let status = child
            .wait()
            .await
            .map_err(|error| failure(format!("cannot wait for it: {error}")))?;
        Ok::<_, Error>((status, output))
    };
    let (status, output) = tokio::time::timeout(VERSION_PROBE_TIMEOUT, run)
        .await
        .map_err(|_elapsed| {
            failure(format!(
                "`--version` did not finish within {} s",
                VERSION_PROBE_TIMEOUT.as_secs()
            ))
        })??;
    if !status.success() {
        return Err(failure(format!("`--version` exited with {status}")));
    }
    let line = String::from_utf8_lossy(&output).trim().to_owned();
    if line == format!("{name} {version}") {
        Ok(())
    } else {
        Err(Error::VersionMismatch {
            binary: binary.to_path_buf(),
            expected: version.to_owned(),
            found: line,
        })
    }
}

fn regular_file(path: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect", path)(error)),
    }
}

/// Compares the staging directory `staged` with the version directory
/// `version`, both below `versions`, through verified descriptors.
///
/// A missing binary makes them differ; an untrusted directory or binary on
/// either side fails instead.
fn same_binaries(versions: &TrustedDir, staged: &str, version: &str) -> Result<bool, Error> {
    let staged = open_version(versions, staged)?;
    let existing = open_version(versions, version)?;
    for binary in BINARIES {
        let (Some(mut left), Some(mut right)) = (
            open_binary(&staged, binary)?,
            open_binary(&existing, binary)?,
        ) else {
            return Ok(false);
        };
        let (left_path, right_path) = (staged.path().join(binary), existing.path().join(binary));
        if !same_contents((&mut left, &left_path), (&mut right, &right_path))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Opens a version or staging directory below `versions` without following
/// a symbolic link and checks it is exactly what publishing creates.
fn open_version(versions: &TrustedDir, name: &str) -> Result<TrustedDir, Error> {
    versions
        .open_child(name, PUBLIC_MODE)
        .map_err(|source| version_error(versions.path().join(name), source))
}

/// Opens one binary of a verified version directory for reading.
///
/// The binary is opened relative to the directory descriptor without
/// following a symbolic link, and the descriptor must be a regular file of
/// this user with mode `0755`, one link, and on macOS no extended ACL granting
/// access, so the contents read are those of the verified file. A missing
/// binary is `None`.
fn open_binary(directory: &TrustedDir, name: &str) -> Result<Option<File>, Error> {
    directory
        .open_file(name, PUBLIC_MODE)
        .map_err(|source| version_error(directory.path().join(name), source))
}

/// Maps a failed verification of a version directory or binary at `path`.
///
/// A symbolic link or non-directory where a version directory belongs fails
/// the no-follow open with `ELOOP` or `ENOTDIR`.
fn version_error(path: PathBuf, source: FsError) -> Error {
    let detail = match &source {
        FsError::UnsafeType { actual, .. } => format!("it is a {actual}"),
        FsError::UnsafeOwner { actual, .. } => format!("it is owned by uid {actual}"),
        FsError::UnsafeMode { actual, .. } => {
            format!("its mode is {actual:#o}, not {PUBLIC_MODE:#o}")
        }
        FsError::UnsafeLinkCount { actual, .. } => format!("it has {actual} hard links"),
        FsError::UnsafeAcl { .. } => "an extended ACL grants other users access".to_owned(),
        FsError::Io { .. }
            if matches!(source.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) =>
        {
            "it is a symbolic link or not a directory".to_owned()
        }
        _ => return fs_error("inspect version directory", source),
    };
    Error::UntrustedVersion { path, detail }
}

/// Compares two open files byte by byte in bounded chunks.
fn same_contents(
    (left_file, left): (&mut File, &Path),
    (right_file, right): (&mut File, &Path),
) -> Result<bool, Error> {
    /// Chunk size for streaming comparison; binaries can be hundreds of MiB.
    const CHUNK: usize = 64 * 1024;

    if left_file
        .metadata()
        .map_err(io_error("inspect", left))?
        .len()
        != right_file
            .metadata()
            .map_err(io_error("inspect", right))?
            .len()
    {
        return Ok(false);
    }
    let mut left_buffer = vec![0_u8; CHUNK];
    let mut right_buffer = vec![0_u8; CHUNK];
    loop {
        let read = read_full(left_file, &mut left_buffer).map_err(io_error("read", left))?;
        let other = read_full(right_file, &mut right_buffer).map_err(io_error("read", right))?;
        if read != other || left_buffer[..read] != right_buffer[..other] {
            return Ok(false);
        }
        if read == 0 {
            return Ok(true);
        }
    }
}

fn read_full(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Returns whether a name is one of this module's scratch entries.
#[must_use]
pub fn is_scratch(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(STAGING_PREFIX) || name.starts_with(REMOVAL_PREFIX))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::service::context::tests::temp_root;

    /// Writes a fake staged binary answering `--version` like the real one.
    pub(crate) fn write_fake(dir: &Path, name: &str, version: &str) {
        std::fs::create_dir_all(dir).expect("create staged dir");
        let path = dir.join(name);
        pohunek_test_support::fs::write_executable(
            &path,
            format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '{name} {version}'\n"),
        )
        .expect("write fake binary");
    }

    fn busy() -> io::Error {
        io::ErrorKind::ExecutableFileBusy.into()
    }

    #[tokio::test(start_paused = true)]
    async fn exec_is_retried_while_the_file_is_busy_and_then_succeeds() {
        let mut busy_results = 3;
        let mut attempts = 0;
        let result = retry_while_exec_busy(EXEC_BUSY_WAIT, EXEC_BUSY_POLL, || {
            attempts += 1;
            if busy_results > 0 {
                busy_results -= 1;
                Err(busy())
            } else {
                Ok(attempts)
            }
        })
        .await;
        assert_eq!(result.expect("retried until free"), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_that_stays_busy_fails_once_the_wait_is_exhausted() {
        let mut attempts = 0_u32;
        let error = retry_while_exec_busy(EXEC_BUSY_WAIT, EXEC_BUSY_POLL, || {
            attempts += 1;
            Err::<(), _>(busy())
        })
        .await
        .expect_err("never free");
        assert!(is_exec_busy(&error), "{error:?}");
        assert!(attempts > 1, "{attempts}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_exec_error_other_than_busy_is_not_retried() {
        let mut attempts = 0_u32;
        let error = retry_while_exec_busy(EXEC_BUSY_WAIT, EXEC_BUSY_POLL, || {
            attempts += 1;
            Err::<(), _>(io::ErrorKind::NotFound.into())
        })
        .await
        .expect_err("not found");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(attempts, 1);
    }

    /// A descriptor open for writing, the state a sibling's pre-`exec` child
    /// leaves on a freshly written executable, makes `exec` fail with the
    /// error [`is_exec_busy`] recognizes.
    ///
    /// Linux only: Darwin lets `exec` succeed on a file that is still open for
    /// writing, so there is no busy state to report there.
    #[cfg(target_os = "linux")]
    #[test]
    fn exec_of_a_file_open_for_writing_is_reported_as_busy() {
        let (_root, root) = temp_root();
        write_fake(root.as_path(), "fake", "1.0.0");
        let path = root.as_path().join("fake");
        let writer = OpenOptions::new().write(true).open(&path).expect("open");

        let error = std::process::Command::new(&path)
            .arg("--version")
            .spawn()
            .expect_err("exec while a writer is open");
        assert!(is_exec_busy(&error), "{error:?}");

        // A sibling test thread's child can still hold a copy of the closed
        // descriptor, so the exec is retried like the probe's.
        drop(writer);
        let mut child = crate::service::usage::tests::spawn_when_exec_free(
            std::process::Command::new(&path)
                .arg("--version")
                .stdout(Stdio::null()),
        )
        .expect("exec once the writer is closed");
        assert!(child.wait().expect("wait").success());
    }

    /// The probe succeeds once a writer that was open at its first attempt
    /// closes: the join polls the probe first, so it meets the open writer
    /// before the writer is dropped. Where `exec` of such a file is refused as
    /// busy (Linux) the probe retries; elsewhere (Darwin) its first attempt
    /// already succeeds.
    #[tokio::test]
    async fn the_version_probe_waits_for_a_descriptor_that_is_still_open_for_writing() {
        let (_root, root) = temp_root();
        write_fake(root.as_path(), "fake", "1.0.0");
        let path = root.as_path().join("fake");
        let writer = OpenOptions::new().write(true).open(&path).expect("open");

        let (probed, ()) = tokio::join!(probe(&path, "fake", "1.0.0"), async move {
            tokio::task::yield_now().await;
            drop(writer);
        });
        probed.expect("the probe ran the binary after the writer closed");
    }

    pub(crate) fn stage_dir(root: &Path, version: &str) -> PathBuf {
        let from = root.join(format!("staged-{version}"));
        for binary in BINARIES {
            write_fake(&from, binary, version);
        }
        from
    }

    #[tokio::test]
    async fn stage_and_publish_create_an_immutable_version_directory() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let from = stage_dir(root.as_path(), "1.0.0");

        let staged = stage(&layout, &from, "1.0.0").await.expect("stage");
        assert!(publish(&layout, &staged, "1.0.0").expect("publish"));
        let dir = layout.version_dir("1.0.0").expect("version dir");
        for binary in BINARIES {
            let mode = std::fs::metadata(dir.join(binary))
                .expect("installed binary")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, PUBLIC_MODE, "{binary}");
        }
        assert_eq!(installed_versions(&layout).expect("list"), ["1.0.0"]);

        let again = stage(&layout, &from, "1.0.0").await.expect("stage again");
        assert!(!publish(&layout, &again, "1.0.0").expect("identical publish"));

        write_fake(&from, DAEMON_EXECUTABLE_NAME, "1.0.0");
        pohunek_test_support::fs::write_file(
            from.join(DAEMON_EXECUTABLE_NAME),
            "#!/bin/sh\necho 'pohunekd 1.0.0' # changed\n",
        )
        .expect("change binary");
        let different = stage(&layout, &from, "1.0.0")
            .await
            .expect("stage different");
        assert!(matches!(
            publish(&layout, &different, "1.0.0"),
            Err(Error::VersionConflict { .. })
        ));
        let leftovers: Vec<_> = std::fs::read_dir(layout.versions_dir())
            .expect("versions dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, ["1.0.0"], "staging directories are removed");
    }

    #[tokio::test]
    async fn a_mismatched_or_missing_binary_is_rejected_before_publishing() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let from = stage_dir(root.as_path(), "1.0.0");
        write_fake(&from, WORKER_EXECUTABLE_NAME, "0.9.0");
        let error = stage(&layout, &from, "1.0.0").await.expect_err("mismatch");
        assert!(
            matches!(&error, Error::VersionMismatch { found, .. } if found == "pohunek-sessiond 0.9.0"),
            "{error:?}"
        );

        std::fs::remove_file(from.join(CLI_NAME)).expect("remove cli");
        assert!(matches!(
            stage(&layout, &from, "1.0.0").await,
            Err(Error::StagedBinary { .. })
        ));
        assert!(installed_versions(&layout).expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_binary_that_ignores_version_is_a_probe_failure() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let from = stage_dir(root.as_path(), "1.0.0");
        pohunek_test_support::fs::write_file(
            from.join(DAEMON_EXECUTABLE_NAME),
            "#!/bin/sh\nexit 3\n",
        )
        .expect("broken daemon");
        assert!(matches!(
            stage(&layout, &from, "1.0.0").await,
            Err(Error::VersionProbe { .. })
        ));
    }

    #[tokio::test]
    async fn cli_copy_is_installed_atomically_and_recognized() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let from = stage_dir(root.as_path(), "1.0.0");
        let staged = stage(&layout, &from, "1.0.0").await.expect("stage");
        publish(&layout, &staged, "1.0.0").expect("publish");

        assert!(!installed_cli_copy(&layout).expect("absent copy"));
        install_cli(&layout, "1.0.0").expect("install cli");
        assert!(installed_cli_copy(&layout).expect("installed copy"));
        std::fs::write(layout.bin_dir().join(CLI_NAME), "someone else's pohunek")
            .expect("replace cli");
        assert!(!installed_cli_copy(&layout).expect("foreign copy"));
    }

    /// Publishes `1.0.0` and stages an identical copy for reuse checks.
    async fn published_and_restaged(root: &Path) -> (InstallLayout, Staged) {
        let layout = InstallLayout::new(root.join("prefix")).expect("layout");
        let from = stage_dir(root, "1.0.0");
        let staged = stage(&layout, &from, "1.0.0").await.expect("stage");
        assert!(publish(&layout, &staged, "1.0.0").expect("publish"));
        let again = stage(&layout, &from, "1.0.0").await.expect("stage again");
        (layout, again)
    }

    fn assert_untrusted(result: Result<impl std::fmt::Debug, Error>, expected: &Path) {
        let error = result.expect_err("an untrusted version is refused");
        assert!(
            matches!(&error, Error::UntrustedVersion { path, .. } if path == expected),
            "{error:?}"
        );
        assert_eq!(error.code(), "service_version_untrusted");
        assert!(error.hint().is_some());
    }

    #[tokio::test]
    async fn an_exact_existing_version_directory_is_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        verify_existing(&layout, &again, "1.0.0").expect("verified reuse");
        assert!(!publish(&layout, &again, "1.0.0").expect("identical publish"));
    }

    #[tokio::test]
    async fn a_writable_version_directory_is_never_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        let dir = layout.version_dir("1.0.0").expect("version dir");
        std::fs::set_permissions(&dir, Permissions::from_mode(0o775)).expect("chmod");
        assert_untrusted(verify_existing(&layout, &again, "1.0.0"), &dir);
        assert_untrusted(publish(&layout, &again, "1.0.0"), &dir);
        assert_eq!(
            installed_versions(&layout).expect("list"),
            ["1.0.0"],
            "the staging directory is removed either way"
        );
    }

    #[tokio::test]
    async fn a_writable_binary_is_never_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        let daemon = layout.daemon_executable("1.0.0").expect("daemon path");
        std::fs::set_permissions(&daemon, Permissions::from_mode(0o775)).expect("chmod");
        assert_untrusted(verify_existing(&layout, &again, "1.0.0"), &daemon);
    }

    #[tokio::test]
    async fn a_hard_linked_binary_is_never_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        let worker = layout.worker_executable("1.0.0").expect("worker path");
        std::fs::hard_link(&worker, root.as_path().join("second-name")).expect("hard link");
        assert_untrusted(verify_existing(&layout, &again, "1.0.0"), &worker);
    }

    #[tokio::test]
    async fn a_symlinked_binary_is_never_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        let cli = layout
            .version_dir("1.0.0")
            .expect("version dir")
            .join(CLI_NAME);
        let elsewhere = root.as_path().join("elsewhere");
        std::fs::rename(&cli, &elsewhere).expect("move CLI");
        std::os::unix::fs::symlink(&elsewhere, &cli).expect("symlink");
        assert_untrusted(verify_existing(&layout, &again, "1.0.0"), &cli);
    }

    #[tokio::test]
    async fn a_symlinked_version_directory_is_never_reused() {
        let (_root, root) = temp_root();
        let (layout, again) = published_and_restaged(root.as_path()).await;
        let dir = layout.version_dir("1.0.0").expect("version dir");
        let elsewhere = root.as_path().join("elsewhere");
        std::fs::rename(&dir, &elsewhere).expect("move version dir");
        std::os::unix::fs::symlink(&elsewhere, &dir).expect("symlink");
        assert_untrusted(verify_existing(&layout, &again, "1.0.0"), &dir);
        assert_untrusted(version_exists(&layout, "1.0.0"), &dir);
    }

    #[test]
    fn one_namespace_owns_a_prefix_until_it_releases_it() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let first = Namespace::parse("0123456789ab").expect("namespace");
        let second = Namespace::parse("ba9876543210").expect("namespace");

        assert!(!release_prefix(&layout, &first).expect("nothing to release"));
        claim_existing_prefix(&layout, &first).expect("nothing to claim");
        assert!(!layout.versions_dir().exists(), "nothing is created");

        assert!(claim_prefix(&layout, &first).expect("claim"));
        assert!(!claim_prefix(&layout, &first).expect("claim again"));
        claim_existing_prefix(&layout, &first).expect("verified claim");
        for error in [
            claim_prefix(&layout, &second).map(drop),
            claim_existing_prefix(&layout, &second),
            release_prefix(&layout, &second).map(drop),
        ] {
            let error = error.expect_err("another namespace is refused");
            assert!(
                matches!(&error, Error::PrefixOwned { owner, record, .. }
                    if owner == first.as_str() && record == &owner_record(&layout)),
                "{error:?}"
            );
            assert_eq!(error.code(), "service_prefix_owned");
        }
        assert!(
            installed_versions(&layout).expect("list").is_empty(),
            "the record is not a version"
        );

        assert!(release_prefix(&layout, &first).expect("release"));
        assert!(claim_prefix(&layout, &second).expect("claim after release"));
    }

    #[test]
    fn an_ownership_record_that_names_no_namespace_is_refused() {
        let (_root, root) = temp_root();
        let layout = InstallLayout::new(root.as_path().join("prefix")).expect("layout");
        let namespace = Namespace::parse("0123456789ab").expect("namespace");
        std::fs::create_dir_all(layout.versions_dir()).expect("versions dir");
        std::fs::write(owner_record(&layout), "not a namespace\n").expect("record");
        std::fs::set_permissions(owner_record(&layout), Permissions::from_mode(OWNER_MODE))
            .expect("chmod");
        assert!(matches!(
            claim_prefix(&layout, &namespace),
            Err(Error::Filesystem { .. } | Error::Io { .. })
        ));
        assert!(owner_record(&layout).exists(), "the record is kept");
    }

    #[test]
    fn untrusted_install_prefix_is_refused_with_the_directory_named() {
        let (_root, root) = temp_root();
        let prefix = root.as_path().join("prefix");
        std::fs::create_dir_all(prefix.join("libexec")).expect("prefix");
        std::fs::set_permissions(prefix.join("libexec"), Permissions::from_mode(0o777))
            .expect("chmod");
        let error = check_trusted(&prefix.join("libexec/pohunek")).expect_err("untrusted");
        assert!(
            matches!(&error, Error::UntrustedDirectory { path, .. } if path == &prefix.join("libexec")),
            "{error:?}"
        );
        assert!(error.to_string().contains("chmod go-w"));
    }

    #[test]
    fn version_of_maps_paths_inside_version_directories() {
        let layout = InstallLayout::new("/home/u/.local").expect("layout");
        assert_eq!(
            version_of(
                &layout,
                Path::new("/home/u/.local/libexec/pohunek/1.2.3/pohunek-sessiond")
            )
            .as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            version_of(&layout, Path::new("/home/u/src/target/debug/pohunekd")),
            None
        );
        assert_eq!(
            version_of(&layout, Path::new("/home/u/.local/libexec/pohunek/..")),
            None
        );
    }
}
