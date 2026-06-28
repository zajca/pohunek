//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use futures::stream;
use pohunek_client::{Client, ClientOptions};
use protocol::{
    event, method, AgentActivity, Event, ProtocolVersion, Request, SessionId, SessionInfo,
    StateSource,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use pohunek_prompt::{
    render as render_prompt, Error as PromptError, Provider as PromptProvider,
};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

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

    /// Value substituted into `{host}` for attach commands.
    #[must_use]
    pub fn attach_host(&self) -> &str {
        match self.transport {
            HostTransport::Local { .. } => "",
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
    Other(Event),
}

/// Message emitted by async host workers and applied to [`Workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    HostConnecting { host_id: HostId },
    HostSnapshotLoaded { snapshot: HostSnapshot },
    HostSubscribed { host_id: HostId },
    HostEvent { host_id: HostId, event: HostEvent },
    HostDisconnected { host_id: HostId, error: String },
}

/// Per-host connection state for the headless workspace model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    Unreachable,
}

/// GUI-facing state for one daemon host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub conn: ConnState,
    pub health: Option<HealthSummary>,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub last_agent_state: Option<AgentStateEvent>,
    pub last_error: Option<String>,
}

impl HostView {
    fn connecting() -> Self {
        Self {
            conn: ConnState::Connecting,
            health: None,
            sessions: BTreeMap::new(),
            last_agent_state: None,
            last_error: None,
        }
    }
}

/// Headless workspace model owned by `gui-core`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub hosts: BTreeMap<HostId, HostView>,
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
                let sessions = snapshot
                    .sessions
                    .iter()
                    .cloned()
                    .map(|session| (session.id.0.clone(), session))
                    .collect();
                self.hosts.insert(
                    snapshot.host_id,
                    HostView {
                        conn: ConnState::Connected,
                        health: Some(snapshot.health),
                        sessions,
                        last_agent_state: None,
                        last_error: None,
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
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Connected;
                host.last_error = None;
                apply_host_event(host, event);
            }
            Message::HostDisconnected { host_id, error } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                host.conn = ConnState::Disconnected;
                host.last_error = Some(error);
            }
        }
    }
}

fn apply_host_event(host: &mut HostView, event: HostEvent) {
    if let HostEvent::AgentState(state) = event {
        if let Some(session) = host.sessions.get_mut(&state.session_id.0) {
            session.activity = Some(state.activity);
            session.state_source = state.source;
        }
        host.last_agent_state = Some(state);
    }
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
    let mut client = connect_client(config).await?;
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
    Ok(HostSnapshot {
        host_id: config.id.clone(),
        health,
        sessions,
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
    stream::unfold(StreamState::Connecting(config), |state| async move {
        match state {
            StreamState::Connecting(config) => {
                let next = StreamState::Subscribing(config.clone());
                Some((
                    Message::HostConnecting {
                        host_id: config.id.clone(),
                    },
                    next,
                ))
            }
            StreamState::Subscribing(config) => match subscribe_events(&config).await {
                Ok(subscription) => Some((
                    Message::HostSubscribed {
                        host_id: config.id.clone(),
                    },
                    StreamState::Reading {
                        config,
                        subscription,
                    },
                )),
                Err(err) => Some((
                    Message::HostDisconnected {
                        host_id: config.id.clone(),
                        error: err.to_string(),
                    },
                    StreamState::Waiting(config),
                )),
            },
            StreamState::Reading {
                config,
                mut subscription,
            } => match subscription.next_line().await {
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
                        },
                    ))
                }
                Ok(None) => Some((
                    Message::HostDisconnected {
                        host_id: config.id.clone(),
                        error: "event subscription closed".to_owned(),
                    },
                    StreamState::Waiting(config),
                )),
                Err(err) => Some((
                    Message::HostDisconnected {
                        host_id: config.id.clone(),
                        error: err.to_string(),
                    },
                    StreamState::Waiting(config),
                )),
            },
            StreamState::Waiting(config) => {
                tokio::time::sleep(RECONNECT_DELAY).await;
                Some((
                    Message::HostConnecting {
                        host_id: config.id.clone(),
                    },
                    StreamState::Subscribing(config),
                ))
            }
        }
    })
}

#[derive(Debug)]
enum StreamState {
    Connecting(HostConfig),
    Subscribing(HostConfig),
    Reading {
        config: HostConfig,
        subscription: pohunek_client::Subscription,
    },
    Waiting(HostConfig),
}

async fn subscribe_events(config: &HostConfig) -> Result<pohunek_client::Subscription, CoreError> {
    let client = connect_client(config).await?;
    let request = Request::new("gui-subscribe", method::SUBSCRIBE, Value::Null);
    Ok(client.subscribe(&request).await?)
}

async fn connect_client(config: &HostConfig) -> Result<Client, CoreError> {
    let options = ClientOptions::default();
    match &config.transport {
        HostTransport::Local { socket_path } => {
            Ok(Client::connect_local_with_options(socket_path, options).await?)
        }
        HostTransport::Tcp { addr } => {
            Ok(Client::connect_tcp_addr_with_options(config.id.as_str(), *addr, options).await?)
        }
    }
}

fn parse_event_message(host_id: &HostId, line: &str) -> Result<Message, CoreError> {
    let raw: Event = serde_json::from_str(line)?;
    let event = if raw.event == event::AGENT_STATE {
        HostEvent::AgentState(parse_agent_state(raw)?)
    } else {
        HostEvent::Other(raw)
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

fn required_str<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, CoreError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CoreError::MissingAgentStateField { field })
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
