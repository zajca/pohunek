//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::{stream, StreamExt};
use pohunek_client::{Client, ClientOptions};
use protocol::{
    event, method, AgentActivity, Event, HostClass, HostDiscoverParams, HostRecord, ProjectInfo,
    ProtocolVersion, Request, SessionId, SessionInfo, StateSource,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use pohunek_prompt::{
    render as render_prompt, Error as PromptError, Provider as PromptProvider,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const UI_STATE_FILE: &str = "ui-state.toml";
const DEFAULT_LEFT_PANE_WIDTH: u16 = 280;
const DEFAULT_AGENTS_PANE_HEIGHT: u16 = 220;
const DEFAULT_WINDOW_WIDTH: u32 = 960;
const DEFAULT_WINDOW_HEIGHT: u32 = 640;

/// Connection and reconciliation timing for host workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub reconcile_interval: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reconcile_interval: DEFAULT_RECONCILE_INTERVAL,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
        }
    }
}

impl ConnectionOptions {
    fn client(self) -> ClientOptions {
        ClientOptions::default()
            .with_connect_timeout(self.connect_timeout)
            .with_request_timeout(self.request_timeout)
    }
}

/// Stable host key used by the GUI state and Iced subscription identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    /// Construct a host id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the stable host id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// SDK transport target for one host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HostTransport {
    /// Local daemon over Unix socket.
    Local { socket_path: PathBuf },
    /// Remote daemon resolved by the SDK through `NetBird`.
    Remote { host: String, socket_path: PathBuf },
    /// Remote daemon over a concrete TCP address.
    Tcp { addr: SocketAddr },
}

/// Static connection config for one host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostConfig {
    pub id: HostId,
    pub transport: HostTransport,
}

impl HostConfig {
    /// Build a local Unix-socket host config.
    #[must_use]
    pub fn local(id: impl Into<String>, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Local {
                socket_path: socket_path.into(),
            },
        }
    }

    /// Build a TCP host config.
    #[must_use]
    pub fn tcp(id: impl Into<String>, addr: SocketAddr) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Tcp { addr },
        }
    }

    /// Build a remote host config resolved by the SDK.
    #[must_use]
    pub fn remote(
        id: impl Into<String>,
        host: impl Into<String>,
        socket_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Remote {
                host: host.into(),
                socket_path: socket_path.into(),
            },
        }
    }

    /// Value substituted into `{host}` for attach commands.
    #[must_use]
    pub fn attach_host(&self) -> &str {
        match &self.transport {
            HostTransport::Local { .. } => "",
            HostTransport::Remote { host, .. } => host,
            HostTransport::Tcp { .. } => self.id.as_str(),
        }
    }
}

/// Minimal daemon health facts used by the spike UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSummary {
    pub status: String,
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
}

/// A host snapshot seeded by `daemon.health` and `session.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    pub host_id: HostId,
    pub health: HealthSummary,
    pub sessions: Vec<SessionInfo>,
    pub projects: Vec<ProjectInfo>,
    pub project_error: Option<String>,
}

/// Parsed `agent_state` event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStateEvent {
    pub session_id: SessionId,
    pub activity: AgentActivity,
    pub source: StateSource,
    pub raw: Event,
}

/// Protocol events surfaced by a host subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    AgentState(AgentStateEvent),
    SessionCreated(SessionInfo),
    SessionUpdated(SessionInfo),
    SessionStopped(SessionInfo),
    Other(Event),
}

/// Message emitted by async host workers and applied to [`Workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "GUI worker messages carry protocol snapshots and live events by value across stream boundaries"
)]
pub enum Message {
    HostConnecting { host_id: HostId },
    HostSnapshotLoaded { snapshot: HostSnapshot },
    HostSubscribed { host_id: HostId },
    HostEvent { host_id: HostId, event: HostEvent },
    HostDisconnected { host_id: HostId, error: String },
    HostUnreachable { host_id: HostId, error: String },
}

/// Per-host connection state for the headless workspace model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    Unreachable,
}

/// Active detail selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    Host {
        host_id: HostId,
    },
    Project {
        host_id: HostId,
        project_id: String,
    },
    Session {
        host_id: HostId,
        session_id: SessionId,
    },
}

/// Persisted expanded workspace tree node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNodeId {
    Host { host_id: HostId },
    Project { host_id: HostId, project_id: String },
}

impl TreeNodeId {
    /// Construct a host node id.
    #[must_use]
    pub fn host(host_id: HostId) -> Self {
        Self::Host { host_id }
    }

    /// Construct a project node id.
    #[must_use]
    pub fn project(host_id: HostId, project_id: impl Into<String>) -> Self {
        Self::Project {
            host_id,
            project_id: project_id.into(),
        }
    }
}

/// Detail tabs restored by the GUI shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailTab {
    Session,
    Agents,
    Project,
}

/// Persisted window dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

/// Persisted UI layout and selection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiState {
    pub left_pane_width: u16,
    pub agents_pane_height: u16,
    pub window_size: WindowSize,
    pub expanded_nodes: BTreeSet<TreeNodeId>,
    pub selection: Option<Selection>,
    pub open_tabs: Vec<DetailTab>,
    pub active_tab: DetailTab,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            left_pane_width: DEFAULT_LEFT_PANE_WIDTH,
            agents_pane_height: DEFAULT_AGENTS_PANE_HEIGHT,
            window_size: WindowSize {
                width: DEFAULT_WINDOW_WIDTH,
                height: DEFAULT_WINDOW_HEIGHT,
            },
            expanded_nodes: BTreeSet::new(),
            selection: None,
            open_tabs: vec![DetailTab::Session, DetailTab::Agents],
            active_tab: DetailTab::Session,
        }
    }
}

impl UiState {
    /// Load persisted UI state from `dir`.
    ///
    /// A missing state file restores defaults; malformed state returns an error
    /// so the shell can surface it instead of silently discarding operator state.
    pub fn load_from_dir(dir: impl AsRef<std::path::Path>) -> Result<Self, UiStateError> {
        let path = dir.as_ref().join(UI_STATE_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => Ok(toml::from_str(&raw).map_err(|source| UiStateError::Parse {
                path: path.clone(),
                source,
            })?),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(UiStateError::Read { path, source }),
        }
    }

    /// Save UI state to `dir`.
    pub fn save_to_dir(&self, dir: impl AsRef<std::path::Path>) -> Result<(), UiStateError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|source| UiStateError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(UI_STATE_FILE);
        let raw = toml::to_string_pretty(self).map_err(UiStateError::Serialize)?;
        std::fs::write(&path, raw).map_err(|source| UiStateError::Write { path, source })
    }
}

/// Errors raised while loading or saving persistent UI state.
#[derive(Debug, Error)]
pub enum UiStateError {
    #[error("failed to create UI state directory `{}`: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read UI state `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse UI state `{}`: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize UI state: {0}")]
    Serialize(toml::ser::Error),
    #[error("failed to write UI state `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: &'static str },
}

/// Return the default XDG state directory for `pohunek-gui`.
pub fn default_state_dir() -> Result<PathBuf, UiStateError> {
    if let Ok(value) = std::env::var("XDG_STATE_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join("pohunek-gui"));
        }
    }
    match std::env::var("HOME") {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)
            .join(".local")
            .join("state")
            .join("pohunek-gui")),
        _ => Err(UiStateError::MissingEnv { var: "HOME" }),
    }
}

/// OS notification requested by core state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationIntent {
    pub id: u64,
    pub host_id: HostId,
    pub session_id: SessionId,
    pub title: String,
    pub body: String,
}

/// In-app notification requested by core state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub host_id: HostId,
    pub session_id: SessionId,
    pub message: String,
}

/// GUI-facing state for one daemon host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub conn: ConnState,
    pub health: Option<HealthSummary>,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub projects: BTreeMap<String, ProjectInfo>,
    pub last_agent_state: Option<AgentStateEvent>,
    pub last_error: Option<String>,
}

impl HostView {
    fn connecting() -> Self {
        Self {
            conn: ConnState::Connecting,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            last_agent_state: None,
            last_error: None,
        }
    }
}

/// Headless workspace model owned by `gui-core`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub hosts: BTreeMap<HostId, HostView>,
    pub selection: Option<Selection>,
    pub notification_intents: Vec<NotificationIntent>,
    pub toasts: Vec<Toast>,
    next_intent_id: u64,
}

impl Workspace {
    /// Apply one async message to the workspace state.
    pub fn apply(&mut self, message: Message) {
        match message {
            Message::HostConnecting { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connecting;
                        host.last_error = None;
                    })
                    .or_insert_with(HostView::connecting);
            }
            Message::HostSnapshotLoaded { snapshot } => {
                let host_id = snapshot.host_id.clone();
                let previous_sessions = self.hosts.get(&host_id).map(|host| host.sessions.clone());
                if let Some(previous_sessions) = &previous_sessions {
                    for session in &snapshot.sessions {
                        let previous = previous_sessions
                            .get(&session.id.0)
                            .and_then(|existing| existing.activity);
                        push_blocked_effects(
                            previous,
                            session,
                            &host_id,
                            &mut self.notification_intents,
                            &mut self.toasts,
                            &mut self.next_intent_id,
                        );
                    }
                }
                let sessions = snapshot
                    .sessions
                    .iter()
                    .cloned()
                    .map(|session| (session.id.0.clone(), session))
                    .collect();
                let projects = snapshot
                    .projects
                    .iter()
                    .cloned()
                    .map(|project| (project.id.clone(), project))
                    .collect();
                self.hosts.insert(
                    snapshot.host_id,
                    HostView {
                        conn: ConnState::Connected,
                        health: Some(snapshot.health),
                        sessions,
                        projects,
                        last_agent_state: None,
                        last_error: snapshot.project_error,
                    },
                );
            }
            Message::HostSubscribed { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connected;
                        host.last_error = None;
                    })
                    .or_insert_with(|| HostView {
                        conn: ConnState::Connected,
                        ..HostView::connecting()
                    });
            }
            Message::HostEvent { host_id, event } => {
                let host = self
                    .hosts
                    .entry(host_id.clone())
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Connected;
                host.last_error = None;
                apply_host_event(
                    host,
                    &host_id,
                    event,
                    &mut self.notification_intents,
                    &mut self.toasts,
                    &mut self.next_intent_id,
                );
            }
            Message::HostDisconnected { host_id, error } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Disconnected;
                host.last_error = Some(error);
            }
            Message::HostUnreachable { host_id, error } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Unreachable;
                host.last_error = Some(error);
            }
        }
    }

    /// Select a session in the detail pane.
    pub fn select_session(&mut self, host_id: HostId, session_id: SessionId) {
        self.selection = Some(Selection::Session {
            host_id,
            session_id,
        });
    }

    /// Build the agents monitor model from current host state.
    #[must_use]
    pub fn agent_monitor(&self) -> AgentMonitor {
        let mut monitor = AgentMonitor::default();
        for (host_id, host) in &self.hosts {
            for session in host.sessions.values() {
                match session.activity {
                    Some(AgentActivity::Blocked) => monitor.blocked += 1,
                    Some(AgentActivity::Working) => monitor.working += 1,
                    Some(AgentActivity::Idle) => monitor.idle += 1,
                    None => monitor.unknown += 1,
                }
                monitor.sessions.push(AgentRow {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                    project_id: session.project_id.clone(),
                    project_label: session.project_label.clone(),
                    agent: session.agent.clone(),
                    activity: session.activity,
                    state: session.state.as_str().to_owned(),
                });
            }
        }
        monitor.sessions.sort_by(|left, right| {
            activity_rank(left.activity)
                .cmp(&activity_rank(right.activity))
                .then_with(|| left.host_id.cmp(&right.host_id))
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        monitor
    }
}

/// Derived row for the flat agents monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub host_id: HostId,
    pub session_id: SessionId,
    pub project_id: Option<String>,
    pub project_label: Option<String>,
    pub agent: String,
    pub activity: Option<AgentActivity>,
    pub state: String,
}

/// Derived agents monitor counts and rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMonitor {
    pub blocked: usize,
    pub working: usize,
    pub idle: usize,
    pub unknown: usize,
    pub sessions: Vec<AgentRow>,
}

fn activity_rank(activity: Option<AgentActivity>) -> u8 {
    match activity {
        Some(AgentActivity::Blocked) => 0,
        Some(AgentActivity::Working) => 1,
        Some(AgentActivity::Idle) => 2,
        None => 3,
    }
}

fn apply_host_event(
    host: &mut HostView,
    host_id: &HostId,
    event: HostEvent,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    match event {
        HostEvent::AgentState(state) => {
            if let Some(session) = host.sessions.get_mut(&state.session_id.0) {
                let previous = session.activity;
                session.activity = Some(state.activity);
                session.state_source = state.source;
                push_blocked_effects(
                    previous,
                    session,
                    host_id,
                    notifications,
                    toasts,
                    next_intent_id,
                );
            }
            host.last_agent_state = Some(state);
        }
        HostEvent::SessionCreated(session)
        | HostEvent::SessionUpdated(session)
        | HostEvent::SessionStopped(session) => {
            let previous = host
                .sessions
                .get(&session.id.0)
                .and_then(|existing| existing.activity);
            push_blocked_effects(
                previous,
                &session,
                host_id,
                notifications,
                toasts,
                next_intent_id,
            );
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::Other(_) => {}
    }
}

fn push_blocked_effects(
    previous: Option<AgentActivity>,
    session: &SessionInfo,
    host_id: &HostId,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    if previous == Some(AgentActivity::Blocked) || session.activity != Some(AgentActivity::Blocked)
    {
        return;
    }
    let id = *next_intent_id;
    *next_intent_id += 1;
    let title = "Agent blocked".to_owned();
    let body = format!(
        "{} on {} is waiting for input",
        session.agent,
        host_id.as_str()
    );
    notifications.push(NotificationIntent {
        id,
        host_id: host_id.clone(),
        session_id: session.id.clone(),
        title,
        body: body.clone(),
    });
    toasts.push(Toast {
        id,
        host_id: host_id.clone(),
        session_id: session.id.clone(),
        message: body,
    });
}

/// Errors raised by the GUI core bridge.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Client(#[from] pohunek_client::ClientError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("agent_state event is missing `{field}`")]
    MissingAgentStateField { field: &'static str },
    #[error("session event is missing `session`")]
    MissingSessionEventPayload,
    #[error("host discovery record does not contain a usable host name")]
    MissingDiscoveredHostName,
}

/// Load one host snapshot with `daemon.health` and `session.list`.
pub async fn load_host(config: HostConfig) -> Message {
    match load_host_snapshot(&config).await {
        Ok(snapshot) => Message::HostSnapshotLoaded { snapshot },
        Err(err) => Message::HostDisconnected {
            host_id: config.id,
            error: err.to_string(),
        },
    }
}

/// Load one host snapshot and return typed data for headless tests.
pub async fn load_host_snapshot(config: &HostConfig) -> Result<HostSnapshot, CoreError> {
    load_host_snapshot_with_options(config, ConnectionOptions::default()).await
}

async fn load_host_snapshot_with_options(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<HostSnapshot, CoreError> {
    let mut client = connect_client(config, options).await?;
    let health = request_json::<HealthSummary>(
        &mut client,
        "gui-daemon-health",
        method::DAEMON_HEALTH,
        Value::Null,
    )
    .await?;
    let sessions = request_json::<Vec<SessionInfo>>(
        &mut client,
        "gui-session-list",
        method::SESSION_LIST,
        Value::Null,
    )
    .await?;
    let projects = match request_json::<Vec<ProjectInfo>>(
        &mut client,
        "gui-project-list",
        method::PROJECT_LIST,
        Value::Null,
    )
    .await
    {
        Ok(projects) => (projects, None),
        Err(err) => (Vec::new(), Some(format!("project.list failed: {err}"))),
    };
    Ok(HostSnapshot {
        host_id: config.id.clone(),
        health,
        sessions,
        projects: projects.0,
        project_error: projects.1,
    })
}

async fn request_json<T>(
    client: &mut Client,
    id: &'static str,
    method: &'static str,
    params: Value,
) -> Result<T, CoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value = client.request(&Request::new(id, method, params)).await?;
    Ok(serde_json::from_value(value)?)
}

/// Build a reconnecting stream of messages for one host's event subscription.
pub fn host_subscription_stream(config: HostConfig) -> impl futures::Stream<Item = Message> {
    host_connection_stream(config, ConnectionOptions::default())
}

/// Build one reconnecting stream for every host and merge their messages.
pub fn workspace_connection_stream(
    hosts: Vec<HostConfig>,
    options: ConnectionOptions,
) -> impl futures::Stream<Item = Message> {
    stream::select_all(
        hosts
            .into_iter()
            .map(|host| host_connection_stream(host, options).boxed())
            .collect::<Vec<_>>(),
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "the reconnecting host worker is a single explicit async state machine"
)]
fn host_connection_stream(
    config: HostConfig,
    options: ConnectionOptions,
) -> impl futures::Stream<Item = Message> {
    stream::unfold(
        StreamState::Connecting {
            config,
            backoff: Backoff::new(options),
        },
        move |state| async move {
            match state {
                StreamState::Connecting { config, backoff } => {
                    let next = StreamState::Subscribing {
                        config: config.clone(),
                        backoff,
                    };
                    Some((
                        Message::HostConnecting {
                            host_id: config.id.clone(),
                        },
                        next,
                    ))
                }
                StreamState::Subscribing { config, backoff } => {
                    match subscribe_events(&config, options).await {
                        Ok(subscription) => Some((
                            Message::HostSubscribed {
                                host_id: config.id.clone(),
                            },
                            StreamState::LoadingSnapshot {
                                config,
                                subscription: Box::new(subscription),
                            },
                        )),
                        Err(err) => Some((
                            Message::HostUnreachable {
                                host_id: config.id.clone(),
                                error: err.to_string(),
                            },
                            StreamState::Waiting { config, backoff },
                        )),
                    }
                }
                StreamState::LoadingSnapshot {
                    config,
                    subscription,
                } => match load_host_snapshot_with_options(&config, options).await {
                    Ok(snapshot) => Some((
                        Message::HostSnapshotLoaded { snapshot },
                        StreamState::Reading {
                            config,
                            subscription,
                            interval: reconcile_interval(options.reconcile_interval),
                            backoff: Backoff::new(options),
                        },
                    )),
                    Err(err) => Some((
                        Message::HostDisconnected {
                            host_id: config.id.clone(),
                            error: err.to_string(),
                        },
                        StreamState::Waiting {
                            config,
                            backoff: Backoff::new(options),
                        },
                    )),
                },
                StreamState::Reading {
                    config,
                    mut subscription,
                    mut interval,
                    backoff,
                } => {
                    tokio::select! {
                        line = subscription.next_line() => {
                            match line {
                                Ok(Some(line)) => {
                                    let message = parse_event_message(&config.id, &line);
                                    Some((
                                        message.unwrap_or_else(|err| Message::HostDisconnected {
                                            host_id: config.id.clone(),
                                            error: err.to_string(),
                                        }),
                                        StreamState::Reading {
                                            config,
                                            subscription,
                                            interval,
                                            backoff,
                                        },
                                    ))
                                }
                                Ok(None) => Some((
                                    Message::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: "event subscription closed".to_owned(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                                Err(err) => Some((
                                    Message::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: err.to_string(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                            }
                        }
                        _ = interval.tick() => {
                            let message = match load_host_snapshot_with_options(&config, options).await {
                                Ok(snapshot) => Message::HostSnapshotLoaded { snapshot },
                                Err(err) => Message::HostDisconnected {
                                    host_id: config.id.clone(),
                                    error: err.to_string(),
                                },
                            };
                            Some((
                                message,
                                StreamState::Reading {
                                    config,
                                    subscription,
                                    interval,
                                    backoff,
                                },
                            ))
                        }
                    }
                }
                StreamState::Waiting {
                    config,
                    mut backoff,
                } => {
                    tokio::time::sleep(backoff.current).await;
                    backoff.advance();
                    Some((
                        Message::HostConnecting {
                            host_id: config.id.clone(),
                        },
                        StreamState::Subscribing { config, backoff },
                    ))
                }
            }
        },
    )
}

#[derive(Debug)]
enum StreamState {
    Connecting {
        config: HostConfig,
        backoff: Backoff,
    },
    Subscribing {
        config: HostConfig,
        backoff: Backoff,
    },
    LoadingSnapshot {
        config: HostConfig,
        subscription: Box<pohunek_client::Subscription>,
    },
    Reading {
        config: HostConfig,
        subscription: Box<pohunek_client::Subscription>,
        interval: tokio::time::Interval,
        backoff: Backoff,
    },
    Waiting {
        config: HostConfig,
        backoff: Backoff,
    },
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    current: Duration,
    max: Duration,
}

impl Backoff {
    fn new(options: ConnectionOptions) -> Self {
        Self {
            current: options.backoff_initial,
            max: options.backoff_max,
        }
    }

    fn advance(&mut self) {
        self.current = self.current.saturating_mul(2).min(self.max);
    }
}

fn reconcile_interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + period, period)
}

async fn subscribe_events(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<pohunek_client::Subscription, CoreError> {
    let client = connect_client(config, options).await?;
    let request = Request::new("gui-subscribe", method::SUBSCRIBE, Value::Null);
    Ok(client.subscribe(&request).await?)
}

async fn connect_client(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<Client, CoreError> {
    let options = options.client();
    match &config.transport {
        HostTransport::Local { socket_path } => {
            Ok(Client::connect_local_with_options(socket_path, options).await?)
        }
        HostTransport::Remote { host, socket_path } => {
            Ok(Client::connect_with_options(host, socket_path, options).await?)
        }
        HostTransport::Tcp { addr } => {
            Ok(Client::connect_tcp_addr_with_options(config.id.as_str(), *addr, options).await?)
        }
    }
}

fn parse_event_message(host_id: &HostId, line: &str) -> Result<Message, CoreError> {
    let raw: Event = serde_json::from_str(line)?;
    let event = match raw.event.as_str() {
        event::AGENT_STATE => HostEvent::AgentState(parse_agent_state(raw)?),
        event::SESSION_CREATED => HostEvent::SessionCreated(parse_session_event(&raw)?),
        event::SESSION_UPDATED => HostEvent::SessionUpdated(parse_session_event(&raw)?),
        event::SESSION_STOPPED => HostEvent::SessionStopped(parse_session_event(&raw)?),
        _ => HostEvent::Other(raw),
    };
    Ok(Message::HostEvent {
        host_id: host_id.clone(),
        event,
    })
}

fn parse_agent_state(raw: Event) -> Result<AgentStateEvent, CoreError> {
    let session_id = required_str(&raw.payload, "session_id")?;
    let activity = required_typed::<AgentActivity>(&raw.payload, "activity")?;
    let source = required_typed::<StateSource>(&raw.payload, "source")?;
    Ok(AgentStateEvent {
        session_id: SessionId(session_id.to_owned()),
        activity,
        source,
        raw,
    })
}

fn parse_session_event(raw: &Event) -> Result<SessionInfo, CoreError> {
    let session = raw
        .payload
        .get("session")
        .ok_or(CoreError::MissingSessionEventPayload)?;
    Ok(serde_json::from_value(session.clone())?)
}

fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, CoreError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CoreError::MissingAgentStateField { field })
}

/// Discover reachable remote hosts through the local daemon and include local.
pub async fn discover_hosts(
    local: HostConfig,
    options: ConnectionOptions,
) -> Result<Vec<HostConfig>, CoreError> {
    let mut client = connect_client(&local, options).await?;
    let records = request_json::<Vec<HostRecord>>(
        &mut client,
        "gui-host-discover",
        method::HOST_DISCOVER,
        serde_json::to_value(HostDiscoverParams { force: false })?,
    )
    .await?;
    let mut hosts = vec![local.clone()];
    for record in records {
        if matches!(record.class, HostClass::ReachableDaemon { .. }) {
            let host = discovered_host_name(&record)?;
            let id = record
                .name
                .clone()
                .or_else(|| record.fqdn.clone())
                .unwrap_or_else(|| host.clone());
            let socket_path = match &local.transport {
                HostTransport::Local { socket_path }
                | HostTransport::Remote { socket_path, .. } => socket_path.clone(),
                HostTransport::Tcp { .. } => PathBuf::new(),
            };
            hosts.push(HostConfig::remote(id, host, socket_path));
        }
    }
    Ok(hosts)
}

fn discovered_host_name(record: &HostRecord) -> Result<String, CoreError> {
    record
        .name
        .clone()
        .or_else(|| record.fqdn.clone())
        .or_else(|| record.netbird_ip.clone())
        .ok_or(CoreError::MissingDiscoveredHostName)
}

fn required_typed<T>(value: &Value, field: &'static str) -> Result<T, CoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value = value
        .get(field)
        .ok_or(CoreError::MissingAgentStateField { field })?;
    Ok(serde_json::from_value(value.clone())?)
}

/// Values filled into an attach command template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTemplateValues {
    pub bin: String,
    pub host: String,
    pub id: String,
}

/// Render the configured attach command by replacing `{bin}`, `{host}`, and `{id}`.
#[must_use]
pub fn render_attach_command(template: &str, values: &AttachTemplateValues) -> String {
    template
        .replace("{bin}", &values.bin)
        .replace("{host}", &values.host)
        .replace("{id}", &values.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_applies_agent_state_to_known_session() {
        let mut workspace = Workspace::default();
        let session = SessionInfo {
            id: SessionId("s-1".to_owned()),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/repo"),
            pid: 42,
            cols: 80,
            rows: 24,
            state: protocol::SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            native_session_id: None,
            native_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            exit_code: None,
        };
        workspace.apply(Message::HostSnapshotLoaded {
            snapshot: HostSnapshot {
                host_id: HostId::new("local"),
                health: HealthSummary {
                    status: "ok".to_owned(),
                    daemon_version: "0.0.0".to_owned(),
                    protocol_version: protocol::PROTOCOL_VERSION,
                },
                sessions: vec![session],
                projects: Vec::new(),
                project_error: None,
            },
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "blocked",
                "source": "osc_title"
            }),
        );
        workspace.apply(Message::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(
            host.sessions
                .get("s-1")
                .and_then(|session| session.activity),
            Some(AgentActivity::Blocked)
        );
        assert_eq!(
            host.last_agent_state
                .as_ref()
                .map(|event| event.session_id.0.as_str()),
            Some("s-1")
        );
    }

    #[test]
    fn attach_command_replaces_declared_tokens() {
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: "devbox".to_owned(),
                id: "s-7".to_owned(),
            },
        );

        assert_eq!(command, "pohunek attach --host devbox s-7");
    }
}
