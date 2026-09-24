//! The journaled install transaction record, `<state>/pohunek/service-install.json`.
//!
//! The record exists only while an install or upgrade is in flight. It is
//! written owner-private (`0600`) with an atomic replace before each step's
//! effects become visible and after they complete, so a later run knows how
//! far the interrupted transaction got and can resume or roll it back.

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_platform::filesystem::{AdvisoryLock, EntryKind, FsError, StageOutcome, TrustedDir};
use serde::{Deserialize, Serialize};

use super::error::{fs_error, replace_error, Error};
use super::settings::{LOCK_POLL, LOCK_WAIT, MAX_RECORD_BYTES};

/// Version of the record schema; any other version is rejected.
pub const SCHEMA_VERSION: u32 = 1;

/// File name of the record inside the application state directory.
pub const FILE_NAME: &str = "service-install.json";

/// File name of the transaction lock inside the application state directory.
pub const LOCK_NAME: &str = "service-install.lock";

/// Mode of the record file: owner read/write only.
const FILE_MODE: u32 = 0o600;

/// Mode of a state directory the installer creates.
const STATE_DIR_MODE: u32 = 0o700;

/// Prefix of a record file staged for removal.
const REMOVAL_PREFIX: &str = ".service-install-removed-";

/// Disambiguates temporary names of concurrent writes in one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Kind of transaction a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// First installation into an empty namespace.
    Install,
    /// Switching an installation to a new version.
    Upgrade,
}

impl Operation {
    /// Returns the stable lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
        }
    }
}

/// The last step whose effects are known to be complete.
///
/// Steps are ordered; a transaction resumes with the step after the
/// recorded one. `Registering` is written before the service manager is
/// asked to install or replace the daemon, because that call's effect is
/// visible before it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Intent recorded; nothing changed yet.
    Started,
    /// The version directory holds the verified binaries.
    Binaries,
    /// `service.toml` names the new version.
    Config,
    /// The daemon definition was built and verified.
    Definition,
    /// The service manager call is in flight.
    Registering,
    /// The service manager accepted the definition.
    Registered,
    /// The daemon answered as the new version.
    Ready,
    /// `<prefix>/bin/pohunek` is the new CLI.
    Cli,
}

impl Step {
    /// Returns the stable lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Binaries => "binaries",
            Self::Config => "config",
            Self::Definition => "definition",
            Self::Registering => "registering",
            Self::Registered => "registered",
            Self::Ready => "ready",
            Self::Cli => "cli",
        }
    }
}

/// One in-flight install or upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Install or upgrade.
    pub operation: Operation,
    /// The version being installed.
    pub version: String,
    /// The installation prefix.
    pub prefix: PathBuf,
    /// The active version before an upgrade; `None` for an install.
    pub previous_version: Option<String>,
    /// Whether the version directory existed before this transaction, in
    /// which case a rollback never removes it.
    pub version_dir_preexisted: bool,
    /// The last completed step.
    pub step: Step,
}

/// Exclusive ownership of the install transaction, released on drop.
///
/// The `flock` belongs to this process's open file descriptions, so a
/// crashed holder releases it and a later run can resume or roll back the
/// record it left.
#[derive(Debug)]
pub struct TransactionLock {
    _lock: AdvisoryLock,
}

/// Owner-private storage of the transaction record.
#[derive(Debug, Clone)]
pub struct Store {
    state_dir: PathBuf,
}

impl Store {
    /// Creates a store rooted at the application state directory.
    #[must_use]
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Returns the record path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.state_dir.join(FILE_NAME)
    }

    /// Loads the pending record, if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Record`] for a malformed or foreign-schema record and
    /// a filesystem error when the file is unsafe.
    pub fn load(&self) -> Result<Option<Record>, Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(None);
        };
        if directory
            .entry_identity(FILE_NAME, EntryKind::RegularFile)
            .map_err(|source| fs_error("inspect install record", source))?
            .is_none()
        {
            return Ok(None);
        }
        let bytes = directory
            .read_file(FILE_NAME, FILE_MODE, MAX_RECORD_BYTES)
            .map_err(|source| fs_error("read install record", source))?;
        let record: Record = serde_json::from_slice(&bytes).map_err(|error| Error::Record {
            path: self.path(),
            detail: error.to_string(),
        })?;
        if record.schema_version != SCHEMA_VERSION {
            return Err(Error::Record {
                path: self.path(),
                detail: format!(
                    "schema_version {} is unsupported; expected {SCHEMA_VERSION}",
                    record.schema_version
                ),
            });
        }
        Ok(Some(record))
    }

    /// Atomically replaces the record.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory is unsafe or the
    /// write fails.
    pub fn save(&self, record: &Record) -> Result<(), Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        let mut bytes = serde_json::to_vec_pretty(record)
            .expect("the record holds only strings, paths, booleans, and enums");
        bytes.push(b'\n');
        let temporary = format!(
            ".{FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        directory
            .replace_file(FILE_NAME, temporary, &bytes, FILE_MODE)
            .map_err(|source| replace_error("write install record", source))
    }

    /// Takes the exclusive transaction lock, waiting at most [`LOCK_WAIT`].
    ///
    /// Every install, upgrade, uninstall, resume, rollback, and garbage
    /// collection runs under this lock. The short wait only absorbs a
    /// concurrent `status` probe, which holds the lock for microseconds; a
    /// real transaction runs for far longer (daemon readiness, stopping
    /// sessions), so a second command is refused promptly instead of stalling.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TransactionInProgress`] while another process holds
    /// the lock, and a filesystem error when the state directory is unsafe.
    pub async fn lock(&self) -> Result<TransactionLock, Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        let deadline = tokio::time::Instant::now() + LOCK_WAIT;
        loop {
            match try_lock(&directory)? {
                Some(lock) => return Ok(lock),
                None if tokio::time::Instant::now() >= deadline => {
                    return Err(Error::TransactionInProgress {
                        path: self.state_dir.join(LOCK_NAME),
                    });
                }
                None => tokio::time::sleep(LOCK_POLL).await,
            }
        }
    }

    /// Returns whether another process currently holds the transaction lock.
    ///
    /// The answer is a snapshot for reporting only; it never guards a change,
    /// and the probe holds the lock only for the instant it takes to test it.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory is unsafe.
    pub fn in_progress(&self) -> Result<bool, Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(false);
        };
        Ok(try_lock(&directory)?.is_none())
    }

    /// Removes the record; a missing record is not an error.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when removal fails.
    pub fn clear(&self) -> Result<(), Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(());
        };
        remove_file(&directory, FILE_NAME)
    }

    fn open(&self, create: bool) -> Result<Option<TrustedDir>, Error> {
        open_state_dir(&self.state_dir, create)
    }
}

/// Tries the transaction lock once; `None` means another process holds it.
fn try_lock(directory: &TrustedDir) -> Result<Option<TransactionLock>, Error> {
    match directory.acquire_lock(LOCK_NAME, FILE_MODE) {
        Ok(lock) => Ok(Some(TransactionLock { _lock: lock })),
        Err(FsError::LockContended { .. }) => Ok(None),
        Err(source) => Err(fs_error("lock the install transaction", source)),
    }
}

/// Opens the application state directory, creating it `0700` on request.
pub(crate) fn open_state_dir(path: &Path, create: bool) -> Result<Option<TrustedDir>, Error> {
    match TrustedDir::open_absolute(path, STATE_DIR_MODE) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.io_kind() == Some(std::io::ErrorKind::NotFound) => {
            if create {
                TrustedDir::open_or_create_absolute(path, STATE_DIR_MODE)
                    .map(Some)
                    .map_err(|source| fs_error("create state directory", source))
            } else {
                Ok(None)
            }
        }
        Err(source) => Err(fs_error("open state directory", source)),
    }
}

/// Removes one regular file of a trusted directory; absence is not an error.
pub(crate) fn remove_file(directory: &TrustedDir, name: &str) -> Result<(), Error> {
    const OPERATION: &str = "remove file";

    let Some(identity) = directory
        .entry_identity(name, EntryKind::RegularFile)
        .map_err(|source| fs_error(OPERATION, source))?
    else {
        return Ok(());
    };
    match directory
        .stage_random(name, REMOVAL_PREFIX, identity)
        .map_err(|source| fs_error(OPERATION, source))?
    {
        StageOutcome::Staged(entry) => entry
            .remove()
            .map(drop)
            .map_err(|source| fs_error(OPERATION, source)),
        StageOutcome::Missing => Ok(()),
        _ => Err(Error::Io {
            operation: OPERATION,
            path: directory.path().join(name),
            source: std::io::Error::other("the file changed while it was being removed"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::service::context::tests::temp_root;

    fn record(step: Step) -> Record {
        Record {
            schema_version: SCHEMA_VERSION,
            operation: Operation::Upgrade,
            version: "1.2.3".to_owned(),
            prefix: PathBuf::from("/home/u/.local"),
            previous_version: Some("1.2.2".to_owned()),
            version_dir_preexisted: false,
            step,
        }
    }

    #[test]
    fn record_round_trips_owner_private_and_clears() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state/pohunek");
        let store = Store::new(state.clone());
        assert_eq!(store.load().expect("load missing"), None);

        store.save(&record(Step::Config)).expect("save");
        let mode = std::fs::metadata(state.join(FILE_NAME))
            .expect("record metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, FILE_MODE);
        assert_eq!(
            store.load().expect("load"),
            Some(record(Step::Config)),
            "the record round-trips"
        );

        store.clear().expect("clear");
        store.clear().expect("clearing twice is fine");
        assert_eq!(store.load().expect("load cleared"), None);
    }

    #[test]
    fn foreign_schema_and_unknown_fields_are_rejected() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state");
        let store = Store::new(state.clone());
        let mut value = serde_json::to_value(record(Step::Started)).expect("value");
        value["schema_version"] = 9.into();
        store
            .save(&record(Step::Started))
            .expect("create directory");
        let write = |value: &serde_json::Value| {
            std::fs::write(state.join(FILE_NAME), value.to_string()).expect("write record");
        };
        write(&value);
        assert!(matches!(store.load(), Err(Error::Record { .. })));

        let mut value = serde_json::to_value(record(Step::Started)).expect("value");
        value["unexpected"] = true.into();
        write(&value);
        assert!(matches!(store.load(), Err(Error::Record { .. })));
    }

    #[test]
    fn steps_are_ordered() {
        assert!(Step::Started < Step::Binaries);
        assert!(Step::Registering < Step::Registered);
        assert!(Step::Ready < Step::Cli);
    }
}
