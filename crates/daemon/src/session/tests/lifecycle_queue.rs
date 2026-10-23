//! A stop or removal queued behind another lifecycle operation of the same
//! session outlives its caller's response deadline: the daemon keeps running
//! it after the caller gave up, so the caller's unknown outcome resolves to a
//! committed one and a repeated stop is harmless.
//!
//! The first stop parks between its in-memory and durable intent, holding the
//! session's lifecycle lock. Real clients on the control socket then queue a
//! second call behind it with an explicit short response deadline.

// Rust guideline compliant 2026-10-20

use super::*;
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource};
use protocol::{ProtocolError, SessionStopResult};

/// Response deadline of the queued caller, far shorter than the parked stop.
const QUEUED_CALLER_DEADLINE: Duration = Duration::from_millis(300);

struct Rig {
    registry: SessionRegistry,
    socket: PathBuf,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

impl Rig {
    async fn start() -> Self {
        let runtime_root = crate::test_support::thread_scoped_dir("pw-queue-");
        let socket = runtime_root.join("test-daemon.sock");
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "exec sleep 60"]),
            stop_grace: Duration::from_millis(50),
            worker_runtime_root: Some(runtime_root),
            ..SessionRegistryConfig::default()
        });
        let state = DaemonState::new(
            HealthInfo::new("test"),
            registry.clone(),
            Arc::new(crate::governance::HostGovernanceService::open_test()),
            crate::test_support::overlay_registry(),
            crate::api::OverlaySource::Default,
        );
        let server = crate::api::ControlServer::bind_with_state(&socket, state)
            .await
            .expect("control server binds");
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            server
                .serve(async move {
                    let _ = stop.await;
                })
                .await;
        });
        Self {
            registry,
            socket,
            shutdown: Some(shutdown),
            server,
        }
    }

    async fn client(&self, deadline: Duration) -> Client {
        let options = ClientOptions::default()
            .with_origin_source(OriginSource::Omitted)
            .with_request_timeout(deadline);
        let mut client = Client::connect_local_with_options(&self.socket, options)
            .await
            .expect("connect to the control server");
        client.handshake().await.expect("negotiate the protocol");
        client
    }

    async fn finish(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.server.await;
    }
}

/// Parks a first stop of `id` on the lifecycle lock; returns its task and gate.
async fn park_first_stop(
    registry: &SessionRegistry,
    id: &SessionId,
) -> (
    tokio::task::JoinHandle<Result<SessionStopResult, ProtocolError>>,
    crate::runtime::lifecycle::tests::StartGate,
) {
    let gate = registry.hold_next_stop_intent(id);
    let first = tokio::spawn({
        let registry = registry.clone();
        let id = id.clone();
        async move { registry.stop(&id).await }
    });
    gate.entered.notified().await;
    (first, gate)
}

#[tokio::test]
async fn a_queued_stop_outlives_its_callers_deadline_and_a_repeat_is_idempotent() {
    let rig = Rig::start().await;
    let created = rig.registry.create(params()).await.expect("create session");
    let (first, gate) = park_first_stop(&rig.registry, &created.id).await;

    let mut impatient = rig.client(QUEUED_CALLER_DEADLINE).await;
    let error = impatient
        .session_stop(created.id.clone())
        .await
        .expect_err("the queued stop cannot finish before its caller's deadline");
    assert!(
        matches!(error, ClientError::SessionStopOutcomeUnknown { .. }),
        "{error:?}"
    );

    gate.release.notify_one();
    assert!(
        first
            .await
            .expect("first stop joins")
            .expect("first stop succeeds")
            .stopped
    );
    let inspected = rig.registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.state, SessionState::Stopped);

    let mut patient = rig.client(pohunek_test_support::wait::HANG_GUARD).await;
    let repeated = patient
        .session_stop(created.id.clone())
        .await
        .expect("a repeated stop of a stopped session succeeds");
    assert!(!repeated.stopped, "nothing was left to stop");
    rig.finish().await;
}

#[tokio::test]
async fn a_queued_removal_still_completes_after_its_caller_gave_up() {
    let rig = Rig::start().await;
    let created = rig.registry.create(params()).await.expect("create session");
    let (first, gate) = park_first_stop(&rig.registry, &created.id).await;

    let mut impatient = rig.client(QUEUED_CALLER_DEADLINE).await;
    let error = impatient
        .session_remove(created.id.clone())
        .await
        .expect_err("the queued removal cannot finish before its caller's deadline");
    assert!(
        matches!(error, ClientError::SessionRemoveOutcomeUnknown { .. }),
        "{error:?}"
    );
    drop(impatient);

    gate.release.notify_one();
    first
        .await
        .expect("first stop joins")
        .expect("first stop succeeds");
    wait_until("the daemon to finish the abandoned removal", || async {
        rig.registry
            .inspect(&created.id)
            .await
            .is_err()
            .then_some(())
    })
    .await;
    rig.finish().await;
}
