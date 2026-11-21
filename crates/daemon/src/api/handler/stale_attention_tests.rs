//! A needs-you record does not outlive the need.
//!
//! Every scenario drives the real session registry with in-process workers,
//! the durable notification inbox, the projector and the debounce coordinator
//! through [`super::handle_request`]. The record a desktop notifier shows is
//! an open (`unread`) attention record, so each scenario reads the inbox the
//! way `notification.list` does.

// Rust guideline compliant 2026-11-21

use protocol::method;
use serde_json::{json, Value};

use super::turn_wait_tests::{
    debounce, ok, pause_clock, settle, surfaced_inbox, Fixture, FLUSH_SLACK,
};
use crate::session::SessionRegistryConfig;

/// Reports a hook-admitted `approval_required` attention record for `session`.
async fn needs_you(fixture: &Fixture, session: &str) {
    ok(fixture
        .call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": "claude",
                    "provider_event": "Notification.permission_prompt",
                    "host_local_source_id": format!("claude-approval-{session}")
                },
                "kind": "approval_required",
                "severity": "action_required",
                "title": "Approval required",
                "body": "Claude is waiting for approval.",
                "session_id": session,
                "source_id": format!("claude:{session}:approval"),
                "dedupe_key": format!("attention:{session}")
            }),
        )
        .await);
}

fn open_records(inbox: &[Value]) -> usize {
    inbox
        .iter()
        .filter(|record| record["status"] == "unread")
        .count()
}

#[tokio::test]
async fn stopping_a_blocked_session_acknowledges_its_needs_you_record() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("stale-stop").await;
    fixture.activity(&session, "working").await;
    let _inhibitor = pause_clock().await;

    needs_you(&fixture, &session).await;
    let surfaced = Box::pin(surfaced_inbox(&fixture)).await;
    assert_eq!(open_records(&surfaced), 1, "{surfaced:?}");

    ok(fixture.call(method::SESSION_STOP, json!(session)).await);
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    let inbox = fixture.inbox().await;
    assert_eq!(open_records(&inbox), 0, "{inbox:?}");
    assert_eq!(inbox[0]["status"], "acknowledged");
}

#[tokio::test]
async fn a_restarted_daemon_lists_no_needs_you_record_for_a_session_it_does_not_have() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("stale-restart").await;
    fixture.activity(&session, "working").await;
    let _inhibitor = pause_clock().await;
    needs_you(&fixture, &session).await;
    let surfaced = Box::pin(surfaced_inbox(&fixture)).await;
    assert_eq!(open_records(&surfaced), 1, "{surfaced:?}");

    // The old daemon dies with the record open; the new one has no such session.
    let restarted = fixture.restarted().await;

    let inbox = restarted.inbox().await;
    assert_eq!(open_records(&inbox), 0, "{inbox:?}");
    assert_eq!(inbox.len(), 1, "the record is acknowledged, not lost");
}

#[tokio::test]
async fn removing_a_session_leaves_no_open_needs_you_record() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("stale-remove").await;
    fixture.activity(&session, "working").await;
    let _inhibitor = pause_clock().await;
    ok(fixture.call(method::SESSION_STOP, json!(session)).await);

    // A report that reaches the daemon after the stop is the record a removal
    // must not leave behind.
    needs_you(&fixture, &session).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;
    let before = fixture.inbox().await;
    assert_eq!(open_records(&before), 1, "{before:?}");
    ok(fixture.call(method::SESSION_REMOVE, json!(session)).await);
    settle().await;

    let inbox = fixture.inbox().await;
    assert_eq!(open_records(&inbox), 0, "{inbox:?}");
}
