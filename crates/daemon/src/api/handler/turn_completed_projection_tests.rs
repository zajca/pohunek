//! Daemon-emitted `turn_completed` records through the public request
//! dispatcher.
//!
//! The scenarios drive the real session registry, the notification projector,
//! the debounce coordinator and the durable inbox, as `turn_wait_tests` does.
//! Activity is reported through `session.report_agent`, and the clock is paused
//! so a turn's length is exactly the virtual time the scenario advances.

use std::time::Duration;

use pohunek_test_support::time::AutoAdvanceInhibitor;
use protocol::{method, NotificationKindPolicy};
use serde_json::{json, Value};

use super::turn_wait_tests::{
    debounce, every_provider, fresh_cursor, joined, ok, pause_clock, settle, Fixture, CLOCK_STEP,
    FLUSH_SLACK, LONG_WAIT_MS,
};
use crate::notifications::default_policy;
use crate::session::SessionRegistryConfig;

/// How long before the idle edge the hook reports the stop.
const HOOK_LEAD: Duration = Duration::from_millis(500);

/// A fixture whose policy keeps `turn_completed` records in the inbox.
fn enabled_fixture() -> Fixture {
    Fixture::with_policy(
        SessionRegistryConfig::default(),
        Some(every_provider(NotificationKindPolicy {
            turn_completed: true,
            ..default_policy().enabled
        })),
    )
}

/// Moves the paused clock past one debounce window, letting the coordinator
/// flush what it holds.
async fn flush_window() {
    let mut moved = Duration::ZERO;
    while moved < debounce() + FLUSH_SLACK {
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
    }
}

/// Returns to the real clock, then stops `session`, whose process exit the
/// daemon bounds with real-time grace periods.
async fn finish(fixture: &Fixture, inhibitor: AutoAdvanceInhibitor, session: &str) {
    inhibitor.release().await;
    tokio::time::resume();
    fixture.stop_all(&[session]).await;
}

/// Runs one turn: working, `length` of virtual time, idle.
async fn turn(fixture: &Fixture, session: &str, length: Duration) {
    fixture.activity(session, "working").await;
    settle().await;
    tokio::time::advance(length).await;
    fixture.activity(session, "idle").await;
    settle().await;
}

/// The `turn_completed` records of the inbox, oldest first.
async fn turn_records(fixture: &Fixture) -> Vec<Value> {
    let mut records: Vec<Value> = fixture
        .inbox()
        .await
        .into_iter()
        .filter(|record| record["kind"] == "turn_completed")
        .collect();
    records.sort_by_key(|record| record["created_at"].as_str().map(str::to_owned));
    records
}

/// The turn events of `session` recorded after `cursor`, read one wait at a time.
async fn turn_events_after(fixture: &Fixture, session: &str, cursor: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut cursor = cursor.to_owned();
    loop {
        let wait = fixture.spawn_wait(json!({
            "session_ids": [session], "until": ["turn_completed"],
            "after": cursor, "timeout_ms": 1
        }));
        let result = ok(joined(wait).await);
        if result["reason"] != "event" {
            return events;
        }
        events.push(result["event"].clone());
        cursor = result["cursor"].as_str().expect("cursor").to_owned();
    }
}

#[tokio::test]
async fn a_hookless_turn_gives_one_stamped_record_and_one_turn_event() {
    let fixture = enabled_fixture();
    let session = fixture.session("stamped").await;
    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": {"work.issue": "955"}}),
        )
        .await);
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    // A session that sits idle without working has no turn to report.
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());

    turn(&fixture, &session, Duration::from_secs(3)).await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record["source"]["provider"], "pohunek");
    assert_eq!(record["dedupe_key"], format!("turn:{session}"));
    assert_eq!(record["session_id"], session.as_str());
    assert_eq!(record["turn_duration_ms"], 3_000);
    assert_eq!(record["session_metadata"], json!({"work.issue": "955"}));
    let started = record["turn_started_at"].as_str().expect("turn start");
    time::OffsetDateTime::parse(started, &time::format_description::well_known::Rfc3339)
        .expect("turn start is RFC 3339");

    let events = turn_events_after(&fixture, &session, &cursor).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["notification_id"], record["id"]);

    // The next turn starts: the reported turn is acknowledged, as before.
    fixture.activity(&session, "working").await;
    settle().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records[0]["status"], "acknowledged");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn consecutive_turns_each_keep_their_own_record() {
    let fixture = enabled_fixture();
    let session = fixture.session("consecutive").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;
    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 1_000);
    assert_eq!(records[1]["turn_duration_ms"], 2_000);
    assert_ne!(records[0]["turn_started_at"], records[1]["turn_started_at"]);
    // The second turn's start acknowledged the first record, so nothing is left
    // to supersede; each turn keeps its own record.
    assert_eq!(records[0]["status"], "acknowledged");
    assert_eq!(records[1]["status"], "unread");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_flap_inside_the_window_is_one_turn_from_its_first_working_edge() {
    let fixture = enabled_fixture();
    let session = fixture.session("flap").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(4)).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert!(
        debounce() > Duration::from_secs(1),
        "the flap must end inside the window"
    );
    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 7_000);
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_turn_that_waits_for_approval_is_one_turn_from_its_first_working_edge() {
    let fixture = enabled_fixture();
    let session = fixture.session("approval").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "blocked").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 6_000);
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_report_held_while_the_turn_waits_for_approval_is_one_record_of_the_whole_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("approval-held").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    fixture.activity(&session, "blocked").await;
    flush_window().await;
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());

    // The approval is given and the turn really ends: that end is reported.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    let length = records[0]["turn_duration_ms"].as_u64().expect("a length");
    assert!(
        length >= 13_000,
        "the whole turn, not its first stretch: {records:?}"
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_turn_that_ends_blocked_is_not_reported_as_completed() {
    let fixture = enabled_fixture();
    let session = fixture.session("blocked").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "blocked").await;
    flush_window().await;
    // The owner answers and the agent goes idle without working again.
    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());

    // The next turn is reported, which also proves the earlier ones were flushed.
    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);
    finish(&fixture, inhibitor, &session).await;
}

/// One turn reported by both the hook and the idle edge: one record, one event.
async fn hook_and_idle_edge_report_one_turn(hook_first: bool) {
    let fixture = enabled_fixture();
    let session = fixture.session("hook").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    // A turn longer than the window, so the provider's report is not the stale
    // stop of an earlier turn and outranks the projector's.
    let length = debounce() + Duration::from_secs(1);
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(length).await;
    if hook_first {
        ok(fixture.turn_completed(&session, 1).await);
        tokio::time::advance(HOOK_LEAD).await;
        fixture.activity(&session, "idle").await;
    } else {
        fixture.activity(&session, "idle").await;
        tokio::time::advance(Duration::from_secs(1)).await;
        ok(fixture.turn_completed(&session, 1).await);
    }
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "hook first: {hook_first}: {records:?}");
    assert_eq!(records[0]["source"]["provider"], "claude");
    let duration = records[0]["turn_duration_ms"].as_u64().expect("stamped");
    // The length ends when the session leaves `working`, whichever report came
    // first.
    let ended_at = length
        + if hook_first {
            HOOK_LEAD
        } else {
            Duration::ZERO
        };
    assert_eq!(duration, u64::try_from(ended_at.as_millis()).unwrap());
    assert!(records[0]["turn_started_at"].is_string());
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1,
        "hook first: {hook_first}"
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_stop_after_the_idle_edge_of_one_turn_is_one_record() {
    hook_and_idle_edge_report_one_turn(false).await;
}

#[tokio::test]
async fn a_hook_stop_before_the_idle_edge_of_one_turn_is_one_record() {
    hook_and_idle_edge_report_one_turn(true).await;
}

#[tokio::test]
async fn a_hook_report_of_a_turn_already_reported_adds_nothing() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-hook").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);

    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    // The inbox keeps the one record of the turn, with its own stamp; the late
    // hook adds one more wait event, which is at-least-once for a report that
    // names no turn, and never carries the earlier turn's stamp.
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["source"]["provider"], "pohunek");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    let events = turn_events_after(&fixture, &session, &cursor).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_ne!(events[0]["notification_id"], events[1]["notification_id"]);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_report_after_a_slow_flush_still_repeats_the_reported_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-hook-slow").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);

    // The repeat window of ruling B runs from the moment the record reached
    // the inbox, not from the idle edge: this hook is later than twice the
    // debounce after the turn ended, and inside it after the commit.
    tokio::time::advance(debounce() + Duration::from_secs(2)).await;
    settle().await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_disabled_kind_still_records_the_turn_event_for_waits() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("disabled").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;

    assert!(turn_records(&fixture).await.is_empty());
    let events = turn_events_after(&fixture, &session, &cursor).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].get("notification_id").is_none());
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_full_metadata_snapshot_is_copied_and_an_empty_one_is_omitted() {
    let fixture = enabled_fixture();
    let session = fixture.session("metadata-limit").await;
    let full: serde_json::Map<String, Value> = (0..32)
        .map(|index| (format!("work.key{index:02}"), json!("v")))
        .collect();
    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": full}),
        )
        .await);
    let bare = fixture.session("metadata-none").await;
    fixture.activity(&session, "idle").await;
    fixture.activity(&bare, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    turn(&fixture, &bare, Duration::from_secs(1)).await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    let of = |id: &str| {
        records
            .iter()
            .find(|record| record["session_id"] == id)
            .unwrap_or_else(|| panic!("a record of {id}: {records:?}"))
    };
    assert_eq!(of(&session)["session_metadata"], Value::Object(full));
    assert!(of(&bare).get("session_metadata").is_none());
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn enabling_the_kind_between_two_turns_gives_a_record_only_for_the_second() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("toggle").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1,
        "the disabled kind still signals the wait"
    );

    fixture
        .notifications()
        .set_policy(every_provider(NotificationKindPolicy {
            turn_completed: true,
            ..default_policy().enabled
        }))
        .expect("set policy");
    turn(&fixture, &session, Duration::from_secs(2)).await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn the_snapshot_is_taken_at_the_idle_edge_and_outlives_the_session() {
    let fixture = enabled_fixture();
    let session = fixture.session("snapshot").await;
    let set = |value: &'static str| {
        fixture.call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": {"work.step": value}}),
        )
    };
    ok(set("before").await);
    fixture.activity(&session, "idle").await;
    let _inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    ok(set("after").await);
    flush_window().await;

    let expected = json!({"work.step": "before"});
    let records = turn_records(&fixture).await;
    assert_eq!(records[0]["session_metadata"], expected);
    ok(fixture.call(method::SESSION_REMOVE, json!(session)).await);
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "the record outlives the session");
    assert_eq!(records[0]["session_metadata"], expected);
}

#[tokio::test]
async fn a_client_cannot_stamp_a_record() {
    let fixture = enabled_fixture();
    let session = fixture.session("forged").await;
    let inhibitor = pause_clock().await;

    let created = ok(fixture
        .call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": "forger", "provider_event": "Stop",
                    "host_local_source_id": "forged-1"
                },
                "kind": "turn_completed", "severity": "info",
                "title": "Turn completed", "body": "forged",
                "session_id": session,
                "dedupe_key": format!("turn:{session}"),
                "turn_started_at": "2020-01-01T00:00:00Z",
                "turn_duration_ms": 999,
                "session_metadata": {"forged": "yes"}
            }),
        )
        .await);
    let record = &created["record"];
    for field in ["turn_started_at", "turn_duration_ms", "session_metadata"] {
        assert!(record.get(field).is_none(), "{field}: {record}");
    }
    tokio::time::advance(Duration::from_millis(LONG_WAIT_MS.min(10_000))).await;
    settle().await;
    let records = turn_records(&fixture).await;
    assert!(records.iter().all(|record| {
        record.get("turn_duration_ms").is_none() && record.get("session_metadata").is_none()
    }));
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_released_agent_does_not_complete_the_turn_it_left_open() {
    let fixture = enabled_fixture();
    let session = fixture.session("released").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    // The reporting agent leaves mid-turn, which clears the session's activity;
    // the next agent starts idle and has no turn in flight.
    let released = ok(fixture
        .call(
            method::SESSION_RELEASE_AGENT,
            json!({"session_id": session, "source": "pohunek:claude", "agent": "turn-agent"}),
        )
        .await);
    assert_eq!(released["released"], true, "{released}");
    settle().await;
    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());

    // The next turn is reported, which also proves the earlier edge was flushed.
    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_stop_that_precedes_a_late_idle_edge_is_held_until_the_turn_ends() {
    let fixture = enabled_fixture();
    let session = fixture.session("early-hook").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    // The hook reports the stop, but the session still reads as working when
    // its window ends: the report cannot say which turn it ends, so it waits.
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());
    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": {"work.step": "end"}}),
        )
        .await);

    // The idle edge ends the turn: the held report and the edge's own report
    // are one record and one turn event, with the final length of the turn.
    let reported_at = debounce() + Duration::from_secs(1);
    tokio::time::advance(Duration::from_secs(2)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["session_metadata"], json!({"work.step": "end"}));
    let length = records[0]["turn_duration_ms"].as_u64().expect("length");
    assert!(
        length >= u64::try_from((reported_at + Duration::from_secs(2)).as_millis()).unwrap(),
        "the record carries the final length, not the length at the hook: {length}"
    );
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_late_hook_of_the_previous_turn_does_not_report_the_running_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-previous-hook").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);

    // The stop hook of the first turn arrives while the second one runs, long
    // after the window of the first: it must not report the second turn.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);

    // The second turn's own end is still reported, once.
    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 2);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_report_of_a_turn_that_ends_blocked_is_dropped() {
    let fixture = enabled_fixture();
    let session = fixture.session("held-hook-blocked").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());

    // The turn pauses in `blocked`: the report is not surfaced for a turn that
    // may resume. It ends without working again, so that turn never completed.
    fixture.activity(&session, "blocked").await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_report_of_a_turn_is_dropped_when_its_agent_is_released() {
    let fixture = enabled_fixture();
    let session = fixture.session("held-hook-released").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());

    // The agent leaves mid-turn: its turn never completed.
    let released = ok(fixture
        .call(
            method::SESSION_RELEASE_AGENT,
            json!({"session_id": session, "source": "pohunek:claude", "agent": "turn-agent"}),
        )
        .await);
    assert_eq!(released["released"], true, "{released}");
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_held_hook_report_does_not_spin_without_a_settle_window() {
    let mut policy = every_provider(NotificationKindPolicy {
        turn_completed: true,
        ..default_policy().enabled
    });
    policy.attention_debounce_secs = 0;
    let fixture = Fixture::with_policy(SessionRegistryConfig::default(), Some(policy));
    let session = fixture.session("held-hook-no-window").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    ok(fixture.turn_completed(&session, 1).await);
    settle().await;

    // With no settle window the held report is evaluated again at most once per
    // floor of virtual time, not continuously.
    let seconds = 10;
    let mut moved = Duration::ZERO;
    while moved < Duration::from_secs(seconds) {
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
    }
    let rechecks = fixture.held_rechecks();
    assert!(
        (1..=seconds + 1).contains(&rechecks),
        "{rechecks} rechecks in {seconds} virtual seconds"
    );
    assert!(turn_records(&fixture).await.is_empty());

    fixture.activity(&session, "idle").await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_after_the_deadline_and_before_the_flush_joins_the_closed_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("hook-before-flush").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;
    let mut policy = fixture.notifications().policy();
    policy.attention_debounce_secs = 0;
    fixture
        .notifications()
        .set_policy(policy)
        .expect("set policy");

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    // No window: the turn is closed the moment it ends, and the hook comes
    // before the clock moves, so before the projector's report is flushed.
    fixture.activity(&session, "idle").await;
    ok(fixture.turn_completed(&session, 1).await);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 1_000, "{records:?}");
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_stop_of_a_new_agent_is_not_taken_for_the_turn_the_previous_one_ran() {
    let fixture = enabled_fixture();
    let session = fixture.session("new-agent-stop").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(1)).await;
    flush_window().await;
    assert_eq!(turn_records(&fixture).await.len(), 1);

    // The agent leaves and another one stops before it was seen working: its
    // report ends a turn the daemon did not observe, which is not the reported
    // turn of the agent that left.
    let released = ok(fixture
        .call(
            method::SESSION_RELEASE_AGENT,
            json!({"session_id": session, "source": "pohunek:claude", "agent": "turn-agent"}),
        )
        .await);
    assert_eq!(released["released"], true, "{released}");
    settle().await;
    // The stop comes after the window in which it would repeat the reported turn.
    tokio::time::advance(debounce() * 2).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 2, "{records:#?}");
    assert!(records[1]["turn_duration_ms"].is_null(), "{records:#?}");
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        2
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_one_shot_run_that_releases_right_after_idle_keeps_the_length_of_its_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("one-shot").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    // The agent reports idle and leaves before the projector handled the edge:
    // the report still carries the turn the edge ended.
    fixture.activity(&session, "idle").await;
    let released = ok(fixture
        .call(
            method::SESSION_RELEASE_AGENT,
            json!({"session_id": session, "source": "pohunek:claude", "agent": "turn-agent"}),
        )
        .await);
    assert_eq!(released["released"], true, "{released}");
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_repeated_working_report_does_not_end_the_running_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("repeated-working").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000, "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_after_a_zero_window_flush_still_repeats_the_stamped_record() {
    let fixture = enabled_fixture();
    let session = fixture.session("zero-window-repeat").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;
    let mut policy = fixture.notifications().policy();
    policy.attention_debounce_secs = 0;
    fixture
        .notifications()
        .set_policy(policy)
        .expect("set policy");

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    // The record flushes at once; the hook of the same turn comes a second later.
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    assert_eq!(turn_records(&fixture).await.len(), 1);
    ok(fixture.turn_completed(&session, 1).await);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 1_000, "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_late_stop_of_a_released_agent_repeats_the_stamped_record_instead_of_replacing_it() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-stop-after-release").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    settle().await;
    let released = ok(fixture
        .call(
            method::SESSION_RELEASE_AGENT,
            json!({"session_id": session, "source": "pohunek:claude", "agent": "turn-agent"}),
        )
        .await);
    assert_eq!(released["released"], true, "{released}");
    // The agent's Stop arrives inside the settle window, after its owner left.
    tokio::time::advance(Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 6_000, "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}

/// Reports `activity` as another reporter of the same agent, which replaces the
/// agent the session presents without releasing it.
async fn replacement_activity(fixture: &Fixture, session: &str, activity: &str) {
    let reported = ok(fixture
        .call(
            method::SESSION_REPORT_AGENT,
            json!({
                "session_id": session,
                "source": "pohunek:replacement",
                "agent": "turn-agent",
                "activity": activity
            }),
        )
        .await);
    assert_eq!(reported["recorded"], true, "{reported}");
}

#[tokio::test]
async fn a_replacement_agent_going_idle_does_not_complete_the_turn_of_the_one_it_replaced() {
    let fixture = enabled_fixture();
    let session = fixture.session("replaced-idle").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    replacement_activity(&fixture, &session, "idle").await;
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());
    assert!(turn_events_after(&fixture, &session, &cursor)
        .await
        .is_empty());

    // The replacement's own turn is reported.
    replacement_activity(&fixture, &session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    replacement_activity(&fixture, &session, "idle").await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_policy_change_applies_to_the_next_turn_before_any_report_is_deferred() {
    let fixture = enabled_fixture();
    let session = fixture.session("policy-window").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    let mut policy = fixture.notifications().policy();
    policy.attention_debounce_secs = 1;
    fixture
        .notifications()
        .set_policy(policy)
        .expect("set policy");
    turn(&fixture, &session, Duration::from_secs(1)).await;
    // Well inside the former window, past the new one.
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    assert_eq!(turn_records(&fixture).await.len(), 1);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_turn_stays_unread_when_its_start_and_end_wait_in_the_projector_together() {
    let fixture = enabled_fixture();
    let session = fixture.session("start-and-end-queued").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;
    let mut policy = fixture.notifications().policy();
    policy.attention_debounce_secs = 0;
    fixture
        .notifications()
        .set_policy(policy)
        .expect("set policy");

    // Both transitions are queued before the projector runs.
    fixture.activity(&session, "working").await;
    fixture.activity(&session, "idle").await;
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["status"], "unread", "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_replacement_agent_that_takes_over_a_working_session_reports_its_own_turn() {
    let fixture = enabled_fixture();
    let session = fixture.session("replaced-working").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    replacement_activity(&fixture, &session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    replacement_activity(&fixture, &session, "idle").await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_replacement_agent_cannot_resume_the_settling_turn_of_the_one_it_replaced() {
    let fixture = enabled_fixture();
    let session = fixture.session("replaced-settling").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(2)).await;
    // The replacement works inside the settle window: a new turn, not the
    // continuation of the first, which is reported as it was.
    replacement_activity(&fixture, &session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    replacement_activity(&fixture, &session, "idle").await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    assert_eq!(records[1]["turn_duration_ms"], 1_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_hook_reported_before_the_idle_edge_does_not_surface_before_a_resumed_turn_ends() {
    // Only the provider keeps turn_completed in the inbox; the projector's
    // report is dropped by the policy but still signals the turn.
    let mut policy = every_provider(NotificationKindPolicy {
        turn_completed: false,
        ..default_policy().enabled
    });
    policy.providers.insert(
        "claude".to_owned(),
        NotificationKindPolicy {
            turn_completed: true,
            ..default_policy().enabled
        },
    );
    let fixture = Fixture::with_policy(SessionRegistryConfig::default(), Some(policy));
    let session = fixture.session("hook-then-flap").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    tokio::time::advance(Duration::from_secs(2)).await;
    fixture.activity(&session, "idle").await;
    settle().await;
    // The hook's own window ends while the turn still settles: nothing surfaces.
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    assert!(turn_records(&fixture).await.is_empty());

    // The session works again before the deadline: still the same turn.
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["source"]["provider"], "claude");
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_turn_surfaces_before_its_successor_opens_even_when_a_hook_delayed_it() {
    let fixture = enabled_fixture();
    let session = fixture.session("surface-before-successor").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let inhibitor = pause_clock().await;
    let mut policy = fixture.notifications().policy();
    policy.attention_debounce_secs = 3;
    fixture
        .notifications()
        .set_policy(policy)
        .expect("set policy");

    // Idle at t0 starts the 3 s settle window; the hook at t2 re-arms the
    // report's timer to t5.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    ok(fixture.turn_completed(&session, 1).await);
    settle().await;
    assert!(turn_records(&fixture).await.is_empty());

    // The next turn opens at t4, after the deadline: the finished turn
    // surfaces first and the resolve acknowledges it.
    tokio::time::advance(Duration::from_secs(2)).await;
    fixture.activity(&session, "working").await;
    settle().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_ne!(records[0]["status"], "unread", "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 1_000);
    assert_eq!(
        turn_events_after(&fixture, &session, &cursor).await.len(),
        1
    );

    // The new turn is unaffected and reports itself.
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    for _ in 0..140 {
        tokio::time::advance(Duration::from_millis(50)).await;
        settle().await;
    }
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[1]["status"], "unread", "{records:?}");
    let length = records[1]["turn_duration_ms"].as_u64().expect("a length");
    assert!((1_000..1_100).contains(&length), "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_working_edge_after_the_deadline_closes_the_turn_it_does_not_resume() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-resume").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    turn(&fixture, &session, Duration::from_secs(2)).await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture.activity(&session, "idle").await;
    flush_window().await;
    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["turn_duration_ms"], 2_000);
    assert_eq!(records[1]["turn_duration_ms"], 1_000);
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_held_hook_report_is_dropped_when_the_session_stops() {
    let fixture = enabled_fixture();
    let session = fixture.session("held-hook-stop").await;
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(debounce() + Duration::from_secs(1)).await;
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;
    assert!(turn_records(&fixture).await.is_empty());

    inhibitor.release().await;
    tokio::time::resume();
    fixture.stop_all(&[session.as_str()]).await;
    assert!(turn_records(&fixture).await.is_empty());
}

#[tokio::test]
async fn a_late_hook_report_carries_the_metadata_of_the_turn_end() {
    let fixture = enabled_fixture();
    let session = fixture.session("late-metadata").await;
    let set = |value: &'static str| {
        fixture.call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": {"work.step": value}}),
        )
    };
    ok(set("during").await);
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    let length = debounce() + Duration::from_secs(1);
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(length).await;
    fixture.activity(&session, "idle").await;
    settle().await;
    // The owner starts the next step after the turn ended, then the hook of the
    // finished turn arrives.
    ok(set("next").await);
    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["source"]["provider"], "claude");
    assert_eq!(
        records[0]["session_metadata"],
        json!({"work.step": "during"}),
        "the snapshot is the session's metadata when the turn ended"
    );
    finish(&fixture, inhibitor, &session).await;
}

#[tokio::test]
async fn a_turn_the_daemon_did_not_observe_is_reported_without_a_length() {
    let fixture = enabled_fixture();
    let session = fixture.session("unobserved").await;
    ok(fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": session, "metadata": {"work.step": "adopted"}}),
        )
        .await);
    // The session was idle when the daemon first saw it, as after a restart
    // that adopted a session mid-turn: no transition into `working` was seen.
    fixture.activity(&session, "idle").await;
    let inhibitor = pause_clock().await;

    ok(fixture.turn_completed(&session, 1).await);
    flush_window().await;

    let records = turn_records(&fixture).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].get("turn_started_at").is_none(), "{records:?}");
    assert!(records[0].get("turn_duration_ms").is_none(), "{records:?}");
    assert!(records[0].get("session_metadata").is_none(), "{records:?}");
    finish(&fixture, inhibitor, &session).await;
}
