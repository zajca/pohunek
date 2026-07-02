//! Resume-binding persistence and relaunch after a daemon restart.

use super::{
    agent_not_resumable, base_resume_template, default_program, info, input_rules_for_agent,
    is_terminal, resume_pty_command_from_template, runtime_error, session_not_found, warn,
    LaunchOpts, Ordering, PathBuf, ProtocolError, PtySessionSpec, ResumeBinding, ResumeTemplate,
    SessionEntry, SessionId, SessionInfo, SessionRef, SessionRefKind, SessionRegistry,
};

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
    /// write is non-fatal and only impairs restart-resume, surfaced via a warn.
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
        let result = match &desired {
            Some(binding) => store.record_resume(binding),
            None => store.remove_resume(&id.0),
        };
        if let Err(err) = result {
            warn!(
                session_id = %id.0,
                error = %err,
                "failed to persist resume binding"
            );
        }
    }

    /// Load the resume-binding store and relaunch each resumable session.
    ///
    /// Called once at daemon startup. A daemon restart kills all live PTYs by
    /// design (see `docs/plan-phase-1.md` "Resume Model"); only sessions whose
    /// native id was captured are persisted, so only those come back here. A
    /// per-session resume failure is logged and skipped, never fatal.
    pub async fn load_and_resume(&self) {
        let Some(store) = &self.inner.store else {
            return;
        };
        let bindings = match store.load_resume() {
            Ok(bindings) => bindings,
            Err(err) => {
                warn!(error = %err, "failed to load resume-binding store; skipping resume");
                return;
            }
        };
        if bindings.is_empty() {
            return;
        }

        info!(
            count = bindings.len(),
            "resuming sessions after daemon restart"
        );
        for binding in bindings {
            let session_id = binding.session_id.clone();
            let agent = binding.agent.clone();
            match self.resume_binding(binding).await {
                Ok(info) => {
                    info!(session_id = %info.id.0, ?agent, "resumed session via native id");
                }
                Err(err) => {
                    // A structurally-corrupt binding (a malformed/absent native
                    // ref) can never resume regardless of environment, so prune
                    // it to self-heal instead of retrying it on every restart.
                    // `agent_binary_missing` is left in place: it may be a
                    // transient PATH gap at startup that resolves on a later run.
                    if matches!(
                        err.code.as_str(),
                        "invalid_session_ref" | "not_resumable" | "agent_not_resumable"
                    ) {
                        // Prune via the serialized persist path: the failed
                        // resume registered no live session, so this re-reads the
                        // id as absent and removes its binding. Routing it here
                        // (instead of a direct store.remove) keeps persist_lock
                        // the single serialization point for all binding writes.
                        self.persist_resume_binding(&SessionId(session_id.clone()))
                            .await;
                        warn!(session_id = %session_id, error = %err, "dropping unresumable binding");
                    } else {
                        warn!(session_id = %session_id, error = %err, "failed to resume session");
                    }
                }
            }
        }
    }

    /// Relaunch a terminal session from its in-memory resume metadata.
    ///
    /// A plain attach requires a live PTY. When a resumable agent exits normally,
    /// the daemon keeps the terminal session entry visible in memory with its
    /// captured native reference, so an explicit user action can relaunch it with
    /// the same pohunek session id. This path intentionally does not auto-resume
    /// sessions that were removed from memory by a daemon restart; startup resume
    /// continues to use the persisted binding store.
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` for an unknown id, `session_not_terminal` for a
    /// still-live session, `not_resumable` when the terminal entry lacks the
    /// native reference required by its frozen resume template, or any PTY launch
    /// error from the relaunch.
    pub async fn resume(&self, id: &SessionId) -> Result<SessionInfo, ProtocolError> {
        let binding = {
            let sessions = self.inner.sessions.lock().await;
            let entry = sessions.get(id).ok_or_else(|| session_not_found(&id.0))?;
            if !is_terminal(entry.info.state) {
                return Err(runtime_error(
                    "session_not_terminal",
                    format!("session is not terminal: {}", id.0),
                ));
            }
            Self::resume_binding_from_entry(id, entry)
        };

        let info = self.resume_binding(binding).await?;
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    fn resume_binding_from_entry(id: &SessionId, entry: &SessionEntry) -> ResumeBinding {
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
    #[expect(
        clippy::too_many_lines,
        reason = "tracked for session module decomposition"
    )]
    pub(super) async fn resume_binding(
        &self,
        binding: ResumeBinding,
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
        let session_ref = match template.ref_kind {
            SessionRefKind::Id => match &binding.native_session_id {
                Some(value) => SessionRef::id(value)?,
                None => {
                    return Err(runtime_error(
                        "not_resumable",
                        format!(
                            "resume binding for {} is id-kind but has no native id",
                            binding.session_id
                        ),
                    ));
                }
            },
            SessionRefKind::Path => match &binding.native_session_path {
                Some(value) => SessionRef::path(value)?,
                None => {
                    return Err(runtime_error(
                        "not_resumable",
                        format!(
                            "resume binding for {} is path-kind but has no native path",
                            binding.session_id
                        ),
                    ));
                }
            },
        };

        let id = SessionId(binding.session_id.clone());
        self.bump_next_id_past(&id);

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
        let (repo, branch, worktree_path) = self.restore_worktree_metadata(&binding.session_id);
        // The project context was captured on the binding when it was persisted
        // (F5), so restore it directly — no git re-detection on the cwd at startup,
        // and a detection failure can no longer silently drop the metadata. An
        // older binding (pre-F5) carries `None`, leaving the resumed session
        // without project context until its next persist.
        let project_id = binding.project_id.clone();
        let is_linked_worktree = binding.is_linked_worktree;
        self.register_pty_session(PtySessionSpec {
            id,
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
    fn restore_worktree_metadata(
        &self,
        session_id: &str,
    ) -> (Option<PathBuf>, Option<String>, Option<PathBuf>) {
        let Some(store) = &self.inner.store else {
            return (None, None, None);
        };
        match store.find_worktree_for_session(session_id) {
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

    /// Advance the session-id counter past a restored `s-<N>` id so a freshly
    /// created session never collides with a resumed one.
    fn bump_next_id_past(&self, id: &SessionId) {
        let Some(n) = id.0.strip_prefix("s-").and_then(|n| n.parse::<u64>().ok()) else {
            return;
        };
        let mut current = self.inner.next_id.load(Ordering::Relaxed);
        while current <= n {
            match self.inner.next_id.compare_exchange_weak(
                current,
                n + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}
