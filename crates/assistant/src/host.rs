//! Host addressing, connection options and runtime capability predicates.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_client::{Client, ClientOptions, OriginSource};
use protocol::{AgentKind, AgentRuntime};
use serde::{Deserialize, Serialize};

use crate::AssistantError;

/// Default time allowed to establish a daemon connection.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Default time allowed for one daemon request.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Default period of the full-state reconcile read of a host worker.
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
/// Default first reconnect delay of a host worker.
const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Default ceiling of the reconnect delay of a host worker.
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Connection and reconciliation timing for host workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub reconcile_interval: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// Where SDK requests take their origin from. Process-local policy, not
    /// part of the serialized form.
    #[serde(skip)]
    pub origin_source: OriginSource,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reconcile_interval: DEFAULT_RECONCILE_INTERVAL,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
            origin_source: OriginSource::default(),
        }
    }
}

impl ConnectionOptions {
    fn client(self) -> ClientOptions {
        ClientOptions::default()
            .with_connect_timeout(self.connect_timeout)
            .with_request_timeout(self.request_timeout)
            .with_origin_source(self.origin_source)
    }
}

/// Stable host key used to identify a host across client state and subscriptions.
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
    /// Remote daemon over a trusted TCP route with a policy-resolved attach selector.
    Tcp {
        addr: SocketAddr,
        attach_host: String,
    },
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
        let id = id.into();
        Self {
            id: HostId::new(id.clone()),
            transport: HostTransport::Tcp {
                addr,
                attach_host: id,
            },
        }
    }

    /// Build a trusted TCP config whose external attach command re-resolves a
    /// provider-qualified selector through CLI overlay policy.
    #[must_use]
    pub fn tcp_with_attach_host(
        id: impl Into<String>,
        addr: SocketAddr,
        attach_host: impl Into<String>,
    ) -> Self {
        Self {
            id: HostId::new(id),
            transport: HostTransport::Tcp {
                addr,
                attach_host: attach_host.into(),
            },
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
    pub fn attach_host(&self) -> String {
        match &self.transport {
            HostTransport::Local { .. } => String::new(),
            HostTransport::Remote { host, .. } => host.clone(),
            HostTransport::Tcp { attach_host, .. } => attach_host.clone(),
        }
    }
}

/// Returns whether a runtime can be selected for a new session.
///
/// A runtime is launchable when the host reports it available and its version
/// policy does not refuse it: `supported == Some(false)` means the daemon found
/// the executable but will not launch it, while `None` means the runtime has no
/// version policy. A runtime with a policy always reports `Some(_)` once it is
/// available, so no runtime id is special-cased here. A future unknown compiled
/// base fails closed.
#[must_use]
pub fn runtime_is_launchable(runtime: &AgentRuntime) -> bool {
    runtime.available
        && runtime.supported != Some(false)
        && !matches!(runtime.agent_base.as_ref(), Some(AgentKind::Unknown(_)))
}

/// Returns whether a launchable runtime can host the assistant.
///
/// Shell-backed profiles are excluded even when their profile name is not the
/// built-in `shell` name. Profiles without `agent_base` are excluded only by
/// the built-in `shell` name.
#[must_use]
pub fn runtime_is_assistant_capable(runtime: &AgentRuntime) -> bool {
    runtime.agent != "shell"
        && runtime.agent_base.as_ref() != Some(&AgentKind::Shell)
        && runtime_is_launchable(runtime)
}

/// Open a daemon client for `config` using the timeouts and origin policy of
/// `options`.
///
/// # Errors
///
/// Returns [`AssistantError::Client`] when the transport cannot be established.
pub async fn connect_client(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<Client, AssistantError> {
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
