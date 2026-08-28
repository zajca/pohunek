//! Typed SDK client errors.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

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

    /// A daemon returned a typed protocol error.
    #[error("daemon error: {0}")]
    Protocol(#[from] ProtocolError),

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

    /// A `NetBird` TCP connection to the host's daemon port could not be opened.
    #[error("could not open a NetBird connection to host '{host}': {source}")]
    HostUnreachable {
        /// The requested host name.
        host: String,
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
            ClientError::Protocol(err) => err.clone(),
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
                    "retry the remote request; if it persists, check the local NetBird state"
                        .to_owned(),
                ),
            ),
            ClientError::InvalidDiscoveryOptions { detail } => ProtocolError::new(
                ErrorClass::Configuration,
                "invalid_discovery_options",
                format!("invalid discovery options: {detail}"),
                Some("fix the invalid discovery option before running discovery".to_owned()),
            ),
            ClientError::HostUnreachable { host, source } => {
                let mut err = ProtocolError::host_unreachable(host);
                err.msg = format!("{}: {source}", err.msg);
                err
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
        overlay::RegistryError::HostUnknown(host) => ProtocolError::new(
            ErrorClass::Discovery,
            "host_unknown",
            format!("host '{host}' was not found among configured overlay peers"),
            Some("run `pohunek host list` to see reachable overlay peers".to_owned()),
        ),
        overlay::RegistryError::AmbiguousHost { host, overlays } => ProtocolError::new(
            ErrorClass::Discovery,
            "overlay_host_ambiguous",
            format!("host '{host}' exists on multiple overlays: {overlays:?}"),
            Some("select the overlay-qualified address shown by `pohunek host list`".to_owned()),
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
        overlay::RegistryError::Empty
        | overlay::RegistryError::InvalidId { .. }
        | overlay::RegistryError::DuplicateId(_)
        | overlay::RegistryError::InvalidPort(_) => ProtocolError::new(
            ErrorClass::Configuration,
            "overlay_registry_invalid",
            error.to_string(),
            Some("fix the configured overlay registry before retrying".to_owned()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use overlay::{OverlayError, OverlayId};

    fn netbird_id() -> OverlayId {
        OverlayId::new("netbird").expect("id")
    }

    #[test]
    fn local_daemon_unreachable_maps_to_daemon_error_with_recovery_hint() {
        let err = ClientError::DaemonUnreachable {
            socket: PathBuf::from("/run/pohunek/daemon.sock"),
            source: io::Error::new(io::ErrorKind::NotFound, "socket missing"),
        };

        let structured = err.to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Daemon);
        assert_eq!(structured.code, "daemon_unreachable");
        let recover = structured
            .recover
            .as_deref()
            .expect("daemon-unreachable carries a recover hint");
        assert!(recover.contains("daemon start"), "recover: {recover}");
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

    #[test]
    fn local_framing_maps_to_transport_framing() {
        let structured = ClientError::Framing("invalid line".to_owned()).to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Transport);
        assert_eq!(structured.code, "framing");
    }

    #[test]
    fn overlay_cli_missing_maps_to_discovery_code() {
        let structured =
            ClientError::Overlay(OverlayError::CliMissing(netbird_id())).to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_cli_missing");
    }

    #[test]
    fn overlay_state_unavailable_maps_to_discovery_code() {
        let structured = ClientError::Overlay(OverlayError::StateUnavailable {
            overlay: netbird_id(),
            detail: "daemon down".to_owned(),
        })
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_state_unavailable");
    }

    #[test]
    fn overlay_parse_detail_is_preserved_without_flattening() {
        let structured = ClientError::Overlay(OverlayError::StateUnavailable {
            overlay: netbird_id(),
            detail: "failed to parse status: bad json".to_owned(),
        })
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_state_unavailable");
        assert!(
            structured.msg.contains("bad json"),
            "msg includes parse detail: {}",
            structured.msg
        );
    }

    #[test]
    fn remote_discovery_failed_maps_to_discovery_code_and_detail() {
        let structured = ClientError::RemoteDiscoveryFailed {
            detail: "blocking task was cancelled".to_owned(),
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "remote_discovery_failed");
        assert!(
            structured.msg.contains("blocking task"),
            "msg includes detail: {}",
            structured.msg
        );
    }

    #[test]
    fn invalid_discovery_options_maps_to_non_retry_configuration_error() {
        let structured = ClientError::InvalidDiscoveryOptions {
            detail: "discovery concurrency must be non-zero".to_owned(),
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Configuration);
        assert_eq!(structured.code, "invalid_discovery_options");
        assert_eq!(
            structured.recover.as_deref(),
            Some("fix the invalid discovery option before running discovery")
        );
    }

    #[test]
    fn host_unknown_maps_to_discovery_code_and_names_host() {
        let structured = ClientError::Overlay(OverlayError::HostUnknown {
            host: "build-box".to_owned(),
            overlay: netbird_id(),
        })
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "host_unknown");
        assert!(
            structured.msg.contains("build-box"),
            "msg names host: {}",
            structured.msg
        );
    }

    #[test]
    fn host_unreachable_maps_to_transport_code_names_host_and_appends_source_detail() {
        let structured = ClientError::HostUnreachable {
            host: "build-box".to_owned(),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused"),
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Transport);
        assert_eq!(structured.code, "host_unreachable");
        assert!(
            structured.msg.contains("build-box"),
            "msg names host: {}",
            structured.msg
        );
        assert!(
            structured.msg.contains("connection refused"),
            "msg includes source detail: {}",
            structured.msg
        );
    }

    #[test]
    fn remote_daemon_unavailable_maps_to_daemon_code_and_names_host() {
        let structured = ClientError::RemoteDaemonUnavailable {
            host: "build-box".to_owned(),
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Daemon);
        assert_eq!(structured.code, "remote_daemon_unavailable");
        assert!(
            structured.msg.contains("build-box"),
            "msg names host: {}",
            structured.msg
        );
    }

    #[test]
    fn request_timeout_maps_to_transient_transport_code() {
        let structured = ClientError::RequestTimeout {
            host: Some("build-box".to_owned()),
            timeout: Duration::from_secs(5),
        }
        .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Transport);
        assert_eq!(structured.code, "request_timeout");
        assert!(structured.msg.contains("build-box"));
        assert!(structured
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("reconcile")));
    }

    #[test]
    fn remote_protocol_preserves_source_contract_and_adds_host_context() {
        let source = ProtocolError::version_mismatch(
            ProtocolVersionRange::new(
                ProtocolVersion::new(1).expect("nonzero version"),
                ProtocolVersion::new(1).expect("nonzero version"),
            )
            .expect("valid exact range"),
            ProtocolVersionRange::new(
                ProtocolVersion::new(2).expect("nonzero version"),
                ProtocolVersion::new(2).expect("nonzero version"),
            )
            .expect("valid exact range"),
        );
        let structured = ClientError::RemoteProtocol {
            host: "build-box".to_owned(),
            source: source.clone(),
        }
        .to_protocol_error();

        assert_eq!(structured.class, source.class);
        assert_eq!(structured.code, source.code);
        assert_eq!(structured.recover, source.recover);
        assert!(
            structured.msg.contains("build-box"),
            "msg names host: {}",
            structured.msg
        );
        assert!(
            structured.msg.contains("does not overlap"),
            "msg preserves source detail: {}",
            structured.msg
        );
    }

    #[test]
    fn protocol_passthrough_keeps_source_code_unchanged() {
        let source = ProtocolError::agent_binary_missing("codex");
        let structured = ClientError::Protocol(source.clone()).to_protocol_error();

        assert_eq!(structured, source);
    }
}
