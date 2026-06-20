//! Live host-capability snapshot builder for `host.inspect`.
//!
//! Builds a [`HostCapabilities`] describing what this host can do right now:
//! which protocol version the daemon speaks, which agent kinds it supports,
//! which agent runtimes are actually installed (probed against `PATH`), and
//! whether git-backed worktree sessions are available. The snapshot is built
//! fresh on every request, so it always reflects the host as it is now and is
//! never cached.

use protocol::{AgentKind, AgentRuntime, HostCapabilities, PROTOCOL_VERSION};

/// Build the live capability snapshot for this host.
///
/// `supported_agents` is the fixed set of agent kinds the daemon knows how to
/// launch. `runtimes` reports, per agent, whether its backing binary is present
/// on `PATH`: the shell runtime is always available (no path), while `codex` and
/// `claude` are probed via [`which_on_path`]. `git_available` reflects a `git`
/// probe and currently also gates worktree support.
#[must_use]
pub fn host_capabilities(daemon_version: &str) -> HostCapabilities {
    let supported_agents = vec![AgentKind::Shell, AgentKind::Codex, AgentKind::Claude];

    let runtimes = vec![
        // The shell runtime is always available; the daemon falls back to a
        // login shell and does not require a named binary on PATH.
        AgentRuntime {
            agent: AgentKind::Shell,
            available: true,
            path: None,
        },
        probe_runtime(AgentKind::Codex, "codex"),
        probe_runtime(AgentKind::Claude, "claude"),
    ];

    let git_available = which_on_path("git").is_some();

    HostCapabilities {
        daemon_version: daemon_version.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents,
        runtimes,
        git_available,
        // Worktree-per-session is implemented on top of `git worktree`, so its
        // availability currently follows git's presence on the host.
        worktree_supported: git_available,
    }
}

/// Probe `PATH` for an agent's backing binary and build its runtime entry.
///
/// `available` is true exactly when the binary resolves on `PATH`; the resolved
/// path is reported when found.
fn probe_runtime(agent: AgentKind, binary: &str) -> AgentRuntime {
    match which_on_path(binary) {
        Some(path) => AgentRuntime {
            agent,
            available: true,
            path: Some(path.display().to_string()),
        },
        None => AgentRuntime {
            agent,
            available: false,
            path: None,
        },
    }
}

/// Resolve a binary name against the `PATH` environment variable.
///
/// A small dependency-free `which`: splits `PATH`, joins the name, and returns
/// the first entry that exists and is a regular file. Mirrors the same helper in
/// the CLI's `doctor` command rather than pulling in a crate for one call.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_protocol_version_and_three_supported_agents() {
        let caps = host_capabilities("1.2.3-test");
        assert_eq!(caps.daemon_version, "1.2.3-test");
        assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            caps.supported_agents,
            vec![AgentKind::Shell, AgentKind::Codex, AgentKind::Claude]
        );
        // worktree support tracks git availability.
        assert_eq!(caps.worktree_supported, caps.git_available);
    }

    #[test]
    fn shell_runtime_is_always_available() {
        let caps = host_capabilities("0.0.0");
        let shell = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == AgentKind::Shell)
            .expect("shell runtime is reported");
        assert!(shell.available, "shell runtime must always be available");
        assert!(
            shell.path.is_none(),
            "shell runtime carries no resolved path"
        );
    }

    #[test]
    fn agent_runtime_availability_matches_resolved_path() {
        let caps = host_capabilities("0.0.0");
        for runtime in &caps.runtimes {
            if runtime.agent == AgentKind::Shell {
                continue;
            }
            // For probed agents, availability is exactly path presence.
            assert_eq!(runtime.available, runtime.path.is_some());
        }
    }
}
