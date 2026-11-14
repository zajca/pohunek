//! Attention isolation of session descendants from a flagged parent.
//!
//! A session carrying the reserved
//! [`protocol::session::ATTENTION_CHILDREN_METADATA_KEY`] metadata key set to
//! [`protocol::session::ATTENTION_CHILDREN_ISOLATE`] stops the attention of its
//! descendants from reaching it or the owner. A descendant is a session
//! reachable through [`protocol::SessionInfo::spawned_by`] links; the walk
//! reports the nearest flagged ancestor, so nested flags resolve to the closest
//! one. The flagged session itself is never isolated by its own flag.
//!
//! One shared walk backs both the `session.list` annotation and the attention
//! coordinator's inbox check, so the two always agree on which session isolates
//! which descendant.

// Rust guideline compliant 2026-11-13

use std::collections::{HashMap, HashSet};

use super::SessionRegistry;
use protocol::{
    SessionId, SessionInfo, ATTENTION_CHILDREN_ISOLATE, ATTENTION_CHILDREN_METADATA_KEY,
};

/// The flagged ancestor isolating `session`, walking `spawned_by` links.
///
/// `lookup` resolves a session id to its current info; the walk stops at a
/// broken chain (an ancestor the registry does not hold) and keeps a visited
/// set, so a malformed loop cannot hang the caller. The session itself is
/// skipped: only ancestors flag descendants.
///
/// With one accepted flag value, any flagged ancestor isolates; the nearest
/// one is reported.
#[must_use]
pub(crate) fn isolating_ancestor<'a>(
    session: &SessionInfo,
    lookup: impl Fn(&SessionId) -> Option<&'a SessionInfo>,
) -> Option<SessionId> {
    let mut visited = HashSet::new();
    visited.insert(session.id.clone());
    let mut current = session
        .spawned_by
        .as_ref()
        .map(|spawned| &spawned.session_id);
    while let Some(id) = current {
        if !visited.insert(id.clone()) {
            return None;
        }
        let ancestor = lookup(id)?;
        if ancestor
            .metadata
            .get(ATTENTION_CHILDREN_METADATA_KEY)
            .is_some_and(|value| value.as_str() == ATTENTION_CHILDREN_ISOLATE)
        {
            return Some(ancestor.id.clone());
        }
        current = ancestor
            .spawned_by
            .as_ref()
            .map(|spawned| &spawned.session_id);
    }
    None
}

/// The flagged ancestor isolating the session `id` names, or `None`.
///
/// Walks the `spawned_by` chain of `id` under the registry lock with a
/// borrowing lookup, so no `SessionInfo` is cloned; a broken chain or an
/// unknown session answers `None`. The attention coordinator calls this for
/// a deferred record that still carries an inbox candidate and before every
/// commit, so isolated descendants never produce an inbox record, while
/// their turn signals are still journaled and `session.wait_turn` keeps
/// working under isolation.
pub(crate) async fn attention_isolated_by(
    registry: &SessionRegistry,
    id: &SessionId,
) -> Option<SessionId> {
    let entries = registry.inner.sessions.lock().await;
    let session = entries.get(id).map(|entry| &entry.info)?;
    isolating_ancestor(session, |ancestor| {
        entries.get(ancestor).map(|entry| &entry.info)
    })
}

/// Fill `attention_isolated_by` on every session of a `session.list` answer.
///
/// The snapshot is built once from `sessions`, then each session is walked
/// against it, so the annotation costs one pass with no per-session lock.
pub(super) fn annotate_list(sessions: &mut [SessionInfo]) {
    let snapshot: HashMap<SessionId, SessionInfo> = sessions
        .iter()
        .map(|info| (info.id.clone(), info.clone()))
        .collect();
    for info in sessions.iter_mut() {
        info.attention_isolated_by = isolating_ancestor(info, |id| snapshot.get(id));
    }
}
