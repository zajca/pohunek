//! Launch-target resolution (project / in-place / worktree) and PTY registration.

use super::{
    build_pty_command, debug, detect_at, event, launch_adapter_for, plan_initial_input_delivery,
    runtime_error, spawn_error_to_protocol, timestamp_now, warn, watch, AgentKind,
    CancellationToken, DetectedProject, InputRules, LaunchOpts, Manifest, PathBuf, ProjectRecord,
    ProtocolError, PtyCommand, PtyHandle, ResolvedAgent, ResumeSnapshot, SessionEntry, SessionId,
    SessionInfo, SessionNewParams, SessionRegistry, SessionState, SessionWarning, ShellCommand,
    StateSource, WorktreeRequest,
};

/// Everything needed to spawn and register one PTY-backed session, shared by
/// first launch (`create`) and resume (`resume_binding`).
#[derive(Debug)]
pub(super) struct PtySessionSpec {
    pub(super) id: SessionId,
    /// Resolved agent NAME (a host-profile name, or a bare base-kind name).
    pub(super) agent: String,
    /// Resolved base kind backing the agent (detection/resume/handshake env).
    pub(super) agent_base: AgentKind,
    /// Input-framing rules for this session (base-kind defaults, profile-overridden).
    pub(super) input_rules: InputRules,
    /// Frozen structural relaunch snapshot (C.4): launch program/args + the resolved
    /// resume template (`None` ⇒ not resumable). Persisted verbatim so a restart
    /// resumes with the launch-time shape even after the profile changes.
    pub(super) snapshot: ResumeSnapshot,
    /// Detection-manifest override (a profile's `manifest =`), threaded to the
    /// detector at spawn. `None` ⇒ inherit the base kind's manifest. Re-resolved by
    /// agent name on the resume path (never persisted).
    pub(super) manifest_override: Option<Manifest>,
    pub(super) cwd: PathBuf,
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) command: PtyCommand,
    /// Native id when relaunching a captured session (`None` on first launch).
    pub(super) native_session_id: Option<String>,
    /// Native transcript path when relaunching a path-resuming captured session.
    pub(super) native_session_path: Option<String>,
    /// Project this session belongs to (derived id), when one was resolved.
    pub(super) project_id: Option<String>,
    /// Whether the session's checkout is a linked worktree (`None` if no git).
    pub(super) is_linked_worktree: Option<bool>,
    /// Source repository, when the session is bound to a worktree.
    pub(super) repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree.
    pub(super) branch: Option<String>,
    /// Bound worktree path (equal to `cwd` for worktree sessions).
    pub(super) worktree_path: Option<PathBuf>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    pub(super) warnings: Vec<SessionWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LaunchCommandPlan {
    pub(super) command: PtyCommand,
    pub(super) pending_initial_input: Option<String>,
}

/// The resolved launch target for a `session.new`: where the agent runs and the
/// project/worktree metadata to stamp on the session (see
/// [`SessionRegistry::resolve_target`]).
#[derive(Debug)]
pub(super) struct TargetResolution {
    /// Directory the agent is launched in (an in-place checkout or a worktree).
    pub(super) launch_cwd: PathBuf,
    /// Source repository, set for a worktree session.
    pub(super) repo: Option<PathBuf>,
    /// Branch checked out in the bound worktree, set for a worktree session.
    pub(super) branch: Option<String>,
    /// Bound worktree path, set for a worktree session.
    pub(super) worktree_path: Option<PathBuf>,
    /// Project the session belongs to (derived id), when one resolved.
    pub(super) project_id: Option<String>,
    /// Whether the checkout is a linked worktree (`None` if no git identity).
    pub(super) is_linked_worktree: Option<bool>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    pub(super) warnings: Vec<SessionWarning>,
    /// Whether a worktree was actually bound (drives launch-failure rollback).
    pub(super) worktree_bound: bool,
}

impl SessionRegistry {
    /// Roll back a worktree bound earlier in [`Self::create`] when the session
    /// then fails to launch, removing the checkout (and its binding) so the
    /// branch is freed for a retry. Best-effort and non-fatal: a rollback failure
    /// is logged and never masks the original launch error. A no-op when worktree
    /// binding is not configured.
    pub(super) async fn cleanup_bound_worktree(&self, id: &SessionId) {
        let Some(manager) = self.inner.worktree.clone() else {
            return;
        };
        let session_id = id.0.clone();
        match tokio::task::spawn_blocking(move || {
            // Remove-hook warnings on the rollback path are logged (the launch error
            // is what the caller surfaces).
            let mut hook_warnings = Vec::new();
            let result = manager.cleanup_session(&session_id, &mut hook_warnings);
            for warning in &hook_warnings {
                warn!(
                    session_id = %session_id,
                    warning = %warning.message,
                    detail = ?warning.detail,
                    "remove hook warning during worktree rollback"
                );
            }
            result
        })
        .await
        {
            Ok(Ok(removed)) => {
                if removed > 0 {
                    debug!(
                        session_id = %id.0,
                        removed,
                        "rolled back worktree after a failed launch"
                    );
                }
            }
            Ok(Err(err)) => warn!(
                session_id = %id.0,
                error = %err,
                "failed to roll back worktree after a failed launch"
            ),
            Err(_) => warn!(
                session_id = %id.0,
                "worktree rollback task panicked"
            ),
        }
    }

    /// Resolve the session's target: the project it belongs to and where the
    /// agent runs (in-place checkout vs a freshly bound worktree), per Decisions
    /// 1 & 3. Runs the blocking git/store work on blocking threads.
    pub(super) async fn resolve_target(
        &self,
        id: &SessionId,
        params: &SessionNewParams,
        fallback_cwd: PathBuf,
    ) -> Result<TargetResolution, ProtocolError> {
        // `base_branch` only branches a worktree; it is meaningless in-place.
        if params.base_branch.is_some() && params.branch.is_none() {
            return Err(ProtocolError::bad_request(
                "session.new base_branch requires branch",
            ));
        }

        // Phase 1: resolve the project (by reference, else by detecting a path).
        let (project, detected) = self.resolve_project(params).await?;

        // Phase 2: isolation (Decision 3).
        let Some(branch) = params.branch.clone() else {
            // A bare project has no working tree, so an in-place agent would land
            // in the bare git dir (objects/refs, no files) — useless. Refuse and
            // steer to `--branch`, which takes the worktree path below (a worktree
            // can be added off a bare repo). Detection auto-registers a bare repo
            // with `is_bare`, and a `--project` reference carries it too, so this
            // one check covers both ways a bare project reaches an in-place start.
            if project.as_ref().is_some_and(|record| record.is_bare) {
                return Err(ProtocolError::bad_request(
                    "cannot start an in-place session in a bare repository; \
                     use --branch to create a worktree",
                ));
            }
            return Ok(Self::in_place_target(project, detected, fallback_cwd));
        };

        // Worktree-per-session. The source repo is an explicit `--repo`, else the
        // resolved project's main checkout; the base is `--base-branch`, else the
        // project's configured default (`None` ⇒ the repo's HEAD at creation). The
        // project id is stamped onto both the session and the worktree binding.
        let project_id = project.as_ref().map(ProjectRecord::id);
        let repo = params
            .repo
            .clone()
            .or_else(|| project.as_ref().map(|record| record.repo_root.clone()))
            .ok_or_else(|| {
                ProtocolError::bad_request(
                    "session.new branch requires --repo or a resolvable --project",
                )
            })?;
        let base_branch = params
            .base_branch
            .clone()
            .or_else(|| project.as_ref().and_then(|r| r.default_base_branch.clone()));
        let bound = self
            .bind_worktree(
                &id.0,
                repo,
                branch,
                base_branch,
                project_id.clone(),
                &params.agent,
            )
            .await?;
        Ok(TargetResolution {
            launch_cwd: bound.path.clone(),
            repo: Some(bound.repository),
            branch: Some(bound.branch),
            worktree_path: Some(bound.path),
            project_id,
            is_linked_worktree: Some(true),
            warnings: bound.warnings,
            worktree_bound: true,
        })
    }

    /// Build the in-place (no-worktree) target: run the agent in the project's
    /// checkout as-is (Decision 3), or in `fallback_cwd` when no project resolved.
    ///
    /// A bare project never reaches here: [`Self::resolve_target`] refuses an
    /// in-place start on a bare repo (no working tree to run in) and steers the
    /// caller to `--branch`. So every project passed in has a real checkout.
    fn in_place_target(
        project: Option<ProjectRecord>,
        detected: Option<DetectedProject>,
        fallback_cwd: PathBuf,
    ) -> TargetResolution {
        let (launch_cwd, project_id, is_linked_worktree) = match (project, detected) {
            // Detected from a path: launch in this work tree's root.
            (Some(record), Some(detected)) => (
                detected.checkout_path,
                Some(record.id()),
                Some(detected.is_linked_worktree),
            ),
            // Resolved by `--project`: the in-place checkout is its main checkout.
            (Some(record), None) => {
                let id = record.id();
                (record.repo_root, Some(id), Some(false))
            }
            // No project: a plain shell in the fallback cwd (today's behavior).
            (None, _) => (fallback_cwd, None, None),
        };
        TargetResolution {
            launch_cwd,
            repo: None,
            branch: None,
            worktree_path: None,
            project_id,
            is_linked_worktree,
            warnings: Vec::new(),
            worktree_bound: false,
        }
    }

    /// Resolve the project this session belongs to, doing the blocking git
    /// detection + store I/O on a blocking thread.
    ///
    /// Order (Decision 1): a `--project <id|label>` reference resolves against the
    /// store (bumping its `last_used_at`); otherwise detect at the explicit
    /// `--repo` path, else — for a local session — the CLI's own `--cwd`,
    /// auto-registering the result. `--project` and `--repo` are mutually exclusive
    /// (rejected earlier by [`validate_new_params`]). An **explicit** `--repo` that
    /// is not a git work tree is an error (no silent fallback to a different dir),
    /// whereas an **implicit** non-git `--cwd` is the normal plain-shell case.
    /// Returns the record and, when detection ran, the [`DetectedProject`] (the
    /// in-place path needs its `checkout_path`/`is_linked_worktree`).
    #[expect(
        clippy::map_err_ignore,
        reason = "spawn_blocking JoinError has no meaningful source to surface in ProtocolError"
    )]
    async fn resolve_project(
        &self,
        params: &SessionNewParams,
    ) -> Result<(Option<ProjectRecord>, Option<DetectedProject>), ProtocolError> {
        let Some(projects) = self.inner.projects.clone() else {
            // No project subsystem (store unconfigured, e.g. some unit tests): a
            // `--project` reference cannot be honored; otherwise there is simply no
            // project, and worktree binding via `--repo`/`--branch` still works.
            if params.project.is_some() {
                return Err(runtime_error(
                    "projects_not_configured",
                    "the daemon is not configured for projects (no metadata store)",
                ));
            }
            return Ok((None, None));
        };
        let reference = params.project.clone();
        let repo = params.repo.clone();
        let cwd = params.cwd.clone();
        tokio::task::spawn_blocking(move || -> Result<_, ProtocolError> {
            // 1. Reference resolves against the store; a session start bumps recency.
            if let Some(reference) = reference {
                let record = projects.resolve(&reference)?;
                let record = projects.touch(&record.git_common_dir)?.unwrap_or(record);
                return Ok((Some(record), None));
            }
            // 2. Explicit --repo: must be a git work tree, else error — never
            // silently launch somewhere else (no-silent-defaults).
            if let Some(repo) = repo {
                let detected =
                    detect_at(&repo)?.ok_or_else(|| crate::project::not_a_git_repo(&repo))?;
                let record = projects.register(&detected, false)?;
                return Ok((Some(record), Some(detected)));
            }
            // 3. Implicit --cwd (local): a non-git cwd is the normal plain shell.
            let Some(cwd) = cwd else {
                return Ok((None, None));
            };
            let Some(detected) = detect_at(&cwd)? else {
                return Ok((None, None));
            };
            let record = projects.register(&detected, false)?;
            Ok((Some(record), Some(detected)))
        })
        .await
        .map_err(|_| runtime_error("project_resolve_failed", "project resolution task panicked"))?
    }

    /// Bind (or reuse) a worktree for `(session, repo, branch)` on a blocking
    /// thread. Errors when worktree binding is not configured.
    #[expect(
        clippy::map_err_ignore,
        reason = "spawn_blocking JoinError has no meaningful source to surface in ProtocolError"
    )]
    async fn bind_worktree(
        &self,
        session_id: &str,
        repo: PathBuf,
        branch: String,
        base_branch: Option<String>,
        project_id: Option<String>,
        agent: &str,
    ) -> Result<crate::worktree::WorktreeBound, ProtocolError> {
        let Some(manager) = self.inner.worktree.clone() else {
            return Err(runtime_error(
                "worktree_not_configured",
                "the daemon is not configured for worktree binding",
            ));
        };
        let request = WorktreeRequest {
            session_id: session_id.to_owned(),
            repo,
            branch,
            base_branch,
            project_id,
            agent: agent.to_owned(),
        };
        tokio::task::spawn_blocking(move || manager.bind(&request))
            .await
            .map_err(|_| runtime_error("worktree_bind_failed", "worktree bind task panicked"))?
    }

    /// Spawn a PTY for `spec.command`, register the session, and start its
    /// detector and exit watcher. Shared by `create` (first launch) and
    /// `resume_binding` (relaunch after a daemon restart).
    #[expect(
        clippy::map_err_ignore,
        reason = "spawn_blocking JoinError has no meaningful source to surface in ProtocolError"
    )]
    pub(super) async fn register_pty_session(
        &self,
        spec: PtySessionSpec,
    ) -> Result<SessionInfo, ProtocolError> {
        let PtySessionSpec {
            id,
            agent,
            agent_base,
            input_rules,
            snapshot,
            manifest_override,
            cwd,
            cols,
            rows,
            command,
            native_session_id,
            native_session_path,
            project_id,
            is_linked_worktree,
            repo,
            branch,
            worktree_path,
            warnings,
        } = spec;

        let history_limit_bytes = self.inner.config.output_history_limit_bytes;
        // Keep the program name for diagnostics: a spawn failure should name what
        // could not be launched (see `spawn_error_to_protocol`).
        let program = command.program.clone();
        let pty =
            tokio::task::spawn_blocking(move || PtyHandle::spawn(command, history_limit_bytes))
                .await
                .map_err(|_| runtime_error("spawn_failed", "PTY spawn task panicked"))?
                .map_err(|err| spawn_error_to_protocol(err, &program))?;
        let detector_output = pty.subscribe_output();
        let detector_cancel = CancellationToken::new();
        let (detector_resize, detector_resize_rx) = watch::channel((rows, cols));

        let now = timestamp_now();
        let info = SessionInfo {
            id: id.clone(),
            agent,
            agent_base,
            cwd,
            pid: pty.pid(),
            cols,
            rows,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id,
            native_session_path,
            project_id,
            // Denormalized for display, resolved fresh at `session.list` time.
            project_label: None,
            is_linked_worktree,
            repo,
            branch,
            worktree_path,
            warnings,
            created_at: now.clone(),
            updated_at: now,
            exit_code: None,
        };

        {
            let mut sessions = self.inner.sessions.lock().await;
            sessions.insert(
                id.clone(),
                SessionEntry {
                    info: info.clone(),
                    pty: pty.clone(),
                    detector_cancel: detector_cancel.clone(),
                    detector_resize,
                    stopping: false,
                    input_rules,
                    snapshot,
                },
            )
        };

        self.emit(event::SESSION_CREATED, &info);
        self.spawn_detector(
            id.clone(),
            agent_base,
            manifest_override,
            detector_output,
            (rows, cols),
            detector_cancel,
            detector_resize_rx,
        );
        self.spawn_exit_watcher(id, pty);
        Ok(info)
    }
}

pub(super) fn build_launch_command(
    resolved: &ResolvedAgent,
    shell_command: &ShellCommand,
    cwd: PathBuf,
    cols: u16,
    rows: u16,
    env_extra: Vec<(String, String)>,
    initial_input: Option<String>,
) -> Result<LaunchCommandPlan, ProtocolError> {
    // Shell carries no agent-hook handshake, but it does carry the universal
    // `POHUNEK_SESSION_ID` marker (see `session_pty_env`) so a `pohunek attach`
    // launched inside it is still caught as a self-feeding loop.
    let opts = LaunchOpts {
        cwd,
        cols,
        rows,
        env_extra,
    };
    let command = match &resolved.profile {
        // A host profile overrides the launch program/args; build via the shared
        // PATH-resolving primitive (the same one the base adapters use).
        Some(profile) => build_pty_command(&profile.program, profile.args.clone(), &opts)?,
        // A bare base kind launches exactly as the compiled adapter.
        None => launch_adapter_for(resolved.base, shell_command).launch(&opts)?,
    };
    Ok(plan_initial_input_delivery(
        resolved,
        command,
        initial_input,
    ))
}
