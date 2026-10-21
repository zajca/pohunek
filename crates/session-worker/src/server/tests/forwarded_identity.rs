//! Forwarded active-identity operations against the worker's request handlers.
//!
//! The handlers run on a real PTY tree and a real journal file; the expected
//! outcomes come from the forwarding contract (ordering against hook claims,
//! subject validation, the revision, durability before the acknowledgement),
//! not from the handlers' own output.

use pohunek_worker_protocol::{
    ActiveIdentityOutcome, ClaimOrigin, ControlCode, ForwardedActiveRelease, ForwardedActiveReport,
    IdentityRejectReason, LeaseId, ProcessIdentity, ReportedSession, ReportedSessionKind,
    RequestKind, ResponseKind, RuntimePhase as WireRuntimePhase, RuntimeScope, SessionId, WorkerId,
    WorkerInstanceId, CURRENT_VERSION, PREVIOUS_VERSION,
};

use super::{
    controller_connection, expiry, launch_claim_fixture, launch_claim_fixture_running, send_hook,
    Connection,
};
use crate::server::{handle_request, Server};
use crate::PtyOwner;
use pohunek_platform::process::{HostInspector, ProcessInspector};

fn scope(lease_id: &LeaseId) -> RuntimeScope {
    RuntimeScope {
        lease_id: lease_id.clone(),
        session_id: SessionId::new("s-112").expect("session id"),
        worker_id: WorkerId::new("worker-claim").expect("worker id"),
        worker_instance_id: WorkerInstanceId::new("runtime-claim").expect("runtime id"),
    }
}

fn root_process(pty: &PtyOwner) -> ProcessIdentity {
    ProcessIdentity {
        pid: pty.identity().pid,
        start_identity: pty
            .identity()
            .start_identity
            .parse()
            .expect("decimal start identity"),
    }
}

fn report(process: ProcessIdentity, sequence: u64) -> ForwardedActiveReport {
    ForwardedActiveReport {
        provider: "claude".to_owned(),
        agent: Some("tuned".to_owned()),
        process,
        sequence,
        expires_at: expiry(),
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Id,
            value: "native-annotation".to_owned(),
            transcript_path: Some("/work/native-annotation.jsonl".to_owned()),
        }),
    }
}

fn release(process: ProcessIdentity, sequence: u64) -> ForwardedActiveRelease {
    ForwardedActiveRelease {
        provider: "claude".to_owned(),
        process,
        sequence,
    }
}

/// Sends `kind` through the request dispatcher and returns the outcome of an
/// `ActiveIdentityRecorded` answer.
async fn forward(
    server: &Server,
    connection: &mut Connection,
    kind: RequestKind,
) -> Result<ActiveIdentityOutcome, pohunek_worker_protocol::ControlError> {
    let response = handle_request(
        &server.shared,
        connection,
        pohunek_worker_protocol::ControlRequest {
            request_id: pohunek_worker_protocol::RequestId::new("forward-1").expect("request id"),
            kind,
        },
    )
    .await;
    match response.kind {
        ResponseKind::ActiveIdentityRecorded { outcome } => Ok(outcome),
        ResponseKind::Error { error } => Err(error),
        other => panic!("unexpected answer {other:?}"),
    }
}

async fn record(
    server: &Server,
    connection: &mut Connection,
    lease_id: &LeaseId,
    report: ForwardedActiveReport,
) -> ActiveIdentityOutcome {
    forward(
        server,
        connection,
        RequestKind::RecordActiveIdentity {
            scope: scope(lease_id),
            report,
        },
    )
    .await
    .expect("the request is served")
}

async fn release_active(
    server: &Server,
    connection: &mut Connection,
    lease_id: &LeaseId,
    release: ForwardedActiveRelease,
) -> ActiveIdentityOutcome {
    forward(
        server,
        connection,
        RequestKind::ReleaseActiveIdentity {
            scope: scope(lease_id),
            release,
        },
    )
    .await
    .expect("the request is served")
}

fn rejected(reason: IdentityRejectReason) -> ActiveIdentityOutcome {
    ActiveIdentityOutcome::Rejected { reason }
}

async fn stop(server: &Server, pty: &PtyOwner) {
    pty.stop(
        "test-cleanup",
        server.shared.config.stop_grace,
        server.shared.config.kill_reap_deadline,
    )
    .await
    .expect("stop the PTY");
}

#[tokio::test]
async fn an_accepted_report_is_journaled_before_the_answer_and_never_touches_the_launch_state() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);

    let outcome = record(
        &server,
        &mut connection,
        &lease_id,
        report(root_process(&pty), 10),
    )
    .await;

    assert_eq!(outcome, ActiveIdentityOutcome::Accepted { revision: 1 });
    let journaled = server.shared.journal.load().expect("load the journal");
    let claim = journaled.active_identity.as_ref().expect("journaled claim");
    assert_eq!(
        (claim.provider.as_str(), claim.sequence, claim.origin),
        ("claude", 10, Some(ClaimOrigin::Forwarded))
    );
    assert_eq!(claim.agent.as_deref(), Some("tuned"));
    assert_eq!(
        claim
            .reported_session
            .as_ref()
            .map(|s| (s.value.as_str(), s.transcript_path.as_deref())),
        Some(("native-annotation", Some("/work/native-annotation.jsonl"))),
        "the id and its transcript path are journaled together"
    );
    assert_eq!(
        (&claim.reference_kind, &claim.native_reference),
        (&None, &None)
    );
    assert_eq!(journaled.active_identity_revision, 1);
    assert!(journaled.launch_identity.is_none());
    assert!(journaled.native_reference_claim.is_none());
    assert!(journaled.pending_launch_claims.is_empty());
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_forwarded_report_older_than_a_hook_claim_of_the_same_process_is_stale() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let hook = serde_json::json!({
        "type": "identity_report", "provider": "claude", "sequence": 20,
        "expires_at": expiry(), "reference_kind": "id", "native_reference": "hooked",
    });
    assert!(send_hook(&server, &pty, hook).await);
    let before = server.shared.journal.load().expect("load the journal");

    let outcome = record(
        &server,
        &mut connection,
        &lease_id,
        report(root_process(&pty), 19),
    )
    .await;

    assert_eq!(outcome, rejected(IdentityRejectReason::SequenceStale));
    let after = server.shared.journal.load().expect("load the journal");
    assert_eq!(after.active_identity, before.active_identity);
    assert_eq!(
        after.active_identity_revision,
        before.active_identity_revision
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_hook_claim_older_than_a_forwarded_report_of_the_same_process_is_stale() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    assert_eq!(
        record(
            &server,
            &mut connection,
            &lease_id,
            report(root_process(&pty), 30)
        )
        .await,
        ActiveIdentityOutcome::Accepted { revision: 1 }
    );

    let hook = serde_json::json!({
        "type": "identity_report", "provider": "claude", "sequence": 29,
        "expires_at": expiry(), "reference_kind": "id", "native_reference": "late",
    });

    assert!(
        !send_hook(&server, &pty, hook).await,
        "the hook claim is older than the forwarded one"
    );
    let journaled = server.shared.journal.load().expect("load the journal");
    assert_eq!(journaled.active_identity_revision, 1);
    assert_eq!(
        journaled.active_identity.map(|claim| claim.sequence),
        Some(30)
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_hook_claim_advances_the_revision_with_a_forwarded_one() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    assert!(
        send_hook(
            &server,
            &pty,
            serde_json::json!({
                "type": "identity_report", "provider": "claude", "sequence": 1,
                "expires_at": expiry(), "reference_kind": "id", "native_reference": "hooked",
            })
        )
        .await
    );
    assert_eq!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity_revision,
        1
    );

    let outcome = record(
        &server,
        &mut connection,
        &lease_id,
        report(root_process(&pty), 2),
    )
    .await;

    assert_eq!(outcome, ActiveIdentityOutcome::Accepted { revision: 2 });
    let hook_release = serde_json::json!({
        "type": "identity_release", "provider": "claude", "sequence": 3,
    });
    assert!(send_hook(&server, &pty, hook_release).await);
    assert_eq!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity_revision,
        3
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_subject_outside_the_pty_tree_or_with_a_reused_start_identity_is_refused() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let outside = ProcessIdentity {
        pid: std::process::id(),
        start_identity: 1,
    };
    let reused = ProcessIdentity {
        start_identity: root.start_identity + 1,
        ..root
    };

    for subject in [outside, reused] {
        assert_eq!(
            record(&server, &mut connection, &lease_id, report(subject, 5)).await,
            rejected(IdentityRejectReason::ReportedProcessInvalid),
            "{subject:?}"
        );
    }

    let journaled = server.shared.journal.load().expect("load the journal");
    assert!(journaled.active_identity.is_none());
    assert_eq!(journaled.active_identity_revision, 0);
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_matching_release_journals_a_tombstone_and_a_stale_or_unmatched_one_does_not() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    record(&server, &mut connection, &lease_id, report(root, 40)).await;

    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(root, 40)).await,
        rejected(IdentityRejectReason::SequenceStale),
        "a release must be newer than the claim it ends"
    );
    let unmatched = ForwardedActiveRelease {
        provider: "codex".to_owned(),
        ..release(root, 41)
    };
    assert_eq!(
        release_active(&server, &mut connection, &lease_id, unmatched).await,
        rejected(IdentityRejectReason::ReleaseUnmatched)
    );
    assert_eq!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity_revision,
        1,
        "refused releases change nothing"
    );

    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(root, 42)).await,
        ActiveIdentityOutcome::Accepted { revision: 2 }
    );
    let journaled = server.shared.journal.load().expect("load the journal");
    assert!(journaled.active_identity.is_none());
    let tombstone = journaled
        .active_identity_release
        .expect("release tombstone");
    assert_eq!(
        (tombstone.provider.as_str(), tombstone.sequence),
        ("claude", 42)
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_label_or_annotation_outside_its_bound_is_refused() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let oversized = ForwardedActiveReport {
        agent: Some("a".repeat(pohunek_worker_protocol::MAX_FORWARDED_AGENT_LABEL_BYTES + 1)),
        ..report(root, 6)
    };
    let control = ForwardedActiveReport {
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Id,
            value: "bad\nid".to_owned(),
            transcript_path: None,
        }),
        ..report(root, 7)
    };

    let transcript_control = ForwardedActiveReport {
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Id,
            value: "id".to_owned(),
            transcript_path: Some("/work/bad\npath".to_owned()),
        }),
        ..report(root, 8)
    };
    let transcript_on_a_path = ForwardedActiveReport {
        reported_session: Some(ReportedSession {
            kind: ReportedSessionKind::Path,
            value: "/work/session.jsonl".to_owned(),
            transcript_path: Some("/work/other.jsonl".to_owned()),
        }),
        ..report(root, 9)
    };

    for invalid in [oversized, control, transcript_control, transcript_on_a_path] {
        assert_eq!(
            record(&server, &mut connection, &lease_id, invalid).await,
            rejected(IdentityRejectReason::ForwardedFieldInvalid)
        );
    }
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_report_of_a_provider_or_with_an_expiry_the_schema_refuses_is_rejected() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let foreign = ForwardedActiveReport {
        provider: "unknown-provider".to_owned(),
        ..report(root, 8)
    };
    let expired = ForwardedActiveReport {
        expires_at: "2001-01-01T00:00:00Z".to_owned(),
        ..report(root, 9)
    };

    assert_eq!(
        record(&server, &mut connection, &lease_id, foreign).await,
        rejected(IdentityRejectReason::ProviderNotAllowed)
    );
    assert_eq!(
        record(&server, &mut connection, &lease_id, expired).await,
        rejected(IdentityRejectReason::ClaimExpired)
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn the_snapshot_withholds_the_forwarding_fields_from_a_previous_version_connection() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    record(
        &server,
        &mut connection,
        &lease_id,
        report(root_process(&pty), 50),
    )
    .await;

    let state = server.shared.state.lock().await;
    let current = super::super::inspect_snapshot(&server.shared, &state, CURRENT_VERSION)
        .await
        .expect("current snapshot");
    let previous = super::super::inspect_snapshot(&server.shared, &state, PREVIOUS_VERSION)
        .await
        .expect("previous snapshot");
    drop(state);

    let claim = current.active_identity.expect("current claim");
    assert_eq!(claim.origin, Some(ClaimOrigin::Forwarded));
    assert_eq!(claim.agent.as_deref(), Some("tuned"));
    assert!(claim.reported_session.is_some());
    assert_eq!(current.active_identity_revision, Some(1));
    let claim = previous.active_identity.expect("previous claim");
    assert_eq!(
        (claim.origin, claim.agent, claim.reported_session),
        (None, None, None),
        "a previous-version peer reads a provider-only claim"
    );
    assert_eq!(claim.reference_kind, None);
    assert_eq!(claim.native_reference, None);
    assert_eq!(previous.active_identity_revision, None);
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_connection_below_the_forwarding_version_cannot_forward() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    connection.selected_version = Some(PREVIOUS_VERSION);

    let error = forward(
        &server,
        &mut connection,
        RequestKind::RecordActiveIdentity {
            scope: scope(&lease_id),
            report: report(root_process(&pty), 60),
        },
    )
    .await
    .expect_err("a previous-version connection cannot forward");

    assert_eq!(error.code, ControlCode::WorkerFeatureUnavailable);
    assert_eq!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity_revision,
        0
    );
    stop(&server, &pty).await;
}

/// The process a PTY child started in the background, as a second subject.
async fn background_child(pty: &PtyOwner) -> ProcessIdentity {
    let inspector = HostInspector::new();
    let root = inspector
        .identity(pty.identity().pid)
        .expect("inspect the shell")
        .expect("the shell is live");
    let child = pohunek_test_support::wait::wait_until("the shell to start its child", || async {
        inspector
            .descendant_identities(root)
            .ok()
            .and_then(|children| children.into_iter().next())
    })
    .await;
    ProcessIdentity {
        pid: child.pid,
        start_identity: child.start_identity.get(),
    }
}

#[tokio::test]
async fn an_unmatched_release_still_makes_earlier_claims_of_its_process_stale() {
    let (server, pty, _directory) = launch_claim_fixture_running("sleep 600 & wait").await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let child = background_child(&pty).await;

    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(child, 70)).await,
        rejected(IdentityRejectReason::ReleaseUnmatched)
    );

    assert_eq!(
        record(&server, &mut connection, &lease_id, report(child, 69)).await,
        rejected(IdentityRejectReason::SequenceStale),
        "the unmatched release ordered the process"
    );
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(root, 1)).await,
        ActiveIdentityOutcome::Accepted { revision: 1 },
        "sequences of different processes are never compared"
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_replaced_claim_does_not_reopen_the_sequences_of_its_process() {
    let (server, pty, _directory) = launch_claim_fixture_running("sleep 600 & wait").await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let child = background_child(&pty).await;
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(root, 80)).await,
        ActiveIdentityOutcome::Accepted { revision: 1 }
    );
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(child, 5)).await,
        ActiveIdentityOutcome::Accepted { revision: 2 }
    );

    assert_eq!(
        record(&server, &mut connection, &lease_id, report(root, 79)).await,
        rejected(IdentityRejectReason::SequenceStale),
        "the first process's mark outlives its replaced claim"
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_report_accepted_from_another_process_ends_the_background_snapshot_of_the_owner() {
    let (server, pty, _directory) = launch_claim_fixture_running("sleep 600 & wait").await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let child = background_child(&pty).await;
    let mut state = server.shared.state.lock().await;
    state.journal.background_owner = Some(crate::journal::BackgroundOwner {
        pid: root.pid,
        start_identity: root.start_identity.to_string(),
    });
    state.journal.background_sequence = 1;
    state.journal.background = Some(pohunek_worker_protocol::BackgroundSnapshot {
        revision: 1,
        observed_at_ms: 0,
        items: Vec::new(),
    });
    drop(state);

    assert_eq!(
        record(&server, &mut connection, &lease_id, report(child, 3)).await,
        ActiveIdentityOutcome::Accepted { revision: 1 }
    );

    let journaled = server.shared.journal.load().expect("load the journal");
    assert!(
        journaled.background.is_none(),
        "the owner did not report the new active identity"
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_replaced_release_tombstone_does_not_reopen_the_sequences_of_its_process() {
    let (server, pty, _directory) = launch_claim_fixture_running("sleep 600 & wait").await;
    let (mut connection, lease_id) = controller_connection(&server);
    let root = root_process(&pty);
    let child = background_child(&pty).await;
    record(&server, &mut connection, &lease_id, report(child, 5)).await;
    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(child, 6)).await,
        ActiveIdentityOutcome::Accepted { revision: 2 }
    );
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(root, 1)).await,
        ActiveIdentityOutcome::Accepted { revision: 3 },
        "another process's claim replaces the tombstone"
    );

    assert_eq!(
        record(&server, &mut connection, &lease_id, report(child, 6)).await,
        rejected(IdentityRejectReason::SequenceStale),
        "the released process keeps its mark"
    );
    stop(&server, &pty).await;
}

/// Kills `child` and waits until the host no longer reports it.
async fn exit_child(child: ProcessIdentity) {
    let inspector = HostInspector::new();
    let killed = std::process::Command::new("kill")
        .arg("-9")
        .arg(child.pid.to_string())
        .status()
        .expect("run kill");
    assert!(killed.success(), "the child accepts the signal");
    pohunek_test_support::wait::wait_until("the child to be gone", || async {
        match inspector.identity(child.pid) {
            Ok(Some(current)) if current.start_identity.get() == child.start_identity => None,
            _ => Some(()),
        }
    })
    .await;
}

/// A process that exited after it reported can still release itself, by its
/// exact pid and start identity; no other subject is accepted once it is gone.
#[tokio::test]
async fn a_release_naming_the_exact_journaled_owner_outlives_its_process() {
    let (server, pty, _directory) =
        launch_claim_fixture_running("sleep 600 & sleep 600 & wait").await;
    let (mut connection, lease_id) = controller_connection(&server);
    let child = background_child(&pty).await;
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(child, 5)).await,
        ActiveIdentityOutcome::Accepted { revision: 1 }
    );
    exit_child(child).await;

    let other_start = ProcessIdentity {
        start_identity: child.start_identity + 1,
        ..child
    };
    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(other_start, 6)).await,
        rejected(IdentityRejectReason::ReportedProcessInvalid),
        "the same pid with another start identity is another process"
    );
    assert!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity
            .is_some(),
        "the refused release kept the claim"
    );

    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(child, 6)).await,
        ActiveIdentityOutcome::Accepted { revision: 2 }
    );
    let journaled = server.shared.journal.load().expect("load the journal");
    assert!(journaled.active_identity.is_none());
    assert_eq!(
        journaled
            .active_identity_release
            .map(|released| (released.process.pid, released.sequence)),
        Some((child.pid, 6))
    );
    stop(&server, &pty).await;
}

/// A release is refused outside the Running phase, as a report is, even when it
/// names the exact journaled owner.
#[tokio::test]
async fn a_release_is_refused_once_the_runtime_is_not_running() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);
    let owner = root_process(&pty);
    assert_eq!(
        record(&server, &mut connection, &lease_id, report(owner, 5)).await,
        ActiveIdentityOutcome::Accepted { revision: 1 }
    );
    server.shared.state.lock().await.phase = WireRuntimePhase::Stopping;

    assert_eq!(
        release_active(&server, &mut connection, &lease_id, release(owner, 6)).await,
        rejected(IdentityRejectReason::PhaseNotRunning)
    );
    assert!(
        server
            .shared
            .journal
            .load()
            .expect("load the journal")
            .active_identity
            .is_some(),
        "the refused release kept the claim"
    );
    stop(&server, &pty).await;
}
