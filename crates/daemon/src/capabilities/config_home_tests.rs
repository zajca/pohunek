//! Tests of the opaque config-home identifier `host.inspect` reports.
//!
//! Every test builds its own host profiles, host-state directory and base
//! environment, so none reads or changes the process environment.

// Rust guideline compliant 2026-10-05

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};

use pohunek_worker_protocol::{BaseEnv, DEFAULT_ENVIRONMENT_ALLOWLIST};
use protocol::{AgentRuntime, HostCapabilities};

use super::host_capabilities;
use crate::agent::host::fixture::builtin_host;
use crate::agent::ProfileRegistry;
use crate::runtime::environment::{base_environment, EnvironmentSource};
use crate::test_support::{scoped_dir, ScopedDir};

/// Directory names no real path or variable contains, so a substring of one in
/// a reported value can only come from the value being derived from it.
const DISTINCTIVE_HOME: &str = "zq-account-home-7f3a91c4";
const DISTINCTIVE_OTHER: &str = "zq-other-home-5d02be68";

/// Hex digits of one identifier: 16 bytes.
const ID_HEX_LEN: usize = 32;

/// A private root holding the user's home directory, the host profiles and the
/// host-state directory of one daemon.
struct Rig {
    root: ScopedDir,
}

impl Rig {
    fn new() -> Self {
        let root = scoped_dir("pohunek-homeid-");
        for dir in ["home", "agents", "state"] {
            fs::create_dir_all(root.join(dir)).expect("create a rig directory");
        }
        // The key record lives in an owner-private directory.
        fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700))
            .expect("make the state directory owner-private");
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// A directory below the rig root, created.
    fn dir(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).expect("create a config home");
        if name.ends_with("state") {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
                .expect("make a state directory owner-private");
        }
        dir
    }

    fn profile(&self, name: &str, base: &str, variable: &str, value: &Path) {
        fs::write(
            self.root.join("agents").join(format!("{name}.toml")),
            format!(
                "base = \"{base}\"\n[env]\n{variable} = \"{}\"\n",
                value.display()
            ),
        )
        .expect("write a profile");
    }

    /// The base environment of a daemon whose user home is the rig's, plus
    /// the `extra` variables, which the daemon is configured to forward.
    fn base(&self, extra: &[(&str, &Path)]) -> BaseEnv {
        let mut variables = vec![(OsString::from("HOME"), self.home().as_os_str().to_owned())];
        variables.extend(
            extra
                .iter()
                .map(|(name, value)| (OsString::from(name), value.as_os_str().to_owned())),
        );
        let allowlist: Vec<&str> = DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .copied()
            .chain(extra.iter().map(|(name, _value)| *name))
            .collect();
        base_environment(&allowlist, &EnvironmentSource::fixed(variables))
            .expect("a base environment")
    }

    /// A registry over the rig's profiles keyed under `state`.
    fn registry(&self, state: Option<PathBuf>) -> ProfileRegistry {
        ProfileRegistry::with_runtimes(Some(self.root.join("agents")), builtin_host())
            .with_revision_state_dir(state)
    }

    fn inspect(&self, registry: &ProfileRegistry) -> HostCapabilities {
        host_capabilities("0.0.0", registry, &self.base(&[]))
    }
}

fn entry<'a>(caps: &'a HostCapabilities, agent: &str) -> &'a AgentRuntime {
    caps.runtimes
        .iter()
        .find(|runtime| runtime.agent == agent)
        .unwrap_or_else(|| panic!("{agent} is reported"))
}

fn id_of(caps: &HostCapabilities, agent: &str) -> String {
    entry(caps, agent)
        .config_home_id
        .clone()
        .unwrap_or_else(|| panic!("{agent} reports a config_home_id"))
}

#[test]
fn the_identifier_is_stable_across_calls_and_a_daemon_restart() {
    let rig = Rig::new();
    rig.profile("work", "claude", "CLAUDE_CONFIG_DIR", &rig.dir("work-home"));

    let first = rig.inspect(&rig.registry(Some(rig.state())));
    let registry = rig.registry(Some(rig.state()));
    let second = rig.inspect(&registry);
    let again = rig.inspect(&registry);

    for agent in ["claude", "codex", "work"] {
        let id = id_of(&first, agent);
        assert_eq!(id.len(), ID_HEX_LEN, "{agent}: {id}");
        assert!(id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
        // A second registry has a cold key cache, as after a restart.
        assert_eq!(id_of(&second, agent), id, "{agent} after a restart");
        assert_eq!(id_of(&again, agent), id, "{agent} on a second call");
    }
}

#[test]
fn distinct_homes_get_distinct_ids_and_one_canonical_home_gets_one() {
    let rig = Rig::new();
    let home = rig.dir("shared-home");
    let alias = rig.root.join("alias-home");
    symlink(&home, &alias).expect("symlink the shared home");
    rig.profile("direct", "claude", "CLAUDE_CONFIG_DIR", &home);
    rig.profile("aliased", "claude", "CLAUDE_CONFIG_DIR", &alias);
    rig.profile(
        "other",
        "claude",
        "CLAUDE_CONFIG_DIR",
        &rig.dir("other-home"),
    );
    rig.profile("codex-work", "codex", "CODEX_HOME", &rig.dir("codex-home"));

    let caps = rig.inspect(&rig.registry(Some(rig.state())));

    assert_eq!(id_of(&caps, "direct"), id_of(&caps, "aliased"));
    assert_ne!(id_of(&caps, "direct"), id_of(&caps, "other"));
    assert_ne!(id_of(&caps, "direct"), id_of(&caps, "codex-work"));
    assert_ne!(id_of(&caps, "claude"), id_of(&caps, "direct"));
    assert_ne!(id_of(&caps, "claude"), id_of(&caps, "codex"));
}

#[test]
fn the_bare_runtime_and_a_profile_naming_its_home_share_the_id() {
    let rig = Rig::new();
    // The bare Claude runtime resolves to `$HOME/.claude`.
    let ambient = rig.home().join(".claude");
    fs::create_dir_all(&ambient).expect("create the ambient home");
    rig.profile("pinned", "claude", "CLAUDE_CONFIG_DIR", &ambient);

    let caps = rig.inspect(&rig.registry(Some(rig.state())));

    assert_eq!(id_of(&caps, "claude"), id_of(&caps, "pinned"));
}

#[test]
fn a_home_that_does_not_exist_is_identified_by_its_normalized_path() {
    let rig = Rig::new();
    let plain = rig.root.join("not-created-yet");
    let dotted = rig
        .root
        .join("elsewhere")
        .join("..")
        .join("not-created-yet");
    rig.profile("plain", "claude", "CLAUDE_CONFIG_DIR", &plain);
    rig.profile("dotted", "claude", "CLAUDE_CONFIG_DIR", &dotted);

    let caps = rig.inspect(&rig.registry(Some(rig.state())));

    assert!(!plain.exists());
    assert_eq!(id_of(&caps, "plain"), id_of(&caps, "dotted"));
}

#[test]
fn the_identifier_reveals_neither_the_path_nor_any_environment_value() {
    let rig = Rig::new();
    let home = rig.dir(DISTINCTIVE_HOME);
    let other = rig.dir(DISTINCTIVE_OTHER);
    rig.profile("work", "claude", "CLAUDE_CONFIG_DIR", &home);
    // A variable the profile sets that is not the home: its value must not
    // leak either.
    fs::write(
        rig.root.join("agents").join("extra.toml"),
        format!(
            "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\nUNRELATED_TOKEN = \"{DISTINCTIVE_OTHER}\"\n",
            other.display()
        ),
    )
    .expect("write a profile with an unrelated variable");

    let caps = rig.inspect(&rig.registry(Some(rig.state())));
    let wire = serde_json::to_string(&caps).expect("serialize the inventory");

    for agent in ["work", "extra"] {
        let id = id_of(&caps, agent);
        assert!(!id.contains(DISTINCTIVE_HOME), "{id}");
        assert!(!id.contains(DISTINCTIVE_OTHER), "{id}");
        assert!(
            !id.contains(&home.display().to_string()) && !id.contains("zq-"),
            "{id}"
        );
    }
    assert!(
        !wire.contains(DISTINCTIVE_HOME),
        "the inventory names a home"
    );
    assert!(
        !wire.contains(DISTINCTIVE_OTHER),
        "the inventory names an environment value"
    );
}

#[test]
fn the_identifier_is_keyed_by_the_host() {
    let rig = Rig::new();
    let home = rig.dir("shared-home");
    rig.profile("work", "claude", "CLAUDE_CONFIG_DIR", &home);
    let other_state = rig.dir("other-state");

    let here = rig.inspect(&rig.registry(Some(rig.state())));
    let there = rig.inspect(&rig.registry(Some(other_state)));

    assert_ne!(
        id_of(&here, "work"),
        id_of(&there, "work"),
        "the same directory on another host is not recognizable from the identifier"
    );
}

#[test]
fn a_key_failure_omits_the_identifier_and_host_inspect_still_succeeds() {
    let rig = Rig::new();
    rig.profile("work", "claude", "CLAUDE_CONFIG_DIR", &rig.dir("work-home"));
    // A host-state path that is a file cannot hold the key record.
    let blocked = rig.root.join("state-is-a-file");
    fs::write(&blocked, b"").expect("create the blocking file");

    for state in [None, Some(blocked)] {
        let caps = rig.inspect(&rig.registry(state));

        assert_eq!(
            caps.supported_agents,
            vec!["shell", "codex", "claude", "hermes", "work"]
        );
        for runtime in &caps.runtimes {
            assert_eq!(runtime.config_home_id, None, "{}", runtime.agent);
        }
    }
}

#[test]
fn runtimes_without_a_declared_home_and_unresolvable_homes_carry_no_id() {
    let rig = Rig::new();
    let caps = rig.inspect(&rig.registry(Some(rig.state())));
    assert_eq!(entry(&caps, "shell").config_home_id, None);
    assert_eq!(entry(&caps, "hermes").config_home_id, None);
    assert!(entry(&caps, "claude").config_home_id.is_some());

    // Neither the declared variable nor HOME: the home cannot be resolved.
    let bare_env = base_environment(
        DEFAULT_ENVIRONMENT_ALLOWLIST,
        &EnvironmentSource::fixed(Vec::new()),
    )
    .expect("an empty base environment");
    let registry = rig.registry(Some(rig.state()));
    let caps = host_capabilities("0.0.0", &registry, &bare_env);
    assert_eq!(entry(&caps, "claude").config_home_id, None);
    assert_eq!(entry(&caps, "codex").config_home_id, None);
    assert!(!caps.runtimes.is_empty(), "the inventory is still reported");
}

#[test]
fn a_relative_home_value_is_not_expanded_into_an_identifier() {
    let rig = Rig::new();
    fs::write(
        rig.root.join("agents").join("tilde.toml"),
        "base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"~/accounts/work\"\n",
    )
    .expect("write a profile");

    let caps = rig.inspect(&rig.registry(Some(rig.state())));

    assert_eq!(entry(&caps, "tilde").config_home_id, None);
}

#[test]
fn only_the_launch_base_environment_steers_the_identifier() {
    let rig = Rig::new();
    let ambient = rig.home().join(".claude");
    fs::create_dir_all(&ambient).expect("create the ambient home");
    let steered = rig.dir("steered-home");
    rig.profile("pinned", "claude", "CLAUDE_CONFIG_DIR", &ambient);
    let registry = rig.registry(Some(rig.state()));

    // CLAUDE_CONFIG_DIR is not in the default allowlist: a launch does not
    // forward it, so the bare runtime resolves the default home whatever the
    // daemon's own environment holds.
    let launch = host_capabilities("0.0.0", &registry, &rig.base(&[]));
    assert_eq!(id_of(&launch, "claude"), id_of(&launch, "pinned"));

    // A base environment that does forward it gives the agent that home.
    let forwarded = host_capabilities(
        "0.0.0",
        &registry,
        &rig.base(&[("CLAUDE_CONFIG_DIR", &steered)]),
    );
    assert_ne!(id_of(&forwarded, "claude"), id_of(&launch, "claude"));
}
