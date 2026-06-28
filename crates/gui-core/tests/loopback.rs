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
    host_subscription_stream, load_host_snapshot, AgentStateEvent, HostConfig, HostEvent, Message,
};
use protocol::{
    method, AgentActivity, AgentKind, Request, SessionId, SessionInfo, SessionNewParams,
    StateSource,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn loopback_hosts_seed_and_stream_agent_state() {
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
