//! The spawning session of a session through the public control socket: the
//! daemon records the origin pair of a `session.new` request, reports it in
//! every session view, keeps a foreign origin as received, answers the
//! `spawned_by` list filter with exactly the children, and keeps the record
//! across a daemon restart.

// Rust guideline compliant 2026-11-01

use super::*;

/// A shell that stays alive for the whole scenario.
const IDLE_SHELL: &str = "exec sleep 30";

/// Daemon id the scenario gives to origins of this daemon.
const LOCAL_DAEMON: &str = "d-local";

/// Daemon id of an origin recorded by another host.
const FOREIGN_DAEMON: &str = "d-foreign-host";

fn spawned_by(session_id: &str, daemon_id: &str) -> protocol::SpawnedBy {
    protocol::SpawnedBy {
        session_id: SessionId(session_id.to_owned()),
        daemon_id: daemon_id.to_owned(),
    }
}

/// Creates a shell session in `cwd`; `origin` is the request origin pair.
async fn create_from(
    control: &mut Framed<UnixStream, LinesCodec>,
    cwd: PathBuf,
    origin: Option<&protocol::SpawnedBy>,
    metadata: BTreeMap<String, String>,
) -> SessionInfo {
    let mut params = session_params_in(cwd);
    params.metadata = metadata;
    let mut request = Request::make(
        "session-new-from-origin",
        method::SESSION_NEW,
        serde_json::to_value(params).expect("serialize params"),
    );
    if let Some(origin) = origin {
        request = request
            .with_origin(
                Some(origin.session_id.clone()),
                Some(origin.daemon_id.clone()),
            )
            .expect("valid origin pair");
    }
    serde_json::from_value(ok_payload(exchange(control, &request).await)).expect("session info")
}

/// Creates a session whose params carry a `spawned_by` key and no origin.
async fn create_with_forged_param(
    control: &mut Framed<UnixStream, LinesCodec>,
    cwd: PathBuf,
) -> SessionInfo {
    let mut params = serde_json::to_value(session_params_in(cwd)).expect("serialize params");
    params["spawned_by"] = serde_json::json!({ "session_id": "s-forged", "daemon_id": "d-forged" });
    let request = Request::make("session-new-forged", method::SESSION_NEW, params);
    serde_json::from_value(ok_payload(exchange(control, &request).await)).expect("session info")
}

/// The ids `session.list` returns for `filters`, sorted.
async fn listed_ids(
    control: &mut Framed<UnixStream, LinesCodec>,
    filters: Vec<SessionListFilter>,
) -> Vec<String> {
    let request = Request::make(
        "session-list-filtered",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams { filters }).expect("serialize params"),
    );
    let sessions: Vec<SessionInfo> =
        serde_json::from_value(ok_payload(exchange(control, &request).await))
            .expect("session list");
    let mut ids: Vec<String> = sessions.into_iter().map(|session| session.id.0).collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario follows the recorded spawner from creation through edits to a restart"
)]
async fn the_spawner_is_recorded_from_the_request_origin_and_survives_a_restart() {
    let socket = temp_socket("spawned-by-restart");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config()).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join(WORK_DIR_NAME);

    let parent = create_from(&mut control, cwd.clone(), None, BTreeMap::new()).await;
    let local_origin = spawned_by(&parent.id.0, LOCAL_DAEMON);
    let foreign_origin = spawned_by(&parent.id.0, FOREIGN_DAEMON);
    let gone_origin = spawned_by("s-gone", LOCAL_DAEMON);
    let worker_role = BTreeMap::from([("role".to_owned(), "worker".to_owned())]);
    let local_child = create_from(
        &mut control,
        cwd.clone(),
        Some(&local_origin),
        worker_role.clone(),
    )
    .await;
    let foreign_child = create_from(
        &mut control,
        cwd.clone(),
        Some(&foreign_origin),
        BTreeMap::new(),
    )
    .await;
    let orphan = create_from(
        &mut control,
        cwd.clone(),
        Some(&gone_origin),
        BTreeMap::new(),
    )
    .await;
    let forged = create_with_forged_param(&mut control, cwd).await;
    assert_eq!(forged.spawned_by, None, "params cannot set the spawner");

    assert_eq!(
        parent.spawned_by, None,
        "a request without an origin records none"
    );
    assert_eq!(local_child.spawned_by.as_ref(), Some(&local_origin));
    assert_eq!(
        foreign_child.spawned_by.as_ref(),
        Some(&foreign_origin),
        "a foreign origin is stored as received"
    );
    assert_eq!(
        orphan.spawned_by.as_ref(),
        Some(&gone_origin),
        "an origin naming no local session is a reference, not a lookup"
    );
    let inspected = inspect_session(&mut control, &local_child.id).await;
    assert_eq!(inspected.spawned_by.as_ref(), Some(&local_origin));

    let children = sorted(vec![local_child.id.0.clone(), foreign_child.id.0.clone()]);
    let by_parent = SessionListFilter::SpawnedBy(parent.id.0.clone());
    assert_eq!(
        listed_ids(&mut control, vec![by_parent.clone()]).await,
        children,
        "the filter returns exactly the children"
    );
    assert_eq!(
        listed_ids(
            &mut control,
            vec![
                by_parent.clone(),
                SessionListFilter::MetadataEq {
                    key: "role".to_owned(),
                    value: "worker".to_owned(),
                },
            ],
        )
        .await,
        vec![local_child.id.0.clone()],
        "the filter combines with the metadata predicates"
    );
    assert_eq!(
        listed_ids(
            &mut control,
            vec![SessionListFilter::SpawnedBy("s-gone".to_owned())],
        )
        .await,
        vec![orphan.id.0.clone()]
    );
    assert!(
        listed_ids(
            &mut control,
            vec![SessionListFilter::SpawnedBy("s-unrelated".to_owned())],
        )
        .await
        .is_empty(),
        "a session that spawned nothing has no children"
    );

    ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-edit-spawned",
                method::SESSION_SET_METADATA,
                serde_json::json!({
                    "session_id": local_child.id,
                    "metadata": { "role": null, "work.issue": "1300" },
                }),
            ),
        )
        .await,
    );
    ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-rename-spawned",
                method::SESSION_RENAME,
                serde_json::json!({ "session_id": local_child.id, "name": "renamed" }),
            ),
        )
        .await,
    );
    ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-resize-spawned",
                method::SESSION_RESIZE,
                serde_json::json!({ "session_id": local_child.id, "cols": 100, "rows": 30 }),
            ),
        )
        .await,
    );
    let edited = inspect_session(&mut control, &local_child.id).await;
    assert_eq!(
        edited.spawned_by.as_ref(),
        Some(&local_origin),
        "the record is immutable"
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    let (restarted, _worker_home) = worker_backed_registry(&socket, config());
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciles the sessions");
    let adopted = |id: &SessionId| {
        let restarted = restarted.clone();
        let id = id.clone();
        async move { restarted.inspect(&id).await.expect("the session is listed") }
    };
    assert_eq!(adopted(&parent.id).await.spawned_by, None);
    assert_eq!(
        adopted(&local_child.id).await.spawned_by.as_ref(),
        Some(&local_origin),
        "the spawner survives a daemon restart"
    );
    assert_eq!(
        adopted(&foreign_child.id).await.spawned_by.as_ref(),
        Some(&foreign_origin)
    );
}

#[tokio::test]
async fn an_unknown_list_filter_is_refused_instead_of_ignored() {
    let socket = temp_socket("spawned-by-refusal");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;

    for (case, filter) in [
        (
            "a filter variant this daemon does not know",
            serde_json::json!({ "key": "spawned_by_future", "value": "s-parent" }),
        ),
        (
            "a spawner filter that does not name a session id",
            serde_json::json!({ "key": "spawned_by", "value": { "session_id": "s-parent" } }),
        ),
    ] {
        let refused = err_payload(
            exchange(
                &mut control,
                &Request::make(
                    "session-list-refused",
                    method::SESSION_LIST,
                    serde_json::json!({ "filters": [filter] }),
                ),
            )
            .await,
        );
        assert_eq!(refused.code, "bad_request", "{case}: {refused:?}");
    }

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}
