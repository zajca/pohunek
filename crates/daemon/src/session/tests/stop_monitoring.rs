//! Monitoring of a session across a stop: the detector, the process watch and
//! its exit watches keep running until the terminal commit, so a stop that
//! fails leaves the session monitored.

use std::time::Instant;

use protocol::{event, SessionId, SessionInfo, SessionState};
use tokio::task::JoinHandle;

use super::{
    claude_fact, codex_fact, descendant_claim_fixture, mock_procwatch_registry, next_runtime_event,
    settle_procwatch_scans, wait_for_first_procwatch_scan, wait_until, MockInspector,
    DESCENDANT_REPORTER_PID, DESCENDANT_WRAPPER_PID, PID_REUSE_AGENT_PID,
};
use crate::procwatch::StartIdentity;
use crate::session::{RuntimeHandle, SessionRegistry};
use crate::store::DesiredState;
use pohunek_test_support::wait::HANG_GUARD;

use std::sync::Arc;

/// Pid of the observed agent the scenarios start and end.
const AGENT_PID: u32 = PID_REUSE_AGENT_PID;

/// A stop parked after it marked the session stopping. The controller lease is
/// released first, so once the stop is let go the worker refuses it while the
/// PTY stays up.
struct FailingStop {
    registry: SessionRegistry,
    id: SessionId,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    task: JoinHandle<Result<protocol::SessionStopResult, protocol::ProtocolError>>,
}

impl FailingStop {
    async fn begin(registry: &SessionRegistry, id: &SessionId) -> Self {
        Self::begin_with(registry, id, |registry, id| {
            let registry = registry.clone();
            let id = id.clone();
            tokio::spawn(async move { registry.stop(&id).await })
        })
        .await
    }

    async fn begin_with(
        registry: &SessionRegistry,
        id: &SessionId,
        start: impl FnOnce(
            &SessionRegistry,
            &SessionId,
        )
            -> JoinHandle<Result<protocol::SessionStopResult, protocol::ProtocolError>>,
    ) -> Self {
        let worker = {
            let sessions = registry.inner.sessions.lock().await;
            let RuntimeHandle::Worker(worker) = &sessions[id].runtime else {
                panic!("live worker runtime");
            };
            worker.clone()
        };
        worker
            .release_controller()
            .await
            .expect("release the controller lease");
        let gate = registry.hold_next_stop_intent(id);
        let task = start(registry, id);
        gate.entered.notified().await;
        Self {
            registry: registry.clone(),
            id: id.clone(),
            entered: gate.entered,
            release: gate.release,
            task,
        }
    }

    /// Lets the parked stop go on and returns its error.
    async fn fail(self) -> protocol::ProtocolError {
        let _ = &self.entered;
        self.release.notify_one();
        let result = tokio::time::timeout(HANG_GUARD, self.task)
            .await
            .expect("the stop ends")
            .expect("the stop task joins");
        let error = result.expect_err("the worker refuses the stop");
        let sessions = self.registry.inner.sessions.lock().await;
        let entry = &sessions[&self.id];
        assert!(!entry.stopping, "a failed stop clears the stopping flag");
        assert_eq!(entry.info.state, SessionState::Running);
        error
    }
}

async fn tokens_live(registry: &SessionRegistry, id: &SessionId) -> (bool, bool) {
    let sessions = registry.inner.sessions.lock().await;
    let entry = &sessions[id];
    (
        !entry.detector_cancel.is_cancelled(),
        !entry.procwatch_cancel.is_cancelled(),
    )
}

async fn reporter_watched(registry: &SessionRegistry, id: &SessionId) -> bool {
    let sessions = registry.inner.sessions.lock().await;
    sessions[id]
        .bound_reporter
        .as_ref()
        .is_some_and(|bound| bound.watched)
}

async fn observed_pids(registry: &SessionRegistry, id: &SessionId) -> Vec<u32> {
    let sessions = registry.inner.sessions.lock().await;
    sessions[id]
        .observed_agents
        .iter()
        .map(|agent| agent.pid)
        .collect()
}

/// The next runtime transition other than a worker reconnect. A stop the worker
/// refuses breaks its control connection, so the exit watcher reconnects at a
/// moment the scenarios do not control.
async fn next_transition_after_reconnects(
    events: &mut tokio::sync::broadcast::Receiver<protocol::Event>,
) -> String {
    loop {
        let name = next_runtime_event(events).await;
        if name != event::SESSION_RUNTIME_RECONNECTED {
            return name;
        }
    }
}

/// A registry whose session observes one Codex agent below its root.
async fn observed_agent_fixture(tag: &str) -> (SessionRegistry, Arc<MockInspector>, SessionInfo) {
    let (registry, inspector, created) = mock_procwatch_registry(tag).await;
    wait_for_first_procwatch_scan(&inspector).await;
    inspector.set_descendants(created.pid, vec![codex_fact(AGENT_PID, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    assert_eq!(observed_pids(&registry, &created.id).await, [AGENT_PID]);
    (registry, inspector, created)
}

async fn wait_for_observed_pids(registry: &SessionRegistry, id: &SessionId, expected: &[u32]) {
    wait_until("the observed agents settled", || async {
        (observed_pids(registry, id).await == expected).then_some(())
    })
    .await;
}

/// A stop that succeeds ends the watchers at the terminal commit, not at the
/// stop request.
#[tokio::test]
async fn stop_cancels_watchers_only_at_terminal_commit() {
    let (registry, inspector, created) = mock_procwatch_registry("stop-cancels-at-commit").await;
    // A scan still running when the stop kills the root would find it gone and
    // end the process watch before the terminal commit. The descendant read
    // ends before the scan's last liveness read, so the scan gate is awaited
    // too.
    wait_for_first_procwatch_scan(&inspector).await;
    settle_procwatch_scans(&registry).await;
    let commit = registry.hold_next_exit_commit(&created.id);
    let stopping = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move { registry.stop(&id).await })
    };
    commit.entered.notified().await;
    assert_eq!(
        tokens_live(&registry, &created.id).await,
        (true, true),
        "the worker already stopped, the terminal state is not committed"
    );
    commit.release.notify_one();
    assert!(
        tokio::time::timeout(HANG_GUARD, stopping)
            .await
            .expect("the stop ends")
            .expect("the stop task joins")
            .expect("the stop succeeds")
            .stopped
    );
    assert_eq!(
        tokens_live(&registry, &created.id).await,
        (false, false),
        "the terminal commit cancels both watchers"
    );
}

/// A stop the worker refuses leaves the detector and the process watch
/// running, and the process watch scans again at once.
#[tokio::test]
async fn failed_stop_keeps_procwatch_and_detector() {
    let (registry, inspector, created) = observed_agent_fixture("failed-stop-monitoring").await;
    let mut events = registry.subscribe();
    let stop = FailingStop::begin(&registry, &created.id).await;
    let (started_during_stop, _, _) = inspector.descendant_scans();
    registry.inner.sessions.lock().await[&created.id]
        .procwatch_rescan
        .notify_one();
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        inspector.descendant_scans().0,
        started_during_stop,
        "a stopping session is not scanned"
    );
    assert_eq!(tokens_live(&registry, &created.id).await, (true, true));

    stop.fail().await;
    assert_eq!(tokens_live(&registry, &created.id).await, (true, true));
    let desired_state = registry.inner.sessions.lock().await[&created.id].desired_state;
    assert_eq!(
        desired_state,
        DesiredState::Stopped,
        "the durable stop intent stays pending"
    );
    wait_until("the failed stop woke the process watch", || async {
        (inspector.descendant_scans().0 > started_during_stop).then_some(())
    })
    .await;

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(AGENT_PID);
    wait_for_observed_pids(&registry, &created.id, &[]).await;
    assert_eq!(
        next_transition_after_reconnects(&mut events).await,
        event::SESSION_UPDATED,
        "the exit after the failed stop is notified"
    );
}

/// An observed agent that exits while the stop is in flight is applied by the
/// first scan after the stop failed.
#[tokio::test]
async fn observed_exit_during_failed_stop_is_applied_by_rescan() {
    let (registry, inspector, created) = observed_agent_fixture("observed-exit-failed-stop").await;
    let mut events = registry.subscribe();
    let stop = FailingStop::begin(&registry, &created.id).await;

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(AGENT_PID);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        observed_pids(&registry, &created.id).await,
        [AGENT_PID],
        "the exit callback ignores a stopping session"
    );

    stop.fail().await;
    wait_for_observed_pids(&registry, &created.id, &[]).await;
    assert_eq!(
        next_transition_after_reconnects(&mut events).await,
        event::SESSION_UPDATED
    );
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
}

/// The exit of the bound reporter during the stop leaves its claim to the
/// first scan after the stop failed.
#[tokio::test]
async fn bound_reporter_exit_during_failed_stop_ends_claim() {
    let (registry, inspector, created) =
        descendant_claim_fixture("bound-exit-failed-stop", false).await;
    assert!(super::reporter_is_bound(&registry, &created.id).await);
    assert!(reporter_watched(&registry, &created.id).await);
    let stop = FailingStop::begin(&registry, &created.id).await;

    // The exited reporter stays listed as a zombie would, so only its
    // liveness read ends the claim.
    inspector.set_not_running(
        super::descendant_reporter_fact(DESCENDANT_WRAPPER_PID, DESCENDANT_WRAPPER_PID).identity(),
    );
    inspector.fire_exit(DESCENDANT_REPORTER_PID);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        super::reporter_is_bound(&registry, &created.id).await,
        "the claim waits for the scan after the stop"
    );
    assert!(
        !reporter_watched(&registry, &created.id).await,
        "the ended watch hands the reporter's liveness to the scans"
    );

    stop.fail().await;
    wait_until("the scan ended the claim", || async {
        (!super::reporter_is_bound(&registry, &created.id).await).then_some(())
    })
    .await;
}

/// A successful stop clears an agent that exited during it exactly once.
#[tokio::test]
async fn observed_exit_during_successful_stop_emits_once() {
    let (registry, inspector, created) = observed_agent_fixture("observed-exit-stop").await;
    let mut events = registry.subscribe();
    let commit = registry.hold_next_exit_commit(&created.id);
    let stopping = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move { registry.stop(&id).await })
    };
    commit.entered.notified().await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(AGENT_PID);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    commit.release.notify_one();
    stopping
        .await
        .expect("the stop task joins")
        .expect("the stop succeeds");

    assert_eq!(
        next_runtime_event(&mut events).await,
        event::SESSION_STOPPED
    );
    super::assert_no_runtime_event(&mut events).await;
}

/// A pid reused after an agent exit during a failed stop is a new agent with
/// its own watch.
#[tokio::test]
async fn reused_pid_during_failed_stop_is_a_new_agent() {
    let (registry, inspector, created) = observed_agent_fixture("reused-pid-failed-stop").await;
    let stop = FailingStop::begin(&registry, &created.id).await;

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(AGENT_PID);
    let mut replacement = claude_fact(AGENT_PID, created.pid);
    replacement.start_identity = StartIdentity::new(u64::from(AGENT_PID) + 1);
    inspector.set_descendants(created.pid, vec![replacement]);

    stop.fail().await;
    wait_until("the scan observed the replacement agent", || async {
        let sessions = registry.inner.sessions.lock().await;
        sessions[&created.id]
            .observed_agents
            .iter()
            .any(|agent| agent.pid == AGENT_PID && agent.start_identity == u64::from(AGENT_PID) + 1)
            .then_some(())
    })
    .await;
    assert_eq!(
        observed_pids(&registry, &created.id).await,
        [AGENT_PID],
        "only the replacement is observed"
    );
    assert_eq!(inspector.exit_watch_count(AGENT_PID), 2);
}

/// A removal whose stop step fails errors and leaves the session monitored,
/// and its stop intent stays `Removed`.
#[tokio::test]
async fn failed_remove_stop_keeps_monitoring() {
    let (registry, inspector, created) = observed_agent_fixture("failed-remove-monitoring").await;
    let stop = FailingStop::begin_with(&registry, &created.id, |registry, id| {
        let registry = registry.clone();
        let id = id.clone();
        tokio::spawn(async move {
            registry
                .remove(&id)
                .await
                .map(|removed| protocol::SessionStopResult {
                    stopped: removed.stopped,
                })
        })
    })
    .await;

    stop.fail().await;
    assert_eq!(tokens_live(&registry, &created.id).await, (true, true));
    let desired_state = registry.inner.sessions.lock().await[&created.id].desired_state;
    assert_eq!(desired_state, DesiredState::Removed);
    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(AGENT_PID);
    wait_for_observed_pids(&registry, &created.id, &[]).await;
}
