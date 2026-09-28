//! The native service manager and the local daemon, as the engine sees them.
//!
//! [`Backend`] bundles the platform daemon supervisor (systemd unit or
//! launchd agent), the platform worker supervisor of the same namespace, and
//! a [`Control`] channel to the daemon's socket. Every native operation goes
//! through the platform backends; nothing here runs `systemctl` or
//! `launchctl`.

// Rust guideline compliant 2026-09-26

use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use pohunek_client::{Client, ClientError, ClientOptions, RawStream, LOCAL_HOST};
use pohunek_platform::process::Pid;
use pohunek_platform::supervisor::{DaemonSupervisor, Namespace, Supervisor};
use protocol::{
    method, DaemonHealthResult, Request, Response, SessionId, SessionInfo, SessionListParams,
    MAX_CONTROL_LINE_BYTES,
};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use super::context::Context;
use super::error::{supervisor_error, Error};
use super::settings;

/// A pending daemon control call.
pub type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, ClientError>> + Send + 'a>>;

/// A `daemon.health` answer and the process that served it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// The daemon's answer, including its version.
    pub result: DaemonHealthResult,
    /// Process serving the socket, from the kernel's peer credentials on the
    /// connection that carried the answer.
    pub pid: Pid,
}

/// Requests the installer sends to the local daemon.
pub trait Control: Debug + Send + Sync {
    /// Returns the daemon's health and the process that answered it.
    ///
    /// A version alone does not identify the supervised daemon: any process
    /// of the same build may hold the socket. The caller compares
    /// [`Health::pid`] with the service manager's main process.
    fn health(&self) -> Call<'_, Health>;

    /// Lists every logical session.
    fn sessions(&self) -> Call<'_, Vec<SessionInfo>>;

    /// Stops one session through the daemon and its worker protocol.
    fn stop<'a>(&'a self, id: &'a str) -> Call<'a, ()>;
}

/// [`Control`] over the local daemon socket; each call opens a fresh connection.
#[derive(Debug, Clone)]
pub struct SocketControl {
    socket: PathBuf,
}

impl SocketControl {
    /// Creates a control channel for the daemon listening on `socket`.
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn connect(&self) -> Result<Client, ClientError> {
        Client::connect_with_options(LOCAL_HOST, &self.socket, options()).await
    }

    /// Sends `daemon.health` and reads the serving process on one connection.
    ///
    /// The SDK client keeps its socket private, so this exchange is framed
    /// here; asking the kernel on the same connection that carried the answer
    /// is what ties the version to a process. The peer is read after the
    /// answer and before the connection closes, as [`pohunek_platform::peer::server`]
    /// requires. No session-origin marker is attached: `daemon.health` is a
    /// read-only probe the daemon answers the same for every caller.
    async fn served_health(&self) -> Result<Health, ClientError> {
        let RawStream::Local(stream) =
            pohunek_client::connect_raw_local_with_options(&self.socket, options()).await?
        else {
            return Err(ClientError::Framing(
                "a local daemon connection did not yield a Unix socket".to_owned(),
            ));
        };
        let mut framed = Framed::new(
            stream,
            LinesCodec::new_with_max_length(MAX_CONTROL_LINE_BYTES),
        );
        let request = Request::new(
            pohunek_client::next_request_id(method::DAEMON_HEALTH),
            method::DAEMON_HEALTH,
            serde_json::Value::Null,
        )?;
        let exchange = exchange(&mut framed, &request);
        let response = tokio::time::timeout(settings::CONTROL_REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_elapsed| ClientError::RequestTimeout {
                host: None,
                timeout: settings::CONTROL_REQUEST_TIMEOUT,
            })??;
        let served_by = pohunek_platform::peer::server(framed.get_ref())
            .map_err(|error| ClientError::Io(std::io::Error::other(error)))?;
        if !request.version_range().contains(response.version()) {
            return Err(ClientError::ProtocolVersionMismatch {
                expected: request.version_range(),
                received: response.version(),
            });
        }
        let result = serde_json::from_value(response.into_result()?)?;
        Ok(Health {
            result,
            pid: served_by.pid,
        })
    }
}

/// Transport bounds of every installer request to the local daemon.
fn options() -> ClientOptions {
    ClientOptions::default()
        .with_request_timeout(settings::CONTROL_REQUEST_TIMEOUT)
        .with_connect_timeout(settings::CONTROL_REQUEST_TIMEOUT)
}

/// Writes `request` as one line and reads its correlated response line.
async fn exchange(
    framed: &mut Framed<UnixStream, LinesCodec>,
    request: &Request,
) -> Result<Response, ClientError> {
    let codec = |error: tokio_util::codec::LinesCodecError| ClientError::Framing(error.to_string());
    framed
        .send(serde_json::to_string(request)?)
        .await
        .map_err(codec)?;
    let line = framed
        .next()
        .await
        .ok_or_else(|| {
            ClientError::Framing("daemon closed the connection without a response".to_owned())
        })?
        .map_err(codec)?;
    let response: Response = serde_json::from_str(&line)?;
    if response.id() != request.id() {
        return Err(ClientError::Framing(format!(
            "daemon answered request '{}' with response '{}'",
            request.id(),
            response.id()
        )));
    }
    Ok(response)
}

impl Control for SocketControl {
    fn health(&self) -> Call<'_, Health> {
        Box::pin(self.served_health())
    }

    fn sessions(&self) -> Call<'_, Vec<SessionInfo>> {
        Box::pin(async move {
            self.connect()
                .await?
                .call::<method::SessionList>(SessionListParams::default())
                .await
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> Call<'a, ()> {
        Box::pin(async move {
            self.connect()
                .await?
                .call::<method::SessionStop>(SessionId(id.to_owned()))
                .await
                .map(drop)
        })
    }
}

/// The service manager and daemon channel of one installation namespace.
#[derive(Debug)]
pub struct Backend {
    daemon: Box<dyn DaemonSupervisor>,
    workers: Box<dyn Supervisor>,
    control: Box<dyn Control>,
}

impl Backend {
    /// Bundles explicit backends.
    #[must_use]
    pub fn new(
        daemon: Box<dyn DaemonSupervisor>,
        workers: Box<dyn Supervisor>,
        control: Box<dyn Control>,
    ) -> Self {
        Self {
            daemon,
            workers,
            control,
        }
    }

    /// Connects the native backends of this host for `namespace`.
    ///
    /// Every `launchctl` command ends after `launchctl_deadline` (macOS only).
    /// Linux uses the systemd user manager over D-Bus with the unit
    /// directory from `context`; macOS uses launchd `gui/<uid>` with the
    /// `LaunchAgents` directory from `context`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Supervisor`] when the service manager is unreachable.
    #[cfg(target_os = "linux")]
    pub async fn connect(
        context: &Context,
        namespace: &Namespace,
        _launchctl_deadline: Duration,
    ) -> Result<Self, Error> {
        use pohunek_platform::supervisor::systemd::{SystemdDaemon, SystemdSupervisor};

        let daemon = SystemdDaemon::connect(
            namespace.clone(),
            context.supervisor_dir().to_path_buf(),
            settings::SUPERVISOR_CALL_TIMEOUT,
        )
        .await
        .map_err(|source| supervisor_error("connect", source))?;
        let workers =
            SystemdSupervisor::connect(namespace.clone(), settings::SUPERVISOR_CALL_TIMEOUT)
                .await
                .map_err(|source| supervisor_error("connect", source))?;
        Ok(Self::new(
            Box::new(daemon),
            Box::new(workers),
            Box::new(SocketControl::new(context.paths().socket.clone())),
        ))
    }

    /// Connects the native backends of this host for `namespace`.
    ///
    /// Every `launchctl` command ends after `launchctl_deadline` (macOS only).
    /// Linux uses the systemd user manager over D-Bus with the unit
    /// directory from `context`; macOS uses launchd `gui/<uid>` with the
    /// `LaunchAgents` directory from `context`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Supervisor`] for an invalid directory.
    #[cfg(target_os = "macos")]
    pub fn connect(
        context: &Context,
        namespace: &Namespace,
        launchctl_deadline: Duration,
    ) -> std::future::Ready<Result<Self, Error>> {
        use pohunek_platform::supervisor::launchd::{LaunchdDaemon, LaunchdSupervisor};

        // launchd needs no connection, so the backend is ready immediately;
        // the future keeps one call shape with the systemd arm.
        let backend = LaunchdDaemon::new(
            namespace,
            context.uid(),
            context.supervisor_dir().to_path_buf(),
            launchctl_deadline,
        )
        .and_then(|daemon| {
            LaunchdSupervisor::new(
                namespace.clone(),
                context.uid(),
                context.paths().launchd_definitions_dir(),
                context.paths().launchd_log_dir(),
                launchctl_deadline,
            )
            .map(|workers| {
                Self::new(
                    Box::new(daemon),
                    Box::new(workers),
                    Box::new(SocketControl::new(context.paths().socket.clone())),
                )
            })
        })
        .map_err(|source| supervisor_error("connect", source));
        std::future::ready(backend)
    }

    /// Returns the daemon supervisor.
    #[must_use]
    pub fn daemon(&self) -> &dyn DaemonSupervisor {
        self.daemon.as_ref()
    }

    /// Returns the worker supervisor.
    #[must_use]
    pub fn workers(&self) -> &dyn Supervisor {
        self.workers.as_ref()
    }

    /// Returns the daemon control channel.
    #[must_use]
    pub fn control(&self) -> &dyn Control {
        self.control.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::UnixListener;

    use super::*;
    use crate::service::context::tests::temp_root;

    /// Answers one `daemon.health` line; `echo_id` false answers another id.
    async fn serve_health(listener: UnixListener, echo_id: bool) {
        let (stream, _address) = listener.accept().await.expect("accept");
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("request line");
        let request: serde_json::Value = serde_json::from_str(&line).expect("request JSON");
        assert_eq!(request["method"], method::DAEMON_HEALTH);
        let id = if echo_id {
            request["id"].as_str().expect("request id").to_owned()
        } else {
            "other-request".to_owned()
        };
        let result = DaemonHealthResult {
            status: "ok".to_owned(),
            daemon_version: "9.9.9".to_owned(),
            protocol_version: protocol::PROTOCOL_VERSION,
        };
        let response = Response::ok(
            protocol::PROTOCOL_VERSION,
            id,
            serde_json::to_value(result).expect("result JSON"),
        )
        .expect("response");
        let mut reply = serde_json::to_vec(&response).expect("response JSON");
        reply.push(b'\n');
        write.write_all(&reply).await.expect("write reply");
        // Keep the connection open until the client hangs up, as the daemon does.
        let mut rest = String::new();
        let read = reader
            .read_line(&mut rest)
            .await
            .expect("read until hang-up");
        assert_eq!(read, 0, "the client sends one request");
    }

    #[tokio::test]
    async fn health_names_the_process_serving_the_socket() {
        let (_temp, root) = temp_root();
        let socket = root.join("d.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(serve_health(listener, true));

        let health = SocketControl::new(socket)
            .health()
            .await
            .expect("health answer");
        assert_eq!(health.result.daemon_version, "9.9.9");
        assert_eq!(health.pid, std::process::id());
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn health_rejects_an_uncorrelated_answer() {
        let (_temp, root) = temp_root();
        let socket = root.join("d.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(serve_health(listener, false));

        let error = SocketControl::new(socket)
            .health()
            .await
            .expect_err("mismatched id");
        assert!(matches!(error, ClientError::Framing(_)), "{error:?}");
        server.await.expect("server task");
    }
}
