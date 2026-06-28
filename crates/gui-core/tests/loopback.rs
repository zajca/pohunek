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
use pohunek_gui_core::{
    host_subscription_stream, load_host_snapshot, workspace_connection_stream, AgentStateEvent,
    ConnState, ConnectionOptions, DetailTab, HostConfig, HostEvent, Message, Selection, TreeNodeId,
    UiState, WindowSize, Workspace,
};
use protocol::{
    method, AgentActivity, AgentKind, Request, SessionId, SessionInfo, SessionNewParams,
    StateSource,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

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
async fn live_agent_state_updates_are_reflected_and_emit_blocked_intent() {
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
    assert_eq!(workspace.notification_intents.len(), 1);
    assert_eq!(workspace.toasts.len(), 1);
    assert_eq!(workspace.notification_intents[0].host_id, host.id);
    assert_eq!(workspace.notification_intents[0].session_id, session.id);

    stop_session(&host, &session.id).await;
    daemon.shutdown().await;
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

#[test]
fn ui_state_persists_and_restores() {
    let state_dir = temp_dir("gui-core-m1-ui-state");
    let mut expanded = std::collections::BTreeSet::new();
    let host = HostConfig::tcp("host-a", "127.0.0.1:65535".parse().expect("addr"));
    expanded.insert(TreeNodeId::host(host.id.clone()));
    expanded.insert(TreeNodeId::project(host.id.clone(), "p-1"));
    let state = UiState {
        left_pane_width: 312,
        agents_pane_height: 180,
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

struct LoopbackDaemon {
    addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl LoopbackDaemon {
    async fn spawn(tag: &str, version: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let addr = listener.local_addr().expect("local addr");
        let state = DaemonState::new(
            HealthInfo::new(version),
            SessionRegistry::new(SessionRegistryConfig {
                stop_grace: Duration::from_millis(50),
                store_path: Some(temp_dir(&format!("{tag}-state")).join("metadata.jsonl")),
                ..SessionRegistryConfig::default()
            }),
        );
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
    let status = std::process::Command::new("git")
        .arg("init")
        .arg(&dir)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");
    dir
}

async fn create_agent_session(host: &HostConfig, agent: AgentKind, cwd: PathBuf) -> SessionInfo {
    let mut client = client(host).await;
    let request = Request::new(
        "gui-core-session-new",
        method::SESSION_NEW,
        serde_json::to_value(SessionNewParams {
            agent: agent_name(agent).to_owned(),
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
