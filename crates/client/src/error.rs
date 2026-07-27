//! Typed SDK client errors.

use std::io;
use std::path::PathBuf;

use protocol::{ErrorClass, ProtocolError};

/// Errors raised by SDK client transport and remote-host discovery.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
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

    /// `NetBird` discovery failed before a remote daemon could be dialed.
    #[error(transparent)]
    Netbird(#[from] netbird::NetbirdError),

    /// Remote-host discovery failed inside the async blocking boundary.
    #[error("remote host discovery failed: {detail}")]
    RemoteDiscoveryFailed {
        /// Non-secret detail describing the discovery failure.
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
    pub fn to_protocol_error(&self) -> protocol::ProtocolError {
        match self {
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
            ClientError::Netbird(err) => netbird_error_to_protocol_error(err),
            ClientError::RemoteDiscoveryFailed { detail } => ProtocolError::new(
                ErrorClass::Discovery,
                "remote_discovery_failed",
                format!("remote host discovery failed: {detail}"),
                Some(
                    "retry the remote request; if it persists, check the local NetBird state"
                        .to_owned(),
                ),
            ),
            ClientError::HostUnreachable { host, source } => {
                let mut err = ProtocolError::host_unreachable(host);
                err.msg = format!("{}: {source}", err.msg);
                err
            }
            ClientError::RemoteDaemonUnavailable { host } => {
                ProtocolError::remote_daemon_unavailable(host)
            }
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

fn netbird_error_to_protocol_error(err: &netbird::NetbirdError) -> ProtocolError {
    match err {
        netbird::NetbirdError::CliMissing => ProtocolError::netbird_cli_missing(),
        netbird::NetbirdError::StateUnavailable(detail) | netbird::NetbirdError::Parse(detail) => {
            ProtocolError::netbird_state_unavailable(detail.clone())
        }
        netbird::NetbirdError::InvalidConfig(detail) => ProtocolError::new(
            ErrorClass::Configuration,
            "netbird_invalid_config",
            detail.clone(),
            Some("fix the invalid NetBird-related configuration and retry".to_owned()),
        ),
        netbird::NetbirdError::HostUnknown(host) => ProtocolError::host_unknown(host),
    }
}

#[cfg(test)]
mod tests {
    use netbird::NetbirdError;
    use protocol::ProtocolVersion;

    use super::*;

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
    fn netbird_cli_missing_maps_to_discovery_code() {
        let structured = ClientError::from(NetbirdError::CliMissing).to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_cli_missing");
    }

    #[test]
    fn netbird_state_unavailable_maps_to_discovery_code() {
        let structured =
            ClientError::from(NetbirdError::StateUnavailable("daemon down".to_owned()))
                .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_state_unavailable");
    }

    #[test]
    fn netbird_parse_error_maps_to_state_unavailable_and_includes_detail() {
        let structured =
            ClientError::from(NetbirdError::Parse("bad json".to_owned())).to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Discovery);
        assert_eq!(structured.code, "netbird_state_unavailable");
        assert!(
            structured.msg.contains("bad json"),
            "msg includes parse detail: {}",
            structured.msg
        );
    }

    #[test]
    fn netbird_invalid_config_maps_to_configuration_error() {
        let structured = ClientError::from(NetbirdError::InvalidConfig("bad port".to_owned()))
            .to_protocol_error();

        assert_eq!(structured.class, ErrorClass::Configuration);
        assert_eq!(structured.code, "netbird_invalid_config");
        assert!(
            structured.msg.contains("bad port"),
            "msg includes config detail: {}",
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
    fn host_unknown_maps_to_discovery_code_and_names_host() {
        let structured = ClientError::from(NetbirdError::HostUnknown("build-box".to_owned()))
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
    fn remote_protocol_preserves_source_contract_and_adds_host_context() {
        let source = ProtocolError::version_mismatch(ProtocolVersion(1), ProtocolVersion(2));
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
            structured.msg.contains("incompatible"),
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
