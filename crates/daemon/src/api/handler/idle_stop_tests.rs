//! Idle stop of opted-in sessions through the public request dispatcher.
//!
//! Every scenario drives the real session registry with in-process workers and
//! real PTYs, the durable notification inbox and the idle-stop task through
//! [`handle_request`], the dispatch a control connection uses. The sessions are
//! created first and the clock is then paused, so a deadline is exactly the
//! virtual time a scenario advances and no scenario sleeps.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use pohunek_test_support::wait::guard;
use protocol::{
    event, method, NotificationCreateParams, NotificationKind, NotificationKindPolicy,
    NotificationSeverity, NotificationSource, NotificationStatus, NotificationUpdateParams,
    ProtocolError, SessionId, IDLE_STOP_METADATA_KEY,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::turn_wait_tests::{err, every_provider, ok, pause_clock, settle, Fixture, AGENT};
use crate::agent::host::fixture::official_host;
use crate::notifications::default_policy;
use crate::session::{spawn_idle_stop, SessionRegistryConfig};
use crate::shutdown::{DaemonShutdown, DriverHandle};

/// The shortest idle time the scenarios' daemon accepts.
const MIN_SECS: u32 = 5;

/// Idle limit the scenarios opt their sessions in with.
const LIMIT_SECS: u64 = 20;

/// How long a reported agent claim without a backing process lives.
///
/// The scenarios' agent is a plain shell, so its claim has no observed process
/// and would expire (and clear the activity) after the default TTL of the long
/// virtual waits below; a real agent's claim is backed by its process.
const CLAIM_TTL: Duration = Duration::from_hours(24);

/// Scheduler rounds a scenario allows for the record of a stop that was just
/// reported; far above what the record needs, so only a missing record exhausts it.
const RECORD_POLLS: usize = 2_000;

/// Scheduler rounds without a new pass of the task that show it has gone quiet.
const QUIET_YIELDS: usize = 256;

/// Longest bounded `session.wait` the daemon accepts, in milliseconds.
const OBSERVATION_WAIT_MS: u32 = protocol::MAX_SESSION_WAIT_MS;

/// Delay before the task looks again at a session whose stop was held back.
const RECHECK_SECS: u64 = 3;

fn config(config_dir: Option<PathBuf>) -> SessionRegistryConfig {
    SessionRegistryConfig {
        idle_stop_min_secs: MIN_SECS,
        idle_stop_recheck: Duration::from_secs(RECHECK_SECS),
        active_agent_claim_ttl: CLAIM_TTL,
        config_dir,
        ..SessionRegistryConfig::default()
    }
}

/// The notification policy that keeps `system` records in the inbox.
fn system_enabled() -> protocol::NotificationPolicy {
    every_provider(NotificationKindPolicy {
        system: true,
        ..default_policy().enabled
    })
}

/// A daemon with live sessions, a paused clock and a running idle-stop task.
struct World {
    fixture: Fixture,
    task: Option<DriverHandle>,
    events: broadcast::Receiver<protocol::Event>,
    inhibitor: Option<AutoAdvanceInhibitor>,
    clock_paused: bool,
    sessions: Vec<String>,
}

impl World {
    async fn new(count: usize) -> Self {
        Self::configured(config(None), Some(system_enabled()), count).await
    }

    async fn configured(
        config: SessionRegistryConfig,
        policy: Option<protocol::NotificationPolicy>,
        count: usize,
    ) -> Self {
        let fixture = Fixture::with_policy(config, policy);
        let mut sessions = Vec::new();
        for index in 0..count {
            sessions.push(fixture.session(&format!("idle-{index}")).await);
        }
        Self::adopt(fixture, sessions).await
    }

    /// Pauses the clock and starts the idle-stop task over `sessions`.
    async fn adopt(fixture: Fixture, sessions: Vec<String>) -> Self {
        let events = fixture.sessions.subscribe();
        let inhibitor = pause_clock().await;
        let tasks = DaemonShutdown::default();
        let task = spawn_idle_stop(
            &tasks,
            fixture.sessions.clone(),
            fixture.notifications().clone(),
        );
        settle().await;
        Self {
            fixture,
            task: Some(task),
            events,
            inhibitor: Some(inhibitor),
            clock_paused: true,
            sessions,
        }
    }

    fn id(&self, index: usize) -> &str {
        &self.sessions[index]
    }

    async fn opt_in(&self, index: usize, value: &str) -> Result<Value, ProtocolError> {
        self.fixture
            .call(
                method::SESSION_SET_METADATA,
                json!({"session_id": self.id(index), "metadata": {IDLE_STOP_METADATA_KEY: value}}),
            )
            .await
    }

    async fn activity(&self, index: usize, activity: &str) {
        self.fixture.activity(self.id(index), activity).await;
        settle().await;
    }

    /// Moves the virtual clock by `secs` and waits until the task has acted on
    /// it, so a session that is still running was kept on purpose.
    async fn elapse(&self, secs: u64) {
        self.advance(Duration::from_secs(secs)).await;
        self.quiesce().await;
    }

    /// Moves the clock to just past `secs` from now and waits for the task.
    async fn elapse_past(&self, secs: u64) {
        self.advance(Duration::from_secs(secs) + TIMER_TICK).await;
        self.quiesce().await;
    }

    async fn advance(&self, by: Duration) {
        tokio::time::advance(by).await;
        settle().await;
    }

    /// Waits until the task is inside a pass it cannot finish, parked behind a
    /// lock the scenario holds.
    async fn task_is_parked(&self) {
        guard("the idle-stop task to park", async {
            while !self.fixture.sessions.idle_stop_pass_under_way_for_test() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    /// Waits until the task has acted on the current time and is parked again.
    async fn quiesce(&self) {
        guard(
            "the idle-stop task to go quiet",
            self.fixture
                .sessions
                .idle_stop_quiesce_for_test(QUIET_YIELDS),
        )
        .await;
    }

    async fn state(&self, index: usize) -> String {
        let info = ok(self
            .fixture
            .call(method::SESSION_INSPECT, json!(self.id(index)))
            .await);
        info["state"].as_str().expect("session state").to_owned()
    }

    async fn assert_running(&self, index: usize, when: &str) {
        assert_eq!(self.state(index).await, "running", "{when}");
    }

    /// Waits for the stop of session `index` to be reported.
    async fn stopped(&mut self, index: usize) {
        self.stopped_all(&[index]).await;
    }

    /// Waits for the stops of every listed session; they may be reported in
    /// any order.
    async fn stopped_all(&mut self, indexes: &[usize]) {
        let mut pending: Vec<String> = indexes
            .iter()
            .map(|index| self.id(*index).to_owned())
            .collect();
        let events = &mut self.events;
        guard("the idle stops to be reported", async {
            while !pending.is_empty() {
                let received = events.recv().await.expect("event stream");
                if received.event() == event::SESSION_STOPPED {
                    let stopped = received.payload()["session"]["id"].clone();
                    pending.retain(|wanted| stopped != wanted.as_str());
                }
            }
        })
        .await;
        for index in indexes {
            assert_eq!(self.state(*index).await, "stopped");
        }
    }

    /// The `system` records of the inbox that name `index`.
    async fn system_records(&self, index: usize) -> Vec<Value> {
        self.fixture
            .inbox()
            .await
            .into_iter()
            .filter(|record| record["kind"] == "system" && record["session_id"] == self.id(index))
            .collect()
    }

    /// The `system` records of `index` once the task has recorded the stop it
    /// reported; the record follows the stop event by a few scheduler turns.
    async fn recorded_stop(&self, index: usize) -> Vec<Value> {
        for _ in 0..RECORD_POLLS {
            let records = self.system_records(index).await;
            if !records.is_empty() {
                return records;
            }
            settle().await;
        }
        Vec::new()
    }

    /// Creates an unread attention record for `index`, as a hook report would.
    fn attention(&self, index: usize, kind: NotificationKind) -> protocol::NotificationId {
        let record = self
            .fixture
            .notifications()
            .create(NotificationCreateParams {
                source: NotificationSource {
                    provider: "pohunek".to_owned(),
                    provider_event: "test".to_owned(),
                    host_local_source_id: format!("attention-{index}-{}", kind.as_str()),
                },
                kind,
                severity: NotificationSeverity::ActionRequired,
                title: "Needs the owner".to_owned(),
                body: "The agent waits.".to_owned(),
                metadata: std::collections::BTreeMap::default(),
                session_id: Some(SessionId(self.id(index).to_owned())),
                agent_kind: None,
                source_id: None,
                dedupe_key: None,
                project_id: None,
                retryable_admission: false,
            })
            .expect("create the attention record");
        record.record.id
    }

    fn acknowledge(&self, id: protocol::NotificationId) {
        self.fixture
            .notifications()
            .update(NotificationUpdateParams {
                id,
                status: NotificationStatus::Acknowledged,
            })
            .expect("acknowledge the record");
    }

    /// Stops the idle-stop task and returns to the real clock, keeping the
    /// sessions' working directories in place.
    async fn release_clock(&mut self) {
        if let Some(task) = self.task.take() {
            task.stop().await;
        }
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.release().await;
        }
        if self.clock_paused {
            tokio::time::resume();
            self.clock_paused = false;
        }
    }

    /// Returns to the real clock and stops the sessions still running.
    async fn finish(mut self, running: &[usize]) {
        self.release_clock().await;
        let ids: Vec<String> = running
            .iter()
            .map(|index| self.id(*index).to_owned())
            .collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.fixture.stop_all(&ids).await;
    }
}

#[tokio::test]
async fn an_opted_in_idle_session_stops_at_its_limit_and_records_why() {
    let config_dir = pohunek_test_support::tempdir().expect("config dir");
    let marker = config_dir.path().join("stop.marker");
    let hooks = config_dir.path().join("hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    std::fs::write(
        hooks.join("session-stop"),
        format!(
            "#!/bin/sh\nprintf '%s:%s\\n' \"$POHUNEK_SESSION_ID\" \"$POHUNEK_STOP_REASON\" >> {}\n",
            marker.display()
        ),
    )
    .expect("write the session-stop hook");
    let mut world = World::configured(
        config(Some(config_dir.path().to_path_buf())),
        Some(system_enabled()),
        1,
    )
    .await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;

    world.elapse(LIMIT_SECS - 1).await;
    world.assert_running(0, "one second before the limit").await;
    assert!(world.system_records(0).await.is_empty());

    world.elapse_past(1).await;
    world.stopped(0).await;
    let records = world.recorded_stop(0).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["metadata"]["reason"], "idle_stop");
    assert_eq!(
        records[0]["metadata"]["summary"],
        format!("idle_secs={LIMIT_SECS}")
    );

    // The hook runs in the session's working directory, which the fixture
    // removes when it is dropped, so the marker is awaited before `finish`.
    let id = world.id(0).to_owned();
    world.release_clock().await;
    let expected = format!("{id}:stopped");
    let hook = pohunek_test_support::wait::wait_until("the session-stop hook marker", || async {
        std::fs::read_to_string(&marker)
            .ok()
            .filter(|contents| contents.contains(&expected))
    })
    .await;
    assert_eq!(hook.lines().count(), 1, "the hook fires once: {hook:?}");
    world.finish(&[]).await;
}

#[tokio::test]
async fn sessions_without_a_usable_opt_in_are_never_stopped() {
    let world = World::new(2).await;
    // No key at all, and `default` while the host has no default.
    ok(world.opt_in(1, "default").await);
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;

    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "a session without the key").await;
    world
        .assert_running(1, "`default` without a host default")
        .await;
    assert!(world.system_records(0).await.is_empty());
    world.finish(&[0, 1]).await;
}

#[tokio::test]
async fn a_blocked_working_or_unreported_session_is_never_stopped() {
    let world = World::new(3).await;
    for index in 0..3 {
        ok(world.opt_in(index, &LIMIT_SECS.to_string()).await);
    }
    world.activity(0, "blocked").await;
    world.activity(1, "working").await;
    // Session 2 never reports an activity.

    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "blocked").await;
    world.assert_running(1, "working").await;
    world.assert_running(2, "no activity").await;

    // The owner has seen what the block recorded, so only the activity holds
    // the session; the clock of a blocked session starts when it goes idle.
    let unread: Vec<Value> = world
        .fixture
        .inbox()
        .await
        .into_iter()
        .filter(|record| record["kind"] == "agent_blocked" && record["session_id"] == world.id(0))
        .collect();
    assert!(!unread.is_empty(), "the block recorded an attention record");
    for record in unread {
        ok(world
            .fixture
            .call(
                method::NOTIFICATION_UPDATE,
                json!({"id": record["id"], "status": "acknowledged"}),
            )
            .await);
    }
    world.elapse(10 * LIMIT_SECS).await;
    world
        .assert_running(0, "blocked with its record acknowledged")
        .await;
    world.activity(0, "idle").await;
    world.elapse(LIMIT_SECS - 1).await;
    world
        .assert_running(0, "one second before the limit of the idle entry")
        .await;
    let mut world = world;
    world.elapse_past(1).await;
    world.stopped(0).await;
    world.finish(&[1, 2]).await;
}

#[tokio::test]
async fn an_activity_change_restarts_the_clock_and_a_restatement_does_not() {
    let mut world = World::new(2).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    ok(world.opt_in(1, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;

    world.elapse(LIMIT_SECS - 1).await;
    // Session 0 works for a moment and idles again; session 1 restates idle.
    world.activity(0, "working").await;
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;

    world.elapse_past(1).await;
    world.stopped(1).await;
    world
        .assert_running(0, "the clock restarted at the second idle")
        .await;

    world.elapse(LIMIT_SECS - 2).await;
    world
        .assert_running(0, "one second before the restarted limit")
        .await;
    world.elapse_past(1).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn the_host_default_applies_to_default_sessions_and_a_number_overrides_it() {
    let mut world = World::new(3).await;
    let set = ok(world
        .fixture
        .call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": LIMIT_SECS}),
        )
        .await);
    assert_eq!(set["default_after_secs"], LIMIT_SECS);
    assert_eq!(set["min_after_secs"], MIN_SECS);
    ok(world.opt_in(0, "default").await);
    ok(world.opt_in(1, &(2 * LIMIT_SECS).to_string()).await);
    ok(world.opt_in(2, &(LIMIT_SECS / 2).to_string()).await);
    for index in 0..3 {
        world.activity(index, "idle").await;
    }

    world.elapse_past(LIMIT_SECS / 2).await;
    world.stopped(2).await;
    world.assert_running(0, "before the default").await;
    world.elapse_past(LIMIT_SECS / 2).await;
    world.stopped(0).await;
    world.assert_running(1, "its own longer limit").await;
    world.elapse_past(LIMIT_SECS).await;
    world.stopped(1).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_changed_default_moves_the_deadline_of_an_armed_default_session() {
    let mut world = World::new(2).await;
    let set = |secs: u64| {
        world.fixture.call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": secs}),
        )
    };
    ok(set(LIMIT_SECS).await);
    ok(world.opt_in(0, "default").await);
    ok(world.opt_in(1, "default").await);
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;

    world.elapse(LIMIT_SECS / 2).await;
    ok(set(3 * LIMIT_SECS).await);
    world.elapse_past(LIMIT_SECS).await;
    world.assert_running(0, "the raised default").await;
    world.elapse(LIMIT_SECS).await;
    // Clearing the default disarms every `default` session.
    ok(set(0).await);
    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "a cleared default").await;
    world.assert_running(1, "a cleared default").await;
    ok(set(LIMIT_SECS).await);
    world.elapse_past(0).await;
    world.stopped_all(&[0, 1]).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_session_adopted_after_a_restart_keeps_its_opt_in_and_is_stopped_a_limit_after_it_reports_idle(
) {
    let dir = pohunek_test_support::tempdir().expect("data dir");
    let first = Fixture::with_policy(
        SessionRegistryConfig {
            store_path: Some(dir.path().join("metadata.jsonl")),
            ..config(None)
        },
        Some(system_enabled()),
    );
    let cwd = first.cwd("adopted");
    std::fs::create_dir_all(&cwd).expect("create the session working directory");
    let created = ok(first
        .call(
            method::SESSION_NEW,
            json!({
                "agent": AGENT, "cwd": cwd, "cols": 80, "rows": 24,
                "metadata": {IDLE_STOP_METADATA_KEY: LIMIT_SECS.to_string()}
            }),
        )
        .await);
    let id = created["id"].as_str().expect("session id").to_owned();
    first.activity(&id, "idle").await;

    let registry = first.sessions.restarted_for_test(official_host()).await;
    let restarted = first.restarted(registry, Some(system_enabled()));
    let mut world = World::adopt(restarted, vec![id]).await;
    // The activity is memory only: the adopted session is not armed until the
    // agent or the detector reports it again.
    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "adopted without an activity").await;
    world.activity(0, "idle").await;
    world.elapse(LIMIT_SECS - 1).await;
    world
        .assert_running(0, "one second before the limit from the idle report")
        .await;
    world.elapse_past(1).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_waiter_that_arrives_while_the_stop_waits_for_its_turn_keeps_the_session() {
    for observation in [false, true] {
        let mut world = World::new(1).await;
        ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
        world.activity(0, "idle").await;
        let id = SessionId(world.id(0).to_owned());
        let lifecycle = world.fixture.sessions.hold_lifecycle_for_test(&id).await;

        // The task passes its first look at the session and parks behind the
        // lifecycle lock; the waiter arrives after that.
        world
            .advance(Duration::from_secs(LIMIT_SECS) + TIMER_TICK)
            .await;
        world.task_is_parked().await;
        let wait = if observation {
            world.fixture.spawn_call(
                method::SESSION_WAIT,
                json!({"session_id": world.id(0), "states": ["stopped", "done", "failed"], "timeout_ms": OBSERVATION_WAIT_MS}),
            )
        } else {
            world.fixture.spawn_wait(json!({
                "session_ids": [world.id(0)], "until": ["agent_blocked"], "timeout_ms": 600_000
            }))
        };
        settle().await;
        drop(lifecycle);
        world.quiesce().await;

        world
            .assert_running(0, "a waiter arrived before the stop transaction")
            .await;
        wait.abort();
        let _ = wait.await;
        world.elapse_past(RECHECK_SECS).await;
        world.stopped(0).await;
        world.finish(&[]).await;
    }
}

#[tokio::test]
async fn attention_that_arrives_while_the_stop_waits_for_its_turn_keeps_the_session() {
    let mut world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    let id = SessionId(world.id(0).to_owned());
    let lifecycle = world.fixture.sessions.hold_lifecycle_for_test(&id).await;

    world
        .advance(Duration::from_secs(LIMIT_SECS) + TIMER_TICK)
        .await;
    world.task_is_parked().await;
    let record = world.attention(0, NotificationKind::ApprovalRequired);
    drop(lifecycle);
    world.quiesce().await;
    world
        .assert_running(0, "an approval request arrived before the stop transaction")
        .await;

    world.acknowledge(record);
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn an_observation_waiter_holds_the_stop_until_the_wait_ends() {
    let mut world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.elapse(LIMIT_SECS - 1).await;
    let wait = world.fixture.spawn_call(
        method::SESSION_WAIT,
        json!({"session_id": world.id(0), "states": ["stopped", "done", "failed"], "timeout_ms": OBSERVATION_WAIT_MS}),
    );
    settle().await;

    // The limit is reached while the wait still runs.
    world.elapse_past(1).await;
    world
        .assert_running(0, "an observation wait in flight")
        .await;
    wait.abort();
    let _ = wait.await;
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn an_invalid_value_is_refused_by_session_new_and_set_metadata() {
    let world = World::new(1).await;
    let too_low = (MIN_SECS - 1).to_string();
    for value in [
        too_low.as_str(),
        "0",
        "",
        " 60",
        "+60",
        "-5",
        "1e3",
        "60s",
        "0x3c",
        "4294967296",
        "DEFAULT",
    ] {
        let refused = err(world.opt_in(0, value).await);
        assert_eq!(refused.code, "invalid_idle_stop_after", "{value:?}");
    }
    let refused = err(world
        .fixture
        .call(
            method::SESSION_NEW,
            json!({
                "agent": AGENT,
                "cwd": world.fixture.cwd("refused"),
                "cols": 80, "rows": 24,
                "metadata": {IDLE_STOP_METADATA_KEY: "soon"}
            }),
        )
        .await);
    assert_eq!(refused.code, "invalid_idle_stop_after");
    let listed = ok(world.fixture.call(method::SESSION_LIST, json!({})).await);
    assert_eq!(
        listed.as_array().expect("session list").len(),
        1,
        "the refused create left no session behind"
    );
    // A refused edit leaves the previous value; other keys are not interpreted.
    ok(world.opt_in(0, "60").await);
    world.opt_in(0, "nope").await.unwrap_err();
    let info = ok(world
        .fixture
        .call(method::SESSION_INSPECT, json!(world.id(0)))
        .await);
    assert_eq!(info["metadata"][IDLE_STOP_METADATA_KEY], "60");
    ok(world
        .fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": world.id(0), "metadata": {
                "pohunek.other": "x", "team.idle_stop_after_secs": "soon"
            }}),
        )
        .await);
    world.finish(&[0]).await;
}

#[tokio::test]
async fn a_stored_value_an_older_daemon_accepted_disarms_the_session_and_blocks_no_edit() {
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let world = World::new(1).await;
    world
        .fixture
        .sessions
        .store_unvalidated_for_test(&SessionId(world.id(0).to_owned()), "soon")
        .await;
    world.activity(0, "idle").await;

    world.elapse(10 * LIMIT_SECS).await;
    world
        .assert_running(0, "a stored value that opts in nothing")
        .await;
    // An edit of another key wakes the task for a second pass that sees the
    // same invalid value.
    ok(world
        .fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": world.id(0), "metadata": {"work.issue": "956"}}),
        )
        .await);
    world.quiesce().await;
    assert_eq!(
        logs.text().matches("session.idle_stop.ignored").count()
            + logs
                .text()
                .matches("ignoring a stored idle-stop value")
                .count(),
        1,
        "the ignored value is reported once, not on every pass"
    );
    // Replacing the value with a valid one arms the session.
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    let mut world = world;
    world.elapse_past(0).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn removing_the_key_disarms_and_a_new_value_measures_from_the_idle_start() {
    let mut world = World::new(2).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;
    world.elapse(LIMIT_SECS / 2).await;
    ok(world
        .fixture
        .call(
            method::SESSION_SET_METADATA,
            json!({"session_id": world.id(0), "metadata": {IDLE_STOP_METADATA_KEY: null}}),
        )
        .await);
    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "the removed key").await;

    // Opting in late counts the idle time already spent.
    ok(world.opt_in(1, &LIMIT_SECS.to_string()).await);
    world.elapse_past(0).await;
    world.stopped(1).await;
    world.finish(&[0]).await;
}

#[tokio::test]
async fn unread_attention_holds_the_stop_until_it_is_acknowledged() {
    let mut world = World::new(2).await;
    for index in 0..2 {
        ok(world.opt_in(index, &LIMIT_SECS.to_string()).await);
    }
    let blocked = world.attention(0, NotificationKind::AgentBlocked);
    let approval = world.attention(1, NotificationKind::ApprovalRequired);
    world.activity(0, "idle").await;
    world.activity(1, "idle").await;

    world.elapse(10 * LIMIT_SECS).await;
    world.assert_running(0, "an unread agent_blocked").await;
    world.assert_running(1, "an unread approval_required").await;

    world.acknowledge(blocked);
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(0).await;
    // The record carries the measured idle time, not the configured limit.
    let records = world.recorded_stop(0).await;
    assert_eq!(
        records[0]["metadata"]["summary"],
        format!("idle_secs={}", 10 * LIMIT_SECS + RECHECK_SECS),
        "{records:?}"
    );
    world
        .assert_running(1, "approval_required is still unread")
        .await;
    world.acknowledge(approval);
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(1).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_waiting_caller_holds_the_stop_until_the_wait_ends() {
    let mut world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    let wait = world.fixture.spawn_wait(json!({
        "session_ids": [world.id(0)], "until": ["agent_blocked"], "timeout_ms": 600_000
    }));
    settle().await;

    world.elapse(3 * LIMIT_SECS).await;
    world.assert_running(0, "a turn wait in flight").await;
    wait.abort();
    let _ = wait.await;
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(0).await;
    world.finish(&[]).await;
}

#[tokio::test]
async fn activity_that_arrives_while_the_stop_waits_for_its_turn_keeps_the_session() {
    let world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    let id = SessionId(world.id(0).to_owned());
    let lifecycle = world.fixture.sessions.hold_lifecycle_for_test(&id).await;

    // The task finds the session due and parks behind the lifecycle lock.
    world
        .advance(Duration::from_secs(LIMIT_SECS) + TIMER_TICK)
        .await;
    world.activity(0, "working").await;
    drop(lifecycle);
    world.elapse(RECHECK_SECS).await;

    world
        .assert_running(0, "work arrived before the stop transaction")
        .await;
    assert!(world.system_records(0).await.is_empty());
    world.finish(&[0]).await;
}

#[tokio::test]
async fn an_operator_stop_before_the_limit_leaves_no_second_stop_or_record() {
    let world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.elapse(LIMIT_SECS / 2).await;
    ok(world
        .fixture
        .call(method::SESSION_STOP, json!(world.id(0)))
        .await);

    world.elapse(10 * LIMIT_SECS).await;
    assert_eq!(world.state(0).await, "stopped");
    assert!(world.system_records(0).await.is_empty());
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_failed_stop_records_nothing_and_is_tried_again() {
    let mut world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.fixture.sessions.fail_idle_stops_for_test(u32::MAX);

    world.elapse_past(LIMIT_SECS).await;
    world.elapse(3 * RECHECK_SECS).await;
    world.assert_running(0, "a stop that failed").await;
    assert!(world.system_records(0).await.is_empty());

    world.fixture.sessions.fail_idle_stops_for_test(0);
    world.elapse_past(RECHECK_SECS).await;
    world.stopped(0).await;
    let records = world.recorded_stop(0).await;
    assert_eq!(records.len(), 1, "{records:?}");
    world.finish(&[]).await;
}

#[tokio::test]
async fn a_policy_that_drops_system_records_still_lets_the_stop_happen() {
    let mut world = World::configured(config(None), None, 1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;

    world.elapse_past(LIMIT_SECS).await;
    world.stopped(0).await;
    for _ in 0..RECORD_POLLS / 20 {
        settle().await;
    }
    assert!(
        world.system_records(0).await.is_empty(),
        "the default policy keeps no system record"
    );
    world.finish(&[]).await;
}

#[tokio::test]
async fn the_default_round_trips_persists_and_is_validated() {
    let dir = pohunek_test_support::tempdir().expect("data dir");
    let path = dir.path().join("session-idle-stop.json");
    let fixture = Fixture::new(SessionRegistryConfig {
        idle_stop_policy_path: Some(path.clone()),
        ..config(None)
    });
    let fresh = ok(fixture
        .call(method::SESSION_IDLE_STOP_GET, Value::Null)
        .await);
    assert_eq!(fresh, json!({"min_after_secs": MIN_SECS}));

    let refused = err(fixture
        .call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": MIN_SECS - 1}),
        )
        .await);
    assert_eq!(refused.code, "invalid_session_policy");
    assert!(!path.exists(), "a refused default writes nothing");

    let set = ok(fixture
        .call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": 90}),
        )
        .await);
    assert_eq!(
        set,
        json!({"default_after_secs": 90, "min_after_secs": MIN_SECS})
    );
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("record is JSON");
    assert_eq!(on_disk, json!({"default_after_secs": 90}));
    assert_eq!(
        std::fs::metadata(&path)
            .expect("record metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // The retention policy document is a different record.
    let retention = ok(fixture.call(method::SESSION_POLICY_GET, Value::Null).await);
    assert_eq!(
        retention["retention"].as_object().map(serde_json::Map::len),
        Some(5)
    );

    // A restart reads the record back.
    let restarted = crate::session::SessionRegistry::new(SessionRegistryConfig {
        idle_stop_policy_path: Some(path.clone()),
        ..config(None)
    });
    let loaded = restarted
        .load_idle_stop_policy()
        .await
        .expect("load the record");
    assert_eq!(loaded.default_after_secs, Some(90));

    // `0` and an absent value clear it.
    let cleared = ok(fixture
        .call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": 0}),
        )
        .await);
    assert_eq!(cleared, json!({"min_after_secs": MIN_SECS}));
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("record")).expect("record is JSON");
    assert_eq!(on_disk, json!({}));
    ok(fixture
        .call(
            method::SESSION_IDLE_STOP_SET,
            json!({"default_after_secs": 90}),
        )
        .await);
    ok(fixture.call(method::SESSION_IDLE_STOP_SET, json!({})).await);
    assert_eq!(
        crate::session::SessionRegistry::new(SessionRegistryConfig {
            idle_stop_policy_path: Some(path.clone()),
            ..config(None)
        })
        .load_idle_stop_policy()
        .await
        .expect("load the cleared record")
        .default_after_secs,
        None
    );
}

#[tokio::test]
async fn a_corrupt_or_unsafe_record_fails_the_load() {
    let dir = pohunek_test_support::tempdir().expect("data dir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private data dir");
    let path = dir.path().join("session-idle-stop.json");
    let registry = || {
        crate::session::SessionRegistry::new(SessionRegistryConfig {
            idle_stop_policy_path: Some(path.clone()),
            ..config(None)
        })
    };
    std::fs::write(&path, b"{ nope").expect("write a corrupt record");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(
        registry()
            .load_idle_stop_policy()
            .await
            .expect_err("corrupt")
            .code,
        "session_policy_store_error"
    );

    std::fs::write(&path, br#"{"default_after_secs": 1}"#)
        .expect("write a value below the minimum");
    assert_eq!(
        registry()
            .load_idle_stop_policy()
            .await
            .expect_err("below minimum")
            .code,
        "invalid_session_policy"
    );

    std::fs::write(&path, br#"{"default_after_secs": 90, "extra": true}"#).expect("write");
    assert!(
        registry().load_idle_stop_policy().await.is_err(),
        "unknown field"
    );

    std::fs::write(&path, br#"{"default_after_secs": 90}"#).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(
        registry().load_idle_stop_policy().await.is_err(),
        "group-readable record"
    );
}

/// Runs `git` in `dir`, panicking when it fails.
fn git(dir: &std::path::Path, args: &[&str]) {
    let output = pohunek_test_support::process_env::command("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
}

#[tokio::test]
async fn the_stop_keeps_the_sessions_worktree() {
    let data = pohunek_test_support::tempdir().expect("data dir");
    let repo = data.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["-c", "init.defaultBranch=main", "init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "init\n").expect("write README");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let fixture = Fixture::with_policy(
        SessionRegistryConfig {
            store_path: Some(data.path().join("metadata.jsonl")),
            worktree_root: Some(data.path().join("worktrees")),
            ..config(None)
        },
        Some(system_enabled()),
    );
    let created = ok(fixture
        .call(
            method::SESSION_NEW,
            json!({
                "agent": AGENT, "repo": repo, "branch": "feat/idle",
                "cols": 80, "rows": 24,
                "metadata": {IDLE_STOP_METADATA_KEY: LIMIT_SECS.to_string()}
            }),
        )
        .await);
    let id = created["id"].as_str().expect("session id").to_owned();
    let worktree = PathBuf::from(created["worktree_path"].as_str().expect("worktree path"));
    assert!(worktree.is_dir(), "the session owns a worktree");
    let mut world = World::adopt(fixture, vec![id]).await;
    world.activity(0, "idle").await;

    world.elapse_past(LIMIT_SECS).await;
    world.stopped(0).await;
    assert!(worktree.is_dir(), "the stop keeps the worktree");
    let records = world.recorded_stop(0).await;
    assert_eq!(records.len(), 1, "{records:?}");
    world.finish(&[]).await;
}

#[tokio::test]
async fn no_idle_stop_starts_once_the_daemon_shuts_down() {
    let world = World::new(1).await;
    ok(world.opt_in(0, &LIMIT_SECS.to_string()).await);
    world.activity(0, "idle").await;
    world.fixture.sessions.begin_daemon_shutdown();

    world.elapse(10 * LIMIT_SECS).await;
    world
        .assert_running(0, "a deadline that fires during shutdown")
        .await;
    for _ in 0..RECORD_POLLS / 20 {
        settle().await;
    }
    assert!(world.system_records(0).await.is_empty());
    world.finish(&[0]).await;
}
