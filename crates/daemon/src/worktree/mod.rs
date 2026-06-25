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
//! The worktree-binding records and their store now live in the unified
//! [`crate::store::Store`] (milestone 9): one owner-private file holds both the
//! resume bindings and the worktree bindings behind a single serialization point,
//! so a session's two records stay mutually consistent. This module owns only the
//! git mechanics (path computation, `git worktree add`, the non-fatal warning
//! paths, the ownership gate) and persists through that shared store.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use protocol::{ErrorClass, ProtocolError, SessionWarning, SessionWarningKind};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::store::{Store, WorktreeBinding, WorktreeStatus};

/// Relative path of the optional per-repository setup script, run inside a
/// freshly created worktree. A non-zero exit (or any spawn failure) is recorded
/// as a [`SessionWarningKind::SetupScript`] warning and never aborts binding.
const SETUP_SCRIPT_REL: &str = ".pohunek/setup";

/// Interpreter used to run the setup script, so a script without an executable
/// bit (the common case for a committed `.pohunek/setup`) still runs.
const SETUP_SCRIPT_INTERPRETER: &str = "sh";

/// How often [`wait_with_timeout`] polls a running setup script for completion.
/// Small enough that a quick script returns promptly, large enough that the busy
/// loop is negligible against the (much larger) setup-script timeout.
const SETUP_SCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Fallback directory-name component when a repository path has no usable file
/// name (e.g. the filesystem root) or it sanitizes to empty.
const REPO_NAME_FALLBACK: &str = "repo";

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
    /// Project this worktree belongs to (derived id), stamped onto the binding so
    /// `project show` / `project rm --prune-worktrees` can find pohunek's own
    /// worktrees for a project. `None` for a bare `--repo` with no resolved project.
    pub project_id: Option<String>,
    /// Resolved agent NAME (Part B), persisted onto the binding and exposed to the
    /// create/remove hooks as `POHUNEK_AGENT`. A non-secret name only.
    pub agent: String,
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

/// Outcome of [`WorktreeManager::cleanup_project`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectPrune {
    /// Number of owned worktrees removed.
    pub removed: usize,
    /// Canonical paths of owned worktrees skipped (a live session was using them).
    pub skipped: Vec<PathBuf>,
}

/// Binds and cleans up per-session worktrees under a single root directory.
#[derive(Debug)]
pub struct WorktreeManager {
    /// Root under which worktrees are created (`<data_dir>/worktrees`).
    root: PathBuf,
    /// Shared unified metadata store holding the worktree bindings. Shared
    /// (`Arc`) with the session registry so resume and worktree records live in
    /// one file behind one serialization point.
    store: Arc<Store>,
    /// Wall-clock bound on a single lifecycle hook (worktree pre/post-create or
    /// -remove, and the session-level hooks). A hook that does not finish within
    /// this window is terminated and recorded as a non-fatal `hook` warning, so a
    /// hanging hook can never wedge `session.new` or cleanup.
    hook_timeout: Duration,
    /// Host config dir (`~/.config/pohunek`), source of the host-global hook layer
    /// (`<config_dir>/hooks/<event>`). `None` disables that layer; in-repo hooks
    /// still run.
    config_dir: Option<PathBuf>,
}

impl WorktreeManager {
    /// Build a manager that creates worktrees under `root`, persists bindings to
    /// the shared `store`, bounds each hook by `hook_timeout`, and composes the
    /// host-global hook layer from `config_dir` (when configured).
    #[must_use]
    pub fn new(
        root: PathBuf,
        store: Arc<Store>,
        hook_timeout: Duration,
        config_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            root,
            store,
            hook_timeout,
            config_dir,
        }
    }

    /// The shared metadata store, for inspection/tests.
    #[must_use]
    pub fn store(&self) -> &Store {
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
                format!("{} is not a git repository", repository.display()),
                Some("pass --repo pointing at a git working tree".to_owned()),
            ));
        }

        let path = self.path_for(&req.session_id, &repository, &slug);
        let owned = self
            .store
            .find_worktree(&req.session_id, &repository, &slug)
            .map_err(|err| store_error("read worktree binding store", &err))?;

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
            fs::create_dir_all(parent)
                .map_err(|err| store_error("create the worktrees root directory", &err))?;
        }

        let mut warnings = Vec::new();
        let base_branch = self.resolve_base_branch(&repository, req, &mut warnings)?;
        // Resolve the start-point for a new branch: the freshly fetched ref when
        // a fetch succeeds, else the (recorded) local base. The logical
        // `base_branch` name is what we persist/display; `start_point` is what
        // `git worktree add` actually branches from.
        let start_point = self.fetch_start_point(&repository, &base_branch, &mut warnings);
        // Pre-create hook: fires only on the fresh-create path (the reuse / recreate
        // / foreign-conflict branches above all returned before here). The worktree
        // does not exist yet, so it runs IN THE REPOSITORY — `POHUNEK_BASE_BRANCH`
        // is already resolved and available.
        self.run_worktree_hook(
            HookEvent::PreCreate,
            &repository,
            req,
            &repository,
            None,
            &base_branch,
            &mut warnings,
        );
        self.create_worktree(&repository, &path, &req.branch, &start_point)?;
        // Post-create hook (replaces the legacy `.pohunek/setup` script, which it
        // still falls back to): runs IN the freshly created worktree.
        self.run_worktree_hook(
            HookEvent::PostCreate,
            &path,
            req,
            &repository,
            Some(&path),
            &base_branch,
            &mut warnings,
        );

        let now = timestamp_now();
        let binding = WorktreeBinding {
            session_id: req.session_id.clone(),
            repository: repository.clone(),
            branch: req.branch.clone(),
            base_branch: base_branch.clone(),
            branch_slug: slug,
            path: path.clone(),
            agent: req.agent.clone(),
            status: WorktreeStatus::Active,
            project_id: req.project_id.clone(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.record_worktree(&binding).map_err(|err| {
            // The worktree directory and branch checkout already exist. If the
            // binding cannot be persisted we would orphan them: with no binding
            // the ownership gate can never reclaim the tree, and the branch stays
            // checked out, blocking the next `session.new` on it with
            // `worktree_branch_in_use`. Roll the checkout back before erroring.
            if let Err(message) = worktree_remove(&repository, &path) {
                warn!(
                    path = %path.display(),
                    error = %message,
                    "failed to remove worktree after a failed binding persist"
                );
            }
            store_error("persist worktree binding", &err)
        })?;

        Ok(WorktreeBound {
            path,
            repository,
            branch: req.branch.clone(),
            base_branch,
            reused: false,
            warnings,
        })
    }

    /// Fire a create-side worktree hook (`pre`/`post-create`) from the request
    /// context, bounded by `hook_timeout` and composed with the host-global layer.
    // The create hook needs the event, the hook-lookup dir, the request, the repo,
    // the optional worktree, the base branch, and the warning sink — all distinct
    // inputs with no natural grouping struct beyond the request itself.
    #[allow(clippy::too_many_arguments)]
    fn run_worktree_hook(
        &self,
        event: HookEvent,
        in_repo_dir: &Path,
        req: &WorktreeRequest,
        repository: &Path,
        worktree: Option<&Path>,
        base_branch: &str,
        warnings: &mut Vec<SessionWarning>,
    ) {
        let ctx = HookContext {
            session_id: req.session_id.clone(),
            project_id: req.project_id.clone(),
            agent: req.agent.clone(),
            repo: Some(repository.to_path_buf()),
            worktree: worktree.map(Path::to_path_buf),
            branch: Some(req.branch.clone()),
            base_branch: Some(base_branch.to_owned()),
            stop_reason: None,
            activity: None,
        };
        run_hook(
            event,
            in_repo_dir,
            &ctx,
            self.hook_timeout,
            self.config_dir.as_deref(),
            warnings,
        );
    }

    /// Fire a remove-side worktree hook (`pre`/`post-remove`) from a persisted
    /// binding. `POHUNEK_AGENT`/`POHUNEK_BASE_BRANCH` come from the binding, so a
    /// remove hook sees the same identity a create hook did.
    fn run_remove_hook(
        &self,
        event: HookEvent,
        in_repo_dir: &Path,
        binding: &WorktreeBinding,
        worktree: Option<&Path>,
        warnings: &mut Vec<SessionWarning>,
    ) {
        let ctx = HookContext {
            session_id: binding.session_id.clone(),
            project_id: binding.project_id.clone(),
            agent: binding.agent.clone(),
            repo: Some(binding.repository.clone()),
            worktree: worktree.map(Path::to_path_buf),
            branch: Some(binding.branch.clone()),
            base_branch: Some(binding.base_branch.clone()),
            stop_reason: None,
            activity: None,
        };
        run_hook(
            event,
            in_repo_dir,
            &ctx,
            self.hook_timeout,
            self.config_dir.as_deref(),
            warnings,
        );
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
    pub fn cleanup_session(
        &self,
        session_id: &str,
        warnings: &mut Vec<SessionWarning>,
    ) -> Result<usize, ProtocolError> {
        let bindings = self
            .store
            .load_worktrees()
            .map_err(|err| store_error("read worktree binding store", &err))?;
        let mut removed = 0;
        for binding in bindings
            .into_iter()
            .filter(|binding| binding.session_id == session_id)
        {
            // pre-remove fires IN the worktree while it still exists.
            if binding.path.is_dir() {
                self.run_remove_hook(
                    HookEvent::PreRemove,
                    &binding.path,
                    &binding,
                    Some(&binding.path),
                    warnings,
                );
            }
            // Ownership proof is the binding itself; only then do we delete.
            if let Err(message) = worktree_remove(&binding.repository, &binding.path) {
                warn!(
                    session_id = %session_id,
                    path = %binding.path.display(),
                    error = %message,
                    "git worktree remove failed during cleanup; dropping binding anyway"
                );
            }
            // post-remove fires IN the repository (the worktree is gone). If the
            // repository itself is gone, skip with a warning rather than spawn in a
            // non-existent cwd.
            if binding.repository.is_dir() {
                self.run_remove_hook(
                    HookEvent::PostRemove,
                    &binding.repository,
                    &binding,
                    None,
                    warnings,
                );
            } else {
                warnings.push(hook_warning(
                    HookEvent::PostRemove,
                    format!(
                        "repository {} no longer exists; post-remove hook skipped",
                        binding.repository.display()
                    ),
                ));
            }
            removed += 1;
        }
        if removed > 0 {
            self.store
                .remove_worktree_session(session_id)
                .map_err(|err| store_error("drop worktree bindings", &err))?;
        }
        Ok(removed)
    }

    /// Remove the worktrees pohunek created for `project_id` (`project rm
    /// --prune-worktrees`), **skipping** any whose canonical path is in `skip`
    /// (a worktree with a live session — left in place so the session keeps its
    /// checkout), then drop the dropped worktrees' bindings. Same ownership rule
    /// as [`Self::cleanup_session`]: only paths recorded in a binding are touched,
    /// so the main checkout and any worktree pohunek did not create are never
    /// removed. Returns the count removed and the canonical paths skipped.
    ///
    /// Best-effort by design — a `git worktree remove` failure is logged and the
    /// binding is still dropped so a half-removed tree does not wedge the prune.
    ///
    /// # Errors
    ///
    /// Only the binding-store read/write can fail with a [`ProtocolError`]; git
    /// failures are non-fatal.
    pub fn cleanup_project(
        &self,
        project_id: &str,
        skip: &std::collections::HashSet<PathBuf>,
        warnings: &mut Vec<SessionWarning>,
    ) -> Result<ProjectPrune, ProtocolError> {
        let bindings = self
            .store
            .load_worktrees()
            .map_err(|err| store_error("read worktree binding store", &err))?;
        let mut prune = ProjectPrune::default();
        let mut removed_sessions = Vec::new();
        for binding in bindings
            .into_iter()
            .filter(|binding| binding.project_id.as_deref() == Some(project_id))
        {
            let canonical = canonical_or_original(&binding.path);
            if skip.contains(&canonical) {
                // A live session is using this worktree; leave it and its binding —
                // and fire NO remove hook for it (the `continue` is before the seams).
                prune.skipped.push(canonical);
                continue;
            }
            // pre-remove fires IN the worktree while it still exists.
            if binding.path.is_dir() {
                self.run_remove_hook(
                    HookEvent::PreRemove,
                    &binding.path,
                    &binding,
                    Some(&binding.path),
                    warnings,
                );
            }
            // The binding is the ownership proof; only an owned worktree is removed.
            if let Err(message) = worktree_remove(&binding.repository, &binding.path) {
                warn!(
                    project_id = %project_id,
                    path = %binding.path.display(),
                    error = %message,
                    "git worktree remove failed during project prune; dropping binding anyway"
                );
            }
            // post-remove fires IN the repository (worktree gone); skip + warn if the
            // repository itself is gone.
            if binding.repository.is_dir() {
                self.run_remove_hook(
                    HookEvent::PostRemove,
                    &binding.repository,
                    &binding,
                    None,
                    warnings,
                );
            } else {
                warnings.push(hook_warning(
                    HookEvent::PostRemove,
                    format!(
                        "repository {} no longer exists; post-remove hook skipped",
                        binding.repository.display()
                    ),
                ));
            }
            // A session binds at most one worktree, so dropping by session id drops
            // exactly this binding and never a skipped (live) one.
            removed_sessions.push(binding.session_id);
            prune.removed += 1;
        }
        for session_id in &removed_sessions {
            self.store
                .remove_worktree_session(session_id)
                .map_err(|err| store_error("drop worktree bindings", &err))?;
        }
        Ok(prune)
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
        let Some(requested) = req
            .base_branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
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
///
/// Shared with [`crate::project::detect`], which keys a project on the canonical
/// `git_common_dir` so symlinked checkouts converge to one record.
pub(crate) fn canonical_or_original(path: &Path) -> PathBuf {
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

/// Whether `repo` is a git repository pohunek can add a worktree to — a normal
/// working tree **or** a bare repo. `git worktree add` works on both; only a
/// non-repo directory is rejected. Accepting bare here is what makes the worktree
/// path the valid escape hatch for a bare project (an in-place session is refused
/// on a bare repo precisely because it has no working tree to run in).
fn is_git_repo(repo: &Path) -> bool {
    let answers_true =
        |args: &[&str]| matches!(git_capture(repo, args), Ok(out) if out.trim() == "true");
    answers_true(&["rev-parse", "--is-inside-work-tree"])
        || answers_true(&["rev-parse", "--is-bare-repository"])
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
fn worktree_add_new(
    repo: &Path,
    path: &Path,
    branch: &str,
    start_point: &str,
) -> Result<(), String> {
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

/// Outcome of supervising a setup script to (possibly forced) completion.
enum SetupOutcome {
    /// The script exited on its own with this status.
    Exited(ExitStatus),
    /// The hook outlived `hook_timeout` and was terminated.
    TimedOut,
    /// Waiting on the child failed (an OS error, not a script failure).
    WaitError(io::Error),
}

/// A lifecycle hook event (Part B). Its [`as_env`](HookEvent::as_env) string is
/// both the `POHUNEK_HOOK_EVENT` value and the hook-script filename under
/// `.pohunek/hooks/` (and the host-global `<config_dir>/hooks/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookEvent {
    /// Before `git worktree add`, in the repository (worktree not yet created).
    PreCreate,
    /// After `git worktree add`, in the new worktree (replaces `.pohunek/setup`).
    PostCreate,
    /// Before `git worktree remove`, in the worktree (still present).
    PreRemove,
    /// After `git worktree remove`, in the repository (worktree gone).
    PostRemove,
    /// After a PTY session is launched, in the session cwd.
    SessionStart,
    /// When a session reaches a terminal state, in the session cwd.
    SessionStop,
    /// When detector-visible agent activity changes, in the session cwd.
    AgentState,
}

impl HookEvent {
    /// The event's wire/filename token.
    pub(crate) fn as_env(self) -> &'static str {
        match self {
            HookEvent::PreCreate => "pre-create",
            HookEvent::PostCreate => "post-create",
            HookEvent::PreRemove => "pre-remove",
            HookEvent::PostRemove => "post-remove",
            HookEvent::SessionStart => "session-start",
            HookEvent::SessionStop => "session-stop",
            HookEvent::AgentState => "agent-state",
        }
    }
}

/// The non-secret context a hook receives as `POHUNEK_*` environment variables.
///
/// Every field is a non-secret identity value (session id, project id, agent
/// NAME, repo/worktree paths, branch names). The daemon handshake vars
/// (`POHUNEK_SOCKET_PATH`/`_DAEMON_ID`/`_ENV`/`_PROTOCOL_VERSION`) are **never**
/// here — a hook runs with a cleared environment plus only this allowlist.
#[derive(Debug, Default)]
pub(crate) struct HookContext {
    /// Owning session id (`POHUNEK_SESSION_ID`).
    pub session_id: String,
    /// Project id (`POHUNEK_PROJECT_ID`, empty string when `None`).
    pub project_id: Option<String>,
    /// Resolved agent NAME (`POHUNEK_AGENT`).
    pub agent: String,
    /// Source repository (`POHUNEK_REPO`).
    pub repo: Option<PathBuf>,
    /// Bound worktree path (`POHUNEK_WORKTREE`); set for post-create / pre-remove.
    pub worktree: Option<PathBuf>,
    /// Branch (`POHUNEK_BRANCH`).
    pub branch: Option<String>,
    /// Base branch (`POHUNEK_BASE_BRANCH`).
    pub base_branch: Option<String>,
    /// Terminal stop reason (`POHUNEK_STOP_REASON`) for `session-stop`.
    pub stop_reason: Option<&'static str>,
    /// Agent activity value (`POHUNEK_ACTIVITY`) for `agent-state`.
    pub activity: Option<&'static str>,
}

/// Build the env-clear allowlist a hook runs with: `PATH`/`HOME` passed through
/// from the daemon's own env, plus the non-secret `POHUNEK_*` context. No daemon
/// handshake var is ever included.
fn hook_env(event: HookEvent, ctx: &HookContext) -> Vec<(String, String)> {
    let mut env = Vec::new();
    // Pass through PATH/HOME so a hook can find `git`/`npm`/etc. and resolve `~`.
    if let Some(path) = std::env::var_os("PATH") {
        env.push(("PATH".to_owned(), path.to_string_lossy().into_owned()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_owned(), home.to_string_lossy().into_owned()));
    }
    env.push(("POHUNEK_HOOK_EVENT".to_owned(), event.as_env().to_owned()));
    env.push(("POHUNEK_SESSION_ID".to_owned(), ctx.session_id.clone()));
    env.push((
        "POHUNEK_PROJECT_ID".to_owned(),
        ctx.project_id.clone().unwrap_or_default(),
    ));
    env.push(("POHUNEK_AGENT".to_owned(), ctx.agent.clone()));
    if let Some(repo) = &ctx.repo {
        env.push(("POHUNEK_REPO".to_owned(), repo.display().to_string()));
    }
    if let Some(worktree) = &ctx.worktree {
        env.push((
            "POHUNEK_WORKTREE".to_owned(),
            worktree.display().to_string(),
        ));
    }
    if let Some(branch) = &ctx.branch {
        env.push(("POHUNEK_BRANCH".to_owned(), branch.clone()));
    }
    if let Some(base_branch) = &ctx.base_branch {
        env.push(("POHUNEK_BASE_BRANCH".to_owned(), base_branch.clone()));
    }
    if let Some(stop_reason) = ctx.stop_reason {
        env.push(("POHUNEK_STOP_REASON".to_owned(), stop_reason.to_owned()));
    }
    if let Some(activity) = ctx.activity {
        env.push(("POHUNEK_ACTIVITY".to_owned(), activity.to_owned()));
    }
    env
}

/// Resolve a hook's in-repo script path: `<in_repo_dir>/.pohunek/hooks/<event>`,
/// or — for `post-create` only — the legacy `.pohunek/setup` fallback (never both).
fn resolve_in_repo_hook(event: HookEvent, in_repo_dir: &Path) -> Option<PathBuf> {
    let hook = in_repo_dir.join(".pohunek/hooks").join(event.as_env());
    if hook.is_file() {
        return Some(hook);
    }
    if event == HookEvent::PostCreate {
        let legacy = in_repo_dir.join(SETUP_SCRIPT_REL);
        if legacy.is_file() {
            return Some(legacy);
        }
    }
    None
}

/// Run the lifecycle hooks for `event`, composing the **host-global layer first,
/// then the in-repo layer** (each an independent, env-cleared spawn; a failure in
/// one is its own warning and does not stop the other). In-repo hooks are looked
/// up under `in_repo_dir`; the host-global layer under `<config_dir>/hooks/`.
///
/// Each hook runs with a **cleared environment** plus only the [`hook_env`]
/// allowlist (closing the env-inheritance exfil gap: a hostile committed hook can
/// no longer read the daemon's `GITHUB_TOKEN`/`ANTHROPIC_API_KEY`/socket path), in
/// its own process group (so a timeout kills the whole subtree), with stdio sent
/// to `/dev/null` (output never feeds a daemon-side sink).
pub(crate) fn run_hook(
    event: HookEvent,
    in_repo_dir: &Path,
    ctx: &HookContext,
    timeout: Duration,
    config_dir: Option<&Path>,
    warnings: &mut Vec<SessionWarning>,
) {
    let env = hook_env(event, ctx);
    let cwd = &ctx.cwd_or(in_repo_dir);
    // Host-global layer first (composed, not overridden).
    if let Some(config_dir) = config_dir {
        let host_script = config_dir.join("hooks").join(event.as_env());
        if host_script.is_file() {
            run_one_hook(event, &host_script, cwd, &env, timeout, warnings);
        }
    }
    if let Some(in_repo_script) = resolve_in_repo_hook(event, in_repo_dir) {
        run_one_hook(event, &in_repo_script, cwd, &env, timeout, warnings);
    }
}

impl HookContext {
    /// The directory a hook runs in: the bound worktree when present (post-create /
    /// pre-remove), else the supplied `in_repo_dir` (the repository).
    fn cwd_or<'a>(&'a self, in_repo_dir: &'a Path) -> &'a Path {
        self.worktree.as_deref().unwrap_or(in_repo_dir)
    }
}

/// Spawn one hook script with the full hook discipline (env-clear + allowlist +
/// process-group + timeout + `/dev/null`), recording any failure as a non-fatal
/// `Hook` warning. The warning detail carries only the event + a generic reason —
/// never the hook's output (which can contain secrets; see the security note on
/// the original setup-script path).
fn run_one_hook(
    event: HookEvent,
    script: &Path,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
    warnings: &mut Vec<SessionWarning>,
) {
    let mut builder = Command::new(SETUP_SCRIPT_INTERPRETER);
    builder
        .arg(script)
        .current_dir(cwd)
        // Load-bearing: clear the daemon's inherited env, then set only the
        // allowlist, so a hook can never exfiltrate the daemon's secrets.
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        builder.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // pgid := child pid, so a timeout can signal the whole group at once.
        builder.process_group(0);
    }

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(err) => {
            warnings.push(hook_warning(
                event,
                format!("failed to spawn {}: {err}", script.display()),
            ));
            return;
        }
    };

    match wait_with_timeout(&mut child, timeout) {
        SetupOutcome::Exited(status) if status.success() => {}
        SetupOutcome::Exited(status) => {
            debug!(script = %script.display(), %status, event = event.as_env(), "hook failed");
            warnings.push(hook_warning(
                event,
                format!("{} exited with status {status}", script.display()),
            ));
        }
        SetupOutcome::TimedOut => {
            warn!(
                script = %script.display(),
                event = event.as_env(),
                timeout_secs = timeout.as_secs(),
                "hook timed out; terminated"
            );
            warnings.push(hook_warning(
                event,
                format!(
                    "{} did not finish within {}s and was terminated",
                    script.display(),
                    timeout.as_secs()
                ),
            ));
        }
        SetupOutcome::WaitError(err) => {
            warnings.push(hook_warning(
                event,
                format!("failed to wait for {}: {err}", script.display()),
            ));
        }
    }
}

/// Build a non-fatal `Hook` warning naming the failing event; the detail never
/// carries the hook's output.
fn hook_warning(event: HookEvent, detail: String) -> SessionWarning {
    SessionWarning {
        kind: SessionWarningKind::Hook,
        message: format!(
            "The {} hook failed; the session proceeded without it.",
            event.as_env()
        ),
        detail: Some(detail),
    }
}

/// Poll `child` to completion, terminating it if `timeout` elapses first.
///
/// Runs on the (blocking) worktree-bind thread, so a simple `try_wait` poll loop
/// is appropriate and keeps the daemon dependency-free.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> SetupOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return SetupOutcome::Exited(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    terminate_setup_script(child);
                    return SetupOutcome::TimedOut;
                }
                thread::sleep(SETUP_SCRIPT_POLL_INTERVAL);
            }
            Err(err) => return SetupOutcome::WaitError(err),
        }
    }
}

/// Kill a timed-out setup script and reap it, leaving no zombie or runaway child.
///
/// On Unix the whole process group (created via `process_group(0)`) is signalled
/// so children the script forked die too; the direct child is then reaped. The
/// child is not yet reaped when this is called, so its pid — and thus the pgid —
/// cannot have been recycled, making the group-directed kill safe.
#[allow(unsafe_code)]
fn terminate_setup_script(child: &mut Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as libc::pid_t;
        // SAFETY: `kill(2)` is a plain syscall with no memory-safety contract. A
        // negative pid targets the process group created by `process_group(0)`;
        // `child` is not yet reaped here, so its pid (hence the pgid) cannot have
        // been recycled. An already-dead group yields `ESRCH`, which we ignore.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
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
/// synthetic status line. Credentials embedded in a remote URL are redacted: git
/// error output for a credentials-in-URL remote echoes the token verbatim, and
/// this message can be persisted (event log) or returned on the wire, so it must
/// never carry a secret.
fn output_failure_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    let message = if message.is_empty() {
        format!("git exited with status {}", output.status)
    } else {
        message
    };
    redact_url_credentials(&message)
}

/// Strip `scheme://userinfo@host` credentials from a message before it is
/// persisted to the event log or surfaced on the wire.
///
/// Git error output for a credentials-in-URL remote echoes the userinfo verbatim
/// — notably the `https://<token>@host` PAT form, which git does **not** redact —
/// so a captured failure message can carry a secret. This replaces the userinfo
/// component of every URL-shaped substring with `<redacted>`, leaving the scheme
/// and host intact. A URL without credentials (no `@` in the authority) is
/// unchanged.
///
/// **Scope (security boundary).** This redacts exactly the RFC 3986 `userinfo`
/// component (`user`, `user:password`, or a bare `token`, all before the `@`),
/// which is the *only* place native git carries a secret in a URL. Deliberately
/// out of scope, because git never authenticates through them:
/// - SCP-form `git@host:org/repo` — no `://`, and the `git@` is a username, not a
///   secret (SSH auth is key-based);
/// - query/fragment (`?token=…`, `#…`) — the authority ends at `?`/`#`, so they
///   are not touched; git does not pass credentials there.
///
/// A non-standard credential helper that smuggled a token into a query string
/// would fall outside this; nothing in pohunek does. See the redaction tests.
pub(crate) fn redact_url_credentials(message: &str) -> String {
    const SCHEME_SEP: &str = "://";
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(idx) = rest.find(SCHEME_SEP) {
        let after = idx + SCHEME_SEP.len();
        out.push_str(&rest[..after]);
        let authority = &rest[after..];
        // The authority runs until the first character that cannot belong to it
        // (path/query/fragment delimiter, whitespace, or a surrounding quote).
        let auth_end = authority
            .find(|c: char| matches!(c, '/' | '?' | '#' | '\'' | '"') || c.is_whitespace())
            .unwrap_or(authority.len());
        let auth = &authority[..auth_end];
        if let Some(at) = auth.rfind('@') {
            out.push_str("<redacted>");
            out.push_str(&auth[at..]);
        } else {
            out.push_str(auth);
        }
        rest = &authority[auth_end..];
    }
    out.push_str(rest);
    out
}

fn git_failure_message(args: &[&str], output: &std::process::Output) -> String {
    let base = output_failure_message(output);
    format!("git {}: {base}", args.join(" "))
}

/// Build a typed worktree error.
fn error(class: ErrorClass, code: &str, msg: String, recover: Option<String>) -> ProtocolError {
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
