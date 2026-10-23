//! Typed SDK client errors.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use protocol::compat::CompatError;
use protocol::{EnvelopeError, ErrorClass, ProtocolError, ProtocolVersion, ProtocolVersionRange};

/// Errors raised by SDK client transport and remote-host discovery.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Exactly one inherited origin marker was present; the pair is atomic.
    #[error("incomplete Pohunek session origin environment")]
    IncompleteOriginEnvironment,
    /// The inherited origin pair contained an invalid non-secret identifier.
    #[error("invalid Pohunek session origin environment")]
    InvalidOriginEnvironment,
    /// A locally constructed protocol envelope violated its wire contract.
    #[error("invalid protocol envelope: {0}")]
    Envelope(#[from] EnvelopeError),

    /// A peer selected a protocol version outside the request's negotiated range.
    #[error("peer selected protocol version {received}, but the expected range is {expected:?}")]
    ProtocolVersionMismatch {
        /// Version that this connection already selected, or the request range on its first reply.
        expected: ProtocolVersionRange,
        /// Invalid version returned by the peer.
        received: ProtocolVersion,
    },
    /// The daemon negotiated an older protocol version that never defined the method.
    ///
    /// Raised before anything is sent: the method exists only in the client's
    /// current protocol, so the daemon's host has to be upgraded first.
    #[error(
        "{}",
        ProtocolError::daemon_protocol_too_old(
            host.as_deref(),
            method,
            *daemon_version,
            *required_version,
        )
        .msg
    )]
    DaemonProtocolTooOld {
        /// Route of the remote host, or `None` for the local daemon.
        host: Option<String>,
        /// Method the older protocol version does not define.
        method: String,
        /// Protocol version the daemon negotiated for this connection.
        daemon_version: ProtocolVersion,
        /// Protocol version that defines the method.
        required_version: ProtocolVersion,
    },
    /// A payload could not be translated for the older version the daemon negotiated.
    #[error(
        "{}",
        ProtocolError::version_translation_failed(host.as_deref(), *daemon_version).msg
    )]
    VersionTranslation {
        /// Route of the remote host, or `None` for the local daemon.
        host: Option<String>,
        /// Protocol version the daemon negotiated for this connection.
        daemon_version: ProtocolVersion,
        /// The adapter failure.
        #[source]
        source: CompatError,
    },
    /// The local daemon socket could not be reached.
    #[error("cannot reach the daemon at {socket}: {source}")]
    DaemonUnreachable {
        /// The socket path that was dialed.
        socket: PathBuf,
        /// Underlying connection error.
        #[source]
        source: io::Error,
    },

    /// This client process exhausted its file-descriptor limit.
    #[error(
        "cannot open a daemon connection at {socket}: \
         this client process exhausted its file-descriptor limit: {source}"
    )]
    ClientFileDescriptorsExhausted {
        /// The socket path that was dialed.
        socket: PathBuf,
        /// Underlying `EMFILE` connection error.
        #[source]
        source: io::Error,
    },

    /// The host exhausted its system-wide open-file table.
    #[error(
        "cannot open a daemon connection at {socket}: \
         the host exhausted its system-wide open-file table: {source}"
    )]
    SystemFileDescriptorsExhausted {
        /// The socket path that was dialed.
        socket: PathBuf,
        /// Underlying `ENFILE` connection error.
        #[source]
        source: io::Error,
    },

    /// A control line could not be framed or parsed.
    #[error("protocol framing error: {0}")]
    Framing(String),

    /// A successful input-wait response omitted or contradicted delivery evidence.
    #[error("invalid session.input wait response: {detail}")]
    InputWaitContract {
        /// Non-secret description of the violated response invariant.
        detail: &'static str,
    },

    /// A daemon returned a typed protocol error.
    #[error("daemon error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The overlay configuration file or the remote port override is unusable.
    ///
    /// The reason never quotes a value from the file.
    #[error("overlay configuration {path} is invalid: {reason}", path = .path.display())]
    OverlayConfiguration {
        /// The configuration file, whether or not it exists.
        path: PathBuf,
        /// Why the configuration cannot be used, by cause only.
        reason: String,
    },

    /// One provider failed before a remote daemon could be dialed.
    #[error(transparent)]
    Overlay(#[from] overlay::OverlayError),

    /// Configured registry construction or route selection failed.
    #[error(transparent)]
    OverlayRegistry(#[from] overlay::RegistryError),

    /// Every configured provider failed during aggregated discovery.
    #[error("all configured overlays failed discovery")]
    OverlayDiscoveryFailed {
        /// Per-overlay typed failures retained without flattening.
        failures: Vec<overlay::OverlayFailure>,
    },

    /// Bounded remote-host discovery or its overall deadline failed.
    #[error("remote host discovery failed: {detail}")]
    RemoteDiscoveryFailed {
        /// Non-secret detail describing the discovery failure.
        detail: String,
    },

    /// Caller-supplied standalone discovery settings violate required bounds.
    #[error("invalid discovery options: {detail}")]
    InvalidDiscoveryOptions {
        /// Non-secret detail describing the violated local invariant.
        detail: String,
    },

    /// An explicit remote route did not contain a valid selector and port.
    #[error("invalid remote route '{route}': {detail}")]
    InvalidRemoteRoute {
        /// Non-secret route supplied by the caller.
        route: String,
        /// Stable explanation of the rejected route shape.
        detail: &'static str,
    },

    /// A remote host has neither a provider peer id nor an FQDN, so no stable
    /// identity can pin the connections of one operation to it.
    #[error("host '{host}' cannot be pinned to one peer identity")]
    HostIdentityUnpinnable {
        /// Host name as the caller typed it.
        host: String,
    },

    /// An overlay TCP connection to the host's daemon port could not be opened.
    #[error(
        "could not open an overlay connection to host '{host}' ({}): {source}",
        describe_route(.overlay.as_deref(), *.address, *.port)
    )]
    HostUnreachable {
        /// The requested host name.
        host: String,
        /// Overlay the host was resolved through, when the caller knows it.
        overlay: Option<String>,
        /// Address that was dialled.
        address: IpAddr,
        /// Daemon port that was dialled.
        port: u16,
        /// Underlying connection error.
        #[source]
        source: io::Error,
    },

    /// A TCP connection opened, but no compatible remote daemon answered.
    #[error("connected to host '{host}' but no compatible pohunek daemon answered")]
    RemoteDaemonUnavailable {
        /// The host whose daemon did not answer.
        host: String,
    },

    /// An established daemon request exceeded its response deadline.
    #[error("timed out after {timeout:?} waiting for a daemon response")]
    RequestTimeout {
        /// Remote host involved in the request, or `None` for the local daemon.
        host: Option<String>,
        /// Configured response deadline that elapsed.
        timeout: Duration,
    },

    /// A `session.resume` exceeded its response deadline, so the daemon may
    /// still complete the resume.
    #[error("timed out after {timeout:?} waiting for the result of a session resume")]
    SessionResumeOutcomeUnknown {
        /// Remote host involved in the request, or `None` for the local daemon.
        host: Option<String>,
        /// Configured response deadline that elapsed.
        timeout: Duration,
    },

    /// A `session.fork` exceeded its response deadline, so the daemon may
    /// still complete the fork and a retry would create a second child.
    #[error("timed out after {timeout:?} waiting for the result of a session fork")]
    SessionForkOutcomeUnknown {
        /// Remote host involved in the request, or `None` for the local daemon.
        host: Option<String>,
        /// Configured response deadline that elapsed.
        timeout: Duration,
    },

    /// A `session.stop` exceeded its response deadline, so the daemon may
    /// still complete the stop.
    #[error("timed out after {timeout:?} waiting for the result of a session stop")]
    SessionStopOutcomeUnknown {
        /// Remote host involved in the request, or `None` for the local daemon.
        host: Option<String>,
        /// Configured response deadline that elapsed.
        timeout: Duration,
    },

    /// A `session.remove` or `session.remove_accepting_unconfirmed` exceeded
    /// its response deadline, so the daemon may still complete the removal.
    #[error("timed out after {timeout:?} waiting for the result of a session removal")]
    SessionRemoveOutcomeUnknown {
        /// Remote host involved in the request, or `None` for the local daemon.
        host: Option<String>,
        /// Configured response deadline that elapsed.
        timeout: Duration,
    },

    /// A daemon on a remote host returned a typed protocol error.
    #[error("host '{host}': {source}")]
    RemoteProtocol {
        /// The host whose daemon returned the error.
        host: String,
        /// The daemon's typed error, relayed unchanged except for host context.
        #[source]
        source: ProtocolError,
    },

    /// Generic I/O failure after a connection was opened.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// JSON serialization or deserialization failure at the client edge.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ClientError {
    /// Structured, serializable representation of this SDK client error.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive mapping keeps SDK error codes and recovery hints centralized"
    )]
    pub fn to_protocol_error(&self) -> protocol::ProtocolError {
        match self {
            ClientError::IncompleteOriginEnvironment => ProtocolError::new(
                ErrorClass::Configuration,
                "incomplete_origin_environment",
                "incomplete Pohunek session origin environment".to_owned(),
                Some("set both POHUNEK_SESSION_ID and POHUNEK_DAEMON_ID, or unset both".to_owned()),
            ),
            ClientError::InvalidOriginEnvironment => ProtocolError::new(
                ErrorClass::Configuration,
                "invalid_origin_environment",
                "invalid Pohunek session origin environment".to_owned(),
                Some(
                    "use bounded UTF-8 identifiers without control characters for both origin markers"
                        .to_owned(),
                ),
            ),
            ClientError::Envelope(error) => ProtocolError::new(
                ErrorClass::Configuration,
                "invalid_protocol_envelope",
                format!("invalid protocol envelope: {error}"),
                None,
            ),
            ClientError::ProtocolVersionMismatch { expected, received } => {
                ProtocolError::version_mismatch(*expected, exact_version_range(*received))
            }
            ClientError::DaemonProtocolTooOld {
                host,
                method,
                daemon_version,
                required_version,
            } => ProtocolError::daemon_protocol_too_old(
                host.as_deref(),
                method,
                *daemon_version,
                *required_version,
            ),
            ClientError::VersionTranslation {
                host,
                daemon_version,
                ..
            } => ProtocolError::version_translation_failed(host.as_deref(), *daemon_version),
            ClientError::DaemonUnreachable { socket, source } => ProtocolError::new(
                ErrorClass::Daemon,
                "daemon_unreachable",
                format!("cannot reach the daemon at {}: {source}", socket.display()),
                Some("start the daemon with `pohunek daemon start`".to_owned()),
            ),
            ClientError::ClientFileDescriptorsExhausted { socket, source } => ProtocolError::new(
                ErrorClass::Runtime,
                "client_file_descriptors_exhausted",
                format!(
                    "cannot open a daemon connection at {}: this client process exhausted \
                         its file-descriptor limit: {source}",
                    socket.display()
                ),
                Some(
                    "close unused connections in this client process or raise its \
                         RLIMIT_NOFILE, then retry; the daemon may still be running"
                        .to_owned(),
                ),
            ),
            ClientError::SystemFileDescriptorsExhausted { socket, source } => ProtocolError::new(
                ErrorClass::Runtime,
                "system_file_descriptors_exhausted",
                format!(
                    "cannot open a daemon connection at {}: the host exhausted its \
                         system-wide open-file table: {source}",
                    socket.display()
                ),
                Some(
                    "free system-wide file descriptors or raise the host file-table limit, \
                         then retry; the daemon may still be running"
                        .to_owned(),
                ),
            ),
            ClientError::Framing(msg) => ProtocolError::new(
                ErrorClass::Transport,
                "framing",
                format!("protocol framing error: {msg}"),
                None,
            ),
            ClientError::InputWaitContract { detail } => ProtocolError::new(
                ErrorClass::Daemon,
                "session_input_wait_contract_mismatch",
                format!("invalid session.input wait response: {detail}"),
                Some(
                    "delivery outcome is unknown; do not retry blindly; upgrade the daemon and \
                     client together, then inspect the session before deciding whether to resend"
                        .to_owned(),
                ),
            ),
            ClientError::Protocol(err) => err.clone(),
            ClientError::OverlayConfiguration { path, reason } => ProtocolError::new(
                ErrorClass::Configuration,
                "overlay_configuration_invalid",
                format!(
                    "overlay configuration {} is invalid: {reason}",
                    path.display()
                ),
                Some(format!(
                    "fix or remove {} and retry; POHUNEK_REMOTE_PORT must be unset or a port number",
                    path.display()
                )),
            ),
            ClientError::Overlay(error) => overlay_error_to_protocol(error),
            ClientError::OverlayRegistry(error) => registry_error_to_protocol(error),
            ClientError::OverlayDiscoveryFailed { failures } => {
                if let [failure] = failures.as_slice() {
                    overlay_error_to_protocol(&failure.error)
                } else {
                    let overlays = failures
                        .iter()
                        .map(|failure| failure.overlay.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    ProtocolError::new(
                        ErrorClass::Discovery,
                        "overlay_discovery_failed",
                        format!("all configured overlays failed discovery: {overlays}"),
                        Some(
                            "run `pohunek doctor` and repair at least one configured overlay"
                                .to_owned(),
                        ),
                    )
                }
            }
            ClientError::RemoteDiscoveryFailed { detail } => ProtocolError::new(
                ErrorClass::Discovery,
                "remote_discovery_failed",
                format!("remote host discovery failed: {detail}"),
                Some(
                    "retry the remote request; if it persists, check the local overlay state"
                        .to_owned(),
                ),
            ),
            ClientError::InvalidDiscoveryOptions { detail } => ProtocolError::new(
                ErrorClass::Configuration,
                "invalid_discovery_options",
                format!("invalid discovery options: {detail}"),
                Some("fix the invalid discovery option before running discovery".to_owned()),
            ),
            ClientError::HostIdentityUnpinnable { host } => ProtocolError::new(
                ErrorClass::Discovery,
                "host_identity_unpinnable",
                format!("host '{host}' reports neither a peer id nor an FQDN, so it cannot be pinned to one peer for this operation"),
                Some(
                    "use the provider-qualified selector from `pohunek host list`, or run a command that opens one connection"
                        .to_owned(),
                ),
            ),
            ClientError::InvalidRemoteRoute { route, detail } => ProtocolError::new(
                ErrorClass::Configuration,
                "invalid_remote_route",
                format!("invalid remote route '{route}': {detail}"),
                Some(
                    "use '<overlay>:<selector>' or '<overlay>:<selector>@<non-zero-port>'"
                        .to_owned(),
                ),
            ),
            ClientError::HostUnreachable {
                host,
                overlay,
                address,
                port,
                source,
            } => {
                let route = describe_route(overlay.as_deref(), *address, *port);
                let recover = match overlay {
                    Some(overlay) => format!(
                        "check that the host is online, that its pohunek daemon is running and \
                         that its overlays.toml lists '{overlay}' as its provider; a daemon \
                         started with another provider does not listen on {overlay}"
                    ),
                    None => "check that the host is online and its pohunek daemon is running"
                        .to_owned(),
                };
                ProtocolError::new(
                    ErrorClass::Transport,
                    "host_unreachable",
                    format!(
                        "could not open an overlay connection to host '{host}' ({route}): {source}"
                    ),
                    Some(recover),
                )
            }
            ClientError::RemoteDaemonUnavailable { host } => {
                ProtocolError::remote_daemon_unavailable(host)
            }
            ClientError::RequestTimeout { host, timeout } => ProtocolError::new(
                ErrorClass::Transport,
                "request_timeout",
                host.as_ref().map_or_else(
                    || format!("timed out after {timeout:?} waiting for the local daemon response"),
                    |host| {
                        format!(
                            "timed out after {timeout:?} waiting for a response from host '{host}'"
                        )
                    },
                ),
                Some(
                    "the request may have completed; reconcile daemon state before retrying a mutation"
                        .to_owned(),
                ),
            ),
            ClientError::SessionResumeOutcomeUnknown { host, timeout } => ProtocolError::new(
                ErrorClass::Transport,
                "session_resume_outcome_unknown",
                host.as_ref().map_or_else(
                    || format!("timed out after {timeout:?} waiting for the local daemon to resume the session"),
                    |host| {
                        format!(
                            "timed out after {timeout:?} waiting for host '{host}' to resume the session"
                        )
                    },
                ),
                Some(
                    "the resume may still complete: run `pohunek session inspect <session>` and read its runtime state before retrying"
                        .to_owned(),
                ),
            ),
            ClientError::SessionForkOutcomeUnknown { host, timeout } => ProtocolError::new(
                ErrorClass::Transport,
                "session_fork_outcome_unknown",
                lifecycle_timeout_message(host.as_deref(), *timeout, "fork the session"),
                Some(
                    "the fork may still complete and a retried fork creates a second child session: run `pohunek session list` and look for the new session before retrying"
                        .to_owned(),
                ),
            ),
            ClientError::SessionStopOutcomeUnknown { host, timeout } => ProtocolError::new(
                ErrorClass::Transport,
                "session_stop_outcome_unknown",
                lifecycle_timeout_message(host.as_deref(), *timeout, "stop the session"),
                Some(
                    "the stop may still complete: run `pohunek session inspect <session>` and read its state; repeating a stop is harmless"
                        .to_owned(),
                ),
            ),
            ClientError::SessionRemoveOutcomeUnknown { host, timeout } => ProtocolError::new(
                ErrorClass::Transport,
                "session_remove_outcome_unknown",
                lifecycle_timeout_message(host.as_deref(), *timeout, "remove the session"),
                Some(
                    "the removal may still complete: run `pohunek session list` to see whether the session is gone before retrying"
                        .to_owned(),
                ),
            ),
            ClientError::RemoteProtocol { host, source } => {
                let mut err = source.clone();
                err.msg = format!("host '{host}': {}", err.msg);
                err
            }
            ClientError::Io(err) => ProtocolError::new(
                ErrorClass::Runtime,
                "io_error",
                format!("io error: {err}"),
                None,
            ),
            ClientError::Json(err) => ProtocolError::new(
                ErrorClass::Daemon,
                "json_error",
                format!("json error: {err}"),
                None,
            ),
        }
    }

    /// Recovery hint to surface beneath this error, when one applies.
    #[must_use]
    pub fn recover_hint(&self) -> Option<String> {
        self.to_protocol_error().recover
    }
}

fn overlay_error_to_protocol(error: &overlay::OverlayError) -> ProtocolError {
    let overlay = error.overlay().as_str();
    match error {
        overlay::OverlayError::CliMissing(_) => ProtocolError::new(
            ErrorClass::Discovery,
            format!("{overlay}_cli_missing"),
            format!("the {overlay} CLI was not found on PATH"),
            Some(format!(
                "install the {overlay} CLI and ensure it is on PATH; run `pohunek doctor` to verify"
            )),
        ),
        overlay::OverlayError::StateUnavailable { detail, .. } => ProtocolError::new(
            ErrorClass::Discovery,
            format!("{overlay}_state_unavailable"),
            format!("{overlay} local state is unavailable: {detail}"),
            Some(format!(
                "ensure the {overlay} daemon is running and this host is logged in; run `pohunek doctor` to verify"
            )),
        ),
        overlay::OverlayError::InvalidConfig { detail, .. } => ProtocolError::new(
            ErrorClass::Configuration,
            format!("{overlay}_configuration_invalid"),
            format!("{overlay} configuration is invalid: {detail}"),
            Some("fix the overlay configuration and restart the command".to_owned()),
        ),
        overlay::OverlayError::ListenerAddressMissing(_) => ProtocolError::new(
            ErrorClass::Discovery,
            format!("{overlay}_listener_address_missing"),
            format!("{overlay} has no safe local listener address"),
            Some(format!(
                "ensure {overlay} is connected and has a current member address; run `pohunek doctor`"
            )),
        ),
        overlay::OverlayError::HostUnknown { host, .. } => ProtocolError::new(
            ErrorClass::Discovery,
            "host_unknown",
            format!("host '{host}' was not found among {overlay} peers"),
            Some(
                "run `pohunek host list` to see reachable peers and check the host name".to_owned(),
            ),
        ),
        overlay::OverlayError::PeerCollision { host, .. } => ProtocolError::new(
            ErrorClass::Discovery,
            "overlay_peer_collision",
            format!("host '{host}' matches multiple peers inside {overlay}"),
            Some("use a unique provider peer identity or concrete discovered route".to_owned()),
        ),
        _ => ProtocolError::new(
            ErrorClass::Discovery,
            "overlay_error",
            error.to_string(),
            Some("run `pohunek doctor` and inspect the configured overlay".to_owned()),
        ),
    }
}

fn registry_error_to_protocol(error: &overlay::RegistryError) -> ProtocolError {
    match error {
        overlay::RegistryError::HostUnknown(host)
        | overlay::RegistryError::InvalidQualifiedHost(host) => ProtocolError::new(
            ErrorClass::Discovery,
            "host_unknown",
            format!("host '{host}' was not found among configured overlay peers"),
            Some("run `pohunek host list` to see reachable overlay peers".to_owned()),
        ),
        overlay::RegistryError::OverlayNotConfigured(overlay) => ProtocolError::new(
            ErrorClass::Discovery,
            "host_unknown",
            format!("overlay '{overlay}' is not configured"),
            Some("run `pohunek host list` to see configured overlay selectors".to_owned()),
        ),
        overlay::RegistryError::AmbiguousHost { candidates, .. } => ProtocolError::new(
            ErrorClass::Discovery,
            "overlay_host_ambiguous",
            error.to_string(),
            Some(ambiguity_hint(candidates)),
        ),
        overlay::RegistryError::HostUnavailable { failures, .. } if failures.len() == 1 => {
            overlay_error_to_protocol(&failures[0].error)
        }
        overlay::RegistryError::HostUnavailable { host, failures } => ProtocolError::new(
            ErrorClass::Discovery,
            "overlay_host_unavailable",
            format!(
                "no healthy overlay could resolve host '{host}'; failures: {}",
                failures
                    .iter()
                    .map(|failure| failure.overlay.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some("repair a configured overlay or use a concrete discovered route".to_owned()),
        ),
        _ => ProtocolError::new(
            ErrorClass::Configuration,
            "overlay_registry_invalid",
            error.to_string(),
            Some("fix the configured overlay registry before retrying".to_owned()),
        ),
    }
}

fn exact_version_range(version: ProtocolVersion) -> ProtocolVersionRange {
    ProtocolVersionRange::new(version, version)
        .expect("a protocol version is always a valid exact range")
}

/// Recovery text for an ambiguous host: the provider-qualified selectors that
/// each name one candidate.
fn ambiguity_hint(candidates: &[overlay::HostCandidate]) -> String {
    // A selector that could also match another candidate would not pick one
    // host: an FQDN selector is offered only when no other candidate of the same
    // overlay carries that name, because `<overlay>:fqdn~...` matches within its
    // overlay only. Peer identities are unique, and `local` may repeat across
    // overlays.
    let mut selectors = std::collections::BTreeSet::new();
    for candidate in candidates {
        let identity = &candidate.identity;
        let name_is_shared = candidates
            .iter()
            .filter(|other| {
                !std::ptr::eq(*other, candidate)
                    && !other.identity.local
                    && other.overlay == candidate.overlay
            })
            .any(|other| other.identity.name.eq_ignore_ascii_case(&identity.name));
        if identity.local || identity.selector_is_peer_id || !name_is_shared {
            selectors.extend(candidate.qualified_selector());
        }
    }
    if selectors.is_empty() {
        "select the provider-qualified selector shown by `pohunek host list`".to_owned()
    } else {
        format!(
            "select one host with a provider-qualified selector: {}",
            selectors.into_iter().collect::<Vec<_>>().join(", ")
        )
    }
}

/// Message of a lifecycle deadline that elapsed while the daemon `action` was
/// still pending.
/// The overlay and socket address a failed connection dialled, for messages.
fn describe_route(overlay: Option<&str>, address: IpAddr, port: u16) -> String {
    let target = SocketAddr::new(address, port);
    match overlay {
        Some(overlay) => format!("overlay {overlay} at {target}"),
        None => format!("at {target}"),
    }
}

fn lifecycle_timeout_message(host: Option<&str>, timeout: Duration, action: &str) -> String {
    host.map_or_else(
        || format!("timed out after {timeout:?} waiting for the local daemon to {action}"),
        |host| format!("timed out after {timeout:?} waiting for host '{host}' to {action}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use overlay::{CandidateIdentity, HostCandidate, OverlayError, OverlayId};

    fn candidate(identity: CandidateIdentity) -> HostCandidate {
        HostCandidate {
            overlay: OverlayId::new("netbird").expect("overlay id"),
            identity,
        }
    }

    #[test]
    fn an_ambiguity_hint_offers_only_selectors_that_name_one_candidate() {
        let twins = [
            candidate(CandidateIdentity::peer(None, Some("twin.test"))),
            candidate(CandidateIdentity::peer(None, Some("twin.test"))),
            candidate(CandidateIdentity::peer(Some("key-3"), Some("twin.test"))),
        ];
        let hint = ambiguity_hint(&twins);
        assert!(hint.contains("netbird:peer~"), "{hint}");
        assert!(!hint.contains("netbird:fqdn~"), "{hint}");

        let indistinguishable = [
            candidate(CandidateIdentity::peer(None, Some("twin.test"))),
            candidate(CandidateIdentity::peer(None, Some("twin.test"))),
        ];
        let cased = [
            candidate(CandidateIdentity::peer(None, Some("box.test"))),
            candidate(CandidateIdentity::peer(None, Some("BOX.test"))),
        ];
        assert!(!ambiguity_hint(&cased).contains("fqdn~"));
        assert_eq!(
            ambiguity_hint(&indistinguishable),
            "select the provider-qualified selector shown by `pohunek host list`"
        );
    }

    fn netbird_id() -> OverlayId {
        OverlayId::new("netbird").expect("id")
    }

    #[test]
    fn client_descriptor_exhaustion_does_not_recommend_starting_daemon() {
        let err = ClientError::ClientFileDescriptorsExhausted {
            socket: PathBuf::from("/run/pohunek/daemon.sock"),
            source: io::Error::from_raw_os_error(libc::EMFILE),
        };

        let structured = err.to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Runtime);
        assert_eq!(structured.code, "client_file_descriptors_exhausted");
        let recover = structured
            .recover
            .as_deref()
            .expect("descriptor exhaustion carries a recovery hint");
        assert!(recover.contains("RLIMIT_NOFILE"), "recover: {recover}");
        assert!(
            !recover.contains("daemon start"),
            "recover must not imply the daemon stopped: {recover}"
        );
    }

    #[test]
    fn system_descriptor_exhaustion_has_distinct_diagnostic() {
        let err = ClientError::SystemFileDescriptorsExhausted {
            socket: PathBuf::from("/run/pohunek/daemon.sock"),
            source: io::Error::from_raw_os_error(libc::ENFILE),
        };

        let structured = err.to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Runtime);
        assert_eq!(structured.code, "system_file_descriptors_exhausted");
        let recover = structured
            .recover
            .as_deref()
            .expect("system descriptor exhaustion carries a recovery hint");
        assert!(recover.contains("system-wide"), "recover: {recover}");
        assert!(
            !recover.contains("daemon start"),
            "recover must not imply the daemon stopped: {recover}"
        );
    }

    /// Documented `class/code` of each SDK error that carries no recovery
    /// contract of its own, with the message fragments it must keep.
    #[test]
    fn errors_map_to_their_documented_class_and_code() {
        let cases = [
            (
                "framing",
                ClientError::Framing("invalid line".to_owned()),
                ErrorClass::Transport,
                "framing",
                vec![],
            ),
            (
                "overlay CLI missing",
                ClientError::Overlay(OverlayError::CliMissing(netbird_id())),
                ErrorClass::Discovery,
                "netbird_cli_missing",
                vec![],
            ),
            (
                "overlay state unavailable keeps the parse detail",
                ClientError::Overlay(OverlayError::StateUnavailable {
                    overlay: netbird_id(),
                    detail: "failed to parse status: bad json".to_owned(),
                }),
                ErrorClass::Discovery,
                "netbird_state_unavailable",
                vec!["bad json"],
            ),
            (
                "remote discovery failed keeps the detail",
                ClientError::RemoteDiscoveryFailed {
                    detail: "blocking task was cancelled".to_owned(),
                },
                ErrorClass::Discovery,
                "remote_discovery_failed",
                vec!["blocking task"],
            ),
            (
                "overlay host unknown names the host",
                ClientError::Overlay(OverlayError::HostUnknown {
                    host: "build-box".to_owned(),
                    overlay: netbird_id(),
                }),
                ErrorClass::Discovery,
                "host_unknown",
                vec!["build-box"],
            ),
            (
                "host unreachable names the host and appends the source",
                ClientError::HostUnreachable {
                    host: "build-box".to_owned(),
                    overlay: Some("netbird".to_owned()),
                    address: "100.64.0.9".parse().expect("address"),
                    port: 18722,
                    source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
                },
                ErrorClass::Transport,
                "host_unreachable",
                vec![
                    "build-box",
                    "connection refused",
                    "netbird",
                    "100.64.0.9:18722",
                ],
            ),
            (
                "remote daemon unavailable names the host",
                ClientError::RemoteDaemonUnavailable {
                    host: "build-box".to_owned(),
                },
                ErrorClass::Daemon,
                "remote_daemon_unavailable",
                vec!["build-box"],
            ),
        ];

        for (name, error, class, code, fragments) in cases {
            let structured = error.to_protocol_error();
            assert_eq!(structured.class, class, "{name}");
            assert_eq!(structured.code, code, "{name}");
            for fragment in fragments {
                assert!(
                    structured.msg.contains(fragment),
                    "{name}: msg must contain {fragment:?}: {}",
                    structured.msg
                );
            }
        }
    }

    #[test]
    fn invalid_remote_route_maps_to_non_retry_configuration_error() {
        let structured = ClientError::InvalidRemoteRoute {
            route: "netbird:stable-key@0".to_owned(),
            detail: "the port must be a non-zero u16",
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Configuration);
        assert_eq!(structured.code, "invalid_remote_route");
        assert!(structured
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("<non-zero-port>")));
    }
}
