//! `session.adopt`: give a session a new parent.
//!
//! The parent link (`spawned_by`) decides which sessions a session's process
//! tree may act on, so only the owner changes it. The change runs under the
//! sessions lock and the persist lock, like a metadata edit, and it is refused
//! when it would close a loop or touch a tree a `session.remove_tree` cascade
//! is removing.

use protocol::{ProtocolError, SessionAdoptParams, SessionAdoptResult, SessionId, SpawnedBy};

use super::rights::descends_from;
use super::{event, session_not_found, timestamp_now, SessionRegistry};

/// Holds `root` in the registry's set of cascades in progress until dropped.
pub(super) struct RemovingRoot<'a> {
    registry: &'a SessionRegistry,
    root: SessionId,
}

impl<'a> RemovingRoot<'a> {
    /// Registers `root` as the root of a cascade in progress.
    pub(super) fn new(registry: &'a SessionRegistry, root: SessionId) -> Self {
        registry
            .inner
            .removing_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone());
        Self { registry, root }
    }
}

impl Drop for RemovingRoot<'_> {
    fn drop(&mut self) {
        self.registry
            .inner
            .removing_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.root);
    }
}

impl SessionRegistry {
    /// Registers a cascade rooted at `root`; see [`RemovingRoot`].
    pub(super) fn begin_tree_removal(&self, root: &SessionId) -> RemovingRoot<'_> {
        RemovingRoot::new(self, root.clone())
    }

    /// Makes `params.new_parent` the parent of `params.session_id`.
    ///
    /// # Errors
    ///
    /// `session_not_found` for an unknown id, the external-session refusal
    /// for an external one, and `session_adopt_refused` with the reason
    /// `self_parent`, `cycle` or `tree_removal_in_progress`.
    pub(crate) async fn adopt(
        &self,
        params: &SessionAdoptParams,
    ) -> Result<SessionAdoptResult, ProtocolError> {
        let child = &params.session_id;
        let parent = &params.new_parent;
        self.ensure_not_external(child).await?;
        self.ensure_not_external(parent).await?;
        if child == parent {
            return Err(ProtocolError::session_adopt_refused("self_parent"));
        }
        // The persist lock and the sessions lock are held from the checks
        // through the store write to the in-memory edit, so no record writer
        // interleaves and a cascade registered afterwards sees the new link.
        let persist = self.inner.persist_lock.lock().await;
        let (info, has_native) = {
            let mut sessions = self.inner.sessions.lock().await;
            if !sessions.contains_key(parent) {
                return Err(session_not_found(&parent.0));
            }
            if !sessions.contains_key(child) {
                return Err(session_not_found(&child.0));
            }
            let lookup = |id: &SessionId| sessions.get(id).map(|entry| &entry.info);
            if descends_from(lookup, parent, child) {
                return Err(ProtocolError::session_adopt_refused("cycle"));
            }
            if self.inside_tree_removal(lookup, child, parent) {
                return Err(ProtocolError::session_adopt_refused(
                    "tree_removal_in_progress",
                ));
            }
            let spawned_by = SpawnedBy {
                session_id: parent.clone(),
                daemon_id: self.inner.daemon_instance_id.clone(),
            };
            let updated_at = timestamp_now();
            self.commit_session_spawned_by(child, &spawned_by, &updated_at)
                .await?;
            let entry = sessions
                .get_mut(child)
                .ok_or_else(|| session_not_found(&child.0))?;
            entry.info.spawned_by = Some(spawned_by);
            entry.info.updated_at = updated_at;
            let has_native =
                entry.info.native_session_id.is_some() || entry.info.native_session_path.is_some();
            (entry.published_info(), has_native)
        };
        drop(persist);

        if has_native {
            self.persist_resume_binding(child).await;
        }
        self.emit(event::SESSION_UPDATED, &info);
        Ok(SessionAdoptResult { session: info })
    }

    /// Whether `child` or `parent` is, or lies below, the root of a cascade in
    /// progress.
    fn inside_tree_removal<'a>(
        &self,
        lookup: impl Fn(&SessionId) -> Option<&'a protocol::SessionInfo> + Copy,
        child: &SessionId,
        parent: &SessionId,
    ) -> bool {
        let roots = self
            .inner
            .removing_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        roots.iter().any(|root| {
            [child, parent]
                .into_iter()
                .any(|id| id == root || descends_from(lookup, id, root))
        })
    }

    async fn commit_session_spawned_by(
        &self,
        id: &SessionId,
        spawned_by: &SpawnedBy,
        updated_at: &str,
    ) -> Result<(), ProtocolError> {
        let Some(store) = self.store_for_write()? else {
            return Ok(());
        };
        let session_id = id.0.clone();
        let spawned_by = spawned_by.clone();
        let updated_at = updated_at.to_owned();
        self.inner
            .background
            .spawn_blocking("replace_session_spawned_by", move || {
                store.replace_session_spawned_by(&session_id, Some(&spawned_by), &updated_at)
            })
            .await
            .map_err(|_join_error| {
                super::runtime_error(
                    "session_store_failed",
                    format!("parent commit task panicked for {}", id.0),
                )
            })?
            .map_err(|error| {
                super::runtime_error(
                    "session_store_failed",
                    format!("failed to write the parent of {}: {error}", id.0),
                )
            })?;
        Ok(())
    }
}
