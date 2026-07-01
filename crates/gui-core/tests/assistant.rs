//! Tests for the shared GUI/CLI assistant launch core.

use pohunek_client::protocol::{AgentRuntime, HostCapabilities, PROTOCOL_VERSION};
use pohunek_gui_core::assistant;

fn caps(runtimes: Vec<(&str, bool)>) -> HostCapabilities {
    HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents: runtimes
            .iter()
            .map(|(name, _available)| (*name).to_owned())
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
fn auto_agent_prefers_pohunek_assistant_then_codex() {
    let selected = assistant::select_agent(
        &caps(vec![
            ("claude", true),
            ("codex", true),
            ("pohunek-assistant", true),
        ]),
        None,
    )
    .expect("agent selected");

    assert_eq!(selected.name, "pohunek-assistant");
}

#[test]
fn explicit_agent_wins_even_when_not_in_runtime_list() {
    let selected = assistant::select_agent(&caps(vec![("codex", true)]), Some("custom"))
        .expect("explicit agent selected");

    assert_eq!(selected.name, "custom");
}

#[test]
fn auto_agent_rejects_shell_only_hosts() {
    let err = assistant::select_agent(&caps(vec![("shell", true)]), None)
        .expect_err("shell is not a capable assistant runtime");

    assert_eq!(err.code, "no_capable_agent");
}
