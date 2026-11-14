//! The tree view of `session.list`: child counts, `top_level` and
//! `has_spawned_by` filters, and the list-only shape of `children`.
//!
//! The scenario builds one family (a parent with two children), one orphan
//! whose parent is gone, one session whose origin names another host, and one
//! unrelated session, then pins the counts and the filter answers. `children`
//! is list-only: inspect, events and the store never carry it.

// Rust guideline compliant 2026-11-13

use super::*;

/// A shell that stays alive for the whole scenario.
pub(crate) const TREE_IDLE_SHELL: &str = "exec sleep 30";

fn tree_origin(session_id: &str, daemon_id: &str) -> protocol::SpawnedBy {
    protocol::SpawnedBy {
        session_id: SessionId(session_id.to_owned()),
        daemon_id: daemon_id.to_owned(),
    }
}

/// Creates a shell session in `cwd`; `origin` is the request origin pair.
async fn create_tree_session(
    control: &mut Framed<UnixStream, LinesCodec>,
    cwd: PathBuf,
    origin: Option<&protocol::SpawnedBy>,
) -> SessionInfo {
    let params = session_params_in(cwd);
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

pub(crate) fn tree_cwd(socket: &TestSocket) -> PathBuf {
    socket.dir.join(WORK_DIR_NAME)
}

/// Creates the family: parent P, children C1 and C2, orphan O (parent gone),
/// foreign F (origin on another host) and unrelated U.
pub(crate) async fn create_tree(
    control: &mut Framed<UnixStream, LinesCodec>,
    cwd: PathBuf,
) -> (
    SessionInfo,
    SessionInfo,
    SessionInfo,
    SessionInfo,
    SessionInfo,
    SessionInfo,
) {
    let parent = create_tree_session(control, cwd.clone(), None).await;
    let origin = tree_origin(&parent.id.0, "d-local");
    let first = create_tree_session(control, cwd.clone(), Some(&origin)).await;
    let second = create_tree_session(control, cwd.clone(), Some(&origin)).await;
    let gone = tree_origin("s-gone", "d-local");
    let orphan = create_tree_session(control, cwd.clone(), Some(&gone)).await;
    let foreign_origin = tree_origin("s-elsewhere", "d-foreign-host");
    let foreign = create_tree_session(control, cwd.clone(), Some(&foreign_origin)).await;
    let unrelated = create_tree_session(control, cwd, None).await;
    (parent, first, second, orphan, foreign, unrelated)
}

async fn listed(
    control: &mut Framed<UnixStream, LinesCodec>,
    filters: Vec<SessionListFilter>,
) -> Vec<SessionInfo> {
    let request = Request::make(
        "session-list-tree",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams { filters }).expect("serialize params"),
    );
    serde_json::from_value(ok_payload(exchange(control, &request).await)).expect("session list")
}

fn by_id(sessions: &[SessionInfo]) -> BTreeMap<&str, &SessionInfo> {
    sessions
        .iter()
        .map(|session| (session.id.0.as_str(), session))
        .collect()
}

fn child_count(session: &SessionInfo) -> u32 {
    session
        .children
        .unwrap_or_else(|| panic!("{}: the list reports a child count", session.id.0))
}

#[tokio::test]
async fn the_list_reports_child_counts_over_the_unfiltered_list() {
    let socket = temp_socket("session-tree-counts");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, first, second, orphan, foreign, unrelated) = create_tree(&mut control, cwd).await;

    let sessions = listed(&mut control, Vec::new()).await;
    let viewed = by_id(&sessions);
    assert_eq!(child_count(viewed[parent.id.0.as_str()]), 2);
    assert_eq!(child_count(viewed[first.id.0.as_str()]), 0);
    assert_eq!(child_count(viewed[second.id.0.as_str()]), 0);
    assert_eq!(child_count(viewed[orphan.id.0.as_str()]), 0);
    assert_eq!(child_count(viewed[foreign.id.0.as_str()]), 0);
    assert_eq!(child_count(viewed[unrelated.id.0.as_str()]), 0);

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn top_level_lists_roots_including_orphans() {
    let socket = temp_socket("session-tree-top-level");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, _, _, orphan, foreign, unrelated) = create_tree(&mut control, cwd).await;

    let sessions = listed(&mut control, vec![SessionListFilter::TopLevel]).await;
    assert_eq!(
        tree_listed_ids(&sessions),
        tree_sorted(vec![
            parent.id.0.clone(),
            orphan.id.0.clone(),
            foreign.id.0.clone(),
            unrelated.id.0.clone(),
        ]),
    );
    // The counts are computed over the unfiltered list, so the parent keeps
    // both children behind the top-level filter.
    assert_eq!(
        by_id(&sessions)[parent.id.0.as_str()]
            .children
            .expect("a filtered list keeps full counts"),
        2
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn has_spawned_by_lists_every_attributed_session() {
    let socket = temp_socket("session-tree-has-spawned-by");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (_, first, second, orphan, foreign, _) = create_tree(&mut control, cwd).await;

    let sessions = listed(&mut control, vec![SessionListFilter::HasSpawnedBy]).await;
    assert_eq!(
        tree_listed_ids(&sessions),
        tree_sorted(vec![
            first.id.0.clone(),
            second.id.0.clone(),
            orphan.id.0.clone(),
            foreign.id.0.clone(),
        ]),
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn top_level_combines_with_the_spawner_filter() {
    let socket = temp_socket("session-tree-combined");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, _, _, _, _, _) = create_tree(&mut control, cwd).await;

    // Both orders of the conjunction: a child is never top-level.
    for filters in [
        vec![
            SessionListFilter::TopLevel,
            SessionListFilter::SpawnedBy(parent.id.0.clone()),
        ],
        vec![
            SessionListFilter::SpawnedBy(parent.id.0.clone()),
            SessionListFilter::TopLevel,
        ],
    ] {
        let sessions = listed(&mut control, filters).await;
        assert!(
            sessions.is_empty(),
            "no child is top-level: {:?}",
            tree_listed_ids(&sessions)
        );
    }
    // The top-level view alone keeps full child counts: the parent shows
    // both children behind the filter. The skeleton leaves `children`
    // unset, so this fails before the implementation.
    let sessions = listed(&mut control, vec![SessionListFilter::TopLevel]).await;
    let viewed = by_id(&sessions);
    assert_eq!(
        viewed[parent.id.0.as_str()]
            .children
            .expect("a filtered top-level list keeps full counts"),
        2
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test]
async fn children_stays_list_only() {
    let socket = temp_socket("session-tree-list-only");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, _, _, _, _, _) = create_tree(&mut control, cwd).await;

    let inspected = inspect_session(&mut control, &parent.id).await;
    assert_eq!(inspected.children, None, "inspect never carries children");

    let request = Request::make(
        "session-list-children-json",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams {
            filters: Vec::new(),
        })
        .expect("serialize params"),
    );
    let Value::Array(entries) = ok_payload(exchange(&mut control, &request).await) else {
        panic!("a session array");
    };
    let entry = entries
        .iter()
        .find(|entry| entry["id"] == parent.id.0)
        .expect("the parent is listed");
    assert!(
        entry.get("children").is_some(),
        "the list carries children: {entry}"
    );

    let store_path = socket.dir.join("metadata.jsonl");
    assert!(
        !std::fs::read_to_string(&store_path)
            .unwrap_or_default()
            .contains("\"children\""),
        "the persisted session line never carries children"
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

fn tree_listed_ids(sessions: &[SessionInfo]) -> Vec<String> {
    tree_sorted(
        sessions
            .iter()
            .map(|session| session.id.0.clone())
            .collect(),
    )
}

fn tree_sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// Drives `session.remove_tree` over the socket: a live family is kept, and
/// once the children stop the cascade removes the tree deepest first.
#[tokio::test]
async fn remove_tree_keeps_a_live_tree_then_removes_it_once_stopped() {
    let socket = temp_socket("session-tree-remove");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, first, second, _, _, _) = create_tree(&mut control, cwd).await;

    let kept: SessionRemoveTreeResult = serde_json::from_value(ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-remove-tree-live",
                method::SESSION_REMOVE_TREE,
                serde_json::to_value(SessionRemoveTreeParams {
                    session_id: parent.id.clone(),
                    accept_unconfirmed_cleanup: false,
                })
                .expect("serialize params"),
            ),
        )
        .await,
    ))
    .expect("remove_tree result");
    assert_eq!(kept.removed, 0, "{kept:?}");
    assert_eq!(kept.kept, 3, "{kept:?}");
    assert!(
        kept.sessions.iter().any(|node| node.session_id == first.id
            && node.keep_reason == Some(SessionTreeKeepReason::Live)),
        "the live child is kept live: {kept:?}"
    );

    for child in [&first, &second] {
        ok_payload(
            exchange(
                &mut control,
                &Request::make(
                    "session-stop-tree-child",
                    method::SESSION_STOP,
                    serde_json::to_value(&child.id).expect("serialize id"),
                ),
            )
            .await,
        );
    }

    let removed: SessionRemoveTreeResult = serde_json::from_value(ok_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-remove-tree-stopped",
                method::SESSION_REMOVE_TREE,
                serde_json::to_value(SessionRemoveTreeParams {
                    session_id: parent.id.clone(),
                    accept_unconfirmed_cleanup: false,
                })
                .expect("serialize params"),
            ),
        )
        .await,
    ))
    .expect("remove_tree result");
    assert_eq!(removed.removed, 3, "{removed:?}");
    assert_eq!(removed.kept, 0, "{removed:?}");
    assert_eq!(removed.refused, 0, "{removed:?}");
    let order: Vec<&str> = removed
        .sessions
        .iter()
        .map(|node| node.session_id.0.as_str())
        .collect();
    assert!(
        order.last() == Some(&parent.id.0.as_str()),
        "the root goes last: {removed:?}"
    );
    assert_eq!(
        listed(&mut control, Vec::new()).await.len(),
        3,
        "orphan, foreign and unrelated stay"
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}

/// `session.remove` with an object param is a `bad_request`, and the session
/// stays listed: the old method accepts only the bare session id.
#[tokio::test]
async fn remove_with_an_object_param_is_a_bad_request() {
    let socket = temp_socket("session-tree-remove-object");
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", TREE_IDLE_SHELL]),
        ..support::hermetic_registry_config()
    };
    let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
    let mut control = connect(&socket).await;
    let cwd = tree_cwd(&socket);

    let (parent, _, _, _, _, _) = create_tree(&mut control, cwd).await;

    let error = err_payload(
        exchange(
            &mut control,
            &Request::make(
                "session-remove-object",
                method::SESSION_REMOVE,
                serde_json::json!({"session_id": parent.id.0}),
            ),
        )
        .await,
    );
    assert_eq!(error.code, "bad_request", "{error:?}");
    assert!(
        listed(&mut control, Vec::new())
            .await
            .iter()
            .any(|session| session.id == parent.id),
        "the session is still listed"
    );

    drop(control);
    let _ = shutdown.send(());
    let _ = handle.await;
}
