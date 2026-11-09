//! The notification store of an earlier format opens in format 3, lists
//! attention only, and the retention sweep rewrites it atomically.
//!
//! Each scenario writes a log with the earlier release's records, opens the
//! real [`NotificationService`] over it, and drives the public sweep with an
//! explicit clock.

// Rust guideline compliant 2026-11-09

use std::path::Path;

use pohunek_daemon::notifications::NotificationService;
use pohunek_service_config::{NotificationSettings, DEFAULT_NOTIFICATION_SETTINGS};
use protocol::{NotificationKind, NotificationListParams, NotificationStatus};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const NOW: &str = "2026-11-09T12:00:00Z";

/// Record lifetime of the scenarios, in seconds.
const TTL_SECS: u64 = 3_600;

fn settings() -> NotificationSettings {
    NotificationSettings {
        record_ttl_secs: TTL_SECS,
        ..DEFAULT_NOTIFICATION_SETTINGS
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::parse(NOW, &Rfc3339).expect("fixed clock")
}

/// One `created` action of a record an earlier release wrote.
fn legacy_line(id: &str, kind: &str, status: &str, resolved_at: Option<&str>) -> String {
    let mut record = json!({
        "id": id,
        "source": {
            "provider": "claude",
            "provider_event": "Notification",
            "host_local_source_id": format!("source-{id}")
        },
        "kind": kind,
        "severity": "action_required",
        "status": status,
        "title": "Title",
        "body": "Body",
        "metadata": {},
        "created_at": "2026-11-01T00:00:00Z",
        "session_id": "s-1",
        "dedupe_key": format!("attention:{id}")
    });
    if let Some(resolved_at) = resolved_at {
        let key = if status == "archived" {
            "archived_at"
        } else {
            "acked_at"
        };
        record[key] = Value::from(resolved_at);
    }
    format!("{}\n", json!({"action": "created", "record": record}))
}

fn write_format_2(dir: &Path) -> String {
    let notifications = dir.join("notifications");
    std::fs::create_dir_all(&notifications).expect("create the notification directory");
    std::fs::write(
        notifications.join("format.json"),
        "{\"format_version\":2}\n",
    )
    .expect("write the marker");
    let log = [
        legacy_line("n-open", "agent_blocked", "unread", None),
        legacy_line("n-read", "approval_required", "read", None),
        legacy_line(
            "n-young",
            "approval_required",
            "acknowledged",
            Some("2026-11-09T11:30:00Z"),
        ),
        legacy_line(
            "n-old",
            "approval_required",
            "acknowledged",
            Some("2026-11-09T09:00:00Z"),
        ),
        legacy_line(
            "n-archived",
            "agent_blocked",
            "archived",
            Some("2026-11-09T08:00:00Z"),
        ),
        legacy_line("n-turn", "turn_completed", "unread", None),
        legacy_line("n-error", "error", "unread", None),
        legacy_line("n-deleted", "agent_blocked", "deleted", None),
    ]
    .concat();
    std::fs::write(notifications.join("notifications.jsonl"), &log).expect("write the log");
    log
}

fn listed_ids(service: &NotificationService) -> Vec<String> {
    let mut ids: Vec<String> = service
        .list(NotificationListParams::default())
        .expect("list")
        .notifications
        .into_iter()
        .map(|record| record.id.0)
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_format_2_store_lists_attention_only_and_the_first_sweep_rewrites_it_once() {
    let dir = pohunek_test_support::tempdir_with_prefix("ph-format3-").expect("temp dir");
    let original = write_format_2(dir.path());
    let service = NotificationService::open_with_settings(dir.path(), settings()).expect("opens");

    // Before any sweep: no turn, error or deleted record is listed.
    assert_eq!(
        listed_ids(&service),
        ["n-archived", "n-old", "n-open", "n-read", "n-young"]
    );
    let all = service
        .list(NotificationListParams {
            kind: Some(NotificationKind::TurnCompleted),
            ..NotificationListParams::default()
        })
        .expect("list");
    assert!(all.notifications.is_empty());

    let outcome = service.run_retention_at(now()).expect("sweep");
    // The turn, error and deleted records and the resolved records older than
    // the lifetime go; open and young records stay.
    assert_eq!(outcome.removed, 5, "{outcome:?}");
    assert_eq!(listed_ids(&service), ["n-open", "n-read", "n-young"]);
    let open = service
        .list(NotificationListParams {
            status: Some(NotificationStatus::Unread),
            ..NotificationListParams::default()
        })
        .expect("list");
    assert_eq!(open.notifications.len(), 1);

    let notifications = dir.path().join("notifications");
    assert_eq!(
        std::fs::read_to_string(notifications.join("notifications.jsonl.pre-format-3"))
            .expect("the pre-format copy exists"),
        original
    );

    // The rewritten file replays to the same set, and a second sweep neither
    // changes it nor replaces the copy.
    drop(service);
    let reopened = NotificationService::open_with_settings(dir.path(), settings()).expect("opens");
    assert_eq!(listed_ids(&reopened), ["n-open", "n-read", "n-young"]);
    assert_eq!(reopened.run_retention_at(now()).expect("sweep").removed, 0);
    assert_eq!(
        std::fs::read_to_string(notifications.join("notifications.jsonl.pre-format-3"))
            .expect("the copy is kept"),
        original
    );
    let marker =
        std::fs::read_to_string(notifications.join("format.json")).expect("the marker is kept");
    assert_eq!(
        serde_json::from_str::<Value>(&marker).expect("marker"),
        json!({"format_version": 3})
    );
}

#[test]
fn a_sweep_that_cannot_replace_the_log_leaves_it_whole_and_the_next_sweep_succeeds() {
    let dir = pohunek_test_support::tempdir_with_prefix("ph-format3-").expect("temp dir");
    write_format_2(dir.path());
    let service = NotificationService::open_with_settings(dir.path(), settings()).expect("opens");
    let notifications = dir.path().join("notifications");
    let log = notifications.join("notifications.jsonl");
    let before = std::fs::read(&log).expect("read the log");

    // A directory in the place of the temporary file makes the rewrite fail
    // before the rename.
    let temp = notifications.join("notifications.jsonl.tmp");
    std::fs::create_dir(&temp).expect("block the temporary file");
    service
        .run_retention_at(now())
        .expect_err("the rewrite cannot start");
    assert_eq!(std::fs::read(&log).expect("read the log"), before);
    assert_eq!(listed_ids(&service).len(), 5, "the service keeps working");

    std::fs::remove_dir(&temp).expect("unblock");
    // A partial temporary file left by an interrupted rewrite is replaced.
    std::fs::write(&temp, b"{\"action\":\"created\",\"rec").expect("stale partial file");
    let outcome = service.run_retention_at(now()).expect("the sweep succeeds");
    assert_eq!(outcome.removed, 5);
    assert!(!temp.exists(), "the temporary file was renamed away");
    drop(service);
    let reopened = NotificationService::open_with_settings(dir.path(), settings()).expect("opens");
    assert_eq!(listed_ids(&reopened), ["n-open", "n-read", "n-young"]);
}
