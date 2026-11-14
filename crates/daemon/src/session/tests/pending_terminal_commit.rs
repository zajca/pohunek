//! A stop or a removal that reaches the worker while the stopped runtime's
//! terminal journal commit is still pending: the worker refuses the stop as
//! retryable, and the daemon sends it again on the connection it still holds
//! instead of failing the caller with `worker_operation_failed`.
//!
//! The in-process worker holds its terminal commit
//! ([`crate::runtime::InProcessWorkerLauncher::hold_terminal_commit`]): the
//! first stop parks at the held commit until the scenario lets it be refused,
//! and the commit lands only once the next stop reaches the worker.

use pohunek_session_worker::TerminalCommitHold;

use super::*;
use crate::runtime::InProcessWorkerLauncher;

/// A registry over in-process workers and one running shell session whose
/// terminal commit is held.
struct HeldCommit {
    registry: SessionRegistry,
    created: SessionInfo,
    hold: TerminalCommitHold,
}

impl HeldCommit {
    async fn start(tag: &str) -> Self {
        let mut config = SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(temp_store_path(tag)),
            ..SessionRegistryConfig::default()
        };
        let (runtime_root, state_root) = test_worker_roots(&config);
        config.supervision = Some(crate::session::test_supervision(&runtime_root, &state_root));
        config.worker_runtime_root = Some(runtime_root.clone());
        config.worker_state_root = Some(state_root.clone());
        let launcher = Arc::new(InProcessWorkerLauncher::new(runtime_root, state_root));
        let registry = SessionRegistry::new_with_runtimes_and_launcher(
            config,
            crate::agent::host::fixture::official_host_over(
                &crate::agent::host::BuiltinSource::from_host_environment(),
            ),
            Arc::clone(&launcher) as Arc<dyn crate::runtime::WorkerLauncher>,
            Arc::new(ReadableHost::new()),
        );
        let created = registry.create(params()).await.expect("create session");
        let hold = launcher.hold_terminal_commit(&created.id.0).await;
        Self {
            registry,
            created,
            hold,
        }
    }

    /// Waits until a stop is parked at the held commit, then lets the worker
    /// refuse it.
    async fn refuse_the_first_stop(&self) {
        guard(
            "the stop to park at the held commit",
            self.hold.stop_parked(),
        )
        .await;
        self.hold.refuse();
    }

    /// The live worker client of the session.
    async fn worker(&self) -> crate::runtime::Worker {
        let sessions = self.registry.inner.sessions.lock().await;
        let RuntimeHandle::Worker(worker) = &sessions[&self.created.id].runtime else {
            panic!("live worker runtime");
        };
        worker.clone()
    }
}

#[tokio::test]
async fn a_removal_waits_for_the_terminal_commit_its_stop_found_pending() {
    let rig = HeldCommit::start("pending-commit-rm").await;
    let removal = tokio::spawn({
        let registry = rig.registry.clone();
        let id = rig.created.id.clone();
        async move { Box::pin(registry.remove(&id)).await }
    });
    rig.refuse_the_first_stop().await;

    let removed = guard("the removal", removal)
        .await
        .expect("removal task")
        .expect("the removal waits for the pending terminal commit");

    assert!(removed.removed, "{removed:?}");
    assert!(removed.stopped, "{removed:?}");
    assert!(
        rig.registry
            .list()
            .await
            .iter()
            .all(|session| session.id != rig.created.id),
        "the removed session is no longer listed"
    );
}

#[tokio::test]
async fn a_stop_waits_for_the_terminal_commit_it_found_pending() {
    let rig = HeldCommit::start("pending-commit-stop").await;
    let stop = tokio::spawn({
        let registry = rig.registry.clone();
        let id = rig.created.id.clone();
        async move { Box::pin(registry.stop(&id)).await }
    });
    rig.refuse_the_first_stop().await;

    let stopped = guard("the stop", stop)
        .await
        .expect("stop task")
        .expect("the stop waits for the pending terminal commit");

    assert!(stopped.stopped);
    let info = rig
        .registry
        .inspect(&rig.created.id)
        .await
        .expect("inspect the stopped session");
    assert_eq!(info.state, SessionState::Stopped, "{info:?}");
}

/// The retried stop goes out on the connection the refused stop still holds,
/// so a worker exchange queued behind the stop runs only after the retry: it
/// sees the runtime the retry let commit, never the runtime between the two
/// stops.
#[tokio::test]
async fn an_exchange_queued_behind_a_refused_stop_runs_after_its_retry() {
    let rig = HeldCommit::start("pending-commit-queued").await;
    let worker = rig.worker().await;
    let stop = tokio::spawn({
        let registry = rig.registry.clone();
        let id = rig.created.id.clone();
        async move { Box::pin(registry.stop(&id)).await }
    });
    guard(
        "the stop to park at the held commit",
        rig.hold.stop_parked(),
    )
    .await;
    // The parked stop holds the worker connection, so this inspection queues
    // behind it; on the test's current-thread runtime the yield runs it up to
    // that wait.
    let queued = tokio::spawn(async move { worker.inspect().await });
    tokio::task::yield_now().await;
    rig.hold.refuse();

    let stopped = guard("the stop", stop)
        .await
        .expect("stop task")
        .expect("the stop waits for the pending terminal commit");
    assert!(stopped.stopped);
    let snapshot = guard("the queued inspection", queued)
        .await
        .expect("inspection task")
        .expect("the queued inspection is answered");
    assert_eq!(
        snapshot.phase,
        pohunek_worker_protocol::RuntimePhase::Exited,
        "the queued inspection ran after the retried stop let the commit land"
    );
}
