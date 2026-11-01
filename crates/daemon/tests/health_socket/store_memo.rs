//! The metadata store behind the public control socket: an edit made outside
//! the daemon is read by the next request, and an API write produces the bytes
//! of the daemon built before the store kept a snapshot.

// Rust guideline compliant 2026-10-31

use super::*;
use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;

use pohunek_daemon::store::STORE_SCHEMA_VERSION;

const SCHEMA_10: &str = include_str!("../../src/store/fixtures/schema-10/metadata.jsonl");

/// The stores the daemon built before the snapshot (main `4c7d2e9b8`) wrote after
/// each request of [`api_golden_steps`], with only the schema 13 changes applied
/// (`schema_version` 13 and `refuse_while_blocked` in a rewritten session's
/// `input_rules`).
const GOLDENS: [&str; 3] = [
    include_str!("../fixtures/store-memo/api-after-rename-1.jsonl"),
    include_str!("../fixtures/store-memo/api-after-rename-2.jsonl"),
    include_str!("../fixtures/store-memo/api-after-remove.jsonl"),
];

/// Stands in for the time reconciliation stamped on the stored session.
const RECONCILED_AT_TOKEN: &str = "<reconciled_at>";

/// Session recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

/// A line that is not JSON.
const CORRUPT_LINE: &str = "{not json at all";

/// A shell that stays alive for the whole scenario.
const IDLE_SHELL: &str = "exec sleep 30";

fn config(socket: &TestSocket, store_path: &Path) -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.to_path_buf()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    }
}

fn write_private(path: &Path, content: &str) {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create owner-private store")
        .write_all(content.as_bytes())
        .expect("write the store");
}

async fn list_projects(control: &mut Framed<UnixStream, LinesCodec>) -> Vec<ProjectInfo> {
    let request = Request::make(
        "memo-project-list",
        method::PROJECT_LIST,
        serde_json::json!({}),
    );
    serde_json::from_value(ok_payload(exchange(control, &request).await)).expect("project list")
}

async fn rename_project(control: &mut Framed<UnixStream, LinesCodec>, reference: &str, name: &str) {
    let request = Request::make(
        "memo-project-rename",
        method::PROJECT_RENAME,
        serde_json::json!({ "reference": reference, "name": name }),
    );
    ok_payload(exchange(control, &request).await);
}

#[tokio::test]
async fn an_edit_made_outside_the_daemon_is_listed_and_kept_by_the_next_write() {
    let socket = temp_socket("memo-edit");
    let store_path = socket.dir.join("metadata.jsonl");
    let repo = init_git_repo();
    let (shutdown, handle, _) =
        spawn_server_with_config(&socket, "0.0.0", config(&socket, &store_path)).await;
    let mut control = connect(&socket).await;
    let add = Request::make(
        "memo-project-add",
        method::PROJECT_ADD,
        serde_json::json!({ "path": repo.cwd() }),
    );
    let project: ProjectInfo =
        serde_json::from_value(ok_payload(exchange(&mut control, &add).await)).expect("project");
    rename_project(&mut control, &project.id, "zzlabel-AAAA").await;
    assert_eq!(list_projects(&mut control).await[0].label, "zzlabel-AAAA");

    // A mutation that matches no record writes nothing.
    let before = std::fs::read_to_string(&store_path).expect("read the store");
    let unknown = Request::make(
        "memo-project-rename-unknown",
        method::PROJECT_RENAME,
        serde_json::json!({ "reference": "no-such-project", "name": "zzlabel-CCCC" }),
    );
    exchange(&mut control, &unknown)
        .await
        .into_result()
        .expect_err("an unknown project is refused");
    assert_eq!(
        std::fs::read_to_string(&store_path).expect("read the store"),
        before,
        "a refused mutation leaves the file as it was"
    );

    // An outside writer rewrites the bytes in place and restores the mtime.
    let original = std::fs::read_to_string(&store_path).expect("read the store");
    let modified = std::fs::metadata(&store_path)
        .expect("store metadata")
        .modified()
        .expect("store mtime");
    let edited = original.replace("zzlabel-AAAA", "zzlabel-BBBB");
    assert_eq!(edited.len(), original.len());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&store_path)
        .expect("open the store in place");
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.write_all(edited.as_bytes()).expect("edit in place");
    file.set_modified(modified).expect("restore the mtime");
    drop(file);

    assert_eq!(
        list_projects(&mut control).await[0].label,
        "zzlabel-BBBB",
        "the edit is listed"
    );
    // A session write goes through the same store and keeps the edit.
    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(
            &mut control,
            session_params_in(socket.dir.join(WORK_DIR_NAME)),
        )
        .await,
    ))
    .expect("session info");
    let set = Request::make(
        "memo-set-metadata",
        method::SESSION_SET_METADATA,
        serde_json::json!({ "session_id": session.id, "metadata": { "work.issue": "1469" } }),
    );
    ok_payload(exchange(&mut control, &set).await);
    assert_eq!(
        list_projects(&mut control).await[0].label,
        "zzlabel-BBBB",
        "the write keeps the edit"
    );
    assert!(std::fs::read_to_string(&store_path)
        .expect("read the store")
        .contains("zzlabel-BBBB"));

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The migrated schema-10 fixture with an unknown field on the accepted line, a
/// project line, a line no loader accepts and a line that is not JSON.
fn golden_input(path: &Path) {
    let scratch = temp_dir("memo-golden-migrate");
    let migrated = scratch.join("metadata.jsonl");
    write_private(&migrated, SCHEMA_10);
    Store::new(migrated.clone())
        .migrate_to_current()
        .expect("migrate previous schema");
    let stored = std::fs::read_to_string(&migrated).expect("read the migrated store");
    let mut accepted: Value = serde_json::from_str(stored.trim_end()).expect("one stored line");
    accepted["unknown_future_field"] = serde_json::json!({ "nested": [1, 2, 3] });
    let mut rejected = accepted.clone();
    rejected["session_id"] = "s-from-a-newer-daemon".into();
    rejected["desired_state"] = "suspended".into();
    let project_line = serde_json::json!({
        "kind": "project",
        "schema_version": STORE_SCHEMA_VERSION,
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "custom_name": "zzlabel-AAAA",
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z"
    });
    write_private(
        path,
        &format!("{project_line}\n{accepted}\n{rejected}\n{CORRUPT_LINE}\n"),
    );
}

/// Three API writes over [`golden_input`]: a rename of the project, a second
/// rename, then the removal of the stored session. Returns the store file
/// after each.
pub(super) async fn api_golden_steps() -> [String; 3] {
    let socket = temp_socket("memo-golden");
    let store_path = socket.dir.join("metadata.jsonl");
    golden_input(&store_path);
    let (registry, worker_home) = worker_backed_registry(&socket, config(&socket, &store_path));
    registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the stored session");
    let (shutdown, handle, _) = serve_registry(&socket, "0.0.0", None, registry, worker_home).await;
    let mut control = connect(&socket).await;
    // Reconciliation stamps the stored session with the time it ran.
    let reconciled_at = inspect_session(&mut control, &SessionId(FIXTURE_SESSION.to_owned()))
        .await
        .updated_at;
    let stored = || {
        std::fs::read_to_string(&store_path)
            .expect("read the store")
            .replace(&reconciled_at, RECONCILED_AT_TOKEN)
    };

    let projects = list_projects(&mut control).await;
    rename_project(&mut control, &projects[0].id, "zzlabel-BBBB").await;
    let after_first_rename = stored();
    rename_project(&mut control, &projects[0].id, "zzlabel-CCCC").await;
    let after_second_rename = stored();
    let remove = Request::make(
        "memo-golden-remove",
        method::SESSION_REMOVE,
        serde_json::json!(FIXTURE_SESSION),
    );
    ok_payload(exchange(&mut control, &remove).await);
    let after_remove = stored();

    let _ = shutdown.send(());
    let _ = handle.await;
    [after_first_rename, after_second_rename, after_remove]
}

#[tokio::test]
async fn api_writes_produce_the_bytes_of_the_daemon_before_the_snapshot() {
    let written = api_golden_steps().await;

    for (step, (actual, golden)) in written.iter().zip(GOLDENS).enumerate() {
        assert_eq!(actual, golden, "store after request {}", step + 1);
    }
}
