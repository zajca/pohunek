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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::{AgentKind, ProjectSource};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::project::detect::project_id;

/// One session's resume binding: everything needed to relaunch-and-resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeBinding {
    /// The pohunek session id (stable across restart).
    pub session_id: String,
    /// Agent kind backing the session.
    pub agent: AgentKind,
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

    /// Upsert a project record (keyed by canonical `git_common_dir`), preserving
    /// every resume and worktree record, and rewrite the file atomically.
    /// Re-detecting (or re-adding) the same repository updates the existing record
    /// in place — never duplicates — because the git common dir is the natural
    /// key. The caller supplies an already-canonical key (detection and
    /// `project add` both canonicalize), so matching is exact-path, mirroring how
    /// worktree records key on the canonicalized repository.
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
        ProjectRecord, ProjectResolution, ResumeBinding, Store, WorktreeBinding, WorktreeStatus,
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
            agent: AgentKind::Claude,
            cwd: PathBuf::from("/workspace/project"),
            cols: 120,
            rows: 40,
            native_session_id: Some(native.to_owned()),
            native_session_path: None,
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
