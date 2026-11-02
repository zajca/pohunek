//! Idempotent `session.new` through the public control socket: a repeated
//! `client_request_id` replays the session it created, another request under the
//! key conflicts, a create that has not settled is in progress, and the key dies
//! with the session. The scenarios drive real git worktrees, real hooks and real
//! daemon restarts.

// Rust guideline compliant 2026-11-01

use super::*;

use pohunek_service_config::RequiredHook;
use serde_json::json;

/// A shell that stays alive for the whole scenario.
const IDLE_SHELL: &str = "exec sleep 30";

/// Key of the scenarios that need only one.
const KEY: &str = "req-1";

/// First delay of the supervision retry that finishes a compensation; short so
/// the scenario that waits for it is quick.
const RETRY_INITIAL: Duration = Duration::from_millis(25);

/// Acknowledgement window of the scenario that waits in it. The scenario ends
/// the wait by stopping the session, so the longest permitted window is never
/// waited out.
const ACK_WINDOW_MS: u32 = 8_000;

/// Frozen schema-15 record of a Pi session created with [`FROZEN_KEY`].
const FROZEN_STORE: &str = include_str!("../../src/store/fixtures/schema-15/metadata.jsonl");

/// Key the frozen record carries.
const FROZEN_KEY: &str = "req-frozen-1";

/// `session.new` params for a plain shell in `cwd`, with `key` and the `extra`
/// members merged over them.
fn keyed_params(cwd: &Path, key: &str, extra: &Value) -> Value {
    let mut params = json!({
        "agent": "shell",
        "cwd": cwd,
        "cols": 80,
        "rows": 24,
        "client_request_id": key,
    });
    for (name, value) in extra.as_object().expect("extra members") {
        params[name] = value.clone();
    }
    params
}

/// Writes `contents` as the owner-private store at `path`, replacing an earlier one.
fn write_store(path: &Path, contents: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create the owner-private store")
        .write_all(contents.as_bytes())
        .expect("write the store");
}

/// `session.new` params for a worktree session on `branch`, with `key`.
fn keyed_worktree_params(repo: &Path, branch: &str, key: &str) -> Value {
    json!({
        "agent": "shell",
        "repo": repo,
        "branch": branch,
        "cols": 80,
        "rows": 24,
        "client_request_id": key,
    })
}

async fn send_new(control: &mut Framed<UnixStream, LinesCodec>, params: Value) -> Response {
    exchange(
        control,
        &Request::make("session-new-keyed", method::SESSION_NEW, params),
    )
    .await
}

fn created(response: Response) -> protocol::SessionNewResult {
    serde_json::from_value(ok_payload(response)).expect("session.new result")
}

/// Asserts that `error` is `code` naming `session_id` in its message and details.
fn assert_names_session(error: &protocol::ProtocolError, code: &str, session_id: &SessionId) {
    assert_eq!(error.code, code, "{error:?}");
    assert_eq!(error.class, ErrorClass::Runtime);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.session_id.as_ref()),
        Some(session_id),
        "{error:?}"
    );
    assert!(error.msg.contains(&session_id.0), "{}", error.msg);
    assert!(error.recover.is_some(), "{error:?}");
}

async fn listed(control: &mut Framed<UnixStream, LinesCodec>) -> Vec<SessionInfo> {
    let list = Request::make("session-list-keyed", method::SESSION_LIST, Value::Null);
    serde_json::from_value(ok_payload(exchange(control, &list).await)).expect("session list")
}

async fn call_on(
    control: &mut Framed<UnixStream, LinesCodec>,
    method: &str,
    id: &SessionId,
) -> Value {
    ok_payload(
        exchange(
            control,
            &Request::make(
                "session-call-keyed",
                method,
                serde_json::to_value(id).expect("serialize id"),
            ),
        )
        .await,
    )
}

#[tokio::test]
async fn a_repeated_key_replays_the_session_and_another_request_conflicts() {
    let socket = temp_socket("request-replay");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join(WORK_DIR_NAME);

    let first = created(send_new(&mut control, keyed_params(&cwd, KEY, &json!({}))).await);
    assert_eq!(first.replayed, None, "a create is not a replay");
    let id = first.session.id.clone();

    // Another terminal size and nothing else: the retry repeats the request.
    let resized = created(
        send_new(
            &mut control,
            keyed_params(&cwd, KEY, &json!({"cols": 120, "rows": 40})),
        )
        .await,
    );
    assert_eq!(resized.replayed, Some(true));
    assert_eq!(resized.session.id, id);
    assert_eq!(
        (resized.session.cols, resized.session.rows),
        (80, 24),
        "a replay reports the session as it is"
    );
    assert_eq!(resized.worktree_source, None);
    assert_eq!(resized.prompt_delivered, None);
    assert_eq!(listed(&mut control).await.len(), 1, "one session exists");

    // Another request under the key conflicts, whatever it changes, and the
    // check precedes agent resolution: an unknown agent conflicts too.
    for (what, extra) in [
        ("metadata", json!({"metadata": {"ticket": "ABC-1"}})),
        ("name", json!({"name": "other"})),
        ("input", json!({"input": "hello"})),
        ("agent", json!({"agent": "no-such-agent"})),
    ] {
        let error = err_payload(send_new(&mut control, keyed_params(&cwd, KEY, &extra)).await);
        assert_names_session(&error, "session_request_conflict", &id);
        assert_eq!(
            listed(&mut control).await.len(),
            1,
            "{what} created nothing"
        );
    }

    // A stopped session still holds its key.
    call_on(&mut control, method::SESSION_STOP, &id).await;
    let after_stop = created(send_new(&mut control, keyed_params(&cwd, KEY, &json!({}))).await);
    assert_eq!(after_stop.replayed, Some(true));
    assert_eq!(after_stop.session.id, id);
    assert_eq!(after_stop.session.state, SessionState::Stopped);

    // The key dies with the session.
    call_on(&mut control, method::SESSION_REMOVE, &id).await;
    let again = created(send_new(&mut control, keyed_params(&cwd, KEY, &json!({}))).await);
    assert_eq!(again.replayed, None);
    assert_ne!(again.session.id, id, "a removed session frees its key");
    assert_eq!(listed(&mut control).await.len(), 1);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn a_malformed_key_is_refused_before_anything_is_written() {
    let socket = temp_socket("request-malformed");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join(WORK_DIR_NAME);

    for key in ["", &"k".repeat(129), "with space", "a/b", "na\u{ef}ve"] {
        let error = err_payload(send_new(&mut control, keyed_params(&cwd, key, &json!({}))).await);
        assert_eq!(error.code, "bad_request", "{key:?}: {error:?}");
        assert!(
            error.msg.contains("client_request_id"),
            "{key:?}: {}",
            error.msg
        );
    }
    assert!(listed(&mut control).await.is_empty(), "nothing was created");
    assert!(
        std::fs::read_to_string(&store_path)
            .unwrap_or_default()
            .is_empty(),
        "nothing was written"
    );

    // No refused key left a claim behind: the boundary lengths are accepted.
    for key in ["k", &"k".repeat(128), "A.b_c:d-1"] {
        let result = created(send_new(&mut control, keyed_params(&cwd, key, &json!({}))).await);
        assert_eq!(result.replayed, None, "{key:?}");
    }
    assert_eq!(listed(&mut control).await.len(), 3);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn a_replay_injects_no_input_again() {
    let socket = temp_socket("request-input");
    let received = socket.dir.join("received.txt");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            ["-c", &format!("exec /bin/cat > '{}'", received.display())],
        ),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join(WORK_DIR_NAME);
    let with_input = || keyed_params(&cwd, KEY, &json!({"input": "first"}));

    let created_once = created(send_new(&mut control, with_input()).await);
    assert_eq!(created_once.applied_input, Some(true));
    assert_eq!(created_once.replayed, None);

    let replayed = created(send_new(&mut control, with_input()).await);
    assert_eq!(replayed.replayed, Some(true));
    assert_eq!(replayed.session.id, created_once.session.id);
    assert_eq!(
        replayed.applied_input,
        Some(true),
        "a session that kept its record delivered its input"
    );

    // Input is ordered on the PTY: a duplicate injected by the replay would
    // precede this line.
    ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-input-keyed",
                method::SESSION_INPUT,
                json!({"session_id": created_once.session.id, "text": "second"}),
            ),
        )
        .await,
    );
    wait_until("both lines to reach the agent", || async {
        let text = std::fs::read_to_string(&received).ok()?;
        (text.contains("second")).then_some(text)
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(&received)
            .expect("read the received input")
            .replace('\r', ""),
        "first\nsecond\n"
    );

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn the_key_survives_record_rewrites_and_a_daemon_restart() {
    let socket = temp_socket("request-restart");
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
    let params = || keyed_params(&cwd, KEY, &json!({}));

    let first = created(send_new(&mut control, params()).await);
    let id = first.session.id.clone();
    for (name, request) in [
        (
            "rename",
            Request::make(
                "keyed-rename",
                method::SESSION_RENAME,
                json!({ "session_id": id, "name": "renamed" }),
            ),
        ),
        (
            "metadata",
            Request::make(
                "keyed-metadata",
                method::SESSION_SET_METADATA,
                json!({ "session_id": id, "metadata": { "work.issue": "1340" } }),
            ),
        ),
        (
            "resize",
            Request::make(
                "keyed-resize",
                method::SESSION_RESIZE,
                json!({ "session_id": id, "cols": 100, "rows": 30 }),
            ),
        ),
    ] {
        ok_payload(exchange(&mut control, &request).await);
        let replay = created(send_new(&mut control, params()).await);
        assert_eq!(
            replay.replayed,
            Some(true),
            "the key survives the {name} rewrite"
        );
        let stored = std::fs::read_to_string(&store_path).expect("read the store");
        assert!(
            stored.contains("\"create_request\"") && stored.contains(KEY),
            "the {name} rewrite kept the key in the durable record: {stored}"
        );
    }
    call_on(&mut control, method::SESSION_STOP, &id).await;

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;

    let (restarted, _worker_home) = worker_backed_registry(&socket, config());
    restarted
        .reconcile_workers()
        .await
        .expect("startup reconciles the sessions");
    let outcome = restarted
        .create_outcome(serde_json::from_value(params()).expect("typed params"))
        .await
        .expect("a keyed retry after the restart");
    assert!(outcome.replayed, "the stopped session replays");
    assert_eq!(outcome.session.id, id);
    assert_eq!(outcome.session.state, SessionState::Stopped);
    assert!(outcome.worktree_source.is_none() && outcome.prompt_delivered.is_none());
}

#[tokio::test]
async fn a_frozen_record_replays_its_request_and_a_duplicated_key_conflicts() {
    let socket = temp_socket("request-frozen");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    // The request that created the frozen session; its fingerprint was
    // computed outside the daemon from the documented serialization.
    let frozen_request = json!({
        "agent": "pi", "name": "schema-14", "cwd": "/fixture",
        "cols": 80, "rows": 24, "client_request_id": FROZEN_KEY,
    });
    let frozen_id = SessionId(
        serde_json::from_str::<Value>(FROZEN_STORE).expect("frozen record")["session_id"]
            .as_str()
            .expect("session id")
            .to_owned(),
    );
    let typed = |params: &Value| -> SessionNewParams {
        serde_json::from_value(params.clone()).expect("typed params")
    };

    write_store(&store_path, FROZEN_STORE);
    let (registry, _worker_home) = worker_backed_registry(&socket, config());
    registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the frozen session");
    let replayed = registry
        .create_outcome(typed(&frozen_request))
        .await
        .expect("the recorded request replays");
    assert!(replayed.replayed);
    assert_eq!(replayed.session.id, frozen_id);
    // A different request under the key conflicts, which also proves the
    // stored fingerprint is the one the daemon computes today.
    for changed in [
        json!({"name": "other"}),
        json!({"metadata": {"k": "v"}}),
        json!({"input": "hello"}),
    ] {
        let mut request = frozen_request.clone();
        request
            .as_object_mut()
            .expect("object")
            .extend(changed.as_object().expect("object").clone());
        let error = registry
            .create_outcome(typed(&request))
            .await
            .expect_err("another request conflicts");
        assert_names_session(&error, "session_request_conflict", &frozen_id);
    }
    drop(registry);

    // A hand-edited store with two records under one key answers for the older.
    let mut twin: Value = serde_json::from_str(FROZEN_STORE).expect("frozen record");
    let twin_id = "s-01M4TRMBMMYEJYVJWER5365RPW";
    twin["session_id"] = json!(twin_id);
    twin["info"]["id"] = json!(twin_id);
    twin["recovery"]["session_id"] = json!(twin_id);
    twin["info"]["created_at"] = json!("2026-10-14T13:53:30.815890556Z");
    write_store(
        &store_path,
        &format!(
            "{}\n{}\n",
            FROZEN_STORE.trim_end(),
            serde_json::to_string(&twin).expect("serialize the twin")
        ),
    );
    let (registry, _worker_home) = worker_backed_registry(&socket, config());
    registry
        .reconcile_workers()
        .await
        .expect("startup reconciles both sessions");
    let error = registry
        .create_outcome(typed(&frozen_request))
        .await
        .expect_err("a duplicated key never replays");
    assert_names_session(&error, "session_request_conflict", &frozen_id);
}

#[tokio::test]
async fn a_create_left_unfinished_by_a_restart_is_compensated_and_frees_its_key() {
    let socket = temp_socket("request-abandoned");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = || SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    // The record a daemon killed while a create was preparing leaves behind.
    let mut abandoned: Value = serde_json::from_str(FROZEN_STORE).expect("frozen record");
    abandoned["transaction"] = json!({
        "id": "create-abandoned", "kind": "create", "phase": "preparing"
    });
    abandoned["runtime"] = json!({ "state": "starting" });
    abandoned["info"]["runtime"] = json!({
        "state": "starting", "runtime_generation": "1"
    });
    abandoned["info"]["state"] = json!("starting");
    write_store(&store_path, &format!("{abandoned}\n"));
    let (registry, _worker_home) = worker_backed_registry(&socket, config());
    registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the abandoned create");
    assert!(
        registry.list().await.is_empty(),
        "reconciliation compensated the create"
    );

    let params = keyed_params(&socket.dir.join(WORK_DIR_NAME), FROZEN_KEY, &json!({}));
    let outcome = registry
        .create_outcome(serde_json::from_value(params).expect("typed params"))
        .await
        .expect("the key is free again");
    assert!(!outcome.replayed, "the retry creates");
}

#[tokio::test]
async fn a_record_without_a_live_session_still_holds_its_key() {
    let socket = temp_socket("request-durable");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    write_store(&store_path, FROZEN_STORE);
    // No reconciliation: the store holds the record and the registry no session.
    let (registry, _worker_home) = worker_backed_registry(&socket, config);
    assert!(registry.list().await.is_empty());
    let frozen_id = SessionId(
        serde_json::from_str::<Value>(FROZEN_STORE).expect("frozen record")["session_id"]
            .as_str()
            .expect("session id")
            .to_owned(),
    );
    let params = |extra: Value| -> SessionNewParams {
        let mut request = json!({
            "agent": "pi", "name": "schema-14", "cwd": "/fixture",
            "cols": 80, "rows": 24, "client_request_id": FROZEN_KEY,
        });
        request
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        serde_json::from_value(request).expect("typed params")
    };

    let in_progress = registry
        .create_outcome(params(json!({})))
        .await
        .expect_err("a record that is not live has not settled");
    assert_names_session(&in_progress, "session_request_in_progress", &frozen_id);
    let conflict = registry
        .create_outcome(params(json!({"name": "other"})))
        .await
        .expect_err("another request conflicts");
    assert_names_session(&conflict, "session_request_conflict", &frozen_id);
    assert!(
        registry.list().await.is_empty(),
        "neither request created a session"
    );
}

#[tokio::test]
async fn retention_frees_the_key_of_the_session_it_removes() {
    let socket = temp_socket("request-retention");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    write_store(&store_path, FROZEN_STORE);
    let (registry, _worker_home) = worker_backed_registry(&socket, config);
    registry
        .reconcile_workers()
        .await
        .expect("startup reconciles the frozen session");
    let frozen = registry.list().await;
    assert_eq!(
        frozen.len(),
        1,
        "the frozen session is live in the registry"
    );

    // The frozen session is weeks old, so the shortest accepted TTLs select it.
    registry
        .set_retention_policy(protocol::SessionRetentionPolicy {
            terminal_ttl_secs: 3_600,
            lost_ttl_secs: 3_600,
            ..protocol::SessionRetentionPolicy::default()
        })
        .await
        .expect("the policy is within bounds");
    let swept = registry
        .sweep_retention(&protocol::SessionRetentionParams {
            dry_run: false,
            limit: None,
        })
        .await
        .expect("the sweep runs");
    assert_eq!(swept.removed, 1, "retention removed the frozen session");
    assert!(registry.list().await.is_empty());

    let params = keyed_params(&socket.dir.join(WORK_DIR_NAME), FROZEN_KEY, &json!({}));
    let outcome = registry
        .create_outcome(serde_json::from_value(params).expect("typed params"))
        .await
        .expect("the key is free again");
    assert!(!outcome.replayed, "the retry creates a new session");
    assert_ne!(outcome.session.id, frozen[0].id);
}

#[tokio::test]
async fn a_create_waiting_for_its_acknowledgement_is_in_progress() {
    let socket = temp_socket("request-ack");
    let store_path = socket.dir.join("metadata.jsonl");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/bin/sh",
            ["-c", "stty -echo; exec /bin/cat > /dev/null"],
        ),
        store_path: Some(store_path.clone()),
        host_state_dir: Some(socket.dir.join("host-state")),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = socket.dir.join(WORK_DIR_NAME);
    // The terminal does not echo and `cat` prints nothing, so the agent never
    // reports `working` and the acknowledgement keeps waiting until the
    // scenario stops the session.
    let params = || {
        keyed_params(
            &cwd,
            KEY,
            &json!({"input": "ack me", "input_ack_timeout_ms": ACK_WINDOW_MS}),
        )
    };
    let mut first_connection = connect(&socket).await;
    let first_params = params();
    let first = tokio::spawn(async move { send_new(&mut first_connection, first_params).await });
    let listed_id = wait_until("the session to be registered", || async {
        let mut observer = connect(&socket).await;
        listed(&mut observer)
            .await
            .first()
            .map(|session| session.id.clone())
    })
    .await;

    let in_progress = err_payload(send_new(&mut control, params()).await);
    assert_names_session(&in_progress, "session_request_in_progress", &listed_id);

    // The durable record drops its `initial_input` marker once the prompt was
    // delivered; the create then only waits for the acknowledgement.
    wait_until("the initial input marker to be committed", || async {
        let stored = std::fs::read_to_string(&store_path).expect("read the store");
        (stored.contains(&listed_id.0) && !stored.contains("\"initial_input\"")).then_some(())
    })
    .await;
    let still_in_progress = err_payload(send_new(&mut control, params()).await);
    assert_names_session(
        &still_in_progress,
        "session_request_in_progress",
        &listed_id,
    );

    // Stopping the session ends the acknowledgement wait without evidence of
    // the prompt being worked on.
    call_on(&mut control, method::SESSION_STOP, &listed_id).await;
    let settled = created(first.await.expect("the first create"));
    assert_eq!(settled.session.id, listed_id);
    assert_eq!(settled.applied_input, Some(true));
    assert!(
        settled.prompt_delivered.is_some(),
        "the window was asked for"
    );
    let replay = created(send_new(&mut control, params()).await);
    assert_eq!(replay.replayed, Some(true));
    assert_eq!(replay.session.id, listed_id);
    assert_eq!(
        replay.prompt_delivered, None,
        "the first response's fact is not replayed"
    );
    assert_eq!(replay.applied_input, Some(true));

    let _ = shutdown.send(());
    let _ = handle.await;
}

/// A started daemon over a git repository whose `post-create` hook is listed in
/// `hooks.required` and writes into one marker directory.
struct HookRig {
    control: Framed<UnixStream, LinesCodec>,
    socket: TestSocket,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
    repo: TestEnv,
    markers: PathBuf,
    config_dir: PathBuf,
    _worktree_root: TestDir,
    _state: TestDir,
}

impl HookRig {
    async fn start(tag: &str) -> Self {
        let repo = init_git_repo();
        let worktree_root = temp_dir(&format!("{tag}-wt"));
        let state = temp_dir(&format!("{tag}-state"));
        let socket = temp_socket(tag);
        let markers = socket.dir.join("markers");
        std::fs::create_dir_all(&markers).expect("create the marker directory");
        let config_dir = socket.dir.join("config");
        std::fs::create_dir_all(config_dir.join("hooks")).expect("create host hook directory");
        let config = SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", IDLE_SHELL]),
            worktree_root: Some(worktree_root.to_path_buf()),
            store_path: Some(state.join("metadata.jsonl")),
            config_dir: Some(config_dir.clone()),
            hook_required: vec![RequiredHook::PostCreate],
            supervision_retry_initial: RETRY_INITIAL,
            ..SessionRegistryConfig::default()
        };
        let (shutdown, server, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let control = connect(&socket).await;
        Self {
            control,
            socket,
            shutdown,
            server,
            repo,
            markers,
            config_dir,
            _worktree_root: worktree_root,
            _state: state,
        }
    }

    fn post_create(&self, body: &str) {
        write_executable(&self.config_dir.join("hooks").join("post-create"), body);
    }

    /// Lines the counting hook wrote: one per run.
    fn hook_runs(&self) -> usize {
        std::fs::read_to_string(self.markers.join("hook-runs"))
            .map_or(0, |text| text.lines().count())
    }

    async fn send(&mut self, params: Value) -> Response {
        send_new(&mut self.control, params).await
    }

    fn params(&self, branch: &str, key: &str) -> Value {
        keyed_worktree_params(self.repo.cwd(), branch, key)
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

#[tokio::test]
async fn a_create_that_has_not_settled_is_in_progress_and_a_settled_one_replays() {
    let mut rig = HookRig::start("request-progress").await;
    let fifo = rig.markers.join("release");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo failed");
    rig.post_create(&format!(
        "#!/bin/sh\necho run >> '{}'\nread _ < '{}'\n",
        rig.markers.join("hook-runs").display(),
        fifo.display()
    ));

    // The first create blocks in its post-create hook, on its own connection.
    let mut first_connection = connect(&rig.socket).await;
    let first_params = rig.params("b", KEY);
    let first = tokio::spawn(async move { send_new(&mut first_connection, first_params).await });
    wait_until("the post-create hook to start", || async {
        (rig.hook_runs() == 1).then_some(())
    })
    .await;

    let in_progress = err_payload(rig.send(rig.params("b", KEY)).await);
    assert_eq!(
        in_progress.code, "session_request_in_progress",
        "{in_progress:?}"
    );
    let pending = in_progress
        .details
        .as_ref()
        .and_then(|details| details.session_id.clone())
        .expect("the error names the session");
    assert_names_session(&in_progress, "session_request_in_progress", &pending);
    // Another request under the claimed key conflicts, naming the same session.
    let conflict = err_payload(rig.send(rig.params("other", KEY)).await);
    assert_names_session(&conflict, "session_request_conflict", &pending);
    assert_eq!(rig.hook_runs(), 1, "neither retry ran a hook");

    // Releasing the hook settles the create under the id the error named.
    let release = fifo.clone();
    tokio::task::spawn_blocking(move || std::fs::write(release, "go\n"))
        .await
        .expect("release task")
        .expect("release the hook");
    let settled = created(first.await.expect("the first create"));
    assert_eq!(settled.session.id, pending);
    assert_eq!(settled.replayed, None);

    let replay = created(rig.send(rig.params("b", KEY)).await);
    assert_eq!(replay.replayed, Some(true));
    assert_eq!(replay.session.id, pending);
    assert_eq!(rig.hook_runs(), 1, "the hook ran once");
    rig.stop().await;
}

#[tokio::test]
async fn a_rolled_back_or_refused_create_frees_its_key() {
    let mut rig = HookRig::start("request-freed").await;

    // A required hook fails: the create rolls back and the key is free.
    rig.post_create("#!/bin/sh\nexit 1\n");
    let failed = err_payload(rig.send(rig.params("b", "rolled-back")).await);
    assert_eq!(failed.code, "hook_required_failed", "{failed:?}");
    assert!(listed(&mut rig.control).await.is_empty());
    rig.post_create("#!/bin/sh\nexit 0\n");
    let retried = created(rig.send(rig.params("b", "rolled-back")).await);
    assert_eq!(retried.replayed, None, "the retry creates");

    // A request refused after the claim (an unknown agent) leaves no claim.
    let mut refused = rig.params("c", "refused");
    refused["agent"] = json!("no-such-agent");
    let error = err_payload(rig.send(refused).await);
    assert!(
        error.code != "session_request_in_progress" && error.code != "session_request_conflict",
        "{error:?}"
    );
    let fixed = created(rig.send(rig.params("c", "refused")).await);
    assert_eq!(fixed.replayed, None, "the same key creates once it can");
    rig.stop().await;
}

#[tokio::test]
async fn a_compensation_that_cannot_finish_holds_the_key_until_it_finishes() {
    let mut rig = HookRig::start("request-pending").await;
    // The hook locks its own worktree, which keeps the rollback from removing it.
    rig.post_create("#!/bin/sh\ngit worktree lock \"$PWD\"\nexit 1\n");

    let failed = err_payload(rig.send(rig.params("b", KEY)).await);
    assert_eq!(failed.code, "hook_required_failed", "{failed:?}");
    assert!(
        failed.msg.contains("create_compensation_pending"),
        "{}",
        failed.msg
    );
    let [pending] = <[SessionInfo; 1]>::try_from(listed(&mut rig.control).await)
        .expect("the unfinished create stays listed");

    let in_progress = err_payload(rig.send(rig.params("b", KEY)).await);
    assert_names_session(&in_progress, "session_request_in_progress", &pending.id);

    let git = |args: &[&str]| {
        let output = rig
            .repo
            .git_command()
            .arg("-C")
            .arg(rig.repo.cwd())
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let registered = git(&["worktree", "list", "--porcelain"]);
    let worktree = registered
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .find(|path| Path::new(path) != rig.repo.cwd())
        .expect("the locked worktree is still registered")
        .to_owned();
    git(&["worktree", "unlock", &worktree]);
    // The supervision retry finishes the compensation; the session leaves and
    // the key is free.
    let socket = &rig.socket;
    wait_until("the retry to compensate the create", || async {
        let mut observer = connect(socket).await;
        listed(&mut observer).await.is_empty().then_some(())
    })
    .await;

    rig.post_create("#!/bin/sh\nexit 0\n");
    let fresh = created(rig.send(rig.params("b", KEY)).await);
    assert_eq!(fresh.replayed, None, "the compensated create freed the key");
    assert_ne!(fresh.session.id, pending.id);
    rig.stop().await;
}
