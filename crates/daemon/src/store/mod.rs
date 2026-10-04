//! Unified metadata store (milestone 9).
//!
//! One owner-private JSON-lines file holds the record kinds the daemon must
//! survive a restart with: the resume bindings ([`ResumeBinding`]), the worktree
//! bindings ([`WorktreeBinding`]), and the project records ([`ProjectRecord`]).
//! They share a single file, a single serialization lock, and a single atomic
//! write-path: every mutation rewrites the whole file via one temp file +
//! `rename`, so a write of one record kind can never corrupt or drop a record of
//! another kind, and any single update is crash-atomic (one `rename(2)` commits
//! it). This is the transactional consistency a `SQLite` store would have given,
//! without the dependency (see `docs/ROADMAP.md`).
//!
//! This is a consistency guarantee about the *write path*, not a lifecycle
//! pairing: the records are written by independent triggers (a worktree binding
//! at `session.new`, a resume binding when the agent later reports its native id,
//! a project record on auto-registration or `project add`) and they have
//! independent lifetimes — a stopped session's resume binding is removed, but its
//! worktree binding is intentionally kept (the on-disk worktree holds the user's
//! work; see [`crate::worktree`]), and a project record outlives every session.
//!
//! Each line is a tagged [`Record`] (`{"kind":"resume", ...}` /
//! `{"kind":"worktree", ...}` / `{"kind":"project", ...}`). Every mutation
//! re-reads the whole file under the write lock, edits the relevant record kind,
//! and rewrites **all** records, preserving the other kinds untouched. The file
//! is small (one line per resumable session, per bound worktree, and per known
//! project) so a full rewrite per mutation is cheap. No daemon-derived secrets
//! are ever written: a native session id, a cwd, a repository path, a branch, a
//! worktree path, and a project's git common dir / credential-redacted origin URL
//! are not secrets. Owner-controlled session metadata is also persisted and must
//! not contain secrets.
//!
//! The resume binding additionally carries the **structural relaunch snapshot**
//! (Part C, C.4): `program`, `args`, `input_rules`, `native_launch` (the typed
//! native-session launch spec: reference kind plus resume and optional fork argv
//! with one reference slot each), and `agent_base`. These are the non-secret
//! fields needed to relaunch-and-resume a host-profile session with exactly its
//! launch-time shape after a daemon restart. The profile's **`env` is deliberately NOT among them** —
//! it may hold secrets, so it is re-resolved by agent name at resume, never
//! persisted (a deleted profile resumes from the structural snapshot with no env).
//! An absent `native_launch` means the session has no native recovery. The
//! snapshot is frozen together with the [`LaunchPin`] of the runtime definition
//! it came from; a line without one loads as unpinned. Load and write checks on
//! the agent kind are syntactic (a valid runtime id): a record whose runtime is
//! not installed stays in the file untouched and is refused at resume time.
//! Persisted
//! bindings from earlier pre-1.0 builds do not infer capabilities from current
//! code.

use std::collections::BTreeMap;
use std::fs;
use std::io;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use pohunek_platform::filesystem::{AtomicReplaceError, FsError, TrustedDir};
use protocol::{AgentKind, ProjectSource, RuntimeRef, RuntimeState, SessionInfo};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::agent::host::{LaunchPin, RESERVED_RUNTIME_IDS};
use crate::agent::{InputRules, NativeSessionLaunch, SessionRefKind};
use crate::project::detect::project_id;

#[cfg(unix)]
const OWNER_PRIVATE_FILE_MODE: u32 = 0o600;
const OWNER_PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// Upper bound for the line-oriented daemon metadata store.
const MAX_METADATA_STORE_BYTES: usize = 16 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One session's native resume and fork recovery binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeBinding {
    /// The pohunek session id (stable across restart).
    pub session_id: String,
    /// Owner-set display name, captured so a restart restores it instead of
    /// dropping the session back to id-only display. Serde default (`None`) for a
    /// legacy line; the store carries no compatibility guarantee beyond loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Resolved agent NAME backing the session (a host-profile name or a base
    /// kind). Free string since Part C; a name only, never a profile body/env.
    pub agent: String,
    /// Resolved base kind for the agent (drives resume/handshake on relaunch, and
    /// `session list --filter agent=<base>` grouping after a restart).
    pub agent_base: AgentKind,
    /// Working directory to relaunch in.
    pub cwd: PathBuf,
    /// Terminal width at capture time.
    pub cols: u16,
    /// Terminal height at capture time.
    pub rows: u16,
    /// Captured native session id used to build resume or fork argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path, for agents that resume or fork from a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_path: Option<String>,
    /// Project this session belongs to ([`ProjectRecord::id`]), captured here so a
    /// daemon restart restores the resumed session's project context directly
    /// instead of re-running git detection on its cwd. `None` for a plain-shell
    /// (non-project) session. Serde default so an older line (no field) still
    /// loads; the store carries no compatibility guarantee beyond that (it may be
    /// wiped on upgrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Whether the session's cwd is a linked worktree (vs the main checkout),
    /// captured alongside `project_id` for the same restart-without-redetect
    /// reason. `None` when there is no project / it was never known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_linked_worktree: Option<bool>,
    /// Owner-controlled metadata for the session. The daemon validates size
    /// limits but does not classify values; callers must not store secrets here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Structural relaunch snapshot (C.4): the resolved launch program, frozen at
    /// creation so a host profile's `program` override survives a restart even if
    /// the profile is later edited or deleted. Serde default (`""`) for a legacy
    /// line — the store carries no compatibility guarantee beyond loading.
    #[serde(default)]
    pub program: String,
    /// Structural relaunch snapshot (C.4): the resolved launch args, frozen at
    /// creation. Serde default (`[]`) for a legacy line.
    #[serde(default)]
    pub args: Vec<String>,
    /// Structural relaunch snapshot (C.4): the resolved input-framing rules, frozen
    /// at creation so a profile's `[input_rules]` override survives a restart.
    #[serde(default)]
    pub input_rules: StoredInputRules,
    /// Structural relaunch snapshot (C.4): the native-session launch spec, frozen
    /// at creation so a profile's `[resume]` override drives the relaunch and fork
    /// argv and decides which validation guard the captured reference passes.
    /// `None` for a session without native recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_launch: Option<NativeSessionLaunch>,
    /// The runtime identity the structural snapshot above was frozen with. A
    /// line written without one is [`LaunchPin::Unpinned`].
    #[serde(default, skip_serializing_if = "LaunchPin::is_unpinned")]
    pub launch_binding: LaunchPin,
}

impl ResumeBinding {
    /// The native reference kind this binding recovers with, when it recovers.
    #[must_use]
    pub fn reference_kind(&self) -> Option<SessionRefKind> {
        self.native_launch
            .as_ref()
            .map(NativeSessionLaunch::reference_kind)
    }

    /// Whether the binding can resume natively.
    #[must_use]
    pub fn resumable(&self) -> bool {
        self.native_launch.is_some()
    }

    /// Whether the binding can fork natively.
    #[must_use]
    pub fn forkable(&self) -> bool {
        self.native_launch
            .as_ref()
            .is_some_and(NativeSessionLaunch::supports_fork)
    }
}

impl<'de> Deserialize<'de> for ResumeBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawResumeBinding {
            session_id: String,
            #[serde(default)]
            name: Option<String>,
            agent: String,
            #[serde(default)]
            agent_base: Option<AgentKind>,
            cwd: PathBuf,
            cols: u16,
            rows: u16,
            #[serde(default)]
            native_session_id: Option<String>,
            #[serde(default)]
            native_session_path: Option<String>,
            #[serde(default)]
            project_id: Option<String>,
            #[serde(default)]
            is_linked_worktree: Option<bool>,
            #[serde(default)]
            metadata: BTreeMap<String, String>,
            #[serde(default)]
            program: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            input_rules: StoredInputRules,
            #[serde(default)]
            native_launch: Option<NativeSessionLaunch>,
            #[serde(default)]
            launch_binding: LaunchPin,
        }

        let raw = RawResumeBinding::deserialize(deserializer)?;
        let agent_base = raw
            .agent_base
            .or_else(|| legacy_agent_base_from_agent(&raw.agent))
            .ok_or_else(|| serde::de::Error::missing_field("agent_base"))?;
        // Syntactic check only: whether the runtime is installed is decided
        // when the binding is used, so an unresolvable runtime keeps its line.
        if !kind_is_persistable(&agent_base) {
            return Err(serde::de::Error::custom(
                "agent_base is not a valid runtime id",
            ));
        }

        Ok(Self {
            session_id: raw.session_id,
            name: raw.name,
            agent: raw.agent,
            agent_base,
            cwd: raw.cwd,
            cols: raw.cols,
            rows: raw.rows,
            native_session_id: raw.native_session_id,
            native_session_path: raw.native_session_path,
            project_id: raw.project_id,
            is_linked_worktree: raw.is_linked_worktree,
            metadata: raw.metadata,
            program: raw.program,
            args: raw.args,
            input_rules: raw.input_rules,
            native_launch: raw.native_launch,
            launch_binding: raw.launch_binding,
        })
    }
}

/// Infers the base of a line written without `agent_base` from its agent
/// name; only a built-in runtime id names its own base.
fn legacy_agent_base_from_agent(agent: &str) -> Option<AgentKind> {
    RESERVED_RUNTIME_IDS
        .contains(&agent)
        .then(|| AgentKind::from_wire(agent))
}

/// Whether a record may carry `kind`: a grammar-valid runtime id, installed or
/// not. An uninstalled runtime keeps its record inert instead of dropping it.
fn kind_is_persistable(kind: &AgentKind) -> bool {
    matches!(kind.as_runtime_ref(), RuntimeRef::Id(_))
}

/// Serializable mirror of [`crate::agent::InputRules`] for the resume snapshot
/// (C.4). A `Duration` serializes as a `{secs, nanos}` object; this stores the
/// submit delay flat as whole milliseconds instead, matching the profile TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StoredInputRules {
    /// Whether prompt text is wrapped in bracketed-paste markers.
    #[serde(default)]
    pub bracketed_paste: bool,
    /// Delay before the submit byte, in whole milliseconds.
    #[serde(default)]
    pub submit_delay_ms: u64,
}

impl From<InputRules> for StoredInputRules {
    fn from(rules: InputRules) -> Self {
        Self {
            bracketed_paste: rules.bracketed_paste,
            // Submit delays are small (≤ a few hundred ms); saturate defensively
            // rather than truncate, so a pathological value can never wrap.
            submit_delay_ms: u64::try_from(rules.submit_delay.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

impl StoredInputRules {
    /// Rebuild the in-memory [`InputRules`] from the persisted snapshot,
    /// keeping the safety contract of the runtime's `base` rules.
    #[must_use]
    pub fn to_input_rules(self, base: InputRules) -> InputRules {
        base.with_framing(
            self.bracketed_paste,
            Duration::from_millis(self.submit_delay_ms),
        )
    }
}

/// Lifecycle status of a worktree binding.
///
/// Mirrors Kandev's `active`/`merged`/`deleted` status strings. Today only
/// `Active` (on bind) and `Deleted` (on cleanup) are ever set; `Merged` is
/// reserved for forward-compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    /// The worktree is bound and in use.
    Active,
    /// The worktree's branch was merged (reserved for later milestones).
    Merged,
    /// The worktree was cleaned up.
    Deleted,
}

/// One bound worktree: the persisted record plus the daemon's ownership proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeBinding {
    /// The pohunek session id that owns this worktree.
    pub session_id: String,
    /// Canonicalized path of the source repository.
    pub repository: PathBuf,
    /// Branch checked out in the worktree.
    pub branch: String,
    /// Base branch the worktree's branch was created from.
    pub base_branch: String,
    /// Filesystem-safe branch slug used to disambiguate two branches of one
    /// `(session, repository)` pair so they never collapse onto one path.
    pub branch_slug: String,
    /// Absolute path of the worktree directory.
    pub path: PathBuf,
    /// Resolved agent NAME the worktree was bound for (Part B). A non-secret name
    /// only — never a profile body/env — exposed to remove hooks as `POHUNEK_AGENT`
    /// so pre/post-remove hooks see the same agent identity as create hooks. Serde
    /// default (`""`) for a legacy line written before Part B.
    #[serde(default)]
    pub agent: String,
    /// Lifecycle status.
    pub status: WorktreeStatus,
    /// Project this worktree belongs to ([`ProjectRecord::id`]), when the binding
    /// was created with a resolved project. `None` for a worktree bound before
    /// projects existed or via a bare `--repo` with no project. Lets
    /// `project show`/`project rm --prune-worktrees` find the worktrees pohunek
    /// created for a project. Serde default so an older line (no field) still
    /// loads; the store carries no compatibility guarantee beyond that (the file
    /// may be wiped on upgrade), this just keeps the read path simple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last-update timestamp (RFC3339).
    pub updated_at: String,
}

/// One known project: a git repository the daemon has seen on this host.
///
/// Persisted shape, keyed (and upserted) by the canonical [`Self::git_common_dir`]
/// — the main checkout and every linked worktree of one repository share it, so
/// they collapse to a single record (design `projects.md` → "Data model"). The
/// display `id` and `label` are **derived** ([`Self::id`] / [`Self::label`]), not
/// stored: the id is a deterministic FNV-1a hash of the key, and the label is the
/// custom name or the repo-root basename — so the persisted form holds only what
/// cannot be recomputed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRecord {
    /// The git common dir — the project's identity key (canonical, absolute).
    pub git_common_dir: PathBuf,
    /// The repository's main checkout (the dir an in-place session runs in).
    pub repo_root: PathBuf,
    /// Operator-set display name; overrides the derived label when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_name: Option<String>,
    /// The `origin` remote URL, credentials already redacted; `None` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    /// Base branch for worktrees created against this project; `None` = repo HEAD
    /// at creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    /// Whether the repository is bare (no working tree); a bare project cannot
    /// host an in-place session.
    #[serde(default)]
    pub is_bare: bool,
    /// Whether the record was auto-registered or added explicitly.
    pub source: ProjectSource,
    /// Registration timestamp (RFC3339).
    pub added_at: String,
    /// Last-used timestamp (RFC3339), bumped on each session start in the project.
    pub last_used_at: String,
}

impl ProjectRecord {
    /// The project's stable, derived id (`"p-"` + FNV-1a of the canonical key).
    #[must_use]
    pub fn id(&self) -> String {
        project_id(&self.git_common_dir)
    }

    /// The project's display label: the custom name, else the repo-root basename
    /// (the bare git common dir's basename for a bare repo, which has no
    /// checkout). Empty only for a pathological root-only path.
    #[must_use]
    pub fn label(&self) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }
        self.repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// Outcome of resolving a `<id|label>` project reference against the store.
///
/// The reference resolves to an `id` first ([`ProjectRecord::id`]), then to a
/// `label` ([`ProjectRecord::label`]); a label shared by several projects is
/// [`Ambiguous`](Self::Ambiguous) and the caller disambiguates with an `id`
/// (design Decision 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectResolution {
    /// Exactly one project matched (by id, or by a unique label).
    Found(ProjectRecord),
    /// No project matched the reference.
    NotFound,
    /// Several projects share the referenced label; pick one by its `id`.
    Ambiguous(Vec<ProjectRecord>),
}

/// Desired durable outcome for one logical session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    /// A worker runtime should be live.
    Running,
    /// The current runtime should be terminal.
    Stopped,
    /// The runtime and logical record should be removed.
    Removed,
}

/// Durable lifecycle operation that reconciliation must finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    /// Initial worker creation.
    Create,
    /// Explicit runtime stop.
    Stop,
    /// Explicit provider-native recovery.
    Recover,
    /// Logical session removal.
    Remove,
}

/// One in-progress idempotent lifecycle transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTransaction {
    /// Stable operation identifier used for deduplication.
    pub id: String,
    /// Operation being completed.
    pub kind: TransactionKind,
    /// Stable implementation phase.
    pub phase: String,
    /// Worker replaced by a recovery transaction, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_worker_id: Option<String>,
    /// Runtime generation replaced by a recovery transaction, when known.
    /// The persisted key stays `runtime_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "previous_runtime_id")]
    pub previous_worker_instance_id: Option<String>,
    /// Daemon instance whose in-memory work the transaction depends on.
    ///
    /// Set on the `initial_input` marker, whose input only that instance
    /// holds: reconciliation rolls back a marker only when another instance
    /// (or none) wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<String>,
}

/// Phase of a committed create whose initial input is not yet delivered.
///
/// Reconciliation rolls such a create back, since the input exists only in
/// the daemon's memory and the session must not run without it.
pub const INITIAL_INPUT_PHASE: &str = "initial_input";

impl SessionTransaction {
    /// Whether this is a committed create still delivering its initial input.
    #[must_use]
    pub fn is_initial_input(&self) -> bool {
        self.kind == TransactionKind::Create && self.phase == INITIAL_INPUT_PHASE
    }
}

/// Last durable binding between a logical session and its worker runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeRecord {
    /// Current worker availability.
    pub state: RuntimeState,
    /// Stable worker identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Stable PTY generation identity.
    /// The persisted key stays `runtime_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: Option<String>,
    /// Supervisor service identifier `<session-id>.<generation>` of the
    /// current worker job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    /// Daemon-issued worker generation token, persisted before the job starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Absolute versioned worker executable named by the job definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    /// Machine-readable loss or conflict reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Durable ordering key for provider-native identity reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeIdentityOrdering {
    /// Runtime generation that accepted the report.
    /// The persisted key stays `runtime_id`.
    #[serde(rename = "runtime_id")]
    pub worker_instance_id: String,
    /// Process id validated for the runtime root.
    pub pid: u32,
    /// Kernel start identity that protects against pid reuse.
    pub pid_start_identity: u64,
    /// Highest accepted monotonic sequence for this runtime.
    pub sequence: u64,
}

/// Durable logical session authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// On-disk record schema.
    pub schema_version: u32,
    /// Stable logical session identifier.
    pub session_id: String,
    /// Desired lifecycle outcome.
    pub desired_state: DesiredState,
    /// In-progress operation, when reconciliation has work to finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<SessionTransaction>,
    /// Sanitized client-facing logical snapshot.
    pub info: SessionInfo,
    /// Structural native-recovery snapshot without profile environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ResumeBinding>,
    /// Last accepted native identity ordering key, including reports that only
    /// reaffirmed an already-known reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_identity_ordering: Option<NativeIdentityOrdering>,
    /// Last durable worker binding.
    pub runtime: RuntimeRecord,
}

/// Result of a conditional durable logical-session write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionWriteOutcome {
    /// The candidate became the durable session authority.
    Applied,
    /// The candidate is visible at the authoritative path, but syncing the
    /// parent directory failed after the atomic rename.
    AppliedDurabilityUncertain {
        /// Sanitized filesystem error describing the failed durability step.
        error: String,
    },
    /// A newer or different runtime already owns this generation on disk.
    StaleRuntime,
    /// The durable record changed after the caller captured its base snapshot.
    StaleSnapshot,
}

/// A single line of the unified store, internally tagged by `kind` so both
/// record kinds coexist in one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Session(Box<SessionRecord>),
    Resume(ResumeBinding),
    Worktree(WorktreeBinding),
    Project(ProjectRecord),
}

type StoreRecords = (
    Vec<ResumeBinding>,
    Vec<WorktreeBinding>,
    Vec<ProjectRecord>,
    Vec<SessionRecord>,
);

enum MetadataWriteOutcome {
    Synced,
    CommittedDurabilityUncertain(io::Error),
}

impl MetadataWriteOutcome {
    fn with_value<T>(self, value: T) -> StoreMutation<T> {
        match self {
            Self::Synced => StoreMutation::Synced(value),
            Self::CommittedDurabilityUncertain(error) => {
                warn!(
                    error = %error,
                    "metadata store mutation committed but durability is uncertain"
                );
                StoreMutation::CommittedDurabilityUncertain {
                    value,
                    error: error.to_string(),
                }
            }
        }
    }
}

/// Result of a metadata mutation whose rename may precede directory-sync failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreMutation<T> {
    /// The namespace mutation and its directory synchronization completed.
    Synced(T),
    /// The new authoritative contents are visible, but crash durability is uncertain.
    CommittedDurabilityUncertain {
        /// Value produced by the committed mutation.
        value: T,
        /// Sanitized description of the failed durability step.
        error: String,
    },
}

impl<T> StoreMutation<T> {
    /// Returns the committed value regardless of the final durability outcome.
    #[must_use]
    pub fn into_value(self) -> T {
        match self {
            Self::Synced(value) | Self::CommittedDurabilityUncertain { value, .. } => value,
        }
    }
}

/// File-backed unified metadata store.
///
/// A single internal lock is the **one writer-serialization point** for the
/// file; every mutating method rewrites the whole file under it via one atomic
/// temp+rename, so the two record kinds stay mutually consistent.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    write_lock: Mutex<()>,
    #[cfg(test)]
    fail_parent_sync_after_rename: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_before_rename_countdown: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_binding_removal: std::sync::atomic::AtomicBool,
}

impl Store {
    /// Open a store at `path`. The file is created lazily on the first write.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
            #[cfg(test)]
            fail_parent_sync_after_rename: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_before_rename_countdown: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_binding_removal: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Makes the next [`Self::remove_worktree_binding`] fail before it writes.
    #[cfg(test)]
    pub(crate) fn fail_next_binding_removal(&self) {
        self.fail_binding_removal
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_parent_sync_after_rename(&self) {
        self.fail_parent_sync_after_rename
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_write_before_rename(&self) {
        self.fail_write_before_rename_after(1);
    }

    #[cfg(test)]
    pub(crate) fn fail_write_before_rename_after(&self, writes: usize) {
        self.fail_before_rename_countdown
            .store(writes, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn should_fail_write_before_rename(&self) -> bool {
        let mut remaining = self
            .fail_before_rename_countdown
            .load(std::sync::atomic::Ordering::SeqCst);

        loop {
            let Some(next) = remaining.checked_sub(1) else {
                return false;
            };

            match self.fail_before_rename_countdown.compare_exchange_weak(
                remaining,
                next,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(previous) => return previous == 1,
                Err(actual) => remaining = actual,
            }
        }
    }

    /// The backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All resume bindings. A missing file yields an empty list.
    pub fn load_resume(&self) -> io::Result<Vec<ResumeBinding>> {
        Ok(self.read_all()?.0)
    }

    /// All worktree bindings. A missing file yields an empty list.
    pub fn load_worktrees(&self) -> io::Result<Vec<WorktreeBinding>> {
        Ok(self.read_all()?.1)
    }

    /// All project records. A missing file yields an empty list.
    pub fn load_projects(&self) -> io::Result<Vec<ProjectRecord>> {
        Ok(self.read_all()?.2)
    }

    /// Loads every durable logical session.
    pub fn load_sessions(&self) -> io::Result<Vec<SessionRecord>> {
        Ok(self.read_all()?.3)
    }

    /// Upserts one logical session and preserves every other record kind.
    pub fn record_session(&self, record: &SessionRecord) -> io::Result<SessionWriteOutcome> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, projects, mut sessions) = self.read_all()?;
        if let Some(existing) = sessions
            .iter_mut()
            .find(|existing| existing.session_id == record.session_id)
        {
            if stale_runtime_snapshot(existing, record) {
                return Ok(SessionWriteOutcome::StaleRuntime);
            }
            let mut replacement = record.clone();
            preserve_newer_native_identity(existing, &mut replacement);
            keep_initial_input_settled(existing, &mut replacement);
            *existing = replacement;
        } else {
            sessions.push(record.clone());
        }
        match self.write_all(&resume, &worktrees, &projects, &sessions)? {
            MetadataWriteOutcome::Synced => Ok(SessionWriteOutcome::Applied),
            MetadataWriteOutcome::CommittedDurabilityUncertain(error) => {
                Ok(SessionWriteOutcome::AppliedDurabilityUncertain {
                    error: error.to_string(),
                })
            }
        }
    }

    /// Conditionally replaces one session only when its durable record still
    /// equals `expected`, preventing a stale same-runtime snapshot from ever
    /// reaching the authoritative path.
    pub fn record_session_if_current(
        &self,
        expected: &SessionRecord,
        record: &SessionRecord,
    ) -> io::Result<SessionWriteOutcome> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, projects, mut sessions) = self.read_all()?;
        let Some(existing) = sessions
            .iter_mut()
            .find(|existing| existing.session_id == record.session_id)
        else {
            return Ok(SessionWriteOutcome::StaleSnapshot);
        };
        if stale_runtime_snapshot(existing, record) {
            return Ok(SessionWriteOutcome::StaleRuntime);
        }
        if existing != expected {
            return Ok(SessionWriteOutcome::StaleSnapshot);
        }
        let mut replacement = record.clone();
        preserve_newer_native_identity(existing, &mut replacement);
        keep_initial_input_settled(existing, &mut replacement);
        *existing = replacement;
        match self.write_all(&resume, &worktrees, &projects, &sessions)? {
            MetadataWriteOutcome::Synced => Ok(SessionWriteOutcome::Applied),
            MetadataWriteOutcome::CommittedDurabilityUncertain(error) => {
                Ok(SessionWriteOutcome::AppliedDurabilityUncertain {
                    error: error.to_string(),
                })
            }
        }
    }

    /// Clears the `create/initial_input` transaction of one session's
    /// current record and leaves every other field as persisted.
    ///
    /// The record is re-read under the write lock, so a state a concurrent
    /// stop, removal, or exit persisted is kept. Returns whether a marker was
    /// cleared; a record without it, or no record at all, is a no-op success.
    pub fn clear_initial_input(&self, session_id: &str) -> io::Result<StoreMutation<bool>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, projects, mut sessions) = self.read_all()?;
        let Some(record) = sessions.iter_mut().find(|record| {
            record.session_id == session_id
                && record
                    .transaction
                    .as_ref()
                    .is_some_and(SessionTransaction::is_initial_input)
        }) else {
            return Ok(StoreMutation::Synced(false));
        };
        record.transaction = None;
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(true))
    }

    /// Removes one logical session and preserves every other record kind.
    pub fn remove_session(&self, session_id: &str) -> io::Result<StoreMutation<bool>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, projects, mut sessions) = self.read_all()?;
        let before = sessions.len();
        sessions.retain(|record| record.session_id != session_id);
        let removed = before != sessions.len();
        if removed {
            return Ok(self
                .write_all(&resume, &worktrees, &projects, &sessions)?
                .with_value(true));
        }
        Ok(StoreMutation::Synced(false))
    }

    /// Upsert a resume binding (keyed by `session_id`), preserving every worktree
    /// record, and rewrite the file atomically.
    pub fn record_resume(&self, binding: &ResumeBinding) -> io::Result<StoreMutation<()>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut resume, worktrees, projects, sessions) = self.read_all()?;
        let mut replacement = binding.clone();
        if let Some(authoritative) = sessions
            .iter()
            .find(|record| record.session_id == binding.session_id)
            .and_then(|record| record.recovery.as_ref())
            .filter(|recovery| {
                recovery.native_session_id.is_some() || recovery.native_session_path.is_some()
            })
        {
            replacement
                .native_session_id
                .clone_from(&authoritative.native_session_id);
            replacement
                .native_session_path
                .clone_from(&authoritative.native_session_path);
        }
        if let Some(existing) = resume
            .iter_mut()
            .find(|existing| existing.session_id == binding.session_id)
        {
            *existing = replacement;
        } else {
            resume.push(replacement);
        }
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(()))
    }

    /// Remove a resume binding by session id, preserving every worktree record. A
    /// missing entry is a no-op.
    pub fn remove_resume(&self, session_id: &str) -> io::Result<StoreMutation<()>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut resume, worktrees, projects, sessions) = self.read_all()?;
        let before = resume.len();
        resume.retain(|binding| binding.session_id != session_id);
        if resume.len() == before {
            return Ok(StoreMutation::Synced(()));
        }
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(()))
    }

    /// Find the active worktree binding for a `(session_id, repository,
    /// branch_slug)` triple. Deleted/merged rows are invisible (reuse filter), so
    /// a cleaned-up session re-binds fresh.
    pub fn find_worktree(
        &self,
        session_id: &str,
        repository: &Path,
        branch_slug: &str,
    ) -> io::Result<Option<WorktreeBinding>> {
        Ok(self.load_worktrees()?.into_iter().find(|binding| {
            binding.status == WorktreeStatus::Active
                && binding.session_id == session_id
                && binding.repository == repository
                && binding.branch_slug == branch_slug
        }))
    }

    /// Find the first active worktree binding owned by `session_id` (a session
    /// binds at most one worktree). Used by resume to restore a session's
    /// worktree metadata without knowing its repository/branch.
    pub fn find_worktree_for_session(
        &self,
        session_id: &str,
    ) -> io::Result<Option<WorktreeBinding>> {
        Ok(self.load_worktrees()?.into_iter().find(|binding| {
            binding.status == WorktreeStatus::Active && binding.session_id == session_id
        }))
    }

    /// Upsert a worktree binding (keyed by `(session_id, repository,
    /// branch_slug)`), preserving every resume record, and rewrite the file
    /// atomically. The triple key keeps two branches of one `(session,
    /// repository)` pair from collapsing onto a single row.
    pub fn record_worktree(&self, binding: &WorktreeBinding) -> io::Result<StoreMutation<()>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, mut worktrees, projects, sessions) = self.read_all()?;
        if let Some(existing) = worktrees.iter_mut().find(|existing| {
            existing.session_id == binding.session_id
                && existing.repository == binding.repository
                && existing.branch_slug == binding.branch_slug
        }) {
            *existing = binding.clone();
        } else {
            worktrees.push(binding.clone());
        }
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(()))
    }

    /// Remove every worktree binding owned by `session_id`, preserving every
    /// resume record. Returns the number removed (`0` is a no-op success).
    pub fn remove_worktree_session(&self, session_id: &str) -> io::Result<StoreMutation<usize>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, mut worktrees, projects, sessions) = self.read_all()?;
        let before = worktrees.len();
        worktrees.retain(|binding| binding.session_id != session_id);
        let removed = before - worktrees.len();
        if removed > 0 {
            return Ok(self
                .write_all(&resume, &worktrees, &projects, &sessions)?
                .with_value(removed));
        }
        Ok(StoreMutation::Synced(0))
    }

    /// Remove the one worktree binding keyed like `binding` (`(session_id,
    /// repository, branch_slug)`), preserving every other record. Returns
    /// whether a binding was removed (`false` is a no-op success).
    pub fn remove_worktree_binding(
        &self,
        binding: &WorktreeBinding,
    ) -> io::Result<StoreMutation<bool>> {
        #[cfg(test)]
        if self
            .fail_binding_removal
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(io::Error::other(
                "injected worktree binding removal failure",
            ));
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, mut worktrees, projects, sessions) = self.read_all()?;
        let before = worktrees.len();
        worktrees.retain(|existing| {
            existing.session_id != binding.session_id
                || existing.repository != binding.repository
                || existing.branch_slug != binding.branch_slug
        });
        if worktrees.len() != before {
            return Ok(self
                .write_all(&resume, &worktrees, &projects, &sessions)?
                .with_value(true));
        }
        Ok(StoreMutation::Synced(false))
    }

    /// Atomically read-modify-write the project keyed by canonical
    /// `git_common_dir`, **entirely under the store write lock** so a concurrent
    /// edit cannot be clobbered by a stale snapshot. This is the safe alternative
    /// to the `load_projects()` → mutate a detached copy → `record_project()`
    /// pattern, which reads outside the lock and so races: two callers can each
    /// read the same record, each mutate a different field, and the second write
    /// reverts the first.
    ///
    /// `mutate` receives the current record (`None` when the project is absent)
    /// and returns the record to store, or `None` to leave the store untouched.
    /// A returned record is upserted by `git_common_dir` (inserted when absent),
    /// so the closure expresses both create-if-missing (return `Some` for a
    /// `None` input) and update-only (return `None` for a `None` input) policies.
    /// Returns the stored record, or `None` when `mutate` declined to write.
    pub fn mutate_project<F>(
        &self,
        git_common_dir: &Path,
        mutate: F,
    ) -> io::Result<StoreMutation<Option<ProjectRecord>>>
    where
        F: FnOnce(Option<ProjectRecord>) -> Option<ProjectRecord>,
    {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, mut projects, sessions) = self.read_all()?;
        let pos = projects
            .iter()
            .position(|existing| existing.git_common_dir == git_common_dir);
        let current = pos.map(|index| projects[index].clone());
        let Some(updated) = mutate(current) else {
            return Ok(StoreMutation::Synced(None));
        };
        match pos {
            Some(index) => projects[index] = updated.clone(),
            None => projects.push(updated.clone()),
        }
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(Some(updated)))
    }

    /// Upsert a project record (keyed by canonical `git_common_dir`), preserving
    /// every resume and worktree record, and rewrite the file atomically.
    /// Re-detecting (or re-adding) the same repository updates the existing record
    /// in place — never duplicates — because the git common dir is the natural
    /// key. The caller supplies an already-canonical key (detection and
    /// `project add` both canonicalize), so matching is exact-path, mirroring how
    /// worktree records key on the canonicalized repository.
    ///
    /// Prefer [`Self::mutate_project`] when the new value depends on the current
    /// record (read-modify-write); this whole-record overwrite is for callers that
    /// already hold the complete intended record.
    pub fn record_project(&self, record: &ProjectRecord) -> io::Result<StoreMutation<()>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, mut projects, sessions) = self.read_all()?;
        if let Some(existing) = projects
            .iter_mut()
            .find(|existing| existing.git_common_dir == record.git_common_dir)
        {
            *existing = record.clone();
        } else {
            projects.push(record.clone());
        }
        Ok(self
            .write_all(&resume, &worktrees, &projects, &sessions)?
            .with_value(()))
    }

    /// Remove the project keyed by `git_common_dir`, preserving every resume and
    /// worktree record. Returns whether a record was removed (`false` is a no-op
    /// success). Only forgets the record; it never touches the on-disk repository
    /// or its worktrees.
    pub fn remove_project(&self, git_common_dir: &Path) -> io::Result<StoreMutation<bool>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (resume, worktrees, mut projects, sessions) = self.read_all()?;
        let before = projects.len();
        projects.retain(|project| project.git_common_dir != git_common_dir);
        let removed = projects.len() != before;
        if removed {
            return Ok(self
                .write_all(&resume, &worktrees, &projects, &sessions)?
                .with_value(true));
        }
        Ok(StoreMutation::Synced(false))
    }

    /// Resolve a `<id|label>` reference to a project (design Decision 2):
    /// an exact `id` match wins; otherwise a `label` match resolves when it is
    /// unique, is [`ProjectResolution::Ambiguous`] when several share the label,
    /// and is [`ProjectResolution::NotFound`] when none match. Read-only.
    pub fn find_project(&self, reference: &str) -> io::Result<ProjectResolution> {
        let projects = self.load_projects()?;
        if let Some(found) = projects.iter().find(|project| project.id() == reference) {
            return Ok(ProjectResolution::Found(found.clone()));
        }
        let mut by_label: Vec<ProjectRecord> = projects
            .into_iter()
            .filter(|project| project.label() == reference)
            .collect();
        Ok(match by_label.len() {
            0 => ProjectResolution::NotFound,
            1 => ProjectResolution::Found(by_label.remove(0)),
            _ => ProjectResolution::Ambiguous(by_label),
        })
    }

    /// Read and partition every record. A missing file yields three empty lists;
    /// malformed lines are skipped (a corrupt line must not block loading the
    /// rest).
    fn read_all(&self) -> io::Result<StoreRecords> {
        let parent = parent_directory(&self.path);
        if !parent.exists() {
            return Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new()));
        }
        let directory = TrustedDir::open_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
            .map_err(fs_error_to_io)?;
        let name = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata store has no filename",
            )
        })?;
        let bytes =
            match directory.read_file(name, OWNER_PRIVATE_FILE_MODE, MAX_METADATA_STORE_BYTES) {
                Ok(bytes) => bytes,
                Err(error) if error.io_kind() == Some(io::ErrorKind::NotFound) => {
                    return Ok((Vec::new(), Vec::new(), Vec::new(), Vec::new()))
                }
                Err(error) => return Err(fs_error_to_io(error)),
            };
        let content = String::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut resume = Vec::new();
        let mut worktrees = Vec::new();
        let mut projects = Vec::new();
        let mut sessions = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Session(record)) if record_agents_are_persistable(&record) => {
                    sessions.push(*record);
                }
                Ok(Record::Resume(binding)) if kind_is_persistable(&binding.agent_base) => {
                    resume.push(binding);
                }
                Ok(Record::Session(_) | Record::Resume(_)) => {
                    warn!("skipping metadata-store record whose agent kind is not a runtime id");
                }
                Ok(Record::Worktree(binding)) => worktrees.push(binding),
                Ok(Record::Project(record)) => projects.push(record),
                // Skip a corrupt line so it cannot block loading the rest, but
                // surface it: a silently-dropped resume line means a session
                // never comes back, a dropped worktree line loses its restored
                // metadata, and a dropped project line forgets a known repo. The
                // store holds no secrets, so logging the offending line is safe
                // and aids debugging.
                Err(err) => {
                    warn!(error = %err, line = %line, "skipping unparseable metadata-store line");
                }
            }
        }
        Ok((resume, worktrees, projects, sessions))
    }

    /// Serialize all records (resume, then worktree, then project) to a temp file
    /// and rename it over the store path. One `rename(2)` commits all kinds.
    fn write_all(
        &self,
        resume: &[ResumeBinding],
        worktrees: &[WorktreeBinding],
        projects: &[ProjectRecord],
        sessions: &[SessionRecord],
    ) -> io::Result<MetadataWriteOutcome> {
        if resume
            .iter()
            .any(|binding| !kind_is_persistable(&binding.agent_base))
            || sessions
                .iter()
                .any(|record| !record_agents_are_persistable(record))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "metadata records cannot persist agent kinds that are not runtime ids",
            ));
        }
        if let Some(parent) = self.path.parent() {
            TrustedDir::open_or_create_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
                .map_err(fs_error_to_io)?;
        }
        let mut body = String::new();
        for binding in resume {
            append_line(&mut body, &Record::Resume(binding.clone()))?;
        }
        for binding in worktrees {
            append_line(&mut body, &Record::Worktree(binding.clone()))?;
        }
        for record in projects {
            append_line(&mut body, &Record::Project(record.clone()))?;
        }
        for record in sessions {
            append_line(&mut body, &Record::Session(Box::new(record.clone())))?;
        }
        if body.len() > MAX_METADATA_STORE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "metadata store exceeds the {MAX_METADATA_STORE_BYTES}-byte durable read limit"
                ),
            ));
        }

        let tmp = self.temp_path();
        #[cfg(test)]
        let should_fail = self.should_fail_write_before_rename();
        #[cfg(test)]
        if should_fail {
            return Err(io::Error::other("injected pre-rename write failure"));
        }
        let parent = parent_directory(&self.path);
        let directory = TrustedDir::open_absolute(parent, OWNER_PRIVATE_DIRECTORY_MODE)
            .map_err(fs_error_to_io)?;
        let destination = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata store has no filename",
            )
        })?;
        let temporary = tmp.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "metadata temporary has no filename",
            )
        })?;
        match directory.replace_file(
            destination,
            temporary,
            body.as_bytes(),
            OWNER_PRIVATE_FILE_MODE,
        ) {
            Ok(()) => {}
            Err(AtomicReplaceError::BeforeCommit(error)) => {
                return Err(fs_error_to_io(error));
            }
            Err(AtomicReplaceError::CommittedDurabilityUncertain(error)) => {
                return Ok(MetadataWriteOutcome::CommittedDurabilityUncertain(
                    io::Error::other(error),
                ));
            }
            Err(error) => return Err(io::Error::other(error)),
        }
        #[cfg(test)]
        if self
            .fail_parent_sync_after_rename
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(MetadataWriteOutcome::CommittedDurabilityUncertain(
                io::Error::other("injected parent directory sync failure"),
            ));
        }
        match sync_parent_directory(&self.path) {
            Ok(()) => Ok(MetadataWriteOutcome::Synced),
            Err(error) => Ok(MetadataWriteOutcome::CommittedDurabilityUncertain(error)),
        }
    }

    fn temp_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map_or_else(|| "metadata.jsonl".into(), std::ffi::OsStr::to_os_string);
        name.push(format!(".tmp.{}", std::process::id()));
        name.push(format!(
            ".{}",
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match self.path.parent() {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        }
    }
}

fn fs_error_to_io(error: FsError) -> io::Error {
    let kind = match &error {
        FsError::InvalidAbsolutePath { .. }
        | FsError::InvalidComponent { .. }
        | FsError::InvalidMode { .. }
        | FsError::UnsafeType { .. }
        | FsError::UnsafeOwner { .. }
        | FsError::UnsafeMode { .. }
        | FsError::UnsafeLinkCount { .. }
        | FsError::IdentityChanged { .. } => io::ErrorKind::InvalidInput,
        FsError::FileTooLarge { .. } => io::ErrorKind::InvalidData,
        _ if error.raw_os_error() == Some(libc::ELOOP) => io::ErrorKind::InvalidInput,
        _ => error.io_kind().unwrap_or(io::ErrorKind::Other),
    };
    io::Error::new(kind, error)
}

/// Drops a pending-initial-input marker that `replacement` would re-add.
///
/// The marker is written only by the create commit, over the create's own
/// transaction, and cleared once the input is delivered. A writer that built
/// its record before that clearing must not bring the marker back, or a
/// restart would roll back a session that already received its input.
fn keep_initial_input_settled(existing: &SessionRecord, replacement: &mut SessionRecord) {
    let reintroduced = replacement
        .transaction
        .as_ref()
        .is_some_and(SessionTransaction::is_initial_input)
        && existing
            .transaction
            .as_ref()
            .is_none_or(|transaction| transaction.kind != TransactionKind::Create);
    if reintroduced {
        replacement.transaction = None;
    }
}

pub(crate) fn preserve_newer_native_identity(
    existing: &SessionRecord,
    replacement: &mut SessionRecord,
) {
    let Some(ordering) = existing.native_identity_ordering.as_ref() else {
        return;
    };
    if replacement.runtime.worker_instance_id.as_deref()
        != Some(ordering.worker_instance_id.as_str())
    {
        return;
    }
    if let Some(candidate) = replacement
        .native_identity_ordering
        .as_ref()
        .filter(|candidate| {
            candidate.worker_instance_id == ordering.worker_instance_id
                && candidate.sequence >= ordering.sequence
        })
    {
        if candidate.sequence > ordering.sequence {
            return;
        }
        replacement.native_identity_ordering = Some(ordering.clone());
        preserve_nonempty_native_identity(existing, replacement);
        return;
    }

    replacement.native_identity_ordering = Some(ordering.clone());
    replacement
        .info
        .native_session_id
        .clone_from(&existing.info.native_session_id);
    replacement
        .info
        .native_session_path
        .clone_from(&existing.info.native_session_path);
    match (replacement.recovery.as_mut(), existing.recovery.as_ref()) {
        (Some(replacement), Some(existing)) => {
            replacement
                .native_session_id
                .clone_from(&existing.native_session_id);
            replacement
                .native_session_path
                .clone_from(&existing.native_session_path);
        }
        (None, Some(existing)) => replacement.recovery = Some(existing.clone()),
        _ => {}
    }
}

fn preserve_nonempty_native_identity(existing: &SessionRecord, replacement: &mut SessionRecord) {
    if existing.info.native_session_id.is_some() {
        replacement
            .info
            .native_session_id
            .clone_from(&existing.info.native_session_id);
    }
    if existing.info.native_session_path.is_some() {
        replacement
            .info
            .native_session_path
            .clone_from(&existing.info.native_session_path);
    }
    match (replacement.recovery.as_mut(), existing.recovery.as_ref()) {
        (Some(replacement), Some(existing)) => {
            if existing.native_session_id.is_some() {
                replacement
                    .native_session_id
                    .clone_from(&existing.native_session_id);
            }
            if existing.native_session_path.is_some() {
                replacement
                    .native_session_path
                    .clone_from(&existing.native_session_path);
            }
        }
        (None, Some(existing)) => replacement.recovery = Some(existing.clone()),
        _ => {}
    }
}

fn stale_runtime_snapshot(existing: &SessionRecord, replacement: &SessionRecord) -> bool {
    let (Some(current), Some(candidate)) = (
        existing.info.runtime.as_ref(),
        replacement.info.runtime.as_ref(),
    ) else {
        return false;
    };
    if candidate.runtime_generation < current.runtime_generation {
        return true;
    }
    if candidate.runtime_generation > current.runtime_generation {
        return false;
    }

    let current_id = existing
        .runtime
        .worker_instance_id
        .as_deref()
        .or(current.runtime_id.as_deref());
    let candidate_id = replacement
        .runtime
        .worker_instance_id
        .as_deref()
        .or(candidate.runtime_id.as_deref());
    current_id.is_some() && candidate_id != current_id
}

fn record_agents_are_persistable(record: &SessionRecord) -> bool {
    kind_is_persistable(&record.info.agent_base)
        && record
            .info
            .active_agent_base
            .as_ref()
            .is_none_or(kind_is_persistable)
        && record
            .recovery
            .as_ref()
            .is_none_or(|binding| kind_is_persistable(&binding.agent_base))
}

/// Serialize one record onto `body` as a single JSON line. Our own types
/// serialize infallibly; any error is mapped to io for the caller.
fn append_line(body: &mut String, record: &Record) -> io::Result<()> {
    let line = serde_json::to_string(record)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    body.push_str(&line);
    body.push('\n');
    Ok(())
}

/// Write a file with owner-only permissions (`0600`).
#[cfg(test)]
fn write_owner_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = owner_private_replace_options().open(path)?;
    set_owner_private_file_permissions(path)?;
    file.write_all(bytes)?;
    pohunek_platform::filesystem::sync_file(&file).map_err(io::Error::other)
}

/// Persist the directory entry created by the atomic rename.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let directory = fs::File::open(parent_directory(path))?;
    pohunek_platform::filesystem::sync_file(&directory).map_err(io::Error::other)
}

/// Directory handles are not portably openable outside Unix.
#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
fn owner_private_replace_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(OWNER_PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW);
    };
    options
}

#[cfg(unix)]
#[cfg(test)]
fn set_owner_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_PRIVATE_FILE_MODE))
}

#[cfg(not(unix))]
#[cfg(test)]
fn set_owner_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use protocol::{AgentActivity, AgentKind, ProjectSource, RuntimeState};

    use super::{
        NativeIdentityOrdering, ProjectRecord, ProjectResolution, ResumeBinding, RuntimeRecord,
        SessionRefKind, SessionTransaction, Store, StoreMutation, StoredInputRules,
        WorktreeBinding, WorktreeStatus,
    };
    use crate::agent::host::LaunchPin;
    use crate::agent::{NativeArgs, NativeSessionLaunch};

    /// Persisted runtime records keep the `runtime_id` key for the worker
    /// instance, so records written by an earlier daemon build still load.
    #[test]
    fn persisted_worker_instance_ids_keep_the_runtime_id_key() {
        let runtime: RuntimeRecord = serde_json::from_value(serde_json::json!({
            "state": "live",
            "runtime_id": "instance-1",
        }))
        .expect("decode runtime record from the persisted key");
        assert_eq!(runtime.state, RuntimeState::Live);
        assert_eq!(runtime.worker_instance_id.as_deref(), Some("instance-1"));
        let encoded = serde_json::to_value(&runtime).expect("encode runtime record");
        assert_eq!(encoded["runtime_id"], "instance-1");
        assert!(encoded.get("worker_instance_id").is_none());

        let ordering = NativeIdentityOrdering {
            worker_instance_id: "instance-1".to_owned(),
            pid: 7,
            pid_start_identity: 9,
            sequence: 3,
        };
        let encoded = serde_json::to_value(&ordering).expect("encode ordering");
        assert_eq!(encoded["runtime_id"], "instance-1");
        assert!(encoded.get("worker_instance_id").is_none());

        let transaction: SessionTransaction = serde_json::from_value(serde_json::json!({
            "id": "t-1",
            "kind": "recover",
            "phase": "started",
            "previous_runtime_id": "instance-0",
        }))
        .expect("decode transaction from the persisted key");
        assert_eq!(
            transaction.previous_worker_instance_id.as_deref(),
            Some("instance-0")
        );
    }

    fn launch(kind: SessionRefKind, resume: &[&str], fork: Option<&[&str]>) -> NativeSessionLaunch {
        NativeSessionLaunch::new(
            kind,
            NativeArgs::from_template(resume).expect("resume template"),
            fork.map(|tokens| NativeArgs::from_template(tokens).expect("fork template")),
        )
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        // The directory lives until the test's thread ends.
        crate::test_support::thread_scoped_dir(&format!("pohunek-store-{tag}-"))
            .join("metadata.jsonl")
    }

    fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
        fs::write(path, bytes).expect("write private fixture");
        fs::set_permissions(
            path,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .expect("make fixture file private");
    }

    fn resume(session_id: &str, native: &str) -> ResumeBinding {
        ResumeBinding {
            session_id: session_id.to_owned(),
            name: None,
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace/project"),
            cols: 120,
            rows: 40,
            native_session_id: Some(native.to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: BTreeMap::new(),
            program: "claude".to_owned(),
            args: Vec::new(),
            input_rules: StoredInputRules {
                bracketed_paste: false,
                submit_delay_ms: 150,
            },
            native_launch: Some(launch(
                SessionRefKind::Id,
                &["--resume", "{reference}"],
                Some(&["--resume", "{reference}", "--fork-session"]),
            )),
            launch_binding: LaunchPin::Unpinned,
        }
    }

    /// A binding to a package-provided runtime.
    fn package_binding(runtime_id: &str) -> protocol::LaunchBinding {
        protocol::LaunchBinding {
            runtime_id: protocol::RuntimeId::parse(runtime_id).expect("runtime id"),
            provenance: protocol::BindingProvenance::Package {
                package: protocol::PackageIdentity {
                    id: protocol::PackageId::parse("acme.runtime").expect("package id"),
                    version: protocol::PackageVersion::parse("1.2.3").expect("version"),
                },
                package_digest: protocol::PackageDigest::parse(
                    "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                )
                .expect("digest"),
            },
        }
    }

    fn worktree(session_id: &str, slug: &str) -> WorktreeBinding {
        WorktreeBinding {
            session_id: session_id.to_owned(),
            repository: PathBuf::from("/workspace/project"),
            branch: format!("feat/{slug}"),
            base_branch: "main".to_owned(),
            branch_slug: slug.to_owned(),
            path: PathBuf::from(format!("/data/worktrees/{session_id}-project-{slug}")),
            agent: "claude".to_owned(),
            status: WorktreeStatus::Active,
            project_id: None,
            created_at: "2026-06-19T00:00:00Z".to_owned(),
            updated_at: "2026-06-19T00:00:00Z".to_owned(),
        }
    }

    fn project(common_dir: &str, repo_root: &str, custom_name: Option<&str>) -> ProjectRecord {
        ProjectRecord {
            git_common_dir: PathBuf::from(common_dir),
            repo_root: PathBuf::from(repo_root),
            custom_name: custom_name.map(str::to_owned),
            origin_url: Some("https://github.com/example/repo.git".to_owned()),
            default_base_branch: None,
            is_bare: false,
            source: ProjectSource::Auto,
            added_at: "2026-06-19T00:00:00Z".to_owned(),
            last_used_at: "2026-06-19T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn load_missing_file_is_empty() {
        let store = Store::new(temp_store_path("missing"));
        assert!(store.load_resume().expect("load resume").is_empty());
        assert!(store.load_worktrees().expect("load worktrees").is_empty());
    }

    #[test]
    fn resume_round_trips_and_upserts_by_session_id() {
        let store = Store::new(temp_store_path("resume-roundtrip"));
        store
            .record_resume(&resume("s-1", "native-1"))
            .expect("record 1");
        store
            .record_resume(&resume("s-2", "native-2"))
            .expect("record 2");
        store
            .record_resume(&resume("s-1", "native-1-updated"))
            .expect("re-record 1");

        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded.len(), 2, "no duplicate session id: {loaded:?}");
        let s1 = loaded.iter().find(|b| b.session_id == "s-1").expect("s-1");
        assert_eq!(s1.native_session_id.as_deref(), Some("native-1-updated"));
    }

    #[test]
    fn resume_structural_snapshot_round_trips_verbatim() {
        // A path-kind host-profile binding with a full C.4 snapshot must survive
        // the JSON-lines round-trip byte-for-byte (PartialEq), including the seven
        // structural fields and the path-vs-id native reference.
        let store = Store::new(temp_store_path("resume-snapshot-roundtrip"));
        let binding = ResumeBinding {
            session_id: "s-path".to_owned(),
            name: Some("triage build".to_owned()),
            agent: "claude-sonnet".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace"),
            cols: 100,
            rows: 30,
            native_session_id: None,
            native_session_path: Some("/home/u/.claude/t.jsonl".to_owned()),
            project_id: Some("p-abc".to_owned()),
            is_linked_worktree: Some(true),
            metadata: BTreeMap::from([
                ("owner".to_owned(), "daemon".to_owned()),
                ("ticket".to_owned(), "DMD-1356".to_owned()),
            ]),
            program: "/opt/claude".to_owned(),
            args: vec!["--model".to_owned(), "sonnet".to_owned()],
            input_rules: StoredInputRules {
                bracketed_paste: true,
                submit_delay_ms: 42,
            },
            native_launch: Some(launch(
                SessionRefKind::Path,
                &["--session", "{reference}"],
                Some(&["--fork", "{reference}"]),
            )),
            launch_binding: LaunchPin::Unpinned,
        };
        store.record_resume(&binding).expect("record");
        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded, vec![binding], "the structural snapshot round-trips");
    }

    #[test]
    fn resume_binding_roundtrips_resume_only_and_fork_specs() {
        let store = Store::new(temp_store_path("resume-native-launch"));
        let mut resume_only = resume("s-resume-only", "native-1");
        resume_only.native_launch =
            Some(launch(SessionRefKind::Id, &["resume", "{reference}"], None));
        let with_fork = resume("s-fork", "native-2");
        store
            .record_resume(&resume_only)
            .expect("record resume-only");
        store.record_resume(&with_fork).expect("record fork");

        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded.len(), 2);
        for binding in &loaded {
            let expected = if binding.session_id == "s-fork" {
                &with_fork
            } else {
                &resume_only
            };
            assert_eq!(binding, expected);
        }
        assert!(resume_only.resumable() && !resume_only.forkable());
        assert!(with_fork.resumable() && with_fork.forkable());
        assert_eq!(with_fork.reference_kind(), Some(SessionRefKind::Id));
    }

    #[test]
    fn resume_line_with_an_invalid_launch_spec_is_dropped() {
        let store = Store::new(temp_store_path("resume-native-launch-corrupt"));
        let corrupt = concat!(
            r#"{"kind":"resume","session_id":"s-bad","agent":"claude","agent_base":"claude","#,
            r#""cwd":"/w","cols":80,"rows":24,"native_session_id":"n","#,
            r#""native_launch":{"reference_kind":"id","resume_args":[{"literal":"--resume"}]}}"#,
            "\n"
        );
        write_private(store.path(), corrupt);
        assert!(
            store.load_resume().expect("load").is_empty(),
            "a spec without a reference slot must not load as a binding"
        );
    }

    #[test]
    fn resume_line_with_removed_mode_fields_loads_as_non_recoverable() {
        let store = Store::new(temp_store_path("resume-removed-fields"));
        let old = concat!(
            r#"{"kind":"resume","session_id":"s-old","agent":"claude","agent_base":"claude","#,
            r#""cwd":"/w","cols":80,"rows":24,"native_session_id":"n","program":"claude","#,
            r#""resume_mode":"flag","ref_kind":"id","resumable":true}"#,
            "\n"
        );
        write_private(store.path(), old);
        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].native_launch, None);
    }

    #[test]
    fn stored_hermes_framing_reapplies_compiled_input_safety() {
        let stored = StoredInputRules {
            bracketed_paste: false,
            submit_delay_ms: 25,
        };
        let rules = stored.to_input_rules(
            crate::agent::host::RuntimeHost::default()
                .resolve_kind(&AgentKind::Hermes)
                .expect("Hermes resolves")
                .input_rules(),
        );

        assert!(!rules.bracketed_paste);
        assert_eq!(rules.submit_delay, std::time::Duration::from_millis(25));
        assert_eq!(
            rules
                .validate_text("unsafe\u{1b}[201~")
                .expect_err("restored Hermes safety rejects terminal controls")
                .code,
            "session_input_rejected"
        );
        assert_eq!(
            rules
                .validate_activity(Some(AgentActivity::Blocked))
                .expect_err("restored Hermes safety rejects approval input")
                .code,
            "session_input_blocked"
        );
    }

    #[test]
    fn resume_legacy_line_loads_with_default_snapshot() {
        // A resume line written before the C.4 snapshot existed (no program/args/
        // input_rules/resume/fork fields) still loads for inspection. This is not
        // a compatibility promise: absent fork fields default fail-closed rather
        // than inferring current compiled provider behavior.
        let store = Store::new(temp_store_path("resume-legacy"));
        let legacy = concat!(
            r#"{"kind":"resume","session_id":"s-old","agent":"claude","agent_base":"claude","#,
            r#""cwd":"/w","cols":80,"rows":24,"native_session_id":"native-old"}"#,
            "\n"
        );
        write_private(store.path(), legacy);
        let loaded = store.load_resume().expect("load legacy");
        assert_eq!(loaded.len(), 1);
        let b = &loaded[0];
        assert_eq!(b.session_id, "s-old");
        assert_eq!(b.native_session_id.as_deref(), Some("native-old"));
        assert_eq!(b.program, "");
        assert!(b.args.is_empty());
        assert_eq!(b.input_rules, StoredInputRules::default());
        assert_eq!(b.native_launch, None);
        assert!(!b.resumable());
        assert!(!b.forkable());
        assert_eq!(b.reference_kind(), None);
        assert!(b.metadata.is_empty());
    }

    #[test]
    fn resume_legacy_line_without_agent_base_infers_base_kind_from_agent_name() {
        let store = Store::new(temp_store_path("resume-legacy-agent-base"));
        let legacy = concat!(
            r#"{"kind":"resume","session_id":"s-codex","agent":"codex","#,
            r#""cwd":"/w","cols":80,"rows":24,"native_session_id":"native-codex"}"#,
            "\n",
            r#"{"kind":"resume","session_id":"s-claude","agent":"claude","#,
            r#""cwd":"/w","cols":100,"rows":30,"native_session_id":"native-claude"}"#,
            "\n"
        );
        write_private(store.path(), legacy);

        let loaded = store.load_resume().expect("load legacy lines");

        assert_eq!(loaded.len(), 2);
        let codex = loaded
            .iter()
            .find(|binding| binding.session_id == "s-codex")
            .expect("codex legacy binding");
        let claude = loaded
            .iter()
            .find(|binding| binding.session_id == "s-claude")
            .expect("claude legacy binding");
        assert_eq!(codex.agent_base, AgentKind::Codex);
        assert_eq!(claude.agent_base, AgentKind::Claude);
    }

    #[test]
    fn kinds_outside_the_runtime_id_grammar_are_neither_loaded_nor_persisted() {
        let path = temp_store_path("invalid-agent-kind");
        let store = Store::new(path.clone());
        let mut binding = resume("s-future", "native-future");
        binding.agent_base = AgentKind::Unknown("Future Agent".to_owned());

        let error = store
            .record_resume(&binding)
            .expect_err("a non-runtime-id kind must fail closed on write");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!path.exists(), "rejected record must not create the store");

        write_private(
            &path,
            concat!(
                r#"{"kind":"resume","session_id":"s-future","agent":"future-agent","#,
                r#""agent_base":"Future Agent","cwd":"/w","cols":80,"rows":24}"#,
                "\n"
            ),
        );
        assert!(
            store.load_resume().expect("load future record").is_empty(),
            "a persisted record whose kind is not a runtime id must be ignored"
        );
    }

    #[test]
    fn an_uninstalled_runtime_keeps_its_binding_across_unrelated_writes() {
        let path = temp_store_path("inert-binding");
        let store = Store::new(path);
        let mut inert = resume("s-inert", "native-inert");
        inert.agent_base = AgentKind::Unknown("acme".to_owned());
        inert.launch_binding = LaunchPin::Pinned(Box::new(package_binding("acme")));
        store.record_resume(&inert).expect("record inert binding");

        // Every mutation rewrites the whole file, so an unrelated write is the
        // moment a binding that failed to resolve would be erased.
        store
            .record_resume(&resume("s-other", "native-other"))
            .expect("record unrelated resume binding");
        store
            .record_worktree(&worktree("s-other", "x"))
            .expect("record unrelated worktree binding");
        store.remove_resume("s-other").expect("remove unrelated");

        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded, vec![inert], "the inert binding survives untouched");
    }

    #[test]
    fn a_line_without_a_launch_binding_loads_unpinned_and_a_pin_round_trips() {
        let path = temp_store_path("launch-pin");
        let store = Store::new(path.clone());
        write_private(
            &path,
            concat!(
                r#"{"kind":"resume","session_id":"s-old","agent":"claude","agent_base":"claude","#,
                r#""cwd":"/w","cols":80,"rows":24}"#,
                "\n"
            ),
        );
        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded[0].launch_binding, LaunchPin::Unpinned);

        let mut pinned = resume("s-new", "native-new");
        pinned.launch_binding = LaunchPin::Pinned(Box::new(package_binding("claude")));
        store.record_resume(&pinned).expect("record pinned");
        let loaded = store.load_resume().expect("reload");
        let found = loaded
            .iter()
            .find(|binding| binding.session_id == "s-new")
            .expect("pinned binding present");
        assert_eq!(found.launch_binding, pinned.launch_binding);
        assert!(
            loaded
                .iter()
                .find(|binding| binding.session_id == "s-old")
                .expect("old line present")
                .launch_binding
                .is_unpinned(),
            "rewriting does not invent a pin for a line that had none"
        );
    }

    #[test]
    fn worktree_round_trips_find_and_remove() {
        let store = Store::new(temp_store_path("worktree-roundtrip"));
        let a = worktree("s-1", "x");
        store.record_worktree(&a).expect("record a");

        let found = store
            .find_worktree("s-1", &a.repository, "x")
            .expect("find")
            .expect("present");
        assert_eq!(found, a);

        // A second branch of the same (session, repo) coexists, not overwrite.
        let b = worktree("s-1", "y");
        store.record_worktree(&b).expect("record b");
        assert_eq!(store.load_worktrees().expect("load").len(), 2);

        // Find-for-session returns one of the session's active bindings.
        let for_session = store
            .find_worktree_for_session("s-1")
            .expect("find for session")
            .expect("present");
        assert_eq!(for_session.session_id, "s-1");

        let removed = store
            .remove_worktree_session("s-1")
            .expect("remove")
            .into_value();
        assert_eq!(removed, 2);
        assert!(store.load_worktrees().expect("load").is_empty());
    }

    #[test]
    fn worktree_binding_removal_drops_only_the_keyed_binding() {
        let store = Store::new(temp_store_path("worktree-remove-one"));
        let kept_branch = worktree("s-1", "x");
        let removed_branch = worktree("s-1", "y");
        let other_session = worktree("s-2", "y");
        for binding in [&kept_branch, &removed_branch, &other_session] {
            store.record_worktree(binding).expect("record binding");
        }

        let removed = store
            .remove_worktree_binding(&removed_branch)
            .expect("remove one binding")
            .into_value();
        let again = store
            .remove_worktree_binding(&removed_branch)
            .expect("repeat removal")
            .into_value();

        assert!(removed, "the keyed binding is removed");
        assert!(!again, "a repeated removal is a no-op success");
        assert_eq!(
            store.load_worktrees().expect("load"),
            [kept_branch, other_session],
            "the session's other branch and other sessions keep their bindings"
        );
    }

    #[test]
    fn worktree_write_preserves_the_committed_uncertain_outcome() {
        let store = Store::new(temp_store_path("worktree-committed-uncertain"));
        let binding = worktree("s-1", "x");
        store.fail_next_parent_sync_after_rename();

        assert!(matches!(
            store.record_worktree(&binding).expect("committed write"),
            StoreMutation::CommittedDurabilityUncertain { value: (), .. }
        ));
        assert_eq!(
            store.load_worktrees().expect("load committed binding"),
            vec![binding]
        );
    }

    #[test]
    fn the_two_record_kinds_coexist_and_updates_preserve_the_other() {
        // The core M9 consistency guarantee: writing one kind never drops the
        // other; the two records for a session live in one file written atomically.
        let store = Store::new(temp_store_path("coexist"));
        store
            .record_resume(&resume("s-1", "native-1"))
            .expect("resume");
        store
            .record_worktree(&worktree("s-1", "x"))
            .expect("worktree");

        assert_eq!(store.load_resume().expect("resume").len(), 1);
        assert_eq!(store.load_worktrees().expect("worktree").len(), 1);

        // Updating the resume record must keep the worktree record.
        store
            .record_resume(&resume("s-1", "native-1-updated"))
            .expect("update resume");
        assert_eq!(
            store.load_worktrees().expect("worktree").len(),
            1,
            "updating a resume record must not drop the worktree record"
        );

        // Updating the worktree record must keep the resume record.
        let mut wt = worktree("s-1", "x");
        wt.base_branch = "develop".to_owned();
        store.record_worktree(&wt).expect("update worktree");
        let resume_after = store.load_resume().expect("resume");
        assert_eq!(
            resume_after.len(),
            1,
            "updating a worktree record must not drop the resume record"
        );
        assert_eq!(
            resume_after[0].native_session_id.as_deref(),
            Some("native-1-updated")
        );

        // Removing one kind for the session leaves the other untouched.
        store.remove_resume("s-1").expect("remove resume");
        assert!(store.load_resume().expect("resume").is_empty());
        assert_eq!(
            store.load_worktrees().expect("worktree").len(),
            1,
            "removing the resume record must not drop the worktree record"
        );
    }

    #[test]
    fn remove_resume_missing_is_noop() {
        let store = Store::new(temp_store_path("remove-missing"));
        store
            .record_worktree(&worktree("s-1", "x"))
            .expect("worktree");
        store.remove_resume("s-unknown").expect("remove missing");
        assert_eq!(store.load_worktrees().expect("worktree").len(), 1);
    }

    /// Every write renames a new file over the store while another handle on
    /// the same path polls it, as a second process or a test observer does.
    /// A load must return one whole version, never fail on the replaced file.
    #[test]
    fn loads_survive_a_concurrent_writer_on_another_handle() {
        // Enough rewrites that the reader lands inside the window between
        // inspecting the old file and the rename replacing it.
        const REWRITES: usize = 400;
        let path = temp_store_path("concurrent-read");
        let writer = Store::new(path.clone());
        let reader = Store::new(path);
        writer
            .record_resume(&resume("s-1", "native-1"))
            .expect("seed resume");
        std::thread::scope(|scope| {
            let writing = scope.spawn(|| {
                for round in 0..REWRITES {
                    writer
                        .record_resume(&resume("s-1", &format!("native-{round}")))
                        .expect("rewrite resume");
                }
            });
            while !writing.is_finished() {
                let bindings = reader
                    .load_resume()
                    .expect("a load racing a rewrite must still succeed");
                assert_eq!(bindings.len(), 1, "each load sees one whole version");
            }
            writing.join().expect("writer thread completes");
        });
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path("perms");
        let store = Store::new(path.clone());
        store
            .record_resume(&resume("s-1", "native-1"))
            .expect("record");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "store file must be owner-private");
    }

    #[cfg(unix)]
    #[test]
    fn owner_private_replace_options_create_files_0600() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path("replace-options");
        let mut file = super::owner_private_replace_options()
            .open(&path)
            .expect("open owner-private file");
        file.write_all(b"data").expect("write owner-private file");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(
            mode & 0o777,
            super::OWNER_PRIVATE_FILE_MODE,
            "owner-private store files must be created with the final mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_owner_private_tightens_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path("tighten-existing");
        fs::write(&path, b"old").expect("write loose file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("set loose mode");

        super::write_owner_private(&path, b"new").expect("rewrite private file");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, super::OWNER_PRIVATE_FILE_MODE);
        assert_eq!(fs::read(&path).expect("read"), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn store_rejects_symlink_target_without_touching_referent() {
        use std::os::unix::fs::symlink;

        let path = temp_store_path("symlink-store");
        let referent = path.with_file_name("referent");
        fs::write(&referent, b"keep").expect("write referent");
        symlink(&referent, &path).expect("create store symlink");
        let store = Store::new(path);

        let error = store
            .record_resume(&resume("s-1", "native-1"))
            .expect_err("store symlink must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(referent).expect("read referent"), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn store_ignores_stale_symlink_temporary_file_without_touching_referent() {
        use std::os::unix::fs::symlink;

        let path = temp_store_path("symlink-temp");
        let store = Store::new(path.clone());
        let referent = path.with_file_name("temp-referent");
        fs::write(&referent, b"keep").expect("write referent");
        symlink(&referent, store.temp_path()).expect("create temp symlink");

        store
            .record_resume(&resume("s-1", "native-1"))
            .expect("fresh unique temporary name avoids stale symlink");

        assert_eq!(fs::read(referent).expect("read referent"), b"keep");
        assert!(path.exists(), "safe write creates the store");
    }

    // --- projects (milestone: projects M2) -----------------------------------

    #[test]
    fn project_round_trips_and_upserts_by_common_dir() {
        let store = Store::new(temp_store_path("project-roundtrip"));
        store
            .record_project(&project("/code/ui/.git", "/code/ui", None))
            .expect("record ui");
        store
            .record_project(&project("/code/api/.git", "/code/api", None))
            .expect("record api");

        // Re-recording the same git_common_dir updates in place, never appends.
        let mut updated = project("/code/ui/.git", "/code/ui", Some("dashboard"));
        updated.source = ProjectSource::Manual;
        updated.last_used_at = "2026-06-20T00:00:00Z".to_owned();
        store.record_project(&updated).expect("re-record ui");

        let loaded = store.load_projects().expect("load");
        assert_eq!(loaded.len(), 2, "common-dir is the key: no duplicate");
        let ui = loaded
            .iter()
            .find(|p| p.git_common_dir == Path::new("/code/ui/.git"))
            .expect("ui present");
        assert_eq!(ui.custom_name.as_deref(), Some("dashboard"));
        assert_eq!(ui.source, ProjectSource::Manual);
        assert_eq!(ui.last_used_at, "2026-06-20T00:00:00Z");
    }

    #[test]
    fn mutate_project_creates_when_absent_and_preserves_other_kinds() {
        let store = Store::new(temp_store_path("mutate-create"));
        store.record_resume(&resume("s-1", "native-1")).expect("r");
        store.record_worktree(&worktree("s-1", "x")).expect("w");

        // Absent project: the closure receives `None` and a create-if-missing
        // policy returns `Some`, so the record is inserted.
        let created = store
            .mutate_project(Path::new("/code/ui/.git"), |existing| {
                assert!(existing.is_none(), "project is absent");
                Some(project("/code/ui/.git", "/code/ui", None))
            })
            .expect("mutate")
            .into_value()
            .expect("a record was written");
        assert_eq!(created.git_common_dir, PathBuf::from("/code/ui/.git"));
        assert_eq!(store.load_projects().expect("p").len(), 1);
        // The atomic rewrite preserves the other record kinds.
        assert_eq!(store.load_resume().expect("r").len(), 1, "resume kept");
        assert_eq!(store.load_worktrees().expect("w").len(), 1, "worktree kept");
    }

    #[test]
    fn mutate_project_passes_current_record_and_merges_edits() {
        // This is the fix for the metadata-clobber race: each edit reads the
        // freshest record *under the write lock* and mutates only its own field,
        // so a later edit cannot revert an earlier one from a stale snapshot.
        let store = Store::new(temp_store_path("mutate-merge"));
        store
            .record_project(&project("/code/ui/.git", "/code/ui", None))
            .expect("seed");

        // Edit A: set the default base branch (as `project add --base-branch`).
        store
            .mutate_project(Path::new("/code/ui/.git"), |existing| {
                let mut record = existing.expect("present");
                record.default_base_branch = Some("develop".to_owned());
                Some(record)
            })
            .expect("edit A");

        // Edit B: set the custom name (as `project rename`). Because the closure
        // reads the freshest record, it observes edit A's base branch and keeps it.
        let after = store
            .mutate_project(Path::new("/code/ui/.git"), |existing| {
                let mut record = existing.expect("present");
                record.custom_name = Some("dashboard".to_owned());
                Some(record)
            })
            .expect("edit B")
            .into_value()
            .expect("written");

        assert_eq!(after.custom_name.as_deref(), Some("dashboard"));
        assert_eq!(
            after.default_base_branch.as_deref(),
            Some("develop"),
            "edit B must not revert edit A's field"
        );
        assert_eq!(store.load_projects().expect("p").len(), 1, "no duplicate");
    }

    #[test]
    fn mutate_project_declining_writes_nothing() {
        let store = Store::new(temp_store_path("mutate-decline"));
        store
            .record_project(&project("/code/ui/.git", "/code/ui", Some("dash")))
            .expect("seed");

        // An update-only policy on a present record may still decline (return
        // `None`); nothing is written and the result is `None`.
        let result = store
            .mutate_project(Path::new("/code/ui/.git"), |_existing| None)
            .expect("mutate present")
            .into_value();
        assert!(result.is_none(), "declined ⇒ no record returned");
        let loaded = store.load_projects().expect("p");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].custom_name.as_deref(), Some("dash"), "untouched");

        // Update-only on an *absent* project writes nothing and returns `None`
        // (the `touch`/`rename`-on-a-removed-project path).
        let missing = store
            .mutate_project(Path::new("/code/gone/.git"), |existing| {
                existing.map(|mut record| {
                    record.custom_name = Some("never".to_owned());
                    record
                })
            })
            .expect("mutate absent")
            .into_value();
        assert!(missing.is_none(), "absent + update-only ⇒ None");
        assert_eq!(store.load_projects().expect("p").len(), 1, "nothing added");
    }

    #[test]
    fn project_id_and_label_are_derived() {
        let auto = project("/code/ui/.git", "/code/ui", None);
        // Label falls back to the repo-root basename when no custom name is set.
        assert_eq!(auto.label(), "ui");
        // A custom name overrides the derived label.
        let named = project("/code/ui/.git", "/code/ui", Some("dashboard"));
        assert_eq!(named.label(), "dashboard");
        // The id is derived from the key and stable.
        assert!(auto.id().starts_with("p-"));
        assert_eq!(auto.id(), named.id(), "same key ⇒ same id, label aside");
    }

    #[test]
    fn the_three_record_kinds_coexist_and_writes_preserve_the_others() {
        // The M2 extension of the M9 invariant: writing/removing any one kind
        // never drops the others; all three live in one atomically-rewritten file.
        let store = Store::new(temp_store_path("three-kinds"));
        store
            .record_project(&project("/code/ui/.git", "/code/ui", None))
            .expect("project");
        store
            .record_resume(&resume("s-1", "native-1"))
            .expect("resume");
        store
            .record_worktree(&worktree("s-1", "x"))
            .expect("worktree");

        assert_eq!(store.load_projects().expect("p").len(), 1);
        assert_eq!(store.load_resume().expect("r").len(), 1);
        assert_eq!(store.load_worktrees().expect("w").len(), 1);

        // Updating the project must keep the resume and worktree records.
        store
            .record_project(&project("/code/ui/.git", "/code/ui", Some("dash")))
            .expect("update project");
        assert_eq!(store.load_resume().expect("r").len(), 1, "resume kept");
        assert_eq!(store.load_worktrees().expect("w").len(), 1, "worktree kept");

        // Removing the resume record keeps the project and worktree.
        store.remove_resume("s-1").expect("remove resume");
        assert!(store.load_resume().expect("r").is_empty());
        assert_eq!(store.load_projects().expect("p").len(), 1, "project kept");
        assert_eq!(store.load_worktrees().expect("w").len(), 1, "worktree kept");
    }

    #[test]
    fn worktree_project_id_round_trips_and_a_legacy_line_loads() {
        let store = Store::new(temp_store_path("wt-project-id"));
        let mut bound = worktree("s-1", "x");
        bound.project_id = Some("p-deadbeef".to_owned());
        store.record_worktree(&bound).expect("record");
        let loaded = store.load_worktrees().expect("load");
        assert_eq!(loaded[0].project_id.as_deref(), Some("p-deadbeef"));

        // A line written before the project_id field existed (the field absent)
        // still loads, defaulting project_id to None — the store's only
        // compatibility concession (serde default), not a guarantee.
        let legacy = concat!(
            r#"{"kind":"worktree","session_id":"s-2","repository":"/r","branch":"feat/y","#,
            r#""base_branch":"main","branch_slug":"feat-y","path":"/p","status":"active","#,
            r#""created_at":"2026-06-19T00:00:00Z","updated_at":"2026-06-19T00:00:00Z"}"#,
            "\n"
        );
        write_private(store.path(), legacy);
        let loaded = store.load_worktrees().expect("load legacy");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session_id, "s-2");
        assert_eq!(loaded[0].project_id, None, "absent field defaults to None");
    }

    #[test]
    fn find_project_resolves_by_id_then_label_and_reports_ambiguity() {
        let store = Store::new(temp_store_path("find-project"));
        // Two distinct repos that happen to share the basename label "ui".
        store
            .record_project(&project("/a/ui/.git", "/a/ui", None))
            .expect("a/ui");
        store
            .record_project(&project("/b/ui/.git", "/b/ui", None))
            .expect("b/ui");
        store
            .record_project(&project("/c/api/.git", "/c/api", None))
            .expect("c/api");

        // Exact id match wins and is unambiguous even though "ui" is shared.
        let a_ui_id = project("/a/ui/.git", "/a/ui", None).id();
        match store.find_project(&a_ui_id).expect("by id") {
            ProjectResolution::Found(found) => {
                assert_eq!(found.git_common_dir, PathBuf::from("/a/ui/.git"));
            }
            other => panic!("expected Found by id, got {other:?}"),
        }

        // A unique label resolves.
        match store.find_project("api").expect("by label") {
            ProjectResolution::Found(found) => assert_eq!(found.repo_root, PathBuf::from("/c/api")),
            other => panic!("expected Found by label, got {other:?}"),
        }

        // A shared label is ambiguous and returns ALL candidates for the CLI to
        // print with their ids.
        match store.find_project("ui").expect("ambiguous") {
            ProjectResolution::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                let keys: Vec<&PathBuf> = candidates.iter().map(|c| &c.git_common_dir).collect();
                assert!(keys.contains(&&PathBuf::from("/a/ui/.git")));
                assert!(keys.contains(&&PathBuf::from("/b/ui/.git")));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        // An unknown reference is NotFound.
        assert_eq!(
            store.find_project("nope").expect("not found"),
            ProjectResolution::NotFound
        );
    }

    #[test]
    fn remove_project_forgets_only_the_record() {
        let store = Store::new(temp_store_path("remove-project"));
        store
            .record_project(&project("/code/ui/.git", "/code/ui", None))
            .expect("project");
        store
            .record_worktree(&worktree("s-1", "x"))
            .expect("worktree");

        assert!(
            store
                .remove_project(&PathBuf::from("/code/ui/.git"))
                .expect("remove")
                .into_value(),
            "removed an existing project"
        );
        assert!(store.load_projects().expect("p").is_empty());
        assert_eq!(
            store.load_worktrees().expect("w").len(),
            1,
            "removing a project never touches worktree records"
        );
        assert!(
            !store
                .remove_project(&PathBuf::from("/code/ui/.git"))
                .expect("remove missing")
                .into_value(),
            "removing an absent project is a no-op false"
        );
    }

    #[test]
    fn a_corrupt_line_is_skipped_preserving_every_valid_record_kind() {
        let store = Store::new(temp_store_path("corrupt-line"));
        // A valid project, a garbage line, and a valid resume — interleaved.
        let body = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&super::Record::Project(project(
                "/code/ui/.git",
                "/code/ui",
                None
            )))
            .expect("project json"),
            "{not valid json at all",
            serde_json::to_string(&super::Record::Resume(resume("s-1", "native-1")))
                .expect("resume json"),
        );
        write_private(store.path(), body);

        assert_eq!(
            store.load_projects().expect("p").len(),
            1,
            "the corrupt line must not block loading the project"
        );
        assert_eq!(
            store.load_resume().expect("r").len(),
            1,
            "the corrupt line must not block loading the resume binding"
        );
    }

    #[test]
    fn durable_atomic_write_resolves_parent_for_absolute_and_relative_store_paths() {
        assert_eq!(
            super::parent_directory(Path::new("/var/lib/pohunek/metadata.jsonl")),
            Path::new("/var/lib/pohunek")
        );
        assert_eq!(
            super::parent_directory(Path::new("metadata.jsonl")),
            Path::new(".")
        );
    }
}
