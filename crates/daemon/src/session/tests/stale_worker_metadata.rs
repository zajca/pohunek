//! A prior worker generation cannot publish metadata into its replacement.

use super::*;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps both real worker generations and the delayed delivery ordered"
)]
async fn a_superseded_workers_snapshot_cannot_replace_the_current_generations_metadata() {
    let dir = temp_dir("stale-worker-metadata");
    let child = dir.join("child-id");
    fs::write(&child, "old-child").expect("select generation A's subagent");
    let script = dir.join("agent");
    let adapter = crate::integration::runnable_script("codex", "pohunek-agent-state.sh");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nchild=$(cat '{}')\n\
             printf '{{\"session_id\":\"parent\",\"hook_event_name\":\"SubagentStart\",\"agent_id\":\"%s\"}}' \"$child\" | sh '{}' subagent-start\n\
             while read -r line; do :; done\n",
            child.display(),
            adapter.display(),
        ),
    );
    let store_path = temp_store_path("stale-worker-metadata");
    let registry = SessionRegistry::new_with_runtimes(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(store_path.clone()),
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        crate::agent::host::fixture::pi_shaped_hooked_host_as(
            &script,
            crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
            "stale-metadata-fixture",
        ),
    );
    let created = registry
        .create(SessionNewParams {
            agent: "stale-metadata-fixture".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create generation A");
    let store = crate::store::Store::new(store_path);
    let durable = || {
        store
            .load_sessions()
            .expect("load durable sessions")
            .into_iter()
            .find(|record| record.session_id == created.id.0)
            .expect("the logical session's durable record")
    };
    let (worker_a, _) = live_worker_and_identity(&registry, &created.id).await;
    let snapshot_a = wait_until("worker A to journal old-child", || async {
        let snapshot = worker_a.inspect().await.expect("inspect worker A");
        (snapshot
            .subagents
            .iter()
            .any(|agent| agent.id == "old-child"))
        .then_some(snapshot)
    })
    .await;
    // Otherwise dead process claims could reject the mutant for an unrelated reason.
    assert!(snapshot_a.launch_identity.is_none());
    assert!(snapshot_a.active_identity.is_none());
    assert!(snapshot_a.worker_instance_id.is_some());

    registry.stop(&created.id).await.expect("stop generation A");
    fs::write(&child, "current-child").expect("select generation B's subagent");
    registry
        .resume(&created.id)
        .await
        .expect("resume as generation B");
    let (worker_b, _) = live_worker_and_identity(&registry, &created.id).await;
    let snapshot_b = wait_until("worker B to journal current-child", || async {
        let snapshot = worker_b.inspect().await.expect("inspect worker B");
        (snapshot
            .subagents
            .iter()
            .any(|agent| agent.id == "current-child"))
        .then_some(snapshot)
    })
    .await;
    assert_ne!(snapshot_a.worker_id, snapshot_b.worker_id);
    assert_ne!(snapshot_a.worker_instance_id, snapshot_b.worker_instance_id);
    let before = wait_until("generation B metadata in memory and store", || async {
        let info = registry.inspect(&created.id).await.expect("inspect B");
        let record = durable();
        let current = info.state == SessionState::Running
            && info
                .subagents
                .iter()
                .any(|agent| agent.id == "current-child")
            && record.info == info
            && record.desired_state == crate::store::DesiredState::Running
            && record.transaction.is_none()
            && record.runtime.state == RuntimeState::Live
            && record.runtime.worker_id.as_deref() == Some(snapshot_b.worker_id.as_str())
            && record.runtime.worker_instance_id.as_deref()
                == snapshot_b
                    .worker_instance_id
                    .as_ref()
                    .map(pohunek_worker_protocol::WorkerInstanceId::as_str);
        current.then_some((info, record))
    })
    .await;

    let outcome = registry
        .apply_worker_metadata_snapshot(&created.id, &snapshot_a)
        .await;
    let after_info = registry
        .inspect(&created.id)
        .await
        .expect("inspect after replay");
    let after_record = durable();
    // Clean up before checking the outcome, including when the planted guard fails.
    registry.stop(&created.id).await.expect("stop generation B");

    assert_eq!(outcome, WorkerMetadataApplyOutcome::Discarded);
    assert_eq!(
        after_info, before.0,
        "stale metadata must not change memory"
    );
    assert_eq!(
        after_record, before.1,
        "stale metadata must not change the store"
    );
}
