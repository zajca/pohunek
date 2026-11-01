//! A store rewrite keeps every line no loader accepts.
//!
//! One scenario runs the real daemon over a store that already holds such lines
//! and drives write RPCs; the others use the public daemon store boundary over a
//! schema-10 fixture migrated to the current schema. The lines a build cannot
//! load are a current-schema session record carrying a desired state this build
//! does not know, and a line that is not JSON.

// Rust guideline compliant 2026-10-26

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pohunek_daemon::store::{ProjectRecord, SchemaMigration, Store, STORE_SCHEMA_VERSION};
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::wait_until;
use pohunek_test_support::{bin_exe, worker_binary};
use protocol::SessionNewParams;
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

/// A line that is not JSON.
const CORRUPT_LINE: &str = "{not json at all";

fn fixture_store(root: &std::path::Path) -> (Store, std::path::PathBuf) {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    let store = Store::new(path.clone());
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    (store, path)
}

/// A current-schema session line whose desired state a newer daemon wrote.
fn newer_session_line(path: &std::path::Path) -> String {
    let stored = std::fs::read_to_string(path).expect("read current store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["session_id"] = "s-from-a-newer-daemon".into();
    line["desired_state"] = "suspended".into();
    serde_json::to_string(&line).expect("serialize rejected line")
}

fn store_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("read store")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn project() -> ProjectRecord {
    serde_json::from_value(serde_json::json!({
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z"
    }))
    .expect("project record")
}

#[test]
fn every_rewrite_keeps_the_lines_no_loader_accepts() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let rejected = [newer_session_line(&path), CORRUPT_LINE.to_owned()];
    let mut content = std::fs::read_to_string(&path).expect("read current store");
    for line in &rejected {
        content.push_str(line);
        content.push('\n');
    }
    std::fs::write(&path, content).expect("append the rejected lines");
    let session = store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session");
    assert_eq!(session.session_id, FIXTURE_SESSION);

    store.record_session(&session).expect("rewrite the session");
    let lines = store_lines(&path);
    assert_eq!(
        lines[lines.len() - 2..],
        rejected,
        "the rejected lines follow the parsed records unchanged and in order"
    );
    assert_eq!(lines.len(), 3, "one parsed session and both rejected lines");

    store
        .mutate_project(std::path::Path::new("/repo/.git"), |_| Some(project()))
        .expect("write a project");
    store
        .remove_session(FIXTURE_SESSION)
        .expect("remove the session");
    let lines = store_lines(&path);
    assert_eq!(lines[lines.len() - 2..], rejected);
    assert_eq!(lines.len(), 3, "one project and both rejected lines");
    assert!(store.load_sessions().expect("load sessions").is_empty());
}

/// A current-schema session line whose `capabilities` carry a key this build
/// does not know.
fn unknown_capability_key_line(path: &std::path::Path) -> String {
    let stored = std::fs::read_to_string(path).expect("read current store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["session_id"] = "s-with-a-capability-key-from-a-newer-daemon".into();
    line["info"]["id"] = line["session_id"].clone();
    line["info"]["capabilities"]["a_key_from_a_newer_daemon"] = true.into();
    serde_json::to_string(&line).expect("serialize rejected line")
}

#[test]
fn a_line_with_an_unknown_capability_key_is_rejected_by_the_loader_and_kept_verbatim() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let rejected = unknown_capability_key_line(&path);
    let mut content = std::fs::read_to_string(&path).expect("read current store");
    content.push_str(&rejected);
    content.push('\n');
    std::fs::write(&path, content).expect("append the rejected line");

    let sessions = store.load_sessions().expect("load sessions");
    assert_eq!(sessions.len(), 1, "only the fixture session loads");
    assert_eq!(sessions[0].session_id, FIXTURE_SESSION);

    store
        .record_session(&sessions[0])
        .expect("rewrite the session");
    let lines = store_lines(&path);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], rejected, "the rejected line is kept unchanged");
}

#[test]
fn a_store_without_rejected_lines_gains_none() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let session = store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session");

    store.record_session(&session).expect("rewrite the session");
    assert_eq!(store_lines(&path).len(), 1);
}

/// Directory name the daemon uses below `XDG_DATA_HOME`.
const DATA_APP_DIR: &str = "pohunek";

/// The read limit of the durable store, as `StoreBodyTooLarge` reports it.
const STORE_READ_LIMIT: usize = 16 * 1024 * 1024;

/// Room left below [`STORE_READ_LIMIT`] by the store of the size scenario.
const SIZE_SCENARIO_HEADROOM: usize = 4 * 1024;

/// Length of the session name that outgrows [`SIZE_SCENARIO_HEADROOM`].
const SIZE_SCENARIO_NAME_LEN: usize = 8 * 1024;

/// A current-schema session line that names a desired state this build rejects.
fn rejected_session_line() -> String {
    format!(
        "{{\"kind\":\"session\",\"schema_version\":{STORE_SCHEMA_VERSION},\"session_id\":\"s-from-a-newer-daemon\",\"desired_state\":\"suspended\"}}"
    )
}

struct Layout {
    runtime: PathBuf,
    state_home: PathBuf,
    data_home: PathBuf,
    store: PathBuf,
}

impl Layout {
    fn new(env: &TestEnv, store_content: &str) -> Self {
        let data_home = env.root().join("data");
        let data_dir = data_home.join(DATA_APP_DIR);
        std::fs::create_dir_all(&data_dir).expect("create data dir");
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .expect("make data dir private");
        let store = data_dir.join("metadata.jsonl");
        std::fs::write(&store, store_content).expect("write store");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600))
            .expect("make store private");
        Self {
            runtime: env.root().join("r"),
            state_home: env.root().join("s"),
            data_home,
            store,
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("pohunek/daemon.sock")
    }
}

fn daemon_command(env: &TestEnv, layout: &Layout) -> tokio::process::Command {
    let mut command = env.tokio_command(bin_exe("pohunekd"));
    command
        .env("XDG_RUNTIME_DIR", &layout.runtime)
        .env("XDG_STATE_HOME", &layout.state_home)
        .env("XDG_DATA_HOME", &layout.data_home)
        .env("SHELL", "/bin/sh")
        .env("POHUNEK_WORKER_LAUNCHER", "subprocess")
        .env("POHUNEK_WORKER_BIN", worker_binary())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn wait_until_ready(child: &mut tokio::process::Child, socket: &Path) {
    let connected = wait_until("the daemon control socket accepts connections", || async {
        UnixStream::connect(socket).await.ok()
    });
    tokio::select! {
        _stream = connected => {}
        status = child.wait() => panic!("daemon exited before readiness: {status:?}"),
    }
}

#[tokio::test]
async fn a_daemon_started_over_rejected_lines_keeps_them_through_write_rpcs() {
    let env = TestEnv::new().expect("create short test environment");
    let rejected = [rejected_session_line(), CORRUPT_LINE.to_owned()];
    let layout = Layout::new(&env, &format!("{}\n{}\n", rejected[0], rejected[1]));

    let mut daemon = daemon_command(&env, &layout)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;
    let mut client = pohunek_client::Client::connect_local(&layout.socket())
        .await
        .expect("connect to the daemon");

    let created = client
        .session_new(SessionNewParams {
            name: None,
            agent: "shell".to_owned(),
            cwd: Some(env.root().to_path_buf()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            extended_input_ready_wait: None,
            branch_source: None,
            on_diverged: None,
            input_ack_timeout_ms: None,
            metadata: std::collections::BTreeMap::new(),
            secret_env: protocol::SessionSecretEnv::default(),
        })
        .await
        .expect("create a session over a store holding rejected lines");
    let lines = store_lines(&layout.store);
    assert_eq!(lines.len(), 3, "the new session and both rejected lines");
    assert!(lines[0].contains(&created.session.id.0));
    assert_eq!(
        lines[1..],
        rejected,
        "the rejected lines follow the records"
    );

    client
        .session_remove(created.session.id.clone())
        .await
        .expect("remove the session");
    assert_eq!(store_lines(&layout.store), rejected);

    daemon.kill().await.expect("stop daemon");
    let mut stderr = String::new();
    daemon
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .await
        .expect("read daemon stderr");
    let _ = daemon.wait().await;
    assert!(
        stderr.contains("the line stays in the store unchanged"),
        "the rejected lines are surfaced with the remedy: {stderr}"
    );
}

#[test]
fn a_rewrite_that_outgrows_the_read_limit_because_of_kept_lines_is_refused() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let session = store
        .load_sessions()
        .expect("load sessions")
        .pop()
        .expect("the fixture session");
    let stored = std::fs::read_to_string(&path).expect("read current store");
    let filler = STORE_READ_LIMIT - stored.len() - SIZE_SCENARIO_HEADROOM;
    let mut content = stored;
    content.push_str(&"x".repeat(filler));
    content.push('\n');
    std::fs::write(&path, &content).expect("append the large rejected line");
    let before = std::fs::read(&path).expect("read store");

    let mut grown = session.clone();
    grown.info.name = Some("n".repeat(SIZE_SCENARIO_NAME_LEN));
    let error = store
        .record_session(&grown)
        .expect_err("the kept line leaves no room for the larger record");
    assert!(
        error.to_string().contains("16777216-byte"),
        "the refusal names the read limit: {error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read store"),
        before,
        "a refused write leaves the store unchanged"
    );
}

#[test]
fn only_a_line_that_may_pin_a_package_makes_the_pinned_set_incomplete() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    assert_eq!(
        store
            .pinned_package_digests()
            .expect("scan")
            .unreadable_lines,
        0
    );
    let mut content = std::fs::read_to_string(&path).expect("read current store");
    let carries_no_pin = |kind: &str| {
        format!(
            "{{\"kind\":\"{kind}\",\"schema_version\":{STORE_SCHEMA_VERSION},\"unexpected\":1}}"
        )
    };
    for kind in ["worktree", "project"] {
        content.push_str(&carries_no_pin(kind));
        content.push('\n');
    }
    std::fs::write(&path, &content).expect("append unreadable worktree and project lines");
    assert_eq!(
        store
            .pinned_package_digests()
            .expect("scan")
            .unreadable_lines,
        0,
        "worktree and project records carry no pin"
    );

    for may_pin in [
        rejected_session_line(),
        carries_no_pin("resume"),
        carries_no_pin("kind_of_a_newer_daemon"),
        CORRUPT_LINE.to_owned(),
    ] {
        content.push_str(&may_pin);
        content.push('\n');
    }
    std::fs::write(&path, &content).expect("append the lines that may pin");
    assert_eq!(
        store
            .pinned_package_digests()
            .expect("scan")
            .unreadable_lines,
        4,
        "a session, a resume, an unknown kind and a corrupt line may each pin"
    );
}

/// Quarantine file the daemon writes next to the store.
const QUARANTINE_NAME: &str = "metadata.rejected.jsonl";

#[test]
fn setting_a_line_aside_keeps_every_other_record_and_a_failed_quarantine_keeps_the_line() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    let rejected = [newer_session_line(&path), CORRUPT_LINE.to_owned()];
    let mut content = std::fs::read_to_string(&path).expect("read current store");
    for line in &rejected {
        content.push_str(line);
        content.push('\n');
    }
    std::fs::write(&path, &content).expect("append the rejected lines");
    let hash = |text: &str| {
        use sha2::{Digest as _, Sha256};
        format!("{:x}", Sha256::digest(text.as_bytes()))
    };

    // A quarantine path that cannot be written leaves the store as it was.
    let quarantine = root.path().join(QUARANTINE_NAME);
    std::fs::create_dir(&quarantine).expect("block the quarantine path");
    let failed = store
        .set_aside_rejected(&hash(&rejected[1]), true)
        .expect_err("the quarantine file cannot be written");
    assert!(matches!(
        failed,
        pohunek_daemon::store::SetAsideError::Failed(_)
    ));
    assert_eq!(
        std::fs::read_to_string(&path).expect("read store"),
        content,
        "a failed quarantine leaves the store untouched"
    );
    std::fs::remove_dir(&quarantine).expect("unblock the quarantine path");

    // The parsed record and the other rejected line survive in order.
    let moved = store
        .set_aside_rejected(&hash(&rejected[1]), true)
        .expect("set the corrupt line aside");
    assert_eq!(moved.lines, [3]);
    let lines = store_lines(&path);
    assert_eq!(lines.len(), 2, "the session and the other rejected line");
    assert_eq!(lines[1], rejected[0]);
    assert_eq!(
        store.load_sessions().expect("load sessions")[0].session_id,
        FIXTURE_SESSION
    );
    let quarantined = std::fs::read_to_string(&quarantine).expect("read the quarantine file");
    let record: serde_json::Value =
        serde_json::from_str(quarantined.trim_end()).expect("one audit record");
    assert_eq!(record["text"], CORRUPT_LINE);
    assert_eq!(record["line"], 3);
    assert_eq!(record["content_hash"], hash(CORRUPT_LINE));
    assert_eq!(record["accepted_pin_loss"], true);
    assert_eq!(
        std::fs::metadata(&quarantine)
            .expect("quarantine metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn the_daemon_doctor_reports_rejected_lines_and_names_the_command() {
    let env = TestEnv::new().expect("create short test environment");
    let layout = Layout::new(&env, &format!("{}\n", rejected_session_line()));
    let mut daemon = daemon_command(&env, &layout)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    wait_until_ready(&mut daemon, &layout.socket()).await;
    let mut client = pohunek_client::Client::connect_local(&layout.socket())
        .await
        .expect("connect to the daemon");
    let check = |report: protocol::DaemonDoctorResult| {
        report
            .report
            .checks
            .into_iter()
            .find(|check| check.name == "metadata_store_rejected_lines")
            .expect("the daemon reports the rejected-lines check")
    };

    let warned = check(
        client
            .call::<protocol::method::DaemonDoctor>(())
            .await
            .expect("doctor"),
    );
    assert_eq!(warned.status, protocol::DoctorStatus::Warn);
    assert!(
        warned.detail.contains("pohunek store rejected list"),
        "the check names the command: {}",
        warned.detail
    );
    assert!(
        !warned.detail.contains("s-from-a-newer-daemon"),
        "the check carries counts, never a line"
    );

    let listed = client
        .call::<protocol::method::StoreRejectedList>(())
        .await
        .expect("list");
    assert_eq!(listed.total, 1);
    client
        .call::<protocol::method::StoreRejectedSetAside>(protocol::StoreRejectedSetAsideParams {
            content_hash: listed.lines[0].content_hash.clone(),
            accept_pin_loss: true,
        })
        .await
        .expect("set the line aside");
    let settled = check(
        client
            .call::<protocol::method::DaemonDoctor>(())
            .await
            .expect("doctor"),
    );
    assert_eq!(settled.status, protocol::DoctorStatus::Ok);
    assert!(
        layout.store.with_file_name(QUARANTINE_NAME).exists(),
        "the line is in the quarantine file"
    );

    daemon.kill().await.expect("stop daemon");
    let _ = daemon.wait().await;
}

#[test]
fn a_line_that_decodes_but_names_a_future_agent_kind_is_listed_without_making_the_pins_unreadable()
{
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let (store, path) = fixture_store(root.path());
    // The fixture's session record, naming an agent kind that is not a runtime id.
    let stored = std::fs::read_to_string(&path).expect("read current store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["session_id"] = "s-future".into();
    line["info"]["agent_base"] = "Future Agent".into();
    let future = serde_json::to_string(&line).expect("serialize the future-kind line");
    let mut content = stored;
    content.push_str(&future);
    content.push('\n');
    std::fs::write(&path, content).expect("append the future-kind line");

    let listed = store.rejected_lines().expect("list rejected lines");
    assert_eq!(listed.len(), 1, "the record is not loaded");
    assert_eq!(listed[0].kind.as_deref(), Some("session"));
    assert!(
        !listed[0].may_pin,
        "the line decodes, so what it pins is read like a loaded record's"
    );
    assert_eq!(
        store
            .pinned_package_digests()
            .expect("scan")
            .unreadable_lines,
        0,
        "the daemon's gate counts the same lines as the list"
    );
}
