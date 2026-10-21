//! Active-identity operations the daemon forwards over the control channel.
//!
//! A forwarded report or release takes the validation of a hook claim that
//! concerns its subject: the hook schema, the claim expiry, the managed PTY
//! tree and the start identity of the process, and the ordering against every
//! earlier claim of that process. The peer-side checks do not apply, because the
//! peer is the lease-authenticated daemon. A forwarded claim never reaches the
//! launch identity, the pending launch claims or the native reference claim.
//! The worker acknowledges only after the journal write returns.

// Rust guideline compliant 2026-10-19

use pohunek_worker_protocol::{
    ActiveIdentityOutcome, ClaimOrigin, ForwardedActiveRelease, ForwardedActiveReport,
    IdentityRejectReason, ACTIVE_IDENTITY_FORWARD_VERSION, MAX_FORWARDED_AGENT_LABEL_BYTES,
};

use super::{
    admit_hook, admit_reporter, background, control_error, identity_pins,
    identity_sequence_is_fresh, is_reporter, log_background_cleared, persist,
    report_background_clear, timestamp, valid_identity_expiry, validate_hook_process,
    validate_scope, ActiveIdentity, ChildIdentity, Connection, ControlCode, ControlError,
    HookAction, JournalRecord, RejectReason, ReleasedIdentity, Reporter, ResponseKind,
    RuntimeScope, Shared, State, WireRuntimePhase, AGENT_CHANGED,
};

/// Records a forwarded public report as the session's active identity.
pub(super) async fn record_active_identity_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    report: ForwardedActiveReport,
) -> Result<ResponseKind, ControlError> {
    require_forward_version(connection)?;
    validate_scope(shared, connection, scope).await?;
    let mut state = shared.state.lock().await;
    let outcome = match admit_report(shared, connection, &mut state, &report) {
        Ok(reporting) => commit_report(shared, &mut state, &report, reporting).await?,
        Err(reason) => ActiveIdentityOutcome::Rejected { reason },
    };
    drop(state);
    Ok(ResponseKind::ActiveIdentityRecorded { outcome })
}

/// Releases the session's active identity on a forwarded public release.
pub(super) async fn release_active_identity_request(
    shared: &Shared,
    connection: &Connection,
    scope: &RuntimeScope,
    release: ForwardedActiveRelease,
) -> Result<ResponseKind, ControlError> {
    require_forward_version(connection)?;
    validate_scope(shared, connection, scope).await?;
    let mut state = shared.state.lock().await;
    let outcome = commit_release(shared, connection, &mut state, &release).await?;
    drop(state);
    Ok(ResponseKind::ActiveIdentityRecorded { outcome })
}

fn require_forward_version(connection: &Connection) -> Result<(), ControlError> {
    match connection.selected_version {
        Some(version) if version >= ACTIVE_IDENTITY_FORWARD_VERSION => Ok(()),
        _ => Err(control_error(
            ControlCode::WorkerFeatureUnavailable,
            "the negotiated protocol cannot forward an active identity",
            false,
        )),
    }
}

/// Logs a refusal under the operation name and returns it.
fn refuse(
    shared: &Shared,
    connection: &Connection,
    operation: &'static str,
    reason: IdentityRejectReason,
) -> IdentityRejectReason {
    super::reject_identity(shared, operation, log_reason(reason), &connection.peer);
    reason
}

/// The structured-log reason code of a refusal.
const fn log_reason(reason: IdentityRejectReason) -> RejectReason {
    match reason {
        IdentityRejectReason::RuntimeMismatch => RejectReason::RuntimeMismatch,
        IdentityRejectReason::PhaseNotRunning => RejectReason::PhaseNotRunning,
        IdentityRejectReason::ProviderNotAllowed => RejectReason::ProviderNotAllowed,
        IdentityRejectReason::ActionNotAllowed => RejectReason::ActionNotAllowed,
        IdentityRejectReason::ClaimExpired => RejectReason::ClaimExpired,
        IdentityRejectReason::ReportedProcessInvalid => RejectReason::ReportedProcessInvalid,
        IdentityRejectReason::SequenceStale => RejectReason::SequenceStale,
        IdentityRejectReason::IdentityReportersExhausted => {
            RejectReason::IdentityReportersExhausted
        }
        IdentityRejectReason::ReleaseUnmatched => RejectReason::ReleaseUnmatched,
        IdentityRejectReason::ForwardedFieldInvalid => RejectReason::ForwardedFieldInvalid,
    }
}

/// Applies every rule of a hook report that concerns its subject.
fn admit_report(
    shared: &Shared,
    connection: &Connection,
    state: &mut State,
    report: &ForwardedActiveReport,
) -> Result<Reporter, IdentityRejectReason> {
    const OPERATION: &str = "forwarded_identity_report";
    if state.phase != WireRuntimePhase::Running {
        return Err(refuse(
            shared,
            connection,
            OPERATION,
            IdentityRejectReason::PhaseNotRunning,
        ));
    }
    let schema = admit_hook(
        state.hook_schema,
        HookAction::IdentityReport,
        &report.provider,
        state.launch_agent_base.as_deref(),
    )
    .map_err(|reason| {
        refuse(
            shared,
            connection,
            OPERATION,
            match reason {
                RejectReason::ActionNotAllowed => IdentityRejectReason::ActionNotAllowed,
                _ => IdentityRejectReason::ProviderNotAllowed,
            },
        )
    })?;
    if !valid_identity_expiry(&report.expires_at) {
        return Err(refuse(
            shared,
            connection,
            OPERATION,
            IdentityRejectReason::ClaimExpired,
        ));
    }
    if !forwarded_fields_valid(report) {
        return Err(refuse(
            shared,
            connection,
            OPERATION,
            IdentityRejectReason::ForwardedFieldInvalid,
        ));
    }
    let root = state
        .pty
        .as_ref()
        .ok_or(IdentityRejectReason::PhaseNotRunning)?
        .identity()
        .clone();
    let reporting = Reporter {
        pid: report.process.pid,
        start_identity: report.process.start_identity,
    };
    if validate_hook_process(
        schema.ancestry,
        reporting.pid,
        reporting.start_identity,
        root.pid,
    )
    .is_err()
    {
        return Err(refuse(
            shared,
            connection,
            OPERATION,
            IdentityRejectReason::ReportedProcessInvalid,
        ));
    }
    if !identity_sequence_is_fresh(
        &state.journal,
        &state.identity_marks,
        reporting,
        report.sequence,
    ) {
        return Err(refuse(
            shared,
            connection,
            OPERATION,
            IdentityRejectReason::SequenceStale,
        ));
    }
    let pinned = identity_pins(&root, state.journal.launch_identity.as_ref());
    if !admit_reporter(
        shared,
        OPERATION,
        &mut state.identity_marks,
        reporting,
        &pinned,
        &connection.peer,
    ) {
        return Err(IdentityRejectReason::IdentityReportersExhausted);
    }
    Ok(reporting)
}

/// Whether the label and the session annotation are inside their bounds.
fn forwarded_fields_valid(report: &ForwardedActiveReport) -> bool {
    report.agent.as_deref().is_none_or(|label| {
        !label.is_empty()
            && label.len() <= MAX_FORWARDED_AGENT_LABEL_BYTES
            && !label.chars().any(char::is_control)
    }) && report
        .reported_session
        .as_ref()
        .is_none_or(pohunek_worker_protocol::ReportedSession::is_valid)
}

/// Journals an admitted report, then moves it into the in-memory state.
async fn commit_report(
    shared: &Shared,
    state: &mut State,
    report: &ForwardedActiveReport,
    reporting: Reporter,
) -> Result<ActiveIdentityOutcome, ControlError> {
    let root = state
        .pty
        .as_ref()
        .ok_or_else(|| control_error(ControlCode::InvalidState, "runtime has no PTY", false))?
        .identity()
        .clone();
    let mut journal = state.journal.clone();
    journal.record_active_identity(ActiveIdentity {
        provider: report.provider.clone(),
        process: ChildIdentity {
            pid: reporting.pid,
            process_group: root.process_group,
            start_identity: reporting.start_identity.to_string(),
        },
        sequence: report.sequence,
        expires_at: report.expires_at.clone(),
        reference_kind: None,
        native_reference: None,
        agent: report.agent.clone(),
        origin: Some(ClaimOrigin::Forwarded),
        reported_session: report.reported_session.clone(),
    });
    // A report accepted from another process ends the background snapshot of
    // the previous owner, on the worker's own clock.
    let background_cleared = report_background_clear(
        &journal,
        reporting.pid,
        reporting.start_identity,
        report.sequence,
        None,
        true,
    )
    .filter(|reason| *reason == AGENT_CHANGED)
    .filter(|_| background::clear_at(&mut journal, background::monotonic_nanos()));
    let revision = journal.active_identity_revision;
    journal_and_adopt(shared, state, journal).await?;
    state.identity_marks.record(reporting, report.sequence);
    if let Some(reason) = background_cleared {
        log_background_cleared(shared, reason);
    }
    Ok(ActiveIdentityOutcome::Accepted { revision })
}

/// Applies the release rules, journals a matching release and answers.
async fn commit_release(
    shared: &Shared,
    connection: &Connection,
    state: &mut State,
    release: &ForwardedActiveRelease,
) -> Result<ActiveIdentityOutcome, ControlError> {
    const OPERATION: &str = "forwarded_identity_release";
    let reject = |reason| ActiveIdentityOutcome::Rejected {
        reason: refuse(shared, connection, OPERATION, reason),
    };
    if state.phase != WireRuntimePhase::Running {
        return Ok(reject(IdentityRejectReason::PhaseNotRunning));
    }
    let schema = match admit_hook(
        state.hook_schema,
        HookAction::IdentityRelease,
        &release.provider,
        state.launch_agent_base.as_deref(),
    ) {
        Ok(schema) => schema,
        Err(RejectReason::ActionNotAllowed) => {
            return Ok(reject(IdentityRejectReason::ActionNotAllowed))
        }
        Err(_) => return Ok(reject(IdentityRejectReason::ProviderNotAllowed)),
    };
    let Some(pty) = state.pty.as_ref() else {
        return Ok(reject(IdentityRejectReason::PhaseNotRunning));
    };
    let root = pty.identity().clone();
    let reporting = Reporter {
        pid: release.process.pid,
        start_identity: release.process.start_identity,
    };
    // The journaled owner was validated against the PTY tree when it reported,
    // and a release that names its exact pid and start identity may outlive the
    // process; any other subject must still be a live member of the tree.
    let names_the_owner = state
        .journal
        .active_identity
        .as_ref()
        .is_some_and(|active| {
            active.provider == release.provider && is_reporter(&active.process, reporting)
        });
    if !names_the_owner
        && validate_hook_process(
            schema.ancestry,
            reporting.pid,
            reporting.start_identity,
            root.pid,
        )
        .is_err()
    {
        return Ok(reject(IdentityRejectReason::ReportedProcessInvalid));
    }
    if !identity_sequence_is_fresh(
        &state.journal,
        &state.identity_marks,
        reporting,
        release.sequence,
    ) {
        return Ok(reject(IdentityRejectReason::SequenceStale));
    }
    let pinned = identity_pins(&root, state.journal.launch_identity.as_ref());
    if !admit_reporter(
        shared,
        OPERATION,
        &mut state.identity_marks,
        reporting,
        &pinned,
        &connection.peer,
    ) {
        return Ok(ActiveIdentityOutcome::Rejected {
            reason: IdentityRejectReason::IdentityReportersExhausted,
        });
    }
    let released_process = state
        .journal
        .active_identity
        .as_ref()
        .filter(|active| {
            active.provider == release.provider && is_reporter(&active.process, reporting)
        })
        .map(|active| active.process.clone());
    let Some(process) = released_process else {
        // A fresh release of a process that is not active ends no identity, but
        // it still makes that process's earlier claims stale when they arrive
        // after it.
        state.identity_marks.record(reporting, release.sequence);
        return Ok(reject(IdentityRejectReason::ReleaseUnmatched));
    };
    let mut journal = state.journal.clone();
    journal.release_active_identity(ReleasedIdentity {
        provider: release.provider.clone(),
        process,
        sequence: release.sequence,
    });
    let revision = journal.active_identity_revision;
    journal_and_adopt(shared, state, journal).await?;
    state.identity_marks.record(reporting, release.sequence);
    Ok(ActiveIdentityOutcome::Accepted { revision })
}

/// Persists `journal` and adopts it as the in-memory journal.
///
/// A failed write leaves the state untouched and is answered as a
/// non-retryable runtime fault, so the daemon can tell it from a refusal.
async fn journal_and_adopt(
    shared: &Shared,
    state: &mut State,
    mut journal: JournalRecord,
) -> Result<(), ControlError> {
    journal.updated_at = timestamp();
    persist(shared.journal.clone(), journal.clone())
        .await
        .map_err(|error| control_error(ControlCode::RuntimeFault, error, false))?;
    state.journal = journal;
    Ok(())
}
