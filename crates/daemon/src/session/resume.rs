//! Native recovery metadata and explicit provider-native relaunch.

use super::{
    agent_fork_unsupported, agent_not_resumable, base_resume_template, default_program,
    fork_pty_command_from_template, input_rules_for_agent, is_terminal,
    resume_pty_command_from_template, runtime_error, session_not_found, validate_session_name,
    warn, LaunchOpts, Ordering, PathBuf, ProtocolError, PtySessionSpec, ResumeBinding,
    ResumeTemplate, SessionEntry, SessionForkParams, SessionId, SessionInfo, SessionRef,
    SessionRefKind, SessionRegistry,
};

use std::io;
use std::sync::Arc;

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
    /// Resolved resume template; `None` ⇒ this session does not resume.
    pub(super) resume: Option<ResumeTemplate>,
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
                entry.snapshot.resume?;
                // Resumable once the agent has reported a native reference — an
                // opaque id (claude/codex) or a transcript path (a path-resuming
                // host profile). No reference yet ⇒ no binding.
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
        let _recovery = self.inner.recovery_lock.lock().await;
        self.ensure_not_external(id).await?;
        let (binding, registration) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
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
                Self::resume_binding_from_entry(id, entry),
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
                    previous_runtime_id: entry
                        .info
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.runtime_id.clone()),
                    created_at: entry.info.created_at.clone(),
                    runtime_watch_cancel: entry.runtime_watch_cancel.clone(),
                },
            )
        };

        let info = match self
            .resume_binding_with_registration(binding, registration)
            .await
        {
            Ok(info) => info,
            Err(error) => {
                let restored = {
                    let sessions = self.inner.sessions.lock().await;
                    sessions
                        .get(id)
                        .map(|entry| Self::session_record(id, entry, entry.desired_state, None))
                };
                if let Some(record) = restored {
                    if let Err(store_error) = self.write_session_record(record).await {
                        warn!(
                            session_id = %id.0,
                            error = %store_error,
                            "failed to roll back native recovery transaction"
                        );
                    }
                }
                return Err(error);
            }
        };
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    /// Fork a native agent conversation into a new pohunek session.
    #[expect(
        clippy::too_many_lines,
        reason = "fork mirrors resume launch assembly while minting a fresh session id"
    )]
    pub async fn fork(&self, params: SessionForkParams) -> Result<SessionInfo, ProtocolError> {
        self.ensure_not_external(&params.session_id).await?;
        let (binding, repo, branch, worktree_path) = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions
                .get(&params.session_id)
                .ok_or_else(|| session_not_found(&params.session_id.0))?;
            (
                Self::resume_binding_from_entry(&params.session_id, entry),
                entry.info.repo.clone(),
                entry.info.branch.clone(),
                entry.info.worktree_path.clone(),
            )
        };

        match params.cwd_mode {
            protocol::ForkCwdMode::Same => {}
        }

        if !binding.resumable && !binding.program.is_empty() {
            return Err(agent_not_resumable(&binding.agent));
        }
        let template = match (binding.resume_mode, binding.ref_kind) {
            (Some(mode), Some(ref_kind)) => ResumeTemplate { mode, ref_kind },
            _ => base_resume_template(binding.agent_base)
                .ok_or_else(|| agent_not_resumable(&binding.agent))?,
        };
        let session_ref = session_ref_from_binding(template, &binding)?;
        if binding.agent_base == protocol::AgentKind::Codex {
            return Err(agent_fork_unsupported(&binding.agent));
        }

        let id = Self::allocate_session_id();
        let has_snapshot = !binding.program.is_empty();
        let program = if has_snapshot {
            binding.program.clone()
        } else {
            default_program(binding.agent_base)
        };
        let input_rules = if has_snapshot {
            binding.input_rules.to_input_rules()
        } else {
            input_rules_for_agent(binding.agent_base, &self.inner.config)
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
        env_extra.extend(self.session_pty_env(binding.agent_base, &id));
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: params.cols,
            rows: params.rows,
            env_extra,
        };
        let command = fork_pty_command_from_template(
            &binding.agent,
            &program,
            binding.args.clone(),
            template,
            &session_ref,
            &opts,
        )?;
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            resume: Some(template),
        };
        let info = self
            .register_pty_session(PtySessionSpec {
                id,
                registration: super::target::PtyRegistration::Create,
                name: validate_session_name(params.name.as_deref())?,
                agent: binding.agent,
                agent_base: binding.agent_base,
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
            })
            .await?;
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    pub(super) fn resume_binding_from_entry(id: &SessionId, entry: &SessionEntry) -> ResumeBinding {
        ResumeBinding {
            session_id: id.0.clone(),
            name: entry.info.name.clone(),
            agent: entry.info.agent.clone(),
            agent_base: entry.info.agent_base,
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
            resume_mode: entry.snapshot.resume.map(|template| template.mode),
            ref_kind: entry.snapshot.resume.map(|template| template.ref_kind),
            resumable: entry.snapshot.resume.is_some(),
        }
    }

    /// Relaunch one session from its stored resume binding, reusing its id.
    #[cfg(test)]
    pub(super) async fn resume_binding(
        &self,
        binding: ResumeBinding,
    ) -> Result<SessionInfo, ProtocolError> {
        self.resume_binding_with_registration(binding, super::target::PtyRegistration::Create)
            .await
    }

    /// Relaunch one session under the supplied durable lifecycle operation.
    async fn resume_binding_with_registration(
        &self,
        binding: ResumeBinding,
        registration: super::target::PtyRegistration,
    ) -> Result<SessionInfo, ProtocolError> {
        // The resume mechanics come from the frozen structural snapshot (C.4). An
        // explicit `(resume_mode, ref_kind)` pair drives the argv; a legacy binding
        // (pre-C2, no snapshot) falls back to the base kind's native template.
        if !binding.resumable && !binding.program.is_empty() {
            return Err(agent_not_resumable(&binding.agent));
        }
        let template = match (binding.resume_mode, binding.ref_kind) {
            (Some(mode), Some(ref_kind)) => ResumeTemplate { mode, ref_kind },
            _ => base_resume_template(binding.agent_base)
                .ok_or_else(|| agent_not_resumable(&binding.agent))?,
        };
        // Build the native reference from the field the frozen `ref_kind` names, so
        // a `path`-kind profile inherits the absolute-path guard and an `id`-kind the
        // leading-dash guard (the documented asymmetry).
        let session_ref = session_ref_from_binding(template, &binding)?;

        let id = SessionId(binding.session_id.clone());

        // A legacy binding carries no snapshot program; fall back to the base kind's
        // default so it still relaunches. `program`/`input_rules` are frozen
        // structural fields — never re-resolved from the profile.
        let has_snapshot = !binding.program.is_empty();
        let program = if has_snapshot {
            binding.program.clone()
        } else {
            default_program(binding.agent_base)
        };
        let input_rules = if has_snapshot {
            binding.input_rules.to_input_rules()
        } else {
            input_rules_for_agent(binding.agent_base, &self.inner.config)
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
        env_extra.extend(self.session_pty_env(binding.agent_base, &id));
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: binding.cols,
            rows: binding.rows,
            env_extra,
        };
        let command = resume_pty_command_from_template(
            &program,
            binding.args.clone(),
            template,
            &session_ref,
            &opts,
        )?;
        // Re-freeze the structural snapshot for the resumed entry so a later resize
        // re-persist keeps the same launch-time shape.
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            resume: Some(template),
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
        self.register_pty_session(PtySessionSpec {
            id,
            registration,
            name: binding.name,
            agent: binding.agent,
            agent_base: binding.agent_base,
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
        })
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

fn session_ref_from_binding(
    template: ResumeTemplate,
    binding: &ResumeBinding,
) -> Result<SessionRef, ProtocolError> {
    match template.ref_kind {
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
