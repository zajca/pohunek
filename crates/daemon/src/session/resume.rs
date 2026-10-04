//! Native recovery metadata and explicit provider-native relaunch.

use super::{
    agent_fork_unsupported, agent_not_resumable, fork_pty_command_from_launch, host,
    input_rules_for_agent, is_terminal, resume_pty_command_from_launch, runtime_error,
    session_not_found, validate_session_name, warn, LaunchOpts, NativeSessionLaunch, Ordering,
    PathBuf, ProtocolError, PtySessionSpec, ResumeBinding, SessionEntry, SessionForkParams,
    SessionId, SessionInfo, SessionRef, SessionRefKind, SessionRegistry, ValidatedLaunchProgram,
};

use std::io;
use std::sync::Arc;

use crate::agent::host::RuntimeDefinition;

/// Frozen structural relaunch snapshot for a session (Part C, C.4).
///
/// Set once at launch from the [`ResolvedAgent`] and persisted verbatim on every
/// resume-binding write, so a daemon restart relaunches with the original launch
/// program/args + resume mechanics even after the host profile is edited or
/// deleted. Deliberately holds **no env** — that is re-resolved by agent name at
/// resume (it may carry secrets, which never touch the store).
#[derive(Debug, Clone)]
pub(super) struct ResumeSnapshot {
    /// Launch program (the profile's `program` or the base kind's default).
    pub(super) program: String,
    /// Launch args (the profile's `args`; empty for a bare base kind).
    pub(super) args: Vec<String>,
    /// Resolved native-session launch spec; `None` ⇒ this session neither resumes
    /// nor forks natively.
    pub(super) native: Option<NativeSessionLaunch>,
    /// The runtime identity this launch shape was frozen with.
    pub(super) launch_binding: host::LaunchPin,
}

impl ResumeSnapshot {
    /// The snapshot of a session with no recorded launch shape.
    pub(super) fn empty() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            native: None,
            launch_binding: host::LaunchPin::Unpinned,
        }
    }

    /// Rebuild the snapshot frozen into a persisted binding.
    pub(super) fn from_binding(binding: &ResumeBinding) -> Self {
        Self {
            program: binding.program.clone(),
            args: binding.args.clone(),
            native: binding.native_launch.clone(),
            launch_binding: binding.launch_binding.clone(),
        }
    }

    /// Return the native reference kind required by resume or fork.
    pub(super) fn native_ref_kind(&self) -> Option<SessionRefKind> {
        self.native
            .as_ref()
            .map(NativeSessionLaunch::reference_kind)
    }
}

impl SessionRegistry {
    /// Make the persisted resume binding for `id` match the session's CURRENT
    /// in-memory state, serialized against every other persister.
    ///
    /// A live session that has captured a native id gets its binding upserted
    /// with the latest cwd/size; any other session (terminal, gone, or never
    /// captured) gets its binding removed. The whole snapshot-then-write is
    /// serialized by `persist_lock` and re-reads the session under the sessions
    /// lock, so when a resize and a native-id capture (or two resizes) race,
    /// whichever runs last reads the freshest state and writes it last — no
    /// stale size can win, and a session that went terminal in between is never
    /// resurrected (it re-reads as terminal and removes instead). Only the brief
    /// snapshot holds the sessions lock; the blocking store I/O runs under
    /// `persist_lock` alone. Best-effort: an unconfigured store or a failed
    /// write is non-fatal and only impairs legacy recovery metadata, surfaced
    /// via a warning. Durable logical session records use the fail-closed store
    /// path separately.
    pub(super) async fn persist_resume_binding(&self, id: &SessionId) {
        let Some(store) = &self.inner.store else {
            return;
        };
        let _persist = self.inner.persist_lock.lock().await;
        let desired = {
            let sessions = self.inner.sessions.lock().await;
            sessions.get(id).and_then(|entry| {
                if is_terminal(entry.info.state) {
                    return None;
                }
                entry.snapshot.native_ref_kind()?;
                // Resume/fork recovery becomes actionable once the agent reports
                // the native reference required by either frozen capability.
                if entry.info.native_session_id.is_none()
                    && entry.info.native_session_path.is_none()
                {
                    return None;
                }
                Some(Self::resume_binding_from_entry(id, entry))
            })
        };
        let store = Arc::clone(store);
        let session_id = id.0.clone();
        let result = tokio::task::spawn_blocking(move || match desired {
            Some(binding) => store.record_resume(&binding),
            None => store.remove_resume(&session_id),
        })
        .await
        .unwrap_or_else(join_error_to_io);
        if let Err(err) = result {
            warn!(
                session_id = %id.0,
                error = %err,
                "failed to persist resume binding"
            );
        }
    }

    /// Relaunch a terminal session from its in-memory resume metadata.
    ///
    /// A plain attach requires a live PTY. When a resumable agent exits normally,
    /// the daemon keeps the terminal session entry visible in memory with its
    /// captured native reference, so an explicit user action can relaunch it with
    /// the same pohunek session id. Lost durable logical sessions are also
    /// eligible. Daemon startup never invokes this operation; only an explicit
    /// `session.resume` request creates the new worker and runtime generation.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` for an unknown id,
    /// `session_runtime_not_recoverable` unless the runtime is terminal or lost,
    /// `not_resumable` when the entry lacks the native reference required by its
    /// frozen resume template, or any worker launch error from recovery.
    pub async fn resume(&self, id: &SessionId) -> Result<SessionInfo, ProtocolError> {
        self.ensure_not_external(id).await?;
        let guard = self.lock_lifecycle(id).await;
        let (binding, definition, registration) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            let binding = Self::resume_binding_from_entry(id, entry);
            // The runtime is resolved before the capability gate: a session whose
            // runtime is not installed reports that, and its binding stays as is.
            let definition = self.binding_definition(&binding)?;
            if !entry.info.capabilities.resume {
                return Err(agent_not_resumable(&entry.info.agent));
            }
            let runtime_state = entry.info.runtime.as_ref().map(|runtime| runtime.state);
            let eligible = matches!(
                runtime_state,
                Some(protocol::RuntimeState::Terminal | protocol::RuntimeState::Lost)
            ) || (runtime_state.is_none() && is_terminal(entry.info.state));
            if !eligible {
                let state = runtime_state.map_or_else(
                    || format!("{:?}", entry.info.state).to_lowercase(),
                    |state| format!("{state:?}").to_lowercase(),
                );
                return Err(runtime_error(
                    "session_runtime_not_recoverable",
                    format!(
                        "session {} runtime is {state}; native recovery requires terminal or lost",
                        id.0
                    ),
                ));
            }
            (
                binding,
                definition,
                super::target::PtyRegistration::Recover {
                    transaction_id: format!(
                        "recover-{}",
                        self.inner.next_write_id.fetch_add(1, Ordering::Relaxed)
                    ),
                    previous_worker_id: entry
                        .info
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.worker_id.clone()),
                    previous_worker_instance_id: entry
                        .info
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.runtime_id.clone()),
                    previous_job: entry.job.clone(),
                    previous_runtime_generation: entry
                        .info
                        .runtime
                        .as_ref()
                        .map_or(protocol::RuntimeGeneration::new(0), |runtime| {
                            runtime.runtime_generation
                        }),
                    created_at: entry.info.created_at.clone(),
                    runtime_watch_cancel: entry.runtime_watch_cancel.clone(),
                },
            )
        };
        let configured_program = binding_program(&binding, &definition);
        let validated_program = host::validate_launch_runtime(&definition, &configured_program)?;

        let info = match self
            .resume_binding_with_registration(binding, registration, validated_program, guard)
            .await
        {
            Ok(info) => info,
            Err(error) => {
                self.persist_failed_recovery_rollback(id).await;
                return Err(error);
            }
        };
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    /// Resolves the runtime a binding relaunches with and checks that it may
    /// serve the binding's frozen launch pin.
    ///
    /// A runtime that is not installed, or that no longer matches the pin,
    /// refuses the relaunch; the stored binding is left untouched so it
    /// resumes once the runtime is available again.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a value that is not a runtime id
    /// and `runtime_not_installed` for an unresolvable or mismatching runtime.
    fn binding_definition(
        &self,
        binding: &ResumeBinding,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let definition = self
            .inner
            .profiles
            .runtimes()
            .resolve_kind(&binding.agent_base)?;
        host::check_pin(&binding.launch_binding, definition)?;
        Ok(Arc::clone(definition))
    }

    async fn persist_failed_recovery_rollback(&self, id: &SessionId) {
        let restored = {
            let sessions = self.inner.sessions.lock().await;
            sessions
                .get(id)
                .map(|entry| Self::session_record(id, entry, entry.desired_state, None))
        };
        let Some(record) = restored else {
            return;
        };
        if let Err(store_error) = self.write_session_record(record).await {
            warn!(
                session_id = %id.0,
                error = %store_error,
                "failed to roll back native recovery transaction"
            );
        }
    }

    /// Fork a native agent conversation into a new pohunek session.
    ///
    /// # Errors
    ///
    /// Returns `migration_manifest_missing` while unmigrated legacy resume
    /// bindings exist, before anything is written, and otherwise the source
    /// lookup, launch-template, or launch error.
    pub async fn fork(&self, params: SessionForkParams) -> Result<SessionInfo, ProtocolError> {
        self.ensure_migration_settled()?;
        self.ensure_not_external(&params.session_id).await?;
        let (binding, repo, branch, worktree_path) = self.fork_source(&params.session_id).await?;
        let definition = self.binding_definition(&binding)?;

        // Fork is fail-closed: only a spec frozen into the binding can fork, never
        // the base kind's compiled spec.
        let launch = binding
            .native_launch
            .clone()
            .filter(NativeSessionLaunch::supports_fork)
            .ok_or_else(agent_fork_unsupported)?;
        let session_ref = session_ref_from_binding(launch.reference_kind(), &binding)?;

        let id = Self::allocate_session_id();
        self.ensure_worker_socket(&id)?;
        let has_snapshot = !binding.program.is_empty();
        let program = binding_program(&binding, &definition);
        let input_rules = if has_snapshot {
            binding.input_rules.to_input_rules(definition.input_rules())
        } else {
            input_rules_for_agent(&binding.agent_base, &self.inner.config)
        };

        let (profile_env, manifest_override) = match self
            .inner
            .profiles
            .resolve_agent(&binding.agent)
        {
            Ok(resolved) => resolved.profile.map_or((Vec::new(), None), |profile| {
                (profile.env, profile.manifest)
            }),
            Err(err) => {
                warn!(
                    session_id = %binding.session_id,
                    agent = %binding.agent,
                    error = %err,
                    "agent profile no longer resolves at fork; launching from the structural snapshot without profile env"
                );
                (Vec::new(), None)
            }
        };
        let mut env_extra = profile_env;
        env_extra.extend(self.session_pty_env(binding.agent_base.clone(), &id));
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: params.cols,
            rows: params.rows,
            env_extra,
            validated_program: None,
        };
        let command = fork_pty_command_from_launch(
            &program,
            binding.args.clone(),
            &launch,
            &session_ref,
            &opts,
        )?;
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            native: Some(launch),
            launch_binding: binding.launch_binding.clone(),
        };
        let guard = self.lock_lifecycle(&id).await;
        let info = self
            .register_pty_session(
                PtySessionSpec {
                    id,
                    registration: super::target::PtyRegistration::Create,
                    name: validate_session_name(params.name.as_deref())?,
                    agent: binding.agent,
                    agent_base: binding.agent_base.clone(),
                    input_rules,
                    snapshot,
                    manifest_override,
                    cwd: binding.cwd,
                    cols: params.cols,
                    rows: params.rows,
                    command,
                    native_session_id: binding.native_session_id,
                    native_session_path: binding.native_session_path,
                    project_id: binding.project_id,
                    is_linked_worktree: binding.is_linked_worktree,
                    repo,
                    branch,
                    worktree_path,
                    metadata: binding.metadata,
                    warnings: Vec::new(),
                    initial_input_pending: false,
                },
                guard,
            )
            .await?;
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    /// The resume binding and git context a fork of `id` launches from.
    async fn fork_source(
        &self,
        id: &SessionId,
    ) -> Result<
        (
            ResumeBinding,
            Option<PathBuf>,
            Option<String>,
            Option<PathBuf>,
        ),
        ProtocolError,
    > {
        let sessions = self.inner.sessions.lock().await;
        let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
        if !entry.info.capabilities.fork {
            return Err(agent_fork_unsupported());
        }
        Ok((
            Self::resume_binding_from_entry(id, entry),
            entry.info.repo.clone(),
            entry.info.branch.clone(),
            entry.info.worktree_path.clone(),
        ))
    }

    pub(super) fn resume_binding_from_entry(id: &SessionId, entry: &SessionEntry) -> ResumeBinding {
        ResumeBinding {
            session_id: id.0.clone(),
            name: entry.info.name.clone(),
            agent: entry.info.agent.clone(),
            agent_base: entry.info.agent_base.clone(),
            cwd: entry.info.cwd.clone(),
            cols: entry.info.cols,
            rows: entry.info.rows,
            native_session_id: entry.info.native_session_id.clone(),
            native_session_path: entry.info.native_session_path.clone(),
            // Capture the project context so resume restores it without
            // re-detecting (F5): a restart reads these back verbatim.
            project_id: entry.info.project_id.clone(),
            is_linked_worktree: entry.info.is_linked_worktree,
            metadata: entry.info.metadata.clone(),
            // Structural relaunch snapshot (C.4): copied verbatim from the frozen
            // entry snapshot on EVERY persist and on explicit resume, so neither a
            // resize re-persist nor a terminal relaunch can overwrite the
            // launch-time shape. `env` is intentionally absent — it is re-resolved
            // by agent name at resume (no secrets in store).
            program: entry.snapshot.program.clone(),
            args: entry.snapshot.args.clone(),
            input_rules: entry.input_rules.into(),
            native_launch: entry.snapshot.native.clone(),
            launch_binding: entry.snapshot.launch_binding.clone(),
        }
    }

    /// Relaunch one session from its stored resume binding, reusing its id.
    #[cfg(test)]
    pub(super) async fn resume_binding(
        &self,
        binding: ResumeBinding,
    ) -> Result<SessionInfo, ProtocolError> {
        let definition = self.binding_definition(&binding)?;
        let configured_program = binding_program(&binding, &definition);
        let validated_program = host::validate_launch_runtime(&definition, &configured_program)?;
        let id = SessionId(binding.session_id.clone());
        let guard = self.lock_lifecycle(&id).await;
        let registration = match self.load_durable_session_record(&id).await? {
            Some(record) => super::target::PtyRegistration::Recover {
                transaction_id: format!(
                    "recover-{}",
                    self.inner.next_write_id.fetch_add(1, Ordering::Relaxed)
                ),
                previous_job: super::Generation::from_record(&id.0, &record.runtime)?,
                previous_worker_id: record.runtime.worker_id,
                previous_worker_instance_id: record.runtime.worker_instance_id,
                previous_runtime_generation: record
                    .info
                    .runtime
                    .as_ref()
                    .map_or(protocol::RuntimeGeneration::new(0), |runtime| {
                        runtime.runtime_generation
                    }),
                created_at: record.info.created_at,
                runtime_watch_cancel: tokio_util::sync::CancellationToken::new(),
            },
            None => super::target::PtyRegistration::Create,
        };
        self.resume_binding_with_registration(binding, registration, validated_program, guard)
            .await
    }

    /// Relaunch one session under the supplied durable lifecycle operation.
    async fn resume_binding_with_registration(
        &self,
        binding: ResumeBinding,
        registration: super::target::PtyRegistration,
        validated_program: Option<ValidatedLaunchProgram>,
        guard: super::LifecycleGuard,
    ) -> Result<SessionInfo, ProtocolError> {
        // The resume mechanics come from the frozen structural snapshot (C.4). A
        // binding without a snapshot program falls back to the base kind's
        // compiled spec.
        let definition = self.binding_definition(&binding)?;
        let launch = binding_native_launch(&binding, &definition)
            .ok_or_else(|| agent_not_resumable(&binding.agent))?;
        // Build the native reference from the field the frozen reference kind
        // names, so a `path`-kind spec applies the absolute-path guard and an
        // `id`-kind spec the leading-dash guard (the documented asymmetry).
        let session_ref = session_ref_from_binding(launch.reference_kind(), &binding)?;

        let id = SessionId(binding.session_id.clone());

        // A legacy binding carries no snapshot program; fall back to the base kind's
        // default so it still relaunches. `program`/`input_rules` are frozen
        // structural fields — never re-resolved from the profile.
        let has_snapshot = !binding.program.is_empty();
        let program = binding_program(&binding, &definition);
        let input_rules = if has_snapshot {
            binding.input_rules.to_input_rules(definition.input_rules())
        } else {
            input_rules_for_agent(&binding.agent_base, &self.inner.config)
        };

        // Re-resolve the profile by NAME to recover its (possibly-secret) env + its
        // detection-manifest override — neither is ever persisted (C.4 no-secrets).
        // A deleted/renamed profile resumes from the frozen structural snapshot with
        // no profile env and a warning, never a failure.
        let (profile_env, manifest_override) = match self
            .inner
            .profiles
            .resolve_agent(&binding.agent)
        {
            Ok(resolved) => resolved.profile.map_or((Vec::new(), None), |profile| {
                (profile.env, profile.manifest)
            }),
            Err(err) => {
                warn!(
                    session_id = %binding.session_id,
                    agent = %binding.agent,
                    error = %err,
                    "agent profile no longer resolves at resume; relaunching from the structural snapshot without profile env"
                );
                (Vec::new(), None)
            }
        };
        // Profile env first, daemon handshake env appended last (POHUNEK_* wins).
        let mut env_extra = profile_env;
        env_extra.extend(self.session_pty_env(binding.agent_base.clone(), &id));
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: binding.cols,
            rows: binding.rows,
            env_extra,
            validated_program,
        };
        let command = resume_pty_command_from_launch(
            &program,
            binding.args.clone(),
            &launch,
            &session_ref,
            &opts,
        )?;
        // Re-freeze the structural snapshot for the resumed entry so a later resize
        // re-persist keeps the same launch-time shape.
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            native: Some(launch),
            launch_binding: binding.launch_binding.clone(),
        };
        // A resumed session relaunches in its recorded cwd, which already is the
        // worktree path for worktree sessions (the worktree persists on disk
        // across a daemon restart). With the unified store the session's worktree
        // metadata (repo/branch/worktree_path) is restored too, so inspect/list
        // show it again after a restart.
        let (repo, branch, worktree_path) =
            self.restore_worktree_metadata(&binding.session_id).await;
        // The project context was captured on the binding when it was persisted
        // (F5), so restore it directly — no git re-detection on the cwd at startup,
        // and a detection failure can no longer silently drop the metadata. An
        // older binding (pre-F5) carries `None`, leaving the resumed session
        // without project context until its next persist.
        let project_id = binding.project_id.clone();
        let is_linked_worktree = binding.is_linked_worktree;
        self.register_pty_session(
            PtySessionSpec {
                id,
                registration,
                name: binding.name,
                agent: binding.agent,
                agent_base: binding.agent_base.clone(),
                input_rules,
                snapshot,
                manifest_override,
                cwd: binding.cwd,
                cols: binding.cols,
                rows: binding.rows,
                command,
                native_session_id: binding.native_session_id,
                native_session_path: binding.native_session_path,
                project_id,
                is_linked_worktree,
                repo,
                branch,
                worktree_path,
                metadata: binding.metadata,
                warnings: Vec::new(),
                initial_input_pending: false,
            },
            guard,
        )
        .await
    }

    /// Look up a resumed session's worktree binding in the unified store and
    /// return its `(repo, branch, worktree_path)` so the restored session shows
    /// its worktree metadata again. Best-effort: a missing store, a read error,
    /// or no binding yields all-`None` — the session still resumes (its cwd is the
    /// worktree path either way); only the display metadata is absent.
    async fn restore_worktree_metadata(
        &self,
        session_id: &str,
    ) -> (Option<PathBuf>, Option<String>, Option<PathBuf>) {
        let Some(store) = &self.inner.store else {
            return (None, None, None);
        };
        let store = Arc::clone(store);
        let session_id_owned = session_id.to_owned();
        match tokio::task::spawn_blocking(move || {
            store.find_worktree_for_session(&session_id_owned)
        })
        .await
        .unwrap_or_else(join_error_to_io)
        {
            Ok(Some(binding)) => (
                Some(binding.repository),
                Some(binding.branch),
                Some(binding.path),
            ),
            Ok(None) => (None, None, None),
            Err(err) => {
                warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to read worktree metadata during resume"
                );
                (None, None, None)
            }
        }
    }
}

fn binding_program(binding: &ResumeBinding, definition: &RuntimeDefinition) -> String {
    if binding.program.is_empty() {
        definition.program().as_str().to_owned()
    } else {
        binding.program.clone()
    }
}

/// Return the native-session launch spec frozen into a persisted binding.
///
/// A binding written without a snapshot program carries no frozen spec and
/// resolves to its runtime definition's spec; a binding with a snapshot is
/// authoritative, so an absent spec means the session does not recover natively.
fn binding_native_launch(
    binding: &ResumeBinding,
    definition: &RuntimeDefinition,
) -> Option<NativeSessionLaunch> {
    if binding.program.is_empty() {
        binding
            .native_launch
            .clone()
            .or_else(|| definition.native().cloned())
    } else {
        binding.native_launch.clone()
    }
}

fn session_ref_from_binding(
    reference_kind: SessionRefKind,
    binding: &ResumeBinding,
) -> Result<SessionRef, ProtocolError> {
    match reference_kind {
        SessionRefKind::Id => match &binding.native_session_id {
            Some(value) => SessionRef::id(value),
            None => Err(runtime_error(
                "not_resumable",
                format!(
                    "resume binding for {} is id-kind but has no native id",
                    binding.session_id
                ),
            )),
        },
        SessionRefKind::Path => match &binding.native_session_path {
            Some(value) => SessionRef::path(value),
            None => Err(runtime_error(
                "not_resumable",
                format!(
                    "resume binding for {} is path-kind but has no native path",
                    binding.session_id
                ),
            )),
        },
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "tokio JoinError is delivered by value from JoinHandle::await"
)]
fn join_error_to_io<T>(err: tokio::task::JoinError) -> io::Result<T> {
    Err(io::Error::other(format!(
        "blocking store task failed: {err}"
    )))
}
