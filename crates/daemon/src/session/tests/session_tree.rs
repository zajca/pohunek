//! Retention sweep selector: `spawned_by` and `session_ids` narrow a sweep
//! to a subset, with every selection rule unchanged outside it.

use super::{
    age_lost, age_terminal, apply_sweep_policy, exited_session, external_session_fixture,
    init_git_repo, params, past_ttl, retention_registry, retention_registry_with_unusable_log_dir,
    session_ids, uuid_like, HANG_GUARD,
};
use crate::procwatch::ProcessInspector;
use protocol::{
    SessionId, SessionRemoveTreeParams, SessionRetentionParams, SessionTreeKeepReason,
    SessionTreeOutcome, SpawnedBy, MAX_RETENTION_SELECTOR_IDS, MAX_SESSION_REMOVE_TREE_SESSIONS,
};

/// Create a finished child of `parent`, optionally on its own worktree.
async fn exited_child(
    registry: &crate::session::SessionRegistry,
    parent: &SessionId,
    repo: Option<&std::path::PathBuf>,
) -> protocol::SessionInfo {
    let create = match repo {
        Some(repo) => protocol::SessionNewParams {
            cwd: None,
            repo: Some(repo.clone()),
            branch: Some(format!("feat/{}", uuid_like())),
            ..params()
        },
        None => params(),
    };
    let outcome = registry
        .create_outcome_spawned_by(
            create,
            Some(SpawnedBy {
                session_id: parent.clone(),
                daemon_id: "d-test".to_owned(),
            }),
        )
        .await
        .expect("create child session");
    let id = outcome.session.id.clone();
    registry
        .wait_for_exit(&id, HANG_GUARD)
        .await
        .expect("child session exits");
    registry.inspect(&id).await.expect("child listed")
}

/// Backdate a finished session so the terminal TTL selects it.
async fn age_finished(registry: &crate::session::SessionRegistry, id: &SessionId) {
    age_terminal(registry, id, past_ttl()).await;
}

#[tokio::test]
async fn sweep_with_spawned_by_removes_only_the_parent_direct_children() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-children");
    let repo = init_git_repo("sweep-selector-children");
    apply_sweep_policy(&registry, &data_dir).await;

    let parent = registry
        .create(params())
        .await
        .expect("create parent session");
    let dirty = exited_child(&registry, &parent.id, Some(&repo)).await;
    let dirty_worktree = dirty.worktree_path.clone().expect("child worktree");
    std::fs::write(
        dirty_worktree.join("README.md"),
        "work the agent did not commit\n",
    )
    .expect("dirty the worktree");
    let clean = exited_child(&registry, &parent.id, Some(&repo)).await;
    let live = registry
        .create_outcome_spawned_by(
            params(),
            Some(SpawnedBy {
                session_id: parent.id.clone(),
                daemon_id: "d-test".to_owned(),
            }),
        )
        .await
        .expect("create live child")
        .session;
    let other = exited_session(&registry, None).await;
    age_finished(&registry, &dirty.id).await;
    age_finished(&registry, &clean.id).await;
    age_finished(&registry, &other.id).await;

    let result = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: Some(parent.id.clone()),
            session_ids: None,
        })
        .await
        .expect("selective sweep");

    assert_eq!(result.examined, 3, "{result:?}");
    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(result.held, 1, "{result:?}");
    let held = result
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == dirty.id)
        .expect("dirty child reported");
    assert_eq!(
        held.hold,
        Some(protocol::SessionRetentionHold::WorktreeUncommitted),
        "{result:?}"
    );
    assert!(dirty_worktree.is_dir(), "the dirty worktree stays on disk");
    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![
        parent.id.0.clone(),
        dirty.id.0.clone(),
        live.id.0.clone(),
        other.id.0.clone(),
    ];
    expected.sort();
    assert_eq!(remaining, expected, "only the clean child is gone");
}

#[tokio::test]
async fn sweep_with_session_ids_removes_only_the_named_sessions() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-ids");
    apply_sweep_policy(&registry, &data_dir).await;

    let parent = registry
        .create(params())
        .await
        .expect("create parent session");
    let first = exited_session(&registry, None).await;
    let second = exited_session(&registry, None).await;
    let untouched = exited_session(&registry, None).await;
    age_finished(&registry, &first.id).await;
    age_finished(&registry, &second.id).await;
    age_finished(&registry, &untouched.id).await;
    age_lost(&registry, &parent.id, past_ttl()).await;

    let result = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: None,
            session_ids: Some(vec![first.id.clone(), second.id.clone()]),
        })
        .await
        .expect("selective sweep");

    assert_eq!(result.examined, 2, "{result:?}");
    assert_eq!(result.removed, 2, "{result:?}");
    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![parent.id.0.clone(), untouched.id.0.clone()];
    expected.sort();
    assert_eq!(remaining, expected);
}

#[tokio::test]
async fn sweep_with_spawned_by_reaches_orphans_of_a_gone_parent() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-orphans");
    apply_sweep_policy(&registry, &data_dir).await;

    let gone = SessionId("s-gone".to_owned());
    let first = registry
        .create_outcome_spawned_by(
            params(),
            Some(SpawnedBy {
                session_id: gone.clone(),
                daemon_id: "d-test".to_owned(),
            }),
        )
        .await
        .expect("create first orphan")
        .session;
    registry
        .wait_for_exit(&first.id, HANG_GUARD)
        .await
        .expect("first orphan exits");
    let second = registry
        .create_outcome_spawned_by(
            params(),
            Some(SpawnedBy {
                session_id: gone.clone(),
                daemon_id: "d-test".to_owned(),
            }),
        )
        .await
        .expect("create second orphan")
        .session;
    registry
        .wait_for_exit(&second.id, HANG_GUARD)
        .await
        .expect("second orphan exits");
    let other = exited_session(&registry, None).await;
    age_finished(&registry, &first.id).await;
    age_finished(&registry, &second.id).await;
    age_finished(&registry, &other.id).await;

    let result = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: Some(gone),
            session_ids: None,
        })
        .await
        .expect("orphan sweep");

    assert_eq!(result.examined, 2, "{result:?}");
    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(
        session_ids(&registry).await,
        vec![other.id.0.clone()],
        "the non-orphan stays"
    );
}

#[tokio::test]
async fn sweep_rejects_an_ambiguous_or_oversized_selector() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-invalid");
    apply_sweep_policy(&registry, &data_dir).await;

    let parent = exited_session(&registry, None).await;
    age_finished(&registry, &parent.id).await;
    let before = session_ids(&registry).await;

    let both = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: Some(parent.id.clone()),
            session_ids: Some(vec![parent.id.clone()]),
        })
        .await;
    assert!(
        matches!(&both, Err(error) if error.code == "bad_request"),
        "both selectors: {both:?}"
    );

    let empty = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: None,
            session_ids: Some(Vec::new()),
        })
        .await;
    assert!(
        matches!(&empty, Err(error) if error.code == "bad_request"),
        "empty session_ids: {empty:?}"
    );

    let oversized: Vec<SessionId> = (0..=MAX_RETENTION_SELECTOR_IDS)
        .map(|n| SessionId(format!("s-{n:04}")))
        .collect();
    let too_many = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: None,
            spawned_by: None,
            session_ids: Some(oversized),
        })
        .await;
    assert!(
        matches!(&too_many, Err(error) if error.code == "bad_request"),
        "oversized session_ids: {too_many:?}"
    );

    assert_eq!(
        session_ids(&registry).await,
        before,
        "a refused selector removes nothing"
    );
}

#[tokio::test]
async fn sweep_with_selector_and_limit_removes_at_most_the_limit() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-limit");
    apply_sweep_policy(&registry, &data_dir).await;

    let parent = registry
        .create(params())
        .await
        .expect("create parent session");
    let mut children = Vec::new();
    for _ in 0..2 {
        let child = exited_child(&registry, &parent.id, None).await;
        age_finished(&registry, &child.id).await;
        children.push(child.id);
    }

    let result = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: false,
            limit: Some(1),
            spawned_by: Some(parent.id.clone()),
            session_ids: None,
        })
        .await
        .expect("limited selective sweep");

    assert_eq!(result.examined, 2, "{result:?}");
    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(session_ids(&registry).await.len(), 2);
}

#[tokio::test]
async fn sweep_with_selector_and_dry_run_removes_nothing() {
    let (registry, data_dir, _worktree_root) = retention_registry("sweep-selector-dry");
    apply_sweep_policy(&registry, &data_dir).await;

    let parent = registry
        .create(params())
        .await
        .expect("create parent session");
    let child = exited_child(&registry, &parent.id, None).await;
    age_finished(&registry, &child.id).await;
    let before = session_ids(&registry).await;

    let result = registry
        .sweep_retention(&SessionRetentionParams {
            dry_run: true,
            limit: None,
            spawned_by: Some(parent.id.clone()),
            session_ids: None,
        })
        .await
        .expect("dry-run selective sweep");

    assert_eq!(result.examined, 1, "{result:?}");
    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(session_ids(&registry).await, before);
}

/// Builds M -> W1, W2 and W1 -> S1, every session finished without a
/// worktree, and returns their ids oldest-first.
async fn finished_tree(
    registry: &crate::session::SessionRegistry,
) -> (SessionId, SessionId, SessionId, SessionId) {
    let root = registry
        .create(params())
        .await
        .expect("create root session");
    let root = registry.inspect(&root.id).await.expect("root listed");
    let first = exited_child(registry, &root.id, None).await;
    let second = exited_child(registry, &root.id, None).await;
    let leaf = exited_child(registry, &first.id, None).await;
    (root.id, first.id, second.id, leaf.id)
}

/// A corrupted two-record cycle (a links to b, b links to a) written behind
/// the write guard, the way the stale-commit tests corrupt entries: no
/// public write can create it. The cascade and the list answer must still
/// terminate, reporting each node once.
async fn corrupt_spawned_by_cycle(
    registry: &crate::session::SessionRegistry,
    first: &SessionId,
    second: &SessionId,
) {
    let mut sessions = registry.inner.sessions.lock().await;
    let first_entry = sessions.get_mut(first).expect("first entry");
    first_entry.info.spawned_by = Some(SpawnedBy {
        session_id: second.clone(),
        daemon_id: "d-test".to_owned(),
    });
    let second_entry = sessions.get_mut(second).expect("second entry");
    second_entry.info.spawned_by = Some(SpawnedBy {
        session_id: first.clone(),
        daemon_id: "d-test".to_owned(),
    });
}

fn tree_params(root: &SessionId, accept_unconfirmed_cleanup: bool) -> SessionRemoveTreeParams {
    SessionRemoveTreeParams {
        session_id: root.clone(),
        accept_unconfirmed_cleanup,
    }
}

/// A session that stays live for the cascade: the registry's shell runs
/// `sleep`, so the session is stoppable but never terminal while the
/// test runs.
fn live_registry() -> crate::session::SessionRegistry {
    crate::session::SessionRegistry::new(crate::session::SessionRegistryConfig {
        shell_command: crate::session::ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        ..crate::session::SessionRegistryConfig::default()
    })
}

async fn live_session(
    registry: &crate::session::SessionRegistry,
    parent: Option<&SessionId>,
) -> protocol::SessionInfo {
    let outcome = match parent {
        Some(parent) => registry
            .create_outcome_spawned_by(
                params(),
                Some(SpawnedBy {
                    session_id: parent.clone(),
                    daemon_id: "d-test".to_owned(),
                }),
            )
            .await
            .expect("create live session"),
        None => registry
            .create_outcome(params())
            .await
            .expect("create live session"),
    };
    let info = registry
        .inspect(&outcome.session.id)
        .await
        .expect("live session listed");
    assert!(
        info.can_stop(),
        "the sleep session is live before the cascade"
    );
    info
}

#[tokio::test]
async fn remove_tree_removes_a_finished_tree_deepest_first() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-order");
    let (root, first, second, leaf) = finished_tree(&registry).await;

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root, false))
        .await
        .expect("finished tree removes");

    assert_eq!(result.removed, 4, "{result:?}");
    assert_eq!(result.kept, 0, "{result:?}");
    assert_eq!(result.refused, 0, "{result:?}");
    let order: Vec<&str> = result
        .sessions
        .iter()
        .map(|node| node.session_id.0.as_str())
        .collect();
    assert_eq!(
        order,
        vec![
            leaf.0.as_str(),
            first.0.as_str(),
            second.0.as_str(),
            root.0.as_str(),
        ],
        "deepest first, siblings by id, root last: {result:?}"
    );
    for node in &result.sessions {
        assert_eq!(node.outcome, SessionTreeOutcome::Removed, "{result:?}");
        assert!(node.keep_reason.is_none(), "{result:?}");
        assert!(node.error.is_none(), "{result:?}");
        assert!(
            node.removal.as_ref().is_some_and(|removal| removal.removed),
            "{result:?}"
        );
    }
    assert_eq!(
        result.sessions.last().expect("root last").depth,
        0,
        "{result:?}"
    );
    assert_eq!(
        result
            .sessions
            .iter()
            .find(|node| node.session_id == leaf)
            .expect("leaf reported")
            .depth,
        2,
        "{result:?}"
    );
    assert!(
        session_ids(&registry).await.is_empty(),
        "the registry is empty of the tree"
    );
}

#[tokio::test]
async fn remove_tree_keeps_a_dirty_child_and_its_ancestors() {
    let (registry, data_dir, _worktree_root) = retention_registry("remove-tree-dirty");
    let repo = init_git_repo("remove-tree-dirty");
    apply_sweep_policy(&registry, &data_dir).await;
    let (root, first, second, leaf) = finished_tree(&registry).await;
    let dirty = exited_child(&registry, &first, Some(&repo)).await;
    let dirty_worktree = dirty.worktree_path.clone().expect("child worktree");
    std::fs::write(
        dirty_worktree.join("README.md"),
        "work the agent did not commit\n",
    )
    .expect("dirty the worktree");

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root, false))
        .await
        .expect("cascade with a dirty child");

    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(result.kept, 3, "{result:?}");
    assert_eq!(result.refused, 0, "{result:?}");
    let mut removed: Vec<&str> = result
        .sessions
        .iter()
        .filter(|node| node.outcome == SessionTreeOutcome::Removed)
        .map(|node| node.session_id.0.as_str())
        .collect();
    removed.sort_unstable();
    assert_eq!(
        removed,
        vec![second.0.as_str(), leaf.0.as_str()],
        "both clean sessions are removed: {result:?}"
    );
    let dirty_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == dirty.id)
        .expect("dirty child reported");
    assert_eq!(dirty_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        dirty_node.keep_reason,
        Some(SessionTreeKeepReason::WorktreeHeld),
        "{result:?}"
    );
    assert!(
        dirty_node.hold.is_some(),
        "the worktree hold names the reason: {result:?}"
    );
    for kept in [&first, &root] {
        let node = result
            .sessions
            .iter()
            .find(|node| node.session_id == *kept)
            .expect("ancestor reported");
        assert_eq!(node.outcome, SessionTreeOutcome::Kept, "{result:?}");
        assert_eq!(
            node.keep_reason,
            Some(SessionTreeKeepReason::DescendantKept),
            "{result:?}"
        );
    }
    assert!(dirty_worktree.is_dir(), "the dirty worktree stays on disk");
    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![root.0.clone(), first.0.clone(), dirty.id.0.clone()];
    expected.sort();
    assert_eq!(remaining, expected, "only the clean sessions are gone");
}

#[tokio::test]
async fn remove_tree_keeps_a_live_child_running_and_its_ancestors() {
    let registry = live_registry();
    let root = live_session(&registry, None).await;
    let live = live_session(&registry, Some(&root.id)).await;
    registry.stop(&root.id).await.expect("stop the root first");

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("cascade with a live child");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 2, "{result:?}");
    let live_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == live.id)
        .expect("live child reported");
    assert_eq!(live_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        live_node.keep_reason,
        Some(SessionTreeKeepReason::Live),
        "{result:?}"
    );
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantKept),
        "{result:?}"
    );
    // The cascade never stops a descendant: a living child is inspectable
    // as running, still stoppable, and never evicted.
    let inspected = registry
        .inspect(&live.id)
        .await
        .expect("the live child still runs");
    assert!(!inspected.state.is_terminal(), "{inspected:?}");
    assert!(
        inspected.can_stop(),
        "the live child was never stopped: {inspected:?}"
    );
    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![root.id.0.clone(), live.id.0.clone()];
    expected.sort();
    assert_eq!(remaining, expected, "nothing was stopped or removed");
}

#[tokio::test]
async fn remove_tree_terminates_on_a_spawned_by_cycle() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-cycle");
    let root = exited_session(&registry, None).await;
    let child = exited_child(&registry, &root.id, None).await;
    corrupt_spawned_by_cycle(&registry, &root.id, &child.id).await;

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("a corrupted cycle still terminates");

    let mut walked: Vec<&str> = result
        .sessions
        .iter()
        .map(|node| node.session_id.0.as_str())
        .collect();
    walked.sort_unstable();
    walked.dedup();
    assert_eq!(
        walked.len(),
        result.sessions.len(),
        "each node is reported once: {result:?}"
    );

    let listed = registry.list().await;
    let tree = protocol::SessionTree::new(&listed);
    let mut shown: Vec<&str> = tree
        .display_order()
        .iter()
        .map(|(session, _)| session.id.0.as_str())
        .collect();
    shown.sort_unstable();
    shown.dedup();
    assert_eq!(
        shown.len(),
        listed.len(),
        "the list answer shows each session once: {listed:?}"
    );
}

#[tokio::test]
async fn remove_tree_stops_a_live_root_after_its_children_are_gone() {
    let registry = live_registry();
    let root = live_session(&registry, None).await;
    let child = live_session(&registry, Some(&root.id)).await;
    registry
        .stop(&child.id)
        .await
        .expect("stop the child first");

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("cascade stops a live root");

    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(result.kept, 0, "{result:?}");
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert!(
        root_node
            .removal
            .as_ref()
            .is_some_and(|removal| removal.stopped),
        "the live root is stopped as session.remove stops it: {result:?}"
    );
    assert_eq!(child.id.0, result.sessions[0].session_id.0, "{result:?}");
    assert!(
        session_ids(&registry).await.is_empty(),
        "both sessions are gone"
    );
}

#[tokio::test]
async fn remove_leaves_descendants_listed() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-without-tree");
    let (root, first, second, leaf) = finished_tree(&registry).await;

    let removed = registry.remove(&root).await.expect("plain remove");
    assert!(removed.removed);

    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![first.0.clone(), second.0.clone(), leaf.0.clone()];
    expected.sort();
    assert_eq!(remaining, expected, "children survive a plain remove");
}

#[tokio::test]
async fn remove_tree_refuses_an_oversized_tree_before_removing_anything() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-cap");
    let mut parent = registry
        .create(params())
        .await
        .expect("create root session")
        .id;
    let root = parent.clone();
    for _ in 1..=MAX_SESSION_REMOVE_TREE_SESSIONS {
        let child = registry
            .create_outcome_spawned_by(
                params(),
                Some(SpawnedBy {
                    session_id: parent.clone(),
                    daemon_id: "d-test".to_owned(),
                }),
            )
            .await
            .expect("create chain child")
            .session;
        parent = child.id;
    }
    let before = session_ids(&registry).await;
    assert_eq!(before.len(), MAX_SESSION_REMOVE_TREE_SESSIONS + 1);

    let error = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root, false))
        .await
        .expect_err("an oversized tree is refused");
    assert_eq!(error.code, "bad_request", "{error:?}");
    assert!(
        error
            .msg
            .contains(&format!("{}", MAX_SESSION_REMOVE_TREE_SESSIONS + 1)),
        "the error names the count: {error:?}"
    );
    assert!(
        error
            .msg
            .contains(&format!("{MAX_SESSION_REMOVE_TREE_SESSIONS}")),
        "the error names the cap: {error:?}"
    );
    assert_eq!(
        session_ids(&registry).await.len(),
        before.len(),
        "nothing removed"
    );
}

#[tokio::test]
async fn remove_tree_keeps_a_root_with_a_child_added_during_the_cascade() {
    let (registry, _data_dir, _worktree_root) =
        retention_registry("remove-tree-added-during-cascade");
    let root = registry
        .create(params())
        .await
        .expect("create root session")
        .id;
    let child = exited_child(&registry, &root, None).await;

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    crate::session::remove_tree::arm_pre_root_gate_for_test(&root, entered_tx, resume_rx);
    let cascade = tokio::spawn({
        let registry = registry.clone();
        let root = root.clone();
        async move {
            crate::session::remove_tree::remove_tree(&registry, &tree_params(&root, false)).await
        }
    });
    entered_rx.await.expect("the cascade reaches the root");
    let added = exited_child(&registry, &root, None).await;
    resume_tx.send(()).expect("release the cascade");
    let result = cascade
        .await
        .expect("the cascade joins")
        .expect("a root kept for an added child");

    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(result.kept, 1, "{result:?}");
    assert_eq!(result.refused, 0, "{result:?}");
    assert_eq!(
        result.sessions.len(),
        2,
        "only the snapshot nodes are reported: {result:?}"
    );
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root)
        .expect("root reported");
    assert_eq!(root_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantAdded),
        "{result:?}"
    );
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == child.id)
        .expect("first child reported");
    assert_eq!(
        child_node.outcome,
        SessionTreeOutcome::Removed,
        "{result:?}"
    );
    let mut remaining = session_ids(&registry).await;
    remaining.sort();
    let mut expected = vec![root.0.clone(), added.id.0.clone()];
    expected.sort();
    assert_eq!(
        remaining, expected,
        "the root and the added child stay listed: {remaining:?}"
    );
}

#[tokio::test]
async fn remove_tree_reports_a_refused_descendant_and_keeps_its_ancestors() {
    use std::sync::Arc;

    use crate::procwatch::readable_host::ReadableHost;
    let inspector: Arc<dyn ProcessInspector> = Arc::new(ReadableHost::new());
    let (registry, _data_dir, _worktree_root) =
        retention_registry_with_unusable_log_dir("remove-tree-refused", inspector);
    let root = exited_session(&registry, None).await;
    let child = exited_child(&registry, &root.id, None).await;

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, true))
        .await
        .expect("a refused descendant is reported, not raised");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 1, "{result:?}");
    assert_eq!(result.refused, 1, "{result:?}");
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == child.id)
        .expect("child reported");
    assert_eq!(
        child_node.outcome,
        SessionTreeOutcome::Refused,
        "{result:?}"
    );
    let error = child_node
        .error
        .as_ref()
        .expect("refusal carries the error");
    assert_eq!(error.code, "session_log_cleanup_failed", "{error:?}");
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantKept),
        "{result:?}"
    );
    assert!(session_ids(&registry).await.contains(&root.id.0));
}

#[tokio::test]
async fn remove_tree_refuses_an_external_root_before_touching_anything() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-external");
    let (root, first, second, leaf) = finished_tree(&registry).await;
    let external = external_session_fixture(&registry, 91_341).await;

    // The cascade roots at the external session; the finished tree stays in
    // the registry only to prove the refusal happens before anything runs.
    let _ = (root, first, second, leaf);
    let error =
        crate::session::remove_tree::remove_tree(&registry, &tree_params(&external.id, false))
            .await
            .expect_err("an external root is refused");
    assert_eq!(error.code, "session_external_read_only", "{error:?}");
    assert_eq!(
        session_ids(&registry).await.len(),
        6,
        "the tree, the fixture shell, and the external root stay"
    );
}

#[tokio::test]
async fn remove_tree_keeps_an_external_child_and_its_ancestors() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-external-child");
    let root = exited_session(&registry, None).await;
    let external = external_session_fixture(&registry, 91_342).await;
    // Adopt the external session as the root's child through its record.
    // `spawned_by` shapes only the tree, never removal safety.
    let mut record = registry
        .inspect(&external.id)
        .await
        .expect("external listed");
    record.spawned_by = Some(SpawnedBy {
        session_id: root.id.clone(),
        daemon_id: "d-test".to_owned(),
    });
    registry
        .inner
        .external
        .upsert_if_current(
            crate::procwatch::ProcessIdentity {
                pid: external.pid,
                start_identity: crate::procwatch::StartIdentity::new(1),
            },
            record,
            || Ok::<_, std::convert::Infallible>(true),
            |_| {},
        )
        .await
        .expect("infallible external adoption")
        .expect("current external session");

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("an external child is kept, not raised");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 2, "{result:?}");
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == external.id)
        .expect("external child reported");
    assert_eq!(child_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        child_node.keep_reason,
        Some(SessionTreeKeepReason::External),
        "{result:?}"
    );
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantKept),
        "{result:?}"
    );
}

/// Turns a finished child into a conflict that names no worker generation:
/// the record keeps no service, generation, executable, worker or instance,
/// and the classification marks the runtime conflicted. Mirrors the
/// no-generation fixture of `conflict_cleanup` without duplicating it.
/// `repo` gives the child its own checkout, so the hold check has a tree
/// to inspect.
async fn conflict_child_without_generation_on_repo(
    registry: &crate::session::SessionRegistry,
    data_dir: &std::path::Path,
    parent: &SessionId,
    repo: Option<&std::path::PathBuf>,
) -> SessionId {
    let child = exited_child(registry, parent, repo).await;
    let store = crate::store::Store::new(data_dir.join("metadata.jsonl"));
    let mut record = store
        .load_sessions()
        .expect("load sessions")
        .into_iter()
        .find(|record| record.session_id == child.id.0)
        .expect("child record");
    record.runtime.worker_id = None;
    record.runtime.worker_instance_id = None;
    record.runtime.service_id = None;
    record.runtime.generation = None;
    record.runtime.executable = None;
    record.info.runtime = None;
    store.record_session(&record).expect("record the edit");
    assert!(
        registry
            .insert_unavailable_record(
                record,
                protocol::RuntimeState::Conflict,
                "launch_identity_reference_kind_mismatch",
            )
            .await
    );
    let listed = registry
        .inspect(&child.id)
        .await
        .expect("conflict child listed");
    assert_eq!(
        listed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Conflict),
        "{listed:?}"
    );
    assert!(
        listed
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.worker_id.is_none()),
        "no generation is recorded: {listed:?}"
    );
    child.id
}

#[tokio::test]
async fn remove_tree_keeps_a_generationless_conflict_without_consent() {
    let (registry, data_dir, _worktree_root) = retention_registry("remove-tree-conflict");
    let root = exited_session(&registry, None).await;
    let conflict =
        conflict_child_without_generation_on_repo(&registry, &data_dir, &root.id, None).await;

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("a generationless conflict is kept, not refused");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 2, "{result:?}");
    assert_eq!(result.refused, 0, "{result:?}");
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == conflict)
        .expect("conflict child reported");
    assert_eq!(child_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        child_node.keep_reason,
        Some(SessionTreeKeepReason::NotRemovable),
        "{result:?}"
    );
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantKept),
        "{result:?}"
    );
    registry
        .inspect(&conflict)
        .await
        .expect("the conflict survives without consent");
}

#[tokio::test]
async fn remove_tree_refuses_a_gone_root_before_touching_its_orphans() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-gone-root");
    let (root, first, second, leaf) = finished_tree(&registry).await;
    let gone_root = root.clone();
    registry
        .remove(&gone_root)
        .await
        .expect("plain remove leaves orphans");
    let before = session_ids(&registry).await;
    assert_eq!(before.len(), 3, "children stay listed: {before:?}");

    let error =
        crate::session::remove_tree::remove_tree(&registry, &tree_params(&gone_root, false))
            .await
            .expect_err("a gone root is refused");
    assert_eq!(error.code, "session_not_found", "{error:?}");
    let mut after = session_ids(&registry).await;
    after.sort();
    let mut expected = vec![first.0.clone(), second.0.clone(), leaf.0.clone()];
    expected.sort();
    assert_eq!(after, expected, "orphans survive the refused root");
}

#[tokio::test]
async fn remove_tree_refuses_an_external_root_with_children_before_touching_anything() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-external-root");
    let external = external_session_fixture(&registry, 91_343).await;
    // Create the finished child with the external session as its origin:
    // the cascade roots at the external session, so the pre-check must
    // refuse before the child is walked.
    let child = registry
        .create_outcome_spawned_by(
            params(),
            Some(SpawnedBy {
                session_id: external.id.clone(),
                daemon_id: "d-test".to_owned(),
            }),
        )
        .await
        .expect("create child session")
        .session;
    registry
        .wait_for_exit(&child.id, HANG_GUARD)
        .await
        .expect("child session exits");

    let error =
        crate::session::remove_tree::remove_tree(&registry, &tree_params(&external.id, false))
            .await
            .expect_err("an external root with children is refused");
    assert_eq!(error.code, "session_external_read_only", "{error:?}");
    registry
        .inspect(&child.id)
        .await
        .expect("the child survives the refused root");
}

#[tokio::test]
async fn remove_tree_keeps_a_dirty_worktree_under_a_consented_conflict_child() {
    let (registry, data_dir, _worktree_root) = retention_registry("remove-tree-conflict-dirty");
    let repo = init_git_repo("remove-tree-conflict-dirty");
    apply_sweep_policy(&registry, &data_dir).await;
    let root = exited_session(&registry, None).await;
    // The conflict child owns the dirty checkout: consent covers the
    // unproven cleanup only, never the data hold.
    let conflict =
        conflict_child_without_generation_on_repo(&registry, &data_dir, &root.id, Some(&repo))
            .await;
    let dirty_worktree = registry
        .inspect(&conflict)
        .await
        .expect("conflict listed")
        .worktree_path
        .expect("conflict worktree");
    std::fs::write(
        dirty_worktree.join("README.md"),
        "work the agent did not commit\n",
    )
    .expect("dirty the worktree");

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, true))
        .await
        .expect("a dirty worktree survives consent");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 2, "{result:?}");
    let conflict_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == conflict)
        .expect("conflict child reported");
    assert_eq!(
        conflict_node.outcome,
        SessionTreeOutcome::Kept,
        "{result:?}"
    );
    assert_eq!(
        conflict_node.keep_reason,
        Some(SessionTreeKeepReason::WorktreeHeld),
        "{result:?}"
    );
    assert!(
        conflict_node.hold.is_some(),
        "the hold names the reason: {result:?}"
    );
    assert!(dirty_worktree.is_dir(), "the dirty worktree stays on disk");
    registry
        .inspect(&conflict)
        .await
        .expect("the conflict survives consented cleanup");
}

#[tokio::test]
async fn remove_tree_reports_a_refused_root_after_removing_its_children() {
    let (registry, _data_dir, _worktree_root) = retention_registry("remove-tree-refused-root");
    let root = exited_session(&registry, None).await;
    let child = exited_child(&registry, &root.id, None).await;
    // Corrupt the root entry behind the write guard, the way the stale-commit
    // tests do: the root removal fails after its finished child is already
    // gone. The failure mode does not matter, only that the failed root is
    // a reported row instead of a raised error.
    {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&root.id).expect("root entry");
        entry.info.runtime = Some(protocol::SessionRuntime {
            state: protocol::RuntimeState::Incompatible,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: Some("worker-root".to_owned()),
            worker_instance_id: Some("runtime-root".to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
            exit: None,
            loss_cause: None,
            generation_recorded: None,
        });
    };

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, false))
        .await
        .expect("a refused root is reported, not raised");

    assert_eq!(result.removed, 1, "{result:?}");
    assert_eq!(result.kept, 0, "{result:?}");
    assert_eq!(result.refused, 1, "{result:?}");
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(root_node.outcome, SessionTreeOutcome::Refused, "{result:?}");
    assert!(
        root_node.error.is_some(),
        "the root carries its error: {result:?}"
    );
    registry
        .inspect(&root.id)
        .await
        .expect("the refused root stays listed");
    registry
        .inspect(&child.id)
        .await
        .expect_err("the removed child is gone");
}

#[tokio::test]
async fn remove_tree_removes_a_generationless_conflict_with_consent() {
    let (registry, data_dir, _worktree_root) = retention_registry("remove-tree-conflict-consent");
    let root = exited_session(&registry, None).await;
    let conflict =
        conflict_child_without_generation_on_repo(&registry, &data_dir, &root.id, None).await;

    let result = crate::session::remove_tree::remove_tree(&registry, &tree_params(&root.id, true))
        .await
        .expect("consent removes a generationless conflict");

    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(result.kept, 0, "{result:?}");
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == conflict)
        .expect("conflict child reported");
    assert!(
        child_node
            .removal
            .as_ref()
            .is_some_and(|removal| removal.removed && removal.identity_proof_skipped),
        "consent replaces no proof, and says so: {result:?}"
    );
    assert!(
        session_ids(&registry).await.is_empty(),
        "root and conflict are gone"
    );
}

#[tokio::test]
async fn remove_tree_keeps_a_child_resumed_during_the_hold_inspection() {
    let (registry, _data_dir, _worktree_root) =
        retention_registry("remove-tree-resumed-during-hold");
    let root = exited_session(&registry, None).await;
    let child = exited_child(&registry, &root.id, None).await;

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    crate::session::remove_tree::arm_pre_removal_gate_for_test(&child.id, entered_tx, resume_rx);
    let cascade = tokio::spawn({
        let registry = registry.clone();
        let root = root.id.clone();
        async move {
            crate::session::remove_tree::remove_tree(&registry, &tree_params(&root, false)).await
        }
    });
    entered_rx
        .await
        .expect("the cascade reaches the child removal");
    {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&child.id).expect("child entry");
        entry.info.state = protocol::SessionState::Running;
        entry.info.runtime = None;
    };
    resume_tx.send(()).expect("release the cascade");
    let result = cascade
        .await
        .expect("the cascade joins")
        .expect("a resumed child is kept, not raised");

    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.kept, 2, "{result:?}");
    assert_eq!(result.refused, 0, "{result:?}");
    let child_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == child.id)
        .expect("child reported");
    assert_eq!(child_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        child_node.keep_reason,
        Some(SessionTreeKeepReason::Live),
        "{result:?}"
    );
    let root_node = result
        .sessions
        .iter()
        .find(|node| node.session_id == root.id)
        .expect("root reported");
    assert_eq!(root_node.outcome, SessionTreeOutcome::Kept, "{result:?}");
    assert_eq!(
        root_node.keep_reason,
        Some(SessionTreeKeepReason::DescendantKept),
        "{result:?}"
    );
    let listed = registry
        .inspect(&child.id)
        .await
        .expect("the resumed child stays listed");
    assert_eq!(
        listed.state,
        protocol::SessionState::Running,
        "the cascade never stopped the resumed child: {listed:?}"
    );
    assert!(
        listed.can_stop(),
        "the resumed child is still live: {listed:?}"
    );
}
