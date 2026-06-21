//! Unified metadata store (milestone 9).
//!
//! One owner-private JSON-lines file holds **both** record kinds the daemon must
//! survive a restart with: the M7 resume bindings ([`ResumeBinding`]) and the M8
//! worktree bindings ([`WorktreeBinding`]). They share a single file, a single
//! serialization lock, and a single atomic write-path: every mutation rewrites
//! the whole file via one temp file + `rename`, so a write of one record kind can
//! never corrupt or drop a record of the other kind, and any single update is
//! crash-atomic (one `rename(2)` commits it). This is the transactional
//! consistency a SQLite store would have given, without the dependency (see
//! `NEXT.md` milestone 9).
//!
//! This is a consistency guarantee about the *write path*, not a lifecycle
//! pairing: the two records are written by independent triggers (a worktree
//! binding at `session.new`, a resume binding when the agent later reports its
//! native id) and they have independent lifetimes — a stopped session's resume
//! binding is removed, but its worktree binding is intentionally kept (the
//! on-disk worktree holds the user's work; see [`crate::worktree`]).
//!
//! Each line is a tagged [`Record`] (`{"kind":"resume", ...}` /
//! `{"kind":"worktree", ...}`). Every mutation re-reads the whole file under the
//! write lock, edits the relevant record kind, and rewrites **all** records,
//! preserving the other kind untouched. The file is small (one line per resumable
//! session and per bound worktree) so a full rewrite per mutation is cheap. No
//! secrets are ever written: a native session id, a cwd, a repository path, a
//! branch, and a worktree path are not secrets.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::AgentKind;
use serde::{Deserialize, Serialize};
use tracing::warn;

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
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last-update timestamp (RFC3339).
    pub updated_at: String,
}

/// A single line of the unified store, internally tagged by `kind` so both
/// record kinds coexist in one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Resume(ResumeBinding),
    Worktree(WorktreeBinding),
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

    /// Upsert a resume binding (keyed by `session_id`), preserving every worktree
    /// record, and rewrite the file atomically.
    pub fn record_resume(&self, binding: &ResumeBinding) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut resume, worktrees) = self.read_all()?;
        if let Some(existing) = resume
            .iter_mut()
            .find(|existing| existing.session_id == binding.session_id)
        {
            *existing = binding.clone();
        } else {
            resume.push(binding.clone());
        }
        self.write_all(&resume, &worktrees)
    }

    /// Remove a resume binding by session id, preserving every worktree record. A
    /// missing entry is a no-op.
    pub fn remove_resume(&self, session_id: &str) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut resume, worktrees) = self.read_all()?;
        let before = resume.len();
        resume.retain(|binding| binding.session_id != session_id);
        if resume.len() == before {
            return Ok(());
        }
        self.write_all(&resume, &worktrees)
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
        let (resume, mut worktrees) = self.read_all()?;
        if let Some(existing) = worktrees.iter_mut().find(|existing| {
            existing.session_id == binding.session_id
                && existing.repository == binding.repository
                && existing.branch_slug == binding.branch_slug
        }) {
            *existing = binding.clone();
        } else {
            worktrees.push(binding.clone());
        }
        self.write_all(&resume, &worktrees)
    }

    /// Remove every worktree binding owned by `session_id`, preserving every
    /// resume record. Returns the number removed (`0` is a no-op success).
    pub fn remove_worktree_session(&self, session_id: &str) -> io::Result<usize> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (resume, mut worktrees) = self.read_all()?;
        let before = worktrees.len();
        worktrees.retain(|binding| binding.session_id != session_id);
        let removed = before - worktrees.len();
        if removed > 0 {
            self.write_all(&resume, &worktrees)?;
        }
        Ok(removed)
    }

    /// Read and partition every record. A missing file yields two empty lists;
    /// malformed lines are skipped (a corrupt line must not block loading the
    /// rest).
    fn read_all(&self) -> io::Result<(Vec<ResumeBinding>, Vec<WorktreeBinding>)> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(err) => return Err(err),
        };
        let mut resume = Vec::new();
        let mut worktrees = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<Record>(line) {
                Ok(Record::Resume(binding)) => resume.push(binding),
                Ok(Record::Worktree(binding)) => worktrees.push(binding),
                // Skip a corrupt line so it cannot block loading the rest, but
                // surface it: a silently-dropped resume line means a session
                // never comes back, and a dropped worktree line loses its
                // restored metadata. The store holds no secrets, so logging the
                // offending line is safe and aids debugging.
                Err(err) => {
                    warn!(error = %err, line = %line, "skipping unparseable metadata-store line");
                }
            }
        }
        Ok((resume, worktrees))
    }

    /// Serialize all records (resume first, then worktree) to a temp file and
    /// rename it over the store path. One `rename(2)` commits both kinds.
    fn write_all(&self, resume: &[ResumeBinding], worktrees: &[WorktreeBinding]) -> io::Result<()> {
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
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use protocol::AgentKind;

    use super::{ResumeBinding, Store, WorktreeBinding, WorktreeStatus};

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
            created_at: "2026-06-19T00:00:00Z".to_owned(),
            updated_at: "2026-06-19T00:00:00Z".to_owned(),
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
}
