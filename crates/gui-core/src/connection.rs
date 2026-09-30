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
/// Replaces `{bin}`, `{host}`, and `{id}` in a single pass with shell-escaped
/// values, so no value can change the command's structure: a value holding
/// quotes, `$()`, backticks, newlines, `;`, spaces, Unicode, or another
/// placeholder reaches the shell as one literal word. The GUI shell spawner
/// executes the rendered command through `sh -c`; launchers that must not use a
/// shell call [`render_attach_argv`] instead.
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
/// );
/// assert_eq!(command, "pohunek --host 'my host' attach s1");
/// ```
#[must_use]
pub fn render_attach_command(template: &str, values: &AttachTemplateValues) -> String {
    substitute_placeholders(
        template,
        &shell_escape(&values.bin),
        &shell_escape(&values.host),
        &shell_escape(&values.id),
    )
}

/// Render the configured attach command as an argument vector without a shell.
///
/// Only the template is split, with POSIX-shell word rules: whitespace
/// separates words, `'...'` is literal, `"..."` honors `\"`, `\\`, `\$`,
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
            whitespace if whitespace.is_whitespace() => {
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
                );
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
        let rendered = render_attach_command("{bin} {host} {id}", &values);
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
            render_attach_command("{bin} {other} {} { id} {host", &values),
            "b {other} {} { id} {host"
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
