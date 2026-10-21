//! `pohunek notifications watch` filters against a real daemon.
//!
//! The `pohunek` binary runs as subprocesses subscribed to a `ControlServer`
//! served from this test process with a real durable notification inbox. The
//! records are created through the public `notification.create` method and
//! changed through the CLI, so every event a watcher prints is one the daemon
//! emitted. An unfiltered watcher on the same stream is the control: it shows
//! each event the filtered watchers must drop was emitted and delivered.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::collections::BTreeSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use pohunek_client::Client;
use protocol::{
    NotificationCreateParams, NotificationKind, NotificationSeverity, NotificationSource, SessionId,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Child;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{stderr_text, Harness};

/// Session the filtered watchers follow.
const WATCHED_SESSION: &str = "s-01J00000000000000000000001";

/// A session the filtered watchers must ignore.
const OTHER_SESSION: &str = "s-01J00000000000000000000002";

/// Producer name outside every hook schema, so a create naming a session that
/// does not run is not a hook claim and the default policy governs it.
const PRODUCER: &str = "watch-scenario";

/// Host id the CLI gives the local daemon.
const LOCAL_HOST: &str = "local";

/// One `pohunek notifications watch` subprocess and the lines it printed.
struct Watcher {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Watcher {
    /// Starts `pohunek notifications watch <arguments>` and collects stdout lines.
    fn spawn(harness: &Harness, arguments: &[&str]) -> Self {
        let mut child = harness
            .env
            .tokio_command(pohunek_test_support::bin_exe("pohunek"))
            .args(["notifications", "watch"])
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start the watcher");
        let stdout = child.stdout.take().expect("piped stdout");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                sink.lock().expect("line sink").push(line);
            }
        });
        Self { child, lines }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().expect("line sink").clone()
    }

    /// Waits until a printed line names `id`, which orders every earlier event.
    async fn wait_for(&self, id: &str, what: &str) {
        pohunek_test_support::wait::wait_until(what, || async {
            self.lines()
                .iter()
                .any(|line| line.contains(id))
                .then_some(())
        })
        .await;
    }

    async fn stop(mut self) {
        self.child.kill().await.expect("stop the watcher");
    }
}

/// Creates one record through the public `notification.create` method and
/// returns its id.
async fn create(harness: &Harness, session: &str, kind: NotificationKind, title: &str) -> String {
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let result = client
        .create_notification(NotificationCreateParams {
            source: NotificationSource {
                provider: PRODUCER.to_owned(),
                provider_event: "scenario".to_owned(),
                host_local_source_id: title.to_owned(),
            },
            kind,
            severity: NotificationSeverity::Info,
            title: title.to_owned(),
            body: String::new(),
            metadata: std::collections::BTreeMap::new(),
            session_id: Some(SessionId(session.to_owned())),
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            retryable_admission: false,
        })
        .await
        .expect("create the notification");
    assert!(result.created, "a new record, not a dedupe: {result:?}");
    result.record.id.0
}

fn json_lines(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("JSON line ({error}): {line}"))
        })
        .collect()
}

/// Enables `turn_completed` records for every producer without an override;
/// the default policy disables them.
async fn enable_turn_completed(harness: &Harness) {
    let (code, policy) = harness
        .json(&[
            "notifications",
            "policy",
            "set",
            "--provider",
            "default",
            "--kind",
            "turn_completed",
            "--enabled",
        ])
        .await;
    assert_eq!(code, 0, "{policy}");
}

/// Creates matching readiness records until every watcher has printed a line,
/// so each one is subscribed before the scenario events, and returns their ids.
///
/// The stream has no replay, so a record created before a watcher subscribed
/// never reaches it; the loop is a readiness poll, not a timing assumption.
async fn subscribe_all(harness: &Harness, watchers: &[&Watcher]) -> BTreeSet<String> {
    let readiness = Mutex::new(BTreeSet::new());
    let mut attempt = 0_u32;
    pohunek_test_support::wait::wait_until("every watcher subscribed", || {
        attempt += 1;
        let title = format!("readiness-{attempt}");
        let readiness = &readiness;
        async move {
            let id = create(
                harness,
                WATCHED_SESSION,
                NotificationKind::TurnCompleted,
                &title,
            )
            .await;
            readiness.lock().expect("readiness ids").insert(id);
            watchers
                .iter()
                .all(|watcher| !watcher.lines().is_empty())
                .then_some(())
        }
    })
    .await;
    readiness.into_inner().expect("readiness ids")
}

/// Events the filtered watchers must drop.
struct Dropped {
    /// A `turn_completed` record of [`OTHER_SESSION`].
    other_session: String,
    /// An `error` record of [`WATCHED_SESSION`].
    other_kind: String,
    /// A printed finish, then acknowledged and deleted.
    acknowledged: String,
}

/// Asserts the unfiltered control stream carries every dropped event, in the
/// unfiltered line shapes.
fn assert_control_saw(control: &Watcher, dropped: &Dropped) {
    let events = json_lines(&control.lines());
    for id in [&dropped.other_session, &dropped.other_kind] {
        assert!(
            events.iter().any(|line| {
                line["event"] == "notification_created" && line["record"]["id"] == id.as_str()
            }),
            "control missed the create of {id}"
        );
    }
    assert!(events.iter().any(|line| {
        line["event"] == "notification_updated"
            && line["record"]["id"] == dropped.acknowledged.as_str()
            && line["record"]["status"] == "acknowledged"
    }));
    assert!(events.contains(&serde_json::json!({
        "host_id": LOCAL_HOST,
        "event": "notification_deleted",
        "notification_id": dropped.acknowledged,
    })));
}

/// Asserts the JSON watcher printed only created, unread `turn_completed`
/// records of [`WATCHED_SESSION`] from `allowed`, including `fence`.
fn assert_filtered_json(watcher: &Watcher, allowed: &BTreeSet<String>, fence: &str) {
    let mut printed = BTreeSet::new();
    for line in json_lines(&watcher.lines()) {
        assert_eq!(line["host_id"], LOCAL_HOST, "{line}");
        assert_eq!(line["event"], "notification_created", "{line}");
        assert_eq!(line["record"]["session_id"], WATCHED_SESSION, "{line}");
        assert_eq!(line["record"]["kind"], "turn_completed", "{line}");
        assert_eq!(line["record"]["status"], "unread", "{line}");
        printed.insert(line["record"]["id"].as_str().expect("id").to_owned());
    }
    assert!(printed.contains(fence), "{printed:?}");
    assert!(printed.is_subset(allowed), "{printed:?}");
}

/// Asserts the human watcher printed the same selection in the human line
/// shape `<host> <event> <status> <severity> <id> <title>`.
fn assert_filtered_human(watcher: &Watcher, allowed: &BTreeSet<String>, fence: &str) {
    let lines = watcher.lines();
    for line in &lines {
        let fields: Vec<&str> = line.splitn(6, ' ').collect();
        assert_eq!(fields.len(), 6, "{line}");
        assert_eq!(
            fields[..4],
            [LOCAL_HOST, "notification_created", "unread", "info"],
            "{line}"
        );
        assert!(allowed.contains(fields[4]), "{line}");
    }
    assert!(lines.iter().any(|line| line.contains(fence)), "{lines:?}");
}

/// A session-scoped finish filter prints only new unread `turn_completed`
/// records of that session: not another session's, not another kind, not the
/// update that acknowledges an earlier finish, and not a delete.
#[tokio::test]
async fn filtered_watch_prints_only_new_unread_turns_of_one_session() {
    let harness = Harness::start_with_notifications().await;
    enable_turn_completed(&harness).await;

    let filtered_json = Watcher::spawn(
        &harness,
        &[
            "--session",
            WATCHED_SESSION,
            "--kind",
            "turn_completed",
            "--status",
            "unread",
            "--json",
        ],
    );
    let filtered_human = Watcher::spawn(
        &harness,
        &[
            "--session",
            WATCHED_SESSION,
            "--kind",
            "turn_completed",
            "--unread",
        ],
    );
    let control = Watcher::spawn(&harness, &["--json"]);
    let watchers = [&filtered_json, &filtered_human, &control];
    let readiness = subscribe_all(&harness, &watchers).await;

    // A finish the filtered JSON watcher already printed; acknowledging it must
    // not read as a new finish.
    let acknowledged = json_lines(&filtered_json.lines())[0]["record"]["id"]
        .as_str()
        .expect("printed record id")
        .to_owned();
    let dropped = Dropped {
        other_session: create(
            &harness,
            OTHER_SESSION,
            NotificationKind::TurnCompleted,
            "other-session",
        )
        .await,
        other_kind: create(
            &harness,
            WATCHED_SESSION,
            NotificationKind::Error,
            "other-kind",
        )
        .await,
        acknowledged,
    };
    for action in ["ack", "delete"] {
        let output = harness
            .run(&["notifications", action, &dropped.acknowledged])
            .await;
        assert!(output.status.success(), "{}", stderr_text(&output));
    }
    // The stream is ordered: once a watcher prints the fence, it has handled
    // every earlier event.
    let fence = create(
        &harness,
        WATCHED_SESSION,
        NotificationKind::TurnCompleted,
        "fence",
    )
    .await;
    for watcher in watchers {
        watcher.wait_for(&fence, "the fence record").await;
    }

    assert_control_saw(&control, &dropped);
    let allowed: BTreeSet<_> = readiness.into_iter().chain([fence.clone()]).collect();
    assert_filtered_json(&filtered_json, &allowed, &fence);
    assert_filtered_human(&filtered_human, &allowed, &fence);

    for watcher in [filtered_json, filtered_human, control] {
        watcher.stop().await;
    }
    harness.stop().await;
}
