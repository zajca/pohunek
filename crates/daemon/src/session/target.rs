//! Launch-target resolution (project / in-place / worktree) and PTY registration.

use std::collections::{BTreeMap, VecDeque};

use pohunek_worker_protocol::{
    read_frame, Dimensions, FrameKind, Initialize, InitializeLimits,
    LaunchIdentity as WorkerLaunchIdentity, SecretEnv, SessionId as WorkerSessionId, StopPolicy,
    StreamId, StreamMode, TransactionId, Version,
};

use super::{
    build_pty_command, debug, detect_at, event, launch_adapter_for, plan_initial_input_delivery,
    runtime_error, timestamp_now, warn, watch, AgentKind, Arc, CancellationToken, CwdSource,
    DesiredState, DetectedProject, DetectorConfig, DetectorScope, InputRules, LaunchCommand,
    LaunchOpts, Manifest, Mutex, Notify, Ordering, PathBuf, ProjectRecord, ProtocolError,
    ResolvedAgent, ResumeBinding, ResumeSnapshot, RuntimeHandle, RuntimeRecord, RuntimeState,
    RuntimeWatchIdentity, SessionEntry, SessionId, SessionInfo, SessionNewParams, SessionRecord,
    SessionRefKind, SessionRegistry, SessionRuntime, SessionState, SessionTransaction,
    SessionWarning, ShellCommand, StateSource, TransactionKind, Worker, WorkerLaunchMode,
    WorktreeRequest, DEFAULT_WORKER_SUBSCRIBER_BYTES, DEFAULT_WORKER_TERMINAL_RETENTION,
    DEFAULT_WORKER_WRITE_DEDUP_ENTRIES, SESSION_RECORD_SCHEMA_VERSION, WORKER_CONNECT_RETRY,
};
use crate::store::StoredInputRules;

/// Everything needed to spawn and register one PTY-backed session, shared by
/// first launch (`create`) and resume (`resume_binding`).
#[derive(Debug)]
pub(super) struct PtySessionSpec {
    pub(super) id: SessionId,
    /// Lifecycle operation that owns this runtime launch.
    pub(super) registration: PtyRegistration,
    /// Owner-set display name, frozen at creation and restored on resume. `None`
    /// shows the session by id.
    pub(super) name: Option<String>,
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
    pub(super) command: LaunchCommand,
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
    /// Owner-controlled metadata attached to the session.
    pub(super) metadata: BTreeMap<String, String>,
    /// Non-fatal worktree-setup warnings to surface on the session.
    pub(super) warnings: Vec<SessionWarning>,
}

fn next_runtime_generation(
    registration: &PtyRegistration,
) -> Result<protocol::RuntimeGeneration, ProtocolError> {
    match registration {
        PtyRegistration::Create => Ok(protocol::RuntimeGeneration::new(1)),
        PtyRegistration::Recover {
            previous_runtime_generation,
            ..
        } => previous_runtime_generation
            .get()
            .checked_add(1)
            .map(protocol::RuntimeGeneration::new)
            .ok_or_else(|| {
                ProtocolError::new(
                    protocol::ErrorClass::Runtime,
                    "runtime_generation_exhausted",
                    "session runtime generation counter is exhausted",
                    Some("create a new logical session instead of resuming this one".to_owned()),
                )
            }),
    }
}

/// Durable lifecycle context for a PTY runtime launch.
#[derive(Debug, Clone)]
pub(super) enum PtyRegistration {
    /// First runtime of a newly-created logical session.
    Create,
    /// Explicit provider-native recovery of an existing logical session.
    Recover {
        /// Stable transaction identifier persisted before worker replacement.
        transaction_id: String,
        /// Worker generation being replaced, when known.
        previous_worker_id: Option<String>,
        /// Runtime generation being replaced, when known.
        previous_runtime_id: Option<String>,
        /// Monotonic generation being replaced.
        previous_runtime_generation: protocol::RuntimeGeneration,
        /// Original logical-session creation time.
        created_at: String,
        /// Cancels reconnect attempts owned by the superseded runtime.
        runtime_watch_cancel: CancellationToken,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LaunchCommandPlan {
    pub(super) command: LaunchCommand,
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
        .map_err(|_join_error| {
            runtime_error("project_resolve_failed", "project resolution task panicked")
        })?
    }

    /// Bind (or reuse) a worktree for `(session, repo, branch)` on a blocking
    /// thread. Errors when worktree binding is not configured.
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
            .map_err(|_join_error| {
                runtime_error("worktree_bind_failed", "worktree bind task panicked")
            })?
    }

    /// Spawn a PTY for `spec.command`, register the session, and start its
    /// detector and exit watcher. Shared by `create` (first launch) and
    /// `resume_binding` (relaunch after a daemon restart).
    #[expect(
        clippy::too_many_lines,
        reason = "session registration assembles one protocol snapshot plus task handles"
    )]
    pub(super) async fn register_pty_session(
        &self,
        spec: PtySessionSpec,
    ) -> Result<SessionInfo, ProtocolError> {
        let PtySessionSpec {
            id,
            registration,
            name,
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
            metadata,
            warnings,
        } = spec;

        let created_at = match &registration {
            PtyRegistration::Create => timestamp_now(),
            PtyRegistration::Recover { created_at, .. } => created_at.clone(),
        };
        let capabilities = protocol::SessionCapabilities {
            resume: snapshot.resume.is_some(),
            fork: snapshot.fork.is_some(),
        };
        let runtime_generation = next_runtime_generation(&registration)?;
        let (
            transaction_id,
            transaction_kind,
            previous_worker_id,
            previous_runtime_id,
            replace_worker,
        ) = match &registration {
            PtyRegistration::Create => (
                format!("create-{}", id.0),
                TransactionKind::Create,
                None,
                None,
                false,
            ),
            PtyRegistration::Recover {
                transaction_id,
                previous_worker_id,
                previous_runtime_id,
                ..
            } => (
                transaction_id.clone(),
                TransactionKind::Recover,
                previous_worker_id.clone(),
                previous_runtime_id.clone(),
                true,
            ),
        };
        let preparing_runtime = SessionRuntime {
            state: RuntimeState::Starting,
            runtime_generation,
            worker_id: None,
            runtime_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        };
        let preparing_info = SessionInfo {
            id: id.clone(),
            external: Some(false),
            capabilities,
            name: name.clone(),
            agent: agent.clone(),
            agent_base: agent_base.clone(),
            cwd: cwd.clone(),
            cwd_source: Some(CwdSource::Launch),
            pid: 0,
            runtime: Some(preparing_runtime),
            cols,
            rows,
            state: SessionState::Starting,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: native_session_id.clone(),
            native_session_path: native_session_path.clone(),
            project_id: project_id.clone(),
            project_label: None,
            is_linked_worktree,
            repo: repo.clone(),
            branch: branch.clone(),
            worktree_path: worktree_path.clone(),
            metadata: metadata.clone(),
            warnings: warnings.clone(),
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            exit_code: None,
        };
        self.write_session_record(SessionRecord {
            schema_version: SESSION_RECORD_SCHEMA_VERSION,
            session_id: id.0.clone(),
            desired_state: DesiredState::Running,
            transaction: Some(SessionTransaction {
                id: transaction_id.clone(),
                kind: transaction_kind,
                phase: "preparing".to_owned(),
                previous_worker_id: previous_worker_id.clone(),
                previous_runtime_id: previous_runtime_id.clone(),
            }),
            info: preparing_info,
            native_identity_ordering: None,
            recovery: Some(ResumeBinding {
                session_id: id.0.clone(),
                name: name.clone(),
                agent: agent.clone(),
                agent_base: agent_base.clone(),
                cwd: cwd.clone(),
                cols,
                rows,
                native_session_id: native_session_id.clone(),
                native_session_path: native_session_path.clone(),
                project_id: project_id.clone(),
                is_linked_worktree,
                metadata: metadata.clone(),
                program: snapshot.program.clone(),
                args: snapshot.args.clone(),
                input_rules: StoredInputRules::from(input_rules),
                resume_mode: snapshot.resume.map(|template| template.mode),
                ref_kind: snapshot.resume.map(|template| template.ref_kind),
                resumable: snapshot.resume.is_some(),
                fork_mode: snapshot.fork.map(|template| template.mode),
                fork_resume_mode: snapshot.fork.map(|template| template.resume.mode),
                fork_ref_kind: snapshot.fork.map(|template| template.resume.ref_kind),
                forkable: snapshot.fork.is_some(),
            }),
            runtime: RuntimeRecord {
                state: RuntimeState::Starting,
                worker_id: None,
                runtime_id: None,
                unit_name: Some(format!("pohunek-session@{}.service", id.0)),
                reason: None,
            },
        })
        .await?;
        if let PtyRegistration::Recover {
            runtime_watch_cancel,
            ..
        } = &registration
        {
            runtime_watch_cancel.cancel();
            tokio::task::yield_now().await;
        }

        let started = match self
            .start_runtime(
                &id,
                &agent,
                agent_base.clone(),
                snapshot.native_ref_kind(),
                command,
                &transaction_id,
                replace_worker,
                previous_worker_id.as_deref(),
                runtime_generation,
            )
            .await
        {
            Ok(started) => started,
            Err(error) => {
                if matches!(registration, PtyRegistration::Create) {
                    self.delete_session_record(&id).await?;
                }
                return Err(error);
            }
        };
        let detector_cancel = CancellationToken::new();
        let procwatch_cancel = CancellationToken::new();
        let runtime_watch_cancel = CancellationToken::new();
        let procwatch_rescan = Arc::new(Notify::new());
        let (detector_resize, detector_resize_rx) = watch::channel((rows, cols));
        let default_detector_config = DetectorConfig::for_profile(&agent_base, manifest_override);
        let (detector_config, detector_config_rx) = watch::channel(default_detector_config.clone());
        let root_pid = started.root_pid;

        let now = timestamp_now();
        let info = SessionInfo {
            id: id.clone(),
            external: Some(false),
            capabilities,
            name,
            agent,
            agent_base,
            cwd,
            cwd_source: Some(CwdSource::Launch),
            pid: started.root_pid,
            runtime: started.runtime_info,
            cols,
            rows,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id,
            native_session_path,
            project_id,
            // Denormalized for display, resolved fresh at `session.list` time.
            project_label: None,
            is_linked_worktree,
            repo,
            branch,
            worktree_path,
            metadata,
            warnings,
            created_at,
            updated_at: now,
            exit_code: None,
        };

        let entry = SessionEntry {
            info: info.clone(),
            activity_revision: 0,
            activity_evidence: VecDeque::new(),
            input_gate: Arc::new(Mutex::new(())),
            runtime: started.handle.clone(),
            desired_state: DesiredState::Running,
            detector_cancel: detector_cancel.clone(),
            detector_resize,
            detector_config,
            default_detector_config,
            procwatch_cancel: procwatch_cancel.clone(),
            runtime_watch_cancel: runtime_watch_cancel.clone(),
            procwatch_rescan: Arc::clone(&procwatch_rescan),
            stopping: false,
            input_rules,
            snapshot,
            active_agent: None,
            last_agent_report: None,
            last_native_report: None,
            observed_agents: Vec::new(),
        };
        if let Err(error) = self.commit_session_entry(&id, entry).await {
            self.stop_uncommitted_runtime(&id, &started.handle).await;
            return Err(error);
        }
        match registration {
            PtyRegistration::Create => self.emit(event::SESSION_CREATED, &info),
            PtyRegistration::Recover {
                previous_runtime_id,
                ..
            } => self.emit_native_recovered(&info, previous_runtime_id),
        }
        let expected = RuntimeWatchIdentity::from_info(&info)
            .expect("committed live runtime has a complete watcher identity");
        self.spawn_detector(
            DetectorScope {
                id: id.clone(),
                runtime: expected.clone(),
            },
            started.detector_output,
            (rows, cols),
            detector_cancel,
            detector_resize_rx,
            detector_config_rx,
        );
        self.spawn_procwatch(id.clone(), root_pid, procwatch_cancel, procwatch_rescan);
        match started.handle {
            RuntimeHandle::Worker(worker) => {
                self.spawn_worker_exit_watcher(id, worker, expected, runtime_watch_cancel);
            }
            RuntimeHandle::Unavailable(state) => {
                return Err(super::unavailable_runtime_error(&id, state));
            }
        }
        Ok(info)
    }

    pub(super) async fn commit_session_entry(
        &self,
        id: &SessionId,
        entry: SessionEntry,
    ) -> Result<(), ProtocolError> {
        let committed = Self::session_record(id, &entry, DesiredState::Running, None);
        self.write_session_record(committed).await?;
        self.inner.sessions.lock().await.insert(id.clone(), entry);
        Ok(())
    }

    async fn stop_uncommitted_runtime(&self, id: &SessionId, runtime: &RuntimeHandle) {
        let RuntimeHandle::Worker(worker) = runtime else {
            return;
        };
        let transaction = format!(
            "rollback-{}",
            self.inner.next_write_id.fetch_add(1, Ordering::Relaxed)
        );
        let Ok(transaction) = TransactionId::new(transaction) else {
            return;
        };
        if let Err(error) = worker.stop(transaction).await {
            warn!(
                session_id = %id.0,
                error = %error,
                "failed to stop an uncommitted session runtime"
            );
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "runtime activation needs the persisted launch and replacement identity fields"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "systemd activation, private negotiation, initialization, and stream opening are one transaction"
    )]
    async fn start_runtime(
        &self,
        id: &SessionId,
        agent: &str,
        agent_base: AgentKind,
        reference_kind: Option<SessionRefKind>,
        command: LaunchCommand,
        transaction_id: &str,
        replace_worker: bool,
        previous_worker_id: Option<&str>,
        runtime_generation: protocol::RuntimeGeneration,
    ) -> Result<StartedRuntime, ProtocolError> {
        let Some(worker_root) = self.inner.config.worker_runtime_root.as_ref() else {
            return Err(runtime_error(
                "worker_backend_required",
                "session launch requires a durable worker runtime root",
            ));
        };
        let launch_mode = if replace_worker {
            WorkerLaunchMode::Replace
        } else {
            WorkerLaunchMode::Start
        };
        self.inner
            .launcher
            .launch(&id.0, launch_mode)
            .await
            .map_err(|error| {
                runtime_error(
                    "worker_manager_unavailable",
                    format!("failed to activate session worker {}: {error}", id.0),
                )
            })?;

        let socket_path = worker_root
            .join(&id.0)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        let deadline = tokio::time::Instant::now() + self.inner.config.worker_connect_deadline;
        let worker = loop {
            match Worker::connect(&socket_path, &id.0, self.daemon_instance_id()).await {
                Ok(worker) => {
                    let worker_id = worker.worker_id().await;
                    let still_previous = replace_worker
                        && previous_worker_id
                            .is_some_and(|previous| worker_id.as_str() == previous);
                    if !still_previous {
                        break worker;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(runtime_error(
                            "worker_replacement_failed",
                            format!(
                                "session worker {} did not advance to a new worker generation",
                                id.0
                            ),
                        ));
                    }
                    debug!(
                        session_id = %id.0,
                        "waiting for replacement worker generation"
                    );
                    tokio::time::sleep(WORKER_CONNECT_RETRY).await;
                }
                Err(error) if tokio::time::Instant::now() < deadline => {
                    debug!(
                        session_id = %id.0,
                        error = %error,
                        "worker bootstrap socket not ready yet"
                    );
                    tokio::time::sleep(WORKER_CONNECT_RETRY).await;
                }
                Err(error) => {
                    return Err(runtime_error(
                        "worker_connect_failed",
                        format!("failed to connect to session worker {}: {error}", id.0),
                    ));
                }
            }
        };

        let worker_id = worker.worker_id().await;
        let dimensions = Dimensions::new(command.cols, command.rows)
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let output_history_bytes = u64::try_from(self.inner.config.output_history_limit_bytes)
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let retention_ms = u64::try_from(DEFAULT_WORKER_TERMINAL_RETENTION.as_millis())
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let limits = InitializeLimits::new(
            output_history_bytes,
            DEFAULT_WORKER_SUBSCRIBER_BYTES,
            DEFAULT_WORKER_WRITE_DEDUP_ENTRIES,
            retention_ms,
        )
        .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let stop_grace_ms = u64::try_from(self.inner.config.stop_grace.as_millis())
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let stop_policy = StopPolicy::new(stop_grace_ms)
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let environment = SecretEnv::new(command.env.iter().cloned().collect())
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let transaction_id = TransactionId::new(transaction_id)
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let worker_session_id = WorkerSessionId::new(&id.0)
            .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))?;
        let runtime_id = worker
            .initialize(Initialize {
                session_id: worker_session_id,
                transaction_id,
                expected_worker_id: worker_id.clone(),
                launch: WorkerLaunchIdentity {
                    agent: agent.to_owned(),
                    agent_base: super::agent_kind_label(&agent_base).to_owned(),
                    reference_kind: reference_kind.map(|kind| match kind {
                        SessionRefKind::Id => "id".to_owned(),
                        SessionRefKind::Path => "path".to_owned(),
                    }),
                },
                executable: PathBuf::from(&command.program),
                arguments: command.args,
                cwd: command.cwd,
                dimensions,
                environment,
                limits,
                stop_policy,
                hook_protocol_version: Version::new(1)
                    .expect("worker hook protocol version is nonzero"),
                public_protocol_version: protocol::PROTOCOL_VERSION.get(),
            })
            .await
            .map_err(super::worker_error_to_protocol)?;
        let snapshot = worker
            .inspect()
            .await
            .map_err(super::worker_error_to_protocol)?;
        let child = snapshot.child_process.ok_or_else(|| {
            runtime_error(
                "worker_initialize_failed",
                format!("worker {} did not report a child process", id.0),
            )
        })?;
        let detector_output = open_detector_output(&worker, id).await?;
        let connected_at = timestamp_now();
        Ok(StartedRuntime {
            handle: RuntimeHandle::Worker(worker),
            detector_output,
            root_pid: child.pid,
            runtime_info: Some(SessionRuntime {
                state: RuntimeState::Live,
                runtime_generation,
                worker_id: Some(worker_id.to_string()),
                runtime_id: Some(runtime_id.to_string()),
                started_at: Some(connected_at.clone()),
                last_connected_at: Some(connected_at),
                loss_reason: None,
            }),
        })
    }
}

struct StartedRuntime {
    handle: RuntimeHandle,
    detector_output: tokio::sync::broadcast::Receiver<Vec<u8>>,
    root_pid: u32,
    runtime_info: Option<SessionRuntime>,
}

pub(super) async fn open_detector_output(
    worker: &Worker,
    id: &SessionId,
) -> Result<tokio::sync::broadcast::Receiver<Vec<u8>>, ProtocolError> {
    let stream_id = StreamId::new(format!("detector-{}", id.0))
        .map_err(|error| runtime_error("worker_detector_failed", error.to_string()))?;
    let data = worker
        .open_data(stream_id, StreamMode::Detector, None)
        .await
        .map_err(super::worker_error_to_protocol)?;
    let (output, receiver) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        let mut stream = data.stream;
        loop {
            match read_frame(&mut stream).await {
                Ok(Some(frame)) => {
                    let (header, payload) = frame.into_parts();
                    match header.kind {
                        FrameKind::Replay { .. }
                        | FrameKind::Output { .. }
                        | FrameKind::TerminalSnapshot { .. } => {
                            let _ = output.send(payload);
                        }
                        FrameKind::Gap { .. }
                        | FrameKind::InputAck { .. }
                        | FrameKind::Exit { .. }
                        | FrameKind::Error { .. }
                        | FrameKind::Close { .. } => {}
                        FrameKind::Open { .. }
                        | FrameKind::AttachReady { .. }
                        | FrameKind::ObservationStart { .. }
                        | FrameKind::Input { .. } => {
                            warn!("worker detector received an invalid server frame");
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    warn!(error = %error, "worker detector data stream failed");
                    break;
                }
            }
        }
    });
    Ok(receiver)
}

pub(super) fn build_launch_command(
    resolved: &ResolvedAgent,
    shell_command: &ShellCommand,
    opts: &LaunchOpts,
    initial_input: Option<String>,
) -> Result<LaunchCommandPlan, ProtocolError> {
    // Shell carries no agent-hook handshake, but it does carry the universal
    // `POHUNEK_SESSION_ID` marker (see `session_pty_env`) so a `pohunek attach`
    // launched inside it is still caught as a self-feeding loop.
    let command = match &resolved.profile {
        // A host profile overrides the launch program/args; build via the shared
        // PATH-resolving primitive (the same one the base adapters use). When the
        // options carry a validated program, that exact path bypasses resolution.
        Some(profile) => build_pty_command(&profile.program, profile.args.clone(), opts)?,
        // A bare base kind launches exactly as the compiled adapter.
        None => launch_adapter_for(&resolved.base, shell_command).launch(opts)?,
    };
    Ok(plan_initial_input_delivery(
        resolved,
        command,
        initial_input,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_generation_increment_fails_closed_at_u64_max() {
        let registration = PtyRegistration::Recover {
            transaction_id: "recover-overflow".to_owned(),
            previous_worker_id: None,
            previous_runtime_id: None,
            previous_runtime_generation: protocol::RuntimeGeneration::new(u64::MAX),
            created_at: "2026-08-04T00:00:00Z".to_owned(),
            runtime_watch_cancel: CancellationToken::new(),
        };

        let error = next_runtime_generation(&registration)
            .expect_err("runtime generation must never wrap or saturate");
        assert_eq!(error.code, "runtime_generation_exhausted");
    }
}
