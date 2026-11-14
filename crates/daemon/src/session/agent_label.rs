//! Live `agent_display_name` enrichment for `session list` and `session inspect`.
//!
//! The label is presentation only: it is read live from the current profile
//! file, so a `display_name` edit shows immediately without touching the
//! stored record. Best-effort: a session whose agent is a bare runtime, whose
//! profile file is gone, or whose profile no longer parses keeps the absent
//! form and shows by agent name. Only the two handlers this module serves set
//! the field; stored records and event frames never carry it.

use std::collections::{HashMap, HashSet};

use protocol::SessionInfo;

use super::SessionRegistry;

/// Distinct profile names the sessions reference, in first-seen order.
fn distinct_agents(sessions: &[SessionInfo]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut agents = Vec::new();
    for session in sessions {
        if seen.insert(session.agent.clone()) {
            agents.push(session.agent.clone());
        }
    }
    agents
}

impl SessionRegistry {
    /// Set each session's `agent_display_name` from its agent profile's
    /// `display_name` key, read live from the current profile file.
    ///
    /// Each distinct `agent` name resolves once on the blocking pool (the read
    /// follows the file rules of a launch, with no base resolution and no
    /// package lookup). A name the registry serves has no profile file and
    /// logs nothing; only any other missing or unreadable file (`Gone`)
    /// logs, at `warn` with the name only.
    pub(crate) async fn enrich_agent_labels(&self, sessions: &mut [SessionInfo]) {
        let agents = distinct_agents(sessions);
        if agents.is_empty() {
            return;
        }
        let profiles = self.inner.profiles.clone();
        let labels = self
            .inner
            .background
            .spawn_blocking("enrich_agent_labels", move || {
                let mut labels = HashMap::with_capacity(agents.len());
                for agent in &agents {
                    let read = crate::agent::profile::display_name_of(&profiles, agent.as_str());
                    if read == crate::agent::profile::LabelRead::Gone {
                        tracing::warn!(
                            agent = agent.as_str(),
                            event = "agent_label.unreadable",
                            "agent profile file unreadable; showing by agent name"
                        );
                    }
                    labels.insert(
                        agent.clone(),
                        match read {
                            crate::agent::profile::LabelRead::Label(label) => Some(label),
                            crate::agent::profile::LabelRead::NoLabel
                            | crate::agent::profile::LabelRead::Broken
                            | crate::agent::profile::LabelRead::Gone => None,
                        },
                    );
                }
                labels
            })
            .await
            .unwrap_or_default();
        for session in sessions.iter_mut() {
            session.agent_display_name = labels.get(&session.agent).cloned().flatten();
        }
    }
}
