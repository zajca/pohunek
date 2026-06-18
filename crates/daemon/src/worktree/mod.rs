//! Worktree-per-session binding (milestone 8).
//!
//! Binds **one git worktree per `(session_id, repository, branch)`** so two
//! concurrent sessions on the same repository never share a working tree (see
//! `docs/plan-phase-1.md` "Worktree-per-Session"). Given a repo + branch +
//! optional base-branch, [`WorktreeManager::bind`] either reuses a worktree the
//! daemon already owns or creates a new one under the data dir at
//! `worktrees/<session>-<repo>-<branch-slug>/`, running `git worktree add`.
//!
//! Three setup failures are treated as **non-fatal warnings** (mirroring
//! Kandev's `FetchWarning` / `BaseBranchFallbackWarning` / `SetupScriptWarning`):
//! a failed `git fetch` falls back to the local base ref, a missing base branch
//! falls back to the repository's default branch, and a failing setup script
//! keeps the worktree. None of them aborts session creation — the worktree is
//! kept, the warning surfaced, and the user decides whether to intervene.
//!
//! Ownership is implicit, exactly as in Kandev: the daemon owns a worktree iff
//! it has a [`WorktreeBinding`] recording that path. `bind` refuses to adopt a
//! foreign directory sitting at the computed path (no binding ⇒ not ours), and
//! [`WorktreeManager::cleanup_session`] refuses to remove a tree the daemon does
//! not own. The git command building follows herdr `src/worktree.rs` (pure
//! `Command` construction + a single executor with stderr→stdout→status error
//! mapping); the on-disk validity gate (a worktree has a `.git` *file* whose
//! content starts with `gitdir:`) follows Kandev `manager_state.go` `IsValid`.
//!
//! TODO(milestone 9): the full SQLite `worktree` table (see
//! `docs/plan-phase-1.md` "SQLite Schema") absorbs the minimal binding store
//! below, exactly as it absorbs the M7 resume-binding store. The columns
//! persisted here (`session_id`, `repository`, `branch`, `base_branch`,
//! `branch_slug`, `path`, `status`, timestamps) are the `worktree` table's
//! columns, so this is a direct precursor, not a parallel design.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use protocol::{ErrorClass, ProtocolError, SessionWarning, SessionWarningKind};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::{debug, warn};

/// Relative path of the optional per-repository setup script, run inside a
/// freshly created worktree. A non-zero exit (or any spawn failure) is recorded
/// as a [`SessionWarningKind::SetupScript`] warning and never aborts binding.
const SETUP_SCRIPT_REL: &str = ".zagentmesh/setup";

/// Interpreter used to run the setup script, so a script without an executable
/// bit (the common case for a committed `.zagentmesh/setup`) still runs.
const SETUP_SCRIPT_INTERPRETER: &str = "sh";

/// Fallback directory-name component when a repository path has no usable file
/// name (e.g. the filesystem root) or it sanitizes to empty.
const REPO_NAME_FALLBACK: &str = "repo";

/// Lifecycle status of a worktree binding.
///
/// Mirrors Kandev's `active`/`merged`/`deleted` status strings. Milestone 8
/// only ever sets `Active` (on bind) and `Deleted` (on cleanup); `Merged` is
/// defined for forward-compatibility with the M9 `worktree` table.
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
    /// The zagentmesh session id that owns this worktree.
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

/// Inputs for binding a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRequest {
    /// Owning session id (becomes the leading path component).
    pub session_id: String,
    /// Source repository path (need not be canonical; `bind` canonicalizes it).
    pub repo: PathBuf,
    /// Branch to check out in the worktree.
    pub branch: String,
    /// Requested base branch. `None` uses the repository's current branch.
    pub base_branch: Option<String>,
}

/// Result of a successful [`WorktreeManager::bind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeBound {
    /// Absolute path of the bound worktree (the agent's launch cwd).
    pub path: PathBuf,
    /// Canonicalized source repository.
    pub repository: PathBuf,
    /// Branch checked out in the worktree.
    pub branch: String,
    /// Base branch actually used (after any fallback).
    pub base_branch: String,
    /// Whether an existing owned worktree was reused rather than created.
    pub reused: bool,
    /// Non-fatal warnings raised while binding.
    pub warnings: Vec<SessionWarning>,
}

/// File-backed minimal worktree-binding store.
///
/// Same shape as the M7 [`crate::store::ResumeStore`]: newline-delimited JSON
/// (one binding per line) under the data dir, rewritten atomically via a temp
/// file + rename, owner-private (`0600`). Writes are serialized by an internal
/// lock. No secrets are written (a repo path, branch, and worktree path are not
/// secrets).
#[derive(Debug)]
pub struct WorktreeStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl WorktreeStore {
    /// Open a store at `path`. The file is created lazily on first `record`.
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

    /// Load all bindings. A missing file yields an empty list; malformed lines
    /// are skipped so one corrupt line cannot hide the rest.
    pub fn load(&self) -> io::Result<Vec<WorktreeBinding>> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        Ok(content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<WorktreeBinding>(line).ok())
            .collect())
    }

    /// Find the active binding for a `(session_id, repository, branch_slug)`
    /// triple, if one exists. Deleted/merged rows are invisible (like Kandev's
    /// `status = 'active'` reuse filter), so a cleaned-up session re-binds fresh.
    pub fn find(
        &self,
        session_id: &str,
        repository: &Path,
        branch_slug: &str,
    ) -> io::Result<Option<WorktreeBinding>> {
        Ok(self.load()?.into_iter().find(|binding| {
            binding.status == WorktreeStatus::Active
                && binding.session_id == session_id
                && binding.repository == repository
                && binding.branch_slug == branch_slug
        }))
    }

    /// Upsert a binding (keyed by `(session_id, repository, branch_slug)`) and
    /// rewrite the file atomically. The triple key is what stops two branches of
    /// one `(session, repository)` pair from collapsing onto a single row.
    pub fn record(&self, binding: &WorktreeBinding) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut bindings = self.load()?;
        if let Some(existing) = bindings.iter_mut().find(|existing| {
            existing.session_id == binding.session_id
                && existing.repository == binding.repository
                && existing.branch_slug == binding.branch_slug
        }) {
            *existing = binding.clone();
        } else {
            bindings.push(binding.clone());
        }
        self.write_all(&bindings)
    }

    /// Remove every binding owned by `session_id` and rewrite the file. Returns
    /// the number of bindings removed (`0` is a no-op success).
    pub fn remove_session(&self, session_id: &str) -> io::Result<usize> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut bindings = self.load()?;
        let before = bindings.len();
        bindings.retain(|binding| binding.session_id != session_id);
        let removed = before - bindings.len();
        if removed > 0 {
            self.write_all(&bindings)?;
        }
        Ok(removed)
    }

    /// Serialize all bindings to a temp file and rename it over the store path.
    fn write_all(&self, bindings: &[WorktreeBinding]) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for binding in bindings {
            let line = serde_json::to_string(binding)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            body.push_str(&line);
            body.push('\n');
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
            .unwrap_or_else(|| "worktree-bindings.jsonl".into());
        name.push(format!(".tmp.{}", std::process::id()));
        match self.path.parent() {
            Some(parent) => parent.join(name),
            None => PathBuf::from(name),
        }
    }
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

/// Binds and cleans up per-session worktrees under a single root directory.
#[derive(Debug)]
pub struct WorktreeManager {
    /// Root under which worktrees are created (`<data_dir>/worktrees`).
    root: PathBuf,
    /// Minimal binding store (the M9 SQLite-table precursor).
    store: WorktreeStore,
}

impl WorktreeManager {
    /// Build a manager that creates worktrees under `root` and persists bindings
    /// to `store`.
    #[must_use]
    pub fn new(root: PathBuf, store: WorktreeStore) -> Self {
        Self { root, store }
    }

    /// The minimal binding store, for inspection/tests.
    #[must_use]
    pub fn store(&self) -> &WorktreeStore {
        &self.store
    }

    /// Compute the deterministic worktree path for a request without touching
    /// the filesystem. Errors when the branch has no usable slug characters.
    ///
    /// # Errors
    ///
    /// [`ProtocolError`] (`invalid_branch_slug`) when the branch sanitizes to an
    /// empty slug (e.g. an all-non-ASCII branch name).
    pub fn worktree_path(
        &self,
        session_id: &str,
        repo: &Path,
        branch: &str,
    ) -> Result<PathBuf, ProtocolError> {
        let slug = require_branch_slug(branch)?;
        Ok(self.path_for(session_id, repo, &slug))
    }

    fn path_for(&self, session_id: &str, repo: &Path, branch_slug: &str) -> PathBuf {
        let repo_name = repo_name_slug(repo);
        self.root
            .join(format!("{session_id}-{repo_name}-{branch_slug}"))
    }

    /// Bind a worktree for `(session_id, repository, branch)`: reuse an owned
    /// worktree if one is valid on disk, recreate it if the daemon owns it but
    /// the directory went missing, refuse to adopt a foreign directory, or
    /// create a fresh worktree otherwise.
    ///
    /// Runs blocking `git` subprocesses; call it from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Fatal failures abort with a typed [`ProtocolError`]: an empty branch slug
    /// (`invalid_branch_slug`), a non-git or unreadable repository
    /// (`not_a_git_repo`), a foreign directory already at the target path
    /// (`worktree_path_conflict`), an unresolvable base branch with no fallback
    /// (`invalid_base_branch`), or a `git worktree add` failure
    /// (`worktree_add_failed`). The three non-fatal paths (fetch, base-branch
    /// fallback, setup script) never abort; they are returned in
    /// [`WorktreeBound::warnings`].
    pub fn bind(&self, req: &WorktreeRequest) -> Result<WorktreeBound, ProtocolError> {
        let slug = require_branch_slug(&req.branch)?;
        // The branch and base branch arrive from the socket and are passed
        // positionally to `git`; reject a leading dash (argv flag injection) or
        // control chars at this trust boundary, mirroring `agent::SessionRef`.
        validate_git_ref_arg(&req.branch, "branch")?;
        if let Some(base) = req.base_branch.as_deref() {
            validate_git_ref_arg(base, "base branch")?;
        }
        let repository = canonical_or_original(&req.repo);
        if !is_git_repo(&repository) {
            return Err(error(
                ErrorClass::Runtime,
                "not_a_git_repo",
                format!(
                    "{} is not a git repository",
                    repository.display()
                ),
                Some("pass --repo pointing at a git working tree".to_owned()),
            ));
        }

        let path = self.path_for(&req.session_id, &repository, &slug);
        let owned = self.store.find(&req.session_id, &repository, &slug).map_err(
            |err| store_error("read worktree binding store", &err),
        )?;

        // Reuse / recreate / refuse-foreign decision (Kandev tryReuseExisting).
        if path.exists() {
            match owned {
                Some(binding) if is_valid_worktree(&path) => {
                    debug!(
                        session_id = %req.session_id,
                        path = %path.display(),
                        "reusing owned worktree"
                    );
                    return Ok(WorktreeBound {
                        path: binding.path,
                        repository,
                        branch: binding.branch,
                        base_branch: binding.base_branch,
                        reused: true,
                        warnings: Vec::new(),
                    });
                }
                Some(_) => {
                    // We own the binding but the directory is no longer a valid
                    // worktree: prune the stale admin entry and remove leftovers
                    // so the create path below can re-add cleanly.
                    debug!(
                        session_id = %req.session_id,
                        path = %path.display(),
                        "recreating owned worktree with an invalid directory"
                    );
                    self.reset_stale_worktree(&repository, &path)?;
                }
                None => {
                    // A directory we have no binding for is not ours; refuse to
                    // adopt or clobber it (the ownership gate).
                    return Err(error(
                        ErrorClass::Runtime,
                        "worktree_path_conflict",
                        format!(
                            "refusing to use {}: a directory already exists there that this daemon does not own",
                            path.display()
                        ),
                        Some("remove the directory or choose a different branch".to_owned()),
                    ));
                }
            }
        } else if owned.is_some() {
            // Owned binding but the directory vanished entirely: prune the stale
            // git admin entry before re-adding at the same path.
            self.reset_stale_worktree(&repository, &path)?;
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                store_error("create the worktrees root directory", &err)
            })?;
        }

        let mut warnings = Vec::new();
        let base_branch = self.resolve_base_branch(&repository, req, &mut warnings)?;
        // Resolve the start-point for a new branch: the freshly fetched ref when
        // a fetch succeeds, else the (recorded) local base. The logical
        // `base_branch` name is what we persist/display; `start_point` is what
        // `git worktree add` actually branches from.
        let start_point = self.fetch_start_point(&repository, &base_branch, &mut warnings);
        self.create_worktree(&repository, &path, &req.branch, &start_point)?;
        run_setup_script(&path, &mut warnings);

        let now = timestamp_now();
        let binding = WorktreeBinding {
            session_id: req.session_id.clone(),
            repository: repository.clone(),
            branch: req.branch.clone(),
            base_branch: base_branch.clone(),
            branch_slug: slug,
            path: path.clone(),
            status: WorktreeStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store
            .record(&binding)
            .map_err(|err| store_error("persist worktree binding", &err))?;

        Ok(WorktreeBound {
            path,
            repository,
            branch: req.branch.clone(),
            base_branch,
            reused: false,
            warnings,
        })
    }

    /// Remove every worktree owned by `session_id` (cleanup). Refuses to touch a
    /// tree the daemon does not own: only paths recorded in the binding store
    /// are removed, then their bindings are dropped. Returns the number of
    /// worktrees removed.
    ///
    /// Best-effort by design — a `git worktree remove` failure is logged and the
    /// binding is still dropped so a half-removed tree does not wedge cleanup.
    ///
    /// # Errors
    ///
    /// Only the binding-store read/write can fail with a [`ProtocolError`]; git
    /// failures are non-fatal.
    pub fn cleanup_session(&self, session_id: &str) -> Result<usize, ProtocolError> {
        let bindings = self
            .store
            .load()
            .map_err(|err| store_error("read worktree binding store", &err))?;
        let mut removed = 0;
        for binding in bindings
            .into_iter()
            .filter(|binding| binding.session_id == session_id)
        {
            // Ownership proof is the binding itself; only then do we delete.
            if let Err(message) = worktree_remove(&binding.repository, &binding.path) {
                warn!(
                    session_id = %session_id,
                    path = %binding.path.display(),
                    error = %message,
                    "git worktree remove failed during cleanup; dropping binding anyway"
                );
            }
            removed += 1;
        }
        if removed > 0 {
            self.store
                .remove_session(session_id)
                .map_err(|err| store_error("drop worktree bindings", &err))?;
        }
        Ok(removed)
    }

    /// Prune a stale git admin entry and remove any leftover directory so a
    /// fresh `git worktree add` can reuse the path.
    fn reset_stale_worktree(&self, repository: &Path, path: &Path) -> Result<(), ProtocolError> {
        // `git worktree prune` clears admin entries for vanished worktrees; a
        // failure here is non-fatal (the add below is the real gate).
        if let Err(message) = worktree_prune(repository) {
            debug!(error = %message, "git worktree prune failed before recreate");
        }
        if path.exists() {
            fs::remove_dir_all(path).map_err(|err| {
                store_error("remove a stale worktree directory before recreate", &err)
            })?;
        }
        Ok(())
    }

    /// Resolve the base ref, falling back to the repository's default branch
    /// when the requested base is missing (non-fatal `base_branch_fallback`).
    ///
    /// The default branch is resolved **lazily** — only when no base was
    /// requested or a fallback is needed — so a repository in detached HEAD can
    /// still bind a worktree as long as an existing `--base-branch` is supplied.
    fn resolve_base_branch(
        &self,
        repository: &Path,
        req: &WorktreeRequest,
        warnings: &mut Vec<SessionWarning>,
    ) -> Result<String, ProtocolError> {
        let Some(requested) = req.base_branch.as_deref().map(str::trim).filter(|b| !b.is_empty())
        else {
            // No base requested: branch from the repository's current branch.
            return self.default_branch(repository);
        };

        // `requested` was validated for flag injection by the caller.
        match branch_exists(repository, requested) {
            Ok(true) => Ok(requested.to_owned()),
            Ok(false) => {
                let default_branch = self.default_branch(repository)?;
                if default_branch == requested {
                    return Err(error(
                        ErrorClass::Runtime,
                        "invalid_base_branch",
                        format!("base branch {requested:?} does not exist"),
                        Some("create the base branch or omit --base-branch".to_owned()),
                    ));
                }
                warnings.push(SessionWarning {
                    kind: SessionWarningKind::BaseBranchFallback,
                    message: format!(
                        "Requested base branch {requested:?} not found; used {default_branch:?} instead."
                    ),
                    detail: Some(format!(
                        "git could not resolve refs/heads/{requested}; recovered using the repository's current branch {default_branch:?}"
                    )),
                });
                Ok(default_branch)
            }
            // Three-valued: a "could not tell" error is loud, not a silent
            // fallback, so we do not mask a broken repository as a missing branch.
            Err(message) => Err(error(
                ErrorClass::Runtime,
                "invalid_base_branch",
                format!("could not verify base branch {requested:?}: {message}"),
                None,
            )),
        }
    }

    /// The repository's current branch, used as the default base ref, validated
    /// like any other git ref argument.
    ///
    /// The branch name comes from the repo's `HEAD` ref **verbatim**; a crafted
    /// repository could point `HEAD` at a dash-leading ref (e.g.
    /// `refs/heads/--upload-pack=evil`) to smuggle a `git` flag into the
    /// positional sinks. Validating it here closes that argv-injection path even
    /// though the user never typed the name.
    fn default_branch(&self, repository: &Path) -> Result<String, ProtocolError> {
        let default = current_branch(repository).map_err(|message| {
            error(
                ErrorClass::Runtime,
                "invalid_base_branch",
                format!(
                    "could not determine the default branch of {} (detached HEAD?): {message}",
                    repository.display()
                ),
                Some("pass --base-branch naming an existing branch".to_owned()),
            )
        })?;
        validate_git_ref_arg(&default, "default branch")?;
        Ok(default)
    }

    /// Determine the start-point for a **new** branch: the freshly fetched base
    /// when a fetch from `origin` succeeds, else the local `base_branch` (with a
    /// non-fatal `fetch` warning on failure). A fetch is only attempted when an
    /// `origin` remote is configured.
    ///
    /// On success the start-point is `FETCH_HEAD` so the new branch genuinely
    /// starts from the up-to-date remote tip, not the stale local ref.
    fn fetch_start_point(
        &self,
        repository: &Path,
        base_branch: &str,
        warnings: &mut Vec<SessionWarning>,
    ) -> String {
        if !has_origin(repository) {
            // No remote to fetch from — branch from the local base ref.
            return base_branch.to_owned();
        }
        match fetch_origin(repository, base_branch) {
            // The fetched tip is in FETCH_HEAD; branch from it so the worktree
            // starts up to date rather than from the stale local ref.
            Ok(()) => "FETCH_HEAD".to_owned(),
            Err(message) => {
                warnings.push(SessionWarning {
                    kind: SessionWarningKind::Fetch,
                    message: format!(
                        "Could not fetch {base_branch:?} from origin; using the local copy, which may be out of date."
                    ),
                    detail: Some(message),
                });
                base_branch.to_owned()
            }
        }
    }

    /// Run `git worktree add`, checking out an existing branch or creating a new
    /// one from the base ref.
    fn create_worktree(
        &self,
        repository: &Path,
        path: &Path,
        branch: &str,
        start_point: &str,
    ) -> Result<(), ProtocolError> {
        let exists = matches!(branch_exists(repository, branch), Ok(true));
        let result = if exists {
            // The branch already exists (e.g. a recreate after the dir was lost):
            // check it out rather than failing on `-b`.
            worktree_add_existing(repository, path, branch)
        } else {
            worktree_add_new(repository, path, branch, start_point)
        };
        result.map_err(|message| {
            // Git allows only one worktree per branch: a concurrent or prior
            // session already holding this branch fails with "already checked
            // out"/"already used by worktree". Surface that as a clear typed
            // error rather than a generic add failure.
            if is_branch_in_use_error(&message) {
                error(
                    ErrorClass::Runtime,
                    "worktree_branch_in_use",
                    format!("branch {branch:?} is already checked out in another worktree"),
                    Some("use a different branch for this session".to_owned()),
                )
            } else {
                error(
                    ErrorClass::Runtime,
                    "worktree_add_failed",
                    format!("git worktree add failed: {message}"),
                    None,
                )
            }
        })
    }
}

/// Whether a `git worktree add` failure means the branch is already bound to
/// another worktree (git enforces one worktree per branch).
fn is_branch_in_use_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already checked out")
        || lower.contains("already used by worktree")
        || (lower.contains("branch named") && lower.contains("already exists"))
}

/// Reject a branch/base argument that could be misparsed as a `git` flag or
/// carry control characters. These values arrive from the socket and are passed
/// positionally to `git`, so a leading `-` would let a caller inject an argv
/// flag (mirrors the resume-id guard in `agent::SessionRef`).
fn validate_git_ref_arg(value: &str, what: &str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(error(
            ErrorClass::Runtime,
            "invalid_branch",
            format!("{what} cannot be empty"),
            None,
        ));
    }
    if value.starts_with('-') {
        return Err(error(
            ErrorClass::Runtime,
            "invalid_branch",
            format!("{what} cannot begin with '-'"),
            Some("choose a name that does not start with a dash".to_owned()),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(error(
            ErrorClass::Runtime,
            "invalid_branch",
            format!("{what} cannot contain control characters"),
            None,
        ));
    }
    Ok(())
}

/// Sanitize a branch name into a filesystem-safe single path segment, requiring
/// a non-empty result.
fn require_branch_slug(branch: &str) -> Result<String, ProtocolError> {
    let slug = branch_slug(branch);
    if slug.is_empty() {
        return Err(error(
            ErrorClass::Runtime,
            "invalid_branch_slug",
            format!("branch {branch:?} has no usable characters for a worktree path"),
            Some("use a branch name containing ASCII letters or digits".to_owned()),
        ));
    }
    Ok(slug)
}

/// Convert a git branch name into a filesystem-safe single path segment.
///
/// Ported from Kandev `SanitizeBranchSlug` (`config.go`): ASCII alphanumerics
/// and `_`, `.`, `-` are kept; every other character (including `/`) becomes a
/// `-`; runs of `-` collapse to one; leading/trailing `-` and `.` are trimmed.
/// Returns an empty string when no usable characters remain (callers must treat
/// that as "no slug", not an empty path segment). Deterministic: the same branch
/// always produces the same slug.
#[must_use]
pub fn branch_slug(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    let mut prev_dash = false;
    for ch in branch.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if !prev_dash {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(mapped);
            prev_dash = false;
        }
    }
    out.trim_matches(|c| c == '-' || c == '.').to_owned()
}

/// Slug a repository directory name for the worktree path, falling back to a
/// fixed name when the repo path has no usable file name.
fn repo_name_slug(repo: &Path) -> String {
    let name = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .map(|name| branch_slug(&name))
        .unwrap_or_default();
    if name.is_empty() {
        REPO_NAME_FALLBACK.to_owned()
    } else {
        name
    }
}

/// Best-effort path canonicalization that never fails (herdr `canonical_or_original`).
fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `path` is a live git worktree: a directory whose `.git` entry is a
/// *file* beginning with `gitdir:` (Kandev `IsValid`). A plain leftover
/// directory or a full clone (`.git` directory) does not qualify.
fn is_valid_worktree(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    match fs::read_to_string(path.join(".git")) {
        Ok(content) => content.trim_start().starts_with("gitdir:"),
        Err(_) => false,
    }
}

/// Whether `repo` is inside a git working tree.
fn is_git_repo(repo: &Path) -> bool {
    matches!(
        git_capture(repo, &["rev-parse", "--is-inside-work-tree"]),
        Ok(out) if out.trim() == "true"
    )
}

/// The repository's current branch (used as the default base ref).
fn current_branch(repo: &Path) -> Result<String, String> {
    git_capture(repo, &["symbolic-ref", "--short", "HEAD"])
}

/// Three-valued branch existence (Kandev `branchExists`): `Ok(true)` exists,
/// `Ok(false)` git ran and the ref is absent, `Err` could not tell.
fn branch_exists(repo: &Path, branch: &str) -> Result<bool, String> {
    let refname = format!("refs/heads/{branch}");
    let output = git_command(repo)
        .args(["rev-parse", "--verify", "--quiet", &refname])
        .output()
        .map_err(|err| format!("failed to run git: {err}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure_message(
            &["rev-parse", "--verify", "--quiet", &refname],
            &output,
        )),
    }
}

/// Whether the repository has an `origin` remote configured.
fn has_origin(repo: &Path) -> bool {
    git_capture(repo, &["remote", "get-url", "origin"]).is_ok()
}

/// Fetch a single ref from `origin` without tags. `--end-of-options` guarantees
/// the ref is treated positionally even if a future caller forgets the
/// leading-dash guard (defense in depth against argv flag injection).
fn fetch_origin(repo: &Path, base_branch: &str) -> Result<(), String> {
    let mut cmd = git_command(repo);
    cmd.arg("fetch")
        .arg("--no-tags")
        .arg("origin")
        .arg("--end-of-options")
        .arg(base_branch);
    run_command(cmd).map(|_| ())
}

/// `git worktree add -b <branch> <path> <start_point>` — new branch from a
/// start-point ref. `--end-of-options` forces the path and start-point to be
/// parsed positionally (the branch is validated by the caller).
fn worktree_add_new(repo: &Path, path: &Path, branch: &str, start_point: &str) -> Result<(), String> {
    let mut cmd = git_command(repo);
    cmd.arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(branch)
        .arg("--end-of-options")
        .arg(path)
        .arg(start_point);
    run_command(cmd).map(|_| ())
}

/// `git worktree add <path> <branch>` — check out an existing branch.
fn worktree_add_existing(repo: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let mut cmd = git_command(repo);
    cmd.arg("worktree")
        .arg("add")
        .arg("--end-of-options")
        .arg(path)
        .arg(branch);
    run_command(cmd).map(|_| ())
}

/// `git worktree remove --force <path>` — remove the checkout (not the branch).
fn worktree_remove(repo: &Path, path: &Path) -> Result<(), String> {
    let mut cmd = git_command(repo);
    cmd.arg("worktree").arg("remove").arg("--force").arg(path);
    run_command(cmd).map(|_| ())
}

/// `git worktree prune` — drop admin entries for vanished worktrees.
fn worktree_prune(repo: &Path) -> Result<(), String> {
    git_run(repo, &["worktree", "prune"]).map(|_| ())
}

/// Run `<repo>/.zagentmesh/setup` inside `worktree` if present; a failure is a
/// non-fatal `setup_script` warning (the worktree is kept).
fn run_setup_script(worktree: &Path, warnings: &mut Vec<SessionWarning>) {
    let script = worktree.join(SETUP_SCRIPT_REL);
    if !script.is_file() {
        return;
    }
    // TODO(milestone 9+): bound the script with a timeout; M8 keeps it minimal
    // and the script is the user's own committed file.
    let result = Command::new(SETUP_SCRIPT_INTERPRETER)
        .arg(&script)
        .current_dir(worktree)
        .output();
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            warnings.push(SessionWarning {
                kind: SessionWarningKind::SetupScript,
                message: "Repository setup script failed; the worktree was kept without it."
                    .to_owned(),
                detail: Some(if stderr.is_empty() {
                    format!("{} exited with status {}", script.display(), output.status)
                } else {
                    stderr
                }),
            });
        }
        Err(err) => {
            warnings.push(SessionWarning {
                kind: SessionWarningKind::SetupScript,
                message: "Repository setup script could not be run; the worktree was kept without it."
                    .to_owned(),
                detail: Some(format!("failed to spawn {}: {err}", script.display())),
            });
        }
    }
}

/// A `git` command scoped to `repo` via `-C` (passed as an `OsStr` so non-UTF-8
/// repo paths survive).
fn git_command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd
}

/// Run a `git` invocation built with [`git_command`], discarding stdout but
/// mapping a non-zero exit (or spawn failure) to the herdr-style error message.
fn run_command(mut cmd: Command) -> Result<String, String> {
    let output = cmd
        .output()
        .map_err(|err| format!("failed to run git: {err}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(output_failure_message(&output))
}

/// Run `git -C <repo> <args>` capturing trimmed stdout on success.
fn git_run(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = git_command(repo);
    cmd.args(args);
    run_command(cmd)
}

/// Like [`git_run`] but for read-only probes: identical behavior, named to make
/// call sites read as "capture this value".
fn git_capture(repo: &Path, args: &[&str]) -> Result<String, String> {
    git_run(repo, args)
}

/// herdr-style failure message: prefer trimmed stderr, then stdout, then a
/// synthetic status line.
fn output_failure_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    if message.is_empty() {
        format!("git exited with status {}", output.status)
    } else {
        message
    }
}

fn git_failure_message(args: &[&str], output: &std::process::Output) -> String {
    let base = output_failure_message(output);
    format!("git {}: {base}", args.join(" "))
}

/// Build a typed worktree error.
fn error(
    class: ErrorClass,
    code: &str,
    msg: String,
    recover: Option<String>,
) -> ProtocolError {
    ProtocolError::new(class, code, msg, recover)
}

/// Map a binding-store I/O failure to a typed runtime error.
fn store_error(what: &str, err: &io::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "worktree_store_error",
        format!("failed to {what}: {err}"),
        None,
    )
}

/// Current UTC time as an RFC3339 string (matches the session store).
fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests;
