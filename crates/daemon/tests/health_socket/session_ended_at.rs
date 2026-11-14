//! The retention clock of a finished session across daemon restarts: it ages
//! from the moment it ended, not from the last time its record was written.

// Rust guideline compliant 2026-11-11

use super::*;
use protocol::{SessionRetentionParams, SessionRetentionReason};

/// The end time the scenarios give the finished session, long before the
/// default terminal TTL.
const LONG_AGO: &str = "2026-01-01T00:00:00Z";

/// The last update the first scenario gives the record before the restart.
const LONG_AGO_EDIT: &str = "2026-01-02T00:00:00Z";

/// A shell that exits at once, so the session ends on its own.
const EXIT_AT_ONCE: &str = "exit 0";

/// Whether the restarted daemon still sees the worker journal of the session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Journal {
    /// The journal proves the worker ended, so the runtime is terminal.
    Kept,
    /// Nothing proves what became of the worker, so the runtime is lost.
    Missing,
}

/// Rewrites the session lines of the store at `path` with `edit`.
fn rewrite_session_lines(path: &Path, edit: impl Fn(&mut serde_json::Map<String, Value>)) {
    let text = std::fs::read_to_string(path).expect("read the store");
    let mut body = String::new();
    for line in text.lines() {
        let mut value: Value = serde_json::from_str(line).expect("a store line is JSON");
        if value.get("kind").and_then(Value::as_str) == Some("session") {
            edit(value.as_object_mut().expect("a session line is an object"));
        }
        body.push_str(&serde_json::to_string(&value).expect("encode the line"));
        body.push('\n');
    }
    std::fs::write(path, body).expect("write the store");
}

/// The `info` object of one session line.
fn info_of(line: &mut serde_json::Map<String, Value>) -> &mut serde_json::Map<String, Value> {
    line.get_mut("info")
        .and_then(Value::as_object_mut)
        .expect("a session line has info")
}

/// Directory below the scenario root that keeps the worker journals the first
/// run wrote, because the first run's worker home goes away with its daemon.
const KEPT_JOURNALS: &str = "kept-journals";

/// Where a worker home keeps its journals.
const JOURNALS: &str = "state/pohunek/workers";

/// Copies the directory `from` to `to` with `cp -a`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.parent().expect("a parent")).expect("create the parent");
    let copied = std::process::Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .expect("run cp");
    assert!(copied.success(), "copy {}", from.display());
}

/// Whether every journal below `journals` is sealed as acknowledged and no
/// journal rewrite is in flight.
///
/// The worker rewrites its sealed journal through a dot-prefixed temporary
/// file and a rename once the daemon acknowledges the terminal state, so a copy
/// taken before that settles can list a name that is gone by the time it is read.
fn journals_settled(journals: &Path) -> bool {
    let Ok(sessions) = std::fs::read_dir(journals) else {
        return false;
    };
    let mut seen = 0;
    for session in sessions.flatten() {
        let Ok(files) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            let Ok(text) = std::fs::read_to_string(file.path()) else {
                return false;
            };
            let Ok(journal) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            if journal.get("terminal_acknowledged") != Some(&Value::Bool(true)) {
                return false;
            }
            seen += 1;
        }
    }
    seen > 0
}

/// Runs a session that ends on its own, keeps its worker journal and stops
/// the daemon.
async fn finished_session_and_stopped_daemon(
    socket: &TestSocket,
    config: impl Fn() -> SessionRegistryConfig,
    edit_metadata: bool,
) -> SessionId {
    let (shutdown, handle, worker_home) = spawn_server_with_config(socket, "0.0.0", config()).await;
    let mut control = connect(socket).await;
    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(
            &mut control,
            session_params_in(socket.dir.join(WORK_DIR_NAME)),
        )
        .await,
    ))
    .expect("session info");
    let ended = wait_for_state(&mut control, &session.id, SessionState::Done).await;
    assert_eq!(
        ended.ended_at.as_deref(),
        Some(ended.updated_at.as_str()),
        "the exit records when it ended"
    );
    if edit_metadata {
        session_metadata::set_edited_key(&mut control, &session.id).await;
        let edited = inspect_session(&mut control, &session.id).await;
        assert_eq!(
            edited.ended_at, ended.ended_at,
            "a metadata edit leaves the end time alone"
        );
        assert_ne!(
            edited.updated_at, ended.updated_at,
            "the edit is a record update"
        );
    }
    let journals = worker_home.join(JOURNALS);
    wait_until("the worker journal to be acknowledged", || async {
        journals_settled(&journals).then_some(())
    })
    .await;
    copy_tree(&journals, &socket.dir.join(KEPT_JOURNALS).join("workers"));
    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
    session.id
}

/// Restarts the daemon over the store and returns the dry-run retention
/// selection with the restarted view of the session.
async fn restart_and_select(
    socket: &TestSocket,
    config: SessionRegistryConfig,
    id: &SessionId,
    journal: Journal,
) -> (SessionInfo, protocol::SessionRetentionResult) {
    let (restarted, worker_home) = worker_backed_registry(socket, config);
    if journal == Journal::Kept {
        // The restarted daemon finds the journal the first run left, as a
        // daemon restarted on the same host state does.
        let target = worker_home.path().join(JOURNALS);
        let _ = std::fs::remove_dir_all(&target);
        copy_tree(&socket.dir.join(KEPT_JOURNALS).join("workers"), &target);
    }
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciles the finished session");
    let session = restarted
        .inspect(id)
        .await
        .expect("the restarted daemon lists the finished session");
    let selection = restarted
        .sweep_retention(&SessionRetentionParams {
            dry_run: true,
            limit: None,
            ..Default::default()
        })
        .await
        .expect("dry-run sweep");
    (session, selection)
}

fn registry_config(socket: &TestSocket) -> impl Fn() -> SessionRegistryConfig + '_ {
    let store_path = socket.dir.join("metadata.jsonl");
    move || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", EXIT_AT_ONCE]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    }
}

#[tokio::test]
async fn a_finished_session_keeps_its_age_across_a_restart_and_a_metadata_edit() {
    let socket = temp_socket("ended-age-restart");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, true).await;

    // The record says the session ended long ago, as it would for a session
    // that finished before several restarts.
    rewrite_session_lines(&store_path, |line| {
        let info = info_of(line);
        info.insert("ended_at".to_owned(), Value::from(LONG_AGO));
        info.insert("updated_at".to_owned(), Value::from(LONG_AGO_EDIT));
    });

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Kept).await;

    assert_eq!(session.state, SessionState::Done);
    assert_eq!(
        session.ended_at.as_deref(),
        Some(LONG_AGO),
        "the restart and the metadata edit leave the end time alone"
    );
    assert_ne!(
        session.updated_at, LONG_AGO_EDIT,
        "the restart rewrote the record, which is why updated_at cannot be the clock"
    );
    let candidate = selection
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == id)
        .unwrap_or_else(|| {
            panic!("the long-finished session is selected for removal: {selection:?} {session:?}")
        });
    assert_eq!(candidate.reason, SessionRetentionReason::Terminal);
    assert!(
        candidate.age_secs > 60 * 24 * 3600,
        "the age counts from the end time: {}",
        candidate.age_secs
    );
}

#[tokio::test]
async fn a_session_that_ended_a_moment_ago_is_not_selected_after_a_restart() {
    let socket = temp_socket("ended-fresh-restart");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, false).await;

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Kept).await;

    assert!(session.ended_at.is_some(), "an ended session records when");
    assert!(
        selection
            .sessions
            .iter()
            .all(|candidate| candidate.session_id != id),
        "a recently finished session is kept: {:?}",
        selection.sessions
    );
}

#[tokio::test]
async fn a_record_written_before_the_end_time_existed_ages_from_its_last_update() {
    let socket = temp_socket("ended-old-record");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, false).await;

    // A schema 18 record: no end time, last updated long ago.
    rewrite_session_lines(&store_path, |line| {
        line.insert("schema_version".to_owned(), Value::from(18));
        let info = info_of(line);
        info.remove("ended_at");
        info.insert("updated_at".to_owned(), Value::from(LONG_AGO));
    });
    Store::new(store_path.clone())
        .migrate_to_current()
        .expect("migrate the store at startup");

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Kept).await;

    assert_eq!(
        session.ended_at.as_deref(),
        Some(LONG_AGO),
        "the migration turns the last update into the end time and the restart keeps it"
    );
    let candidate = selection
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == id)
        .expect("the old record is selected for removal");
    assert!(candidate.age_secs > 60 * 24 * 3600);
}

#[tokio::test]
async fn a_lost_session_ages_from_its_last_connection_not_from_its_end_time() {
    let socket = temp_socket("ended-lost-old-connection");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, false).await;
    rewrite_session_lines(&store_path, |line| {
        let info = info_of(line);
        let runtime = info
            .get_mut("runtime")
            .and_then(Value::as_object_mut)
            .expect("the session has a runtime");
        runtime.insert("last_connected_at".to_owned(), Value::from(LONG_AGO));
    });

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Missing).await;

    assert_eq!(
        session.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Lost)
    );
    assert!(session.ended_at.is_some(), "the session ended a moment ago");
    let candidate = selection
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == id)
        .expect("the long-unreachable session is selected for removal");
    assert_eq!(candidate.reason, SessionRetentionReason::Lost);
    assert!(candidate.age_secs > 90 * 24 * 3600);
}

#[tokio::test]
async fn a_lost_session_reached_a_moment_ago_is_kept_whatever_its_end_time() {
    let socket = temp_socket("ended-lost-fresh-connection");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, false).await;
    rewrite_session_lines(&store_path, |line| {
        info_of(line).insert("ended_at".to_owned(), Value::from(LONG_AGO));
    });

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Missing).await;

    assert_eq!(
        session.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Lost)
    );
    assert!(
        selection
            .sessions
            .iter()
            .all(|candidate| candidate.session_id != id),
        "the lost window counts from the last connection: {:?}",
        selection.sessions
    );
}

#[tokio::test]
async fn a_lost_session_that_never_connected_ages_from_its_end_time() {
    let socket = temp_socket("ended-lost-never-connected");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = registry_config(&socket);
    let id = finished_session_and_stopped_daemon(&socket, &config, false).await;
    rewrite_session_lines(&store_path, |line| {
        let info = info_of(line);
        info.insert("ended_at".to_owned(), Value::from(LONG_AGO));
        let runtime = info
            .get_mut("runtime")
            .and_then(Value::as_object_mut)
            .expect("the session has a runtime");
        runtime.remove("last_connected_at");
    });

    let (session, selection) = restart_and_select(&socket, config(), &id, Journal::Missing).await;

    assert_eq!(
        session.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Lost)
    );
    assert_eq!(
        session
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.last_connected_at.as_deref()),
        None
    );
    let candidate = selection
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == id)
        .expect("the lost session is selected from its end time");
    assert_eq!(candidate.reason, SessionRetentionReason::Lost);
    assert!(candidate.age_secs > 90 * 24 * 3600);
}
