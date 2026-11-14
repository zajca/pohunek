//! A stop or a removal that reaches the worker while the stopped runtime's
//! terminal journal commit is still pending: the worker refuses the stop as
//! retryable, and the daemon waits for the commit instead of failing the
//! caller with `worker_operation_failed`.
//!
//! The in-process worker holds its terminal commit
//! ([`crate::runtime::InProcessWorkerLauncher::hold_terminal_commit`]), so its
//! first stop is always refused with the pending commit and the commit lands
//! only after that refusal.

use super::*;
use crate::runtime::InProcessWorkerLauncher;

/// A registry over in-process workers whose launcher stays reachable, and one
/// running shell session whose terminal commit is held.
struct HeldCommit {
    registry: SessionRegistry,
    created: SessionInfo,
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
        launcher.hold_terminal_commit(&created.id.0).await;
        Self { registry, created }
    }
}

#[tokio::test]
async fn a_removal_waits_for_the_terminal_commit_its_stop_found_pending() {
    let rig = HeldCommit::start("pending-commit-rm").await;

    let removed = Box::pin(rig.registry.remove(&rig.created.id))
        .await
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

    let stopped = Box::pin(rig.registry.stop(&rig.created.id))
        .await
        .expect("the stop waits for the pending terminal commit");

    assert!(stopped.stopped);
    let info = rig
        .registry
        .inspect(&rig.created.id)
        .await
        .expect("inspect the stopped session");
    assert_eq!(info.state, SessionState::Stopped, "{info:?}");
}
