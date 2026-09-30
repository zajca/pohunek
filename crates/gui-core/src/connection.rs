//! Reconnecting host transport: event streams, wire parsing, and attach spawn.

// Rust guideline compliant 2026-09-30

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
        HostTransport::Tcp { addr, .. } => {
            Ok(
                Client::connect_trusted_tcp_addr_with_options(config.id.as_str(), *addr, options)
                    .await?,
            )
        }
    }
}

pub(crate) fn parse_event_message(host_id: &HostId, line: &str) -> Result<DomainEvent, CoreError> {
    let raw: Event = serde_json::from_str(line)?;
    let event = match raw.event() {
        event::AGENT_STATE => HostEvent::AgentState(parse_agent_state(raw)?),
        event::SUBAGENT_STATE => {
            HostEvent::SubagentState(serde_json::from_value(raw.payload().clone())?)
        }
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
            hosts.push(discovered_host_config(&record)?);
        }
    }
    Ok(hosts)
}

pub(crate) fn discovered_host_config(record: &HostRecord) -> Result<HostConfig, CoreError> {
    discovered_transport_addr(record)?;
    let identity = if let Some(peer_id) =
        record.peer_id.as_deref().filter(|value| !value.is_empty())
    {
        pohunek_client::ExternalIdentity::peer_id(peer_id)
            .map_err(pohunek_client::ClientError::from)?
    } else if let Some(fqdn) = record.fqdn.as_deref().filter(|value| !value.is_empty()) {
        pohunek_client::ExternalIdentity::fqdn(fqdn).map_err(pohunek_client::ClientError::from)?
    } else {
        return Err(CoreError::MissingDiscoveredStableIdentity);
    };
    let selector = format!("{}:{}", record.overlay, identity.selector());
    let route = pohunek_client::remote_host_with_port(&selector, record.port)?;
    Ok(HostConfig::remote(selector, route, ""))
}

pub(crate) fn discovered_transport_addr(
    record: &HostRecord,
) -> Result<std::net::SocketAddr, CoreError> {
    if record.port == 0 {
        return Err(CoreError::InvalidDiscoveredPort);
    }
    let address = record
        .address
        .clone()
        .ok_or(CoreError::MissingDiscoveredHostName)?;
    address
        .parse::<std::net::IpAddr>()
        .map(|address| std::net::SocketAddr::new(address, record.port))
        .map_err(|source| CoreError::InvalidDiscoveredAddress {
            address,
            port: record.port,
            source,
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

/// Placeholder names an attach template may use.
const PLACEHOLDER_BIN: &str = "{bin}";
const PLACEHOLDER_HOST: &str = "{host}";
const PLACEHOLDER_ID: &str = "{id}";

/// Reports why an attach template cannot become an argument vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AttachTemplateError {
    /// A quote or trailing backslash in the template is never closed.
    #[error("attach command template has an unterminated quote or escape")]
    UnterminatedQuote,
    /// The template holds no command word.
    #[error("attach command template is empty")]
    EmptyCommand,
    /// The program word is empty after placeholder substitution.
    #[error("attach command program is empty after substitution")]
    EmptyProgram,
    /// A substituted argument contains a NUL byte, which no process can receive.
    #[error("attach command argument contains a NUL byte")]
    NulByte,
    /// A placeholder sits where the shell quoting cannot be proven safe.
    #[error(
        "attach command placeholder cannot be used with {context}; write the placeholder \
         unquoted and pass values as positional parameters, for example \
         `sh -c 'exec \"$@\"' sh {{bin}} attach {{host}} {{id}}`, or render the template \
         as an argument vector"
    )]
    UnsafePlaceholderContext {
        /// The construct that prevents a safe substitution.
        context: &'static str,
    },
}

/// Reports why an attach command could not be rendered and spawned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AttachSpawnError {
    /// The template cannot be rendered safely.
    #[error(transparent)]
    Template(#[from] AttachTemplateError),
    /// The spawner failed to start the rendered command.
    #[error("{0}")]
    Spawn(String),
}

/// Replaces `{bin}`, `{host}`, and `{id}` in `text` in one left-to-right pass.
///
/// Inserted values are never rescanned, so a value that itself contains a
/// placeholder cannot be substituted again. Other brace sequences stay
/// literal.
fn substitute_placeholders(text: &str, bin: &str, host: &str, id: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        let (value, length) = if tail.starts_with(PLACEHOLDER_BIN) {
            (Some(bin), PLACEHOLDER_BIN.len())
        } else if tail.starts_with(PLACEHOLDER_HOST) {
            (Some(host), PLACEHOLDER_HOST.len())
        } else if tail.starts_with(PLACEHOLDER_ID) {
            (Some(id), PLACEHOLDER_ID.len())
        } else {
            (None, '{'.len_utf8())
        };
        match value {
            Some(value) => output.push_str(value),
            None => output.push('{'),
        }
        rest = &tail[length..];
    }
    output.push_str(rest);
    output
}

/// Render the configured attach command as one shell string.
///
/// Replaces `{bin}`, `{host}`, and `{id}` in a single pass. A placeholder is
/// accepted only as an unquoted word, and its value is escaped as exactly one
/// such word, so a value holding quotes, `$()`, backticks, newlines, `;`,
/// spaces, Unicode, or another placeholder never changes the command's
/// structure. Values are data for the launched program: a shell builtin that
/// evaluates its arguments (`let`, `eval`, arithmetic) can still interpret one,
/// so never pass a value to such a builtin.
///
/// The template is not parsed as shell. When it holds a placeholder it must fit
/// an allowlist grammar: outside single quotes, no backtick, parenthesis,
/// bracket, `<`, `>`, literal brace, `#` comment, line continuation, or `$`
/// other than a plain `$NAME`; double-quoted text may hold no `$` construct or
/// backtick; single-quoted text is opaque. Anything else is refused, never
/// rendered best-effort. To run a nested script, pass the values as positional
/// parameters: `sh -c 'exec "$@"' sh {bin} attach --host {host} {id}`.
/// [`render_attach_argv`] accepts quoted placeholders and passes values as data.
///
/// # Errors
///
/// Returns [`AttachTemplateError::UnterminatedQuote`] for an unclosed quote or
/// trailing backslash, [`AttachTemplateError::EmptyCommand`] for a template with
/// no command, and [`AttachTemplateError::UnsafePlaceholderContext`] for a
/// template outside the grammar above or a placeholder that is not an unquoted
/// word.
///
/// # Examples
///
/// ```
/// use pohunek_gui_core::{render_attach_command, AttachTemplateValues};
///
/// let command = render_attach_command(
///     "pohunek --host {host} attach {id}",
///     &AttachTemplateValues {
///         bin: "pohunek".to_owned(),
///         host: "my host".to_owned(),
///         id: "s1".to_owned(),
///     },
/// )?;
/// assert_eq!(command, "pohunek --host 'my host' attach s1");
/// # Ok::<(), pohunek_gui_core::AttachTemplateError>(())
/// ```
pub fn render_attach_command(
    template: &str,
    values: &AttachTemplateValues,
) -> Result<String, AttachTemplateError> {
    let sites = scan_shell_template(template)?;
    let mut output = String::with_capacity(template.len());
    let mut cursor = 0;
    for (offset, placeholder) in sites {
        output.push_str(&template[cursor..offset]);
        let value = match placeholder {
            Placeholder::Bin => &values.bin,
            Placeholder::Host => &values.host,
            Placeholder::Id => &values.id,
        };
        output.push_str(&shell_escape(value));
        cursor = offset + placeholder.token().len();
    }
    output.push_str(&template[cursor..]);
    Ok(output)
}

/// Checks that a shell-mode attach template can be rendered for any values.
///
/// # Errors
///
/// Returns the structural errors of [`render_attach_command`].
pub fn validate_attach_shell_template(template: &str) -> Result<(), AttachTemplateError> {
    scan_shell_template(template).map(drop)
}

/// Checks that an argv-mode attach template splits into at least a program.
///
/// # Errors
///
/// Returns the structural errors of [`render_attach_argv`].
pub fn validate_attach_argv_template(template: &str) -> Result<(), AttachTemplateError> {
    if split_template_words(template)?.is_empty() {
        return Err(AttachTemplateError::EmptyCommand);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placeholder {
    Bin,
    Host,
    Id,
}

impl Placeholder {
    const fn token(self) -> &'static str {
        match self {
            Self::Bin => PLACEHOLDER_BIN,
            Self::Host => PLACEHOLDER_HOST,
            Self::Id => PLACEHOLDER_ID,
        }
    }

    fn at(bytes: &[u8]) -> Option<Self> {
        [Self::Bin, Self::Host, Self::Id]
            .into_iter()
            .find(|placeholder| bytes.starts_with(placeholder.token().as_bytes()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Unquoted,
    Single,
    Double,
}

/// Walks `text` and returns the unquoted placeholders.
///
/// A template that holds a placeholder is checked against an allowlist
/// grammar, not parsed as shell: outside single quotes it may hold no
/// backtick, parenthesis, bracket, `<`, `>`, literal brace, comment, line
/// continuation, or `$` other than a plain `$NAME`, and double-quoted text may
/// hold no `$` construct or backtick either. Single-quoted text is opaque.
/// Anything outside that grammar is refused, so the scanner never has to
/// follow nested shell syntax it could misread. Quote state is still tracked
/// for every template so an unclosed quote is always reported.
fn scan_shell_template(text: &str) -> Result<Vec<(usize, Placeholder)>, AttachTemplateError> {
    let bytes = text.as_bytes();
    let strict = [PLACEHOLDER_BIN, PLACEHOLDER_HOST, PLACEHOLDER_ID]
        .iter()
        .any(|token| text.contains(token));
    let refuse = |context| AttachTemplateError::UnsafePlaceholderContext { context };
    let mut sites = Vec::new();
    let mut state = ScanState::Unquoted;
    let mut word_start = true;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(placeholder) = Placeholder::at(&bytes[index..]) {
            match state {
                ScanState::Single => return Err(refuse("single quotes")),
                ScanState::Double => return Err(refuse("double quotes")),
                ScanState::Unquoted => {}
            }
            if follows_parameter(bytes, index) {
                return Err(refuse("a parameter expansion"));
            }
            sites.push((index, placeholder));
            index += placeholder.token().len();
            word_start = false;
            continue;
        }
        if state == ScanState::Single {
            if byte == b'\'' {
                state = ScanState::Unquoted;
                word_start = false;
            }
            index += 1;
            continue;
        }
        let in_double = state == ScanState::Double;
        match byte {
            b'\\' => {
                let Some(next) = bytes.get(index + 1).copied() else {
                    return Err(AttachTemplateError::UnterminatedQuote);
                };
                if strict && next == b'\n' {
                    return Err(refuse("a line continuation"));
                }
                if strict && Placeholder::at(&bytes[index + 1..]).is_some() {
                    return Err(refuse("an escaped placeholder"));
                }
                index += 1 + utf8_len(next);
                word_start = false;
                continue;
            }
            b'"' => {
                state = if in_double {
                    ScanState::Unquoted
                } else {
                    ScanState::Double
                };
                word_start = false;
            }
            b'\'' if !in_double => {
                state = ScanState::Single;
                word_start = false;
            }
            b'`' if strict => return Err(refuse("a backtick")),
            b'$' if strict => {
                let plain_name = bytes
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'_');
                if !plain_name {
                    return Err(refuse("a `$` construct other than a plain $NAME"));
                }
            }
            b'(' | b')' | b'[' | b']' | b'<' | b'>' | b'{' | b'}' if strict && !in_double => {
                return Err(refuse("parentheses, brackets, braces, or redirection"));
            }
            b'#' if strict && !in_double && word_start => return Err(refuse("a comment")),
            _ => {}
        }
        if !in_double && state == ScanState::Unquoted {
            word_start = matches!(byte, b' ' | b'\t' | b'\n' | b';' | b'&' | b'|');
        }
        index += 1;
    }
    if state != ScanState::Unquoted {
        return Err(AttachTemplateError::UnterminatedQuote);
    }
    if !has_shell_content(text) {
        return Err(AttachTemplateError::EmptyCommand);
    }
    Ok(sites)
}

/// Whether some line holds something other than blanks or a `#` comment.
fn has_shell_content(text: &str) -> bool {
    text.lines()
        .map(str::trim_start)
        .any(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Whether the text before `index` is `$` or `$name`, so a value would extend
/// the parameter name instead of standing alone.
fn follows_parameter(bytes: &[u8], index: usize) -> bool {
    let mut start = index;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    start > 0 && bytes[start - 1] == b'$'
}

/// Length of the UTF-8 character starting with `lead`, so escapes skip whole characters.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Render the configured attach command as an argument vector without a shell.
///
/// Only the template is split, with POSIX-shell word rules: space, tab, and
/// newline separate words (other Unicode spaces are word characters), `'...'` is literal, `"..."` honors `\"`, `\\`, `\$`,
/// and `` \` ``, and an unquoted backslash escapes the next character. No
/// expansion happens: `$VAR`, `~`, globs, and command substitution stay
/// literal text. After splitting, `{bin}`, `{host}`, and `{id}` are replaced
/// in one pass inside each word, so every value stays part of exactly one
/// argument and is never split again. The first element is the program.
///
/// # Examples
///
/// ```
/// use pohunek_gui_core::{render_attach_argv, AttachTemplateValues};
///
/// let argv = render_attach_argv(
///     "open -a Terminal --args {bin} attach {id}",
///     &AttachTemplateValues {
///         bin: "/opt/My Tools/pohunek".to_owned(),
///         host: String::new(),
///         id: "s 1".to_owned(),
///     },
/// )?;
/// assert_eq!(argv[4], "/opt/My Tools/pohunek");
/// assert_eq!(argv[6], "s 1");
/// # Ok::<(), pohunek_gui_core::AttachTemplateError>(())
/// ```
///
/// # Errors
///
/// Returns [`AttachTemplateError::UnterminatedQuote`] for an unclosed quote or
/// trailing backslash, [`AttachTemplateError::EmptyCommand`] for a template
/// without words, [`AttachTemplateError::EmptyProgram`] when the program word
/// is empty after substitution, and [`AttachTemplateError::NulByte`] when an
/// argument contains a NUL byte.
pub fn render_attach_argv(
    template: &str,
    values: &AttachTemplateValues,
) -> Result<Vec<String>, AttachTemplateError> {
    let words = split_template_words(template)?;
    if words.is_empty() {
        return Err(AttachTemplateError::EmptyCommand);
    }
    let argv: Vec<String> = words
        .iter()
        .map(|word| substitute_placeholders(word, &values.bin, &values.host, &values.id))
        .collect();
    if argv.iter().any(|argument| argument.contains('\0')) {
        return Err(AttachTemplateError::NulByte);
    }
    if argv[0].is_empty() {
        return Err(AttachTemplateError::EmptyProgram);
    }
    Ok(argv)
}

/// Splits `template` into words by POSIX-shell quoting rules, without expansion.
fn split_template_words(template: &str) -> Result<Vec<String>, AttachTemplateError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut characters = template.chars();
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                in_word = true;
                loop {
                    match characters.next() {
                        Some('\'') => break,
                        Some(inner) => current.push(inner),
                        None => return Err(AttachTemplateError::UnterminatedQuote),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some('\\') => match characters.next() {
                            Some(escaped @ ('"' | '\\' | '$' | '`')) => current.push(escaped),
                            Some('\n') => {}
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => return Err(AttachTemplateError::UnterminatedQuote),
                        },
                        Some(inner) => current.push(inner),
                        None => return Err(AttachTemplateError::UnterminatedQuote),
                    }
                }
            }
            '\\' => match characters.next() {
                Some('\n') => {}
                Some(escaped) => {
                    in_word = true;
                    current.push(escaped);
                }
                None => return Err(AttachTemplateError::UnterminatedQuote),
            },
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    // A leading `-` could read as an option; `,` and `=` are left out of the
    // bare set so brace expansion and assignment-word shapes never appear.
    if value.bytes().all(is_shell_safe_byte) && !value.starts_with('-') {
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
    )
}

/// Resolve and spawn an external attach command.
///
/// # Errors
///
/// Returns [`AttachSpawnError::Template`] when the template cannot be rendered
/// safely and [`AttachSpawnError::Spawn`] when the spawner fails.
pub fn spawn_attach_command<S>(
    spawner: &mut S,
    template: &str,
    values: &AttachTemplateValues,
) -> Result<AttachSpawnIntent, AttachSpawnError>
where
    S: AttachCommandSpawner + ?Sized,
{
    let command = render_attach_command(template, values)?;
    spawner.spawn(&command).map_err(AttachSpawnError::Spawn)?;
    Ok(AttachSpawnIntent { command })
}

#[cfg(test)]
mod attach_template_tests {
    use std::process::Command;

    use super::*;

    /// Values that break naive quoting or re-substitution.
    fn difficult_values() -> Vec<String> {
        [
            "plain",
            "with space",
            "it's",
            "say \"hi\"",
            "$(echo INJECTED)",
            "`echo INJECTED`",
            "a;echo INJECTED",
            "a&&echo INJECTED",
            "line one\nline two",
            "tab\there",
            "back\\slash",
            "\u{10d}esk\u{fd} projekt \u{65e5}\u{672c}\u{8a9e} \u{1f980}",
            "{bin}",
            "{host}",
            "{id}",
            "{host};echo INJECTED",
            "x{id}y",
            "'{id}'",
            "$HOME ~ * ?",
            "--flag=value",
            "",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// Runs `command` through a real `sh -c` and splits its NUL-terminated output.
    ///
    /// Injected `echo INJECTED` payloads would add output and break equality.
    fn run_sh(command: &str) -> Vec<Vec<u8>> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .expect("run sh");
        assert!(output.status.success(), "sh failed for {command:?}");
        let mut fields: Vec<Vec<u8>> = output
            .stdout
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect();
        assert_eq!(fields.pop(), Some(Vec::new()), "output ends with a NUL");
        fields
    }

    #[test]
    fn shell_form_round_trips_difficult_values_through_a_real_shell() {
        let template = "printf '%s\\0' {bin} {host} {id}";
        let values = difficult_values();
        for bin in &values {
            for host in &values {
                let id = "sess ion;id";
                let command = render_attach_command(
                    template,
                    &AttachTemplateValues {
                        bin: bin.clone(),
                        host: host.clone(),
                        id: id.to_owned(),
                    },
                )
                .expect("render");
                let fields = run_sh(&command);
                assert_eq!(
                    fields,
                    [bin.as_bytes(), host.as_bytes(), id.as_bytes()],
                    "bin={bin:?} host={host:?} rendered={command:?}"
                );
            }
        }
    }

    #[test]
    fn shell_form_does_not_resubstitute_inserted_values() {
        let values = AttachTemplateValues {
            bin: "{host}".to_owned(),
            host: "x' ; echo INJECTED ; echo '".to_owned(),
            id: "{bin}".to_owned(),
        };
        let rendered = render_attach_command("{bin} {host} {id}", &values).expect("render");
        assert_eq!(
            rendered,
            format!(
                "{} {} {}",
                shell_escape(&values.bin),
                shell_escape(&values.host),
                shell_escape(&values.id)
            )
        );
    }

    #[test]
    fn shell_form_keeps_unknown_braces_literal() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        assert_eq!(
            render_attach_command("echo {other} {} { id} {host", &values).expect("render"),
            "echo {other} {} { id} {host"
        );
    }

    fn hostile_values() -> Vec<&'static str> {
        vec![
            "safe; printf INJECTED",
            "'",
            "\"",
            "$(printf INJECTED)",
            "`printf INJECTED`",
            "line one\nline two",
            "{host}",
            "it's \"mixed\" \\ $HOME",
            "",
            "\u{10d}esk\u{fd} \u{65e5}\u{672c}",
        ]
    }

    /// Renders `template` for every hostile value in each placeholder slot and
    /// checks a real shell hands exactly the values to `printf` as arguments.
    fn assert_recorded(template: &str) {
        for hostile in hostile_values() {
            let values = AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: hostile.to_owned(),
                id: hostile.to_owned(),
            };
            let command = render_attach_command(template, &values).expect("render");
            assert_eq!(
                run_sh(&command),
                [
                    b"pohunek".to_vec(),
                    hostile.as_bytes().to_vec(),
                    hostile.as_bytes().to_vec()
                ],
                "template={template:?} value={hostile:?} rendered={command:?}"
            );
        }
    }

    #[test]
    fn unquoted_placeholders_deliver_exact_arguments_and_run_nothing_else() {
        assert_recorded("printf '%s\\0' {bin} {host} {id}");
        assert_recorded("true; printf '%s\\0' {bin} {host} {id} && true");
        // Positional parameters carry the values into a nested script.
        assert_recorded("sh -c 'exec printf \"%s\\0\" \"$@\"' sh {bin} {host} {id}");
    }

    #[test]
    fn placeholders_outside_unquoted_words_are_typed_errors() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        let dollar = "a `$` construct other than a plain $NAME";
        let grouping = "parentheses, brackets, braces, or redirection";
        for (template, context) in [
            ("sh -c 'exec {bin} attach {host}'", "single quotes"),
            ("terminal -- \"{bin}\"", "double quotes"),
            ("echo $'{host}'", dollar),
            ("echo x # {host}", "a comment"),
            ("cat <<EOF; echo {host}", grouping),
            ("echo \\\n{host}", "a line continuation"),
            ("echo $(true) {host}", dollar),
            ("echo `true` {host}", "a backtick"),
            ("echo \\{host}", "an escaped placeholder"),
            ("echo ${id}", dollar),
            ("echo $x{host}", "a parameter expansion"),
            ("echo ${x:-{host}}", dollar),
            ("echo ${x:-${id}}", dollar),
            ("echo $(({id}))", dollar),
            ("echo $({bin})", dollar),
            // Reviewer repros: nested quotes and brackets desynchronised a
            // parity-based scanner.
            ("echo \"${x:-\"{host}\"}\"", dollar),
            ("echo $[ {host} + 1 ]", dollar),
            ("[[ 1 -eq {host} ]]", grouping),
            ("(( {host} ))", grouping),
            ("echo {host} > out", grouping),
            ("echo {a,b} {host}", grouping),
            ("echo \"$(true)\" {host}", dollar),
            ("echo \"`true`\" {host}", "a backtick"),
            ("echo $1 {host}", dollar),
        ] {
            let expected = Err(AttachTemplateError::UnsafePlaceholderContext { context });
            assert_eq!(
                render_attach_command(template, &values),
                expected,
                "{template:?}"
            );
            assert_eq!(
                validate_attach_shell_template(template),
                expected.map(drop),
                "{template:?}"
            );
        }
        let message = render_attach_command("echo '{host}'", &values)
            .expect_err("quoted")
            .to_string();
        assert!(message.contains("positional parameters"), "{message}");
        assert!(!message.contains("attach_command_mode"), "{message}");
        for template in ["echo {host} 'x", "echo {host} \"x", "echo {host} \\"] {
            assert_eq!(
                render_attach_command(template, &values),
                Err(AttachTemplateError::UnterminatedQuote),
                "{template:?}"
            );
        }
    }

    #[test]
    fn unsupported_constructs_away_from_placeholders_are_accepted() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        // Without a placeholder the template is static and any syntax passes.
        assert_eq!(
            render_attach_command("echo $(true) # comment\necho x", &values).expect("render"),
            "echo $(true) # comment\necho x"
        );
        assert_eq!(
            render_attach_command("$TERMINAL -e {bin} attach --host {host} {id}", &values)
                .expect("render"),
            "$TERMINAL -e b attach --host h i"
        );
    }

    #[test]
    fn argv_form_accepts_quoted_placeholders_as_data() {
        let spaced = AttachTemplateValues {
            bin: "/opt/My Tools/pohunek".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        assert_eq!(
            render_attach_argv("terminal -- \"{bin}\"", &spaced).expect("argv"),
            ["terminal", "--", "/opt/My Tools/pohunek"]
        );
        for hostile in hostile_values()
            .into_iter()
            .chain(["a\u{a0}b", "it's \"q\""])
        {
            let values = AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: hostile.to_owned(),
                id: hostile.to_owned(),
            };
            let argv = render_attach_argv(
                "sh -c 'exec \"$@\"' sh {bin} attach '{host}' \"{id}\"",
                &values,
            )
            .expect("argv");
            assert_eq!(
                argv,
                [
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "exec \"$@\"".to_owned(),
                    "sh".to_owned(),
                    "pohunek".to_owned(),
                    "attach".to_owned(),
                    hostile.to_owned(),
                    hostile.to_owned(),
                ],
                "{hostile:?}"
            );
        }
    }

    #[test]
    fn templates_without_executable_content_are_empty_commands() {
        for template in ["", "   ", "\n\t ", "# only a comment", "  # note\n  # more"] {
            assert_eq!(
                validate_attach_shell_template(template),
                Err(AttachTemplateError::EmptyCommand),
                "{template:?}"
            );
        }
        // A comment after real content, and real content after a comment.
        assert_eq!(validate_attach_shell_template("true # note"), Ok(()));
        assert_eq!(validate_attach_shell_template("# note\ntrue"), Ok(()));
    }

    #[test]
    fn a_parameter_expansion_closed_before_a_placeholder_is_fine() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "s-7".to_owned(),
        };
        assert_eq!(
            render_attach_command("echo $HOME/x \"$HOME\" {id}", &values).expect("render"),
            "echo $HOME/x \"$HOME\" s-7"
        );
        // Without the guard `${id}` would render `$s-7`, which sh expands as `$s`.
        render_attach_command("echo ${id}", &values).expect_err("refused");
    }

    #[test]
    fn bare_words_exclude_assignment_brace_and_option_shapes() {
        for (value, escaped) in [
            ("x=y", "'x=y'"),
            ("{a,b}", "'{a,b}'"),
            ("a,b", "'a,b'"),
            ("-oProxyCommand=x", "'-oProxyCommand=x'"),
            ("-", "'-'"),
            ("/opt/pohunek", "/opt/pohunek"),
            ("devbox:7000", "devbox:7000"),
            ("a-b", "a-b"),
        ] {
            assert_eq!(shell_escape(value), escaped, "{value:?}");
        }
    }

    #[test]
    fn the_documented_attach_templates_validate() {
        for template in [
            "$TERMINAL -e sh -c 'exec \"$@\"' sh {bin} attach --host {host} {id}",
            "$TERMINAL -e sh -c 'printf \"\\033]0;pohunek:%s\\007\" \"$5\"; exec \"$@\"' sh {bin} attach --host {host} {id}",
            "kitty -e {bin} --host={host} attach -- {id}",
        ] {
            assert_eq!(validate_attach_shell_template(template), Ok(()), "{template}");
        }
    }

    #[test]
    fn argv_templates_validate_structurally() {
        assert_eq!(validate_attach_argv_template("kitty -e {bin}"), Ok(()));
        assert_eq!(
            validate_attach_argv_template("  "),
            Err(AttachTemplateError::EmptyCommand)
        );
        assert_eq!(
            validate_attach_argv_template("kitty '"),
            Err(AttachTemplateError::UnterminatedQuote)
        );
    }

    #[test]
    fn argv_form_keeps_every_value_a_single_byte_exact_argument() {
        let values = difficult_values();
        for bin in &values {
            for host in &values {
                let argv = render_attach_argv(
                    "launcher --host {host} --id={id} {bin}",
                    &AttachTemplateValues {
                        bin: bin.clone(),
                        host: host.clone(),
                        id: "sess ion;id".to_owned(),
                    },
                )
                .expect("argv");
                assert_eq!(
                    argv,
                    [
                        "launcher".to_owned(),
                        "--host".to_owned(),
                        host.clone(),
                        "--id=sess ion;id".to_owned(),
                        bin.clone(),
                    ],
                    "bin={bin:?} host={host:?}"
                );
            }
        }
    }

    #[test]
    fn argv_form_splits_only_the_template() {
        let values = AttachTemplateValues {
            bin: "/opt/My Tools/pohunek".to_owned(),
            host: "a b 'c' \"d\"".to_owned(),
            id: "1".to_owned(),
        };
        let argv = render_attach_argv(
            r#"open -a "Some App" --args '{bin}' attach\ --host={host} "" {id}"#,
            &values,
        )
        .expect("argv");
        assert_eq!(
            argv,
            [
                "open",
                "-a",
                "Some App",
                "--args",
                "/opt/My Tools/pohunek",
                "attach --host=a b 'c' \"d\"",
                "",
                "1"
            ]
        );
    }

    #[test]
    fn argv_form_double_quotes_follow_posix_escape_rules() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        let argv = render_attach_argv(r#"x "a\"b\\c\$d\e" 'q\n'"#, &values).expect("argv");
        assert_eq!(argv, ["x", "a\"b\\c$d\\e", "q\\n"]);
    }

    #[test]
    fn argv_form_splits_words_like_sh_for_unicode_spaces() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        let template = "printf '%s\\0' /opt/My\u{a0}Tools/bin\u{2003}x \u{a0} tab\tend";
        let argv = render_attach_argv(template, &values).expect("argv");
        // The real shell agrees on the word boundaries; its `printf` builtin
        // prints each word NUL-terminated.
        let shell_words = run_sh(template);
        assert_eq!(
            argv[2..]
                .iter()
                .map(|word| word.as_bytes().to_vec())
                .collect::<Vec<_>>(),
            shell_words
        );
        assert_eq!(argv[2], "/opt/My\u{a0}Tools/bin\u{2003}x");
        assert_eq!(argv[3], "\u{a0}");
    }

    #[test]
    fn argv_form_performs_no_expansion() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        let argv = render_attach_argv("x $HOME ~ * $(id) `id`", &values).expect("argv");
        assert_eq!(argv, ["x", "$HOME", "~", "*", "$(id)", "`id`"]);
    }

    #[test]
    fn argv_form_rejects_unterminated_quotes_and_escapes() {
        let values = AttachTemplateValues {
            bin: "b".to_owned(),
            host: "h".to_owned(),
            id: "i".to_owned(),
        };
        for template in ["x 'open", "x \"open", "x open\\", "x \"open\\"] {
            assert_eq!(
                render_attach_argv(template, &values),
                Err(AttachTemplateError::UnterminatedQuote),
                "{template:?}"
            );
        }
    }

    #[test]
    fn argv_form_rejects_empty_commands_and_nul_values() {
        let values = AttachTemplateValues {
            bin: String::new(),
            host: "h\0x".to_owned(),
            id: "i".to_owned(),
        };
        assert_eq!(
            render_attach_argv("   ", &values),
            Err(AttachTemplateError::EmptyCommand)
        );
        assert_eq!(
            render_attach_argv("{bin} attach", &values),
            Err(AttachTemplateError::EmptyProgram)
        );
        assert_eq!(
            render_attach_argv("tool {host}", &values),
            Err(AttachTemplateError::NulByte)
        );
    }
}
