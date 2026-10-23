//! Prompt-acceptance reports and write marks against the worker's handlers.
//!
//! The handlers run on a real PTY and a real journal file. Expected outcomes
//! come from the delivery-evidence contract (who may report, how reports are
//! ordered, which protocol versions see the fields), not from the handlers'
//! own output: sequences are literals and the submit mark is bracketed by the
//! test's own `CLOCK_MONOTONIC` readings.

use pohunek_worker_protocol::{
    InputFragment, InputPlan, InspectSnapshot, LeaseId, ResponseKind, RuntimeScope, SecretBytes,
    SessionId, Version, WorkerId, WorkerInstanceId, WriteId, CURRENT_VERSION, PREVIOUS_VERSION,
};

use super::{
    controller_connection, expiry, launch_claim_fixture, launch_report, send_hook, Connection,
};
use crate::server::{inspect_snapshot, write_request, Server};
use crate::PtyOwner;

/// Sequence of the first applied prompt report. The worker clamps a stored
/// sequence to its own clock, and this literal is far below any real reading.
const FIRST_SEQUENCE: u64 = 100;

fn prompt(provider: &str, sequence: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "prompt_accepted", "provider": provider, "sequence": sequence,
        "expires_at": expiry(),
    })
}

async fn snapshot(server: &Server, version: Version) -> InspectSnapshot {
    let state = server.shared.state.lock().await;
    inspect_snapshot(&server.shared, &state, version)
        .await
        .expect("inspect the worker")
}

/// Establishes the launch identity with the fixture's own process.
async fn establish_launch(server: &Server, pty: &PtyOwner) {
    assert!(send_hook(server, pty, launch_report(pty, 1, "native-1")).await);
    assert!(
        server
            .shared
            .state
            .lock()
            .await
            .journal
            .launch_identity
            .is_some(),
        "the launch process established the launch identity"
    );
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
async fn a_launch_process_report_is_kept_in_memory_and_shown_from_protocol_8() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    establish_launch(&server, &pty).await;
    let before = serde_json::to_value(server.shared.journal.load().expect("journal"))
        .expect("serialize the journal");

    assert!(send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await);

    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        Some(FIRST_SEQUENCE)
    );
    assert_eq!(
        snapshot(&server, PREVIOUS_VERSION)
            .await
            .prompt_accepted_sequence,
        None,
        "a version-seven connection never sees the field"
    );
    let after = serde_json::to_value(server.shared.journal.load().expect("journal"))
        .expect("serialize the journal");
    assert_eq!(after, before, "the report changes no journaled field");
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_duplicate_older_or_expired_report_changes_nothing() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    establish_launch(&server, &pty).await;
    assert!(send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await);

    for sequence in [FIRST_SEQUENCE, FIRST_SEQUENCE - 1] {
        assert!(
            !send_hook(&server, &pty, prompt("claude", sequence)).await,
            "sequence {sequence} is not ahead of the applied one"
        );
    }
    let mut expired = prompt("claude", FIRST_SEQUENCE + 10);
    expired["expires_at"] = serde_json::json!("2000-01-01T00:00:00Z");
    assert!(
        !send_hook(&server, &pty, expired).await,
        "an expired report"
    );
    let mut beyond_ceiling = prompt("claude", FIRST_SEQUENCE + 10);
    beyond_ceiling["expires_at"] = serde_json::json!("2999-01-01T00:00:00Z");
    assert!(
        !send_hook(&server, &pty, beyond_ceiling).await,
        "a lease beyond the shared ceiling"
    );
    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        Some(FIRST_SEQUENCE)
    );

    assert!(send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE + 1)).await);
    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        Some(FIRST_SEQUENCE + 1)
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_far_future_sequence_cannot_outrank_the_reports_that_follow() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    establish_launch(&server, &pty).await;

    assert!(send_hook(&server, &pty, prompt("claude", u64::MAX)).await);
    let stored = snapshot(&server, CURRENT_VERSION)
        .await
        .prompt_accepted_sequence
        .expect("the forged report was applied");
    assert!(
        stored < u64::MAX,
        "the stored sequence is clamped: {stored}"
    );

    assert!(
        send_hook(&server, &pty, prompt("claude", stored + 1)).await,
        "a genuine report ordered after the forged one still applies"
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_report_before_the_launch_identity_or_from_another_process_is_refused() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    assert!(
        !send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await,
        "no launch identity has been accepted yet"
    );
    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        None
    );

    establish_launch(&server, &pty).await;
    // A nested `claude -p` inherits the session's environment and sits in the
    // PTY tree, but it is not the process the launch identity names.
    server
        .shared
        .state
        .lock()
        .await
        .journal
        .launch_identity
        .as_mut()
        .expect("the launch identity")
        .process
        .start_identity = "1".to_owned();
    assert!(!send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await);
    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        None
    );
    stop(&server, &pty).await;
}

#[tokio::test]
async fn a_provider_the_schema_or_the_launch_runtime_does_not_admit_is_refused() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    establish_launch(&server, &pty).await;

    assert!(
        !send_hook(&server, &pty, prompt("codex", FIRST_SEQUENCE)).await,
        "a codex report inside a claude session"
    );
    for launch_runtime in ["shell", "codex"] {
        server.shared.state.lock().await.launch_agent_base = Some(launch_runtime.to_owned());
        assert!(
            !send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await,
            "claude hosted in a {launch_runtime} session"
        );
        assert!(
            !send_hook(&server, &pty, prompt(launch_runtime, FIRST_SEQUENCE)).await,
            "{launch_runtime} reports no prompt acceptance"
        );
    }
    server.shared.state.lock().await.launch_agent_base = Some("claude".to_owned());
    server.shared.state.lock().await.hook_schema =
        pohunek_worker_protocol::hook_schema("identity-v1");
    assert!(
        !send_hook(&server, &pty, prompt("claude", FIRST_SEQUENCE)).await,
        "the identity-only schema has no prompt action"
    );
    assert_eq!(
        snapshot(&server, CURRENT_VERSION)
            .await
            .prompt_accepted_sequence,
        None
    );
    stop(&server, &pty).await;
}

fn scope(lease_id: &LeaseId) -> RuntimeScope {
    RuntimeScope {
        lease_id: lease_id.clone(),
        session_id: SessionId::new("s-112").expect("session id"),
        worker_id: WorkerId::new("worker-claim").expect("worker id"),
        worker_instance_id: WorkerInstanceId::new("runtime-claim").expect("runtime id"),
    }
}

fn fragment(bytes: &[u8]) -> InputFragment {
    InputFragment {
        bytes: SecretBytes::new(bytes.to_vec()),
        delay_after_ms: 0,
    }
}

fn monotonic_nanos() -> u64 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(time.tv_sec).expect("seconds") * 1_000_000_000
        + u64::try_from(time.tv_nsec).expect("nanoseconds")
}

async fn write(
    server: &Server,
    connection: &Connection,
    lease_id: &LeaseId,
    write_id: &str,
) -> pohunek_worker_protocol::WriteAck {
    let plan = InputPlan::new(
        WriteId::new(write_id).expect("write id"),
        vec![fragment(b"typed"), fragment(b"\r")],
    )
    .expect("input plan");
    match write_request(&server.shared, connection, &scope(lease_id), plan)
        .await
        .expect("the write is served")
    {
        ResponseKind::WriteCompleted { acknowledgement } => acknowledgement,
        other => panic!("unexpected answer {other:?}"),
    }
}

#[tokio::test]
async fn the_acknowledgement_carries_the_submit_mark_only_from_protocol_8() {
    let (server, pty, _directory) = launch_claim_fixture().await;
    let (mut connection, lease_id) = controller_connection(&server);

    let started = monotonic_nanos();
    let ack = write(&server, &connection, &lease_id, "input-lease-initialize-1").await;
    let finished = monotonic_nanos();
    let mark = ack.submit_mark_ns.expect("a protocol-8 acknowledgement");
    assert!(
        (started..=finished).contains(&mark),
        "the mark {mark} lies inside the call [{started}, {finished}]"
    );

    let retried = write(&server, &connection, &lease_id, "input-lease-initialize-1").await;
    assert_eq!(
        retried.submit_mark_ns,
        Some(mark),
        "a deduplicated retry reports the mark of the first execution"
    );

    connection.selected_version = Some(PREVIOUS_VERSION);
    let older = write(&server, &connection, &lease_id, "input-lease-initialize-2").await;
    assert_eq!(older.submit_mark_ns, None, "a version-seven connection");
    assert_eq!(older.bytes_written, 6);
    stop(&server, &pty).await;
}
