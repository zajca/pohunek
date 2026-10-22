//! Bounded event journal (milestone 9).
//!
//! A local, owner-private JSON-lines audit/debug trail under `<data_dir>/events/`
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). It is
//! fed from daemon control-plane broadcast channels and writes **one JSON line
//! per structured [`Event`]** (session created/updated/stopped, attach
//! opened/closed, `agent_state`, notification lifecycle events), except for
//! `agent_state` refreshes that restate the state a session already journaled.
//!
//! The log **never contains secrets and never contains raw terminal bytes**: it
//! records only the structured control-plane events, whose payloads carry bounded
//! daemon metadata. PTY output flows on separate authenticated worker data
//! streams that this log never opens, so terminal bytes are out of reach by
//! construction.
//!
//! The journal is bounded by [`EventRetention`]: a file rotates before a write
//! would cross `max_file_bytes`, at most `max_files` files are kept, and rotated
//! files older than `max_age` are pruned. Rotation, pruning and the oversized
//! file check at open use file metadata, `rename` and `unlink` only; the
//! journal is never read back.
//!
//! One dedicated writer thread owns the files and the heartbeat filter, so no
//! tokio worker ever waits on the filesystem. Drain tasks hand events to it
//! over a bounded queue; a full queue makes the drain wait, the broadcast
//! receiver then lags, and the lag is recorded as an [`EVENT_DROPPED`] line.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use pohunek_logging::{Files, Legacy, Policy, Writer};
use pohunek_service_config::EventRetention;
use protocol::Event;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Build one daemon-originated event at the daemon's current protocol version.
///
/// Event names are compile-time constants and payloads are daemon-owned JSON
/// objects. Centralizing the checked constructor keeps producers concise while
/// subscription transport remains responsible for projecting the negotiated
/// version onto each connection.
pub(crate) fn event(name: &str, payload: serde_json::Value) -> Event {
    Event::new(protocol::PROTOCOL_VERSION, name, payload)
        .expect("daemon events use valid names and object payloads")
}

/// File name of the active journal file inside the events directory.
const EVENT_LOG_NAME: &str = "events.jsonl";

/// Name of the writer thread, as it appears in process listings and backtraces.
const WRITER_THREAD_NAME: &str = "pohunek-event-journal";

/// Events the writer queue holds before a drain waits for the writer.
///
/// A control-plane event is a few hundred bytes to a few kilobytes, so the
/// queue stays within a few megabytes while it absorbs a burst far larger than
/// the broadcast channel's own buffer.
const WRITER_QUEUE_CAPACITY: usize = 1024;

/// Synthetic event recorded when the broadcast channel drops events faster than
/// the log can drain them, so the audit trail stays honest about gaps.
const EVENT_DROPPED: &str = "events_dropped";

/// What the drain tasks send to the writer thread.
enum Command {
    /// Journal one event, unless it restates a session's last journaled state.
    Append(Event),
    /// Acknowledge once every earlier command has been processed.
    Flush(oneshot::Sender<()>),
}

/// Bounded, owner-private JSON-lines event journal.
#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    queue: mpsc::Sender<Command>,
}

impl EventLog {
    /// Open the journal under `dir` and start its writer thread.
    ///
    /// Creates `dir` owner-private (`0700`); journal files are `0600`. A
    /// journal file above `retention.max_file_bytes`, such as one an earlier
    /// release grew without bound, or last written longer ago than
    /// `retention.max_age`, is removed without being read, with one warning
    /// that names its path and size. Rotated files past the size,
    /// count or age bound are pruned.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the directory cannot be prepared or is a
    /// symlink, an owned file cannot be inspected or pruned, or the writer
    /// thread cannot start, so the daemon can fail fast on a misconfigured log
    /// location.
    pub fn open(dir: &Path, retention: EventRetention) -> io::Result<Self> {
        let path = dir.join(EVENT_LOG_NAME);
        let policy = Policy::new(
            retention.max_file_bytes,
            usize::try_from(retention.max_files).map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)?
        .with_max_age(retention.max_age);
        let removal = pending_removal(&path, retention);
        let files = Files::new(EVENT_LOG_NAME, Legacy::None).map_err(io::Error::other)?;
        let writer = Writer::open(dir, files, policy).map_err(io::Error::other)?;
        if let Some((removed_bytes, removal)) = removal {
            warn!(
                path = %path.display(),
                removed_bytes,
                reason = removal.reason(),
                "event journal removed at start without being read"
            );
        }
        let (queue, commands) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        thread::Builder::new()
            .name(WRITER_THREAD_NAME.to_owned())
            .spawn(move || run_writer(writer, commands))?;
        Ok(Self { path, queue })
    }

    /// The active journal file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue one event for the writer thread, waiting while the queue is full.
    pub async fn append(&self, event: Event) {
        let name = event.event().to_owned();
        if self.queue.send(Command::Append(event)).await.is_err() {
            warn!(event = %name, "event journal writer stopped; event not recorded");
        }
    }

    /// Wait until the writer has processed everything queued before this call.
    pub async fn flush(&self) {
        let (ack, acked) = oneshot::channel();
        if self.queue.send(Command::Flush(ack)).await.is_ok() {
            let _ = acked.await;
        }
    }
}

/// Why the writer removes the active journal file when it opens.
#[derive(Debug, Clone, Copy)]
enum Removal {
    AboveSizeBound,
    PastAgeBound,
}

impl Removal {
    const fn reason(self) -> &'static str {
        match self {
            Self::AboveSizeBound => "above its size bound",
            Self::PastAgeBound => "last written longer ago than its age bound",
        }
    }
}

/// The active journal file's size and why opening the writer will remove it.
///
/// Only metadata is inspected; the file content is never opened.
fn pending_removal(path: &Path, retention: EventRetention) -> Option<(u64, Removal)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    if metadata.len() > retention.max_file_bytes {
        return Some((metadata.len(), Removal::AboveSizeBound));
    }
    let age = metadata.modified().ok()?.elapsed().ok()?;
    (age > retention.max_age).then_some((metadata.len(), Removal::PastAgeBound))
}

/// Writer thread body: journals queued events until every sender is gone.
fn run_writer(mut writer: Writer, mut commands: mpsc::Receiver<Command>) {
    let mut filter = HeartbeatFilter::default();
    while let Some(command) = commands.blocking_recv() {
        match command {
            Command::Append(event) => {
                if !filter.admits(&event) {
                    continue;
                }
                if let Err(err) = write_line(&mut writer, &event) {
                    filter.forget(&event);
                    warn!(
                        error = %err,
                        event = %event.event(),
                        "failed to append event to the event log"
                    );
                }
            }
            Command::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

/// Serialize `event` as one line and hand it to the writer as a single unit,
/// so a rotation never splits it.
fn write_line(writer: &mut Writer, event: &Event) -> io::Result<()> {
    let mut line = serde_json::to_string(event)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    line.push('\n');
    writer.write_all(line.as_bytes())
}

/// Drops `agent_state` refreshes that restate what a session last journaled.
///
/// A refresh differs from the previous journaled event of its session only in
/// `revision`. Any other difference (activity, source, runtime, epoch) is a
/// transition and is journaled. The remembered state is per session and is
/// forgotten when the session stops or is removed, and across daemon restarts.
#[derive(Debug, Default)]
struct HeartbeatFilter {
    last_state: HashMap<String, Value>,
}

impl HeartbeatFilter {
    /// Whether `event` belongs in the journal.
    fn admits(&mut self, event: &Event) -> bool {
        match event.event() {
            protocol::event::AGENT_STATE => self.admits_agent_state(event.payload()),
            protocol::event::SESSION_STOPPED | protocol::event::SESSION_REMOVED => {
                if let Some(id) = event.payload()["session"]["id"].as_str() {
                    self.last_state.remove(id);
                }
                true
            }
            _ => true,
        }
    }

    /// Forgets the remembered state of `event`'s session, so the next
    /// identical refresh is journaled after a failed write.
    fn forget(&mut self, event: &Event) {
        if let Some(id) = event.payload()["session_id"].as_str() {
            self.last_state.remove(id);
        }
    }

    fn admits_agent_state(&mut self, payload: &Value) -> bool {
        let (Some(id), Some(fields)) = (payload["session_id"].as_str(), payload.as_object()) else {
            return true;
        };
        let mut state = fields.clone();
        state.remove("revision");
        let state = Value::Object(state);
        if self.last_state.get(id) == Some(&state) {
            return false;
        }
        self.last_state.insert(id.to_owned(), state);
        true
    }
}

/// Spawn a background task that drains `events` into `log` until either the
/// broadcast closes or `shutdown` is cancelled.
///
/// A broadcast *lag* (the channel dropped events because the log fell behind)
/// queues a synthetic [`EVENT_DROPPED`] record so the trail never silently
/// hides a gap.
///
/// On `shutdown`, the task makes a final non-blocking pass over whatever is still
/// buffered so events emitted just before shutdown are not silently lost, then
/// waits until the writer has them on disk. The caller is expected to await the
/// returned handle (see [`crate::session::SessionRegistry::shutdown_event_log`])
/// so the flush completes before the process exits.
#[must_use]
pub fn spawn_drain(
    log: Arc<EventLog>,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    drain_buffered(&log, &mut events).await;
                    break;
                }
                received = events.recv() => match received {
                    Ok(event) => log.append(event).await,
                    Err(broadcast::error::RecvError::Lagged(dropped)) => record_lag(&log, dropped).await,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        log.flush().await;
    })
}

/// Drain every currently-buffered event without awaiting new ones, used for the
/// final flush on shutdown.
async fn drain_buffered(log: &EventLog, events: &mut broadcast::Receiver<Event>) {
    loop {
        match events.try_recv() {
            Ok(event) => log.append(event).await,
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => record_lag(log, dropped).await,
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
}

/// Record a broadcast lag as a synthetic [`EVENT_DROPPED`] marker so the trail
/// stays honest about the gap.
async fn record_lag(log: &EventLog, dropped: u64) {
    warn!(dropped, "event log lagged; some events were not recorded");
    log.append(event(EVENT_DROPPED, json!({ "dropped": dropped })))
        .await;
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use pohunek_service_config::EventRetention;
    use protocol::{event, Event};
    use serde_json::{json, Value};
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::{spawn_drain, EventLog, EVENT_DROPPED, WRITER_QUEUE_CAPACITY};

    const DAY: Duration = Duration::from_hours(24);

    fn retention(max_file_bytes: u64, max_files: u32) -> EventRetention {
        EventRetention {
            max_file_bytes,
            max_files,
            max_age: 30 * DAY,
        }
    }

    fn events_dir(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let root = pohunek_test_support::tempdir_with_prefix(&format!("pohunek-events-{tag}-"))
            .expect("create isolated test directory");
        let dir = root.path().join("events");
        (root, dir)
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("read event log")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is valid JSON"))
            .collect()
    }

    /// Journal files of the family, `(name, size)` sorted by name, lock excluded.
    fn family(dir: &Path) -> Vec<(String, u64)> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .expect("read events dir")
            .map(|entry| entry.expect("dir entry"))
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name.starts_with("events.jsonl")
                    && Path::new(&name).extension().is_none_or(|ext| ext != "lock"))
                .then(|| (name, entry.metadata().expect("metadata").len()))
            })
            .collect();
        files.sort();
        files
    }

    fn updated(n: u64) -> Event {
        crate::events::event(event::SESSION_UPDATED, json!({ "n": n }))
    }

    fn agent_state(session: &str, activity: &str, source: &str, revision: u64) -> Event {
        crate::events::event(
            event::AGENT_STATE,
            json!({
                "session_id": session,
                "activity": activity,
                "source": source,
                "runtime": { "worker_instance_id": "w-1", "runtime_generation": 1 },
                "activity_epoch": "epoch-1",
                "revision": revision,
            }),
        )
    }

    async fn journal(dir: &Path, retention: EventRetention, events: Vec<Event>) -> Arc<EventLog> {
        let log = Arc::new(EventLog::open(dir, retention).expect("open log"));
        for event in events {
            log.append(event).await;
        }
        log.flush().await;
        log
    }

    fn names(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line["event"].as_str().expect("event name").to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_burst_rotates_the_journal_and_keeps_it_bounded_with_the_newest_events() {
        let (_root, dir) = events_dir("burst");
        let bound = retention(4096, 3);
        let log = journal(&dir, bound, (0..400).map(updated).collect()).await;

        let files = family(&dir);
        assert!(files.len() <= 3, "at most max_files files: {files:?}");
        assert!(
            files.iter().all(|(_, size)| *size <= 4096),
            "every file within max_file_bytes: {files:?}"
        );
        let active = read_lines(log.path());
        assert_eq!(active.last().expect("newest line")["n"], 399);
        let oldest_kept = files
            .iter()
            .filter(|(name, _)| name != "events.jsonl")
            .flat_map(|(name, _)| read_lines(&dir.join(name)))
            .map(|line| line["n"].as_u64().expect("n"))
            .min()
            .expect("rotated files exist");
        assert!(oldest_kept > 0, "the oldest events were evicted");
    }

    #[tokio::test]
    async fn one_line_larger_than_the_bound_is_replaced_and_later_events_still_land() {
        let (_root, dir) = events_dir("oversize-line");
        let huge = crate::events::event(
            event::SESSION_UPDATED,
            json!({ "blob": "x".repeat(10_000) }),
        );
        let log = journal(&dir, retention(4096, 2), vec![huge, updated(1)]).await;

        let files = family(&dir);
        assert!(files.iter().all(|(_, size)| *size <= 4096), "{files:?}");
        let lines = read_lines(log.path());
        assert_eq!(lines.last().expect("line")["n"], 1);
        assert!(
            !fs::read_to_string(log.path())
                .expect("read")
                .contains("xxxxxxxx"),
            "the oversized event is dropped as a unit"
        );
    }

    #[tokio::test]
    async fn restated_agent_state_is_journaled_once() {
        let (_root, dir) = events_dir("refresh");
        let events = (1..=50)
            .map(|rev| agent_state("s-1", "idle", "screen", rev))
            .collect();
        let log = journal(&dir, retention(1 << 20, 2), events).await;

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1, "50 refreshes of one state: {lines:?}");
        assert_eq!(names(&lines), ["agent_state"]);
        assert!(
            !names(&lines).contains(&EVENT_DROPPED.to_owned()),
            "filtered refreshes are not drops"
        );
    }

    #[tokio::test]
    async fn every_state_change_of_a_session_is_journaled() {
        let (_root, dir) = events_dir("transitions");
        let mut new_runtime = agent_state("s-1", "idle", "screen", 9);
        let mut payload = new_runtime.payload().clone();
        payload["runtime"] = json!({ "worker_instance_id": "w-2", "runtime_generation": 2 });
        new_runtime = crate::events::event(event::AGENT_STATE, payload);
        let mut new_epoch = agent_state("s-1", "idle", "screen", 1);
        let mut payload = new_epoch.payload().clone();
        payload["runtime"] = json!({ "worker_instance_id": "w-2", "runtime_generation": 2 });
        payload["activity_epoch"] = json!("epoch-2");
        new_epoch = crate::events::event(event::AGENT_STATE, payload);
        let events = vec![
            agent_state("s-1", "idle", "screen", 1),
            agent_state("s-1", "working", "screen", 2),
            agent_state("s-1", "working", "screen", 3),
            agent_state("s-1", "idle", "screen", 4),
            agent_state("s-1", "idle", "osc_title", 5),
            agent_state("s-2", "idle", "osc_title", 1),
            new_runtime,
            new_epoch,
        ];
        let log = journal(&dir, retention(1 << 20, 2), events).await;

        // idle, working, idle, idle/osc_title, s-2, new runtime, new epoch
        assert_eq!(read_lines(log.path()).len(), 7);
    }

    #[tokio::test]
    async fn a_stopped_session_forgets_its_state_and_other_events_are_never_filtered() {
        let (_root, dir) = events_dir("forget");
        let stopped = crate::events::event(
            event::SESSION_STOPPED,
            json!({ "session": { "id": "s-1" } }),
        );
        let bad = crate::events::event(event::AGENT_STATE, json!({ "activity": "idle" }));
        let events = vec![
            agent_state("s-1", "idle", "screen", 1),
            stopped.clone(),
            agent_state("s-1", "idle", "screen", 2),
            bad.clone(),
            bad,
            updated(1),
            updated(1),
            stopped,
        ];
        let log = journal(&dir, retention(1 << 20, 2), events).await;

        assert_eq!(
            names(&read_lines(log.path())),
            [
                "agent_state",
                "session_stopped",
                "agent_state",
                "agent_state",
                "agent_state",
                "session_updated",
                "session_updated",
                "session_stopped",
            ],
        );
    }

    #[tokio::test]
    async fn the_first_agent_state_after_a_restart_is_journaled() {
        let (_root, dir) = events_dir("restart");
        let first = journal(
            &dir,
            retention(1 << 20, 2),
            vec![agent_state("s-1", "idle", "screen", 1)],
        )
        .await;
        drop(first);
        let second = journal(
            &dir,
            retention(1 << 20, 2),
            vec![agent_state("s-1", "idle", "screen", 2)],
        )
        .await;

        assert_eq!(read_lines(second.path()).len(), 2);
    }

    /// Holds the journal's family lock, so its writer thread blocks in the filesystem.
    fn hold_family_lock(dir: &Path) -> fs::File {
        let lock = fs::File::options()
            .read(true)
            .write(true)
            .open(dir.join("events.jsonl.lock"))
            .expect("open family lock");
        lock.lock().expect("take family lock");
        lock
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_blocked_writer_never_blocks_the_executor_and_loses_nothing_on_release() {
        let (_root, dir) = events_dir("blocked");
        let log = Arc::new(EventLog::open(&dir, retention(1 << 20, 2)).expect("open log"));
        let lock = hold_family_lock(&dir);
        let (tx, rx) = broadcast::channel(64);
        let shutdown = CancellationToken::new();
        let drain = spawn_drain(Arc::clone(&log), rx, shutdown.clone());
        for n in 0..8 {
            tx.send(updated(n)).expect("broadcast send");
        }

        // The only thread of this runtime is free while the writer sits on the lock.
        tokio::spawn(async {})
            .await
            .expect("executor still runs tasks");
        tokio::task::yield_now().await;

        lock.unlock().expect("release family lock");
        shutdown.cancel();
        drain.await.expect("drain joins");
        assert_eq!(read_lines(log.path()).len(), 8);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_overloaded_queue_backpressures_the_drain_and_records_the_gap() {
        let (_root, dir) = events_dir("overload");
        let log = Arc::new(EventLog::open(&dir, retention(8 << 20, 2)).expect("open log"));
        let lock = hold_family_lock(&dir);
        let (tx, rx) = broadcast::channel(4);
        let shutdown = CancellationToken::new();
        let drain = spawn_drain(Arc::clone(&log), rx, shutdown.clone());
        // One event sits in the blocked writer, the queue holds WRITER_QUEUE_CAPACITY
        // more; the rest overflow the broadcast buffer while the drain waits.
        let total = WRITER_QUEUE_CAPACITY + 1 + 32;
        for n in 0..total {
            tx.send(updated(n as u64)).expect("broadcast send");
            tokio::task::yield_now().await;
        }

        lock.unlock().expect("release family lock");
        shutdown.cancel();
        drain.await.expect("drain survives the overload");

        let lines = read_lines(log.path());
        let dropped: u64 = lines
            .iter()
            .filter(|line| line["event"] == EVENT_DROPPED)
            .map(|line| line["dropped"].as_u64().expect("count"))
            .sum();
        assert!(dropped > 0, "the gap is recorded");
        let journaled = lines.len() as u64
            - lines.iter().filter(|l| l["event"] == EVENT_DROPPED).count() as u64;
        assert_eq!(
            journaled + dropped,
            total as u64,
            "every event is journaled or counted"
        );
    }

    #[tokio::test]
    async fn drain_records_every_broadcast_event_exactly_once() {
        let (_root, dir) = events_dir("drain");
        let log = Arc::new(EventLog::open(&dir, retention(1 << 20, 2)).expect("open log"));
        let (tx, rx) = broadcast::channel(16);
        let handle = spawn_drain(Arc::clone(&log), rx, CancellationToken::new());
        for n in 0..3 {
            tx.send(updated(n)).expect("broadcast send");
        }
        // Dropping the sender closes the channel; the drain finishes after the
        // writer has every buffered event on disk, so awaiting it is a flush.
        drop(tx);
        handle.await.expect("drain task joins");

        assert_eq!(read_lines(log.path()).len(), 3);
    }

    #[tokio::test]
    async fn drain_flushes_buffered_events_on_shutdown_cancellation() {
        let (_root, dir) = events_dir("shutdown-flush");
        let log = Arc::new(EventLog::open(&dir, retention(1 << 20, 2)).expect("open log"));
        let (tx, rx) = broadcast::channel(16);
        let shutdown = CancellationToken::new();
        let handle = spawn_drain(Arc::clone(&log), rx, shutdown.clone());

        // The real daemon keeps the broadcast Sender alive at shutdown, so the
        // drain never sees Closed and must rely on the cancellation flush.
        for n in 0..4 {
            tx.send(updated(n)).expect("broadcast send");
        }
        shutdown.cancel();
        handle.await.expect("drain joins after shutdown");

        assert_eq!(read_lines(log.path()).len(), 4);
    }

    #[tokio::test]
    async fn drain_keeps_running_after_a_slow_consumer_lag() {
        let (_root, dir) = events_dir("lag");
        let log = Arc::new(EventLog::open(&dir, retention(1 << 20, 2)).expect("open log"));
        let (tx, rx) = broadcast::channel(2);
        // Fill beyond capacity before the drain starts so the first recv lags.
        for n in 0..8 {
            let _ = tx.send(updated(n));
        }
        let handle = spawn_drain(Arc::clone(&log), rx, CancellationToken::new());
        drop(tx);
        handle.await.expect("drain task joins");

        let lines = read_lines(log.path());
        assert!(names(&lines).contains(&EVENT_DROPPED.to_owned()));
        assert!(lines.len() > 1, "drain must keep recording after a lag");
    }

    /// Collects formatted log output of the test thread's default subscriber.
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogCapture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            // A second registered dispatcher keeps callsite interest per thread.
            static ANCHOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
            ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
            let sink = self.clone();
            tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_writer(move || sink.clone())
                    .with_ansi(false)
                    .finish(),
            )
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("capture")).into_owned()
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_journal_above_the_bound_at_start_is_removed_unread_with_one_warning() {
        use std::os::unix::fs::PermissionsExt;

        let (_root, dir) = events_dir("oversized-start");
        fs::create_dir_all(&dir).expect("create dir");
        let old = dir.join("events.jsonl");
        let file = fs::File::create(&old).expect("create old journal");
        file.set_len(5 << 20).expect("sparse growth");
        drop(file);
        // Write-only: any attempt to read the old journal fails.
        fs::set_permissions(&old, fs::Permissions::from_mode(0o200)).expect("chmod");
        let logs = LogCapture::default();
        let _guard = logs.install();

        let log = journal(&dir, retention(1 << 20, 3), vec![updated(1)]).await;

        let warnings = logs.text();
        assert_eq!(
            warnings.matches("removed at start").count(),
            1,
            "{warnings}"
        );
        assert!(warnings.contains(&old.display().to_string()), "{warnings}");
        assert!(warnings.contains("removed_bytes=5242880"), "{warnings}");
        assert_eq!(family(&dir).len(), 1, "no rotated copy of the old journal");
        assert_eq!(read_lines(log.path()).len(), 1);
    }

    #[tokio::test]
    async fn start_prunes_rotated_files_past_the_count_size_and_age_bounds() {
        let (_root, dir) = events_dir("prune");
        fs::create_dir_all(&dir).expect("create dir");
        let age = |name: &str, by: Duration| {
            fs::File::options()
                .write(true)
                .open(dir.join(name))
                .expect("open")
                .set_modified(SystemTime::now() - by)
                .expect("set mtime");
        };
        for name in [
            "events.jsonl.1",
            "events.jsonl.2",
            "events.jsonl.3",
            "events.jsonl.4",
        ] {
            fs::write(dir.join(name), b"{}\n").expect("write rotated");
        }
        fs::File::options()
            .write(true)
            .open(dir.join("events.jsonl.2"))
            .expect("open")
            .set_len(8192)
            .expect("oversized rotated file");
        age("events.jsonl.1", DAY);
        let bound = EventRetention {
            max_file_bytes: 4096,
            max_files: 4,
            max_age: 2 * DAY,
        };
        age("events.jsonl.3", 3 * DAY);

        drop(EventLog::open(&dir, bound).expect("open"));

        // .2 is over the size bound, .3 past the age bound, .4 beyond max_files - 1.
        let names: Vec<_> = family(&dir).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["events.jsonl.1"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn journal_files_are_owner_private_and_a_loose_existing_file_is_tightened() {
        use std::os::unix::fs::PermissionsExt;

        let (_root, dir) = events_dir("perms");
        fs::create_dir_all(&dir).expect("create dir");
        let active = dir.join("events.jsonl");
        fs::write(&active, "{\"event\":\"old\"}\n").expect("write loose log");
        fs::set_permissions(&active, fs::Permissions::from_mode(0o666)).expect("loosen");

        let log = journal(&dir, retention(4096, 3), (0..200).map(updated).collect()).await;

        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "events dir is owner-private");
        for (name, _) in family(&dir) {
            assert_eq!(mode(&dir.join(&name)), 0o600, "{name}");
        }
        assert_eq!(mode(&dir.join("events.jsonl.lock")), 0o600);
        assert_eq!(mode(log.path()), 0o600);
        assert!(family(&dir).len() > 1, "the scenario rotated");
    }

    #[tokio::test]
    async fn an_active_file_past_the_age_bound_is_removed_at_start_with_a_warning() {
        let (_root, dir) = events_dir("stale-active");
        fs::create_dir_all(&dir).expect("create dir");
        let active = dir.join("events.jsonl");
        fs::write(&active, b"{\"event\":\"old\"}\n").expect("write old journal");
        fs::File::options()
            .write(true)
            .open(&active)
            .expect("open")
            .set_modified(SystemTime::now() - 40 * DAY)
            .expect("set mtime");
        let logs = LogCapture::default();
        let _guard = logs.install();

        let log = journal(&dir, retention(1 << 20, 3), vec![updated(1)]).await;

        let warnings = logs.text();
        assert_eq!(
            warnings.matches("removed at start").count(),
            1,
            "{warnings}"
        );
        assert!(warnings.contains("age bound"), "{warnings}");
        assert_eq!(
            read_lines(log.path()).len(),
            1,
            "only the new event remains"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_events_directory_is_refused() {
        let (root, dir) = events_dir("symlink");
        let target = root.path().join("elsewhere");
        fs::create_dir_all(&target).expect("create target");
        fs::write(target.join("events.jsonl"), vec![b'x'; 4096]).expect("write big file");
        let logs = LogCapture::default();
        let _guard = logs.install();
        std::os::unix::fs::symlink(&target, &dir).expect("symlink events dir");

        EventLog::open(&dir, retention(1024, 2)).expect_err("a symlinked events dir is refused");
        assert!(
            target.join("events.jsonl").exists(),
            "the target is untouched"
        );
        assert!(!logs.text().contains("removed at start"), "{}", logs.text());
    }
}
