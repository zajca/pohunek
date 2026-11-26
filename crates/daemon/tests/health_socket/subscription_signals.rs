//! The `subscribe` stream's `subscription_lagged` and `heartbeat` signals
//! through the public control socket.
//!
//! The heartbeat scenarios run on tokio's paused clock with auto-advance off,
//! so virtual time moves only through the explicit `advance` calls. The lag
//! scenarios stall one subscriber while a draining subscriber keeps pace, so the
//! stalled one overflows its broadcast channel and the draining one never does.

// Rust guideline compliant 2026-11-26

use super::*;
use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use serde_json::json;

/// The smallest `heartbeat_ms` the default service config accepts.
const HEARTBEAT_MIN_MS: u64 = 1_000;

/// The largest `heartbeat_ms` the default service config accepts.
const HEARTBEAT_MAX_MS: u64 = 60_000;

/// Interval the heartbeat scenarios ask for.
const INTERVAL: Duration = Duration::from_millis(HEARTBEAT_MIN_MS);

/// Events a flooding scenario emits at most. Far above what the daemon's send
/// buffer, the socket queue and the broadcast channel hold together, so the
/// stalled subscriber has lost events long before the flood ends.
const FLOOD_EVENTS: usize = 6_000;

/// How many leading digits the flood sequence number has in a name or title.
const SEQUENCE_DIGITS: usize = 5;

/// A running daemon with one idle session.
struct Fixture {
    socket: TestSocket,
    session: SessionInfo,
    control: Framed<UnixStream, LinesCodec>,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(tag: &str) -> Self {
        let socket = temp_socket(tag);
        let config = SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            ..SessionRegistryConfig::default()
        };
        let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let mut control = connect(&socket).await;
        let session = create_session(&mut control, &socket).await;
        Self {
            socket,
            session,
            control,
            shutdown,
            handle,
        }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }

    /// Renames the session to `name`, which publishes one `session_updated`.
    async fn rename(&mut self, name: &str) {
        let request = Request::make(
            "session-rename",
            method::SESSION_RENAME,
            serde_json::to_value(protocol::SessionRenameParams {
                session_id: self.session.id.clone(),
                name: Some(name.to_owned()),
            })
            .expect("serialize rename params"),
        );
        ok_payload(exchange(&mut self.control, &request).await);
    }

    /// Creates a notification titled `title`, which publishes one
    /// `notification_created`.
    async fn notify(&mut self, title: &str) {
        let mut params = notification_params(None);
        params.title = title.to_owned();
        params.source.host_local_source_id = title.to_owned();
        params.dedupe_key = None;
        params.source_id = Some(title.to_owned());
        create_notification(&mut self.control, params).await;
    }
}

/// Subscribes a new connection with `params` and returns the stream with the
/// daemon's answer.
async fn subscribe_with(
    socket: &Path,
    params: Value,
) -> (Framed<UnixStream, LinesCodec>, Response) {
    let mut subscriber = connect(socket).await;
    let request = Request::make("subscribe-signals", method::SUBSCRIBE, params);
    let answer = exchange(&mut subscriber, &request).await;
    (subscriber, answer)
}

async fn subscribe_heartbeat(socket: &Path, heartbeat_ms: u64) -> Framed<UnixStream, LinesCodec> {
    let (subscriber, answer) =
        subscribe_with(socket, json!({ "heartbeat_ms": heartbeat_ms })).await;
    assert!(answer.is_ok(), "heartbeat_ms {heartbeat_ms} is accepted");
    subscriber
}

async fn next_event(subscriber: &mut Framed<UnixStream, LinesCodec>) -> Event {
    wait::guard("the next streamed event", async {
        let line = subscriber
            .next()
            .await
            .expect("a streamed event line")
            .expect("event framing ok");
        serde_json::from_str(&line).expect("parse event")
    })
    .await
}

/// Reads events until a `session_updated` names the session `name`, asserting
/// that none of the events before it is a heartbeat.
async fn read_rename_without_heartbeat(
    subscriber: &mut Framed<UnixStream, LinesCodec>,
    name: &str,
) {
    loop {
        let event = next_event(subscriber).await;
        assert_ne!(
            event.event(),
            event::HEARTBEAT,
            "no heartbeat while events keep arriving within the interval"
        );
        if event.event() == event::SESSION_UPDATED
            && event.payload()["session"]["name"] == Value::from(name)
        {
            return;
        }
    }
}

#[tokio::test]
async fn heartbeat_follows_the_interval_only_for_the_subscriber_that_asked() {
    // The daemon and its session start on the real clock; virtual time begins
    // once they are up.
    let mut fixture = Fixture::start("hb-interval").await;
    tokio::time::pause();
    let inhibitor = AutoAdvanceInhibitor::new();
    let mut opted_in = subscribe_heartbeat(&fixture.socket, HEARTBEAT_MIN_MS).await;
    let (mut plain, answer) = subscribe_with(&fixture.socket, Value::Null).await;
    assert!(answer.is_ok(), "null params still subscribe");

    for _ in 0..3 {
        tokio::time::advance(INTERVAL + TIMER_TICK).await;
        let event = next_event(&mut opted_in).await;
        assert_eq!(event.event(), event::HEARTBEAT);
        assert_eq!(*event.payload(), json!({}), "a heartbeat carries no fields");
    }

    // Everything the plain subscriber was sent is ahead of this rename; none of
    // it is a heartbeat.
    fixture.rename("after-quiet").await;
    read_rename_without_heartbeat(&mut plain, "after-quiet").await;

    inhibitor.release().await;
    fixture.stop().await;
}

#[tokio::test]
async fn each_forwarded_event_restarts_the_quiet_interval() {
    let mut fixture = Fixture::start("hb-restart").await;
    tokio::time::pause();
    let inhibitor = AutoAdvanceInhibitor::new();
    let mut opted_in = subscribe_heartbeat(&fixture.socket, HEARTBEAT_MIN_MS).await;

    // Three events, each half an interval after the previous one: the stream is
    // never quiet for a whole interval, though 1.5 intervals pass in total.
    for index in 0..3 {
        tokio::time::advance(INTERVAL / 2).await;
        let name = format!("steady-{index}");
        fixture.rename(&name).await;
        read_rename_without_heartbeat(&mut opted_in, &name).await;
    }

    // The quiet stream then gets its heartbeat.
    tokio::time::advance(INTERVAL + TIMER_TICK).await;
    loop {
        if next_event(&mut opted_in).await.event() == event::HEARTBEAT {
            break;
        }
    }

    inhibitor.release().await;
    fixture.stop().await;
}

#[tokio::test]
async fn heartbeat_outside_the_bounds_is_refused_and_does_not_subscribe() {
    let fixture = Fixture::start("hb-bounds").await;

    for refused in [0, HEARTBEAT_MIN_MS - 1, HEARTBEAT_MAX_MS + 1] {
        let (mut connection, answer) =
            subscribe_with(&fixture.socket, json!({ "heartbeat_ms": refused })).await;
        let error = err_payload(answer);
        assert_eq!(error.code, "heartbeat_out_of_range", "{refused}");

        // A connection that did not subscribe still answers requests.
        let health = Request::make("health-after-refusal", method::DAEMON_HEALTH, Value::Null);
        let reply = exchange(&mut connection, &health).await;
        assert!(reply.is_ok(), "{refused}: the connection serves requests");
    }

    for accepted in [HEARTBEAT_MIN_MS, HEARTBEAT_MAX_MS] {
        let (_subscriber, answer) =
            subscribe_with(&fixture.socket, json!({ "heartbeat_ms": accepted })).await;
        assert!(answer.is_ok(), "{accepted} is within the bounds");
    }

    fixture.stop().await;
}

#[tokio::test]
async fn host_inspect_advertises_the_heartbeat_bounds() {
    let fixture = Fixture::start("hb-capability").await;
    let mut connection = connect(&fixture.socket).await;

    for params in [Value::Null, json!({ "skip_runtime_probes": true })] {
        let request = Request::make("inspect-capability", method::HOST_INSPECT, params);
        let capabilities = ok_payload(exchange(&mut connection, &request).await);
        assert_eq!(
            capabilities["subscribe_heartbeat"],
            json!({ "min_ms": HEARTBEAT_MIN_MS, "max_ms": HEARTBEAT_MAX_MS })
        );
    }

    fixture.stop().await;
}

/// The `n`-th flood name, which sorts and parses by its sequence number.
fn flood_name(sequence: usize) -> String {
    format!("flood-{sequence:0SEQUENCE_DIGITS$}")
}

fn flood_sequence(name: &str) -> Option<usize> {
    name.strip_prefix("flood-")?.parse().ok()
}

/// What a flooding scenario publishes and where its events carry the name.
#[derive(Clone, Copy)]
enum Channel {
    Session,
    Notification,
}

impl Channel {
    fn wire_name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Notification => "notification",
        }
    }

    fn event_name(self) -> &'static str {
        match self {
            Self::Session => event::SESSION_UPDATED,
            Self::Notification => event::NOTIFICATION_CREATED,
        }
    }

    /// The flood name an event of this channel carries, if it is one.
    fn name_of(self, event: &Event) -> Option<String> {
        if event.event() != self.event_name() {
            return None;
        }
        let name = match self {
            Self::Session => &event.payload()["session"]["name"],
            Self::Notification => &event.payload()["record"]["title"],
        };
        name.as_str().map(str::to_owned)
    }

    async fn emit(self, fixture: &mut Fixture, name: &str) {
        match self {
            Self::Session => fixture.rename(name).await,
            Self::Notification => fixture.notify(name).await,
        }
    }
}

/// Floods `channel` while `stalled` reads nothing and `draining` keeps pace,
/// then resumes `stalled`.
///
/// Asserts that the draining subscriber saw every event and no lag, and that
/// the stalled subscriber's stream shows an in-order prefix, then a
/// `subscription_lagged` for `channel` that accounts for exactly the events it
/// lost, then the later events.
async fn assert_lag_is_reported(tag: &str, channel: Channel) {
    let mut fixture = Fixture::start(tag).await;
    let (mut stalled, ack) = subscribe_with(&fixture.socket, Value::Null).await;
    assert!(ack.is_ok());
    let (mut draining, ack) = subscribe_with(&fixture.socket, Value::Null).await;
    assert!(ack.is_ok());

    for sequence in 0..FLOOD_EVENTS {
        let name = flood_name(sequence);
        channel.emit(&mut fixture, &name).await;
        loop {
            let event = next_event(&mut draining).await;
            assert!(
                event.subscription_lagged().is_none(),
                "the draining subscriber never lags"
            );
            if channel.name_of(&event).as_deref() == Some(name.as_str()) {
                break;
            }
        }
    }

    // The stalled subscriber now reads: an in-order prefix, the gap notice,
    // then the events after the gap.
    let mut last_before_gap = None;
    let lagged = loop {
        let event = next_event(&mut stalled).await;
        if let Some(lagged) = event.subscription_lagged() {
            break lagged;
        }
        if let Some(sequence) = channel.name_of(&event).as_deref().and_then(flood_sequence) {
            assert_eq!(
                sequence,
                last_before_gap.map_or(0, |last| last + 1),
                "events before the gap arrive in order"
            );
            last_before_gap = Some(sequence);
        }
    };
    assert_eq!(
        serde_json::to_value(lagged.channel).expect("serialize the channel"),
        Value::from(channel.wire_name())
    );
    assert!(lagged.skipped > 0, "the notice counts the lost events");

    let first_after_gap = loop {
        let event = next_event(&mut stalled).await;
        assert!(
            event.subscription_lagged().is_none(),
            "one overflow, one notice"
        );
        if let Some(sequence) = channel.name_of(&event).as_deref().and_then(flood_sequence) {
            break sequence;
        }
    };
    let skipped = usize::try_from(lagged.skipped).expect("skipped fits usize");
    assert_eq!(
        first_after_gap,
        last_before_gap.map_or(0, |last| last + 1) + skipped,
        "the notice accounts for exactly the events between the prefix and the resumed stream"
    );

    fixture.stop().await;
}

#[tokio::test]
async fn a_lagging_session_subscriber_is_told_before_its_next_event() {
    assert_lag_is_reported("lag-session", Channel::Session).await;
}

#[tokio::test]
async fn a_lagging_notification_subscriber_is_told_before_its_next_event() {
    assert_lag_is_reported("lag-notification", Channel::Notification).await;
}
