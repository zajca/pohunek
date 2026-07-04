//! Headless GUI-core harness against in-process loopback daemons.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use pohunek_client::Client;
use pohunek_daemon::api::{DaemonState, HealthInfo, RemoteServer};
use pohunek_daemon::session::{SessionRegistry, SessionRegistryConfig};
use pohunek_gui_core::assistant::{self, AssistantPaths, Intent, LaunchParams};
use pohunek_gui_core::{
    add_project, host_subscription_stream, inspect_session, launch_action_prompt_with_options,
    launch_provider_item_with_options, list_project_actions, list_projects, load_host_snapshot,
    preview_action_prompt, preview_prompt_content, remove_project, rename_project,
    resolve_project_action, resolve_project_prompt, session_link_metadata, session_metadata_rows,
    set_session_metadata, show_project, spawn_attach_command, stop_session as stop_gui_session,
    workspace_connection_stream, AgentStateEvent, AttachCommandSpawner, AttachSpawnIntent,
    AttachTemplateValues, ConnState, ConnectionOptions, DetailTab, HealthSummary, HostConfig,
    HostEvent, HostId, HostSnapshot, Message, PromptContext, PromptLaunchParams, PromptPreview,
    ProviderLaunchItem, ProviderLaunchParams, Selection, SessionLinkKind, SessionLinkProvider,
    TreeNodeId, UiState, WindowSize, Workspace,
};
use protocol::{
    method, AgentActivity, AgentKind, ErrorClass, ProjectActionParams, ProjectActionResult,
    ProjectActionsParams, ProjectAddParams, ProjectPromptParams, ProjectRemoveParams,
    ProjectRenameParams, ProjectShowParams, ProtocolError, ProviderKind, Request, Response,
    SessionId, SessionInfo, SessionNewParams, SessionReportNativeIdParams,
    SessionSetMetadataParams, StateSource,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn loopback_hosts_seed_and_stream_agent_state() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-codex-bin");
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\n/bin/sleep 0.2\nprintf '\\033]2;Action Required\\007'\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let daemon_a = LoopbackDaemon::spawn("gui-a", "0.0.0-a").await;
    let daemon_b = LoopbackDaemon::spawn("gui-b", "0.0.0-b").await;
    let host_a = HostConfig::tcp("host-a", daemon_a.addr);
    let host_b = HostConfig::tcp("host-b", daemon_b.addr);

    let snapshot_a = load_host_snapshot(&host_a).await.expect("host-a seed");
    let snapshot_b = load_host_snapshot(&host_b).await.expect("host-b seed");
    assert_eq!(snapshot_a.health.status, "ok");
    assert_eq!(snapshot_b.health.status, "ok");
    assert!(snapshot_a.sessions.is_empty());
    assert!(snapshot_b.sessions.is_empty());

    let mut events = Box::pin(host_subscription_stream(host_a.clone()));
    assert!(matches!(
        events.next().await.expect("connecting message"),
        Message::HostConnecting { .. }
    ));
    assert!(matches!(
        events.next().await.expect("subscribed message"),
        Message::HostSubscribed { .. }
    ));

    let created = create_agent_session(&host_a, AgentKind::Codex, temp_dir("gui-core-cwd")).await;
    let state = wait_for_agent_state(&mut events, &created.id).await;
    assert_eq!(state.activity, AgentActivity::Blocked);
    assert_eq!(state.source, StateSource::OscTitle);

    stop_session(&host_a, &created.id).await;
    daemon_a.shutdown().await;
    daemon_b.shutdown().await;
}

#[tokio::test]
async fn workspace_connects_to_multiple_loopback_daemons_and_lists_sessions() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m1-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon_a = LoopbackDaemon::spawn("m1-a", "0.1.0-a").await;
    let daemon_b = LoopbackDaemon::spawn("m1-b", "0.1.0-b").await;
    let host_a = HostConfig::tcp("host-a", daemon_a.addr);
    let host_b = HostConfig::tcp("host-b", daemon_b.addr);
    let repo_a = init_git_repo("gui-core-m1-repo-a");
    let repo_b = init_git_repo("gui-core-m1-repo-b");
    let session_a = create_agent_session(&host_a, AgentKind::Codex, repo_a).await;
    let session_b = create_agent_session(&host_b, AgentKind::Codex, repo_b).await;

    let mut workspace = Workspace::default();
    let mut stream = Box::pin(workspace_connection_stream(
        vec![host_a.clone(), host_b.clone()],
        test_connection_options(),
    ));
    wait_for_hosts_with_sessions(
        &mut workspace,
        &mut stream,
        &[(&host_a, &session_a.id), (&host_b, &session_b.id)],
    )
    .await;

    let view_a = workspace.hosts.get(&host_a.id).expect("host-a view");
    let view_b = workspace.hosts.get(&host_b.id).expect("host-b view");
    assert_eq!(view_a.conn, ConnState::Connected);
    assert_eq!(view_b.conn, ConnState::Connected);
    assert!(view_a.sessions.contains_key(&session_a.id.0));
    assert!(view_b.sessions.contains_key(&session_b.id.0));
    assert!(
        !view_a.projects.is_empty(),
        "host-a should seed projects through project.list"
    );
    assert!(
        !view_b.projects.is_empty(),
        "host-b should seed projects through project.list"
    );

    stop_session(&host_a, &session_a.id).await;
    stop_session(&host_b, &session_b.id).await;
    daemon_a.shutdown().await;
    daemon_b.shutdown().await;
}

#[tokio::test]
async fn live_agent_state_updates_are_reflected() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m1-blocked-bin");
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\n/bin/sleep 0.2\nprintf '\\033]2;Action Required\\007'\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m1-blocked", "0.1.0-blocked").await;
    let host = HostConfig::tcp("host-blocked", daemon.addr);
    let mut workspace = Workspace::default();
    let mut stream = Box::pin(workspace_connection_stream(
        vec![host.clone()],
        test_connection_options(),
    ));
    wait_for_host_connected(&mut workspace, &mut stream, &host).await;

    let session = create_agent_session(&host, AgentKind::Codex, temp_dir("gui-core-m1-cwd")).await;
    wait_for_session_activity(
        &mut workspace,
        &mut stream,
        &host,
        &session.id,
        AgentActivity::Blocked,
    )
    .await;

    let view = workspace.hosts.get(&host.id).expect("host view");
    assert_eq!(
        view.sessions
            .get(&session.id.0)
            .and_then(|session| session.activity),
        Some(AgentActivity::Blocked)
    );
    // The transient blocked-session OS notification path was removed. OS intents
    // now originate from durable `notification_created` events produced by the
    // daemon projector, so a bare `agent_state` transition raises no intent.
    assert!(workspace.notification_intents.is_empty());
    assert!(workspace.toasts.is_empty());

    stop_session(&host, &session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn notification_seed_degrades_gracefully_without_daemon_support() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-notif-seed-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    // This daemon build does not serve `notification.list`, so seeding must be
    // non-fatal: the host still connects and streams sessions with an empty
    // inbox rather than failing the whole snapshot load.
    let daemon = LoopbackDaemon::spawn("notif-seed", "0.1.0-notif").await;
    let host = HostConfig::tcp("host-notif", daemon.addr);
    let mut workspace = Workspace::default();
    let mut stream = Box::pin(workspace_connection_stream(
        vec![host.clone()],
        test_connection_options(),
    ));
    wait_for_host_connected(&mut workspace, &mut stream, &host).await;

    let session =
        create_agent_session(&host, AgentKind::Codex, temp_dir("gui-core-notif-cwd")).await;
    wait_for_hosts_with_sessions(&mut workspace, &mut stream, &[(&host, &session.id)]).await;

    let view = workspace.hosts.get(&host.id).expect("host view");
    assert_eq!(view.conn, ConnState::Connected);
    assert!(view.notifications.is_empty());
    assert_eq!(workspace.unread_notification_count(), 0);

    stop_session(&host, &session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn notification_seed_runtime_error_surfaces_on_snapshot() {
    let daemon = NotificationListErrorDaemon::spawn().await;
    let host = HostConfig::tcp("host-notif-error", daemon.addr);

    let snapshot = load_host_snapshot(&host)
        .await
        .expect("runtime notification error is non-fatal to host seed");

    assert!(snapshot.notifications.is_empty());
    let error = snapshot
        .project_error
        .as_deref()
        .expect("notification.list runtime error is surfaced");
    assert!(
        error.contains("notification.list failed"),
        "seed error is attributed to notification.list: {error}"
    );
    assert!(
        error.contains("notification_store_unavailable"),
        "seed error keeps daemon code: {error}"
    );

    daemon.join().await;
}

#[tokio::test]
async fn unreachable_host_marks_error_without_breaking_other_hosts() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m1-unreachable-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m1-live", "0.1.0-live").await;
    let live_host = HostConfig::tcp("host-live", daemon.addr);
    let dead_host = HostConfig::tcp("host-dead", unused_loopback_addr().await);
    let session = create_agent_session(
        &live_host,
        AgentKind::Codex,
        init_git_repo("gui-core-m1-live-repo"),
    )
    .await;

    let mut workspace = Workspace::default();
    let mut stream = Box::pin(workspace_connection_stream(
        vec![dead_host.clone(), live_host.clone()],
        test_connection_options(),
    ));
    wait_for_host_error(&mut workspace, &mut stream, &dead_host).await;
    wait_for_hosts_with_sessions(&mut workspace, &mut stream, &[(&live_host, &session.id)]).await;

    let dead = workspace.hosts.get(&dead_host.id).expect("dead host view");
    assert_eq!(dead.conn, ConnState::Unreachable);
    assert!(dead.last_error.is_some());
    let live = workspace.hosts.get(&live_host.id).expect("live host view");
    assert_eq!(live.conn, ConnState::Connected);
    assert!(live.sessions.contains_key(&session.id.0));

    stop_session(&live_host, &session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn session_lifecycle_create_inspect_and_stop_reconciles_workspace_state() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m2-session-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m2-session", "0.2.0-session").await;
    let host = HostConfig::tcp("host-session", daemon.addr);
    let mut workspace = Workspace::default();
    let mut stream = Box::pin(workspace_connection_stream(
        vec![host.clone()],
        test_connection_options(),
    ));
    wait_for_host_connected(&mut workspace, &mut stream, &host).await;

    let created = pohunek_gui_core::create_session(
        &host,
        SessionNewParams {
            agent: agent_name(AgentKind::Codex).to_owned(),
            name: None,
            cwd: Some(temp_dir("gui-core-m2-session-cwd")),
            cols: 100,
            rows: 32,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: std::collections::BTreeMap::from([("source".to_owned(), "gui".to_owned())]),
        },
    )
    .await
    .expect("session.new through gui-core");
    workspace.apply(Message::SessionCreated {
        host_id: host.id.clone(),
        session: created.session.clone(),
    });
    assert!(workspace
        .hosts
        .get(&host.id)
        .expect("host view")
        .sessions
        .contains_key(&created.session.id.0));

    let inspected = inspect_session(&host, &created.session.id)
        .await
        .expect("session.inspect through gui-core");
    workspace.apply(Message::SessionInspected {
        host_id: host.id.clone(),
        session: inspected.clone(),
    });
    assert_eq!(inspected.id, created.session.id);
    assert_eq!(session_metadata_rows(&inspected)[0].key, "source");

    let stopped = stop_gui_session(&host, &created.session.id)
        .await
        .expect("session.stop through gui-core");
    assert!(stopped.stopped);
    workspace.apply(Message::SessionStopCompleted {
        host_id: host.id.clone(),
        session_id: created.session.id.clone(),
        result: stopped,
    });
    assert_eq!(
        workspace
            .hosts
            .get(&host.id)
            .and_then(|view| view.sessions.get(&created.session.id.0))
            .map(|session| session.state),
        Some(protocol::SessionState::Stopped)
    );

    wait_for_session_state(
        &mut workspace,
        &mut stream,
        &host,
        &created.session.id,
        protocol::SessionState::Stopped,
    )
    .await;

    daemon.shutdown().await;
}

#[tokio::test]
async fn session_metadata_merge_and_clear_round_trips() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m2-metadata-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m2-metadata", "0.2.0-metadata").await;
    let host = HostConfig::tcp("host-metadata", daemon.addr);
    let created = pohunek_gui_core::create_session(
        &host,
        SessionNewParams {
            agent: agent_name(AgentKind::Codex).to_owned(),
            name: None,
            cwd: Some(temp_dir("gui-core-m2-metadata-cwd")),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: std::collections::BTreeMap::from([
                ("keep".to_owned(), "original".to_owned()),
                ("remove".to_owned(), "gone".to_owned()),
            ]),
        },
    )
    .await
    .expect("session.new with metadata");

    let updated = set_session_metadata(
        &host,
        SessionSetMetadataParams {
            session_id: created.session.id.clone(),
            metadata: std::collections::BTreeMap::from([
                ("keep".to_owned(), Some("updated".to_owned())),
                ("remove".to_owned(), None),
                ("added".to_owned(), Some("value".to_owned())),
            ]),
        },
    )
    .await
    .expect("session.set_metadata");

    assert_eq!(
        updated.session.metadata,
        std::collections::BTreeMap::from([
            ("added".to_owned(), "value".to_owned()),
            ("keep".to_owned(), "updated".to_owned()),
        ])
    );
    let inspected = inspect_session(&host, &created.session.id)
        .await
        .expect("session.inspect after metadata update");
    assert_eq!(inspected.metadata, updated.session.metadata);
    assert_eq!(
        session_metadata_rows(&inspected)
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect::<Vec<_>>(),
        vec![
            ("added".to_owned(), "value".to_owned()),
            ("keep".to_owned(), "updated".to_owned()),
        ]
    );

    stop_session(&host, &created.session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn project_add_list_show_rename_and_remove_round_trips() {
    let _path_lock = PATH_LOCK.lock().await;
    let daemon = LoopbackDaemon::spawn("m2-project", "0.2.0-project").await;
    let host = HostConfig::tcp("host-project", daemon.addr);
    let repo = init_git_repo("gui-core-m2-project-repo");

    let added = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo.clone()),
            name: Some("M2 Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    assert_eq!(added.label, "M2 Project");

    let listed = list_projects(&host).await.expect("project.list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, added.id);

    let shown = show_project(
        &host,
        ProjectShowParams {
            reference: added.id.clone(),
        },
    )
    .await
    .expect("project.show");
    assert_eq!(shown.project.id, added.id);
    assert!(shown
        .worktrees
        .iter()
        .any(|worktree| worktree.path == std::fs::canonicalize(&repo).expect("canonical repo")));

    let renamed = rename_project(
        &host,
        ProjectRenameParams {
            reference: added.id.clone(),
            name: "Renamed M2 Project".to_owned(),
        },
    )
    .await
    .expect("project.rename");
    assert_eq!(renamed.label, "Renamed M2 Project");

    let removed = remove_project(
        &host,
        ProjectRemoveParams {
            reference: renamed.id.clone(),
            prune_worktrees: false,
        },
    )
    .await
    .expect("project.remove");
    assert!(removed.removed);
    assert!(list_projects(&host)
        .await
        .expect("project.list after remove")
        .is_empty());

    daemon.shutdown().await;
}

#[tokio::test]
async fn worktree_creation_is_session_new_with_branch_and_visible_in_project_show() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m2-worktree-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m2-worktree", "0.2.0-worktree").await;
    let host = HostConfig::tcp("host-worktree", daemon.addr);
    let repo = init_git_repo("gui-core-m2-worktree-repo");
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Worktree Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add for worktree");

    let created = pohunek_gui_core::create_session(
        &host,
        SessionNewParams {
            agent: agent_name(AgentKind::Codex).to_owned(),
            name: None,
            cwd: None,
            cols: 80,
            rows: 24,
            project: Some(project.id.clone()),
            repo: None,
            branch: Some("feature/gui-m2".to_owned()),
            base_branch: Some("main".to_owned()),
            input: None,
            metadata: std::collections::BTreeMap::new(),
        },
    )
    .await
    .expect("session.new creates worktree when branch is set");

    assert_eq!(
        created.session.project_id.as_deref(),
        Some(project.id.as_str())
    );
    assert_eq!(created.session.branch.as_deref(), Some("feature/gui-m2"));
    assert!(
        created.session.worktree_path.is_some(),
        "worktree creation must be represented by session.new with branch"
    );

    let shown = show_project(
        &host,
        ProjectShowParams {
            reference: project.id.clone(),
        },
    )
    .await
    .expect("project.show after worktree session");
    assert!(shown.worktrees.iter().any(|worktree| {
        worktree.owned && worktree.session_id.as_deref() == Some(created.session.id.0.as_str())
    }));

    stop_session(&host, &created.session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn prompt_actions_and_prompt_resolve_from_target_host() {
    let _path_lock = PATH_LOCK.lock().await;
    let daemon = LoopbackDaemon::spawn("m3-resolve", "0.3.0-resolve").await;
    let host = HostConfig::tcp("host-prompts", daemon.addr);
    let repo = init_git_repo("gui-core-m3-resolve-repo");
    write_file(
        &repo.join(".pohunek/templates.toml"),
        r#"
[template.issue]
agent = "codex"
prompt = "issue"
base_branch = "main"
"#,
    );
    write_file(
        &repo.join(".pohunek/actions.toml"),
        r#"
[action.process-issue]
template = "issue"
provider = "linear_issue"
"#,
    );
    write_file(
        &repo.join(".pohunek/prompts/issue.tmpl"),
        "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n",
    );
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Prompt Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");

    let actions = list_project_actions(
        &host,
        ProjectActionsParams {
            reference: project.id.clone(),
        },
    )
    .await
    .expect("project.actions through gui-core");
    assert_eq!(actions.actions.len(), 1);
    assert_eq!(actions.actions[0].name, "process-issue");
    assert_eq!(actions.actions[0].provider, ProviderKind::LinearIssue);

    let prompt = resolve_project_prompt(
        &host,
        ProjectPromptParams {
            reference: project.id.clone(),
            name: "issue".to_owned(),
        },
    )
    .await
    .expect("project.prompt through gui-core");
    assert_eq!(
        prompt.content,
        "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n"
    );

    let action = resolve_project_action(
        &host,
        ProjectActionParams {
            reference: project.id,
            name: "process-issue".to_owned(),
        },
    )
    .await
    .expect("project.action through gui-core");
    assert_eq!(action.agent, "codex");
    assert_eq!(action.base_branch.as_deref(), Some("main"));
    assert_eq!(action.prompt_content, prompt.content);

    daemon.shutdown().await;
}

#[tokio::test]
async fn remote_prompt_resolution_uses_target_daemon_config_not_operator_filesystem() {
    let _path_lock = PATH_LOCK.lock().await;
    let operator_config_home = temp_dir("gui-core-m3-operator-config-home");
    write_file(
        &operator_config_home.join("pohunek/prompts/issue.tmpl"),
        "OPERATOR LOCAL ${title}",
    );
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", operator_config_home);

    let remote_config_dir = temp_dir("gui-core-m3-remote-config");
    write_file(
        &remote_config_dir.join("prompts/issue.tmpl"),
        "REMOTE TARGET ${title}",
    );
    let daemon = LoopbackDaemon::spawn_with_config(
        "m3-remote",
        "0.3.0-remote",
        Some(remote_config_dir),
        None,
        None,
        false,
    )
    .await;
    let host = HostConfig::tcp("remote-host", daemon.addr);
    let repo = init_git_repo("gui-core-m3-remote-repo");
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Remote Prompt Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");

    let prompt = resolve_project_prompt(
        &host,
        ProjectPromptParams {
            reference: project.id,
            name: "issue".to_owned(),
        },
    )
    .await
    .expect("project.prompt through remote daemon");

    assert_eq!(prompt.content, "REMOTE TARGET ${title}");
    assert!(!prompt.content.contains("OPERATOR LOCAL"));

    daemon.shutdown().await;
}

#[test]
fn rendered_gui_prompt_matches_shared_prompt_render_for_same_context() {
    let action = ProjectActionResult {
        provider: ProviderKind::LinearIssue,
        agent: "codex".to_owned(),
        base_branch: Some("develop".to_owned()),
        branch: None,
        prompt_name: "issue".to_owned(),
        prompt_content: "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n".to_owned(),
    };
    let context_json = r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#;

    let preview =
        preview_action_prompt(&action, "LIN-123", context_json).expect("GUI action prompt preview");
    let direct_preview = preview_prompt_content(
        "issue",
        &action.prompt_content,
        &PromptContext {
            provider: pohunek_gui_core::PromptProvider::LinearIssue,
            item_id: "LIN-123".to_owned(),
            json: context_json.to_owned(),
        },
    )
    .expect("GUI direct prompt preview");
    let expected = pohunek_gui_core::render_prompt(
        &action.prompt_content,
        pohunek_gui_core::PromptProvider::LinearIssue,
        "LIN-123",
        context_json,
    )
    .expect("shared prompt render");

    assert_eq!(preview.rendered, expected);
    assert_eq!(direct_preview.rendered, expected);
    assert_eq!(preview.prompt_name, "issue");
    assert_eq!(preview.branch.as_deref(), Some("lin-123-fix-launcher"));
}

#[test]
fn preview_state_updates_without_launching_session() {
    let host_id = HostId::new("preview-host");
    let mut workspace = Workspace::default();
    workspace.apply(Message::HostSnapshotLoaded {
        snapshot: HostSnapshot {
            host_id: host_id.clone(),
            health: HealthSummary {
                status: "ok".to_owned(),
                daemon_version: "0.3.0-preview".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            },
            sessions: Vec::new(),
            projects: Vec::new(),
            project_error: None,
            notifications: Vec::new(),
        },
    });
    let preview = PromptPreview {
        prompt_name: "issue".to_owned(),
        rendered: "Issue LIN-123".to_owned(),
        branch: Some("lin-123".to_owned()),
    };

    workspace.apply(Message::PromptPreviewRendered {
        host_id: host_id.clone(),
        preview: preview.clone(),
    });

    let host = workspace.hosts.get(&host_id).expect("host view");
    assert_eq!(host.prompt.preview, Some(preview));
    assert!(host.sessions.is_empty());
}

#[tokio::test]
async fn launch_from_rendered_preset_creates_one_session_with_rendered_input() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m3-launch-bin");
    let record_dir = temp_dir("gui-core-m3-launch-record");
    let prompt_out = record_dir.join("prompt.txt");
    let _prompt_out = EnvGuard::set("POHUNEK_TEST_PROMPT_OUT", &prompt_out);
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\nprintf '%s' \"${1:-}\" > \"$POHUNEK_TEST_PROMPT_OUT\"\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m3-launch", "0.3.0-launch").await;
    let host = HostConfig::tcp("host-launch", daemon.addr);
    let repo = init_git_repo("gui-core-m3-launch-repo");
    write_provider_action_fixture(
        &repo,
        "issue",
        "process-issue",
        "linear_issue",
        "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n",
    );
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Launch Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    let before = load_host_snapshot(&host)
        .await
        .expect("snapshot before launch")
        .sessions
        .len();
    let action = resolve_project_action(
        &host,
        ProjectActionParams {
            reference: project.id.clone(),
            name: "process-issue".to_owned(),
        },
    )
    .await
    .expect("project.action");
    let context_json = r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123"}"#;
    let preview =
        preview_action_prompt(&action, "LIN-123", context_json).expect("render action preview");

    let launched = launch_action_prompt_with_options(
        &host,
        PromptLaunchParams {
            project: project.id.clone(),
            action,
            preview: preview.clone(),
            cols: 80,
            rows: 24,
            metadata: std::collections::BTreeMap::new(),
            name: None,
        },
        test_connection_options(),
    )
    .await
    .expect("launch rendered prompt");

    assert_eq!(
        launched.session.branch.as_deref(),
        Some("lin-123-fix-launcher")
    );
    assert_eq!(
        launched.session.project_id.as_deref(),
        Some(project.id.as_str())
    );
    assert!(launched.session.metadata.is_empty());

    let recorded = wait_for_file(&prompt_out).await;
    assert_eq!(recorded, preview.rendered);

    let after = load_host_snapshot(&host)
        .await
        .expect("snapshot after launch")
        .sessions;
    assert_eq!(after.len(), before + 1);
    assert_eq!(
        after
            .iter()
            .filter(|session| session.id == launched.session.id)
            .count(),
        1
    );

    stop_session(&host, &launched.session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn assistant_launch_creates_project_session_with_opening_prompt() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-assistant-bin");
    let record_dir = temp_dir("gui-core-assistant-record");
    let prompt_out = record_dir.join("prompt.txt");
    let _prompt_out = EnvGuard::set("POHUNEK_TEST_PROMPT_OUT", &prompt_out);
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\nprintf '%s' \"${1:-}\" > \"$POHUNEK_TEST_PROMPT_OUT\"\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("assistant-launch", "0.4.0-assistant").await;
    let host = HostConfig::tcp("host-assistant", daemon.addr);
    let repo = init_git_repo("gui-core-assistant-repo");
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Assistant Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    let paths = AssistantPaths {
        runtime_dir: temp_dir("gui-core-assistant-runtime"),
        data_dir: temp_dir("gui-core-assistant-data"),
        log_dir: temp_dir("gui-core-assistant-logs"),
        cache_dir: temp_dir("gui-core-assistant-cache"),
        config_dir: temp_dir("gui-core-assistant-config"),
    };

    let launched = assistant::launch_with_options(
        &host,
        &paths,
        LaunchParams {
            intent: Intent::Debug,
            request: Some("inspect the GUI assistant launcher".to_owned()),
            agent: None,
            project: Some(project.id.clone()),
            repo: None,
            branch: None,
            base_branch: None,
            cols: 80,
            rows: 24,
            no_snapshot: true,
            degraded: false,
            auto_started_daemon: false,
        },
        test_connection_options(),
    )
    .await
    .expect("assistant launch");

    assert_eq!(
        launched.session.project_id.as_deref(),
        Some(project.id.as_str())
    );
    assert_eq!(launched.session.agent, "codex");
    assert_eq!(launched.applied_input, Some(true));
    assert_eq!(launched.assistant.intent, Intent::Debug);
    assert_eq!(launched.assistant.agent, "codex");
    assert_eq!(launched.assistant.knowledge, "materialized");

    let recorded = wait_for_file(&prompt_out).await;
    assert!(recorded.contains("# Pohunek Assistant"));
    assert!(recorded.contains("intent: debug"));
    assert!(recorded.contains("request: inspect the GUI assistant launcher"));

    stop_session(&host, &launched.session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "keeps the linked launch and restart persistence flow in one end-to-end assertion"
)]
async fn provider_launch_linear_issue_creates_one_linked_session_and_persists_metadata() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m4-linear-bin");
    let record_dir = temp_dir("gui-core-m4-linear-record");
    let prompt_out = record_dir.join("prompt.txt");
    let _prompt_out = EnvGuard::set("POHUNEK_TEST_PROMPT_OUT", &prompt_out);
    write_executable(
        &bin_dir.join("codex"),
        "#!/bin/sh\nprintf '%s' \"${1:-}\" > \"$POHUNEK_TEST_PROMPT_OUT\"\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let store_path = temp_dir("gui-core-m4-linear-store").join("metadata.jsonl");
    let daemon = LoopbackDaemon::spawn_with_store_path(
        "m4-linear",
        "0.4.0-linear",
        store_path.clone(),
        false,
    )
    .await;
    let host = HostConfig::tcp("host-linear", daemon.addr);
    let repo = init_git_repo("gui-core-m4-linear-repo");
    write_provider_action_fixture(
        &repo,
        "issue",
        "process-issue",
        "linear_issue",
        "Issue ${id}: ${title}\n${body}\nbranch=${branch}\n",
    );
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Linear Launch Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    let before = load_host_snapshot(&host)
        .await
        .expect("snapshot before linear launch")
        .sessions
        .len();
    let context_json = r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher","url":"https://linear.test/LIN-123","token":"lin_api_secret_fixture"}"#;
    let item =
        ProviderLaunchItem::linear_issue("LIN-123", context_json, "https://linear.test/LIN-123")
            .expect("linear launch item");

    let launched = launch_provider_item_with_options(
        &host,
        ProviderLaunchParams {
            project: project.id.clone(),
            action_name: "process-issue".to_owned(),
            item,
            cols: 80,
            rows: 24,
            name: None,
        },
        test_connection_options(),
    )
    .await
    .expect("launch linked Linear issue");

    let expected_prompt = "Issue LIN-123: Fix launcher\nIssue body\nbranch=lin-123-fix-launcher\n";
    assert_eq!(wait_for_file(&prompt_out).await, expected_prompt);
    assert_eq!(
        launched.session.branch.as_deref(),
        Some("lin-123-fix-launcher")
    );
    assert_eq!(
        launched.session.project_id.as_deref(),
        Some(project.id.as_str())
    );
    let expected_link = expected_linear_link_metadata();
    assert_eq!(
        launched.session.metadata,
        expected_link.to_session_metadata()
    );
    assert_eq!(
        session_link_metadata(&launched.session),
        Some(expected_link)
    );
    let metadata_json = serde_json::to_string(&launched.session.metadata).expect("metadata json");
    assert!(!metadata_json.contains("lin_api_secret_fixture"));

    let after = load_host_snapshot(&host)
        .await
        .expect("snapshot after linear launch")
        .sessions;
    assert_eq!(after.len(), before + 1);
    assert_eq!(
        after
            .iter()
            .filter(|session| session.id == launched.session.id)
            .count(),
        1
    );

    report_native_id(&host, &launched.session.id, "codex", "native-linear-1").await;
    let captured = wait_for_native_id_tcp(&host, &launched.session.id, "native-linear-1").await;
    assert_eq!(captured.metadata, launched.session.metadata);

    daemon.shutdown().await;
    let restarted = LoopbackDaemon::spawn_with_store_path(
        "m4-linear-restart",
        "0.4.0-linear",
        store_path,
        true,
    )
    .await;
    let restarted_host = HostConfig::tcp("host-linear-restart", restarted.addr);
    let resumed = wait_for_session_with_metadata(
        &restarted_host,
        &launched.session.id,
        &launched.session.metadata,
    )
    .await;
    assert_eq!(
        session_link_metadata(&resumed),
        session_link_metadata(&launched.session)
    );

    stop_session(&restarted_host, &launched.session.id).await;
    restarted.shutdown().await;
}

#[tokio::test]
async fn provider_launch_github_pr_creates_one_linked_session_with_rendered_input() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m4-github-bin");
    let record_dir = temp_dir("gui-core-m4-github-record");
    let prompt_out = record_dir.join("prompt.txt");
    let _prompt_out = EnvGuard::set("POHUNEK_TEST_PROMPT_OUT", &prompt_out);
    write_executable(
        &bin_dir.join("claude"),
        "#!/bin/sh\nprintf '%s' \"${1:-}\" > \"$POHUNEK_TEST_PROMPT_OUT\"\n/bin/sleep 30\n",
    );
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m4-github", "0.4.0-github").await;
    let host = HostConfig::tcp("host-github", daemon.addr);
    let repo = init_git_repo("gui-core-m4-github-repo");
    write_provider_action_fixture(
        &repo,
        "pr",
        "review-pr",
        "github_pr",
        "PR ${number}: ${title}\n${body}\nbranch=${branch}\nurl=${url}\n",
    );
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("GitHub Launch Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    let before = load_host_snapshot(&host)
        .await
        .expect("snapshot before github launch")
        .sessions
        .len();
    let context_json = r#"{"number":7,"title":"Fix filters","body":"Body text","headRefName":"feature/filters","branch":"feature/filters","url":"https://github.example/repo/pull/7"}"#;
    let item = ProviderLaunchItem::github_pull_request(
        "7",
        context_json,
        "https://github.example/repo/pull/7",
    )
    .expect("GitHub PR launch item");

    let launched = launch_provider_item_with_options(
        &host,
        ProviderLaunchParams {
            project: project.id.clone(),
            action_name: "review-pr".to_owned(),
            item,
            cols: 80,
            rows: 24,
            name: None,
        },
        test_connection_options(),
    )
    .await
    .expect("launch linked GitHub PR");

    let expected_prompt =
        "PR 7: Fix filters\nBody text\nbranch=feature/filters\nurl=https://github.example/repo/pull/7\n";
    assert_eq!(wait_for_file(&prompt_out).await, expected_prompt);
    assert_eq!(launched.session.branch.as_deref(), Some("feature/filters"));
    assert_eq!(
        launched.session.metadata,
        std::collections::BTreeMap::from([
            ("link.provider".to_owned(), "github".to_owned()),
            ("link.kind".to_owned(), "pull_request".to_owned()),
            ("link.id".to_owned(), "7".to_owned()),
            (
                "link.url".to_owned(),
                "https://github.example/repo/pull/7".to_owned(),
            ),
            ("link.branch".to_owned(), "feature/filters".to_owned()),
        ])
    );

    let after = load_host_snapshot(&host)
        .await
        .expect("snapshot after github launch")
        .sessions;
    assert_eq!(after.len(), before + 1);
    assert_eq!(
        after
            .iter()
            .filter(|session| session.id == launched.session.id)
            .count(),
        1
    );

    stop_session(&host, &launched.session.id).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn prompt_errors_surface_without_corrupting_workspace_state() {
    let _path_lock = PATH_LOCK.lock().await;
    let bin_dir = temp_dir("gui-core-m3-error-bin");
    write_executable(&bin_dir.join("codex"), "#!/bin/sh\n/bin/sleep 30\n");
    let _path = PathGuard::prepend(&bin_dir);

    let daemon = LoopbackDaemon::spawn("m3-error", "0.3.0-error").await;
    let host = HostConfig::tcp("host-error", daemon.addr);
    let repo = init_git_repo("gui-core-m3-error-repo");
    write_prompt_error_fixture(&repo);
    let project = add_project(
        &host,
        ProjectAddParams {
            path: Some(repo),
            name: Some("Error Project".to_owned()),
            base_branch: Some("main".to_owned()),
        },
    )
    .await
    .expect("project.add");
    let existing =
        create_agent_session(&host, AgentKind::Codex, temp_dir("gui-core-m3-existing")).await;
    let mut workspace = Workspace::default();
    workspace.apply(Message::HostSnapshotLoaded {
        snapshot: load_host_snapshot(&host).await.expect("seed workspace"),
    });
    let before_sessions = workspace
        .hosts
        .get(&host.id)
        .expect("host view")
        .sessions
        .clone();
    let before_projects = workspace
        .hosts
        .get(&host.id)
        .expect("host view")
        .projects
        .clone();

    apply_prompt_error_cases(&mut workspace, &host, project.id).await;

    let host_view = workspace
        .hosts
        .get(&host.id)
        .expect("host view after errors");
    assert!(host_view.last_error.is_some());
    assert_eq!(host_view.sessions, before_sessions);
    assert_eq!(host_view.projects, before_projects);
    assert!(host_view.sessions.contains_key(&existing.id.0));

    stop_session(&host, &existing.id).await;
    daemon.shutdown().await;
}

fn write_prompt_error_fixture(repo: &Path) {
    write_file(
        &repo.join(".pohunek/templates.toml"),
        r#"
[template.issue]
agent = "codex"
prompt = "issue"
"#,
    );
    write_file(
        &repo.join(".pohunek/actions.toml"),
        r#"
[action.process-issue]
template = "issue"
provider = "linear_issue"
"#,
    );
    write_file(
        &repo.join(".pohunek/prompts/issue.tmpl"),
        "Issue ${id}: ${missing}\n",
    );
}

async fn apply_prompt_error_cases(
    workspace: &mut Workspace,
    host: &HostConfig,
    project_id: String,
) {
    let missing_prompt = resolve_project_prompt(
        host,
        ProjectPromptParams {
            reference: project_id.clone(),
            name: "missing".to_owned(),
        },
    )
    .await
    .expect_err("missing prompt should fail");
    workspace.apply(Message::HostOperationFailed {
        host_id: host.id.clone(),
        error: missing_prompt.to_string(),
    });

    let missing_action = resolve_project_action(
        host,
        ProjectActionParams {
            reference: project_id.clone(),
            name: "missing-action".to_owned(),
        },
    )
    .await
    .expect_err("missing action should fail");
    workspace.apply(Message::HostOperationFailed {
        host_id: host.id.clone(),
        error: missing_action.to_string(),
    });

    let action = resolve_project_action(
        host,
        ProjectActionParams {
            reference: project_id,
            name: "process-issue".to_owned(),
        },
    )
    .await
    .expect("project.action");
    let render_error = preview_action_prompt(
        &action,
        "LIN-123",
        r#"{"identifier":"LIN-123","title":"Fix launcher","description":"Issue body","branchName":"lin-123-fix-launcher"}"#,
    )
    .expect_err("unknown variable should fail");
    workspace.apply(Message::HostOperationFailed {
        host_id: host.id.clone(),
        error: render_error.to_string(),
    });
}

#[test]
fn attach_command_spawn_intent_is_resolved_without_embedded_terminal() {
    #[derive(Debug, Default)]
    struct RecordingSpawner {
        commands: Vec<String>,
    }

    impl AttachCommandSpawner for RecordingSpawner {
        fn spawn(&mut self, command: &str) -> Result<(), String> {
            self.commands.push(command.to_owned());
            Ok(())
        }
    }

    let mut spawner = RecordingSpawner::default();
    let intent = spawn_attach_command(
        &mut spawner,
        "$TERMINAL -e {bin} attach --host {host} {id}",
        &AttachTemplateValues {
            bin: "pohunek".to_owned(),
            host: "devbox".to_owned(),
            id: "s-42".to_owned(),
        },
    )
    .expect("spawn attach command");

    assert_eq!(
        intent,
        AttachSpawnIntent {
            command: "$TERMINAL -e pohunek attach --host devbox s-42".to_owned(),
        }
    );
    assert_eq!(spawner.commands, vec![intent.command]);
}

#[test]
fn ui_state_persists_and_restores() {
    let state_dir = temp_dir("gui-core-m1-ui-state");
    let mut expanded = std::collections::BTreeSet::new();
    let host = HostConfig::tcp("host-a", "127.0.0.1:65535".parse().expect("addr"));
    expanded.insert(TreeNodeId::host(host.id.clone()));
    expanded.insert(TreeNodeId::project(host.id.clone(), "p-1"));
    let state = UiState {
        left_pane_width: 312,
        agents_pane_height: 420,
        window_size: WindowSize {
            width: 1440,
            height: 900,
        },
        expanded_nodes: expanded,
        selection: Some(Selection::Session {
            host_id: host.id,
            session_id: SessionId("s-1".to_owned()),
        }),
        open_tabs: vec![DetailTab::Session, DetailTab::Agents],
        active_tab: DetailTab::Agents,
    };

    state.save_to_dir(&state_dir).expect("save ui state");
    let restored = UiState::load_from_dir(&state_dir).expect("restore ui state");

    assert_eq!(restored, state);
}

#[test]
fn ui_state_load_raises_legacy_agents_pane_height() {
    let state_dir = temp_dir("gui-core-m1-ui-state-agents-height");
    let state = UiState {
        agents_pane_height: 220,
        ..UiState::default()
    };

    state.save_to_dir(&state_dir).expect("save ui state");
    let restored = UiState::load_from_dir(&state_dir).expect("restore ui state");

    assert_eq!(restored.agents_pane_height, 360);
}

struct LoopbackDaemon {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl LoopbackDaemon {
    async fn spawn(tag: &str, version: &str) -> Self {
        Self::spawn_with_config(tag, version, None, None, None, false).await
    }

    async fn spawn_with_store_path(
        tag: &str,
        version: &str,
        store_path: PathBuf,
        load_resume: bool,
    ) -> Self {
        Self::spawn_with_config(tag, version, None, None, Some(store_path), load_resume).await
    }

    async fn spawn_with_config(
        tag: &str,
        version: &str,
        config_dir: Option<PathBuf>,
        shell_command: Option<pohunek_daemon::session::ShellCommand>,
        store_path: Option<PathBuf>,
        load_resume: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let addr = listener.local_addr().expect("local addr");
        let store_path =
            store_path.unwrap_or_else(|| temp_dir(&format!("{tag}-state")).join("metadata.jsonl"));
        let mut config = SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            store_path: Some(store_path),
            worktree_root: Some(temp_dir(&format!("{tag}-worktrees"))),
            config_dir,
            ..SessionRegistryConfig::default()
        };
        if let Some(shell_command) = shell_command {
            config.shell_command = shell_command;
        }
        let sessions = SessionRegistry::new(config);
        if load_resume {
            sessions.load_and_resume().await;
        }
        let state = DaemonState::new(HealthInfo::new(version), sessions);
        let server = RemoteServer::from_listener(listener, state);
        let (shutdown, rx) = oneshot::channel();
        let tag = tag.to_owned();
        let handle = tokio::spawn(async move {
            server
                .serve(async move {
                    let _ = rx.await;
                })
                .await;
            drop(tag);
        });
        Self {
            addr,
            shutdown,
            handle,
        }
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }
}

struct NotificationListErrorDaemon {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl NotificationListErrorDaemon {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("notification error daemon bind");
        let addr = listener
            .local_addr()
            .expect("notification error daemon addr");
        let handle = tokio::spawn(async move {
            let (stream, _addr) = listener
                .accept()
                .await
                .expect("notification error daemon accept");
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                let bytes = reader
                    .read_line(&mut line)
                    .await
                    .expect("notification error daemon read request");
                if bytes == 0 {
                    break;
                }
                let request: Request =
                    serde_json::from_str(line.trim_end()).expect("parse request");
                let response = notification_error_response(&request);
                let reply = serde_json::to_string(&response).expect("serialize response");
                reader
                    .get_mut()
                    .write_all(reply.as_bytes())
                    .await
                    .expect("write response");
                reader
                    .get_mut()
                    .write_all(b"\n")
                    .await
                    .expect("write response newline");
            }
        });
        Self { addr, handle }
    }

    async fn join(self) {
        self.handle.await.expect("notification error daemon task");
    }
}

fn notification_error_response(request: &Request) -> Response {
    match request.method.as_str() {
        method::DAEMON_HEALTH => Response::ok(
            request.id.clone(),
            serde_json::to_value(HealthSummary {
                status: "ok".to_owned(),
                daemon_version: "0.1.0-notif-error".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            })
            .expect("serialize health"),
        ),
        method::SESSION_LIST | method::PROJECT_LIST => {
            Response::ok(request.id.clone(), serde_json::json!([]))
        }
        method::NOTIFICATION_LIST => Response::err(
            request.id.clone(),
            ProtocolError::new(
                ErrorClass::Runtime,
                "notification_store_unavailable",
                "notification store unavailable",
                None,
            ),
        ),
        method => Response::err(request.id.clone(), ProtocolError::method_not_found(method)),
    }
}

fn test_connection_options() -> ConnectionOptions {
    ConnectionOptions {
        connect_timeout: Duration::from_millis(100),
        request_timeout: Duration::from_millis(500),
        reconcile_interval: Duration::from_millis(100),
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
    }
}

async fn wait_for_hosts_with_sessions<S>(
    workspace: &mut Workspace,
    events: &mut S,
    expected: &[(&HostConfig, &SessionId)],
) where
    S: futures::Stream<Item = Message> + Unpin,
{
    wait_for_workspace(events, workspace, |workspace| {
        expected.iter().all(|(host, session_id)| {
            workspace
                .hosts
                .get(&host.id)
                .is_some_and(|view| view.sessions.contains_key(&session_id.0))
        })
    })
    .await;
}

async fn wait_for_host_connected<S>(workspace: &mut Workspace, events: &mut S, host: &HostConfig)
where
    S: futures::Stream<Item = Message> + Unpin,
{
    wait_for_workspace(events, workspace, |workspace| {
        workspace
            .hosts
            .get(&host.id)
            .is_some_and(|view| view.conn == ConnState::Connected)
    })
    .await;
}

async fn wait_for_host_error<S>(workspace: &mut Workspace, events: &mut S, host: &HostConfig)
where
    S: futures::Stream<Item = Message> + Unpin,
{
    wait_for_workspace(events, workspace, |workspace| {
        workspace
            .hosts
            .get(&host.id)
            .is_some_and(|view| view.conn == ConnState::Unreachable && view.last_error.is_some())
    })
    .await;
}

async fn wait_for_session_activity<S>(
    workspace: &mut Workspace,
    events: &mut S,
    host: &HostConfig,
    session_id: &SessionId,
    activity: AgentActivity,
) where
    S: futures::Stream<Item = Message> + Unpin,
{
    wait_for_workspace(events, workspace, |workspace| {
        workspace
            .hosts
            .get(&host.id)
            .and_then(|view| view.sessions.get(&session_id.0))
            .and_then(|session| session.activity)
            == Some(activity)
    })
    .await;
}

async fn wait_for_session_state<S>(
    workspace: &mut Workspace,
    events: &mut S,
    host: &HostConfig,
    session_id: &SessionId,
    state: protocol::SessionState,
) where
    S: futures::Stream<Item = Message> + Unpin,
{
    wait_for_workspace(events, workspace, |workspace| {
        workspace
            .hosts
            .get(&host.id)
            .and_then(|view| view.sessions.get(&session_id.0))
            .map(|session| session.state)
            == Some(state)
    })
    .await;
}

async fn wait_for_workspace<S, F>(events: &mut S, workspace: &mut Workspace, mut done: F)
where
    S: futures::Stream<Item = Message> + Unpin,
    F: FnMut(&Workspace) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !done(workspace) {
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "workspace condition timed out");
        let message = tokio::time::timeout(deadline - now, events.next())
            .await
            .expect("message before deadline")
            .expect("workspace message");
        workspace.apply(message);
    }
}

async fn unused_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unused loopback");
    let addr = listener.local_addr().expect("unused local addr");
    drop(listener);
    addr
}

fn init_git_repo(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let output = std::process::Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&dir)
        .output()
        .expect("run git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for args in [
        ["config", "user.email", "test@example.com"],
        ["config", "user.name", "Test"],
        ["config", "commit.gpgsign", "false"],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .expect("run git config");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .expect("run git commit");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    dir
}

fn write_file(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("path has parent")).expect("create parent dir");
    std::fs::write(path, body).expect("write file");
}

fn write_provider_action_fixture(
    repo: &Path,
    prompt_name: &str,
    action_name: &str,
    provider: &str,
    prompt_content: &str,
) {
    let agent = if provider == "github_pr" {
        "claude"
    } else {
        "codex"
    };
    write_file(
        &repo.join(".pohunek/templates.toml"),
        &format!(
            r#"
[template.{prompt_name}]
agent = "{agent}"
prompt = "{prompt_name}"
base_branch = "develop"
"#
        ),
    );
    write_file(
        &repo.join(".pohunek/actions.toml"),
        &format!(
            r#"
[action.{action_name}]
template = "{prompt_name}"
provider = "{provider}"
"#
        ),
    );
    write_file(
        &repo.join(format!(".pohunek/prompts/{prompt_name}.tmpl")),
        prompt_content,
    );
}

fn expected_linear_link_metadata() -> pohunek_gui_core::SessionLinkMetadata {
    pohunek_gui_core::SessionLinkMetadata {
        provider: SessionLinkProvider::Linear,
        kind: SessionLinkKind::Issue,
        id: "LIN-123".to_owned(),
        url: "https://linear.test/LIN-123".to_owned(),
        branch: "lin-123-fix-launcher".to_owned(),
    }
}

async fn wait_for_file(path: &Path) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::read_to_string(path) {
            Ok(value) => return value,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => panic!("failed to read {}: {err}", path.display()),
        }
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "file {} was not written before deadline",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn report_native_id(host: &HostConfig, id: &SessionId, agent: &str, native_id: &str) {
    let mut client = client(host).await;
    let request = Request::new(
        "gui-core-report-native-id",
        method::SESSION_REPORT_NATIVE_ID,
        serde_json::to_value(SessionReportNativeIdParams {
            session_id: id.clone(),
            agent: agent.to_owned(),
            native_session_id: native_id.to_owned(),
            transcript_path: None,
        })
        .expect("serialize native id params"),
    );
    let _ = client
        .request(&request)
        .await
        .expect("session.report_native_id");
}

async fn wait_for_native_id_tcp(host: &HostConfig, id: &SessionId, native_id: &str) -> SessionInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let inspected = inspect_session(host, id).await.expect("inspect native id");
        if inspected.native_session_id.as_deref() == Some(native_id) {
            return inspected;
        }
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "native id was not captured before deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_session_with_metadata(
    host: &HostConfig,
    id: &SessionId,
    metadata: &std::collections::BTreeMap<String, String>,
) -> SessionInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = load_host_snapshot(host)
            .await
            .expect("snapshot while waiting for resumed session")
            .sessions;
        if let Some(session) = sessions
            .into_iter()
            .find(|session| session.id == *id && session.metadata == *metadata)
        {
            return session;
        }
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "resumed session metadata was not visible before deadline"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn create_agent_session(host: &HostConfig, agent: AgentKind, cwd: PathBuf) -> SessionInfo {
    let mut client = client(host).await;
    let request = Request::new(
        "gui-core-session-new",
        method::SESSION_NEW,
        serde_json::to_value(SessionNewParams {
            agent: agent_name(agent).to_owned(),
            name: None,
            cwd: Some(cwd),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: std::collections::BTreeMap::new(),
        })
        .expect("serialize session.new params"),
    );
    serde_json::from_value(client.request(&request).await.expect("session.new"))
        .expect("session info")
}

async fn stop_session(host: &HostConfig, id: &SessionId) {
    let mut client = client(host).await;
    let request = Request::new(
        "gui-core-session-stop",
        method::SESSION_STOP,
        serde_json::to_value(id).expect("serialize session id"),
    );
    let _ = client.request(&request).await.expect("session.stop");
}

async fn client(host: &HostConfig) -> Client {
    match host.transport {
        pohunek_gui_core::HostTransport::Tcp { addr } => {
            Client::connect_tcp_addr(host.id.as_str(), addr)
                .await
                .expect("connect tcp")
        }
        pohunek_gui_core::HostTransport::Local { .. } => {
            panic!("loopback harness expects TCP hosts")
        }
        pohunek_gui_core::HostTransport::Remote { .. } => {
            panic!("loopback harness expects direct TCP hosts")
        }
    }
}

async fn wait_for_agent_state<S>(events: &mut S, id: &SessionId) -> AgentStateEvent
where
    S: futures::Stream<Item = Message> + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let now = tokio::time::Instant::now();
        assert!(now < deadline, "agent_state event timed out");
        let message = tokio::time::timeout(deadline - now, events.next())
            .await
            .expect("event before deadline")
            .expect("subscription message");
        if let Message::HostEvent {
            event: HostEvent::AgentState(state),
            ..
        } = message
        {
            if state.session_id == *id {
                return state;
            }
        }
    }
}

fn agent_name(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-test-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod executable");
    }
}

struct PathGuard {
    old_path: Option<OsString>,
}

struct EnvGuard {
    key: &'static str,
    old_value: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old_value = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, old_value }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old_value {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

impl PathGuard {
    fn prepend(path: &Path) -> Self {
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![path.to_path_buf()];
        if let Some(old_path) = &old_path {
            paths.extend(std::env::split_paths(old_path));
        }
        let joined = std::env::join_paths(paths).expect("join PATH");
        std::env::set_var("PATH", joined);
        Self { old_path }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }
}
