//! Persists the worker-owned runtime journal atomically.

// Rust guideline compliant 2026-09-24

use std::fmt::{Debug, Formatter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_platform::filesystem::{AtomicReplaceError, TrustedDir};
use serde::{Deserialize, Serialize};

/// Worker journal schema understood by this crate.
const JOURNAL_SCHEMA_VERSION: u32 = 3;
/// Owner-only directory permissions.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Owner-only journal permissions.
const PRIVATE_FILE_MODE: u32 = 0o600;
/// Maximum serialized journal size accepted from disk.
///
/// Runtime journals contain bounded session metadata; one MiB leaves ample
/// schema growth room while preventing unbounded allocation from a replaced file.
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;

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
    /// A trusted descriptor-relative filesystem operation failed.
    #[error("trusted worker journal operation failed: {0}")]
    TrustedFilesystem(#[from] pohunek_platform::filesystem::FsError),
    /// Atomic journal replacement failed before or after its commit point.
    #[error("atomic worker journal replacement failed: {0}")]
    AtomicReplace(#[from] AtomicReplaceError),
}

impl JournalError {
    /// Reports whether the journal rename committed before durability failed.
    #[must_use]
    pub fn committed_durability_uncertain(&self) -> bool {
        matches!(
            self,
            Self::AtomicReplace(AtomicReplaceError::CommittedDurabilityUncertain(_))
        )
    }
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

/// Worker-owned launch claim awaiting a coherent process-tree observation.
///
/// Runtime, root and reporting-process generations are immutable, as is the
/// first native reference reported by that process. The original hook expiry
/// bounds retries independently of subsequent active reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLaunchClaim {
    /// Whether verification may still be retried. Rejected records fence later
    /// reports from replacing the original native reference during this runtime.
    pub retry_pending: bool,
    /// Runtime generation that accepted this claim.
    pub runtime_id: String,
    /// Exact PTY root generation authorizing the claim.
    pub root: ChildIdentity,
    /// Unverified immutable launch reference; never exposed as verified state.
    pub identity: LaunchIdentity,
    /// Original hook claim's RFC 3339 expiry.
    pub expires_at: String,
}

/// Durable ordering tombstone for an accepted active-identity release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedIdentity {
    /// Provider base released by the hook.
    pub provider: String,
    /// Released process identity.
    pub process: ChildIdentity,
    /// Monotonic release sequence.
    pub sequence: u64,
}

/// Durable lifecycle of one provider-managed subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentPhase {
    /// The subagent is running.
    Running,
    /// The subagent completed successfully.
    Completed,
    /// The subagent failed.
    Failed,
    /// The subagent was cancelled.
    Cancelled,
    /// The owning runtime ended before completion was reported.
    Lost,
}

/// Sanitized lifecycle state retained by the PTY-owning worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRecord {
    /// Provider-native lifecycle correlation identifier.
    pub id: String,
    /// Parent subagent identifier, when reported.
    pub parent_id: Option<String>,
    /// Provider base name.
    pub provider: String,
    /// Provider-defined subagent type, when reported.
    pub agent_type: Option<String>,
    /// Current lifecycle phase.
    pub phase: SubagentPhase,
    /// Latest provider-supplied ordering sequence.
    pub sequence: u64,
    /// Worker-owned monotonic revision.
    pub revision: u64,
    /// First accepted start timestamp.
    pub started_at_ms: u64,
    /// Latest accepted transition timestamp.
    pub updated_at_ms: u64,
    /// Terminal transition timestamp, when terminal.
    pub finished_at_ms: Option<u64>,
}

/// Identifies the worker binary and daemon-issued generation behind a journal.
///
/// Reconciliation compares these facts with the service manager's definition
/// of the job, so a journal proves which executable and generation wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerOrigin {
    /// Absolute path of the running worker executable.
    pub executable: PathBuf,
    /// Package version of the running worker.
    pub version: String,
    /// Daemon-issued worker generation token.
    pub generation: String,
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
    /// Worker executable, version, and generation, stored as top-level keys.
    #[serde(flatten)]
    pub origin: WorkerOrigin,
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
    /// Operating-system boot identity that scopes process start identities.
    pub boot_identity: String,
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
    /// Deferred claims and rejection fences, not verified recovery identities.
    pub pending_launch_claims: Vec<PendingLaunchClaim>,
    /// Latest sanitized active identity.
    pub active_identity: Option<ActiveIdentity>,
    /// Latest accepted release, distinguishing explicit release from no report.
    #[serde(default)]
    pub active_identity_release: Option<ReleasedIdentity>,
    /// Worker-owned revision assigned to the next subagent mutation.
    #[serde(default)]
    pub subagent_revision: u64,
    /// Active subagents plus bounded recent terminal history.
    #[serde(default)]
    pub subagents: Vec<SubagentRecord>,
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
            .field("origin", &self.origin)
            .field("runtime_id", &self.runtime_id)
            .field("protocol_minimum", &self.protocol_minimum)
            .field("protocol_maximum", &self.protocol_maximum)
            .field("worker_pid", &self.worker_pid)
            .field("worker_start_identity", &self.worker_start_identity)
            .field("boot_identity", &self.boot_identity)
            .field("child", &self.child)
            .field("pty_created_at", &self.pty_created_at)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("phase", &self.phase)
            .field("outcome", &self.outcome)
            .field("launch_identity", &self.launch_identity)
            .field("pending_launch_claims", &self.pending_launch_claims)
            .field("active_identity", &self.active_identity)
            .field("active_identity_release", &self.active_identity_release)
            .field("subagent_revision", &self.subagent_revision)
            .field("subagents", &self.subagents)
            .field("next_output_offset", &self.next_output_offset)
            .field("terminal_acknowledged", &self.terminal_acknowledged)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl JournalRecord {
    /// Creates a bootstrap journal record.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is an independent bootstrap fact with its own source"
    )]
    pub fn bootstrap(
        session_id: String,
        worker_id: String,
        origin: WorkerOrigin,
        process_id: u32,
        worker_start_identity: String,
        boot_identity: String,
        protocol_range: (u16, u16),
        updated_at: String,
    ) -> Self {
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            session_id,
            worker_id,
            origin,
            runtime_id: None,
            protocol_minimum: protocol_range.0,
            protocol_maximum: protocol_range.1,
            worker_pid: process_id,
            worker_start_identity,
            boot_identity,
            child: None,
            pty_created_at: None,
            cols: None,
            rows: None,
            phase: RuntimePhase::Bootstrap,
            outcome: None,
            launch_identity: None,
            pending_launch_claims: Vec::new(),
            active_identity: None,
            active_identity_release: None,
            subagent_revision: 0,
            subagents: Vec::new(),
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
        let directory = TrustedDir::open_or_create_absolute(parent, PRIVATE_DIR_MODE)?;

        let mut bytes = serde_json::to_vec_pretty(record).map_err(JournalError::Serialize)?;
        bytes.push(b'\n');
        let temp = temp_path(&self.path);
        let destination = self
            .path
            .file_name()
            .ok_or_else(|| JournalError::MissingParent {
                path: self.path.clone(),
            })?;
        let temporary = temp
            .file_name()
            .ok_or_else(|| JournalError::MissingParent { path: temp.clone() })?;
        directory.replace_file(destination, temporary, &bytes, PRIVATE_FILE_MODE)?;
        Ok(())
    }

    /// Loads and validates the current journal.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] for unsafe paths, I/O, or malformed JSON.
    pub fn load(&self) -> Result<JournalRecord, JournalError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| JournalError::MissingParent {
                path: self.path.clone(),
            })?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| JournalError::MissingParent {
                path: self.path.clone(),
            })?;
        let directory = TrustedDir::open_absolute(parent, PRIVATE_DIR_MODE)?;
        let bytes = directory.read_file(name, PRIVATE_FILE_MODE, MAX_JOURNAL_BYTES)?;
        serde_json::from_slice(&bytes).map_err(|source| JournalError::Corrupt {
            path: self.path.clone(),
            source,
        })
    }
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
    use pohunek_platform::filesystem::AtomicReplaceError;

    use super::{
        Journal, JournalError, JournalRecord, LaunchIdentity, ReleasedIdentity, SubagentPhase,
        SubagentRecord, WorkerOrigin,
    };
    use crate::ChildIdentity;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_dir(tag: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        crate::test_support::temp_root().join(format!(
            "pohunek-session-worker-{tag}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn record(secret: &str) -> JournalRecord {
        let mut record = JournalRecord::bootstrap(
            "s-1".to_owned(),
            "worker-1".to_owned(),
            WorkerOrigin {
                executable: PathBuf::from("/opt/pohunek/libexec/pohunek/0.1.0/pohunek-sessiond"),
                version: "0.1.0".to_owned(),
                generation: "abcd2345".to_owned(),
            },
            10,
            "start-1".to_owned(),
            "boot-test".to_owned(),
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
            .pending_launch_claims
            .push(super::PendingLaunchClaim {
                retry_pending: true,
                runtime_id: "runtime-1".to_owned(),
                root: record.launch_identity.as_ref().unwrap().process.clone(),
                identity: record.launch_identity.as_ref().unwrap().clone(),
                expires_at: "2026-07-23T00:01:00Z".to_owned(),
            });
        record.active_identity_release = Some(ReleasedIdentity {
            provider: "claude".to_owned(),
            process: ChildIdentity {
                pid: 12,
                process_group: 12,
                start_identity: "start-3".to_owned(),
            },
            sequence: 8,
        });
        record.subagent_revision = 1;
        record.subagents.push(SubagentRecord {
            id: "child-1".to_owned(),
            parent_id: None,
            provider: "codex".to_owned(),
            agent_type: Some("explore".to_owned()),
            phase: SubagentPhase::Running,
            sequence: 9,
            revision: 1,
            started_at_ms: 100,
            updated_at_ms: 100,
            finished_at_ms: None,
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
    fn journal_records_executable_version_and_generation_as_top_level_keys() {
        let record = record("native-reference");
        let json = serde_json::to_value(&record).expect("serialize journal");

        assert_eq!(
            json["executable"],
            "/opt/pohunek/libexec/pohunek/0.1.0/pohunek-sessiond"
        );
        assert_eq!(json["version"], "0.1.0");
        assert_eq!(json["generation"], "abcd2345");
        assert!(json.get("origin").is_none());
        assert_eq!(
            serde_json::from_value::<JournalRecord>(json).expect("deserialize journal"),
            record
        );
    }

    #[test]
    fn journal_without_origin_is_rejected() {
        let mut json = serde_json::to_value(record("native-reference")).expect("serialize");
        json.as_object_mut()
            .expect("journal object")
            .remove("generation");

        serde_json::from_value::<JournalRecord>(json).expect_err("generation is required");
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
            Err(JournalError::AtomicReplace(
                AtomicReplaceError::BeforeCommit(_)
            ))
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }
}
