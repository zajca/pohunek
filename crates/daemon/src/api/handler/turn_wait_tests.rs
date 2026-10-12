//! `session.wait_turn` scenarios through the public request dispatcher.
//!
//! Every scenario drives the real session registry with in-process workers
//! and real PTYs, the durable notification inbox, the debounce coordinator and
//! the notification projector through [`handle_request`], the same dispatch a
//! control connection uses. Requests and results are raw JSON, so the wire
//! contract of `docs/public-api.md` is what the assertions check. Where the
//! debounce window or a wait deadline is the behavior under test, sessions are
//! created first and the clock is then paused, so virtual time moves only
//! through `tokio::time::advance`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use pohunek_test_support::wait::{guard, wait_until};
use protocol::{method, NotificationKindPolicy, NotificationPolicy, ProtocolError, Request};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

use super::{handle_request, DaemonState, HealthInfo};
use crate::notifications::{
    default_policy, AttentionCoordinator, AttentionCoordinatorTask, NotificationProjector,
    NotificationService, DEFAULT_ATTENTION_DEBOUNCE_SECS,
};
use crate::session::{SessionRegistry, SessionRegistryConfig};

/// Name of the host profile every scenario session launches.
const AGENT: &str = "turn-agent";

/// A Claude-based profile, so Claude hook reports are admitted for the
/// session, running a shell that prints `tick` whenever a `go` file appears in
/// its working directory. The loop ends by itself after 2,400 polls of 50 ms,
/// the two-minute test hang guard, so a failed stop cannot leave it running.
const AGENT_PROFILE: &str = r#"base = "claude"
program = "/bin/sh"
args = ["-c", "printf ready; i=0; while [ $i -lt 2400 ] && sleep 0.05; do i=$((i+1)); if [ -e go ]; then rm -f go; printf tick; fi; done"]
"#;

/// Debounce window of the default notification policy.
fn debounce() -> Duration {
    Duration::from_secs(u64::from(DEFAULT_ATTENTION_DEBOUNCE_SECS))
}

/// A wait duration no scenario reaches unless it advances the clock to it.
const LONG_WAIT_MS: u64 = 600_000;

/// Virtual-time increment of [`advance_until_finished`].
const CLOCK_STEP: Duration = Duration::from_millis(50);

/// Virtual time past the debounce window within which a flushed turn must
/// complete a parked wait.
const FLUSH_SLACK: Duration = Duration::from_secs(1);

/// Real-time bound of a wait that a correct daemon completes at once; a
/// daemon that misses the completion lets it time out instead of hanging.
const EVICTION_WAIT_MS: u64 = 5_000;

/// Journal capacity of the flood scenario.
const FLOOD_CAPACITY: usize = 4;

/// Turn reports the flood scenario sends, twice the journal capacity.
const FLOOD_REPORTS: u32 = 8;

/// Gap between the steps of the two-quick-turns scenario, well inside one
/// debounce window.
const QUICK_TURN_STEP: Duration = Duration::from_secs(1);

/// Number of scheduler yields that let spawned tasks reach their next await.
const SETTLE_YIELDS: usize = 64;

/// Daemon state, its registry, inbox, and the owning task handles.
struct Fixture {
    state: DaemonState,
    sessions: SessionRegistry,
    root: tempfile::TempDir,
    _projector: NotificationProjector,
    _coordinator: AttentionCoordinatorTask,
}

impl Fixture {
    /// Builds the daemon components with the default notification policy.
    fn new(config: SessionRegistryConfig) -> Self {
        Self::with_policy(config, None)
    }

    /// Builds the daemon components with `policy` replacing the default
    /// notification policy.
    fn with_policy(config: SessionRegistryConfig, policy: Option<NotificationPolicy>) -> Self {
        let root = pohunek_test_support::tempdir_with_prefix("ph-turn-").expect("fixture root");
        let agents = root.path().join("agents");
        std::fs::create_dir_all(&agents).expect("create the agents dir");
        std::fs::write(agents.join(format!("{AGENT}.toml")), AGENT_PROFILE)
            .expect("write the agent profile");
        let notifications = NotificationService::open(&root.path().join("data"))
            .expect("notification service opens");
        if let Some(policy) = policy {
            notifications.set_policy(policy).expect("set the policy");
        }
        let sessions = SessionRegistry::new(SessionRegistryConfig {
            agents_dir: Some(agents),
            ..config
        });
        let (attention, coordinator) =
            AttentionCoordinator::spawn(notifications.clone(), &sessions);
        let projector =
            NotificationProjector::spawn(&sessions, notifications.clone(), attention.clone());
        let state = DaemonState::new(
            HealthInfo::new("test"),
            sessions.clone(),
            Arc::new(crate::governance::HostGovernanceService::open_test()),
            crate::test_support::overlay_registry(),
        )
        .with_notifications(notifications)
        .with_attention_coordinator(attention);
        Self {
            state,
            sessions,
            root,
            _projector: projector,
            _coordinator: coordinator,
        }
    }

    /// Creates a live session through `session.new` and returns its id.
    async fn session(&self, tag: &str) -> String {
        let cwd = self.cwd(tag);
        std::fs::create_dir_all(&cwd).expect("create the session working directory");
        let created = ok(self
            .call(
                method::SESSION_NEW,
                json!({"agent": AGENT, "cwd": cwd, "cols": 80, "rows": 24}),
            )
            .await);
        created["id"].as_str().expect("session id").to_owned()
    }

    fn cwd(&self, tag: &str) -> PathBuf {
        self.root.path().join("work").join(tag)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ProtocolError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = format!("turn-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let request = Request::new(id, method, params).expect("valid request");
        handle_request(&request, &self.state).await.into_result()
    }

    /// Starts `session.wait_turn` on its own task, as a dedicated connection does.
    fn spawn_wait(&self, params: Value) -> JoinHandle<Result<Value, ProtocolError>> {
        let state = self.state.clone();
        tokio::spawn(async move {
            let request =
                Request::new("turn-wait", method::SESSION_WAIT_TURN, params).expect("request");
            handle_request(&request, &state).await.into_result()
        })
    }

    /// Reports a Claude `Stop` hook for `session`, as the managed hook does.
    async fn turn_completed(&self, session: &str, seq: u32) -> Result<Value, ProtocolError> {
        self.turn_report("claude", session, seq).await
    }

    /// Reports a `turn_completed` for `session` from `provider`, shaped like a
    /// provider `Stop` hook report.
    async fn turn_report(
        &self,
        provider: &str,
        session: &str,
        seq: u32,
    ) -> Result<Value, ProtocolError> {
        self.call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": provider,
                    "provider_event": "Stop",
                    "host_local_source_id": format!("{provider}-stop-{session}-{seq}")
                },
                "kind": "turn_completed",
                "severity": "info",
                "title": "Turn completed",
                "body": "Claude completed a turn.",
                "session_id": session,
                "source_id": format!("{provider}:{session}:stop:{seq}"),
                "dedupe_key": format!("turn:{session}")
            }),
        )
        .await
    }

    /// Reports the active agent's activity, as the managed identity hook does.
    ///
    /// A reported activity owns the session's activity from then on, so screen
    /// detection of the test program cannot start a turn on its own.
    async fn activity(&self, session: &str, activity: &str) {
        let reported = ok(self
            .call(
                method::SESSION_REPORT_AGENT,
                json!({
                    "session_id": session,
                    "source": "pohunek:claude",
                    "agent": AGENT,
                    "activity": activity
                }),
            )
            .await);
        assert_eq!(reported["recorded"], true, "{activity} report is recorded");
    }

    async fn inbox(&self) -> Vec<Value> {
        let listed = ok(self.call(method::NOTIFICATION_LIST, json!({})).await);
        listed["notifications"]
            .as_array()
            .expect("notification list")
            .clone()
    }

    async fn stop_all(&self, sessions: &[&str]) {
        for session in sessions {
            let stopped = ok(self.call(method::SESSION_STOP, json!(session)).await);
            assert_eq!(stopped["stopped"], true, "{session} stops");
        }
    }
}

/// The default policy with `enabled` as the per-kind switches of every provider.
fn every_provider(enabled: NotificationKindPolicy) -> NotificationPolicy {
    let mut policy = default_policy();
    policy.enabled = enabled;
    policy.providers.clear();
    policy
}

fn ok(result: Result<Value, ProtocolError>) -> Value {
    result.unwrap_or_else(|error| panic!("expected a success result: {error:?}"))
}

fn err(result: Result<Value, ProtocolError>) -> ProtocolError {
    result.expect_err("expected an error result")
}

async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

/// Pauses the clock so only `tokio::time::advance` moves it, while real PTY
/// and socket I/O keeps running.
///
/// The clock then moves past one debounce window, so a turn resolution
/// recorded while the sessions started (an activity edge before the first
/// hook report) is too old to consume a later turn report.
async fn pause_clock() -> AutoAdvanceInhibitor {
    let inhibitor = AutoAdvanceInhibitor::new();
    tokio::time::pause();
    settle().await;
    tokio::time::advance(debounce() + TIMER_TICK).await;
    settle().await;
    inhibitor
}

/// Waits until `count` turn waits hold slots, so their deadlines are armed
/// before a paused clock moves. A parked wait has acquired its slot and set its
/// deadline in the same poll.
async fn parked(fixture: &Fixture, count: usize) {
    guard("the turn waits to park", async {
        while fixture.sessions.turn_events().active_waiters() < count {
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Moves the paused clock in [`CLOCK_STEP`] increments until `wait` finishes.
///
/// A hook report reaches the debounce coordinator from a blocking task, so the
/// coordinator may arm its timer a few scheduler turns after the report
/// returns; stepping lets it run between increments.
///
/// # Panics
///
/// Panics when `wait` is still running after `limit` of virtual time.
async fn advance_until_finished(wait: &JoinHandle<Result<Value, ProtocolError>>, limit: Duration) {
    let mut moved = Duration::ZERO;
    while !wait.is_finished() {
        assert!(moved <= limit, "the wait did not finish within {limit:?}");
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
    }
}

async fn joined(wait: JoinHandle<Result<Value, ProtocolError>>) -> Result<Value, ProtocolError> {
    guard("the turn wait", wait).await.expect("wait task joins")
}

/// Issues a cursor at the current end of the journal with a 1 ms wait on the
/// running clock.
async fn fresh_cursor(fixture: &Fixture, session: &str) -> String {
    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "timeout_ms": 1
    }));
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "timeout");
    result["cursor"].as_str().expect("cursor").to_owned()
}

fn assert_progress_fields(progress: &Value, session: &str) {
    assert_eq!(progress["session_id"], session);
    assert_eq!(progress["state"], "running");
    assert!(progress
        .get("output_advanced")
        .is_some_and(Value::is_boolean));
    for field in ["last_output_at", "turn_started_at"] {
        let at = progress[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field}: {progress}"));
        time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| panic!("{field} is RFC 3339: {at}"));
    }
}

#[tokio::test]
async fn disabled_hook_turn_wakes_the_wait_without_an_inbox_record_and_chains() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("disabled").await;
    fixture.activity(&session, "idle").await;
    let start = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    // A disabled turn report still answers with the policy refusal, and after
    // the debounce window it completes the wait.
    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"],
        "after": start, "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    let refused = err(fixture.turn_completed(&session, 1).await);
    assert_eq!(refused.code, "notification_kind_disabled");
    settle().await;
    tokio::time::advance(debounce().saturating_sub(Duration::from_millis(1))).await;
    settle().await;
    assert!(
        !wait.is_finished(),
        "the turn waits out the debounce window"
    );
    advance_until_finished(&wait, FLUSH_SLACK).await;
    let first = ok(joined(wait).await);
    assert_eq!(first["reason"], "event");
    assert_eq!(first["event"]["session_id"], session);
    assert_eq!(first["event"]["kind"], "turn_completed");
    assert!(first["event"].get("notification_id").is_none());
    assert_eq!(first["progress"][0]["session_id"], session);
    assert!(
        fixture.inbox().await.is_empty(),
        "no inbox record is created"
    );

    // A turn that ended before a call without a cursor is not returned by it.
    let fresh = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "timeout_ms": 2_000
    }));
    parked(&fixture, 1).await;
    tokio::time::advance(Duration::from_millis(2_000) + TIMER_TICK).await;
    let fresh = ok(joined(fresh).await);
    assert_eq!(
        fresh["reason"], "timeout",
        "an earlier turn is not returned"
    );
    assert!(fresh.get("event").is_none());

    // A turn recorded while no wait runs is returned by the next chained wait.
    let _ = fixture.turn_completed(&session, 2).await;
    settle().await;
    tokio::time::advance(debounce() + TIMER_TICK).await;
    settle().await;
    let chained = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"],
        "after": first["cursor"], "timeout_ms": LONG_WAIT_MS
    }));
    advance_until_finished(&chained, debounce() + FLUSH_SLACK).await;
    let chained = ok(joined(chained).await);
    assert_eq!(chained["reason"], "event");
    assert_eq!(chained["event"]["kind"], "turn_completed");
    assert_ne!(chained["cursor"], first["cursor"]);

    // The chain is consumed: the next wait from its cursor times out.
    let drained = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"],
        "after": chained["cursor"], "timeout_ms": 1_000
    }));
    parked(&fixture, 1).await;
    tokio::time::advance(Duration::from_millis(1_000) + TIMER_TICK).await;
    assert_eq!(ok(joined(drained).await)["reason"], "timeout");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn enabled_turn_names_its_inbox_record() {
    let fixture = Fixture::with_policy(
        SessionRegistryConfig::default(),
        Some(every_provider(NotificationKindPolicy {
            turn_completed: true,
            ..default_policy().enabled
        })),
    );
    let session = fixture.session("enabled").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"],
        "after": cursor, "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    let created = ok(fixture.turn_completed(&session, 1).await);
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event");
    assert_eq!(result["event"]["notification_id"], created["record"]["id"]);
    let inbox = fixture.inbox().await;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["id"], created["record"]["id"]);
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_stop_resolved_by_resumed_work_inside_the_window_never_fires() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("resolved").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"],
        "after": cursor, "timeout_ms": 30_000
    }));
    parked(&fixture, 1).await;
    let _ = fixture.turn_completed(&session, 1).await;
    settle().await;
    tokio::time::advance(debounce() / 2).await;
    // A queued prompt starts the next turn inside the window.
    fixture.activity(&session, "working").await;
    let mut moved = Duration::ZERO;
    while moved < debounce() + FLUSH_SLACK {
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
        assert!(
            !wait.is_finished(),
            "the resolved turn does not complete the wait"
        );
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "timeout");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_blocked_agent_fires_through_the_projector_whatever_the_policy() {
    for blocked_inbox in [true, false] {
        let fixture = Fixture::with_policy(
            SessionRegistryConfig::default(),
            Some(every_provider(NotificationKindPolicy {
                agent_blocked: blocked_inbox,
                ..default_policy().enabled
            })),
        );
        let session = fixture.session("blocked").await;
        fixture.activity(&session, "working").await;
        let inhibitor = pause_clock().await;

        let wait = fixture.spawn_wait(json!({
            "session_ids": [session], "until": ["agent_blocked"], "timeout_ms": LONG_WAIT_MS
        }));
        parked(&fixture, 1).await;
        fixture.activity(&session, "blocked").await;
        advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;
        let result = ok(joined(wait).await);
        assert_eq!(result["reason"], "event", "inbox enabled: {blocked_inbox}");
        assert_eq!(result["event"]["kind"], "agent_blocked");
        assert_eq!(result["progress"][0]["activity"], "blocked");
        let inbox = fixture.inbox().await;
        if blocked_inbox {
            assert_eq!(inbox.len(), 1);
            assert_eq!(result["event"]["notification_id"], inbox[0]["id"]);
        } else {
            assert!(inbox.is_empty(), "a disabled kind creates no inbox record");
            assert!(result["event"].get("notification_id").is_none());
        }
        fixture.stop_all(&[&session]).await;
        inhibitor.release().await;
        tokio::time::resume();
    }
}

#[tokio::test]
async fn a_dropped_provider_report_keeps_the_inbox_record_and_its_deadline() {
    let mut policy = default_policy();
    policy
        .providers
        .get_mut("claude")
        .expect("the default policy has a claude entry")
        .approval_required = false;
    let fixture = Fixture::with_policy(SessionRegistryConfig::default(), Some(policy));
    let session = fixture.session("outranked").await;
    fixture.activity(&session, "working").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["agent_blocked", "approval_required"],
        "after": cursor, "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    // The projector's agent_blocked is kept by the policy and arms the window.
    fixture.activity(&session, "blocked").await;
    settle().await;
    let head_start = Duration::from_secs(2);
    tokio::time::advance(head_start).await;
    // A Claude approval report for the same moment is dropped by the policy.
    let refused = err(fixture
        .call(
            method::NOTIFICATION_CREATE,
            json!({
                "source": {
                    "provider": "claude",
                    "provider_event": "PermissionRequest",
                    "host_local_source_id": format!("claude-permission-{session}")
                },
                "kind": "approval_required",
                "severity": "action_required",
                "title": "Approval required",
                "body": "Claude waits for a tool approval.",
                "session_id": session,
                "source_id": format!("claude:{session}:permission:1"),
                "dedupe_key": format!("attention:{session}")
            }),
        )
        .await);
    assert_eq!(refused.code, "notification_kind_disabled");
    settle().await;
    tokio::time::advance(
        debounce()
            .saturating_sub(head_start)
            .saturating_sub(Duration::from_millis(1)),
    )
    .await;
    settle().await;
    assert!(
        !wait.is_finished(),
        "nothing flushes before the original deadline"
    );
    // The dropped report did not re-arm the window: the flush follows the
    // projector's deadline, far sooner than another full window.
    advance_until_finished(&wait, FLUSH_SLACK).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event");
    assert_eq!(
        result["event"]["kind"], "approval_required",
        "the provider report outranks the projector for the turn event"
    );
    assert!(
        result["event"].get("notification_id").is_none(),
        "the inbox kept a record of another kind"
    );
    let inbox = fixture.inbox().await;
    assert_eq!(
        inbox.len(),
        1,
        "the dropped report does not displace the kept record"
    );
    assert_eq!(inbox[0]["kind"], "agent_blocked");
    assert_eq!(inbox[0]["source"]["provider"], "pohunek");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_multi_session_wait_names_the_session_that_fired() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let first = fixture.session("multi-a").await;
    let second = fixture.session("multi-b").await;
    for session in [&first, &second] {
        fixture.activity(session, "idle").await;
    }
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [first, second],
        "until": ["turn_completed", "agent_blocked"],
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    let _ = fixture.turn_completed(&second, 1).await;
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event");
    assert_eq!(result["event"]["session_id"], second);
    let progress = result["progress"].as_array().expect("progress");
    assert_eq!(progress.len(), 2);
    assert_eq!(
        progress[0]["session_id"], first,
        "progress keeps request order"
    );
    assert_eq!(progress[1]["session_id"], second);
    fixture.stop_all(&[&first, &second]).await;
}

#[tokio::test]
async fn a_timeout_reports_every_progress_field_and_output_since_the_cursor() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let first = fixture.session("progress-a").await;
    let second = fixture.session("progress-b").await;
    for session in [&first, &second] {
        fixture.activity(session, "working").await;
    }
    // Both sessions printed `ready`; wait until the daemon observed it.
    let both =
        json!({"session_ids": [first, second], "until": ["turn_completed"], "timeout_ms": 1});
    wait_until("both sessions' first output", || async {
        let result = ok(joined(fixture.spawn_wait(both.clone())).await);
        result["progress"]
            .as_array()
            .expect("progress")
            .iter()
            .all(|progress| progress.get("last_output_at").is_some())
            .then_some(())
    })
    .await;

    let cursor = ok(joined(fixture.spawn_wait(both.clone())).await)["cursor"].clone();
    std::fs::write(fixture.cwd("progress-b").join("go"), b"").expect("trigger output");
    let since = json!({
        "session_ids": [first, second], "until": ["turn_completed"],
        "after": cursor, "timeout_ms": 1
    });
    let result = wait_until("output after the cursor", || async {
        let result = ok(joined(fixture.spawn_wait(since.clone())).await);
        (result["progress"][1]["output_advanced"] == true).then_some(result)
    })
    .await;
    assert_eq!(result["reason"], "timeout");
    assert!(result.get("event").is_none());
    assert_progress_fields(&result["progress"][0], &first);
    assert_progress_fields(&result["progress"][1], &second);
    assert_eq!(result["progress"][0]["activity"], "working");
    assert_eq!(
        result["progress"][0]["output_advanced"], false,
        "no output after the cursor for the first session"
    );

    // Without a cursor, output counts only from the call's own start.
    let fresh = ok(joined(fixture.spawn_wait(both)).await);
    assert_eq!(fresh["progress"][1]["output_advanced"], false);
    fixture.stop_all(&[&first, &second]).await;
}

#[tokio::test]
async fn the_daemon_default_applies_and_the_ceiling_is_an_error() {
    let default = Duration::from_secs(3);
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_wait_default: default,
        turn_wait_max: Duration::from_secs(60),
        ..SessionRegistryConfig::default()
    });
    let session = fixture.session("limits").await;

    let above = err(fixture
        .call(
            method::SESSION_WAIT_TURN,
            json!({"session_ids": [session], "until": ["exited"], "timeout_ms": 60_001}),
        )
        .await);
    assert_eq!(above.code, "session_wait_limit_exceeded", "never clamped");

    let _inhibitor = pause_clock().await;
    let wait = fixture.spawn_wait(json!({"session_ids": [session], "until": ["exited"]}));
    parked(&fixture, 1).await;
    tokio::time::advance(default.saturating_sub(Duration::from_millis(1))).await;
    settle().await;
    assert!(!wait.is_finished(), "the default has not elapsed yet");
    tokio::time::advance(Duration::from_millis(1) + TIMER_TICK).await;
    assert_eq!(ok(joined(wait).await)["reason"], "timeout");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn the_turn_waiter_pool_is_capped_per_session_and_globally() {
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_global_waiters: 2,
        turn_session_waiters: 1,
        ..SessionRegistryConfig::default()
    });
    let sessions = [
        fixture.session("pool-a").await,
        fixture.session("pool-b").await,
        fixture.session("pool-c").await,
    ];
    let pending = |session: &str| {
        fixture.spawn_wait(json!({
            "session_ids": [session], "until": ["turn_completed"], "timeout_ms": LONG_WAIT_MS
        }))
    };
    let held_a = pending(&sessions[0]);
    parked(&fixture, 1).await;
    let same_session = err(joined(pending(&sessions[0])).await);
    assert_eq!(same_session.code, "session_waiter_limit_reached");
    let held_b = pending(&sessions[1]);
    parked(&fixture, 2).await;
    let global = err(joined(pending(&sessions[2])).await);
    assert_eq!(global.code, "session_waiter_limit_reached");

    // An abandoned wait releases its slots.
    held_a.abort();
    let _ = held_a.await;
    let replacement = pending(&sessions[2]);
    parked(&fixture, 2).await;
    assert!(!replacement.is_finished(), "a released slot is reusable");
    replacement.abort();
    held_b.abort();
    fixture
        .stop_all(&sessions.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
}

#[tokio::test]
async fn exit_fires_then_ended_targets_return_at_once() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("exit").await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["exited"], "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    ok(fixture.call(method::SESSION_STOP, json!(session)).await);
    let exited = ok(joined(wait).await);
    assert_eq!(exited["reason"], "event");
    assert_eq!(exited["event"]["kind"], "exited");
    assert_eq!(exited["event"]["session_id"], session);
    assert_eq!(exited["progress"][0]["state"], "stopped");

    for params in [
        json!({"session_ids": [session], "until": ["exited"], "after": exited["cursor"],
               "timeout_ms": LONG_WAIT_MS}),
        json!({"session_ids": [session], "until": ["turn_completed"], "timeout_ms": LONG_WAIT_MS}),
    ] {
        let ended = ok(joined(fixture.spawn_wait(params)).await);
        assert_eq!(ended["reason"], "targets_ended");
        assert!(ended.get("event").is_none());
    }

    // A metadata change on the stopped session is not another exit.
    ok(fixture
        .call(
            method::SESSION_RENAME,
            json!({"session_id": session, "name": "renamed"}),
        )
        .await);
    let after_rename = ok(joined(fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["exited"], "after": exited["cursor"],
        "timeout_ms": LONG_WAIT_MS
    })))
    .await);
    assert_eq!(after_rename["reason"], "targets_ended");
}

#[tokio::test]
async fn unknown_and_removed_targets_fail_with_session_not_found() {
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_global_waiters: 1,
        turn_session_waiters: 1,
        ..SessionRegistryConfig::default()
    });
    let live = fixture.session("removed-live").await;
    let doomed = fixture.session("removed-doomed").await;
    ok(fixture.call(method::SESSION_STOP, json!(doomed)).await);

    let unknown = err(fixture
        .call(
            method::SESSION_WAIT_TURN,
            json!({"session_ids": [live, "s-unknown"], "until": ["exited"]}),
        )
        .await);
    assert_eq!(unknown.code, "session_not_found");

    let wait = fixture.spawn_wait(json!({
        "session_ids": [live, doomed], "until": ["turn_completed"], "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    assert!(!wait.is_finished(), "one live target keeps the wait open");
    ok(fixture.call(method::SESSION_REMOVE, json!(doomed)).await);
    assert_eq!(err(joined(wait).await).code, "session_not_found");

    // The unknown-target refusal took no slot and the removal released its slot.
    let probe = fixture.spawn_wait(json!({
        "session_ids": [live], "until": ["turn_completed"], "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    assert!(!probe.is_finished(), "the single global slot is free again");
    probe.abort();
    fixture.stop_all(&[&live]).await;
}

#[tokio::test]
async fn foreign_and_unplaceable_cursors_reset_and_bad_params_are_refused() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("cursor").await;

    for cursor in [
        "t1.0.0.d-another-daemon",
        "t1.99999.0.d-x",
        "opaque-garbage",
    ] {
        let reset = ok(fixture
            .call(
                method::SESSION_WAIT_TURN,
                json!({"session_ids": [session], "until": ["turn_completed"], "after": cursor,
                       "timeout_ms": LONG_WAIT_MS}),
            )
            .await);
        assert_eq!(reset["reason"], "cursor_reset", "{cursor}");
        assert_ne!(reset["cursor"], cursor);
        assert_eq!(reset["progress"][0]["session_id"], session);
    }
    let own = fresh_cursor(&fixture, &session).await;
    let placed = ok(joined(fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": own, "timeout_ms": 1
    })))
    .await);
    assert_eq!(
        placed["reason"], "timeout",
        "this daemon's cursor is accepted"
    );

    for params in [
        json!({"session_ids": [], "until": ["exited"]}),
        json!({"session_ids": [session, session], "until": ["exited"]}),
        json!({"session_ids": [session], "until": []}),
        json!({"session_ids": [session], "until": ["exited", "exited"]}),
        json!({"session_ids": [session], "until": ["finished"]}),
        json!({"session_ids": [session], "until": ["exited"], "timeout_ms": 0}),
        json!({"session_ids": [session], "until": ["exited"], "after": ""}),
        json!({"session_ids": [session], "until": ["exited"], "states": ["stopped"]}),
    ] {
        let refused = err(fixture
            .call(method::SESSION_WAIT_TURN, params.clone())
            .await);
        assert_eq!(refused.code, "bad_request", "{params}");
    }
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_cursor_older_than_the_retained_journal_resets() {
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_event_capacity: 2,
        ..SessionRegistryConfig::default()
    });
    let live = fixture.session("evict-live").await;
    let ended = [
        fixture.session("evict-a").await,
        fixture.session("evict-b").await,
        fixture.session("evict-c").await,
    ];
    let before_all = fresh_cursor(&fixture, &live).await;
    fixture.stop_all(&[&ended[0]]).await;
    let after_first = fresh_cursor(&fixture, &live).await;
    // Two more exits leave only the last two events retained, so the event
    // right after `before_all` is evicted while `after_first` still borders
    // the oldest retained event.
    fixture.stop_all(&[&ended[1], &ended[2]]).await;

    let wait = |after: &str| {
        fixture.spawn_wait(json!({
            "session_ids": [live], "until": ["turn_completed"], "after": after, "timeout_ms": 1
        }))
    };
    let evicted = ok(joined(wait(&before_all)).await);
    assert_eq!(evicted["reason"], "cursor_reset");
    assert_eq!(evicted["progress"][0]["session_id"], live);
    let bordering = ok(joined(wait(&after_first)).await);
    assert_eq!(
        bordering["reason"], "timeout",
        "the oldest retained event follows it"
    );
    fixture.stop_all(&[&live]).await;
}

#[tokio::test]
async fn daemon_shutdown_ends_a_turn_wait() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("shutdown").await;
    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    fixture.sessions.begin_daemon_shutdown();
    assert_eq!(err(joined(wait).await).code, "daemon_shutting_down");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn ended_targets_are_reported_only_after_their_exit_is_recorded() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("exit-gap").await;
    let cursor = fresh_cursor(&fixture, &session).await;

    // Park the stop after its terminal state is committed and before the
    // exit is announced, the window in which a wait sees the terminal state.
    let gate = fixture.sessions.hold_next_exit_announcement();
    let stop = {
        let state = fixture.state.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let request = Request::new("exit-gap-stop", method::SESSION_STOP, json!(session))
                .expect("request");
            handle_request(&request, &state).await.into_result()
        })
    };
    guard("the held exit announcement", gate.entered.notified()).await;

    let exited = ok(joined(fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["exited"], "after": cursor,
        "timeout_ms": LONG_WAIT_MS
    })))
    .await);
    assert_eq!(
        exited["reason"], "event",
        "a terminal target already has its exit recorded: {exited}"
    );
    assert_eq!(exited["event"]["kind"], "exited");
    let ended = ok(joined(fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["exited"], "after": exited["cursor"],
        "timeout_ms": LONG_WAIT_MS
    })))
    .await);
    assert_eq!(ended["reason"], "targets_ended");

    gate.release.notify_one();
    let stopped = ok(guard("the stop", stop).await.expect("stop task joins"));
    assert_eq!(stopped["stopped"], true);
}

#[tokio::test]
async fn events_evicted_while_a_wait_is_parked_reset_it() {
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_event_capacity: 2,
        ..SessionRegistryConfig::default()
    });
    let live = fixture.session("parked-evict-live").await;
    let ended = [
        fixture.session("parked-evict-a").await,
        fixture.session("parked-evict-b").await,
        fixture.session("parked-evict-c").await,
    ];
    let cursor = fresh_cursor(&fixture, &live).await;
    let wait = fixture.spawn_wait(json!({
        "session_ids": [live], "until": ["turn_completed"], "after": cursor,
        "timeout_ms": EVICTION_WAIT_MS
    }));
    parked(&fixture, 1).await;
    // Three exits of other sessions overflow a two-event journal, so the event
    // right after the parked wait's position is evicted.
    fixture
        .stop_all(&ended.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    let result = ok(joined(wait).await);
    assert_eq!(
        result["reason"], "cursor_reset",
        "a parked wait never skips evicted events: {result}"
    );
    fixture.stop_all(&[&live]).await;
}

#[tokio::test]
async fn untrusted_turn_reports_neither_wake_a_wait_nor_flood_the_journal() {
    let fixture = Fixture::new(SessionRegistryConfig {
        turn_event_capacity: FLOOD_CAPACITY,
        ..SessionRegistryConfig::default()
    });
    let session = fixture.session("untrusted").await;
    fixture.activity(&session, "working").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": cursor,
        "timeout_ms": 20_000
    }));
    parked(&fixture, 1).await;
    // A producer without a hook schema claims the session's turn ended.
    let forged = err(fixture.turn_report("any-tool", &session, 1).await);
    assert_eq!(forged.code, "notification_kind_disabled");
    // A hook provider reports turns for sessions this daemon does not hold.
    for seq in 0..FLOOD_REPORTS {
        let flood = err(fixture
            .turn_report("claude", &format!("s-unknown-{seq}"), seq)
            .await);
        assert_eq!(flood.code, "notification_kind_disabled");
    }
    settle().await;
    let mut moved = Duration::ZERO;
    while moved < debounce() + FLUSH_SLACK {
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
        assert!(
            !wait.is_finished(),
            "an untrusted report neither wakes the wait nor evicts its position"
        );
    }
    tokio::time::advance(Duration::from_secs(20)).await;
    assert_eq!(ok(joined(wait).await)["reason"], "timeout");
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_short_turn_records_its_stop_once() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("short-turn").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": cursor,
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    // Input starts a turn, and the agent stops two seconds later, well inside
    // the debounce window that follows the working edge.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let _ = fixture.turn_completed(&session, 1).await;
    fixture.activity(&session, "idle").await;
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event", "a short turn is a finished turn");
    assert_eq!(result["event"]["kind"], "turn_completed");

    let again = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": result["cursor"],
        "timeout_ms": 10_000
    }));
    parked(&fixture, 1).await;
    tokio::time::advance(Duration::from_secs(10) + TIMER_TICK).await;
    assert_eq!(
        ok(joined(again).await)["reason"],
        "timeout",
        "recorded once"
    );
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn a_stale_stop_during_the_next_turn_is_dropped() {
    let fixture = Fixture::new(SessionRegistryConfig::default());
    let session = fixture.session("stale-stop").await;
    fixture.activity(&session, "working").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    // The working edge of the running turn lies well before the stale stop.
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": cursor,
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    // The stop of the previous turn arrives late, while the next one runs.
    let _ = fixture.turn_completed(&session, 1).await;
    settle().await;
    let mut moved = Duration::ZERO;
    while moved < debounce() + FLUSH_SLACK {
        tokio::time::advance(CLOCK_STEP).await;
        moved += CLOCK_STEP;
        settle().await;
        assert!(
            !wait.is_finished(),
            "a stale stop while working is not a turn"
        );
    }
    // The running turn ends and reports its own stop.
    let _ = fixture.turn_completed(&session, 2).await;
    fixture.activity(&session, "idle").await;
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;
    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event");
    assert_eq!(result["event"]["kind"], "turn_completed");

    let again = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": result["cursor"],
        "timeout_ms": 10_000
    }));
    parked(&fixture, 1).await;
    tokio::time::advance(Duration::from_secs(10) + TIMER_TICK).await;
    assert_eq!(
        ok(joined(again).await)["reason"],
        "timeout",
        "recorded once"
    );
    fixture.stop_all(&[&session]).await;
}

#[tokio::test]
async fn two_quick_turns_keep_the_inbox_suppression_and_record_the_last_stop() {
    let fixture = Fixture::with_policy(
        SessionRegistryConfig::default(),
        Some(every_provider(NotificationKindPolicy {
            turn_completed: true,
            ..default_policy().enabled
        })),
    );
    let session = fixture.session("two-quick").await;
    fixture.activity(&session, "idle").await;
    let cursor = fresh_cursor(&fixture, &session).await;
    let _inhibitor = pause_clock().await;

    let wait = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": cursor,
        "timeout_ms": LONG_WAIT_MS
    }));
    parked(&fixture, 1).await;
    // Turn one: work starts, and its stop follows one second later, inside the
    // window after the working edge, so the inbox treats it as stale.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(QUICK_TURN_STEP).await;
    let _ = fixture.turn_completed(&session, 1).await;
    fixture.activity(&session, "idle").await;
    settle().await;
    tokio::time::advance(QUICK_TURN_STEP).await;
    // Turn two starts inside turn one's window and stops one second later.
    fixture.activity(&session, "working").await;
    settle().await;
    tokio::time::advance(QUICK_TURN_STEP).await;
    let _ = fixture.turn_completed(&session, 2).await;
    fixture.activity(&session, "idle").await;
    advance_until_finished(&wait, debounce() + FLUSH_SLACK).await;

    let result = ok(joined(wait).await);
    assert_eq!(result["reason"], "event");
    assert_eq!(result["event"]["kind"], "turn_completed");
    assert!(
        fixture.inbox().await.is_empty(),
        "both stops arrive inside a window after a working edge, so the inbox keeps none"
    );
    assert!(result["event"].get("notification_id").is_none());

    let again = fixture.spawn_wait(json!({
        "session_ids": [session], "until": ["turn_completed"], "after": result["cursor"],
        "timeout_ms": 10_000
    }));
    parked(&fixture, 1).await;
    tokio::time::advance(Duration::from_secs(10) + TIMER_TICK).await;
    assert_eq!(
        ok(joined(again).await)["reason"],
        "timeout",
        "recorded once"
    );
    assert!(fixture.inbox().await.is_empty());
    fixture.stop_all(&[&session]).await;
}
