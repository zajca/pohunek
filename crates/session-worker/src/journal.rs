//! Persists the worker-owned runtime journal atomically.

// Rust guideline compliant 2026-07-23

use std::fmt::{Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Worker journal schema understood by this crate.
const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Owner-only directory permissions.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Owner-only journal permissions.
const PRIVATE_FILE_MODE: u32 = 0o600;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Durable worker-journal failure.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A journal path has no parent.
    #[error("journal path has no parent: {}", path.display())]
    MissingParent {
        /// Invalid path.
        path: PathBuf,
    },
    /// A path has an unsafe filesystem type or owner/mode.
    #[error("unsafe worker journal path {}: {reason}", path.display())]
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
        /// Rejection reason.
        reason: &'static str,
    },
    /// A journal filesystem operation failed.
    #[error("journal filesystem operation failed for {}: {source}", path.display())]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Journal JSON is invalid.
    #[error("worker journal {} is corrupt: {source}", path.display())]
    Corrupt {
        /// Corrupt journal path.
        path: PathBuf,
        /// JSON parsing error.
        source: serde_json::Error,
    },
    /// Journal serialization failed.
    #[error("worker journal serialization failed: {0}")]
    Serialize(serde_json::Error),
}

/// Durable worker runtime phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    /// Socket is ready but no initialization was accepted.
    Bootstrap,
    /// Initialization was accepted and PTY setup is running.
    Starting,
    /// PTY and child are live.
    Live,
    /// Child exited and final state is retained.
    Terminal,
    /// Initialization was never received before the deadline.
    NeverInitialized,
    /// Runtime failed after bootstrap.
    Faulted,
}

/// Retained OS process identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildIdentity {
    /// Numeric process identifier.
    pub pid: u32,
    /// Process group leader.
    pub process_group: i32,
    /// Platform process start identity.
    pub start_identity: String,
}

/// Terminal child outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeOutcome {
    /// Process exit code when available.
    pub exit_code: Option<i32>,
    /// Signal name when available.
    pub signal: Option<String>,
    /// Whether the process succeeded.
    pub success: bool,
    /// RFC 3339 terminal timestamp.
    pub exited_at: String,
    /// Worker-classified terminal reason.
    pub reason: String,
}

/// Immutable launch provider identity.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchIdentity {
    /// Provider base.
    pub provider: String,
    /// Designated launch process.
    pub process: ChildIdentity,
    /// Resume-reference kind.
    pub reference_kind: String,
    /// Provider-native recovery reference.
    pub native_reference: String,
}

impl Debug for LaunchIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchIdentity")
            .field("provider", &self.provider)
            .field("process", &self.process)
            .field("reference_kind", &self.reference_kind)
            .field("native_reference", &"[REDACTED]")
            .finish()
    }
}

/// Latest sanitized active provider claim.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveIdentity {
    /// Provider base.
    pub provider: String,
    /// Claiming process.
    pub process: ChildIdentity,
    /// Monotonic claim sequence.
    pub sequence: u64,
    /// RFC 3339 claim expiry.
    pub expires_at: String,
    /// Native-reference kind reported by the active provider.
    pub reference_kind: Option<String>,
    /// Native reference for the active provider.
    pub native_reference: Option<String>,
}

impl Debug for ActiveIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveIdentity")
            .field("provider", &self.provider)
            .field("process", &self.process)
            .field("sequence", &self.sequence)
            .field("expires_at", &self.expires_at)
            .field("reference_kind", &self.reference_kind)
            .field("native_reference", &"[REDACTED]")
            .finish()
    }
}

/// Durable non-secret worker state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// Journal schema version.
    pub schema_version: u32,
    /// Logical session identifier.
    pub session_id: String,
    /// Stable worker identifier.
    pub worker_id: String,
    /// Runtime generation identifier after initialization.
    pub runtime_id: Option<String>,
    /// Lowest supported private-protocol version.
    pub protocol_minimum: u16,
    /// Highest supported private-protocol version.
    pub protocol_maximum: u16,
    /// Worker process identifier.
    pub worker_pid: u32,
    /// Worker process start identity.
    pub worker_start_identity: String,
    /// Managed PTY root identity.
    pub child: Option<ChildIdentity>,
    /// PTY creation timestamp.
    pub pty_created_at: Option<String>,
    /// Current terminal columns.
    pub cols: Option<u16>,
    /// Current terminal rows.
    pub rows: Option<u16>,
    /// Current runtime phase.
    pub phase: RuntimePhase,
    /// Terminal outcome.
    pub outcome: Option<RuntimeOutcome>,
    /// Immutable launch recovery identity.
    pub launch_identity: Option<LaunchIdentity>,
    /// Latest sanitized active identity.
    pub active_identity: Option<ActiveIdentity>,
    /// Next raw-output byte offset.
    pub next_output_offset: u64,
    /// Whether a daemon durably imported terminal state.
    pub terminal_acknowledged: bool,
    /// RFC 3339 last-update timestamp.
    pub updated_at: String,
}

impl Debug for JournalRecord {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalRecord")
            .field("schema_version", &self.schema_version)
            .field("session_id", &self.session_id)
            .field("worker_id", &self.worker_id)
            .field("runtime_id", &self.runtime_id)
            .field("protocol_minimum", &self.protocol_minimum)
            .field("protocol_maximum", &self.protocol_maximum)
            .field("worker_pid", &self.worker_pid)
            .field("worker_start_identity", &self.worker_start_identity)
            .field("child", &self.child)
            .field("pty_created_at", &self.pty_created_at)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("phase", &self.phase)
            .field("outcome", &self.outcome)
            .field("launch_identity", &self.launch_identity)
            .field("active_identity", &self.active_identity)
            .field("next_output_offset", &self.next_output_offset)
            .field("terminal_acknowledged", &self.terminal_acknowledged)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl JournalRecord {
    /// Creates a bootstrap journal record.
    #[must_use]
    pub fn bootstrap(
        session_id: String,
        worker_id: String,
        process_id: u32,
        worker_start_identity: String,
        protocol_range: (u16, u16),
        updated_at: String,
    ) -> Self {
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            session_id,
            worker_id,
            runtime_id: None,
            protocol_minimum: protocol_range.0,
            protocol_maximum: protocol_range.1,
            worker_pid: process_id,
            worker_start_identity,
            child: None,
            pty_created_at: None,
            cols: None,
            rows: None,
            phase: RuntimePhase::Bootstrap,
            outcome: None,
            launch_identity: None,
            active_identity: None,
            next_output_offset: 0,
            terminal_acknowledged: false,
            updated_at,
        }
    }
}

/// Sole-writer handle for one worker journal.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Creates a journal handle without touching disk.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Returns the exact journal path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomically replaces the journal with `record`.
    ///
    /// The temporary file is owner-only from creation, is flushed before
    /// rename, and the containing directory is flushed afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] for unsafe paths, I/O, or serialization failure.
    pub fn write(&self, record: &JournalRecord) -> Result<(), JournalError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| JournalError::MissingParent {
                path: self.path.clone(),
            })?;
        ensure_private_dir(parent)?;
        reject_symlink(&self.path)?;

        let bytes = serde_json::to_vec_pretty(record).map_err(JournalError::Serialize)?;
        let temp = temp_path(&self.path);
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(PRIVATE_FILE_MODE)
                .open(&temp)
                .map_err(|source| JournalError::Io {
                    path: temp.clone(),
                    source,
                })?;
            file.write_all(&bytes).map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
            file.write_all(b"\n").map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
            file.sync_all().map_err(|source| JournalError::Io {
                path: temp.clone(),
                source,
            })?;
            fs::rename(&temp, &self.path).map_err(|source| JournalError::Io {
                path: self.path.clone(),
                source,
            })?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| JournalError::Io {
                    path: parent.to_path_buf(),
                    source,
                })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// Loads and validates the current journal.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] for unsafe paths, I/O, or malformed JSON.
    pub fn load(&self) -> Result<JournalRecord, JournalError> {
        reject_symlink(&self.path)?;
        let metadata = fs::metadata(&self.path).map_err(|source| JournalError::Io {
            path: self.path.clone(),
            source,
        })?;
        validate_private_file(&self.path, &metadata)?;
        let mut bytes = Vec::new();
        File::open(&self.path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|source| JournalError::Io {
                path: self.path.clone(),
                source,
            })?;
        serde_json::from_slice(&bytes).map_err(|source| JournalError::Corrupt {
            path: self.path.clone(),
            source,
        })
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), JournalError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(JournalError::UnsafePath {
                path: path.to_path_buf(),
                reason: "expected a real directory",
            });
        }
        validate_owner_mode(path, &metadata, PRIVATE_DIR_MODE)?;
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|source| JournalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE)).map_err(|source| {
        JournalError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| JournalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    validate_owner_mode(path, &metadata, PRIVATE_DIR_MODE)
}

fn reject_symlink(path: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JournalError::UnsafePath {
            path: path.to_path_buf(),
            reason: "symlinks are not accepted",
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(JournalError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<(), JournalError> {
    if !metadata.is_file() {
        return Err(JournalError::UnsafePath {
            path: path.to_path_buf(),
            reason: "expected a regular file",
        });
    }
    validate_owner_mode(path, metadata, PRIVATE_FILE_MODE)
}

fn validate_owner_mode(
    path: &Path,
    metadata: &fs::Metadata,
    expected_mode: u32,
) -> Result<(), JournalError> {
    if metadata.uid() != nix::unistd::Uid::effective().as_raw() {
        return Err(JournalError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path is not owned by the effective user",
        });
    }
    if metadata.mode() & 0o777 != expected_mode {
        return Err(JournalError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path permissions are not owner-private",
        });
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("journal");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

#[cfg(test)]
mod tests {
    use super::{Journal, JournalError, JournalRecord, LaunchIdentity};
    use crate::ChildIdentity;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_dir(tag: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pohunek-session-worker-{tag}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn record(secret: &str) -> JournalRecord {
        let mut record = JournalRecord::bootstrap(
            "s-1".to_owned(),
            "worker-1".to_owned(),
            10,
            "start-1".to_owned(),
            (1, 2),
            "2026-07-23T00:00:00Z".to_owned(),
        );
        record.launch_identity = Some(LaunchIdentity {
            provider: "codex".to_owned(),
            process: ChildIdentity {
                pid: 11,
                process_group: 11,
                start_identity: "start-2".to_owned(),
            },
            reference_kind: "thread_id".to_owned(),
            native_reference: secret.to_owned(),
        });
        record
    }

    #[test]
    fn atomic_round_trip_is_owner_private() {
        let root = test_dir("round-trip");
        let path = root.join("s-1").join("worker-1.json");
        let journal = Journal::new(&path);
        let record = record("native-reference");

        journal.write(&record).expect("write");
        assert_eq!(journal.load().expect("load"), record);
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn debug_redacts_native_reference() {
        let secret = "seeded-private-native-reference";
        let rendered = format!("{:?}", record(secret));

        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn corrupt_journal_returns_typed_error() {
        let root = test_dir("corrupt");
        fs::create_dir_all(&root).expect("create");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("chmod");
        let path = root.join("record.json");
        fs::write(&path, b"{").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");

        assert!(matches!(
            Journal::new(&path).load(),
            Err(JournalError::Corrupt { .. })
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn symlink_target_is_rejected() {
        let root = test_dir("symlink");
        fs::create_dir_all(&root).expect("create");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("chmod");
        let target = root.join("target");
        fs::write(&target, b"{}").expect("write target");
        let path = root.join("record.json");
        symlink(&target, &path).expect("symlink");

        assert!(matches!(
            Journal::new(&path).write(&record("secret")),
            Err(JournalError::UnsafePath { .. })
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }
}
