//! Native recovery metadata and explicit provider-native relaunch.

// Rust guideline compliant 2026-10-12

use super::{
    agent_fork_unsupported, agent_not_resumable, fork_pty_command_from_launch, host,
    input_rules_for_definition, is_terminal, resume_pty_command_from_launch, runtime_error,
    session_not_found, validate_session_name, warn, LaunchOpts, NativeSessionLaunch, Ordering,
    PathBuf, ProtocolError, PtySessionSpec, ResumeBinding, SessionEntry, SessionForkParams,
    SessionId, SessionInfo, SessionRef, SessionRefKind, SessionRegistry, ValidatedLaunchProgram,
};

use std::ffi::OsString;
use std::io;
use std::sync::Arc;

use crate::agent::host::{LaunchPin, ProfileRevision, RuntimeDefinition, RuntimeHost};
use crate::agent::{
    InputRules, LaunchEnv, LaunchView, NativeReferenceProvenance, Pin, ProfilePin, ProfileRegistry,
    ReferenceCheckFailure, ReferenceExistence, StoreRoot,
};
use crate::detect::Manifest;
use crate::runtime::environment::{base_environment, effective_variable};
use package::PackageDigest;
use pohunek_worker_protocol::BaseEnv;
use protocol::ErrorClass;
use protocol::{BindingProvenance, LaunchBinding, PackageId, RuntimeRef};

/// Frozen structural relaunch snapshot for a session (Part C, C.4).
///
/// Set once at launch from the [`ResolvedAgent`] and persisted verbatim on every
/// resume-binding write, so a daemon restart relaunches with the original launch
/// program/args + resume mechanics even after the host profile is edited or
/// deleted. Deliberately holds **no env**: it is re-resolved by agent name at
/// resume, and only while the profile still has the frozen
/// [`ResumeSnapshot::profile_revision`] (env may carry secrets, which never
/// touch the store).
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
    /// Where the native reference the session currently holds came from. It
    /// changes with the reference, unlike the launch shape above.
    pub(super) reference_provenance: NativeReferenceProvenance,
    /// Keyed revision of the host profile the session was launched under;
    /// `None` for a session without a profile or a binding that never froze one.
    pub(super) profile_revision: Option<ProfileRevision>,
}

impl ResumeSnapshot {
    /// The snapshot of a session with no recorded launch shape.
    pub(super) fn empty() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            native: None,
            launch_binding: host::LaunchPin::Unpinned,
            reference_provenance: NativeReferenceProvenance::Reported,
            profile_revision: None,
        }
    }

    /// Rebuild the snapshot frozen into a persisted binding.
    pub(super) fn from_binding(binding: &ResumeBinding) -> Self {
        Self {
            program: binding.program.clone(),
            args: binding.args.clone(),
            native: binding.native_launch.clone(),
            launch_binding: binding.launch_binding.clone(),
            reference_provenance: binding.native_reference_provenance,
            profile_revision: binding.profile_revision.clone(),
        }
    }

    /// Return the native reference kind required by resume or fork.
    pub(super) fn native_ref_kind(&self) -> Option<SessionRefKind> {
        self.native
            .as_ref()
            .map(NativeSessionLaunch::reference_kind)
    }
}

/// The owner's decision on a host profile that no longer has the revision
/// frozen into a session.
///
/// [`ProfileChange::Accept`] is an authority only the local owner holds: the
/// control handlers build it after checking the transport, and no remote or
/// relay request can produce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileChange {
    /// Refuse a relaunch whose profile changed, is missing, or was never frozen.
    #[default]
    Refuse,
    /// Relaunch under the current profile and freeze its revision.
    Accept,
}

impl ProfileChange {
    /// The decision an owner request carries: accept when `accept` is set.
    #[must_use]
    pub fn requested(accept: bool) -> Self {
        if accept {
            Self::Accept
        } else {
            Self::Refuse
        }
    }
}

/// The host profile a resume or fork relaunches under, resolved once.
pub(super) struct RecoveryProfile {
    /// Profile `[env]`, secret-bearing.
    pub(super) env: LaunchEnv,
    /// Detection-manifest override of the profile.
    pub(super) manifest: Option<Manifest>,
    /// Current revision of the profile, frozen into the relaunched session;
    /// `None` for a session without a profile.
    pub(super) revision: Option<ProfileRevision>,
}

impl RecoveryProfile {
    /// The carrier of a session that has no profile.
    fn none() -> Self {
        Self {
            env: LaunchEnv::default(),
            manifest: None,
            revision: None,
        }
    }
}

/// Everything a resume or fork launches with, resolved once and consumed by
/// verification, launch and the relaunched snapshot.
pub(super) struct RelaunchPlan {
    definition: Arc<RuntimeDefinition>,
    launch: NativeSessionLaunch,
    session_ref: SessionRef,
    program: String,
    validated_program: Option<ValidatedLaunchProgram>,
    /// Base environment the launch hands to the agent.
    base_environment: BaseEnv,
    pub(super) profile: RecoveryProfile,
}

struct ActivityGroup {
    existence: ReferenceExistence,
    store: StoreRoot,
    references: Vec<(usize, SessionRef)>,
}

/// The profile a session was launched from no longer resolves.
fn profile_missing(binding: &ResumeBinding, reason: &str) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_profile_missing",
        format!(
            "agent profile '{}' of session {} no longer resolves ({reason})",
            binding.agent, binding.session_id
        ),
        Some("restore the profile to relaunch the session".to_owned()),
    )
}

/// The profile a session was launched from differs from the frozen revision, or
/// the session never froze one.
fn profile_changed(binding: &ResumeBinding, was_frozen: bool) -> ProtocolError {
    let message = if was_frozen {
        format!(
            "agent profile '{}' changed since session {} was launched",
            binding.agent, binding.session_id
        )
    } else {
        format!(
            "session {} has no recorded revision of agent profile '{}'",
            binding.session_id, binding.agent
        )
    };
    ProtocolError::new(
        ErrorClass::Runtime,
        "agent_profile_changed",
        message,
        Some(
            "review the profile, then relaunch with --accept-profile-change to run \
             under the current profile"
                .to_owned(),
        ),
    )
}

fn native_evidence_unavailable(
    binding: &ResumeBinding,
    reason: impl std::fmt::Display,
) -> ProtocolError {
    ProtocolError::new(
        ErrorClass::Runtime,
        "native_identity_evidence_unavailable",
        format!(
            "session {} native conversation evidence cannot be verified: {reason}",
            binding.session_id
        ),
        Some("inspect the durable store and worker journal, then retry recovery".to_owned()),
    )
}

impl SessionRegistry {
    /// The activity lookup of a listed session's native reference: where its
    /// transcript is checked, the reference and the profile environment that
    /// resolves the store. `None` when the reference cannot be looked up.
    fn native_activity_lookup(
        &self,
        (target, binding, definition): (String, ResumeBinding, Arc<RuntimeDefinition>),
        resolutions: &mut PinResolutions,
    ) -> Option<(ReferenceExistence, SessionRef, LaunchEnv)> {
        let launch = binding_native_launch(&binding, &definition);
        let existence = reference_existence(&binding, launch.as_ref(), &definition)?;
        let Ok(profile) = self.resolve_recovery_profile_memo(
            &binding,
            &definition,
            ProfileChange::Refuse,
            false,
            resolutions,
        ) else {
            return None;
        };
        let Ok(reference) = SessionRef::id(&target) else {
            return None;
        };
        Some((existence, reference, profile.env))
    }

    /// Adds verified native-reference last activity to public response copies,
    /// and clears the resume and fork capabilities of each listed session whose
    /// recovery resolution refuses them, naming the refusal codes, so a
    /// response never advertises an operation that resolution refuses. Refusals
    /// that depend on the session's state or on one request are left to the
    /// operation.
    ///
    /// A runtime whose descriptor declares the conversation store reports the
    /// verified modification time of the reference's transcript, so list and
    /// inspect responses can lead with the freshest common knowledge. Store
    /// records keep this field unset: transcript activity changes outside
    /// the daemon, so a cached value would give clients a stale recovery time.
    ///
    /// Each listed session resolves its recovery definition once; the
    /// capabilities and the activity both read that result. The frozen
    /// capabilities stay the upper bound: a capability is only ever cleared,
    /// never set. Nothing is persisted.
    pub(crate) async fn enrich_native_activity(&self, sessions: &mut [SessionInfo]) {
        let candidates = {
            let entries = self.inner.sessions.lock().await;
            sessions
                .iter()
                .map(|info| {
                    let recoverable = info.capabilities.resume
                        || info.capabilities.fork
                        || info.native_session_id.is_some();
                    recoverable
                        .then(|| entries.get(&info.id))
                        .flatten()
                        .map(|entry| Self::resume_binding_from_entry(&info.id, entry))
                })
                .collect::<Vec<_>>()
        };
        let mut lookup_inputs = Vec::with_capacity(sessions.len());
        let mut resolutions = PinResolutions::default();
        for (info, candidate) in sessions.iter_mut().zip(candidates) {
            let Some(binding) = candidate else {
                lookup_inputs.push(None);
                continue;
            };
            let definition =
                recovery_definition_memo(&self.inner.profiles, &binding, &mut resolutions);
            clear_refused_capabilities(info, &binding, definition.as_ref());
            let definition = definition.ok();
            lookup_inputs.push(
                info.native_session_id
                    .clone()
                    .zip(definition)
                    .map(|(target, definition)| (target, binding, definition)),
            );
        }
        let lookups = lookup_inputs
            .into_iter()
            .map(|input| self.native_activity_lookup(input?, &mut resolutions))
            .collect::<Vec<_>>();
        if lookups.iter().all(Option::is_none) {
            return;
        }
        let Ok(base_environment) = self.launch_base_environment() else {
            return;
        };
        let sessions_len = sessions.len();
        let modified = tokio::task::spawn_blocking(move || {
            let mut groups: Vec<ActivityGroup> = Vec::new();
            for (index, lookup) in lookups.into_iter().enumerate() {
                let Some((existence, reference, profile_env)) = lookup else {
                    continue;
                };
                let resolve = |name: &str| {
                    effective_variable(&base_environment, profile_env.as_slice(), name)
                };
                let Ok(Some(store)) = existence.activity_store_root(&resolve) else {
                    continue;
                };
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.existence == existence && group.store == store)
                {
                    group.references.push((index, reference));
                } else {
                    groups.push(ActivityGroup {
                        existence,
                        store,
                        references: vec![(index, reference)],
                    });
                }
            }
            let mut modified = vec![None; sessions_len];
            for group in groups {
                let references = group
                    .references
                    .iter()
                    .map(|(_, reference)| reference.clone())
                    .collect::<Vec<_>>();
                for ((index, _), nanos) in group.references.into_iter().zip(
                    group
                        .existence
                        .modified_many_unix_nanos(&group.store, &references),
                ) {
                    modified[index] = nanos;
                }
            }
            modified
        })
        .await;
        let Ok(modified) = modified else {
            return;
        };
        for (info, modified) in sessions.iter_mut().zip(modified) {
            info.native_last_activity_at = modified
                .and_then(|nanos| time::OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
                .and_then(|time| {
                    time.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                });
        }
    }

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
        let _persist = self.inner.persist_lock.lock().await;
        let store = match self.store_for_write() {
            Ok(Some(store)) => store,
            Ok(None) => return,
            Err(error) => {
                warn!(session_id = %id.0, %error, "resume binding not persisted");
                return;
            }
        };
        #[cfg(test)]
        if self.inner.resume_writes_blocked.load(Ordering::Relaxed) {
            return;
        }
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
        let session_id = id.0.clone();
        let result = self
            .inner
            .background
            .spawn_blocking(move || match desired {
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
    /// A session launched from a host profile relaunches only while the profile
    /// still has the revision frozen into the session; see [`ProfileChange`].
    ///
    /// # Errors
    ///
    /// Returns `session_not_found` for an unknown id,
    /// `session_runtime_not_recoverable` unless the runtime is terminal or lost,
    /// `native_identity_missing` or `native_identity_unverified` when a hook
    /// runtime lacks a trusted native reference, `not_resumable` when another
    /// entry lacks the reference required by its frozen resume template,
    /// `native_identity_evidence_unavailable` when the durable record or exact
    /// worker generation cannot be read and matched,
    /// `native_identity_uncertain` when the worker journal carries a newer
    /// conversation switch the daemon cannot verify, so recovery neither
    /// launches an agent into a conversation it cannot confirm nor falls back
    /// to the older verified target,
    /// `agent_native_reference_missing` when a required conversation file is
    /// absent or cannot be verified, `agent_native_store_relocated` when the
    /// agent would keep its conversations outside the declared store,
    /// `agent_profile_changed` or `agent_profile_missing`
    /// when the profile no longer matches the session, or any worker launch
    /// error from recovery.
    pub async fn resume(&self, id: &SessionId) -> Result<SessionInfo, ProtocolError> {
        self.resume_with(id, ProfileChange::Refuse).await
    }

    /// [`Self::resume`] with the owner's decision on a changed profile.
    ///
    /// # Errors
    ///
    /// Returns the [`Self::resume`] errors.
    pub async fn resume_with(
        &self,
        id: &SessionId,
        change: ProfileChange,
    ) -> Result<SessionInfo, ProtocolError> {
        self.ensure_not_external(id).await?;
        // Held from resolving the pin until the detached registration has
        // persisted the resumed record, so an adopting package cannot be
        // uninstalled in between: the built-in pin does not count as a
        // reference to it. Taken before the lifecycle guard, as `fork` does.
        let package_authority = Arc::clone(&self.inner.package_lifecycle).read_owned().await;
        let guard = self.lock_lifecycle(id).await;
        let (binding, definition, registration, record) = {
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
                        .and_then(|runtime| runtime.worker_instance_id.clone()),
                    previous_cleanup_unconfirmed: entry.info.runtime.as_ref().is_some_and(
                        |runtime| {
                            runtime.state == protocol::RuntimeState::Lost
                                && runtime.loss_reason.as_deref()
                                    == Some(super::supervision::RUNTIME_LOST_CLEANUP_UNCONFIRMED)
                        },
                    ),
                    previous_job: entry.job.clone(),
                    previous_runtime_generation: entry
                        .info
                        .runtime
                        .as_ref()
                        .map_or(protocol::RuntimeGeneration::new(0), |runtime| {
                            runtime.runtime_generation
                        }),
                    created_at: entry.info.created_at.clone(),
                    previous_native_report: entry.last_native_report.clone().map(Box::new),
                    runtime_watch_cancel: entry.runtime_watch_cancel.clone(),
                },
                Self::session_record(id, entry, entry.desired_state, None),
            )
        };
        let launch = binding_native_launch(&binding, &definition)
            .ok_or_else(|| agent_not_resumable(&binding.agent))?;
        self.hook_recovery_journal_error(&record, &binding, &launch)
            .await?;
        #[cfg(test)]
        self.hold_definition().await;
        let relaunch = self.plan_relaunch(&binding, definition, launch, change, false)?;

        let info = match self
            .resume_binding_with_registration(
                binding,
                registration,
                relaunch,
                guard,
                Some(package_authority),
            )
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

    /// Refuses recovery of a hook binding whose reference or exact generation
    /// evidence cannot be verified against the durable state.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the target or its generation cannot be
    /// verified before a new worker is created.
    async fn hook_recovery_journal_error(
        &self,
        record: &crate::store::SessionRecord,
        binding: &ResumeBinding,
        launch: &NativeSessionLaunch,
    ) -> Result<(), ProtocolError> {
        if let Some(error) = self
            .missing_hook_reference_error(record, binding, launch)
            .await
        {
            return Err(error);
        }
        self.unverified_switch_claim_error(record, binding, launch)
            .await
    }

    async fn missing_hook_reference_error(
        &self,
        record: &crate::store::SessionRecord,
        binding: &ResumeBinding,
        launch: &NativeSessionLaunch,
    ) -> Option<ProtocolError> {
        let missing = launch.assigned().is_none()
            && match launch.reference_kind() {
                SessionRefKind::Id => binding.native_session_id.is_none(),
                SessionRefKind::Path => binding.native_session_path.is_none(),
            };
        if !missing {
            return None;
        }
        let code = match self.generation_has_native_claim(record).await {
            Ok(true) => "native_identity_unverified",
            Ok(false) => "native_identity_missing",
            Err(_) => "native_identity_evidence_unavailable",
        };
        Some(runtime_error(
            code,
            format!(
                "session {} has no verified native conversation reference",
                binding.session_id
            ),
        ))
    }

    /// Refuses recovery when the worker's journal carries a newer conversation
    /// switch the daemon cannot verify (issue #421).
    ///
    /// The persisted target of a hook runtime follows only claims the daemon
    /// admitted. A journaled native-reference claim of the record's worker
    /// generation that names the session's provider and reference kind, is
    /// newer than the durable ordering mark for that generation, and names a
    /// conversation other than the persisted target, is such a claim the
    /// durable record still refuses or has not imported. Recovery must not
    /// silently fall back to the older verified target in that state: the
    /// daemon cannot tell which conversation the agent last used.
    ///
    /// A claim at or behind the accepted ordering mark is stale and cannot
    /// overwrite the newer verified target, so it leaves recovery alone. A
    /// claim of another generation is history the record already moved past,
    /// and a claim that re-affirms the persisted target is already imported.
    /// Without a journal native-reference claim, an immutable launch claim of
    /// this generation that names another conversation, with no ordering mark
    /// of the generation, is the same uncertain switch. An assigned runtime
    /// confirms its reference against its declared existence check instead of
    /// ordering.
    async fn unverified_switch_claim_error(
        &self,
        record: &crate::store::SessionRecord,
        binding: &ResumeBinding,
        launch: &NativeSessionLaunch,
    ) -> Result<(), ProtocolError> {
        if launch.assigned().is_some() {
            return Ok(());
        }
        let (ref_kind, stored) = match launch.reference_kind() {
            SessionRefKind::Id => (SessionRefKind::Id, record.info.native_session_id.as_deref()),
            SessionRefKind::Path => (
                SessionRefKind::Path,
                record.info.native_session_path.as_deref(),
            ),
        };
        let Some(stored) = stored else {
            return Ok(());
        };
        // The journal of the last durable worker generation is the evidence a
        // terminal runtime leaves behind. The in-memory entry may have already
        // retired its job and dropped the generation token, so the durable
        // record names it. An unreadable durable record is uncertain evidence.
        let durable = self
            .load_durable_session_record(&SessionId(record.session_id.clone()))
            .await
            .map_err(|_error| {
                native_evidence_unavailable(binding, "the durable session record cannot be read")
            })?;
        let durable = durable.filter(|durable| durable.runtime.generation.is_some());
        let durable = durable.as_ref().unwrap_or(record);
        let Some(snapshot) = self
            .generation_journal_snapshot_checked(durable)
            .await
            .map_err(|_reason| {
                native_evidence_unavailable(binding, "the worker journal cannot be read")
            })?
        else {
            if durable.runtime.generation.is_some() {
                return Err(native_evidence_unavailable(
                    binding,
                    "the expected worker journal is absent",
                ));
            }
            return Ok(());
        };
        if Some(snapshot.worker_id.as_str()) != durable.runtime.worker_id.as_deref() {
            return Err(native_evidence_unavailable(
                binding,
                "the worker journal names another worker",
            ));
        }
        let instance = snapshot
            .worker_instance_id
            .as_ref()
            .map(pohunek_worker_protocol::WorkerInstanceId::as_str);
        if instance != durable.runtime.worker_instance_id.as_deref() {
            return Err(native_evidence_unavailable(
                binding,
                "the worker journal names another instance",
            ));
        }
        let instance = instance.ok_or_else(|| {
            native_evidence_unavailable(binding, "the worker journal has no instance identity")
        })?;
        let provider = super::agent_kind_label(&durable.info.agent_base);
        let ordering = durable
            .native_identity_ordering
            .as_ref()
            .filter(|ordering| ordering.worker_instance_id == instance);
        let uncertain = if let Some(reference) = snapshot.native_reference.as_ref() {
            reference.provider == provider
                && super::reconcile::parse_reference_kind(&reference.reference_kind)
                    == Some(ref_kind)
                && reference.native_reference != stored
                && super::native_report_is_current(
                    ordering,
                    instance,
                    reference.sequence,
                    crate::store::ReportTransport::Worker,
                )
        } else if let Some(claim) = snapshot.launch_identity.as_ref() {
            claim.provider == provider
                && super::reconcile::parse_reference_kind(&claim.reference_kind) == Some(ref_kind)
                && ordering.is_none()
                && claim.native_reference != stored
        } else {
            false
        };
        if !uncertain {
            return Ok(());
        }
        Err(runtime_error(
            "native_identity_uncertain",
            format!(
                "session {} has a newer native conversation switch that could not be verified; native recovery is unavailable",
                binding.session_id
            ),
        ))
    }

    /// Resolves the runtime a binding relaunches with (see
    /// [`recovery_definition`]).
    ///
    /// # Errors
    ///
    /// Returns the [`recovery_definition`] errors.
    fn binding_definition(
        &self,
        binding: &ResumeBinding,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        recovery_definition(&self.inner.profiles, binding)
    }

    pub(super) async fn persist_failed_recovery_rollback(&self, id: &SessionId) {
        let written = self
            .write_entry_record(id, |entry| {
                Self::session_record(id, entry, entry.desired_state, None)
            })
            .await;
        match written {
            Ok(()) => {}
            // The session left the registry, so no record is left to roll back.
            Err(error) if error.code == "session_not_found" => {}
            Err(store_error) => warn!(
                session_id = %id.0,
                error = %store_error,
                "failed to roll back native recovery transaction"
            ),
        }
    }

    /// Fork a native agent conversation into a new pohunek session.
    ///
    /// The fork launches under the source session's host profile only while
    /// the profile still has the revision frozen into the source (see
    /// [`ProfileChange`]); the fork freezes the revision it launched under.
    /// A runtime whose assigned reference covers forks launches the fork under
    /// a freshly generated reference, which the fork holds with provenance
    /// `assigned` from its creation on, so the fork is resumable at once.
    ///
    /// # Errors
    ///
    /// Returns `migration_manifest_missing` while unmigrated legacy resume
    /// bindings exist, before anything is written, `agent_profile_changed` or
    /// `agent_profile_missing` when the profile no longer matches the source,
    /// `native_identity_evidence_unavailable` when the source's durable record
    /// or exact worker generation cannot be read and matched,
    /// `native_identity_uncertain` when the source's worker journal carries a
    /// newer conversation switch the daemon cannot verify, so no fork child
    /// launches from a source whose target the daemon cannot confirm,
    /// `agent_native_reference_missing` when the source's conversation file is
    /// absent or cannot be verified, `agent_native_store_relocated` when the
    /// agent would keep its conversations outside the declared store,
    /// and otherwise the source lookup, launch-template, or launch error.
    pub async fn fork(&self, params: SessionForkParams) -> Result<SessionInfo, ProtocolError> {
        self.ensure_migration_settled()?;
        self.ensure_not_external(&params.session_id).await?;
        // Held from resolving the pin until the detached registration has
        // persisted the fork's record, so the pinned package cannot be
        // uninstalled in between. A disabled package still forks.
        let package_authority = Arc::clone(&self.inner.package_lifecycle).read_owned().await;
        let (binding, repo, branch, worktree_path, record) =
            self.fork_source(&params.session_id).await?;
        let definition = self.binding_definition(&binding)?;

        // Fork is fail-closed: only a spec frozen into the binding can fork, never
        // the base kind's compiled spec.
        let launch = frozen_fork_launch(&binding)?;
        recovery_contract(&binding, &definition, true)?;
        self.hook_recovery_journal_error(&record, &binding, &launch)
            .await?;
        #[cfg(test)]
        self.hold_definition().await;
        // A fork starts a new process of the runtime, so a runtime with a
        // version probe is probed again before anything is allocated.
        let change = ProfileChange::requested(params.accept_profile_change);
        let relaunch = self.plan_relaunch(&binding, definition, launch, change, true)?;
        let relaunch_pin = host::relaunch_pin(&binding.launch_binding, &relaunch.definition);
        let outdated_hooks = self.outdated_relaunch_warnings(&binding, &relaunch).await;
        let id = Self::allocate_session_id();
        self.ensure_worker_socket(&id)?;
        let input_rules = self.recovery_input_rules(&binding, &relaunch.definition);

        // Fork is fail-closed on a reference that no longer names a conversation.
        verify_recovery_reference(&binding, &relaunch).await?;
        #[cfg(test)]
        self.hold_recovery(&id).await;
        let RelaunchPlan {
            launch,
            session_ref,
            program,
            validated_program,
            profile:
                RecoveryProfile {
                    env,
                    manifest,
                    revision,
                },
            ..
        } = relaunch;
        let env_extra = self.launch_env(&env, binding.agent_base.clone(), &id);
        let opts = LaunchOpts {
            cwd: binding.cwd.clone(),
            cols: params.cols,
            rows: params.rows,
            env_extra,
            validated_program,
        };
        let (command, fork_reference) = fork_pty_command_from_launch(
            &program,
            binding.args.clone(),
            &launch,
            &session_ref,
            &opts,
        )?;
        // A fork reads the source reference but starts a distinct conversation.
        // Its own target is the reference core assigned to it at launch, when
        // the runtime assigns forks one, and otherwise only a verified report
        // from the child process may set it.
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            native: Some(launch),
            launch_binding: relaunch_pin,
            reference_provenance: if fork_reference.is_some() {
                NativeReferenceProvenance::Assigned
            } else {
                NativeReferenceProvenance::Reported
            },
            profile_revision: revision,
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
                    manifest_override: manifest,
                    cwd: binding.cwd,
                    cols: params.cols,
                    rows: params.rows,
                    command,
                    native_session_id: fork_reference.map(|reference| reference.value().to_owned()),
                    native_session_path: None,
                    project_id: binding.project_id,
                    is_linked_worktree: binding.is_linked_worktree,
                    repo,
                    branch,
                    worktree_path,
                    metadata: binding.metadata,
                    warnings: outdated_hooks,
                    reference_launch: None,
                    initial_input_pending: false,
                    prompt_in_argv: false,
                    package_authority: Some(package_authority),
                },
                guard,
            )
            .await?;
        self.persist_resume_binding(&info.id).await;
        Ok(info)
    }

    /// The base environment `host.inspect` assumes for launches.
    ///
    /// A registry whose base environment cannot be built cannot launch an
    /// agent either; the inventory then probes under an empty environment, so
    /// it never reports a `PATH` no launch would see.
    pub(crate) fn inspect_base_environment(&self) -> BaseEnv {
        self.launch_base_environment().unwrap_or_default()
    }

    /// The `PATH` a launch of `definition` hands to the agent: the base
    /// environment overridden by `profile_env`.
    ///
    /// Only a runtime with a version probe needs it, so any other definition
    /// yields `None` without building the base environment.
    pub(super) fn probe_search_path(
        &self,
        definition: &RuntimeDefinition,
        profile_env: &[(String, String)],
    ) -> Result<Option<OsString>, ProtocolError> {
        if definition.version_probe_parser().is_none() {
            return Ok(None);
        }
        let base = self.launch_base_environment()?;
        Ok(effective_variable(&base, profile_env, "PATH"))
    }

    /// The base environment a launch of this registry hands to the agent.
    pub(super) fn launch_base_environment(&self) -> Result<BaseEnv, ProtocolError> {
        let lifecycle = self.lifecycle()?;
        base_environment(
            &lifecycle.config.environment_allowlist,
            &lifecycle.config.environment_source,
        )
        .map_err(|error| runtime_error("worker_initialize_invalid", error.to_string()))
    }

    /// Resolves the host profile `binding` was launched from, once, and checks
    /// it against the revision frozen into the binding.
    ///
    /// The environment and detection-manifest override are re-resolved by name
    /// because neither is persisted. The returned [`RecoveryProfile`] is the
    /// only resolution of the operation: probe, reference check, launch and the
    /// relaunched snapshot all use it, so a profile edited meanwhile cannot
    /// change what launches.
    ///
    /// A session without a profile (its name resolves to a bare runtime and no
    /// revision was frozen) yields an empty carrier.
    ///
    /// # Errors
    ///
    /// Returns `agent_profile_missing` when a profile the session was launched
    /// from (frozen, or named by a binding that predates freezing) no longer
    /// resolves, `agent_profile_changed` when its revision differs from the
    /// frozen one or none was frozen and `change` is [`ProfileChange::Refuse`],
    /// and `agent_profile_revision_unavailable` when the revision key cannot
    /// be read.
    pub(super) fn resolve_recovery_profile(
        &self,
        binding: &ResumeBinding,
        definition: &Arc<RuntimeDefinition>,
        change: ProfileChange,
        fork: bool,
    ) -> Result<RecoveryProfile, ProtocolError> {
        self.resolve_recovery_profile_memo(
            binding,
            definition,
            change,
            fork,
            &mut PinResolutions::default(),
        )
    }

    /// [`Self::resolve_recovery_profile`] resolving the pin judgements through
    /// `resolutions`.
    pub(super) fn resolve_recovery_profile_memo(
        &self,
        binding: &ResumeBinding,
        definition: &Arc<RuntimeDefinition>,
        change: ProfileChange,
        fork: bool,
        resolutions: &mut PinResolutions,
    ) -> Result<RecoveryProfile, ProtocolError> {
        recovery_profile_memo(
            &self.inner.profiles,
            binding,
            Some(definition),
            change,
            fork,
            resolutions,
        )
    }

    /// The input framing a relaunch of `binding` uses: the rules frozen into
    /// the binding, else the runtime's own.
    fn recovery_input_rules(
        &self,
        binding: &ResumeBinding,
        definition: &RuntimeDefinition,
    ) -> InputRules {
        if binding.program.is_empty() {
            input_rules_for_definition(definition, &self.inner.config)
        } else {
            binding.input_rules.to_input_rules(definition.input_rules())
        }
    }

    /// Resolves everything a resume or fork of `binding` launches with, once:
    /// the profile, the native reference, the program and the version probe.
    ///
    /// # Errors
    ///
    /// Returns the [`Self::resolve_recovery_profile`] errors, the reference
    /// error of a binding without the native reference `launch` consumes,
    /// `agent_binary_missing` when the frozen program no longer resolves to an
    /// executable, and `agent_runtime_unsupported` when the version probe
    /// rejects the executable that does.
    fn plan_relaunch(
        &self,
        binding: &ResumeBinding,
        definition: Arc<RuntimeDefinition>,
        launch: NativeSessionLaunch,
        change: ProfileChange,
        fork: bool,
    ) -> Result<RelaunchPlan, ProtocolError> {
        let launch = host::relaunch_launch(&binding.launch_binding, &definition, launch);
        // Build the native reference from the field the frozen reference kind
        // names, so a `path`-kind spec applies the absolute-path guard and an
        // `id`-kind spec the leading-dash guard (the documented asymmetry).
        let session_ref = session_ref_from_binding(launch.reference_kind(), binding)?;
        let profile = self.resolve_recovery_profile(binding, &definition, change, fork)?;
        let base_environment = self.launch_base_environment()?;
        let program = binding_program(binding, &definition);
        let launch_path = self.probe_search_path(&definition, profile.env.as_slice())?;
        let validated_program = host::validate_launch_runtime(
            &definition,
            host::Spawn::Program(&program),
            launch_path.as_deref(),
        )?;
        Ok(RelaunchPlan {
            definition,
            launch,
            session_ref,
            program,
            validated_program,
            base_environment,
            profile,
        })
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
            crate::store::SessionRecord,
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
            Self::session_record(id, entry, entry.desired_state, None),
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
            // launch-time shape. `env` is intentionally absent: it is re-resolved
            // by agent name at resume (no secrets in store); only the keyed
            // revision of the profile it came from is kept.
            program: entry.snapshot.program.clone(),
            args: entry.snapshot.args.clone(),
            input_rules: entry.input_rules.into(),
            native_launch: entry.snapshot.native.clone(),
            launch_binding: entry.snapshot.launch_binding.clone(),
            native_reference_provenance: entry.snapshot.reference_provenance,
            profile_revision: entry.snapshot.profile_revision.clone(),
            native_launch_unresolved: false,
        }
    }

    /// Relaunch one session from its stored resume binding, reusing its id.
    #[cfg(test)]
    pub(super) async fn resume_binding(
        &self,
        binding: ResumeBinding,
    ) -> Result<SessionInfo, ProtocolError> {
        self.resume_binding_with(binding, ProfileChange::Refuse)
            .await
    }

    /// [`Self::resume_binding`] with the owner's decision on a changed profile.
    #[cfg(test)]
    pub(super) async fn resume_binding_with(
        &self,
        binding: ResumeBinding,
        change: ProfileChange,
    ) -> Result<SessionInfo, ProtocolError> {
        let definition = self.binding_definition(&binding)?;
        let launch = binding_native_launch(&binding, &definition)
            .ok_or_else(|| agent_not_resumable(&binding.agent))?;
        let relaunch = self.plan_relaunch(&binding, definition, launch, change, false)?;
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
                previous_cleanup_unconfirmed: record.info.runtime.as_ref().is_some_and(|runtime| {
                    runtime.state == protocol::RuntimeState::Lost
                        && runtime.loss_reason.as_deref()
                            == Some(super::supervision::RUNTIME_LOST_CLEANUP_UNCONFIRMED)
                }),
                previous_runtime_generation: record
                    .info
                    .runtime
                    .as_ref()
                    .map_or(protocol::RuntimeGeneration::new(0), |runtime| {
                        runtime.runtime_generation
                    }),
                created_at: record.info.created_at,
                previous_native_report: record.native_identity_ordering.map(Box::new),
                runtime_watch_cancel: tokio_util::sync::CancellationToken::new(),
            },
            None => super::target::PtyRegistration::Create,
        };
        self.resume_binding_with_registration(binding, registration, relaunch, guard, None)
            .await
    }

    /// Relaunch one session under the supplied durable lifecycle operation.
    async fn resume_binding_with_registration(
        &self,
        binding: ResumeBinding,
        registration: super::target::PtyRegistration,
        relaunch: RelaunchPlan,
        guard: super::LifecycleGuard,
        package_authority: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<SessionInfo, ProtocolError> {
        // The resume mechanics come from the frozen structural snapshot (C.4). A
        // binding without a snapshot program falls back to the base kind's
        // compiled spec.
        let id = SessionId(binding.session_id.clone());
        let outdated_hooks = self.outdated_relaunch_warnings(&binding, &relaunch).await;

        // A legacy binding carries no snapshot program; fall back to the base kind's
        // default so it still relaunches. `program`/`input_rules` are frozen
        // structural fields: never re-resolved from the profile.
        let input_rules = self.recovery_input_rules(&binding, &relaunch.definition);

        // A reference core assigned is only as good as the conversation behind
        // it: relaunching into a conversation the agent never wrote, or one it
        // left, would silently start an empty session.
        verify_recovery_reference(&binding, &relaunch).await?;
        #[cfg(test)]
        self.hold_recovery(&id).await;
        let relaunch_pin = host::relaunch_pin(&binding.launch_binding, &relaunch.definition);
        let RelaunchPlan {
            launch,
            session_ref,
            program,
            validated_program,
            profile:
                RecoveryProfile {
                    env,
                    manifest: manifest_override,
                    revision,
                },
            ..
        } = relaunch;
        let env_extra = self.launch_env(&env, binding.agent_base.clone(), &id);
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
        // re-persist keeps the same launch-time shape and the profile revision the
        // relaunch ran under.
        let snapshot = ResumeSnapshot {
            program,
            args: binding.args.clone(),
            native: Some(launch),
            launch_binding: relaunch_pin,
            reference_provenance: binding.native_reference_provenance,
            profile_revision: revision,
        };
        // A resumed session relaunches in its recorded cwd, which already is the
        // worktree path for worktree sessions (the worktree persists on disk
        // across a daemon restart). With the unified store the session's worktree
        // metadata (repo/branch/worktree_path) is restored too, so inspect/list
        // show it again after a restart.
        let (repo, branch, worktree_path) =
            self.restore_worktree_metadata(&binding.session_id).await;
        // The project context was captured on the binding when it was persisted
        // (F5), so restore it directly: no git re-detection on the cwd at startup,
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
                warnings: outdated_hooks,
                // The live entry's launch marker is carried when the resumed entry
                // is installed.
                reference_launch: None,
                initial_input_pending: false,
                prompt_in_argv: false,
                package_authority,
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

/// Confirms that the native reference still names an existing conversation.
///
/// An assigned reference verifies against the check its launch declares. A
/// hook runtime's id-kind reference verifies against the
/// `[native_reference.existence]` check its runtime descriptor declares; a hook
/// runtime that leaves the check undeclared pointed the reference there itself.
/// The check reads its variables and `HOME` from the environment the agent is
/// launched with (the filtered base environment, then the profile
/// environment), first refuses a relaunch whose arguments, store variable or
/// settings move the store out of reach (with the binding's arguments and
/// working directory), and runs off the async runtime because it reads files
/// and lists directories.
///
/// # Errors
///
/// Returns `agent_native_store_relocated` when the agent would keep its
/// conversations elsewhere, and `agent_native_reference_missing` when the
/// conversation is not found or cannot be verified; the caller must not launch
/// anything.
async fn verify_recovery_reference(
    binding: &ResumeBinding,
    relaunch: &RelaunchPlan,
) -> Result<(), ProtocolError> {
    let Some(existence) =
        reference_existence(binding, Some(&relaunch.launch), &relaunch.definition)
    else {
        return Ok(());
    };
    let session_ref = relaunch.session_ref.clone();
    let profile_env = relaunch.profile.env.clone();
    let base_environment = relaunch.base_environment.clone();
    let args = binding.args.clone();
    let cwd = binding.cwd.clone();
    let checked = existence.clone();
    let verdict = tokio::task::spawn_blocking(move || {
        check_recovery_reference(
            &checked,
            &session_ref,
            &base_environment,
            profile_env.as_slice(),
            &args,
            &cwd,
        )
    })
    .await
    .map_err(|_join_error| {
        // Covers the whole check: the store refusal and the existence scan.
        ProtocolError::new(
            ErrorClass::Daemon,
            "native_store_check_task_panicked",
            format!(
                "the native reference check of session {} panicked; nothing was launched",
                binding.session_id
            ),
            None,
        )
    })?;
    verdict.map_err(|failure| {
        warn!(
            session_id = %binding.session_id,
            failure = %failure,
            "native reference is not recoverable"
        );
        existence.refusal(failure, crate::agent::RefusalPhase::Recovery)
    })
}

/// The reference check resume runs before it relaunches a session: first the
/// relocation refusal for the launch's arguments, working directory and store
/// variable, then the existence scan of `session_ref`. Both read the launch
/// environment, which is `base_environment` overlaid by `profile_env`.
///
/// Reads files and lists directories: the async resume path runs it on the
/// blocking pool, and the synchronous upgrade preflight calls it directly.
pub(super) fn check_recovery_reference(
    existence: &ReferenceExistence,
    session_ref: &SessionRef,
    base_environment: &BaseEnv,
    profile_env: &[(String, String)],
    args: &[String],
    cwd: &std::path::Path,
) -> Result<(), ReferenceCheckFailure> {
    let lookup = |name: &str| effective_variable(base_environment, profile_env, name);
    existence.check_relocation(&LaunchView {
        env: &lookup,
        args,
        cwd,
    })?;
    existence.verify(session_ref, &lookup)
}

/// The existence check the native-reference preflight and the last-activity
/// enrichment run on `binding`, from the runtime descriptor alone.
///
/// An assigned reference carries its check frozen into the launch spec; a hook
/// runtime's id-kind reference uses the `[native_reference.existence]`
/// declaration of the descriptor. A hook reference of a runtime that leaves
/// the check undeclared, and any path-kind reference, verifies nothing: the
/// runtime pointed the reference into its own store.
#[must_use]
pub(super) fn reference_existence(
    binding: &ResumeBinding,
    launch: Option<&NativeSessionLaunch>,
    definition: &RuntimeDefinition,
) -> Option<ReferenceExistence> {
    if binding.reference_kind() != Some(SessionRefKind::Id) {
        return None;
    }
    match binding.native_reference_provenance {
        NativeReferenceProvenance::Assigned => launch
            .and_then(NativeSessionLaunch::assigned)
            .map(|assigned| assigned.existence().clone()),
        NativeReferenceProvenance::Reported => definition.hook_existence().cloned(),
    }
}

/// Clears the resume and fork capabilities of a listed session that the
/// recovery `definition` would refuse, and records the error code each
/// refused operation returns in [`SessionInfo::capability_refusals`]. A
/// capability is never set here.
///
/// The checks follow [`SessionRegistry::resume_with`] and
/// [`SessionRegistry::fork`] past their frozen capability gate: both resolve
/// the definition first, then resume needs a native launch spec and fork the
/// fork contract.
fn clear_refused_capabilities(
    info: &mut SessionInfo,
    binding: &ResumeBinding,
    definition: Result<&Arc<RuntimeDefinition>, &ProtocolError>,
) {
    let resume = info.capabilities.resume.then(|| {
        let definition = definition.map_err(|error| error.code.clone())?;
        binding_native_launch(binding, definition)
            .map(drop)
            .ok_or_else(|| agent_not_resumable(&binding.agent).code)
    });
    let fork = info.capabilities.fork.then(|| {
        let definition = definition.map_err(|error| error.code.clone())?;
        recovery_contract(binding, definition, true).map_err(|error| error.code)
    });
    info.capabilities.resume = matches!(resume, Some(Ok(())));
    info.capabilities.fork = matches!(fork, Some(Ok(())));
    let refusals = protocol::SessionCapabilityRefusals {
        resume: resume.and_then(Result::err),
        fork: fork.and_then(Result::err),
    };
    info.capability_refusals = (!refusals.is_empty()).then_some(refusals);
}

/// Whether a profile whose revision was frozen over a pin the definition now
/// serves by adoption still matches that revision.
///
/// The frozen revision digests the binding the session stored, so the package
/// that now serves the runtime cannot reproduce it. The match is accepted only
/// when the definition is an adoption of the pin and honors the frozen launch
/// for this operation. For a built-in pin ([`host::adopts`]) the profile's pin
/// keys, if any, name that package. For an older version of the package
/// ([`host::adopts_newer`]) the profile pins the newer version, and putting
/// the frozen pin back reproduces the frozen revision. A session that resolves
/// its exact frozen digest is matched by [`repinned_profile_matches_in`].
fn adopted_profile_matches_in(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    name: &str,
    definition: &RuntimeDefinition,
    frozen: &ProfileRevision,
    fork: bool,
    resolutions: &mut PinResolutions,
) -> Result<bool, ProtocolError> {
    let built_in = host::adopts(&binding.launch_binding, definition);
    let newer = host::adopts_newer(&binding.launch_binding, definition);
    if !(built_in || newer) {
        return repinned_profile_matches_in(
            profiles,
            binding,
            name,
            definition,
            frozen,
            resolutions,
        );
    }
    if recovery_contract(binding, definition, fork).is_err() {
        return Ok(false);
    }
    let (
        Some(stored),
        BindingProvenance::Package {
            package,
            package_digest,
        },
    ) = (
        binding.launch_binding.binding(),
        &definition.binding().provenance,
    )
    else {
        return Ok(false);
    };
    if binding.program.is_empty() {
        return Ok(false);
    }
    let launch = (binding.program.as_str(), binding.args.as_slice());
    if built_in {
        return profiles.frozen_builtin_matches(
            name,
            stored,
            launch,
            (&package.id, package_digest),
            frozen,
        );
    }
    let BindingProvenance::Package {
        package: frozen_package,
        package_digest: frozen_digest,
    } = &stored.provenance
    else {
        return Ok(false);
    };
    let launched = Pin {
        package: frozen_package.id.clone(),
        digest: frozen_digest.clone(),
    };
    let served = Pin {
        package: package.id.clone(),
        digest: package_digest.clone(),
    };
    // A profile repinned while the session was recovering is recorded under the
    // frozen binding with its new pin, so the text as it stands reproduces the
    // frozen revision.
    Ok(
        profiles.frozen_package_matches(name, stored, launch, (&launched, &served), frozen)?
            || profiles.frozen_package_matches(name, stored, launch, (&served, &served), frozen)?,
    )
}

/// Whether a profile that a release step repinned still matches the revision
/// frozen into a session that relaunches from its exact frozen digest.
///
/// The step moves a profile to a strictly newer official version of the
/// official package the session was launched from. When that version does not
/// adopt the session (another integration, reference check or launch
/// contract), the session resumes from its frozen digest as it did before the
/// step. The profile then differs from its frozen revision only in the pin
/// keys, so it matches when putting the frozen pin back reproduces the frozen
/// revision. Any other pin, or any other change, does not match.
fn repinned_profile_matches_in(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    name: &str,
    definition: &RuntimeDefinition,
    frozen: &ProfileRevision,
    resolutions: &mut PinResolutions,
) -> Result<bool, ProtocolError> {
    let Some(stored) = binding.launch_binding.binding() else {
        return Ok(false);
    };
    let BindingProvenance::Package {
        package: frozen_package,
        package_digest: frozen_digest,
    } = &stored.provenance
    else {
        return Ok(false);
    };
    if definition.binding() != stored || binding.program.is_empty() {
        return Ok(false);
    }
    let ProfilePin::Pinned(current) = profiles.profile_pin(name)? else {
        return Ok(false);
    };
    if !resolutions.pins_newer_official(
        profiles.runtimes(),
        stored,
        &current.package,
        &current.digest,
    ) {
        return Ok(false);
    }
    let launched = Pin {
        package: frozen_package.id.clone(),
        digest: frozen_digest.clone(),
    };
    profiles.frozen_package_matches(
        name,
        stored,
        (binding.program.as_str(), binding.args.as_slice()),
        (&launched, &current),
        frozen,
    )
}

/// Resolves the host profile `binding` was launched from and checks it against
/// the revision frozen into the binding, as a resume or a fork of `binding`
/// does. Resume and the upgrade preflight both run this one check.
///
/// `definition` is the [`recovery_definition`] the operation resolved; the
/// profile is built on it, so the definition and the profile revision never
/// come from two resolutions. `None` when the definition did not resolve: the
/// profile is then resolved by name against what the registry serves now, to
/// report its own error.
///
/// # Errors
///
/// Returns `agent_profile_missing` when a profile the session was launched
/// from (frozen, or named by a binding that predates freezing) does not
/// resolve, `agent_profile_changed` when its revision differs from the
/// frozen one or none was frozen and `change` is [`ProfileChange::Refuse`],
/// and `agent_profile_revision_unavailable` when the revision key cannot be
/// read.
pub(super) fn recovery_profile_in(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    definition: Option<&Arc<RuntimeDefinition>>,
    change: ProfileChange,
    fork: bool,
) -> Result<RecoveryProfile, ProtocolError> {
    recovery_profile_memo(
        profiles,
        binding,
        definition,
        change,
        fork,
        &mut PinResolutions::default(),
    )
}

/// [`recovery_profile_in`] resolving the pin judgements through `resolutions`.
pub(super) fn recovery_profile_memo(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    definition: Option<&Arc<RuntimeDefinition>>,
    change: ProfileChange,
    fork: bool,
    resolutions: &mut PinResolutions,
) -> Result<RecoveryProfile, ProtocolError> {
    let frozen = binding.profile_revision.as_ref();
    let resolved = match definition.map_or_else(
        || profiles.resolve_agent(&binding.agent),
        |definition| profiles.resolve_agent_pinned(&binding.agent, definition),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            warn!(
                session_id = %binding.session_id,
                agent = %binding.agent,
                error_code = %error.code,
                "agent profile no longer resolves"
            );
            return Err(profile_missing(binding, &error.code));
        }
    };
    let Some(profile) = resolved.profile else {
        return match frozen {
            Some(_) => Err(profile_missing(binding, "profile_removed")),
            None => Ok(RecoveryProfile::none()),
        };
    };
    let keys = profiles.revision_keys();
    let current = keys.revision(&profile.inputs)?;
    let unchanged = match frozen {
        Some(frozen) => {
            keys.matches(&profile.inputs, frozen)?
                || adopted_profile_matches_in(
                    profiles,
                    binding,
                    &resolved.name,
                    &resolved.definition,
                    frozen,
                    fork,
                    resolutions,
                )?
        }
        None => false,
    };
    if !unchanged && change == ProfileChange::Refuse {
        return Err(profile_changed(binding, frozen.is_some()));
    }
    if !unchanged {
        warn!(
            session_id = %binding.session_id,
            agent = %binding.agent,
            "relaunching under a changed agent profile the owner accepted"
        );
    }
    Ok(RecoveryProfile {
        env: profile.env,
        manifest: profile.manifest,
        revision: Some(current),
    })
}

pub(super) fn binding_program(binding: &ResumeBinding, definition: &RuntimeDefinition) -> String {
    if binding.program.is_empty() {
        definition.program().as_str().to_owned()
    } else {
        binding.program.clone()
    }
}

/// Resolves the definition a resume of `binding` launches from: the runtime
/// its launch pin names, which a package serves only when it is the
/// adoption of an official alias (`host::adopts`) or of an older version of
/// the same official package (`host::adopts_newer`) that honors the frozen
/// launch.
///
/// A newer version adopts the session only when it honors everything the
/// frozen launch supports (resume, and fork when the frozen spec forks), and,
/// for a session launched from a host profile, only when that profile now pins
/// the newer version: a profile still pinned to the frozen digest keeps the
/// session on it. Otherwise the session resolves its exact digest, as it would
/// without the newer version.
///
/// The stored binding is never changed here, so a refusal leaves the session
/// resumable once the runtime is served again.
///
/// # Errors
///
/// Returns `agent_kind_unsupported` for a value that is not a runtime id,
/// `runtime_builtin_removed` for a built-in or unpinned snapshot of an
/// official alias nothing serves, `runtime_served_by_package` for a built-in
/// pin an official package serves without honoring the frozen launch,
/// `runtime_not_installed` for any other unresolvable or mismatching runtime
/// and `runtime_incompatible` for a pinned package whose root fails
/// verification.
pub(super) fn recovery_definition(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
    recovery_definition_memo(profiles, binding, &mut PinResolutions::default())
}

/// The pin resolutions of one batch of bindings.
///
/// Resolving a pin reads the registry record and verifies the package root, so
/// a listing resolves each distinct runtime source and frozen launch once
/// instead of once per session.
#[derive(Default)]
pub(super) struct PinResolutions {
    adopters: Vec<(RuntimeRef, LaunchPin, Option<Arc<RuntimeDefinition>>)>,
    pinned: Vec<(
        RuntimeRef,
        LaunchPin,
        Result<Arc<RuntimeDefinition>, ProtocolError>,
    )>,
    newer_official: Vec<(LaunchBinding, PackageId, PackageDigest, bool)>,
}

impl PinResolutions {
    fn adopter(
        &mut self,
        runtimes: &RuntimeHost,
        binding: &ResumeBinding,
    ) -> Option<Arc<RuntimeDefinition>> {
        let known = self
            .adopters
            .iter()
            .find(|(base, pin, _)| *base == binding.agent_base && *pin == binding.launch_binding);
        if let Some((_, _, adopter)) = known {
            return adopter.clone();
        }
        let adopter = runtimes.newer_adopter(&binding.agent_base, &binding.launch_binding);
        self.adopters.push((
            binding.agent_base.clone(),
            binding.launch_binding.clone(),
            adopter.clone(),
        ));
        adopter
    }

    /// [`RuntimeHost::pins_newer_official`], which reads the registry record.
    fn pins_newer_official(
        &mut self,
        runtimes: &RuntimeHost,
        frozen: &LaunchBinding,
        package: &PackageId,
        digest: &PackageDigest,
    ) -> bool {
        let known = self.newer_official.iter().find(|(binding, id, pinned, _)| {
            binding == frozen && id == package && pinned == digest
        });
        if let Some((_, _, _, pins)) = known {
            return *pins;
        }
        let pins = runtimes.pins_newer_official(frozen, package, digest);
        self.newer_official
            .push((frozen.clone(), package.clone(), digest.clone(), pins));
        pins
    }

    fn pinned(
        &mut self,
        runtimes: &RuntimeHost,
        binding: &ResumeBinding,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        let known = self
            .pinned
            .iter()
            .find(|(base, pin, _)| *base == binding.agent_base && *pin == binding.launch_binding);
        if let Some((_, _, definition)) = known {
            return definition.clone();
        }
        let definition = runtimes.resolve_pinned(&binding.agent_base, &binding.launch_binding);
        self.pinned.push((
            binding.agent_base.clone(),
            binding.launch_binding.clone(),
            definition.clone(),
        ));
        definition
    }
}

/// [`recovery_definition`] resolving the pin through `resolutions`.
pub(super) fn recovery_definition_memo(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    resolutions: &mut PinResolutions,
) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
    let runtimes = profiles.runtimes();
    if let Some(adopter) = resolutions.adopter(runtimes, binding) {
        if newer_adoption_holds(profiles, binding, &adopter) {
            return Ok(adopter);
        }
    }
    let definition = resolutions.pinned(runtimes, binding)?;
    recovery_contract(binding, &definition, false)?;
    Ok(definition)
}

/// Whether the newer package version `adopter` may serve `binding`'s relaunch:
/// it honors the frozen resume, and the frozen fork when there is one, and a
/// host profile the session was launched from pins `adopter`.
fn newer_adoption_holds(
    profiles: &ProfileRegistry,
    binding: &ResumeBinding,
    adopter: &RuntimeDefinition,
) -> bool {
    let forks = binding
        .native_launch
        .as_ref()
        .is_some_and(NativeSessionLaunch::supports_fork);
    if recovery_contract(binding, adopter, false).is_err()
        || (forks && recovery_contract(binding, adopter, true).is_err())
    {
        return false;
    }
    let BindingProvenance::Package {
        package,
        package_digest,
    } = &adopter.binding().provenance
    else {
        return false;
    };
    match profiles.profile_pin(&binding.agent) {
        // A bare runtime session: the agent name is the runtime id and no
        // profile revision was frozen.
        Ok(ProfilePin::Absent) => binding.profile_revision.is_none(),
        Ok(ProfilePin::Pinned(pin)) => pin.package == package.id && &pin.digest == package_digest,
        Ok(ProfilePin::Unpinned) => false,
        // The exact-digest resolution and the profile check report the profile
        // error themselves.
        Err(error) => {
            tracing::debug!(
                session_id = %binding.session_id,
                agent = %binding.agent,
                error_code = %error.code,
                "the session's host profile cannot be read, so a newer package does not adopt it"
            );
            false
        }
    }
}

/// Checks that `definition` honors the launch `binding` froze for a resume,
/// or for a fork when `fork` is set (see [`host::honors_frozen_launch`]).
///
/// A fork needs fork support in the frozen spec, which is the spec fork
/// launches from, and in `definition`.
///
/// # Errors
///
/// Returns `agent_fork_unsupported` for a fork the frozen spec does not
/// support, and `runtime_served_by_package` for an adopted package that does
/// not honor the frozen launch.
pub(super) fn recovery_contract(
    binding: &ResumeBinding,
    definition: &RuntimeDefinition,
    fork: bool,
) -> Result<(), ProtocolError> {
    let frozen = if fork {
        Some(frozen_fork_launch(binding)?)
    } else {
        binding_native_launch(binding, definition)
    };
    host::honors_frozen_launch(&binding.launch_binding, definition, frozen.as_ref(), fork)
}

/// The launch spec a fork of `binding` runs from: only a spec frozen into the
/// binding can fork, never the base kind's compiled spec.
///
/// # Errors
///
/// Returns `agent_fork_unsupported` when the frozen spec does not fork.
fn frozen_fork_launch(binding: &ResumeBinding) -> Result<NativeSessionLaunch, ProtocolError> {
    binding
        .native_launch
        .clone()
        .filter(NativeSessionLaunch::supports_fork)
        .ok_or_else(agent_fork_unsupported)
}

/// Return the native-session launch spec frozen into a persisted binding.
///
/// A binding written without a snapshot program carries no frozen spec and
/// resolves to its runtime definition's spec; a binding with a snapshot is
/// authoritative, so an absent spec means the session does not recover natively.
pub(super) fn binding_native_launch(
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

pub(super) fn session_ref_from_binding(
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
