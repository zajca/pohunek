//! Agent selection and the read-access preflight.
//!
//! The assistant must start with a capable coding agent, never a plain shell.
//! Selection is explicit and reported in both human and JSON output, and is
//! always overridable with `--agent`.

use std::path::Path;

use protocol::{HostCapabilities, ProtocolError};

use crate::error::CliError;

/// The chosen agent and the reason it was chosen, surfaced in output so the
/// selection is never silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSelection {
    pub(crate) name: String,
    pub(crate) reason: String,
}

/// Agents preferred for the assistant, in ranking order. A user-defined
/// `pohunek-assistant` profile wins, then the two first-class coding agents.
const RANKED_AGENTS: [&str; 3] = ["pohunek-assistant", "codex", "claude"];

/// Resolve which agent to launch.
///
/// Resolution order (design "Agent Selection"):
/// 1. `--agent` wins (trusted; the daemon resolves the profile on its host).
/// 2. a configured assistant default, when present.
/// 3. the documented ranking over runtimes the host reports available.
///
/// # Errors
///
/// [`ProtocolError::no_capable_agent`] when no capable runtime is available and
/// the caller named none.
pub(crate) fn select_agent(
    capabilities: &HostCapabilities,
    requested: Option<&str>,
    configured_default: Option<&str>,
) -> Result<AgentSelection, CliError> {
    if let Some(agent) = requested {
        return Ok(AgentSelection {
            name: agent.to_owned(),
            reason: "selected explicitly via --agent".to_owned(),
        });
    }

    if let Some(default) = configured_default {
        return Ok(AgentSelection {
            name: default.to_owned(),
            reason: "configured assistant default".to_owned(),
        });
    }

    for candidate in RANKED_AGENTS {
        if is_available(capabilities, candidate) {
            return Ok(AgentSelection {
                name: candidate.to_owned(),
                reason: format!("highest-ranked available runtime ({candidate})"),
            });
        }
    }

    // Fall back to any other available non-shell runtime before giving up.
    if let Some(runtime) = capabilities
        .runtimes
        .iter()
        .find(|runtime| runtime.available && runtime.agent != "shell")
    {
        return Ok(AgentSelection {
            name: runtime.agent.clone(),
            reason: format!("available host runtime ({})", runtime.agent),
        });
    }

    Err(ProtocolError::no_capable_agent().into())
}

fn is_available(capabilities: &HostCapabilities, agent: &str) -> bool {
    capabilities
        .runtimes
        .iter()
        .any(|runtime| runtime.agent == agent && runtime.available)
        || capabilities.supported_agents.iter().any(|a| a == agent)
}

/// Confirm the selected agent's execution context can read the materialized
/// knowledge directory and the snapshot file before `session.new`.
///
/// Today the agent shares the daemon UID with no sandbox, so for a local launch
/// this is a path-exists + readable assertion. Remote launches cannot stat the
/// remote filesystem from here; the remote daemon already materialized and
/// returned both paths, which proves they exist in its filesystem view. The
/// `remote` branch is kept as the forward seam for profiles that declare a
/// restricted root.
///
/// # Errors
///
/// [`ProtocolError::agent_cannot_read_bundle`] when a local path is unreadable.
pub(crate) fn preflight_read_access(
    bundle_path: &str,
    snapshot_path: &str,
    remote: bool,
) -> Result<(), CliError> {
    if remote {
        return Ok(());
    }

    let bundle_index = Path::new(bundle_path).join("index.md");
    check_readable(
        &bundle_index,
        "materialized knowledge bundle is not readable",
    )?;
    check_readable(
        Path::new(snapshot_path),
        "materialized snapshot is not readable",
    )?;
    Ok(())
}

/// For degraded launches: confirm only the snapshot file is readable.
///
/// The knowledge bundle is absent by design in degraded mode, so only the
/// snapshot path is checked. Remote degraded launches skip even this check (the
/// remote daemon materialized the snapshot and proved it exists).
///
/// # Errors
///
/// [`ProtocolError::agent_cannot_read_bundle`] when the snapshot is unreadable.
pub(crate) fn preflight_snapshot_readable(snapshot_path: &str) -> Result<(), CliError> {
    check_readable(
        Path::new(snapshot_path),
        "degraded snapshot is not readable",
    )
}

fn check_readable(path: &Path, constraint: &str) -> Result<(), CliError> {
    match std::fs::File::open(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(ProtocolError::agent_cannot_read_bundle(
            &path.display().to_string(),
            &format!("{constraint}: {err}"),
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{AgentRuntime, PROTOCOL_VERSION};

    use super::*;

    fn caps(runtimes: Vec<(&str, bool)>) -> HostCapabilities {
        HostCapabilities {
            daemon_version: "test".to_owned(),
            protocol_version: PROTOCOL_VERSION,
            supported_agents: runtimes
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            runtimes: runtimes
                .into_iter()
                .map(|(name, available)| AgentRuntime {
                    agent: name.to_owned(),
                    available,
                    path: None,
                })
                .collect(),
            git_available: true,
            worktree_supported: true,
        }
    }

    #[test]
    fn explicit_agent_wins() {
        let selection = select_agent(&caps(vec![("codex", true)]), Some("my-profile"), None)
            .expect("explicit agent");
        assert_eq!(selection.name, "my-profile");
    }

    #[test]
    fn ranking_prefers_pohunek_assistant_then_codex() {
        let selection = select_agent(
            &caps(vec![("claude", true), ("codex", true), ("shell", true)]),
            None,
            None,
        )
        .expect("ranked");
        assert_eq!(selection.name, "codex");
    }

    #[test]
    fn no_capable_agent_when_only_shell() {
        let err =
            select_agent(&caps(vec![("shell", true)]), None, None).expect_err("no capable agent");
        let CliError::Protocol(source) = err else {
            panic!("expected protocol error");
        };
        assert_eq!(source.code, "no_capable_agent");
    }

    #[test]
    fn preflight_fails_for_missing_local_bundle() {
        let err = preflight_read_access("/nonexistent/bundle", "/nonexistent/snap.json", false)
            .expect_err("missing bundle");
        let CliError::Protocol(source) = err else {
            panic!("expected protocol error");
        };
        assert_eq!(source.code, "agent_cannot_read_bundle");
    }

    #[test]
    fn preflight_skips_local_checks_for_remote() {
        preflight_read_access("/remote/bundle", "/remote/snap.json", true).expect("remote ok");
    }
}
