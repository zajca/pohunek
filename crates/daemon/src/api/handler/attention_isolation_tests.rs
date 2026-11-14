//! Attention isolation of a flagged parent's descendants.
//!
//! Every scenario drives the real session registry with in-process workers,
//! the durable notification inbox and the debounce coordinator through
//! [`super::handle_request`], the same dispatch a control connection uses.
//! The pieces shared with the turn-wait harness live there; this module adds
//! only the parent/child setup this feature needs.
//!
//! A parent flag (`pohunek.attention.children=isolate`) stops the attention
//! of its descendants from reaching it or the owner: no inbox record, no
//! roll-up, no `Needs you`, no tray count. Turn signals still flow, so
//! `session.wait_turn` keeps working under isolation.

// Rust guideline compliant 2026-11-13

use std::time::Duration;

use pohunek_test_support::wait::guard;
use protocol::method;
use serde_json::{json, Value};

use super::turn_wait_tests::{
    advance_until_finished, debounce, err, fresh_cursor, joined, ok, parked, pause_clock, settle,
    surfaced_inbox, Fixture, FLUSH_SLACK, LONG_WAIT_MS,
};
use crate::session::SessionRegistryConfig;

/// Reserved metadata key whose single accepted value isolates descendants.
const ATTENTION_KEY: &str = "pohunek.attention.children";
/// The only accepted flag value.
const ISOLATE: &str = "isolate";

/// Creates a live child session of `parent` through `session.new` carrying
/// the parent as the request origin, like [`Fixture::session`] but linked.
async fn child_session(fixture: &Fixture, tag: &str, parent: &str) -> String {
    let cwd = fixture.cwd(tag);
    std::fs::create_dir_all(&cwd).expect("create the session working directory");
    let created = ok(fixture
        .call_with_origin(
            method::SESSION_NEW,
            json!({"agent": super::turn_wait_tests::AGENT, "cwd": cwd, "cols": 80, "rows": 24}),
            parent,
        )
        .await);
    created["id"].as_str().expect("session id").to_owned()
}

/// Creates a live child session of `parent` carrying the attention flag in
/// metadata, for nested-flag chains.
async fn flagged_child(fixture: &Fixture, tag: &str, parent: &str) -> String {
    let cwd = fixture.cwd(tag);
    std::fs::create_dir_all(&cwd).expect("create the session working directory");
    let created = ok(fixture
        .call_with_origin(
            method::SESSION_NEW,
            json!({
                "agent": super::turn_wait_tests::AGENT,
                "cwd": cwd,
                "cols": 80,
                "rows": 24,
                "metadata": {ATTENTION_KEY: ISOLATE}
            }),
            parent,
        )
        .await);
    created["id"].as_str().expect("session id").to_owned()
}

/// Creates a live parent session carrying the attention flag in metadata.
async fn flagged_parent(fixture: &Fixture, tag: &str) -> String {
    let cwd = fixture.cwd(tag);
    std::fs::create_dir_all(&cwd).expect("create the session working directory");
    let created = ok(fixture
        .call(
            method::SESSION_NEW,
            json!({
                "agent": super::turn_wait_tests::AGENT,
                "cwd": cwd,
                "cols": 80,
                "rows": 24,
                "metadata": {ATTENTION_KEY: ISOLATE}
            }),
        )
        .await);
    created["id"].as_str().expect("session id").to_owned()
}

/// Reports a hook-admitted `approval_required` attention record for `session`,
/// the same request shape `turn_wait_tests.rs` uses: provider `claude` with
/// the session's `attention:<id>` dedupe key through the coordinator path.
async fn attention_for(fixture: &Fixture, session: &str, seq: u32) -> Value {
    ok(fixture
        .call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": "claude",
                    "provider_event": "Notification.permission_prompt",
                    "host_local_source_id": format!("claude-approval-{session}-{seq}")
                },
                "kind": "approval_required",
                "severity": "action_required",
                "title": "Approval required",
                "body": "Claude is waiting for approval.",
                "session_id": session,
                "source_id": format!("claude:{session}:approval:{seq}"),
                "dedupe_key": format!("attention:{session}")
            }),
        )
        .await)
}

/// The `session.list` rows, decoded as JSON values.
async fn list_rows(fixture: &Fixture) -> Vec<Value> {
    let listed = ok(fixture.call(method::SESSION_LIST, json!({})).await);
    listed.as_array().expect("session list").clone()
}

/// The `session.list` row for `session`, or `None` when it is not listed.
async fn list_row(fixture: &Fixture, session: &str) -> Option<Value> {
    list_rows(fixture)
        .await
        .into_iter()
        .find(|row| row["id"] == session)
}

fn unread_count(inbox: &[Value]) -> usize {
    inbox
        .iter()
        .filter(|record| record["status"] == "unread")
        .count()
}

/// Waits until the session's latest activity equals `expected`, so a turn
/// report below only opens a turn progress for the status under test.
async fn settle_activity(fixture: &Fixture, session: &str, expected: &str) {
    Box::pin(guard("the activity settles", async {
        loop {
            let activity = fixture.latest_activity(session).await;
            if activity == expected {
                return;
            }
            settle().await;
        }
    }))
    .await;
}

#[tokio::test]
async fn a_blocked_child_of_a_flagged_parent_leaves_no_record() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "flagged-parent").await;
    let child = child_session(&fixture, "isolated-child", &parent).await;
    fixture.activity(&child, "working").await;
    let _inhibitor = pause_clock().await;
    let unread_before = unread_count(&fixture.inbox().await);

    let created = attention_for(&fixture, &child, 1).await;
    assert_eq!(created["created"], true);
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    let inbox = fixture.inbox().await;
    assert!(
        inbox.is_empty(),
        "an isolated child's attention is withheld: {inbox:?}"
    );
    assert_eq!(unread_count(&inbox), unread_before);
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert_eq!(row["attention_isolated_by"], json!(parent));
    let parent_row = list_row(&fixture, &parent)
        .await
        .expect("the parent is listed");
    assert!(
        parent_row.get("attention_isolated_by").is_none()
            || parent_row["attention_isolated_by"].is_null(),
        "the flagged session itself is never isolated: {parent_row:?}"
    );
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn a_blocked_child_of_an_unflagged_parent_still_surfaces() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = fixture.session("plain-parent").await;
    let child = child_session(&fixture, "plain-child", &parent).await;
    fixture.activity(&child, "working").await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    let records = Box::pin(surfaced_inbox(&fixture)).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(fixture.inbox().await.len(), 1);
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert!(
        row.get("attention_isolated_by").is_none() || row["attention_isolated_by"].is_null(),
        "no flag means no isolation: {row:?}"
    );
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn the_nearest_flagged_ancestor_wins_for_nested_parents() {
    // Two flagged ancestors: the walk must stop at the nearest one, so the
    // row names the parent, not the grandparent. A walk that continued past
    // the first flag would report the grandparent instead.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let grand = flagged_parent(&fixture, "grand").await;
    let parent = flagged_child(&fixture, "mid", &grand).await;
    let child = child_session(&fixture, "deep-child", &parent).await;
    fixture.activity(&child, "working").await;
    settle_activity(&fixture, &child, "working").await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    assert!(
        fixture.inbox().await.is_empty(),
        "a doubly nested child is still isolated"
    );
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert_eq!(
        row["attention_isolated_by"],
        json!(parent),
        "the nearest flagged ancestor wins: {row:?}"
    );
    fixture.stop_all(&[&child, &parent, &grand]).await;
}

#[tokio::test]
async fn the_walk_passes_an_unflagged_ancestor_to_the_flagged_grandparent() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let grand = flagged_parent(&fixture, "grand").await;
    let parent = child_session(&fixture, "plain-mid", &grand).await;
    let child = child_session(&fixture, "deep-child", &parent).await;
    fixture.activity(&child, "working").await;
    settle_activity(&fixture, &child, "working").await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    assert!(
        fixture.inbox().await.is_empty(),
        "an unflagged middle session passes isolation through"
    );
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert_eq!(
        row["attention_isolated_by"],
        json!(grand),
        "the walk passes an unflagged ancestor: {row:?}"
    );
    fixture.stop_all(&[&child, &parent, &grand]).await;
}

#[tokio::test]
async fn the_flagged_session_itself_still_surfaces_its_attention() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "flagged-self").await;
    fixture.activity(&parent, "working").await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &parent, 1).await;
    let records = Box::pin(surfaced_inbox(&fixture)).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(fixture.inbox().await.len(), 1);
    fixture.stop_all(&[&parent]).await;
}

#[tokio::test]
async fn a_flag_set_before_attention_withholds_and_clearing_restores() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = fixture.session("late-flag-parent").await;
    let child = child_session(&fixture, "late-flag-child", &parent).await;
    fixture.activity(&child, "working").await;
    let _inhibitor = pause_clock().await;

    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": parent, "metadata": {ATTENTION_KEY: ISOLATE}}),
        )
        .await);
    attention_for(&fixture, &child, 1).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;
    assert!(
        fixture.inbox().await.is_empty(),
        "a flag set before the attention withholds it"
    );

    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": parent, "metadata": {ATTENTION_KEY: Value::Null}}),
        )
        .await);
    attention_for(&fixture, &child, 2).await;
    let records = Box::pin(surfaced_inbox(&fixture)).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;
    assert_eq!(records.len(), 1, "{records:?}");
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn a_flag_set_inside_the_debounce_window_still_withholds() {
    // The coordinator re-checks isolation at flush time, so a flag set after
    // the attention was deferred but before the window elapses still
    // withholds the record. Deleting the flush re-check lets this scenario
    // create a record.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = fixture.session("mid-window-parent").await;
    let child = child_session(&fixture, "mid-window-child", &parent).await;
    fixture.activity(&child, "working").await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    // Let the coordinator dequeue the defer before the flag lands: the defer
    // check then sees no flag, so only the flush re-check can withhold.
    settle().await;
    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": parent, "metadata": {ATTENTION_KEY: ISOLATE}}),
        )
        .await);
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;
    assert!(
        fixture.inbox().await.is_empty(),
        "a flag set inside the debounce window withholds the record"
    );
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert_eq!(row["attention_isolated_by"], json!(parent));
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn the_flag_survives_stop_and_still_isolates() {
    // The real resume path lives in the socket suite
    // (`native_recovery::a_flagged_parent_still_isolates_its_child_after_session_resume`),
    // where the Claude fixture agent can report a verified native
    // conversation. Stopping keeps the session in the registry with its
    // metadata, which is the resume path's input: the flag is copied from
    // the stored record, never rebuilt. This scenario pins that input.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "resume-parent").await;
    let child = child_session(&fixture, "resume-child", &parent).await;
    fixture.activity(&child, "working").await;

    ok(fixture.call(method::SESSION_STOP, json!(parent)).await);
    let inspected = ok(fixture.call(method::SESSION_INSPECT, json!(parent)).await);
    assert_eq!(
        inspected["metadata"][ATTENTION_KEY],
        json!(ISOLATE),
        "the flag survives stop: {inspected:?}"
    );
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;
    assert!(
        fixture.inbox().await.is_empty(),
        "a stopped flag still isolates"
    );
    let row = list_row(&fixture, &child)
        .await
        .expect("the child is listed");
    assert_eq!(row["attention_isolated_by"], json!(parent));
    // The parent is already terminal, which the shared teardown does not
    // accept: stop only the live child here.
    fixture.stop_all(&[&child]).await;
}

#[tokio::test]
async fn an_isolated_turn_signal_still_completes_session_wait_turn() {
    // A turn report is never an inbox record, isolated or not; the recorded
    // turn still ends a `turn_completed` wait for the isolated session.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "wait-parent").await;
    let child = child_session(&fixture, "wait-child", &parent).await;
    // Report idle so the session's latest activity opens a turn progress the
    // report can attach to; without any activity the report has no turn and
    // the flush holds it until the settle table opens one.
    fixture.activity(&child, "idle").await;
    let cursor = fresh_cursor(&fixture, &child).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [child],
        "until": ["turn_completed"],
        "after": cursor,
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    ok(fixture.turn_completed(&child, 1).await);
    advance_until_finished(&wait, Duration::from_secs(120)).await;

    let done = ok(joined(wait).await);
    assert_eq!(done["reason"], "event");
    assert_eq!(done["event"]["kind"], "turn_completed");
    assert!(
        fixture.inbox().await.is_empty(),
        "a turn signal is no inbox record"
    );
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn an_isolated_attention_still_fires_session_wait_turn_without_an_inbox_record() {
    // The withheld record keeps its turn signal with `committed = None`, so
    // a `wait_turn` on the attention kind still fires while the inbox stays
    // empty. Without the signal path the wait would time out.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "signal-parent").await;
    let child = child_session(&fixture, "signal-child", &parent).await;
    fixture.activity(&child, "working").await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [child],
        "until": ["approval_required"],
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    attention_for(&fixture, &child, 1).await;
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;

    let done = ok(joined(wait).await);
    assert_eq!(done["reason"], "event");
    assert_eq!(done["event"]["kind"], "approval_required");
    assert!(
        done["event"].get("notification_id").is_none(),
        "the withheld record names no inbox record: {:?}",
        done["event"]
    );
    assert!(
        fixture.inbox().await.is_empty(),
        "the isolated attention leaves no inbox record"
    );
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn an_immediate_notification_naming_the_child_is_still_created() {
    // An attention record with an explicit non-attention dedupe key skips the
    // debounce queue, so isolation must not suppress it either.
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "immediate-parent").await;
    let child = child_session(&fixture, "immediate-child", &parent).await;
    let _inhibitor = pause_clock().await;

    let created = ok(fixture
        .call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": "script",
                    "provider_event": "note",
                    "host_local_source_id": format!("script-note-{child}")
                },
                "kind": "agent_blocked",
                "severity": "action_required",
                "title": "A note",
                "body": "An explicit message, not attention.",
                "session_id": child,
                "dedupe_key": format!("note:{child}")
            }),
        )
        .await);
    assert_eq!(created["created"], true);
    settle().await;
    assert_eq!(fixture.inbox().await.len(), 1);
    fixture.stop_all(&[&child, &parent]).await;
}

#[tokio::test]
async fn another_reserved_value_is_refused_at_both_mutation_sites() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = fixture.session("refused-parent").await;

    let cwd = fixture.cwd("refused-new");
    std::fs::create_dir_all(&cwd).expect("create the session working directory");
    let refused_new = err(fixture
        .call(
            method::SESSION_NEW,
            json!({
                "agent": super::turn_wait_tests::AGENT,
                "cwd": cwd,
                "cols": 80,
                "rows": 24,
                "metadata": {ATTENTION_KEY: "yes"}
            }),
        )
        .await);
    assert_eq!(refused_new.code, "bad_request");
    assert!(
        refused_new.msg.contains(ATTENTION_KEY) && refused_new.msg.contains(ISOLATE),
        "the refusal names the key and the accepted value: {}",
        refused_new.msg
    );

    let refused_meta = err(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": parent, "metadata": {ATTENTION_KEY: "yes"}}),
        )
        .await);
    assert_eq!(refused_meta.code, "bad_request");
    fixture.stop_all(&[&parent]).await;
}

#[tokio::test]
async fn a_list_filter_on_the_reserved_key_is_accepted() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "filter-parent").await;

    let listed = ok(fixture
        .call(
            method::SESSION_LIST,
            json!({"filters": [{"key": "metadata_eq", "value": {"key": ATTENTION_KEY, "value": ISOLATE}}]}),
        )
        .await);
    assert!(
        listed
            .as_array()
            .expect("session list")
            .iter()
            .any(|row| row["id"] == parent),
        "the flagged parent matches its own filter: {listed:?}"
    );
    // A non-accepted value is not a reserved-metadata write, so the filter
    // path accepts it too (and matches nothing here).
    let other = ok(fixture
        .call(
            method::SESSION_LIST,
            json!({"filters": [{"key": "metadata_eq", "value": {"key": ATTENTION_KEY, "value": "yes"}}]}),
        )
        .await);
    assert!(
        other.as_array().expect("session list").is_empty(),
        "a filter on another value succeeds with an empty list: {other:?}"
    );
    fixture.stop_all(&[&parent]).await;
}

#[tokio::test]
async fn inspect_of_an_isolated_child_carries_no_isolation_field() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let parent = flagged_parent(&fixture, "inspect-parent").await;
    let child = child_session(&fixture, "inspect-child", &parent).await;
    let _inhibitor = pause_clock().await;

    attention_for(&fixture, &child, 1).await;
    tokio::time::advance(debounce() + FLUSH_SLACK).await;
    settle().await;

    let inspected = ok(fixture.call(method::SESSION_INSPECT, json!(child)).await);
    assert!(
        inspected.get("attention_isolated_by").is_none()
            || inspected["attention_isolated_by"].is_null(),
        "inspect never carries the list-only field: {inspected:?}"
    );
    fixture.stop_all(&[&child, &parent]).await;
}
