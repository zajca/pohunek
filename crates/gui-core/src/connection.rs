//! Reconnecting host transport: event streams, wire parsing, and attach spawn.

use std::time::Duration;

use futures::{stream, StreamExt};
use pohunek_client::{next_request_id, Client};
use protocol::{
    event, method, AgentActivity, Event, HostClass, HostDiscoverParams, HostRecord,
    NotificationCreatedEvent, NotificationDeletedEvent, NotificationId, NotificationRecord,
    NotificationUpdatedEvent, Request, SessionId, SessionInfo, StateSource,
};
use serde_json::Value;

use crate::sdk::{call_client, load_host_snapshot_with_options};
use crate::{
    AgentStateEvent, ConnectionOptions, CoreError, DomainEvent, HostConfig, HostEvent, HostId,
    HostTransport, DEFAULT_BACKOFF_MAX,
};

/// Build a reconnecting stream of messages for one host's event subscription.
pub fn host_subscription_stream(config: HostConfig) -> impl futures::Stream<Item = DomainEvent> {
    host_connection_stream(config, ConnectionOptions::default())
}

/// Build one reconnecting stream for every host and merge their messages.
pub fn workspace_connection_stream(
    hosts: Vec<HostConfig>,
    options: ConnectionOptions,
) -> impl futures::Stream<Item = DomainEvent> {
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
) -> impl futures::Stream<Item = DomainEvent> {
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
                        DomainEvent::HostConnecting {
                            host_id: config.id.clone(),
                        },
                        next,
                    ))
                }
                StreamState::Subscribing { config, backoff } => {
                    match subscribe_events(&config, options).await {
                        Ok(subscription) => Some((
                            DomainEvent::HostSubscribed {
                                host_id: config.id.clone(),
                            },
                            StreamState::LoadingSnapshot {
                                config,
                                subscription: Box::new(subscription),
                            },
                        )),
                        Err(err) => Some((
                            DomainEvent::HostUnreachable {
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
                        DomainEvent::HostSnapshotLoaded { snapshot },
                        StreamState::Reading {
                            config,
                            subscription,
                            interval: reconcile_interval(options.reconcile_interval),
                            backoff: Backoff::new(options),
                        },
                    )),
                    Err(err) => Some((
                        DomainEvent::HostDisconnected {
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
                                        message.unwrap_or_else(|err| DomainEvent::HostDisconnected {
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
                                    DomainEvent::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: "event subscription closed".to_owned(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                                Err(err) => Some((
                                    DomainEvent::HostDisconnected {
                                        host_id: config.id.clone(),
                                        error: err.to_string(),
                                    },
                                    StreamState::Waiting { config, backoff },
                                )),
                            }
                        }
                        _ = interval.tick() => {
                            let message = match load_host_snapshot_with_options(&config, options).await {
                                Ok(snapshot) => DomainEvent::HostSnapshotLoaded { snapshot },
                                Err(err) => DomainEvent::HostDisconnected {
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
                        DomainEvent::HostConnecting {
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
pub(crate) struct Backoff {
    pub(crate) current: Duration,
    pub(crate) max: Duration,
}

impl Backoff {
    pub(crate) fn new(options: ConnectionOptions) -> Self {
        let max = options.backoff_max.min(DEFAULT_BACKOFF_MAX);
        Self {
            current: options.backoff_initial.min(max),
            max,
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
    let request = subscribe_request();
    Ok(client.subscribe(&request).await?)
}

pub(crate) fn subscribe_request() -> Request {
    Request::new(
        next_request_id(method::SUBSCRIBE),
        method::SUBSCRIBE,
        Value::Null,
    )
    .expect("the SDK request ID generator and subscribe method constant are valid")
}

pub(crate) async fn connect_client(
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

pub(crate) fn parse_event_message(host_id: &HostId, line: &str) -> Result<DomainEvent, CoreError> {
    let raw: Event = serde_json::from_str(line)?;
    let event = match raw.event() {
        event::AGENT_STATE => HostEvent::AgentState(parse_agent_state(raw)?),
        event::SESSION_CREATED => HostEvent::SessionCreated(parse_session_event(&raw)?),
        event::SESSION_UPDATED => HostEvent::SessionUpdated(parse_session_event(&raw)?),
        event::SESSION_STOPPED => HostEvent::SessionStopped(parse_session_event(&raw)?),
        event::SESSION_REMOVED => HostEvent::SessionRemoved(parse_session_event(&raw)?),
        event::SESSION_RUNTIME_RECONNECTED => {
            HostEvent::RuntimeReconnected(parse_session_event(&raw)?)
        }
        event::SESSION_RUNTIME_LOST => HostEvent::RuntimeLost(parse_session_event(&raw)?),
        event::SESSION_RUNTIME_CONFLICT => HostEvent::RuntimeConflict(parse_session_event(&raw)?),
        event::SESSION_NATIVE_RECOVERED => HostEvent::NativeRecovered(parse_session_event(&raw)?),
        event::NOTIFICATION_CREATED => {
            HostEvent::NotificationCreated(parse_notification_created(&raw)?)
        }
        event::NOTIFICATION_UPDATED => {
            HostEvent::NotificationUpdated(parse_notification_updated(&raw)?)
        }
        event::NOTIFICATION_DELETED => {
            HostEvent::NotificationDeleted(parse_notification_deleted(&raw)?)
        }
        _ => HostEvent::Other(raw),
    };
    Ok(DomainEvent::HostEvent {
        host_id: host_id.clone(),
        event,
    })
}

fn parse_notification_created(raw: &Event) -> Result<NotificationRecord, CoreError> {
    let event: NotificationCreatedEvent = serde_json::from_value(raw.payload().clone())?;
    Ok(event.record)
}

fn parse_notification_updated(raw: &Event) -> Result<NotificationRecord, CoreError> {
    let event: NotificationUpdatedEvent = serde_json::from_value(raw.payload().clone())?;
    Ok(event.record)
}

fn parse_notification_deleted(raw: &Event) -> Result<NotificationId, CoreError> {
    let event: NotificationDeletedEvent = serde_json::from_value(raw.payload().clone())?;
    Ok(event.notification_id)
}

pub(crate) fn parse_agent_state(raw: Event) -> Result<AgentStateEvent, CoreError> {
    let session_id = required_str(raw.payload(), "session_id")?;
    let activity = required_typed::<AgentActivity>(raw.payload(), "activity")?;
    let source = required_typed::<StateSource>(raw.payload(), "source")?;
    Ok(AgentStateEvent {
        session_id: SessionId(session_id.to_owned()),
        activity,
        source,
        raw,
    })
}

fn parse_session_event(raw: &Event) -> Result<SessionInfo, CoreError> {
    let session = raw
        .payload()
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
    let records =
        call_client::<method::HostDiscover>(&mut client, HostDiscoverParams { force: false })
            .await?;
    let mut hosts = vec![local.clone()];
    for record in records {
        if matches!(record.class, HostClass::ReachableDaemon { .. }) {
            let addr = discovered_transport_addr(&record)?;
            let identity = record
                .peer_id
                .as_deref()
                .or(record.fqdn.as_deref())
                .or(record.name.as_deref())
                .unwrap_or_else(|| record.address.as_deref().expect("reachable route address"));
            hosts.push(HostConfig::tcp(
                format!("{}:{identity}", record.overlay),
                addr,
            ));
        }
    }
    Ok(hosts)
}

pub(crate) fn discovered_transport_addr(
    record: &HostRecord,
) -> Result<std::net::SocketAddr, CoreError> {
    let address = record
        .address
        .clone()
        .ok_or(CoreError::MissingDiscoveredHostName)?;
    address
        .parse::<std::net::IpAddr>()
        .map(|address| std::net::SocketAddr::new(address, record.port))
        .map_err(|_| CoreError::InvalidDiscoveredAddress {
            address,
            port: record.port,
        })
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

/// Intent recorded after resolving and spawning attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSpawnIntent {
    pub command: String,
}

/// Port used by the shell to spawn external attach commands.
pub trait AttachCommandSpawner {
    /// Spawn `command` in the platform shell.
    fn spawn(&mut self, command: &str) -> Result<(), String>;
}

/// Render the configured attach command.
///
/// Replaces `{bin}`, `{host}`, and `{id}` with shell-escaped values because the
/// GUI shell spawner executes the rendered command through `sh -c`.
#[must_use]
pub fn render_attach_command(template: &str, values: &AttachTemplateValues) -> String {
    let bin = shell_escape(&values.bin);
    let host = shell_escape(&values.host);
    let id = shell_escape(&values.id);
    template
        .replace("{bin}", &bin)
        .replace("{host}", &host)
        .replace("{id}", &id)
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value.bytes().all(is_shell_safe_byte) {
        return value.to_owned();
    }

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for character in value.chars() {
        if character == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(character);
        }
    }
    escaped.push('\'');
    escaped
}

const fn is_shell_safe_byte(byte: u8) -> bool {
    // POSIX shell metacharacters are intentionally excluded from this allowlist.
    matches!(
        byte,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'_'
            | b'-'
            | b'.'
            | b'/'
            | b':'
            | b'@'
            | b'%'
            | b'+'
            | b','
            | b'='
    )
}

/// Resolve and spawn an external attach command.
pub fn spawn_attach_command<S>(
    spawner: &mut S,
    template: &str,
    values: &AttachTemplateValues,
) -> Result<AttachSpawnIntent, String>
where
    S: AttachCommandSpawner + ?Sized,
{
    let command = render_attach_command(template, values);
    spawner.spawn(&command)?;
    Ok(AttachSpawnIntent { command })
}
