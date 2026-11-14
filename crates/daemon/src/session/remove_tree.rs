//! `session.remove_tree`: remove a session together with its descendants.
//!
//! The cascade walks the tree deepest first, each node through the normal
//! removal safety: a live node is never stopped, a worktree hold keeps the
//! node, and a conflict follows the explicit-consent path only. A kept or
//! refused node keeps its ancestors, and the named root follows the rules
//! of `session.remove` exactly once every descendant is gone.

use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use protocol::SessionId;
use protocol::{
    ProtocolError, SessionInfo, SessionRemoveTreeParams, SessionRemoveTreeResult,
    SessionRetentionCandidate, SessionRetentionReason, SessionTree, SessionTreeKeepReason,
    SessionTreeOutcome, SessionTreeRemoval, MAX_SESSION_REMOVE_TREE_SESSIONS,
};
use tracing::info;

use super::{SessionRegistry, UnconfirmedCleanup};

/// Test-only gate before the root removal: the child-added-during-cascade test arms it, waits
/// until the cascade reaches the root, creates a new child of the root, and
/// releases the cascade, proving the re-enumeration keeps the root with
/// `descendant_added`. Production builds never see it.
#[cfg(test)]
struct PreRootGate {
    root: SessionId,
    entered_tx: tokio::sync::oneshot::Sender<()>,
    resume_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static PRE_ROOT_GATE: std::sync::Mutex<Option<PreRootGate>> = std::sync::Mutex::new(None);

/// Test-only gate before a descendant removal: the child-resumed-during-hold test arms it,
/// waits until the cascade passes the worktree hold for that session, makes
/// the session live, and releases the cascade, proving the post-hold
/// re-read keeps it live instead of stopping and removing it. Production
/// builds never see it.
#[cfg(test)]
struct PreRemovalGate {
    session: SessionId,
    entered_tx: tokio::sync::oneshot::Sender<()>,
    resume_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static PRE_REMOVAL_GATE: std::sync::Mutex<Option<PreRemovalGate>> = std::sync::Mutex::new(None);

/// Arms the pre-removal gate for the next `remove_descendant` call of
/// `session` in this process.
///
/// The next call for that session reports when it reaches the removal and
/// waits for the resume signal, so a test can change the session between
/// the hold inspection and `remove_with` deterministically. A call for
/// another session leaves the gate armed, so parallel cascades cannot steal
/// each other's gate.
#[cfg(test)]
pub(crate) fn arm_pre_removal_gate_for_test(
    session: &SessionId,
    entered_tx: tokio::sync::oneshot::Sender<()>,
    resume_rx: tokio::sync::oneshot::Receiver<()>,
) {
    *PRE_REMOVAL_GATE.lock().expect("pre-removal gate") = Some(PreRemovalGate {
        session: session.clone(),
        entered_tx,
        resume_rx,
    });
}

/// Arms the pre-root gate for the next `remove_tree` call of `root` in this
/// process.
///
/// The next call for that root reports when it reaches the root and waits
/// for the resume signal, so a test can add a child between the enumeration
/// and the root removal deterministically. A call for another root leaves
/// the gate armed, so parallel cascades cannot steal each other's gate.
#[cfg(test)]
pub(crate) fn arm_pre_root_gate_for_test(
    root: &SessionId,
    entered_tx: tokio::sync::oneshot::Sender<()>,
    resume_rx: tokio::sync::oneshot::Receiver<()>,
) {
    *PRE_ROOT_GATE.lock().expect("pre-root gate") = Some(PreRootGate {
        root: root.clone(),
        entered_tx,
        resume_rx,
    });
}

/// Removes `params.session_id` together with its descendants, deepest first.
///
/// The snapshot enumerates the tree once for the size cap and the walk
/// order; every node is then re-read from the registry right before its
/// removal, and a node with children is re-enumerated before it. A node
/// with a kept or refused descendant, or with a child that appeared after
/// the snapshot, is kept with `descendant_kept` or `descendant_added`.
/// The named root is resolved before anything runs: a root the fresh list
/// no longer names is `session_not_found`, and an external root is refused
/// like `session.remove` refuses it, both before any descendant is walked.
///
/// # Errors
///
/// Returns `bad_request` when the tree exceeds the size cap. A descendant
/// that fails is reported as `refused` in the result, and so is the root:
/// a failed root removal after removed descendants is a `refused` root row
/// with its error, never a raised error.
pub(crate) async fn remove_tree(
    registry: &SessionRegistry,
    params: &SessionRemoveTreeParams,
) -> Result<SessionRemoveTreeResult, ProtocolError> {
    let cleanup = UnconfirmedCleanup::from(params.accept_unconfirmed_cleanup);
    let snapshot = registry.list_raw().await;
    let tree = SessionTree::new(&snapshot);
    let descendants = tree.descendants_post_order(&params.session_id);
    let total = descendants.len().saturating_add(1);
    if total > MAX_SESSION_REMOVE_TREE_SESSIONS {
        return Err(ProtocolError::bad_request(format!(
            "session.remove_tree selects {total} sessions, above the {MAX_SESSION_REMOVE_TREE_SESSIONS} session cap"
        )));
    }

    // The root check runs before any descendant is walked: a gone or
    // external root never removes the sessions its absence orphaned.
    let root_info = snapshot.iter().find(|info| info.id == params.session_id);
    match root_info {
        None => return Err(super::session_not_found(&params.session_id.0)),
        Some(info) if info.external == Some(true) => {
            return Err(super::session_external_read_only(&params.session_id));
        }
        Some(_) => {}
    }

    let mut finished: BTreeMap<String, SessionTreeOutcome> = BTreeMap::new();
    let mut sessions: Vec<SessionTreeRemoval> = Vec::with_capacity(total);
    for node in &descendants {
        let removal = remove_descendant(registry, node, &finished, cleanup).await;
        if matches!(
            removal.outcome,
            SessionTreeOutcome::Kept | SessionTreeOutcome::Refused
        ) {
            info!(
                session_id = %removal.session_id.0,
                session.outcome = removal.outcome.as_str(),
                session.keep_reason = removal.keep_reason.map_or("refused", SessionTreeKeepReason::as_str),
                session.parent = node.parent.as_ref().map_or("", |parent| parent.0.as_str()),
                "session.remove_tree kept a descendant"
            );
        }
        finished.insert(node.session_id.0.clone(), removal.outcome);
        sessions.push(removal);
    }

    #[cfg(test)]
    wait_for_pre_root_gate(&params.session_id).await;
    #[cfg(not(test))]
    wait_for_pre_root_gate();
    let root = remove_root(registry, params, &finished, cleanup).await;
    if root.outcome != SessionTreeOutcome::Removed {
        info!(
            session_id = %root.session_id.0,
            session.outcome = root.outcome.as_str(),
            session.keep_reason = root.keep_reason.map_or("", SessionTreeKeepReason::as_str),
            "session.remove_tree kept the root"
        );
    }
    sessions.push(root);

    let removed = count_outcome(&sessions, SessionTreeOutcome::Removed);
    let kept = count_outcome(&sessions, SessionTreeOutcome::Kept);
    let refused = count_outcome(&sessions, SessionTreeOutcome::Refused);
    info!(
        session.root = %params.session_id.0,
        session.removed = removed,
        session.kept = kept,
        session.refused = refused,
        "session.remove_tree finished"
    );
    Ok(SessionRemoveTreeResult {
        removed,
        kept,
        refused,
        sessions,
    })
}

/// Decides whether `info` is kept without removal, before the worktree hold
/// runs and again after it: the hold inspects the worktree with blocking git
/// calls, so a session that became live while the hold ran must be kept,
/// never stopped. Returns `None` when the cascade may proceed to the hold
/// (or, after the hold, to `remove_with`).
fn live_keep_reason(
    info: &SessionInfo,
    cleanup: UnconfirmedCleanup,
) -> Option<SessionTreeKeepReason> {
    if info.external == Some(true) {
        return Some(SessionTreeKeepReason::External);
    }
    if is_conflict(info) && !names_recorded_worker(info) {
        // A conflict that names no worker generation has no proof to retire:
        // only the operator's explicit consent removes it, through the
        // normal `remove_with(Accept)` path. Without consent the cascade
        // never calls removal for it; it is kept, not refused. Consent
        // covers the unproven cleanup only, never the data: the worktree
        // hold still runs first, so a dirty checkout is always kept.
        if cleanup != UnconfirmedCleanup::Accept {
            return Some(SessionTreeKeepReason::NotRemovable);
        }
        return None;
    }
    if !info.can_remove() {
        return Some(SessionTreeKeepReason::NotRemovable);
    }
    if info.can_stop() && !is_conflict(info) {
        return Some(SessionTreeKeepReason::Live);
    }
    None
}

/// Removes one descendant of the named root, or keeps it for a recorded
/// reason. The descendant is re-read from the registry first, so a session
/// that became live since the snapshot is kept, and the node's current
/// direct children decide `descendant_kept` and `descendant_added`. The
/// entry is re-read again after the worktree hold, immediately before
/// `remove_with`, so a session resumed during the hold inspection is kept
/// live instead of stopped and removed.
async fn remove_descendant(
    registry: &SessionRegistry,
    node: &protocol::TreeNode,
    finished: &BTreeMap<String, SessionTreeOutcome>,
    cleanup: UnconfirmedCleanup,
) -> SessionTreeRemoval {
    let fresh = registry.list_raw().await;
    let fresh_tree = SessionTree::new(&fresh);
    let current = fresh_tree
        .children(&node.session_id)
        .iter()
        .map(|child| child.id.0.clone())
        .collect::<BTreeSet<_>>();
    let mut added = false;
    let mut doomed = false;
    for child in &current {
        match finished.get(child) {
            Some(SessionTreeOutcome::Removed) | None => {
                if !finished.contains_key(child) {
                    added = true;
                    doomed = true;
                }
            }
            Some(
                SessionTreeOutcome::Kept
                | SessionTreeOutcome::Refused
                | SessionTreeOutcome::Unknown,
            ) => {
                doomed = true;
            }
        }
    }
    // A snapshot descendant the fresh list no longer names was removed
    // concurrently, which counts as removed; `spawned_by` never changes,
    // so the fresh enumeration covers every snapshot descendant.
    if doomed {
        return kept_removal(
            node,
            if added {
                SessionTreeKeepReason::DescendantAdded
            } else {
                SessionTreeKeepReason::DescendantKept
            },
            None,
        );
    }
    let Some(info) = fresh.iter().find(|info| info.id == node.session_id) else {
        // A concurrent remove already evicted the session, which counts as
        // removed exactly like `session.remove` reports it.
        return removed_removal(node, removal_concurrent());
    };
    if let Some(reason) = live_keep_reason(info, cleanup) {
        return kept_removal(node, reason, None);
    }
    if let Some(hold) = worktree_hold(registry, info).await {
        return kept_removal(node, SessionTreeKeepReason::WorktreeHeld, Some(hold));
    }
    // The hold inspection runs blocking git calls, so the entry is re-read
    // after it and gated again immediately before `remove_with`: a session
    // resumed during the hold is kept live instead of stopped and removed.
    // A concurrent remove in the same window counts as removed; a new live
    // state keeps the node, and the hold result above still stands.
    #[cfg(test)]
    wait_for_pre_removal_gate(&node.session_id).await;
    #[cfg(not(test))]
    wait_for_pre_removal_gate();
    let fresh = registry.list_raw().await;
    let Some(info) = fresh.iter().find(|info| info.id == node.session_id) else {
        return removed_removal(node, removal_concurrent());
    };
    if let Some(reason) = live_keep_reason(info, cleanup) {
        return kept_removal(node, reason, None);
    }
    run_removal(registry, node, cleanup).await
}

/// Removes the named root after every descendant is gone.
///
/// A kept or refused descendant, or a child that appeared after the
/// snapshot, keeps the root. Otherwise the root follows `session.remove`
/// exactly, including stopping a live root; a failed root removal is a
/// `refused` root row with its error, never a raised error, so partial
/// progress stays reported.
async fn remove_root(
    registry: &SessionRegistry,
    params: &SessionRemoveTreeParams,
    finished: &BTreeMap<String, SessionTreeOutcome>,
    cleanup: UnconfirmedCleanup,
) -> SessionTreeRemoval {
    let fresh = registry.list_raw().await;
    let fresh_tree = SessionTree::new(&fresh);
    let mut added = false;
    let mut doomed = false;
    for child in fresh_tree.children(&params.session_id) {
        match finished.get(&child.id.0) {
            Some(SessionTreeOutcome::Removed) | None => {
                if !finished.contains_key(&child.id.0) {
                    added = true;
                    doomed = true;
                }
            }
            Some(_) => {
                doomed = true;
            }
        }
    }
    let node = protocol::TreeNode {
        session_id: params.session_id.clone(),
        parent: None,
        depth: 0,
    };
    if doomed {
        return kept_removal(
            &node,
            if added {
                SessionTreeKeepReason::DescendantAdded
            } else {
                SessionTreeKeepReason::DescendantKept
            },
            None,
        );
    }
    // The root follows `session.remove` exactly: a live root is stopped
    // first. The pre-check above already refused a gone or external root
    // before anything ran, so an error here means the root failed after
    // its descendants were walked: it is a refused row, not a raised
    // error, and the removed descendants stay reported.
    match registry.remove_with(&params.session_id, cleanup).await {
        Ok(removal) => SessionTreeRemoval {
            session_id: params.session_id.clone(),
            parent: None,
            depth: 0,
            outcome: SessionTreeOutcome::Removed,
            keep_reason: None,
            hold: None,
            error: None,
            removal: Some(removal),
        },
        Err(error) => SessionTreeRemoval {
            session_id: params.session_id.clone(),
            parent: None,
            depth: 0,
            outcome: SessionTreeOutcome::Refused,
            keep_reason: None,
            hold: None,
            error: Some(error),
            removal: None,
        },
    }
}

/// Runs the normal per-session removal for one cascade node, reporting a
/// failure as `refused` with the error.
async fn run_removal(
    registry: &SessionRegistry,
    node: &protocol::TreeNode,
    cleanup: UnconfirmedCleanup,
) -> SessionTreeRemoval {
    match registry.remove_with(&node.session_id, cleanup).await {
        Ok(removal) => SessionTreeRemoval {
            session_id: node.session_id.clone(),
            parent: node.parent.clone(),
            depth: node.depth,
            outcome: SessionTreeOutcome::Removed,
            keep_reason: None,
            hold: None,
            error: None,
            removal: Some(removal),
        },
        Err(error) => SessionTreeRemoval {
            session_id: node.session_id.clone(),
            parent: node.parent.clone(),
            depth: node.depth,
            outcome: SessionTreeOutcome::Refused,
            keep_reason: None,
            hold: None,
            error: Some(error),
            removal: None,
        },
    }
}

/// Reports one kept cascade node with its reason.
fn kept_removal(
    node: &protocol::TreeNode,
    keep_reason: SessionTreeKeepReason,
    hold: Option<protocol::SessionRetentionHold>,
) -> SessionTreeRemoval {
    SessionTreeRemoval {
        session_id: node.session_id.clone(),
        parent: node.parent.clone(),
        depth: node.depth,
        outcome: SessionTreeOutcome::Kept,
        keep_reason: Some(keep_reason),
        hold,
        error: None,
        removal: None,
    }
}

/// Reports a node a concurrent remove already evicted.
fn removed_removal(
    node: &protocol::TreeNode,
    removal: protocol::SessionRemoveResult,
) -> SessionTreeRemoval {
    SessionTreeRemoval {
        session_id: node.session_id.clone(),
        parent: node.parent.clone(),
        depth: node.depth,
        outcome: SessionTreeOutcome::Removed,
        keep_reason: None,
        hold: None,
        error: None,
        removal: Some(removal),
    }
}

/// The removal result a concurrent remove leaves behind: nothing was
/// evicted by this call and nothing was stopped by it.
fn removal_concurrent() -> protocol::SessionRemoveResult {
    protocol::SessionRemoveResult {
        removed: false,
        stopped: false,
        worktrees_removed: 0,
        worktrees_failed: 0,
        accepted_unconfirmed_processes: Vec::new(),
        accepted_unconfirmed_total: None,
        identity_proof_skipped: false,
        warnings: Vec::new(),
    }
}

/// Whether the session's runtime is conflicted.
fn is_conflict(info: &SessionInfo) -> bool {
    info.runtime
        .as_ref()
        .is_some_and(|runtime| runtime.state == protocol::RuntimeState::Conflict)
}

/// Whether the session's runtime names the worker and instance the daemon
/// proves before it retires a conflict: the same predicate
/// [`SessionInfo::can_remove`] applies.
fn names_recorded_worker(info: &SessionInfo) -> bool {
    info.runtime
        .as_ref()
        .is_some_and(|runtime| runtime.worker_id.is_some() && runtime.worker_instance_id.is_some())
}

/// The worktree hold keeping `info`, if its owned checkout holds work.
///
/// The sweep's holds apply here: `session rm` itself never inspects
/// worktrees, so the cascade reuses the sweep's inspection instead of
/// reimplementing it.
async fn worktree_hold(
    registry: &SessionRegistry,
    info: &SessionInfo,
) -> Option<protocol::SessionRetentionHold> {
    let worktree_path = info.worktree_path.clone()?;
    let mut candidates = vec![SessionRetentionCandidate {
        session_id: info.id.clone(),
        reason: SessionRetentionReason::Terminal,
        age_secs: 0,
        worktree_path: Some(worktree_path),
        hold: None,
        removed: false,
    }];
    registry.apply_worktree_holds(&mut candidates).await;
    candidates.pop()?.hold
}

/// Counts the nodes with `outcome` in `sessions`.
fn count_outcome(sessions: &[SessionTreeRemoval], outcome: SessionTreeOutcome) -> u32 {
    u32::try_from(
        sessions
            .iter()
            .filter(|node| node.outcome == outcome)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

/// Waits for the test-only pre-removal gate armed for this session;
/// production builds have no gate and never wait. A gate armed for another
/// session stays armed.
#[cfg(test)]
async fn wait_for_pre_removal_gate(session: &SessionId) {
    let gate = PRE_REMOVAL_GATE.lock().expect("pre-removal gate").take();
    match gate {
        Some(gate) if gate.session == *session => {
            let _ = gate.entered_tx.send(());
            let _ = gate.resume_rx.await;
        }
        gate => {
            *PRE_REMOVAL_GATE.lock().expect("pre-removal gate") = gate;
        }
    }
}

#[cfg(not(test))]
fn wait_for_pre_removal_gate() {}

/// Waits for the test-only pre-root gate armed for this root; production
/// builds have no gate and never wait. A gate armed for another root stays
/// armed.
#[cfg(test)]
async fn wait_for_pre_root_gate(root: &SessionId) {
    let gate = PRE_ROOT_GATE.lock().expect("pre-root gate").take();
    match gate {
        Some(gate) if gate.root == *root => {
            let _ = gate.entered_tx.send(());
            let _ = gate.resume_rx.await;
        }
        gate => {
            *PRE_ROOT_GATE.lock().expect("pre-root gate") = gate;
        }
    }
}

#[cfg(not(test))]
fn wait_for_pre_root_gate() {}
