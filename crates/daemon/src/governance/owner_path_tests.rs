//! Owner-path regressions for unavailable relay governance.

// Rust guideline compliant 2026-09-04

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use futures::{SinkExt, StreamExt};
use protocol::{
    method, AttachHeader, EnrollmentStatus, HostGovernanceStatus, HostOwner, PrincipalId,
    ProposalExpiry, ProposalId, ProposalNonce, RelayId, Request, Response, SessionAttachParams,
    SessionAttachResult, SessionDetachParams, SessionInfo, SessionListParams, SessionNewParams,
    SessionState, SessionStopResult, TransferCoordinates, TransferProposal,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;

use super::{
    GovernanceTransitionError, HostGovernanceError, HostGovernanceService, LocalGovernanceCommand,
    LocalGovernanceConfirmation, RelayProjection,
};
use crate::api::{ControlServer, DaemonState, HealthInfo, RemoteServer};
use crate::procwatch::LinuxInspector;
use crate::runtime::{SubprocessWorkerEnvironment, SubprocessWorkerLauncher};
use crate::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

/// Allows PTY-backed shells enough time to leave their process group during cleanup.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// Bounds each control request write and response read to avoid a wedged test client.
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds a real raw attach read without relying on an unbounded test hang.
const ATTACH_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds server cleanup after cancellation so a broken accept loop is visible.
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Isolates test host state and makes the server health response identifiable.
const TEST_DAEMON_VERSION: &str = "owner-path-governance-test";
/// Cargo's workspace-local default output directory when no target override is configured.
const DEFAULT_CARGO_TARGET_DIRECTORY: &str = "target";
/// The real session worker is built in Cargo's debug profile for these tests.
const CARGO_DEBUG_DIRECTORY: &str = "debug";
/// The real worker binary spawned by the owner-path regression suite.
const WORKER_BINARY_NAME: &str = "pohunek-sessiond";

type TestResult<T> = Result<T, TestError>;

/// A bounded owner-path test operation failed.
#[derive(Debug, thiserror::Error)]
enum TestError {
    /// A bounded operation did not complete before the test deadline.
    #[error("{operation} did not complete within {CONTROL_IO_TIMEOUT:?}")]
    ControlTimeout { operation: &'static str },
    /// The control connection ended before a response was available.
    #[error("{operation} closed before returning a response")]
    ControlClosed { operation: &'static str },
    /// A control operation failed with a transport or framing error.
    #[error("{operation} failed: {detail}")]
    Control {
        operation: &'static str,
        detail: String,
    },
    /// A session result did not satisfy the owner-path regression contract.
    #[error("owner-path assertion failed: {detail}")]
    Assertion { detail: String },
    /// The bounded server shutdown did not complete.
    #[error("owner servers did not stop within {SERVER_SHUTDOWN_TIMEOUT:?}")]
    ServerShutdownTimeout,
    /// The server task failed while stopping.
    #[error("owner server task failed during shutdown: {detail}")]
    ServerShutdown { detail: String },
}

#[derive(Clone, Copy)]
enum GovernanceCondition {
    Quarantined,
    LocallyUnenrolled,
}

#[derive(Clone, Copy)]
enum OwnerTransport {
    Unix,
    Tcp,
}

impl GovernanceCondition {
    const fn name(self) -> &'static str {
        match self {
            Self::Quarantined => "quarantined",
            Self::LocallyUnenrolled => "locally-unenrolled",
        }
    }

    const fn status(self) -> EnrollmentStatus {
        match self {
            Self::Quarantined => EnrollmentStatus::Quarantined,
            Self::LocallyUnenrolled => EnrollmentStatus::LocallyUnenrolled,
        }
    }
}

struct Servers {
    _root: TempDir,
    socket: PathBuf,
    tcp_addr: SocketAddr,
    sessions: SessionRegistry,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Servers {
    async fn shutdown(&mut self) -> TestResult<()> {
        self.shutdown.cancel();
        self.sessions.begin_daemon_shutdown();
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        match timeout(SERVER_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(TestError::ServerShutdown {
                detail: error.to_string(),
            }),
            Err(_) => {
                task.abort();
                let _ = timeout(SERVER_SHUTDOWN_TIMEOUT, task).await;
                Err(TestError::ServerShutdownTimeout)
            }
        }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.sessions.begin_daemon_shutdown();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[tokio::test]
async fn quarantined_governance_keeps_owner_sessions_available_over_unix() -> TestResult<()> {
    with_servers(GovernanceCondition::Quarantined, OwnerTransport::Unix).await
}

#[tokio::test]
async fn quarantined_governance_keeps_owner_sessions_available_over_tcp() -> TestResult<()> {
    with_servers(GovernanceCondition::Quarantined, OwnerTransport::Tcp).await
}

#[tokio::test]
async fn locally_unenrolled_governance_keeps_owner_sessions_available_over_unix() -> TestResult<()>
{
    with_servers(GovernanceCondition::LocallyUnenrolled, OwnerTransport::Unix).await
}

#[tokio::test]
async fn locally_unenrolled_governance_keeps_owner_sessions_available_over_tcp() -> TestResult<()> {
    with_servers(GovernanceCondition::LocallyUnenrolled, OwnerTransport::Tcp).await
}

async fn with_servers(condition: GovernanceCondition, transport: OwnerTransport) -> TestResult<()> {
    let mut servers = start_servers(condition).await;
    let result = exercise_owner_transport(&servers, transport).await;
    let shutdown = servers.shutdown().await;
    match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

async fn start_servers(condition: GovernanceCondition) -> Servers {
    let root = tempfile::Builder::new()
        .prefix("po-")
        .tempdir()
        .expect("create isolated owner-path root");
    let socket = root.path().join("daemon.sock");
    let governance = Arc::new(
        HostGovernanceService::open(root.path().join("host-state"))
            .await
            .expect("open real isolated governance state"),
    );
    apply_condition(&governance, condition).await;

    let registry = worker_backed_registry(&socket, root.path());
    let sessions = registry.clone();
    let state = DaemonState::new(
        HealthInfo::new(TEST_DAEMON_VERSION),
        registry,
        Arc::clone(&governance),
        crate::test_support::overlay_registry(),
    );
    let remote_state = state.clone();
    assert!(
        Arc::ptr_eq(&state.governance, &remote_state.governance),
        "both owner transports must share one governance service"
    );
    let unix = ControlServer::bind_with_state(&socket, state)
        .await
        .expect("bind owner unix server");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind owner TCP loopback listener");
    let remote = RemoteServer::from_listener(listener, remote_state);
    let tcp_addr = remote.local_addr();
    let shutdown = CancellationToken::new();
    let unix_shutdown = shutdown.clone();
    let tcp_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        tokio::join!(
            unix.serve(async move { unix_shutdown.cancelled().await }),
            remote.serve(async move { tcp_shutdown.cancelled().await }),
        );
    });

    Servers {
        _root: root,
        socket,
        tcp_addr,
        sessions,
        shutdown,
        task: Some(task),
    }
}

async fn apply_condition(service: &HostGovernanceService, condition: GovernanceCondition) {
    service
        .apply_local_confirmation(confirmation(LocalGovernanceCommand::Initial {
            relay_id: relay(1),
            owner: principal(2),
        }))
        .await
        .expect("confirm initial enrollment");
    let pending = service.status().expect("read pending governance status");
    service
        .apply_local_confirmation(confirmation(LocalGovernanceCommand::Activate {
            expected: pending,
        }))
        .await
        .expect("confirm enrollment activation");
    let active = service.status().expect("read active governance status");

    match condition {
        GovernanceCondition::Quarantined => {
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: transfer(&active, 4, 5, principal(3)),
                }))
                .await
                .expect("commit real relay governance before reconciliation");
            service
                .reconcile_projection(RelayProjection::Missing)
                .await
                .expect("reconcile missing projection into quarantine");
            let quarantined = service
                .status()
                .expect("read quarantined governance status");
            let transfer = transfer(&quarantined, 6, 7, principal(8));
            let error = service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::Transfer {
                    proposal: transfer,
                }))
                .await
                .expect_err("quarantine rejects relay governance transfer");
            assert!(
                matches!(
                    error,
                    HostGovernanceError::Transition(GovernanceTransitionError::ProposalMismatch)
                ),
                "quarantine must fail closed for relay governance transfers: {error}"
            );
        }
        GovernanceCondition::LocallyUnenrolled => {
            service
                .apply_local_confirmation(confirmation(LocalGovernanceCommand::LocallyUnenroll {
                    expected: active,
                }))
                .await
                .expect("confirm local unenrollment");
        }
    }

    let status = service
        .status()
        .expect("read conditioned governance status");
    assert_eq!(
        status
            .enrollment()
            .expect("conditioned governance remains auditable")
            .status(),
        condition.status(),
        "{} condition is durable before owner clients connect",
        condition.name()
    );
}

fn worker_backed_registry(socket: &Path, root: &Path) -> SessionRegistry {
    let environment = SubprocessWorkerEnvironment {
        runtime_home: root.join("r"),
        state_home: root.join("s"),
        data_home: root.join("d"),
        config_home: root.join("c"),
        cache_home: root.join("k"),
        daemon_socket: socket.to_path_buf(),
    };
    let config = SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", std::iter::empty::<&str>()),
        stop_grace: STOP_GRACE,
        socket_path: Some(socket.to_path_buf()),
        worker_runtime_root: Some(environment.runtime_home.join("pohunek/workers")),
        worker_state_root: Some(environment.state_home.join("pohunek/workers")),
        ..SessionRegistryConfig::default()
    };
    SessionRegistry::new_with_launcher_and_inspector(
        config,
        Arc::new(SubprocessWorkerLauncher::new(worker_binary(), environment)),
        Arc::new(LinuxInspector::new()),
    )
}

fn worker_binary() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("daemon crate is inside the workspace")
        .to_path_buf();
    let worker_override = std::env::var_os("POHUNEK_WORKER_BIN").map(PathBuf::from);
    let binary = resolve_worker_binary(
        worker_override.as_deref(),
        std::env::var_os("CARGO_TARGET_DIR")
            .as_deref()
            .map(Path::new),
        &workspace,
    );
    if worker_override.is_some() {
        return binary;
    }
    assert!(
        binary.is_file(),
        "build the real worker first with `cargo build -p pohunek-session-worker --bin pohunek-sessiond`, or set POHUNEK_WORKER_BIN"
    );
    binary
}

fn resolve_worker_binary(
    worker_override: Option<&Path>,
    cargo_target_dir: Option<&Path>,
    workspace: &Path,
) -> PathBuf {
    if let Some(worker_override) = worker_override {
        return worker_override.to_path_buf();
    }
    let target = match cargo_target_dir {
        Some(target) if target.is_absolute() => target.to_path_buf(),
        Some(target) => workspace.join(target),
        None => workspace.join(DEFAULT_CARGO_TARGET_DIRECTORY),
    };
    target.join(CARGO_DEBUG_DIRECTORY).join(WORKER_BINARY_NAME)
}

#[test]
fn worker_binary_resolver_preserves_an_explicit_worker_path() {
    let workspace = Path::new("/workspace");
    let override_path = Path::new("custom/pohunek-sessiond");

    assert_eq!(
        resolve_worker_binary(
            Some(override_path),
            Some(Path::new("target-dir")),
            workspace
        ),
        override_path
    );
}

#[test]
fn worker_binary_resolver_uses_the_workspace_target_by_default() {
    let workspace = Path::new("/workspace");

    assert_eq!(
        resolve_worker_binary(None, None, workspace),
        workspace.join("target/debug/pohunek-sessiond")
    );
}

#[test]
fn worker_binary_resolver_keeps_an_absolute_cargo_target_directory() {
    let workspace = Path::new("/workspace");
    let target = Path::new("/isolated/target");

    assert_eq!(
        resolve_worker_binary(None, Some(target), workspace),
        target.join("debug/pohunek-sessiond")
    );
}

#[test]
fn worker_binary_resolver_anchors_a_relative_cargo_target_directory_at_the_workspace() {
    let workspace = Path::new("/workspace");
    let target = Path::new(".cache/issue81/owner-path-target");

    assert_eq!(
        resolve_worker_binary(None, Some(target), workspace),
        workspace.join(target).join("debug/pohunek-sessiond")
    );
}

fn confirmation(command: LocalGovernanceCommand) -> LocalGovernanceConfirmation {
    LocalGovernanceConfirmation { command }
}

fn relay(byte: u8) -> RelayId {
    RelayId::parse(&identifier("relay_", byte)).expect("valid test relay identifier")
}

fn principal(byte: u8) -> HostOwner {
    HostOwner::Principal(
        PrincipalId::parse(&identifier("principal_", byte))
            .expect("valid test principal identifier"),
    )
}

fn transfer(
    status: &HostGovernanceStatus,
    proposal_byte: u8,
    nonce_byte: u8,
    target: HostOwner,
) -> TransferProposal {
    let enrollment = status
        .enrollment()
        .expect("quarantined host remains enrolled");
    TransferProposal::new(
        TransferCoordinates::new(
            enrollment.relay_id().clone(),
            status.host_id().clone(),
            status
                .owner_revision()
                .expect("enrolled host has owner revision"),
            status.owner().expect("enrolled host has owner").clone(),
            target,
        ),
        ProposalId::parse(&identifier("proposal_", proposal_byte))
            .expect("valid test proposal identifier"),
        ProposalNonce::parse(&identifier("nonce_", nonce_byte)).expect("valid test proposal nonce"),
        ProposalExpiry::parse("2030-01-01T00:00:00Z").expect("valid future proposal expiry"),
    )
}

fn identifier(prefix: &str, byte: u8) -> String {
    format!("{prefix}{}", BASE64_URL_SAFE_NO_PAD.encode([byte; 32]))
}

async fn exercise_owner_transport(servers: &Servers, transport: OwnerTransport) -> TestResult<()> {
    match transport {
        OwnerTransport::Unix => {
            let mut control = connect_unix(&servers.socket).await?;
            let socket = servers.socket.clone();
            exercise_owner_control(&mut control, move |stream_id| {
                let socket = socket.clone();
                let stream_id = stream_id.to_owned();
                async move { open_unix_attach(&socket, &stream_id).await }
            })
            .await
        }
        OwnerTransport::Tcp => {
            let mut control = connect_tcp(servers.tcp_addr).await?;
            let tcp_addr = servers.tcp_addr;
            exercise_owner_control(&mut control, move |stream_id| {
                let stream_id = stream_id.to_owned();
                async move { open_tcp_attach(tcp_addr, &stream_id).await }
            })
            .await
        }
    }
}

async fn connect_unix(socket: &Path) -> TestResult<Framed<UnixStream, LinesCodec>> {
    let stream = timeout(CONTROL_IO_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "connect owner Unix control client",
        })?
        .map_err(|error| TestError::Control {
            operation: "connect owner Unix control client",
            detail: error.to_string(),
        })?;
    Ok(Framed::new(stream, LinesCodec::new()))
}

async fn connect_tcp(addr: SocketAddr) -> TestResult<Framed<TcpStream, LinesCodec>> {
    let stream = timeout(CONTROL_IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "connect owner TCP control client",
        })?
        .map_err(|error| TestError::Control {
            operation: "connect owner TCP control client",
            detail: error.to_string(),
        })?;
    Ok(Framed::new(stream, LinesCodec::new()))
}

async fn exercise_owner_control<S, Open, Future, Raw>(
    control: &mut Framed<S, LinesCodec>,
    open_attach: Open,
) -> TestResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Open: FnOnce(&str) -> Future,
    Future: std::future::Future<Output = TestResult<Raw>>,
    Raw: AsyncRead + AsyncWrite + Unpin,
{
    let created = create_session(control).await?;
    if created.state != SessionState::Running {
        return cleanup_after_error(
            control,
            &created.id,
            None,
            TestError::Assertion {
                detail: format!("owner session did not start running: {created:?}"),
            },
        )
        .await;
    }
    let list = match list_sessions(control).await {
        Ok(list) => list,
        Err(error) => return cleanup_after_error(control, &created.id, None, error).await,
    };
    if !list.iter().any(|session| session.id == created.id) {
        return cleanup_after_error(
            control,
            &created.id,
            None,
            TestError::Assertion {
                detail: format!("owner list omitted created session: {list:?}"),
            },
        )
        .await;
    }

    let attach = match attach_session(control, &created.id).await {
        Ok(attach) => attach,
        Err(error) => return cleanup_after_error(control, &created.id, None, error).await,
    };
    let mut raw = match open_attach(&attach.stream_id).await {
        Ok(raw) => raw,
        Err(error) => {
            return cleanup_after_error(control, &created.id, Some(&attach.stream_id), error).await
        }
    };
    let marker = b"owner-path-governance-attach";
    let result = async {
        timeout(
            ATTACH_IO_TIMEOUT,
            raw.write_all(b"printf 'owner-path-governance-attach\\n'\\n"),
        )
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "write raw owner attach input",
        })?
        .map_err(|error| TestError::Control {
            operation: "write raw owner attach input",
            detail: error.to_string(),
        })?;
        let output = read_until_marker(&mut raw, marker).await?;
        if output.windows(marker.len()).any(|window| window == marker) {
            Ok(())
        } else {
            Err(TestError::Assertion {
                detail: format!(
                    "owner attach omitted live PTY output: {}",
                    String::from_utf8_lossy(&output)
                ),
            })
        }
    }
    .await;
    let cleanup = cleanup_session(control, &created.id, Some(&attach.stream_id)).await;
    drop(raw);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

async fn cleanup_after_error<S>(
    control: &mut Framed<S, LinesCodec>,
    session_id: &protocol::SessionId,
    stream_id: Option<&str>,
    error: TestError,
) -> TestResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let _ = cleanup_session(control, session_id, stream_id).await;
    Err(error)
}

async fn create_session<S>(control: &mut Framed<S, LinesCodec>) -> TestResult<SessionInfo>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = Request::new(
        "owner-path-session-new",
        method::SESSION_NEW,
        serde_json::to_value(SessionNewParams {
            name: None,
            agent: "shell".to_owned(),
            cwd: Some(std::env::temp_dir()),
            cols: 80,
            rows: 24,
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            input: None,
            metadata: BTreeMap::default(),
        })
        .expect("serialize real session parameters"),
    )
    .expect("construct real session request");
    serde_json::from_value(ok_payload(exchange(control, &request).await?)?).map_err(|error| {
        TestError::Control {
            operation: "deserialize session create response",
            detail: error.to_string(),
        }
    })
}

async fn list_sessions<S>(control: &mut Framed<S, LinesCodec>) -> TestResult<Vec<SessionInfo>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = Request::new(
        "owner-path-session-list",
        method::SESSION_LIST,
        serde_json::to_value(SessionListParams {
            filters: Vec::new(),
        })
        .expect("serialize session list parameters"),
    )
    .expect("construct real session list request");
    serde_json::from_value(ok_payload(exchange(control, &request).await?)?).map_err(|error| {
        TestError::Control {
            operation: "deserialize session list response",
            detail: error.to_string(),
        }
    })
}

async fn attach_session<S>(
    control: &mut Framed<S, LinesCodec>,
    session_id: &protocol::SessionId,
) -> TestResult<SessionAttachResult>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = Request::new(
        "owner-path-session-attach",
        method::SESSION_ATTACH,
        serde_json::to_value(SessionAttachParams {
            session_id: session_id.clone(),
            initial_dimensions: None,
            origin_session_id: None,
            origin_daemon_id: None,
            origin_worker_id: None,
        })
        .expect("serialize attach parameters"),
    )
    .expect("construct real attach request");
    serde_json::from_value(ok_payload(exchange(control, &request).await?)?).map_err(|error| {
        TestError::Control {
            operation: "deserialize session attach response",
            detail: error.to_string(),
        }
    })
}

async fn cleanup_session<S>(
    control: &mut Framed<S, LinesCodec>,
    session_id: &protocol::SessionId,
    stream_id: Option<&str>,
) -> TestResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let detach = match stream_id {
        Some(stream_id) => detach_stream(control, stream_id).await,
        None => Ok(()),
    };
    let stop = stop_session(control, session_id).await;
    match (detach, stop) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

async fn detach_stream<S>(control: &mut Framed<S, LinesCodec>, stream_id: &str) -> TestResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = Request::new(
        "owner-path-session-detach",
        method::SESSION_DETACH,
        serde_json::to_value(SessionDetachParams {
            stream_id: stream_id.to_owned(),
        })
        .expect("serialize detach parameters"),
    )
    .expect("construct real detach request");
    let result: protocol::SessionDetachResult = serde_json::from_value(ok_payload(
        exchange(control, &request).await?,
    )?)
    .map_err(|error| TestError::Control {
        operation: "deserialize session detach response",
        detail: error.to_string(),
    })?;
    if result.detached {
        Ok(())
    } else {
        Err(TestError::Assertion {
            detail: "owner detach did not remove the raw stream".to_owned(),
        })
    }
}

async fn stop_session<S>(
    control: &mut Framed<S, LinesCodec>,
    session_id: &protocol::SessionId,
) -> TestResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = Request::new(
        "owner-path-session-stop",
        method::SESSION_STOP,
        serde_json::to_value(session_id).expect("serialize session stop identifier"),
    )
    .expect("construct real stop request");
    let _: SessionStopResult = serde_json::from_value(ok_payload(
        exchange(control, &request).await?,
    )?)
    .map_err(|error| TestError::Control {
        operation: "deserialize session stop response",
        detail: error.to_string(),
    })?;
    Ok(())
}

async fn exchange<S>(control: &mut Framed<S, LinesCodec>, request: &Request) -> TestResult<Response>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let line = serde_json::to_string(request).expect("serialize control request");
    timeout(CONTROL_IO_TIMEOUT, control.send(line))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "send control request",
        })?
        .map_err(|error| TestError::Control {
            operation: "send control request",
            detail: error.to_string(),
        })?;
    let reply = timeout(CONTROL_IO_TIMEOUT, control.next())
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "receive control response",
        })?
        .ok_or(TestError::ControlClosed {
            operation: "receive control response",
        })?
        .map_err(|error| TestError::Control {
            operation: "frame control response",
            detail: error.to_string(),
        })?;
    serde_json::from_str(&reply).map_err(|error| TestError::Control {
        operation: "deserialize control response",
        detail: error.to_string(),
    })
}

fn ok_payload(response: Response) -> TestResult<Value> {
    response.into_result().map_err(|error| TestError::Control {
        operation: "read successful control response",
        detail: error.to_string(),
    })
}

async fn open_unix_attach(socket: &Path, stream_id: &str) -> TestResult<UnixStream> {
    let mut raw = timeout(ATTACH_IO_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "connect raw Unix attach",
        })?
        .map_err(|error| TestError::Control {
            operation: "connect raw Unix attach",
            detail: error.to_string(),
        })?;
    send_attach_prelude(&mut raw, stream_id).await?;
    Ok(raw)
}

async fn open_tcp_attach(addr: SocketAddr, stream_id: &str) -> TestResult<TcpStream> {
    let mut raw = timeout(ATTACH_IO_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "connect raw TCP attach",
        })?
        .map_err(|error| TestError::Control {
            operation: "connect raw TCP attach",
            detail: error.to_string(),
        })?;
    send_attach_prelude(&mut raw, stream_id).await?;
    Ok(raw)
}

async fn send_attach_prelude<S>(stream: &mut S, stream_id: &str) -> TestResult<()>
where
    S: AsyncWrite + Unpin,
{
    let header = serde_json::to_string(&AttachHeader {
        attach: stream_id.to_owned(),
    })
    .expect("serialize raw attach header");
    timeout(ATTACH_IO_TIMEOUT, stream.write_all(header.as_bytes()))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "write raw attach header",
        })?
        .map_err(|error| TestError::Control {
            operation: "write raw attach header",
            detail: error.to_string(),
        })?;
    timeout(ATTACH_IO_TIMEOUT, stream.write_all(b"\n"))
        .await
        .map_err(|_timeout| TestError::ControlTimeout {
            operation: "terminate raw attach header",
        })?
        .map_err(|error| TestError::Control {
            operation: "terminate raw attach header",
            detail: error.to_string(),
        })?;
    Ok(())
}

async fn read_until_marker<S>(stream: &mut S, marker: &[u8]) -> TestResult<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    timeout(ATTACH_IO_TIMEOUT, async {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stream
                .read(&mut buffer)
                .await
                .map_err(|error| TestError::Control {
                    operation: "read raw attach output",
                    detail: error.to_string(),
                })?;
            if count == 0 {
                return Err(TestError::ControlClosed {
                    operation: "read raw attach output",
                });
            }
            output.extend_from_slice(&buffer[..count]);
            if output.windows(marker.len()).any(|window| window == marker) {
                return Ok(output);
            }
        }
    })
    .await
    .map_err(|_timeout| TestError::ControlTimeout {
        operation: "read raw attach output",
    })?
}
