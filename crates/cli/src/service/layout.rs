//! Version directories, the installed CLI copy, and their removal.
//!
//! A version directory `<prefix>/libexec/pohunek/<version>/` holds
//! `pohunekd`, `pohunek-sessiond`, and a copy of `pohunek`. It is assembled
//! in a private staging directory next to it, each binary's `--version` is
//! probed there, and only then is it renamed into place without replacement.
//! An existing version directory is never overwritten; identical contents
//! make publishing idempotent.

// Rust guideline compliant 2026-09-24

use std::ffi::OsStr;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use pohunek_paths::{
    valid_install_version, InstallLayout, DAEMON_EXECUTABLE_NAME, WORKER_EXECUTABLE_NAME,
};
use pohunek_platform::filesystem::{EntryKind, MoveOutcome, StageOutcome, TrustedDir};
use tokio::io::AsyncReadExt as _;

use super::error::{fs_error, io_error, Error};
use super::settings::{VERSION_PROBE_OUTPUT, VERSION_PROBE_TIMEOUT};

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
            let identical = same_binaries(&staged.path, &versions.path().join(version));
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
/// Returns [`Error::VersionConflict`] when contents differ or are missing.
pub fn verify_existing(
    layout: &InstallLayout,
    staged: &Staged,
    version: &str,
) -> Result<(), Error> {
    let path = layout
        .version_dir(version)
        .expect("callers pass validated versions");
    if same_binaries(&staged.path, &path)? {
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
        .map_err(|source| fs_error("inspect version directory", source))
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
            .map_err(|source| fs_error("inspect version directory", source))?
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
    for version in installed_versions(layout)? {
        let candidate = layout
            .version_dir(&version)
            .expect("listed versions are valid")
            .join(CLI_NAME);
        if regular_file(&candidate)? && same_contents(&cli, &candidate)? {
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

/// Runs `<binary> --version` with an empty environment and checks its answer.
async fn probe(binary: &Path, name: &str, version: &str) -> Result<(), Error> {
    let failure = |detail: String| Error::VersionProbe {
        binary: binary.to_path_buf(),
        detail,
    };
    let mut child = tokio::process::Command::new(binary)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
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

fn same_binaries(left: &Path, right: &Path) -> Result<bool, Error> {
    for binary in BINARIES {
        let (left, right) = (left.join(binary), right.join(binary));
        if !regular_file(&left)? || !regular_file(&right)? || !same_contents(&left, &right)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Compares two files byte by byte in bounded chunks.
fn same_contents(left: &Path, right: &Path) -> Result<bool, Error> {
    /// Chunk size for streaming comparison; binaries can be hundreds of MiB.
    const CHUNK: usize = 64 * 1024;

    let mut left_file = File::open(left).map_err(io_error("open", left))?;
    let mut right_file = File::open(right).map_err(io_error("open", right))?;
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
        let read = read_full(&mut left_file, &mut left_buffer).map_err(io_error("read", left))?;
        let other =
            read_full(&mut right_file, &mut right_buffer).map_err(io_error("read", right))?;
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
        std::fs::write(
            &path,
            format!("#!/bin/sh\n[ \"$1\" = --version ] && echo '{name} {version}'\n"),
        )
        .expect("write fake binary");
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).expect("chmod");
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
        std::fs::write(
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
        std::fs::write(from.join(DAEMON_EXECUTABLE_NAME), "#!/bin/sh\nexit 3\n")
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
