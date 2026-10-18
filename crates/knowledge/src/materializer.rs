//! Durably publish the embedded bundle below an owner-private cache root.

use std::io;
use std::path::{Component, Path, PathBuf};

const KNOWLEDGE_DIR: &str = "knowledge";
const COMPLETE_MARKER: &str = ".complete";

/// Extract the bundle durably into its version directory, repairing incomplete
/// publications. Concurrent materializers and garbage collectors serialize on
/// the private knowledge root. Garbage collection is best effort.
///
/// # Errors
///
/// Returns an error for unsafe paths, invalid version names, or any failed
/// extraction or durability operation. Unsupported platforms fail closed.
pub fn materialize(cache_dir: impl AsRef<Path>, version_hash: &str) -> io::Result<PathBuf> {
    validate_version_hash(version_hash)?;
    implementation::materialize(cache_dir.as_ref(), version_hash, &mut |_, _| Ok(()))
}

/// Collect old versions and abandoned temporary directories under the private
/// knowledge root. A held materializer lock always preserves its temporary dir.
///
/// # Errors
///
/// Returns an error for invalid names, unsafe paths, or failed collection.
pub fn gc(cache_dir: impl AsRef<Path>, keep: &str) -> io::Result<()> {
    validate_version_hash(keep)?;
    implementation::gc(cache_dir.as_ref(), keep)
}

fn validate_version_hash(version: &str) -> io::Result<()> {
    let mut components = Path::new(version).components();
    let valid = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !version.starts_with(".tmp-")
        && !version.starts_with(".pohunek-")
        && version != ".materialize.lock";
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "knowledge version hash must be a non-reserved single path segment",
        ))
    }
}

#[cfg(unix)]
mod implementation {
    use super::{COMPLETE_MARKER, KNOWLEDGE_DIR};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    use include_dir::Dir;
    use pohunek_platform::filesystem::{
        AdvisoryLock, EntryKind, FsError, MoveOutcome, StageOutcome, TrustedDir,
    };

    const PRIVATE_DIR_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;
    const ROOT_LOCK: &str = ".materialize.lock";
    const OWNER_LOCK: &str = ".owner";
    const TEMP_PREFIX: &str = ".tmp-v2-";
    // Contended materializers should yield the CPU without adding a noticeable
    // delay after a short extraction releases the root lock.
    const ROOT_LOCK_RETRY: Duration = Duration::from_millis(5);
    // Materialization normally takes milliseconds, and even a slow host should
    // finish within minutes. One day avoids racing older, slow preparations.
    // Current writers also hold the root lock; old non-cooperating binaries
    // cannot receive that guarantee and must not run concurrently during GC.
    const LEGACY_TEMP_MIN_AGE: Duration = Duration::from_hours(24);

    pub(super) fn materialize(
        cache: &Path,
        version: &str,
        observer: &mut Observer<'_>,
    ) -> io::Result<PathBuf> {
        let root =
            open_root(cache, true)?.ok_or_else(|| io::Error::other("knowledge root missing"))?;
        let _lock = lock_root(&root)?;
        let target = root.path().join(version);
        if let Some(directory) = target_directory(&root, version)? {
            if is_complete(&directory)? {
                // Older bundles used the same marker without synchronizing their
                // contents. Synchronize them too before promising durability.
                sync_tree(&directory, observer)?;
                sync_directory(&root, Event::ParentSync, observer)?;
                let _ = collect(&root, version);
                return Ok(target);
            }
            let identity = root
                .entry_identity(version, EntryKind::Directory)
                .map_err(fs_error)?
                .ok_or_else(|| io::Error::other("incomplete knowledge version disappeared"))?;
            let retired = temporary_name(version);
            match root.stage(version, &retired, identity).map_err(fs_error)? {
                StageOutcome::Staged(_) => {
                    let directory = root
                        .open_child_owner_safe(&retired, 0o022)
                        .map_err(fs_error)?;
                    let _owner = directory
                        .acquire_lock(OWNER_LOCK, PRIVATE_FILE_MODE)
                        .map_err(fs_error)?;
                }
                _ => {
                    return Err(io::Error::other(
                        "knowledge version identity changed during repair",
                    ))
                }
            }
        }

        let name = temporary_name(version);
        let temp = root
            .create_child_exclusive(&name, PRIVATE_DIR_MODE)
            .map_err(fs_error)?;
        let owner = temp
            .acquire_lock(OWNER_LOCK, PRIVATE_FILE_MODE)
            .map_err(fs_error)?;
        let result = publish(&root, &temp, &name, version, observer);
        drop(owner);
        if result.is_err() {
            let _ = remove_directory(&root, &name);
        }
        result?;
        let _ = collect(&root, version);
        Ok(target)
    }

    pub(super) fn gc(cache: &Path, keep: &str) -> io::Result<()> {
        let Some(root) = open_root(cache, false)? else {
            return Ok(());
        };
        let _lock = lock_root(&root)?;
        collect(&root, keep)
    }

    fn open_root(cache: &Path, create: bool) -> io::Result<Option<TrustedDir>> {
        let absolute = std::path::absolute(cache)?;
        let cache = if create {
            TrustedDir::open_or_create_absolute(&absolute, PRIVATE_DIR_MODE)
        } else {
            TrustedDir::open_absolute(&absolute, PRIVATE_DIR_MODE)
        };
        let cache = match cache {
            Ok(cache) => cache,
            Err(error) if !create && error.io_kind() == Some(io::ErrorKind::NotFound) => {
                return Ok(None)
            }
            Err(error) => return Err(fs_error(error)),
        };
        match if create {
            cache.open_or_create_child(KNOWLEDGE_DIR, PRIVATE_DIR_MODE)
        } else {
            cache.open_child(KNOWLEDGE_DIR, PRIVATE_DIR_MODE)
        } {
            Ok(root) => Ok(Some(root)),
            Err(error) if !create && error.io_kind() == Some(io::ErrorKind::NotFound) => Ok(None),
            Err(error) => Err(fs_error(error)),
        }
    }

    fn lock_root(root: &TrustedDir) -> io::Result<AdvisoryLock> {
        loop {
            match root.acquire_lock(ROOT_LOCK, PRIVATE_FILE_MODE) {
                Ok(lock) => return Ok(lock),
                Err(FsError::LockContended { .. }) => std::thread::sleep(ROOT_LOCK_RETRY),
                Err(error) => return Err(fs_error(error)),
            }
        }
    }

    fn target_directory(root: &TrustedDir, name: &str) -> io::Result<Option<TrustedDir>> {
        if root
            .entry_identity(name, EntryKind::Directory)
            .map_err(fs_error)?
            .is_none()
        {
            return Ok(None);
        }
        root.open_child_owner_safe(name, 0o022)
            .map(Some)
            .map_err(fs_error)
    }

    fn is_complete(directory: &TrustedDir) -> io::Result<bool> {
        directory
            .entry_identity(COMPLETE_MARKER, EntryKind::RegularFile)
            .map(|identity| identity.is_some())
            .map_err(fs_error)
    }

    fn publish(
        root: &TrustedDir,
        temp: &TrustedDir,
        name: &OsStr,
        version: &str,
        observer: &mut Observer<'_>,
    ) -> io::Result<()> {
        extract(crate::assistant::embedded_bundle().as_dir(), temp, observer)?;
        write_synced_file(temp, OsStr::new(COMPLETE_MARKER), b"complete\n", observer)?;
        sync_directory(temp, Event::TempSync, observer)?;
        observer(Event::Publish, &root.path().join(version))?;
        match root
            .move_no_replace(name, root, version)
            .map_err(fs_error)?
        {
            MoveOutcome::Moved => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "knowledge version appeared during publication",
                ))
            }
        }
        sync_directory(root, Event::ParentSync, observer)
    }

    fn write_synced_file(
        directory: &TrustedDir,
        name: &OsStr,
        contents: &[u8],
        observer: &mut Observer<'_>,
    ) -> io::Result<()> {
        observer(Event::FileSync, &directory.path().join(name))?;
        directory
            .create_file(name, contents, PRIVATE_FILE_MODE)
            .map(|_| ())
            .map_err(fs_error)
    }

    fn sync_directory(
        directory: &TrustedDir,
        event: Event,
        observer: &mut Observer<'_>,
    ) -> io::Result<()> {
        observer(event, directory.path())?;
        directory.sync().map_err(fs_error)
    }

    fn extract(
        bundle: &Dir<'_>,
        target: &TrustedDir,
        observer: &mut Observer<'_>,
    ) -> io::Result<()> {
        for file in bundle.files() {
            let name = file
                .path()
                .file_name()
                .ok_or_else(|| io::Error::other("embedded file lacks a name"))?;
            write_synced_file(target, name, file.contents(), observer)?;
        }
        for directory in bundle.dirs() {
            let name = directory
                .path()
                .file_name()
                .ok_or_else(|| io::Error::other("embedded directory lacks a name"))?;
            let child = target
                .create_child_exclusive(name, PRIVATE_DIR_MODE)
                .map_err(fs_error)?;
            extract(directory, &child, observer)?;
        }
        sync_directory(target, Event::DirectorySync, observer)
    }

    fn sync_tree(directory: &TrustedDir, observer: &mut Observer<'_>) -> io::Result<()> {
        for name in directory.entry_names().map_err(fs_error)? {
            if name == OsStr::new(OWNER_LOCK) {
                continue;
            }
            // `entry_identity` validates the kind without following links. The
            // metadata lookup only chooses the secure operation to perform.
            let metadata = fs::symlink_metadata(directory.path().join(&name))?;
            if metadata.is_dir() {
                let child = directory
                    .open_child_owner_safe(&name, 0o022)
                    .map_err(fs_error)?;
                sync_tree(&child, observer)?;
            } else {
                let file = directory
                    .open_file(&name, metadata.permissions().mode() & 0o777)
                    .map_err(fs_error)?
                    .ok_or_else(|| io::Error::other("knowledge file disappeared"))?;
                observer(Event::FileSync, &directory.path().join(&name))?;
                pohunek_platform::filesystem::sync_file(&file).map_err(fs_error)?;
            }
        }
        sync_directory(directory, Event::DirectorySync, observer)
    }

    use std::os::unix::fs::PermissionsExt as _;

    fn collect(root: &TrustedDir, keep: &str) -> io::Result<()> {
        for name in root.entry_names().map_err(fs_error)? {
            if name == OsStr::new(keep) || name == OsStr::new(ROOT_LOCK) {
                continue;
            }
            let metadata = fs::symlink_metadata(root.path().join(&name))?;
            if !metadata.is_dir() {
                continue;
            }
            let directory = root.open_child_owner_safe(&name, 0o022).map_err(fs_error)?;
            let temporary = name.to_str().is_some_and(|name| name.starts_with(".tmp-"));
            let mut owner = None;
            if temporary {
                let protocol = name
                    .to_str()
                    .is_some_and(|name| name.starts_with(TEMP_PREFIX))
                    && directory
                        .entry_identity(OWNER_LOCK, EntryKind::RegularFile)
                        .map_err(fs_error)?
                        .is_some();
                if !protocol && (legacy_owner_may_be_alive(&name) || !old_enough(&metadata)?) {
                    continue;
                }
                match directory.acquire_lock(OWNER_LOCK, PRIVATE_FILE_MODE) {
                    Ok(lock) => owner = Some(lock),
                    Err(FsError::LockContended { .. }) => continue,
                    Err(error) => return Err(fs_error(error)),
                }
            }
            remove_directory(root, &name)?;
            drop(owner);
        }
        Ok(())
    }

    fn legacy_owner_may_be_alive(name: &OsStr) -> bool {
        use pohunek_platform::process::{HostInspector, ProcessInspector as _};
        let Some(name) = name.to_str() else {
            return false;
        };
        let mut parts = name.rsplitn(4, '-');
        let Some(sequence) = parts.next() else {
            return false;
        };
        let Some(nanos) = parts.next() else {
            return false;
        };
        let Some(pid) = parts.next() else {
            return false;
        };
        if sequence.parse::<u64>().is_err() || nanos.parse::<u128>().is_err() {
            return false;
        }
        let Ok(pid) = pid.parse::<u32>() else {
            return false;
        };
        // Old names have no process start identity. PID reuse or an inspection
        // failure cannot prove that the old writer is gone, so retain it.
        !matches!(HostInspector::new().process(pid), Ok(None))
    }

    fn old_enough(metadata: &fs::Metadata) -> io::Result<bool> {
        Ok(SystemTime::now()
            .duration_since(metadata.modified()?)
            .is_ok_and(|age| age >= LEGACY_TEMP_MIN_AGE))
    }

    fn remove_directory(root: &TrustedDir, name: &OsStr) -> io::Result<()> {
        let Some(identity) = root
            .entry_identity(name, EntryKind::Directory)
            .map_err(fs_error)?
        else {
            return Ok(());
        };
        match root
            .stage_random(name, ".tmp-v2-gc-", identity)
            .map_err(fs_error)?
        {
            StageOutcome::Staged(entry) => {
                entry.remove_tree().map_err(fs_error)?;
                Ok(())
            }
            _ => Err(io::Error::other(
                "knowledge directory identity changed during collection",
            )),
        }
    }

    fn temporary_name(version: &str) -> OsString {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        OsString::from(format!(
            "{TEMP_PREFIX}{version}-{}-{nanos}-{sequence}",
            std::process::id()
        ))
    }

    fn fs_error(error: FsError) -> io::Error {
        let kind = error.io_kind().unwrap_or(match error {
            FsError::LockContended { .. } => io::ErrorKind::WouldBlock,
            _ => io::ErrorKind::InvalidInput,
        });
        io::Error::new(kind, error)
    }

    use super::{Event, Observer};
}

#[doc(hidden)]
#[cfg_attr(
    not(feature = "test-util"),
    expect(
        unreachable_pub,
        reason = "only test-util reexports the durability observer event"
    )
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    /// Write and durably synchronize a file.
    FileSync,
    /// Synchronize an extracted or reused directory.
    DirectorySync,
    /// Synchronize the completed temporary bundle before publication.
    TempSync,
    /// Atomically publish the prepared bundle.
    Publish,
    /// Synchronize the knowledge root after publication or reuse.
    ParentSync,
}

type Observer<'a> = dyn FnMut(Event, &Path) -> io::Result<()> + 'a;

/// Run real materialization with a per-call observer for durability tests.
/// Observer errors fail the corresponding operation before it commits.
///
/// # Errors
///
/// Returns the same errors as [`materialize`], or an error from the observer.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub fn materialize_with_observer(
    cache: impl AsRef<Path>,
    version: &str,
    mut observer: impl FnMut(Event, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    validate_version_hash(version)?;
    implementation::materialize(cache.as_ref(), version, &mut observer)
}

#[cfg(not(unix))]
mod implementation {
    use std::{
        io,
        path::{Path, PathBuf},
    };
    pub(super) fn materialize(
        _: &Path,
        _: &str,
        _: &mut super::Observer<'_>,
    ) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable knowledge materialization requires Unix filesystem primitives",
        ))
    }
    pub(super) fn gc(_: &Path, _: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable knowledge collection requires Unix filesystem primitives",
        ))
    }
}
