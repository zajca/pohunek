//! Unified metadata store (milestone 9).
//!
//! One owner-private JSON-lines file holds the record kinds the daemon must
//! survive a restart with: the resume bindings ([`ResumeBinding`]), the worktree
//! bindings ([`WorktreeBinding`]), and the project records ([`ProjectRecord`]).
//! They share a single file, a single serialization lock, and a single atomic
//! write-path: every mutation rewrites the whole file via one temp file +
//! `rename`, so a write of one record kind can never corrupt or drop a record of
//! another kind, and any single update is crash-atomic (one `rename(2)` commits
//! it). This is the transactional consistency a SQLite store would have given,
//! without the dependency (see `NEXT.md` milestone 9).
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
//! project) so a full rewrite per mutation is cheap. No secrets are ever written:
//! a native session id, a cwd, a repository path, a branch, a worktree path, and
//! a project's git common dir / credential-redacted origin URL are not secrets.
//!
//! The resume binding additionally carries the **structural relaunch snapshot**
//! (Part C, C.4): `program`, `args`, `input_rules`, `resume_mode`, `ref_kind`,
//! `resumable`, and `agent_base`. These are the seven non-secret fields needed to
//! relaunch-and-resume a host-profile session with exactly its launch-time shape
//! after a daemon restart. The profile's **`env` is deliberately NOT among them** —
//! it may hold secrets, so it is re-resolved by agent name at resume, never
//! persisted (a deleted profile resumes from the structural snapshot with no env).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use protocol::{AgentKind, ProjectSource};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::agent::{InputRules, ResumeMode, SessionRefKind};
use crate::project::detect::project_id;

/// One session's resume binding: everything needed to relaunch-and-resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResumeBinding {
    /// The pohunek session id (stable across restart).
    pub session_id: String,
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
    /// Captured native session id used to build the resume argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Captured native session path, for agents that resume from a path.
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
    /// Structural relaunch snapshot (C.4): the resume argv mode, frozen at creation
    /// so a profile's `[resume] mode` override drives the relaunch argv. `None` for
    /// a non-resumable session or a legacy line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_mode: Option<ResumeMode>,
    /// Structural relaunch snapshot (C.4): the native-reference kind, frozen at
    /// creation. Decides whether the captured reference resumes via the id (dash)
    /// guard or the path (absolute) guard. `None` for non-resumable / legacy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_kind: Option<SessionRefKind>,
    /// Structural relaunch snapshot (C.4): whether this session resumes at all,
    /// frozen at creation. Serde default (`false`) for a legacy line.
    #[serde(default)]
    pub resumable: bool,
}

impl<'de> Deserialize<'de> for ResumeBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawResumeBinding {
            session_id: String,
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
            program: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            input_rules: StoredInputRules,
            #[serde(default)]
            resume_mode: Option<ResumeMode>,
            #[serde(default)]
            ref_kind: Option<SessionRefKind>,
            #[serde(default)]
            resumable: bool,
        }

        let raw = RawResumeBinding::deserialize(deserializer)?;
        let agent_base = raw
            .agent_base
            .or_else(|| legacy_agent_base_from_agent(&raw.agent))
            .ok_or_else(|| serde::de::Error::missing_field("agent_base"))?;

        Ok(Self {
            session_id: raw.session_id,
            agent: raw.agent,
            agent_base,
            cwd: raw.cwd,
            cols: raw.cols,
            rows: raw.rows,
            native_session_id: raw.native_session_id,
            native_session_path: raw.native_session_path,
            project_id: raw.project_id,
            is_linked_worktree: raw.is_linked_worktree,
            program: raw.program,
            args: raw.args,
            input_rules: raw.input_rules,
            resume_mode: raw.resume_mode,
            ref_kind: raw.ref_kind,
            resumable: raw.resumable,
        })
    }
}

fn legacy_agent_base_from_agent(agent: &str) -> Option<AgentKind> {
    match agent {
        "shell" => Some(AgentKind::Shell),
        "codex" => Some(AgentKind::Codex),
        "claude" => Some(AgentKind::Claude),
        _ => None,
    }
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
    /// Rebuild the in-memory [`InputRules`] from the persisted snapshot.
    #[must_use]
    pub fn to_input_rules(self) -> InputRules {
        InputRules {
            bracketed_paste: self.bracketed_paste,
            submit_delay: Duration::from_millis(self.submit_delay_ms),
        }
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

/// A single line of the unified store, internally tagged by `kind` so both
/// record kinds coexist in one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Resume(ResumeBinding),
    Worktree(WorktreeBinding),
    Project(ProjectRecord),
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
}

impl Store {
    /// Open a store at `path`. The file is created lazily on the first write.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
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

    /// Upsert a resume binding (keyed by `session_id`), preserving every worktree
    /// record, and rewrite the file atomically.
    pub fn record_resume(&self, binding: &ResumeBinding) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut resume, worktrees, projects) = self.read_all()?;
        if let Some(existing) = resume
            .iter_mut()
            .find(|existing| existing.session_id == binding.session_id)
        {
            *existing = binding.clone();
        } else {
            resume.push(binding.clone());
        }
        self.write_all(&resume, &worktrees, &projects)
    }

    /// Remove a resume binding by session id, preserving every worktree record. A
    /// missing entry is a no-op.
    pub fn remove_resume(&self, session_id: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut resume, worktrees, projects) = self.read_all()?;
        let before = resume.len();
        resume.retain(|binding| binding.session_id != session_id);
        if resume.len() == before {
            return Ok(());
        }
        self.write_all(&resume, &worktrees, &projects)
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
    pub fn record_worktree(&self, binding: &WorktreeBinding) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, mut worktrees, projects) = self.read_all()?;
        if let Some(existing) = worktrees.iter_mut().find(|existing| {
            existing.session_id == binding.session_id
                && existing.repository == binding.repository
                && existing.branch_slug == binding.branch_slug
        }) {
            *existing = binding.clone();
        } else {
            worktrees.push(binding.clone());
        }
        self.write_all(&resume, &worktrees, &projects)
    }

    /// Remove every worktree binding owned by `session_id`, preserving every
    /// resume record. Returns the number removed (`0` is a no-op success).
    pub fn remove_worktree_session(&self, session_id: &str) -> io::Result<usize> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, mut worktrees, projects) = self.read_all()?;
        let before = worktrees.len();
        worktrees.retain(|binding| binding.session_id != session_id);
        let removed = before - worktrees.len();
        if removed > 0 {
            self.write_all(&resume, &worktrees, &projects)?;
        }
        Ok(removed)
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
    ) -> io::Result<Option<ProjectRecord>>
    where
        F: FnOnce(Option<ProjectRecord>) -> Option<ProjectRecord>,
    {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, worktrees, mut projects) = self.read_all()?;
        let pos = projects
            .iter()
            .position(|existing| existing.git_common_dir == git_common_dir);
        let current = pos.map(|index| projects[index].clone());
        let Some(updated) = mutate(current) else {
            return Ok(None);
        };
        match pos {
            Some(index) => projects[index] = updated.clone(),
            None => projects.push(updated.clone()),
        }
        self.write_all(&resume, &worktrees, &projects)?;
        Ok(Some(updated))
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
    pub fn record_project(&self, record: &ProjectRecord) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, worktrees, mut projects) = self.read_all()?;
        if let Some(existing) = projects
            .iter_mut()
            .find(|existing| existing.git_common_dir == record.git_common_dir)
        {
            *existing = record.clone();
        } else {
            projects.push(record.clone());
        }
        self.write_all(&resume, &worktrees, &projects)
    }

    /// Remove the project keyed by `git_common_dir`, preserving every resume and
    /// worktree record. Returns whether a record was removed (`false` is a no-op
    /// success). Only forgets the record; it never touches the on-disk repository
    /// or its worktrees.
    pub fn remove_project(&self, git_common_dir: &Path) -> io::Result<bool> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, worktrees, mut projects) = self.read_all()?;
        let before = projects.len();
        projects.retain(|project| project.git_common_dir != git_common_dir);
        let removed = projects.len() != before;
        if removed {
            self.write_all(&resume, &worktrees, &projects)?;
        }
        Ok(removed)
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
    fn read_all(
        &self,
    ) -> io::Result<(Vec<ResumeBinding>, Vec<WorktreeBinding>, Vec<ProjectRecord>)> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new(), Vec::new()))
            }
            Err(err) => return Err(err),
        };
        let mut resume = Vec::new();
        let mut worktrees = Vec::new();
        let mut projects = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Resume(binding)) => resume.push(binding),
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
        Ok((resume, worktrees, projects))
    }

    /// Serialize all records (resume, then worktree, then project) to a temp file
    /// and rename it over the store path. One `rename(2)` commits all kinds.
    fn write_all(
        &self,
        resume: &[ResumeBinding],
        worktrees: &[WorktreeBinding],
        projects: &[ProjectRecord],
    ) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
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

        let tmp = self.temp_path();
        write_owner_private(&tmp, body.as_bytes())?;
        fs::rename(&tmp, &self.path)
    }

    fn temp_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| "metadata.jsonl".into());
        name.push(format!(".tmp.{}", std::process::id()));
        match self.path.parent() {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        }
    }
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
fn write_owner_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::{AgentKind, ProjectSource};

    use super::{
        ProjectRecord, ProjectResolution, ResumeBinding, ResumeMode, SessionRefKind, Store,
        StoredInputRules, WorktreeBinding, WorktreeStatus,
    };

    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pohunek-store-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("metadata.jsonl")
    }

    fn resume(session_id: &str, native: &str) -> ResumeBinding {
        ResumeBinding {
            session_id: session_id.to_owned(),
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace/project"),
            cols: 120,
            rows: 40,
            native_session_id: Some(native.to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            program: "claude".to_owned(),
            args: Vec::new(),
            input_rules: StoredInputRules {
                bracketed_paste: false,
                submit_delay_ms: 150,
            },
            resume_mode: Some(ResumeMode::Flag),
            ref_kind: Some(SessionRefKind::Id),
            resumable: true,
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
            agent: "claude-sonnet".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: PathBuf::from("/workspace"),
            cols: 100,
            rows: 30,
            native_session_id: None,
            native_session_path: Some("/home/u/.claude/t.jsonl".to_owned()),
            project_id: Some("p-abc".to_owned()),
            is_linked_worktree: Some(true),
            program: "/opt/claude".to_owned(),
            args: vec!["--model".to_owned(), "sonnet".to_owned()],
            input_rules: StoredInputRules {
                bracketed_paste: true,
                submit_delay_ms: 42,
            },
            resume_mode: Some(ResumeMode::Subcommand),
            ref_kind: Some(SessionRefKind::Path),
            resumable: true,
        };
        store.record_resume(&binding).expect("record");
        let loaded = store.load_resume().expect("load");
        assert_eq!(loaded, vec![binding], "the structural snapshot round-trips");
    }

    #[test]
    fn resume_legacy_line_loads_with_default_snapshot() {
        // A resume line written before the C.4 snapshot existed (no program/args/
        // input_rules/resume_mode/ref_kind/resumable) still loads, defaulting the
        // snapshot — the store's only compatibility concession (serde default).
        let store = Store::new(temp_store_path("resume-legacy"));
        let legacy = concat!(
            r#"{"kind":"resume","session_id":"s-old","agent":"claude","agent_base":"claude","#,
            r#""cwd":"/w","cols":80,"rows":24,"native_session_id":"native-old"}"#,
            "\n"
        );
        fs::write(store.path(), legacy).expect("write legacy line");
        let loaded = store.load_resume().expect("load legacy");
        assert_eq!(loaded.len(), 1);
        let b = &loaded[0];
        assert_eq!(b.session_id, "s-old");
        assert_eq!(b.native_session_id.as_deref(), Some("native-old"));
        assert_eq!(b.program, "");
        assert!(b.args.is_empty());
        assert_eq!(b.input_rules, StoredInputRules::default());
        assert_eq!(b.resume_mode, None);
        assert_eq!(b.ref_kind, None);
        assert!(!b.resumable);
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
        fs::write(store.path(), legacy).expect("write legacy lines");

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

        let removed = store.remove_worktree_session("s-1").expect("remove");
        assert_eq!(removed, 2);
        assert!(store.load_worktrees().expect("load").is_empty());
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
            .expect("mutate present");
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
            .expect("mutate absent");
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
        fs::write(store.path(), legacy).expect("write legacy line");
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
                .expect("remove"),
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
                .expect("remove missing"),
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
        fs::write(store.path(), body).expect("write store");

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
}
