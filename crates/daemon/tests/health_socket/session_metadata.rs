//! `session.set_metadata` through the public control socket: an edit is part of
//! the durable session record, survives a daemon restart, and a refused write
//! is reported instead of leaving the daemon and its record disagreeing.

// Rust guideline compliant 2026-10-22

use super::*;

/// The key the scenarios edit.
const EDITED_KEY: &str = "work.issue";

/// A shell that stays alive for the whole scenario.
const IDLE_SHELL: &str = "exec sleep 30";

fn set_metadata_request(id: &SessionId, metadata: &Value) -> ProtocolRequest {
    Request::make(
        "session-set-metadata",
        method::SESSION_SET_METADATA,
        serde_json::json!({ "session_id": id, "metadata": metadata }),
    )
}

/// The metadata of `id` in the durable record of the store at `path`.
fn stored_metadata(path: &Path, id: &SessionId) -> BTreeMap<String, String> {
    Store::new(path.to_path_buf())
        .load_sessions()
        .expect("read the durable records")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session has a durable record")
        .info
        .metadata
}

#[tokio::test]
async fn metadata_set_on_a_session_without_a_native_id_survives_a_restart() {
    let socket = temp_socket("meta-restart");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config()).await;
    let mut control = connect(&socket).await;
    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(
            &mut control,
            session_params_in(socket.dir.join(WORK_DIR_NAME)),
        )
        .await,
    ))
    .expect("session info");
    ok_payload(
        exchange(
            &mut control,
            &set_metadata_request(
                &session.id,
                &serde_json::json!({ EDITED_KEY: "1469", "work.scratch": "x" }),
            ),
        )
        .await,
    );
    ok_payload(
        exchange(
            &mut control,
            &set_metadata_request(&session.id, &serde_json::json!({ "work.scratch": null })),
        )
        .await,
    );
    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    let (restarted, _worker_home) = worker_backed_registry(&socket, config());
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciles the session");

    let adopted = restarted
        .inspect(&session.id)
        .await
        .expect("the restarted daemon lists the session");
    assert_eq!(
        adopted.metadata.get(EDITED_KEY).map(String::as_str),
        Some("1469"),
        "the edit survives the restart"
    );
    assert!(
        !adopted.metadata.contains_key("work.scratch"),
        "a removed key stays removed: {:?}",
        adopted.metadata
    );
}

#[tokio::test]
async fn a_metadata_write_the_store_refuses_is_reported_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let socket = temp_socket("meta-refused");
    let state = temp_dir("meta-refused-state");
    let store_path = state.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(state.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let session: SessionInfo = serde_json::from_value(ok_payload(
        create_session_with_params(
            &mut control,
            session_params_in(socket.dir.join(WORK_DIR_NAME)),
        )
        .await,
    ))
    .expect("session info");

    std::fs::set_permissions(&*state, std::fs::Permissions::from_mode(0o500))
        .expect("make the store directory read-only");
    let refused = err_payload(
        exchange(
            &mut control,
            &set_metadata_request(&session.id, &serde_json::json!({ EDITED_KEY: "1469" })),
        )
        .await,
    );
    std::fs::set_permissions(&*state, std::fs::Permissions::from_mode(0o700))
        .expect("let the fixture clean up");

    assert_eq!(refused.code, "session_store_failed", "{refused:?}");
    let unchanged = inspect_session(&mut control, &session.id).await;
    assert!(
        !unchanged.metadata.contains_key(EDITED_KEY),
        "a refused write is not applied in memory: {:?}",
        unchanged.metadata
    );
    assert!(!stored_metadata(&store_path, &session.id).contains_key(EDITED_KEY));

    ok_payload(
        exchange(
            &mut control,
            &set_metadata_request(&session.id, &serde_json::json!({ EDITED_KEY: "1469" })),
        )
        .await,
    );
    assert_eq!(
        stored_metadata(&store_path, &session.id)
            .get(EDITED_KEY)
            .map(String::as_str),
        Some("1469"),
        "the retried edit is written"
    );
    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// The durable record of a session that has captured a native id carries the
/// edit as well, so a restart reads it back.
pub(super) fn assert_native_session_edit_is_durable(store_path: &Path, id: &SessionId) {
    assert_eq!(
        stored_metadata(store_path, id)
            .get(EDITED_KEY)
            .map(String::as_str),
        Some("1469"),
        "the durable record of a native-bound session holds the edit"
    );
}

/// Sets the edited key on `id` through `control`.
pub(super) async fn set_edited_key(control: &mut Framed<UnixStream, LinesCodec>, id: &SessionId) {
    ok_payload(
        exchange(
            control,
            &set_metadata_request(id, &serde_json::json!({ EDITED_KEY: "1469" })),
        )
        .await,
    );
}
