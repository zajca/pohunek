//! Append-only event log (milestone 9).
//!
//! A local, owner-private JSON-lines audit/debug trail under `<data_dir>/events/`
//! (see `docs/architecture.md` "Configuration, State, and Log Storage"). It is
//! fed from [`crate::session::SessionRegistry::subscribe`] and writes **exactly
//! one JSON line per lifecycle [`Event`]** (session created/updated/stopped,
//! attach opened/closed, `agent_state`).
//!
//! The log **never contains secrets and never contains raw terminal bytes**: it
//! records only the structured control-plane events, whose payloads carry session
//! metadata (ids, cwd, size, state). PTY output flows on a *separate* broadcast
//! channel (`PtyHandle::subscribe_output`) that this log never taps, so terminal
//! bytes are out of reach by construction.
//!
//! Unlike the (deferred) `state.db`, this log is **not rebuildable**, so it is
//! append-only: each event is appended and flushed, never rewritten.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use protocol::Event;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// File name of the append-only event log inside the events directory.
const EVENT_LOG_NAME: &str = "events.jsonl";

/// Synthetic event recorded when the broadcast channel drops events faster than
/// the log can drain them, so the audit trail stays honest about gaps.
const EVENT_DROPPED: &str = "events_dropped";

/// Append-only, owner-private JSON-lines event log.
#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    /// The open append handle, behind a `Mutex` so a future second writer (e.g. a
    /// daemon-error sink) is safe; today only the drain task writes.
    file: Mutex<File>,
}

impl EventLog {
    /// Open (creating if needed) the append-only event log under `dir`.
    ///
    /// Creates `dir` owner-private (`0700`) and the log file owner-private
    /// (`0600`) in append mode.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the directory or file cannot be
    /// created/opened, so the daemon can fail fast on a misconfigured log
    /// location.
    pub fn open(dir: &Path) -> io::Result<Self> {
        create_private_dir(dir)?;
        let path = dir.join(EVENT_LOG_NAME);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// The backing log file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event as a single JSON line, flushed before returning so a
    /// crash cannot lose an already-recorded event.
    pub fn append(&self, event: &Event) -> io::Result<()> {
        let mut line = serde_json::to_string(event)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(|err| err.into_inner());
        file.write_all(line.as_bytes())?;
        file.flush()
    }
}

/// Create a directory (and parents) owner-private (`0700`).
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Spawn a background task that drains `events` into `log` until either the
/// broadcast closes or `shutdown` is cancelled.
///
/// A failed append is logged and the task keeps running — losing one audit line
/// must not take down the daemon. A broadcast *lag* (the channel dropped events
/// because the log fell behind) appends a synthetic [`EVENT_DROPPED`] record so
/// the trail never silently hides a gap.
///
/// On `shutdown`, the task makes a final non-blocking pass over whatever is still
/// buffered so events emitted just before shutdown are not silently lost; the
/// caller is expected to await the returned handle (see
/// [`crate::session::SessionRegistry::shutdown_event_log`]) so the flush
/// completes before the process exits.
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
                    drain_buffered(&log, &mut events);
                    break;
                }
                received = events.recv() => match received {
                    Ok(event) => append_event(&log, &event),
                    Err(broadcast::error::RecvError::Lagged(dropped)) => record_lag(&log, dropped),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    })
}

/// Drain every currently-buffered event without awaiting, used for the final
/// flush on shutdown.
fn drain_buffered(log: &EventLog, events: &mut broadcast::Receiver<Event>) {
    loop {
        match events.try_recv() {
            Ok(event) => append_event(log, &event),
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => record_lag(log, dropped),
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
}

/// Append one event, logging (not propagating) a write failure.
fn append_event(log: &EventLog, event: &Event) {
    if let Err(err) = log.append(event) {
        warn!(
            error = %err,
            event = %event.event,
            "failed to append event to the event log"
        );
    }
}

/// Record a broadcast lag as a synthetic [`EVENT_DROPPED`] marker so the trail
/// stays honest about the gap.
fn record_lag(log: &EventLog, dropped: u64) {
    warn!(dropped, "event log lagged; some events were not recorded");
    let marker = Event::new(EVENT_DROPPED, json!({ "dropped": dropped }));
    if let Err(err) = log.append(&marker) {
        warn!(error = %err, "failed to append events_dropped marker");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::{event, Event};
    use serde_json::{json, Value};
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use super::{spawn_drain, EventLog};

    fn temp_events_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "pohunek-events-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn read_lines(path: &std::path::Path) -> Vec<String> {
        fs::read_to_string(path)
            .expect("read event log")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn append_writes_exactly_one_json_line_per_event() {
        let dir = temp_events_dir("append");
        let log = EventLog::open(&dir).expect("open log");
        let events = [
            Event::new(
                event::SESSION_CREATED,
                json!({ "session": { "id": "s-1" } }),
            ),
            Event::new(
                event::AGENT_STATE,
                json!({ "session_id": "s-1", "activity": "working" }),
            ),
            Event::new(
                event::SESSION_STOPPED,
                json!({ "session": { "id": "s-1" } }),
            ),
        ];
        for e in &events {
            log.append(e).expect("append");
        }

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), events.len(), "one line per event");
        for (line, expected) in lines.iter().zip(events.iter()) {
            let parsed: Value = serde_json::from_str(line).expect("each line is valid JSON");
            assert_eq!(parsed["event"], json!(expected.event));
            assert!(
                parsed.get("v").is_some(),
                "each line carries a protocol version"
            );
        }
    }

    #[tokio::test]
    async fn drain_records_every_broadcast_event_exactly_once() {
        let dir = temp_events_dir("drain");
        let log = Arc::new(EventLog::open(&dir).expect("open log"));
        let (tx, rx) = broadcast::channel(16);
        let handle = spawn_drain(log.clone(), rx, CancellationToken::new());

        let sent = [
            Event::new(
                event::SESSION_CREATED,
                json!({ "session": { "id": "s-1" } }),
            ),
            Event::new(
                event::SESSION_UPDATED,
                json!({ "session": { "id": "s-1" } }),
            ),
            Event::new(
                event::SESSION_STOPPED,
                json!({ "session": { "id": "s-1" } }),
            ),
        ];
        for e in &sent {
            tx.send(e.clone()).expect("broadcast send");
        }
        // Dropping the sender closes the channel; the drain task finishes after
        // writing every buffered event, so awaiting it is a deterministic flush.
        drop(tx);
        handle.await.expect("drain task joins");

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), sent.len(), "exactly one line per event");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_file_and_dir_are_owner_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_events_dir("perms");
        let log = EventLog::open(&dir).expect("open log");
        log.append(&Event::new(event::SESSION_CREATED, json!({})))
            .expect("append");

        let dir_mode = fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "events dir must be owner-private");
        let file_mode = fs::metadata(log.path())
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600, "event log must be owner-private");
    }

    #[tokio::test]
    async fn reopening_appends_rather_than_truncates() {
        let dir = temp_events_dir("reopen");
        {
            let log = EventLog::open(&dir).expect("first open");
            log.append(&Event::new(event::SESSION_CREATED, json!({ "n": 1 })))
                .expect("append 1");
        }
        {
            let log = EventLog::open(&dir).expect("second open");
            log.append(&Event::new(event::SESSION_STOPPED, json!({ "n": 2 })))
                .expect("append 2");
            assert_eq!(
                read_lines(log.path()).len(),
                2,
                "reopen must append to the existing log, not truncate it"
            );
        }
    }

    #[tokio::test]
    async fn drain_keeps_running_after_a_slow_consumer_lag() {
        // A tiny channel forces a lag when more events are sent than buffered
        // before the drain catches up; the drain must survive and keep recording.
        let dir = temp_events_dir("lag");
        let log = Arc::new(EventLog::open(&dir).expect("open log"));
        let (tx, rx) = broadcast::channel(2);
        // Fill beyond capacity before the drain starts so the first recv lags.
        for n in 0..8 {
            let _ = tx.send(Event::new(event::SESSION_UPDATED, json!({ "n": n })));
        }
        let handle = spawn_drain(log.clone(), rx, CancellationToken::new());
        // Give the drain a moment to process the lag + remaining buffered events.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(tx);
        handle.await.expect("drain task joins");

        // The log is non-empty and every line is a valid event (the synthetic
        // events_dropped marker included), proving the lag did not wedge the task.
        let lines = read_lines(log.path());
        assert!(!lines.is_empty(), "drain must keep recording after a lag");
        for line in &lines {
            let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
            assert!(parsed.get("event").is_some());
        }
    }

    #[tokio::test]
    async fn drain_flushes_buffered_events_on_shutdown_cancellation() {
        let dir = temp_events_dir("shutdown-flush");
        let log = Arc::new(EventLog::open(&dir).expect("open log"));
        let (tx, rx) = broadcast::channel(16);
        let shutdown = CancellationToken::new();
        let handle = spawn_drain(log.clone(), rx, shutdown.clone());

        // Buffer events, then trigger shutdown WITHOUT dropping the sender — the
        // real daemon keeps the broadcast Sender alive at shutdown, so the drain
        // never sees Closed and must rely on the cancellation flush instead.
        for n in 0..4 {
            tx.send(Event::new(event::SESSION_UPDATED, json!({ "n": n })))
                .expect("broadcast send");
        }
        shutdown.cancel();
        handle.await.expect("drain joins after shutdown");

        assert_eq!(
            read_lines(log.path()).len(),
            4,
            "shutdown flush must record every event buffered before cancellation"
        );
    }
}
