//! The native service manager and the local daemon, as the engine sees them.
//!
//! [`Backend`] bundles the platform daemon supervisor (systemd unit or
//! launchd agent), the platform worker supervisor of the same namespace, and
//! a [`Control`] channel to the daemon's socket. Every native operation goes
//! through the platform backends; nothing here runs `systemctl` or
//! `launchctl`.

// Rust guideline compliant 2026-09-24

use std::fmt::Debug;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use pohunek_client::{Client, ClientError, ClientOptions, LOCAL_HOST};
use pohunek_platform::supervisor::{DaemonSupervisor, Namespace, Supervisor};
use protocol::{method, DaemonHealthResult, SessionId, SessionInfo, SessionListParams};

use super::context::Context;
use super::error::{supervisor_error, Error};
use super::settings;

/// A pending daemon control call.
pub type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, ClientError>> + Send + 'a>>;

/// Requests the installer sends to the local daemon.
pub trait Control: Debug + Send + Sync {
    /// Returns the daemon's health, including its version.
    fn health(&self) -> Call<'_, DaemonHealthResult>;

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
        let options = ClientOptions::default()
            .with_request_timeout(settings::CONTROL_REQUEST_TIMEOUT)
            .with_connect_timeout(settings::CONTROL_REQUEST_TIMEOUT);
        Client::connect_with_options(LOCAL_HOST, &self.socket, options).await
    }
}

impl Control for SocketControl {
    fn health(&self) -> Call<'_, DaemonHealthResult> {
        Box::pin(async move { self.connect().await?.call::<method::DaemonHealth>(()).await })
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
    #[expect(
        clippy::unused_async,
        reason = "one signature for both targets; systemd connects asynchronously"
    )]
    pub async fn connect(
        context: &Context,
        namespace: &Namespace,
        launchctl_deadline: Duration,
    ) -> Result<Self, Error> {
        use pohunek_platform::supervisor::launchd::{LaunchdDaemon, LaunchdSupervisor};

        let daemon = LaunchdDaemon::new(
            namespace,
            context.uid(),
            context.supervisor_dir().to_path_buf(),
            launchctl_deadline,
        )
        .map_err(|source| supervisor_error("connect", source))?;
        let workers = LaunchdSupervisor::new(
            namespace.clone(),
            context.uid(),
            context.paths().launchd_definitions_dir(),
            context.paths().launchd_log_dir(),
            launchctl_deadline,
        )
        .map_err(|source| supervisor_error("connect", source))?;
        Ok(Self::new(
            Box::new(daemon),
            Box::new(workers),
            Box::new(SocketControl::new(context.paths().socket.clone())),
        ))
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
