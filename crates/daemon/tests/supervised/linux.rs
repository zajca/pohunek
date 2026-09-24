//! systemd side of the supervised lifecycle fixture.
//!
//! Workers are transient units of the real user manager and the daemon is a
//! namespaced unit written to `$XDG_RUNTIME_DIR/systemd/user` of the real
//! session: the manager only loads units from its search path, which never
//! includes the fixture's temporary config home. The unit names embed the
//! installation namespace, so they never collide with a real installation.
//!
//! Requirements: `POHUNEK_SYSTEMD_E2E=1` and a running user manager whose
//! environment carries `DBUS_SESSION_BUS_ADDRESS` (the daemon runs with the
//! fixture's temporary `XDG_RUNTIME_DIR`, so it cannot fall back to
//! `$XDG_RUNTIME_DIR/bus`).

// Rust guideline compliant 2026-09-24

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pohunek_paths::BasePaths;
use pohunek_platform::supervisor::systemd::{SystemdDaemon, SystemdSupervisor};
use pohunek_platform::supervisor::{DaemonSupervisor, JobLogs, Namespace, Supervisor, WorkerKey};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use super::MANAGER_CALL_TIMEOUT;

/// Environment variable that opts into the real user-manager tests.
const E2E_VARIABLE: &str = "POHUNEK_SYSTEMD_E2E";

/// Bytes relayed per read; D-Bus messages of the manager calls are far smaller.
const RELAY_BUFFER_BYTES: usize = 64 * 1024;

/// Units planted outside the daemon, stopped on teardown.
#[derive(Debug, Default)]
pub(crate) struct Extra {
    units: Vec<String>,
}

impl Extra {
    /// Records a unit to stop on teardown.
    pub(crate) fn unit(&mut self, name: String) {
        self.units.push(name);
    }

    /// Records a worker job of another namespace to retire on teardown.
    ///
    /// The job's slices belong to that namespace, so they are stopped too.
    pub(crate) fn job(&mut self, namespace: &Namespace, key: &WorkerKey) {
        self.units.extend([
            namespace.worker_unit(key),
            namespace.sessions_slice(),
            format!("pohunek-{}.slice", namespace.as_str()),
        ]);
    }
}

/// Fails the test unless the real user manager was explicitly opted into.
pub(crate) fn require() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "set {E2E_VARIABLE}=1 explicitly"
    );
}

/// systemd keeps no per-installation directories.
pub(crate) fn prepare(_paths: &BasePaths) {}

/// Worker jobs of `namespace` as transient units of the user manager.
pub(crate) async fn workers(_paths: &BasePaths, namespace: &Namespace) -> Box<dyn Supervisor> {
    Box::new(
        SystemdSupervisor::connect(namespace.clone(), MANAGER_CALL_TIMEOUT)
            .await
            .expect("connect the systemd user manager"),
    )
}

/// The namespaced daemon unit in the session's runtime unit directory.
pub(crate) async fn daemon(_root: &Path, namespace: &Namespace) -> Box<dyn DaemonSupervisor> {
    Box::new(
        SystemdDaemon::connect(namespace.clone(), unit_dir(), MANAGER_CALL_TIMEOUT)
            .await
            .expect("connect the systemd user manager"),
    )
}

/// systemd sends daemon stdio to the user journal.
pub(crate) fn daemon_logs(_paths: &BasePaths, _namespace: &Namespace) -> Option<JobLogs> {
    None
}

/// Worker output goes to the user journal; definitions name no log files.
pub(crate) fn worker_logs(
    _paths: &BasePaths,
    _namespace: &Namespace,
    _key: &WorkerKey,
) -> Option<JobLogs> {
    None
}

/// Worker jobs of another installation namespace sharing the user manager.
pub(crate) async fn foreign_workers(_root: &Path, namespace: &Namespace) -> Box<dyn Supervisor> {
    Box::new(
        SystemdSupervisor::connect(namespace.clone(), MANAGER_CALL_TIMEOUT)
            .await
            .expect("connect the systemd user manager"),
    )
}

/// Starts a running unit that matches the namespace's worker pattern but
/// names no valid session and generation; returns its unit name.
pub(crate) fn plant_malformed(
    extra: &mut Extra,
    _paths: &BasePaths,
    namespace: &Namespace,
) -> String {
    let name = format!("pohunek-{}-worker-malformed.service", namespace.as_str());
    let status = Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--unit",
            &name,
            "--property",
            "Restart=no",
        ])
        .args(["/bin/sleep", "600"])
        .status()
        .expect("run systemd-run");
    assert!(status.success(), "systemd-run {name} failed: {status}");
    extra.unit(name.clone());
    name
}

/// Whether a planted unit is still active.
pub(crate) fn planted_alive(name: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "--quiet", "is-active", name])
        .status()
        .expect("run systemctl")
        .success()
}

/// Transient units keep no definition files.
pub(crate) fn definition_labels(_paths: &BasePaths) -> Option<Vec<String>> {
    None
}

/// Retires every job of `namespace` and removes the daemon unit.
pub(crate) async fn teardown(
    _root: &Path,
    _paths: &BasePaths,
    namespace: &Namespace,
    extra: Extra,
) {
    if let Ok(daemon) =
        SystemdDaemon::connect(namespace.clone(), unit_dir(), MANAGER_CALL_TIMEOUT).await
    {
        // Teardown only: a failure is reported by the fallback below.
        let _uninstalled = daemon.uninstall().await;
    }
    if let Ok(workers) = SystemdSupervisor::connect(namespace.clone(), MANAGER_CALL_TIMEOUT).await {
        if let Ok(jobs) = workers.discover().await {
            for job in jobs {
                let _retired = workers.retire(&job.id).await;
            }
        }
    }
    // Catch-all for units the typed backends could not parse or reach.
    let mut units = extra.units;
    units.extend([
        namespace.daemon_unit(),
        namespace.worker_unit_pattern(),
        namespace.sessions_slice(),
        format!("pohunek-{}.slice", namespace.as_str()),
    ]);
    for unit in units {
        for action in ["stop", "reset-failed"] {
            let _status = Command::new("systemctl")
                .args(["--user", "--quiet", action, unit.as_str()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// `$XDG_RUNTIME_DIR/systemd/user` of the real user session.
fn unit_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR of the user session"))
        .join("systemd/user")
}

/// Session bus address of the real user manager, as a socket path.
fn session_bus() -> PathBuf {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .expect("DBUS_SESSION_BUS_ADDRESS names the user bus");
    address
        .split(';')
        .filter_map(|entry| entry.strip_prefix("unix:"))
        .flat_map(|entry| entry.split(','))
        .find_map(|pair| pair.strip_prefix("path="))
        .map(PathBuf::from)
        .expect("the session bus is a unix:path= address")
}

/// Byte-level relay between a daemon and the real session bus.
///
/// D-Bus authenticates the relay's own connection, which has the same UID,
/// so relayed traffic is indistinguishable from a direct connection. While
/// paused, neither direction is read: calls reach the manager only after
/// [`resume`](Self::resume), so the daemon observes its configured D-Bus call
/// deadline expiring exactly as with an unresponsive user manager.
#[derive(Debug)]
pub(crate) struct BusRelay {
    path: PathBuf,
    paused: Arc<watch::Sender<bool>>,
    task: tokio::task::JoinHandle<()>,
}

impl BusRelay {
    /// Listens on `path` and relays every connection to the session bus.
    pub(crate) fn start(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).expect("bind the bus relay");
        let upstream = session_bus();
        let paused = Arc::new(watch::Sender::new(false));
        let relay_paused = Arc::clone(&paused);
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, _address)) = listener.accept().await else {
                    return;
                };
                let Ok(server) = UnixStream::connect(&upstream).await else {
                    continue;
                };
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                tokio::spawn(pump(client_read, server_write, relay_paused.subscribe()));
                tokio::spawn(pump(server_read, client_write, relay_paused.subscribe()));
            }
        });
        Self { path, paused, task }
    }

    /// Address to pass as `DBUS_SESSION_BUS_ADDRESS`.
    pub(crate) fn address(&self) -> String {
        format!("unix:path={}", self.path.display())
    }

    /// Stops relaying in both directions.
    pub(crate) fn pause(&self) {
        self.paused.send_replace(true);
    }

    /// Relays again, delivering everything held while paused.
    pub(crate) fn resume(&self) {
        self.paused.send_replace(false);
    }
}

impl Drop for BusRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Copies one direction of a relayed connection while not paused.
async fn pump(
    mut from: tokio::net::unix::OwnedReadHalf,
    mut to: tokio::net::unix::OwnedWriteHalf,
    mut paused: watch::Receiver<bool>,
) {
    let mut buffer = vec![0_u8; RELAY_BUFFER_BYTES];
    loop {
        if paused.wait_for(|paused| !paused).await.is_err() {
            return;
        }
        let count = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        // A read already pending when the relay paused is held, not sent.
        if paused.wait_for(|paused| !paused).await.is_err() {
            return;
        }
        if to.write_all(&buffer[..count]).await.is_err() {
            return;
        }
    }
}
