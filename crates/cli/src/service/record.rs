//! The journaled install transaction record, `<state>/pohunek/service-install.json`.
//!
//! The record exists only while an install or upgrade is in flight. It is
//! written owner-private (`0600`) with an atomic replace before each step's
//! effects become visible and after they complete, so a later run knows how
//! far the interrupted transaction got and can resume or roll it back. A
//! rollback is journaled the same way: once it begins, the record is never
//! resumed forward again.

// Rust guideline compliant 2026-09-29

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_platform::filesystem::{
    AdvisoryLock, EntryKind, FileLock, FsError, LockKind, StageOutcome, TrustedDir,
};
use pohunek_platform::process::{
    HostInspector, ProcessIdentity, ProcessInspector as _, StartIdentity,
};
use serde::{Deserialize, Serialize};

use super::error::{fs_error, replace_error, Error};
use super::inherited::Token;
use super::ledger::{self, AcceptedSet, Ledger};
use super::settings::{LOCK_POLL, LOCK_WAIT, MAX_HOLDER_BYTES, MAX_LEDGER_BYTES, MAX_RECORD_BYTES};

/// Largest accepted backup file in bytes.
///
/// The backup is an exact copy of a `service.toml`, whose size the service
/// configuration reader already bounds; this constant keeps the same ceiling
/// on the copy.
const MAX_CONFIG_BACKUP_BYTES: usize = pohunek_service_config::MAX_CONFIG_BYTES;

/// Version of the record schema; any other version is rejected.
///
/// Version 5 carries the gate's accepted set, [`Record::accepted_risks`].
/// Version 4 carries the runtime step's journal, [`Record::runtime_stop`] and
/// [`Record::runtime_backup`], and [`Step::Runtimes`]. Version 3 carries
/// [`Record::config_backup`], the migration transaction's backup description.
/// A record of an earlier version cannot say whether its `service.toml` was
/// rewritten from the previous release's schema, so it is refused rather than
/// guessed at. Version 2 carried [`Record::rolling_back`], which stays.
pub const SCHEMA_VERSION: u32 = 5;

/// The record schemas before [`SCHEMA_VERSION`] that still load.
///
/// A record of version 4 has no accepted set and one of version 3 has no
/// runtime journal either, so they load with those empty; their steps keep
/// their meaning under the new step order.
const PREVIOUS_SCHEMA_VERSIONS: [u32; 2] = [4, 3];

/// Directory of the runtime backup inside the application state directory.
///
/// The step copies the files the official package apply may change into it,
/// owner-private, before the apply runs. Rollback copies them back and the
/// directory is removed with the record.
pub const RUNTIME_BACKUP_DIR_NAME: &str = "service-install.runtime-backup";

/// File name of the record inside the application state directory.
pub const FILE_NAME: &str = "service-install.json";

/// File name of the owner-private backup of a pre-upgrade `service.toml`
/// inside the application state directory.
///
/// A migration transaction writes the exact pre-upgrade bytes here before it
/// touches `service.toml`, and rolls back by restoring them. The file is
/// removed together with the record, so it never outlives the transaction
/// that backs it.
pub const CONFIG_BACKUP_NAME: &str = "service-install.config-backup.toml";

/// File name of the transaction lock inside the application state directory.
pub const LOCK_NAME: &str = "service-install.lock";

/// File name of the lock holder record inside the application state directory.
///
/// `pohunek service lock` writes it while it holds the lock for a command;
/// see [`Store::hand_off`].
pub const HOLDER_NAME: &str = "service-install.lock.holder";

/// File name of the adopters' lock inside the application state directory.
///
/// Every command that adopts the lock of a `pohunek service lock` holder
/// holds it shared for its whole transaction; the holder takes it exclusive
/// before it releases the transaction lock, and every other transaction
/// requires it free (see [`Store::adopt`]).
pub const ADOPTED_NAME: &str = "service-install.lock.adopted";

/// File name of the lock that serializes adopted transactions.
///
/// Adopters of one `pohunek service lock` holder share its transaction lock,
/// so each one that runs a transaction also holds this file exclusively;
/// two adopted transactions never run at once (see [`Store::adopt`]).
pub const INHERITED_NAME: &str = "service-install.lock.inherited";

/// Version of the holder record schema; any other version is refused.
const HOLDER_SCHEMA_VERSION: u32 = 1;

/// Mode of the record file: owner read/write only.
const FILE_MODE: u32 = 0o600;

/// Mode of a state directory the installer creates.
const STATE_DIR_MODE: u32 = 0o700;

/// Prefix of a record file staged for removal.
const REMOVAL_PREFIX: &str = ".service-install-removed-";

/// Files a `Store::clear` removes together with the record.
///
/// The backup of a migration transaction is removed with the record, never
/// after it, so a record that ends now never leaves its backup behind.
const CLEARED_FILES: [&str; 2] = [FILE_NAME, CONFIG_BACKUP_NAME];

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

/// The last durable transaction checkpoint.
///
/// Steps are ordered. `StopDaemon` and `Registering` record intent before
/// their service-manager effects, so a resume reconciles either effect even
/// when the journaled call had not returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Intent recorded; nothing changed yet.
    Started,
    /// The version directory holds the verified binaries.
    Binaries,
    /// A migration transaction saved the exact pre-upgrade `service.toml`
    /// bytes owner-private and journaled their digest; the other flavors
    /// mark this step unchanged.
    BackedUp,
    /// A migration transaction intends to stop the daemon job before it
    /// rewrites `service.toml`; resume repeats that idempotent manager call.
    /// The other flavors mark this step unchanged.
    StopDaemon,
    /// `service.toml` names the new version.
    Config,
    /// The daemon definition was built and verified.
    Definition,
    /// The official runtime packages and host profile pins of the release
    /// are in place. A transaction that stopped the old daemon for it journals
    /// [`Record::runtime_stop`] and [`Record::runtime_backup`] first.
    Runtimes,
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
            Self::BackedUp => "backed_up",
            Self::StopDaemon => "stop_daemon",
            Self::Config => "config",
            Self::Definition => "definition",
            Self::Runtimes => "runtimes",
            Self::Registering => "registering",
            Self::Registered => "registered",
            Self::Ready => "ready",
            Self::Cli => "cli",
        }
    }
}

/// The journaled backup of a pre-upgrade `service.toml`.
///
/// A transaction whose source file was written by a previous release with
/// another schema records the backup here, so a rollback restores the exact
/// bytes the previous daemon reads and any resume knows which file to trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigBackup {
    /// Schema of the backed-up file: the `SCHEMA_VERSION` of the release
    /// that wrote it.
    pub source_schema_version: u32,
    /// SHA-256 of the exact pre-upgrade bytes, hexadecimal.
    pub digest: String,
}

/// The directory of a state file the runtime step may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDir {
    /// The runtime package store, `<state>/plugins`.
    Plugins,
    /// The host profiles, `<config>/agents`.
    Agents,
}

/// One state file the runtime step may change, as it was before the step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBackupEntry {
    /// The directory the file lives in.
    pub dir: RuntimeDir,
    /// The file name inside `dir`.
    pub name: String,
    /// SHA-256 of the exact bytes before the step, hexadecimal; `None` when
    /// the file did not exist, so rollback removes the file the step created.
    pub digest: Option<String>,
    /// The permission bits of the file before the step, restored exactly.
    pub mode: u32,
}

/// The journaled backup of the state files the runtime step may change.
///
/// Entry `n` is the backup file `n` in [`RUNTIME_BACKUP_DIR_NAME`]. The
/// digests bind every backup file to this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBackup {
    /// One entry per state file, in the order the backup wrote them.
    pub entries: Vec<RuntimeBackupEntry>,
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
    /// The pre-upgrade `service.toml` this transaction migrates, when the
    /// source was written by a previous release's schema.
    ///
    /// A present value changes the transaction's ordering: the daemon is
    /// stopped before `service.toml` is rewritten, and a rollback restores
    /// the exact backed-up bytes instead of re-rendering the current file.
    pub config_backup: Option<ConfigBackup>,
    /// The last completed step.
    pub step: Step,
    /// Whether a rollback has begun.
    ///
    /// Written before the rollback's first effect. A rollback restores
    /// `service.toml` before it restores the daemon, so a record whose
    /// rollback stopped partway describes steps whose effects are undone;
    /// resuming it would skip them. Such a record is only ever rolled back.
    pub rolling_back: bool,
    /// Whether the runtime step stopped the daemon.
    ///
    /// Written before the stop, so a rollback that finds it set restarts the
    /// previous daemon even when the stop did not return.
    #[serde(default)]
    pub runtime_stop: bool,
    /// The state the runtime step backed up before its apply, when it has.
    ///
    /// Written after the backup files are durable and before the apply, so a
    /// present value means the apply may have started and the backup is the
    /// only copy of the state before it.
    #[serde(default)]
    pub runtime_backup: Option<RuntimeBackup>,
    /// The launchd default `PATH` of the previous job, read from its loaded
    /// `default environment` at the upgrade's early gate.
    ///
    /// Written on macOS only, and only when that gate read it from the loaded
    /// job. A rollback, and the migration's steps after the job is unloaded,
    /// use it when launchd reports the job as not loaded. On Linux the field is
    /// never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchd_default_path: Option<String>,
    /// The boot identity (`kern.bootsessionuuid`) read when `launchd_default_path` was
    /// journaled.
    ///
    /// A `launchctl config user path` change takes effect at the next boot, so a
    /// journaled default is current only while the boot is the same. A journal
    /// without it (written before it existed, or when the boot time could not be
    /// read) cannot prove that. Never written on Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchd_boot_identity: Option<String>,
    /// The `PATH` of the persisted launchd user-domain configuration read when
    /// `launchd_default_path` was journaled.
    ///
    /// After a reboot that configuration is what launchd applied, provided it
    /// still holds this value; a different value was written (or the file
    /// removed) at an unknown time since, which no clock can date. Never written
    /// on Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launchd_persisted_path: Option<String>,
    /// The at-risk verdicts the last adoption gate of this transaction passed.
    ///
    /// Written before the transaction advances past the gate, so a resume past
    /// it knows what the gate accepted. The accepted-risk ledger is written
    /// from it when the transaction completes; a rollback discards it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_risks: Option<AcceptedSet>,
}

/// Exclusive ownership of the install transaction, released on drop.
///
/// The `flock` belongs to this process's open file descriptions, so a
/// crashed holder releases it and a later run can resume or roll back the
/// record it left. A lock adopted from a `pohunek service lock` ancestor is
/// the ancestor's; the adopter holds a shared lock on [`ADOPTED_NAME`]
/// instead, which keeps the ancestor from releasing it and keeps every other
/// transaction out even if the ancestor dies first.
#[derive(Debug)]
pub struct TransactionLock {
    held: Held,
}

impl TransactionLock {
    /// Whether a `pohunek service lock` ancestor holds the lock for this process.
    #[must_use]
    pub fn is_adopted(&self) -> bool {
        matches!(self.held, Held::Adopted { .. })
    }
}

/// How the transaction lock covers this process.
#[derive(Debug)]
enum Held {
    /// This process acquired it.
    Acquired {
        /// Holds the `flock` until dropped.
        _guard: AdvisoryLock,
    },
    /// A live `pohunek service lock` ancestor holds it.
    Adopted {
        /// The ancestor's token.
        token: Token,
        /// Keeps the ancestor from releasing the lock until dropped.
        _shared: FileLock,
        /// Serializes this transaction against the ancestor's other
        /// adopters; `None` for [`Adoption::Relay`].
        _serial: Option<FileLock>,
    },
}

/// What an adopter does under the adopted lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    /// Runs a transaction (or reads its state, like `service check`), which
    /// excludes every other adopted transaction.
    Transaction,
    /// Only passes the lock on to a command, like a nested `service lock`;
    /// the transactions that command runs serialize themselves.
    Relay,
}

/// The holder record a `pohunek service lock` process publishes.
///
/// Its token lets the command it runs, and that command's own `pohunek
/// service` calls, adopt the lock ([`Store::adopt`]); the recorded process
/// identity ties the token to a holder that is still alive.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Holder {
    /// Always [`HOLDER_SCHEMA_VERSION`].
    schema_version: u32,
    /// Process ID of the holder.
    pid: u32,
    /// Start identity of the holder, which rejects a reused PID.
    start_identity: u64,
    /// The token the holder handed to its command.
    token: String,
}

/// A published holder token, removed again by [`Handoff::release`].
///
/// A handoff of an adopted lock passes the ancestor's token on and owns no
/// record. Dropping an unreleased handoff removes its record on a best-effort
/// basis, so an early error never leaves one behind while the lock is held.
#[derive(Debug)]
pub struct Handoff {
    token: Token,
    record: Option<TrustedDir>,
}

impl Handoff {
    /// Returns the token the command receives.
    #[must_use]
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// Removes the holder record this process published.
    ///
    /// Call it before the lock is released: afterwards no command can adopt
    /// the lock with this token.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the record cannot be removed.
    pub fn release(mut self) -> Result<(), Error> {
        match self.record.take() {
            Some(directory) => remove_file(&directory, HOLDER_NAME),
            None => Ok(()),
        }
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        if let Some(directory) = self.record.take() {
            // The explicit `release` reports failures; this path only runs
            // after another error, which is the one the caller sees.
            let _ = remove_file(&directory, HOLDER_NAME);
        }
    }
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

    /// Returns the migration backup path.
    #[must_use]
    pub fn config_backup_path(&self) -> PathBuf {
        self.state_dir.join(CONFIG_BACKUP_NAME)
    }

    /// Writes the owner-private backup of a pre-upgrade `service.toml`.
    ///
    /// The copy is written atomically at mode `0600` before the transaction's
    /// first configuration effect, so a crash never leaves a partial backup.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory is unsafe or the
    /// write fails.
    pub fn write_config_backup(&self, bytes: &[u8]) -> Result<(), Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        let temporary = format!(
            ".{CONFIG_BACKUP_NAME}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        directory
            .replace_file(CONFIG_BACKUP_NAME, temporary, bytes, FILE_MODE)
            .map_err(|source| replace_error("write the pre-upgrade service.toml backup", source))
    }

    /// Reads the owner-private backup of a pre-upgrade `service.toml`.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory or the backup is
    /// unsafe, or when reading it fails.
    pub fn read_config_backup(&self) -> Result<Option<Vec<u8>>, Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(None);
        };
        if directory
            .entry_identity(CONFIG_BACKUP_NAME, EntryKind::RegularFile)
            .map_err(|source| fs_error("inspect the pre-upgrade service.toml backup", source))?
            .is_none()
        {
            return Ok(None);
        }
        directory
            .read_file(CONFIG_BACKUP_NAME, FILE_MODE, MAX_CONFIG_BACKUP_BYTES)
            .map(Some)
            .map_err(|source| fs_error("read the pre-upgrade service.toml backup", source))
    }

    /// Returns the accepted-risk ledger path.
    #[must_use]
    pub fn ledger_path(&self) -> PathBuf {
        self.state_dir.join(ledger::FILE_NAME)
    }

    /// Reads the bytes of the accepted-risk ledger, if there is one.
    ///
    /// Nothing is created or changed, so a check can call it.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory or the file is
    /// unsafe (a symlink, a foreign owner, a wrong mode, too large).
    pub fn read_ledger(&self) -> Result<Option<Vec<u8>>, Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(None);
        };
        if directory
            .entry_identity(ledger::FILE_NAME, EntryKind::RegularFile)
            .map_err(|source| ledger_error(&self.ledger_path(), &source))?
            .is_none()
        {
            return Ok(None);
        }
        directory
            .read_file(ledger::FILE_NAME, FILE_MODE, MAX_LEDGER_BYTES)
            .map(Some)
            .map_err(|source| ledger_error(&self.ledger_path(), &source))
    }

    /// Replaces the accepted-risk ledger, atomically and owner-private.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory is unsafe or the
    /// write fails.
    pub fn write_ledger(&self, ledger: &Ledger) -> Result<(), Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        let mut bytes = serde_json::to_vec_pretty(ledger)
            .expect("the ledger holds only strings, paths, booleans, and enums");
        bytes.push(b'\n');
        if bytes.len() > MAX_LEDGER_BYTES {
            return Err(Error::AcceptedRisks {
                path: self.ledger_path(),
                detail: format!(
                    "it would be {} bytes, over the {MAX_LEDGER_BYTES} byte limit",
                    bytes.len()
                ),
            });
        }
        let temporary = format!(
            ".{}.{}.{}.tmp",
            ledger::FILE_NAME,
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        directory
            .replace_file(ledger::FILE_NAME, temporary, &bytes, FILE_MODE)
            .map_err(|source| replace_error("write the accepted-risk ledger", source))
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
        let malformed = |error: serde_json::Error| Error::Record {
            path: self.path(),
            detail: error.to_string(),
        };
        // The version is checked before the fields, so a record of another
        // schema is named as such instead of as a missing or unknown field.
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(malformed)?;
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        let supported = std::iter::once(SCHEMA_VERSION)
            .chain(PREVIOUS_SCHEMA_VERSIONS)
            .any(|known| version == Some(u64::from(known)));
        if !supported {
            return Err(Error::Record {
                path: self.path(),
                detail: format!(
                    "schema_version {} is unsupported; expected {SCHEMA_VERSION}",
                    value
                        .get("schema_version")
                        .unwrap_or(&serde_json::Value::Null)
                ),
            });
        }
        let mut record: Record = serde_json::from_value(value).map_err(malformed)?;
        // The next save writes the current schema, which is what a
        // previous release refuses by name instead of misreading.
        record.schema_version = SCHEMA_VERSION;
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
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::Record {
                path: self.path(),
                detail: format!(
                    "it would be {} bytes, over the {MAX_RECORD_BYTES} byte limit",
                    bytes.len()
                ),
            });
        }
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
    /// the lock, or while a command that adopted the lock of a `pohunek
    /// service lock` still runs after that holder died, and a filesystem
    /// error when the state directory is unsafe.
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

    /// Publishes the holder record that lets `lock`'s command adopt it.
    ///
    /// For a lock this process acquired, a fresh [`Token`] is written with
    /// this process's identity into `<state>/pohunek/service-install.lock.holder`
    /// (owner-private, atomically replaced). An adopted lock passes its
    /// ancestor's token on and writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when no token can be drawn or this process's
    /// identity cannot be read, and a filesystem error when the record
    /// cannot be written.
    pub fn hand_off(&self, lock: &TransactionLock) -> Result<Handoff, Error> {
        if let Held::Adopted { token, .. } = &lock.held {
            return Ok(Handoff {
                token: token.clone(),
                record: None,
            });
        }
        let token = Token::generate()?;
        let pid = std::process::id();
        let identity = HostInspector::new()
            .identity(pid)
            .map_err(|source| Error::Io {
                operation: "read the identity of",
                path: PathBuf::from(format!("process {pid}")),
                source: std::io::Error::other(source.to_string()),
            })?
            .ok_or_else(|| Error::Io {
                operation: "read the identity of",
                path: PathBuf::from(format!("process {pid}")),
                source: std::io::Error::other("the process is not visible to itself"),
            })?;
        let directory = self.write_holder(&Holder {
            schema_version: HOLDER_SCHEMA_VERSION,
            pid,
            start_identity: identity.start_identity.get(),
            token: token.as_str().to_owned(),
        })?;
        Ok(Handoff {
            token,
            record: Some(directory),
        })
    }

    /// Adopts the transaction lock of the `pohunek service lock` whose token is `token`.
    ///
    /// The lock must be held right now (a fresh try-lock fails), the holder
    /// record must be trusted and name `token` (compared in constant time),
    /// and the recorded holder must still run with its recorded identity.
    /// The adopter then holds [`ADOPTED_NAME`] shared for as long as the
    /// returned lock lives and validates the holder once more: the holder
    /// waits for every shared holder before it releases the lock, and every
    /// other transaction is refused while one remains, even after the holder
    /// died. Anything else is refused rather than replaced by a lock of this
    /// process, because the ancestor that passed the token relies on its lock
    /// covering this command. A descendant that starts after the holder
    /// finished cannot adopt: the holder removes its record before releasing
    /// the lock, and a crashed holder's record names a process that no longer
    /// runs.
    ///
    /// Adopters share the transaction lock, so an [`Adoption::Transaction`]
    /// also holds [`INHERITED_NAME`] exclusively while it lives: a second
    /// adopted transaction, such as another backgrounded `service install`
    /// under the same holder, is refused instead of running beside it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InheritedLock`] when any of those conditions fails,
    /// [`Error::TransactionInProgress`] while another adopted transaction
    /// runs, and a filesystem error when the state directory, the record, or
    /// a lock file is unsafe.
    pub fn adopt(&self, token: &Token, adoption: Adoption) -> Result<TransactionLock, Error> {
        let directory = self
            .open(false)?
            .ok_or_else(|| refuse("the state directory does not exist"))?;
        validate_holder(&directory, token)?;
        let shared = match directory.lock_file(ADOPTED_NAME, FILE_MODE, LockKind::Shared) {
            Ok(shared) => shared,
            Err(FsError::LockContended { .. }) => {
                return Err(refuse("the lock holder is releasing the lock"));
            }
            Err(source) => return Err(fs_error("share the adopted transaction lock", source)),
        };
        // The holder may have finished between the first validation and the
        // shared lock; once the shared lock is held it cannot finish anymore.
        validate_holder(&directory, token)?;
        let serial = match adoption {
            Adoption::Relay => None,
            Adoption::Transaction => {
                match directory.lock_file(INHERITED_NAME, FILE_MODE, LockKind::Exclusive) {
                    Ok(serial) => Some(serial),
                    Err(FsError::LockContended { .. }) => {
                        return Err(Error::TransactionInProgress {
                            path: self.state_dir.join(INHERITED_NAME),
                        });
                    }
                    Err(source) => {
                        return Err(fs_error("serialize the adopted transaction", source));
                    }
                }
            }
        };
        Ok(TransactionLock {
            held: Held::Adopted {
                token: token.clone(),
                _shared: shared,
                _serial: serial,
            },
        })
    }

    /// Excludes every adopter of the lock, without waiting.
    ///
    /// A `pohunek service lock` holder calls this after its command exited
    /// and keeps the result until it released the lock: no command can adopt
    /// the lock meanwhile, and `None` means an adopter still runs.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory or the lock file
    /// is unsafe.
    pub fn exclude_adopters(&self) -> Result<Option<FileLock>, Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        exclude_adopters(&directory)
    }

    /// Atomically replaces the holder record and returns its directory.
    fn write_holder(&self, holder: &Holder) -> Result<TrustedDir, Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        let mut bytes = serde_json::to_vec_pretty(holder)
            .expect("the holder record holds numbers and a string");
        bytes.push(b'\n');
        let temporary = format!(
            ".{HOLDER_NAME}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        directory
            .replace_file(HOLDER_NAME, temporary, &bytes, FILE_MODE)
            .map_err(|source| replace_error("write the lock holder record", source))?;
        Ok(directory)
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

    /// Removes the record and the backup it may reference; both are absent-ok.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when removal fails.
    pub fn clear(&self) -> Result<(), Error> {
        let Some(directory) = self.open(false)? else {
            return Ok(());
        };
        for name in CLEARED_FILES {
            remove_file(&directory, name)?;
        }
        super::layout::remove_tree(&directory, RUNTIME_BACKUP_DIR_NAME)?;
        Ok(())
    }

    /// Returns the path of runtime backup file `index`.
    #[must_use]
    pub fn runtime_backup_path(&self, index: usize) -> PathBuf {
        self.state_dir
            .join(RUNTIME_BACKUP_DIR_NAME)
            .join(index.to_string())
    }

    /// Replaces the whole runtime backup with `files`, one file per index.
    ///
    /// The previous backup is removed first, so no file of an earlier
    /// transaction can be restored by mistake. Every file is written
    /// atomically at mode `0600`.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error when the state directory is unsafe or a
    /// write fails.
    pub fn write_runtime_backup(&self, files: &[Vec<u8>]) -> Result<(), Error> {
        let directory = self
            .open(true)?
            .expect("open(create = true) always yields a directory");
        super::layout::remove_tree(&directory, RUNTIME_BACKUP_DIR_NAME)?;
        let backup = directory
            .create_child_exclusive(RUNTIME_BACKUP_DIR_NAME, STATE_DIR_MODE)
            .map_err(|source| fs_error("create the runtime backup directory", source))?;
        for (index, bytes) in files.iter().enumerate() {
            let name = index.to_string();
            let temporary = format!(
                ".{name}.{}.{}.tmp",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            backup
                .replace_file(&name, temporary, bytes, FILE_MODE)
                .map_err(|source| replace_error("write a runtime backup file", source))?;
        }
        backup
            .sync()
            .map_err(|source| fs_error("synchronize the runtime backup directory", source))
    }

    /// Reads runtime backup file `index`, bounded by `max_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RuntimeBackup`] when the file is absent or unsafe and
    /// a filesystem error when the directory is.
    pub fn read_runtime_backup(&self, index: usize, max_bytes: usize) -> Result<Vec<u8>, Error> {
        let path = self.runtime_backup_path(index);
        let directory = self.open(false)?.ok_or_else(|| Error::RuntimeBackup {
            path: path.clone(),
            detail: "the state directory is missing".to_owned(),
        })?;
        let backup = directory
            .open_child(RUNTIME_BACKUP_DIR_NAME, STATE_DIR_MODE)
            .map_err(|source| fs_error("open the runtime backup directory", source))?;
        backup
            .read_file(index.to_string(), FILE_MODE, max_bytes)
            .map_err(|source| Error::RuntimeBackup {
                path,
                detail: source.to_string(),
            })
    }

    fn open(&self, create: bool) -> Result<Option<TrustedDir>, Error> {
        open_state_dir(&self.state_dir, create)
    }
}

/// The typed error of an accepted-risk ledger that is unsafe to read.
fn ledger_error(path: &Path, source: &FsError) -> Error {
    Error::AcceptedRisks {
        path: path.to_path_buf(),
        detail: source.to_string(),
    }
}

/// Tries the transaction lock once; `None` means another process holds it.
///
/// An adopter left running after its `pohunek service lock` holder died
/// still holds its shared lock on [`ADOPTED_NAME`], so the transaction lock
/// counts as held until that adopter ends.
fn try_lock(directory: &TrustedDir) -> Result<Option<TransactionLock>, Error> {
    let lock = match directory.acquire_lock(LOCK_NAME, FILE_MODE) {
        Ok(lock) => lock,
        Err(FsError::LockContended { .. }) => return Ok(None),
        Err(source) => return Err(fs_error("lock the install transaction", source)),
    };
    // The exclusive probe is released at once: this process's own command
    // must be able to adopt the lock, and no adopter can appear without a
    // live holder, which this process now is.
    Ok(exclude_adopters(directory)?.map(|_probe| TransactionLock {
        held: Held::Acquired { _guard: lock },
    }))
}

/// Takes the exclusive lock on [`ADOPTED_NAME`]; `None` while an adopter holds it shared.
fn exclude_adopters(directory: &TrustedDir) -> Result<Option<FileLock>, Error> {
    match directory.lock_file(ADOPTED_NAME, FILE_MODE, LockKind::Exclusive) {
        Ok(lock) => Ok(Some(lock)),
        Err(FsError::LockContended { .. }) => Ok(None),
        Err(source) => Err(fs_error(
            "lock out adopters of the transaction lock",
            source,
        )),
    }
}

/// An [`Error::InheritedLock`] with `detail`.
fn refuse(detail: &str) -> Error {
    Error::InheritedLock {
        detail: detail.to_owned(),
    }
}

/// Verifies that a live `pohunek service lock` holds the lock with `token`.
///
/// The lock must be held right now (a fresh try-lock fails), the holder
/// record must be trusted and name `token` (compared in constant time), and
/// the recorded holder must still run with its recorded identity.
fn validate_holder(directory: &TrustedDir, token: &Token) -> Result<(), Error> {
    match directory.acquire_lock(LOCK_NAME, FILE_MODE) {
        Ok(_free) => return Err(refuse("no process holds the transaction lock")),
        Err(FsError::LockContended { .. }) => {}
        Err(source) => return Err(fs_error("lock the install transaction", source)),
    }
    if directory
        .entry_identity(HOLDER_NAME, EntryKind::RegularFile)
        .map_err(|source| fs_error("inspect the lock holder record", source))?
        .is_none()
    {
        return Err(refuse("no `pohunek service lock` holds the lock"));
    }
    let bytes = directory
        .read_file(HOLDER_NAME, FILE_MODE, MAX_HOLDER_BYTES)
        .map_err(|source| fs_error("read the lock holder record", source))?;
    let holder: Holder = serde_json::from_slice(&bytes)
        .map_err(|_malformed| refuse("the lock holder record is malformed"))?;
    if holder.schema_version != HOLDER_SCHEMA_VERSION {
        return Err(refuse("the lock holder record has an unsupported schema"));
    }
    let recorded = Token::parse(&holder.token)
        .map_err(|_malformed| refuse("the lock holder record is malformed"))?;
    if !recorded.matches(token) {
        return Err(refuse("the token is not the lock holder's"));
    }
    let identity = ProcessIdentity {
        pid: holder.pid,
        start_identity: StartIdentity::new(holder.start_identity),
    };
    match HostInspector::new().is_running(identity) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refuse("the recorded lock holder no longer runs")),
        Err(source) => Err(Error::InheritedLock {
            detail: format!("the recorded lock holder cannot be inspected: {source}"),
        }),
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
    remove_entry(directory, name, EntryKind::RegularFile)
}

/// Unlinks a recognized purge artifact, including a symlink itself, never its target.
pub(crate) fn remove_purge_artifact(directory: &TrustedDir, name: &str) -> Result<(), Error> {
    let kind = match directory.entry_identity(name, EntryKind::RegularFile) {
        Ok(_) => EntryKind::RegularFile,
        Err(FsError::UnsafeType {
            actual: "symbolic link",
            ..
        }) => EntryKind::Symlink,
        Err(source) => return Err(fs_error("inspect purge artifact", source)),
    };
    remove_entry(directory, name, kind)
}

fn remove_entry(directory: &TrustedDir, name: &str, kind: EntryKind) -> Result<(), Error> {
    const OPERATION: &str = "remove file";

    let Some(identity) = directory
        .entry_identity(name, kind)
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
            config_backup: None,
            step,
            rolling_back: false,
            runtime_stop: false,
            runtime_backup: None,
            launchd_default_path: None,
            launchd_boot_identity: None,
            launchd_persisted_path: None,
            accepted_risks: None,
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
    fn a_schema_3_record_loads_with_an_empty_runtime_journal_and_is_saved_as_schema_4() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state");
        let store = Store::new(state.clone());
        store
            .save(&record(Step::Definition))
            .expect("create directory");
        let mut value = serde_json::to_value(record(Step::Definition)).expect("value");
        value["schema_version"] = 3.into();
        let object = value.as_object_mut().expect("record object");
        object.remove("runtime_stop");
        object.remove("runtime_backup");
        std::fs::write(state.join(FILE_NAME), value.to_string()).expect("write record");

        let loaded = store.load().expect("load").expect("record");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.step, Step::Definition);
        assert!(!loaded.runtime_stop);
        assert_eq!(loaded.runtime_backup, None);

        store.save(&loaded).expect("save");
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state.join(FILE_NAME)).expect("read"))
                .expect("json");
        assert_eq!(written["schema_version"], SCHEMA_VERSION);
    }

    #[test]
    fn a_runtime_journal_round_trips_and_its_backup_is_removed_with_the_record() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state");
        let store = Store::new(state.clone());
        let journal = RuntimeBackup {
            entries: vec![
                RuntimeBackupEntry {
                    dir: RuntimeDir::Plugins,
                    name: "registry.json".to_owned(),
                    digest: Some("ab".repeat(32)),
                    mode: FILE_MODE,
                },
                RuntimeBackupEntry {
                    dir: RuntimeDir::Agents,
                    name: "claude.toml".to_owned(),
                    digest: None,
                    mode: FILE_MODE,
                },
            ],
        };
        let saved = Record {
            runtime_stop: true,
            runtime_backup: Some(journal.clone()),
            launchd_default_path: None,
            launchd_boot_identity: None,
            launchd_persisted_path: None,
            step: Step::Runtimes,
            ..record(Step::Runtimes)
        };
        store.save(&saved).expect("save");
        assert_eq!(store.load().expect("load"), Some(saved));

        store
            .write_runtime_backup(&[b"before".to_vec(), Vec::new()])
            .expect("write backup");
        assert_eq!(
            store.read_runtime_backup(0, 64).expect("read"),
            b"before".to_vec()
        );
        assert!(
            store.read_runtime_backup(0, 3).is_err(),
            "bounded reads refuse"
        );

        store.clear().expect("clear");
        assert!(!state.join(RUNTIME_BACKUP_DIR_NAME).exists());
        assert_eq!(store.load().expect("load cleared"), None);
    }

    fn accepted_set(sessions: usize) -> ledger::AcceptedSet {
        ledger::AcceptedSet {
            store_path: PathBuf::from("/store"),
            recorded_at: "2026-10-19T00:00:00Z".to_owned(),
            entries: (0..sessions)
                .map(|index| ledger::Entry {
                    session_id: format!("s-{index:05}"),
                    list: ledger::List::Live,
                    verdict: pohunek_service_config::preflight::Verdict::WouldNotBeAdopted,
                    code: "worker_generation_missing".to_owned(),
                    scope: format!("v1:{}", "ab".repeat(32)),
                })
                .collect(),
            one_time: Vec::new(),
        }
    }

    #[test]
    fn a_record_and_a_ledger_of_thousands_of_accepted_risks_round_trip() {
        let (_root, root) = temp_root();
        let store = Store::new(root.as_path().join("state"));
        // Far past the former 64 KiB record and 1 MiB ledger limits.
        let set = accepted_set(9_000);
        let saved = Record {
            accepted_risks: Some(set.clone()),
            ..record(Step::Runtimes)
        };
        store.save(&saved).expect("a large journal is saved");
        assert_eq!(store.load().expect("and read back"), Some(saved));

        let written = Ledger::of("ns", "1.0.0", None, &set);
        store
            .write_ledger(&written)
            .expect("a large ledger is written");
        let bytes = store.read_ledger().expect("read").expect("present");
        assert_eq!(
            serde_json::from_slice::<Ledger>(&bytes).expect("parse"),
            written
        );
    }

    #[test]
    fn a_record_or_ledger_over_its_read_limit_is_refused_when_written() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state");
        let store = Store::new(state.clone());
        let huge = accepted_set(MAX_RECORD_BYTES / 100);
        let record = Record {
            accepted_risks: Some(huge.clone()),
            ..record(Step::Runtimes)
        };
        assert!(matches!(store.save(&record), Err(Error::Record { .. })));
        assert!(!state.join(FILE_NAME).exists(), "nothing was written");
        assert!(matches!(
            store.write_ledger(&Ledger::of("ns", "1.0.0", None, &huge)),
            Err(Error::AcceptedRisks { .. })
        ));
        assert!(!state.join(ledger::FILE_NAME).exists());
    }

    /// Pads the last entry's code until `size_of` reports exactly `target` bytes.
    fn padded_to(
        mut set: ledger::AcceptedSet,
        target: usize,
        size_of: impl Fn(&ledger::AcceptedSet) -> usize,
    ) -> ledger::AcceptedSet {
        let missing = target - size_of(&set);
        set.entries
            .last_mut()
            .expect("an entry")
            .code
            .push_str(&"x".repeat(missing));
        assert_eq!(size_of(&set), target);
        set
    }

    #[test]
    fn a_record_and_a_ledger_exactly_at_the_limit_are_written_and_read() {
        let (_root, root) = temp_root();
        let store = Store::new(root.as_path().join("state"));
        // `to_vec_pretty` plus the trailing newline is what is written.
        let record_size = |set: &ledger::AcceptedSet| {
            let record = Record {
                accepted_risks: Some(set.clone()),
                ..record(Step::Runtimes)
            };
            serde_json::to_vec_pretty(&record).expect("json").len() + 1
        };
        let ledger_size = |set: &ledger::AcceptedSet| {
            serde_json::to_vec_pretty(&Ledger::of("ns", "1.0.0", None, set))
                .expect("json")
                .len()
                + 1
        };

        let set = padded_to(accepted_set(100), MAX_RECORD_BYTES, record_size);
        let saved = Record {
            accepted_risks: Some(set),
            ..record(Step::Runtimes)
        };
        store.save(&saved).expect("a record at the limit is saved");
        assert_eq!(store.load().expect("and read"), Some(saved));

        let set = padded_to(accepted_set(100), MAX_LEDGER_BYTES, ledger_size);
        store
            .write_ledger(&Ledger::of("ns", "1.0.0", None, &set))
            .expect("a ledger at the limit is written");
        assert_eq!(
            store.read_ledger().expect("read").expect("present").len(),
            MAX_LEDGER_BYTES
        );

        let over = padded_to(accepted_set(100), MAX_LEDGER_BYTES + 1, ledger_size);
        assert!(store
            .write_ledger(&Ledger::of("ns", "1.0.0", None, &over))
            .is_err());
        let over = padded_to(accepted_set(100), MAX_RECORD_BYTES + 1, record_size);
        let over = Record {
            accepted_risks: Some(over),
            ..record(Step::Runtimes)
        };
        assert!(store.save(&over).is_err());
    }

    #[test]
    fn a_record_of_a_future_schema_is_refused_by_name() {
        let (_root, root) = temp_root();
        let state = root.as_path().join("state");
        let store = Store::new(state.clone());
        store
            .save(&record(Step::Started))
            .expect("create directory");
        let mut value = serde_json::to_value(record(Step::Started)).expect("value");
        let future = SCHEMA_VERSION + 1;
        value["schema_version"] = future.into();
        std::fs::write(state.join(FILE_NAME), value.to_string()).expect("write record");
        let Err(Error::Record { detail, .. }) = store.load() else {
            panic!("a record of schema {future} is refused");
        };
        assert!(
            detail.contains(&format!("schema_version {future} is unsupported")),
            "{detail}"
        );
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

        // A schema 1 record lacks `rolling_back`; it is refused by its version.
        let mut value = serde_json::to_value(record(Step::Started)).expect("value");
        value["schema_version"] = 1.into();
        value
            .as_object_mut()
            .expect("record object")
            .remove("rolling_back");
        write(&value);
        let Err(Error::Record { detail, .. }) = store.load() else {
            panic!("a schema 1 record is refused");
        };
        assert!(
            detail.contains("schema_version 1 is unsupported"),
            "{detail}"
        );

        let mut value = serde_json::to_value(record(Step::Started)).expect("value");
        value
            .as_object_mut()
            .expect("record object")
            .remove("rolling_back");
        write(&value);
        assert!(
            matches!(store.load(), Err(Error::Record { .. })),
            "the rollback marker is required"
        );
    }
}
