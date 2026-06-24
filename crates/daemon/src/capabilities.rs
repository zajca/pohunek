//! Live host-capability snapshot builder for `host.inspect`.
//!
//! Builds a [`HostCapabilities`] describing what this host can do right now:
//! which protocol version the daemon speaks, which agent kinds it supports,
//! which agent runtimes are actually installed (probed against `PATH`), and
//! whether git-backed worktree sessions are available. The snapshot is built
//! fresh on every request, so it always reflects the host as it is now and is
//! never cached.

use protocol::{AgentRuntime, HostCapabilities, PROTOCOL_VERSION};

use crate::agent::{default_program, which_executable, ProfileRegistry};

/// Build the live capability snapshot for this host.
///
/// `supported_agents` is the three compiled base kinds plus every resolvable host
/// agent profile (Part C). `runtimes` reports, per agent, whether its backing
/// program is present on `PATH`: the shell runtime is always available (no path),
/// `codex`/`claude` are probed, and each profile probes its (possibly-overridden)
/// program. Probing uses the same executable check as the launch path
/// ([`which_executable`]) so "available" agrees with what a launch would accept.
/// `git_available` reflects a `git` probe and currently also gates worktree support.
#[must_use]
pub(crate) fn host_capabilities(
    daemon_version: &str,
    profiles: &ProfileRegistry,
) -> HostCapabilities {
    let mut supported_agents = vec!["shell".to_owned(), "codex".to_owned(), "claude".to_owned()];

    let mut runtimes = vec![
        // The shell runtime is always available; the daemon falls back to a
        // login shell and does not require a named binary on PATH.
        AgentRuntime {
            agent: "shell".to_owned(),
            available: true,
            path: None,
        },
        probe_runtime("codex", "codex"),
        probe_runtime("claude", "claude"),
    ];

    // Host profiles: each resolvable profile is a launchable agent name; probe its
    // resolved program exactly as a base kind, so availability is consistent.
    for agent in profiles.enumerate() {
        let program = agent.profile.as_ref().map_or_else(
            || default_program(agent.base),
            |profile| profile.program.clone(),
        );
        runtimes.push(probe_runtime(&agent.name, &program));
        supported_agents.push(agent.name);
    }

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

/// Probe `PATH` for an agent's backing program and build its runtime entry.
///
/// `available` is true exactly when the program resolves to an **executable** on
/// `PATH` (the launch-path check, [`which_executable`]); the resolved path is
/// reported when found.
fn probe_runtime(agent: &str, binary: &str) -> AgentRuntime {
    match which_executable(binary) {
        Some(path) => AgentRuntime {
            agent: agent.to_owned(),
            available: true,
            path: Some(path.display().to_string()),
        },
        None => AgentRuntime {
            agent: agent.to_owned(),
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// An empty profile registry (no host-config layer) for the base-kind tests.
    fn no_profiles() -> ProfileRegistry {
        ProfileRegistry::default()
    }

    fn temp_agents_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pohunek-caps-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create agents dir");
        dir
    }

    #[test]
    fn snapshot_reports_protocol_version_and_three_supported_agents() {
        let caps = host_capabilities("1.2.3-test", &no_profiles());
        assert_eq!(caps.daemon_version, "1.2.3-test");
        assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
        assert_eq!(caps.supported_agents, vec!["shell", "codex", "claude"]);
        // worktree support tracks git availability.
        assert_eq!(caps.worktree_supported, caps.git_available);
    }

    #[test]
    fn shell_runtime_is_always_available() {
        let caps = host_capabilities("0.0.0", &no_profiles());
        let shell = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "shell")
            .expect("shell runtime is reported");
        assert!(shell.available, "shell runtime must always be available");
        assert!(
            shell.path.is_none(),
            "shell runtime carries no resolved path"
        );
    }

    #[test]
    fn agent_runtime_availability_matches_resolved_path() {
        let caps = host_capabilities("0.0.0", &no_profiles());
        for runtime in &caps.runtimes {
            if runtime.agent == "shell" {
                continue;
            }
            // For probed agents, availability is exactly path presence.
            assert_eq!(runtime.available, runtime.path.is_some());
        }
    }

    #[test]
    fn host_capabilities_enumerates_resolvable_profiles_and_probes_their_program() {
        let dir = temp_agents_dir();
        // A profile whose program is a real executable so availability is true.
        std::fs::write(
            dir.join("my-claude.toml"),
            "base = \"claude\"\nprogram = \"/bin/sh\"\n",
        )
        .expect("write profile");
        let caps = host_capabilities("0.0.0", &ProfileRegistry::new(Some(dir)));

        assert!(
            caps.supported_agents.contains(&"my-claude".to_owned()),
            "a resolvable profile is a supported agent: {:?}",
            caps.supported_agents
        );
        let runtime = caps
            .runtimes
            .iter()
            .find(|runtime| runtime.agent == "my-claude")
            .expect("profile runtime is probed");
        // The availability invariant holds for profile programs too.
        assert_eq!(runtime.available, runtime.path.is_some());
        assert!(runtime.available, "/bin/sh resolves as executable");
    }
}
