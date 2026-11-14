//! Which host agent profiles the sessions use, for `package.remove_profile`.
//!
//! A removal runs under the exclusive package lifecycle lock and refuses a
//! profile that a session which is not terminal runs. A resume resolves its
//! profile under the shared lock, so it runs either before the removal (and
//! its session is then counted) or after it (and finds no profile). A create
//! resolves its profile before it takes the shared lock; it reads the profile
//! again once it holds the lock, so it never launches a profile removed in
//! between.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeSet;

use protocol::{ProtocolError, SessionNewParams};

use super::{host, SessionRegistry};
use crate::agent::{agent_profile_not_found, ResolvedAgent};

impl SessionRegistry {
    /// The agent names of the sessions that are not terminal (`starting` or
    /// `running`); each such session runs its profile.
    pub(crate) async fn live_agents(&self) -> BTreeSet<String> {
        self.inner
            .sessions
            .lock()
            .await
            .values()
            .filter(|entry| !entry.info.state.is_terminal())
            .map(|entry| entry.info.agent.clone())
            .collect()
    }

    /// Refuses the create of a profile-backed `resolved` agent whose name no
    /// longer resolves to a host profile.
    ///
    /// The caller holds the shared package lifecycle guard, so a profile
    /// removal either finished before this read or starts after the session
    /// is registered, when its live session refuses the removal.
    ///
    /// # Errors
    ///
    /// The resolution error of the name as it is now, or
    /// `agent_profile_not_found` when it now names a bare runtime.
    pub(super) fn ensure_profile_still_resolves(
        &self,
        params: &SessionNewParams,
        resolved: &ResolvedAgent,
    ) -> Result<(), ProtocolError> {
        if resolved.profile.is_none() {
            return Ok(());
        }
        let current = host::LaunchSource::OwnerLocal {
            name: params.agent.clone(),
            options: params.agent_options.clone(),
        }
        .resolve(&self.inner.profiles)?;
        if current.profile.is_none() {
            return Err(agent_profile_not_found(&params.agent));
        }
        Ok(())
    }

    /// Parks a create after its agent resolved, before it takes the package
    /// launch guard, when a test armed the hold.
    #[cfg(test)]
    pub(super) async fn hold_resolved_create(&self) {
        let gate = self
            .inner
            .resolved_create_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }
}
