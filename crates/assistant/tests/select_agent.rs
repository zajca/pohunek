//! Tests for assistant agent selection.

use pohunek_assistant::launch as assistant;
use pohunek_assistant::runtime_is_launchable;
use pohunek_client::protocol::{AgentKind, AgentRuntime, HostCapabilities, PROTOCOL_VERSION};

fn runtime(
    name: &str,
    agent_base: Option<AgentKind>,
    available: bool,
    supported: Option<bool>,
) -> AgentRuntime {
    AgentRuntime {
        agent: name.to_owned(),
        agent_base,
        available,
        path: None,
        version: None,
        supported,
    }
}

fn capabilities(runtimes: Vec<AgentRuntime>) -> HostCapabilities {
    HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        supported_agents: runtimes
            .iter()
            .map(|runtime| runtime.agent.clone())
            .collect(),
        runtimes,
        git_available: true,
        worktree_supported: true,
        terminal_read_supported: true,
        output_read_supported: true,
        session_wait_supported: true,
    }
}

fn caps(runtimes: Vec<(&str, bool)>) -> HostCapabilities {
    capabilities(
        runtimes
            .into_iter()
            .map(|(name, available)| runtime(name, None, available, None))
            .collect(),
    )
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
fn explicit_custom_agent_wins_when_absent_from_runtime_list() {
    let selected = assistant::select_agent(&caps(vec![("codex", true)]), Some("custom"))
        .expect("explicit agent selected");

    assert_eq!(selected.name, "custom");
}

#[test]
fn auto_agent_uses_hermes_after_codex_and_claude() {
    let selected = assistant::select_agent(
        &capabilities(vec![runtime(
            "hermes",
            Some(AgentKind::Hermes),
            true,
            Some(true),
        )]),
        None,
    )
    .expect("Hermes fallback selected");

    assert_eq!(selected.name, "hermes");
}

#[test]
fn auto_agent_falls_back_to_an_available_custom_profile() {
    let selected = assistant::select_agent(&caps(vec![("custom", true)]), None)
        .expect("custom runtime fallback selected");

    assert_eq!(selected.name, "custom");
}

#[test]
fn explicit_agent_without_a_runtime_entry_is_daemon_authoritative() {
    let selected = assistant::select_agent(&caps(Vec::new()), Some("hermes"))
        .expect("an agent the host does not list is left to the daemon");

    assert_eq!(selected.name, "hermes");
}

#[test]
fn explicit_runtime_with_refused_version_policy_is_rejected() {
    // The refusal comes from the reported policy, not from the runtime's name
    // or compiled base.
    for (name, base) in [
        ("hermes", Some(AgentKind::Hermes)),
        ("pinned-tool", Some(AgentKind::Codex)),
        ("unlabelled", None),
    ] {
        let capabilities = capabilities(vec![runtime(name, base, true, Some(false))]);

        let err = assistant::select_agent(&capabilities, Some(name))
            .expect_err("a runtime whose version policy refuses it must not launch");

        assert_eq!(err.code, "no_capable_agent", "{name}");
    }
}

#[test]
fn explicit_runtime_with_confirmed_version_policy_is_selected() {
    for name in ["hermes", "hermes-review", "pinned-tool"] {
        let capabilities = capabilities(vec![runtime(
            name,
            Some(AgentKind::Hermes),
            true,
            Some(true),
        )]);

        let selected = assistant::select_agent(&capabilities, Some(name))
            .expect("a runtime whose version policy accepts it is selected explicitly");

        assert_eq!(selected.name, name);
    }
}

#[test]
fn launchability_follows_availability_and_version_policy_only() {
    let cases = [
        // (available, supported, launchable)
        (false, Some(true), false),
        (false, None, false),
        (true, Some(false), false),
        (true, Some(true), true),
        (true, None, true),
    ];
    for (available, supported, expected) in cases {
        for base in [None, Some(AgentKind::Hermes), Some(AgentKind::Claude)] {
            let candidate = runtime("any-name", base.clone(), available, supported);
            assert_eq!(
                runtime_is_launchable(&candidate),
                expected,
                "available={available} supported={supported:?} base={base:?}"
            );
        }
    }
}

#[test]
fn auto_agent_skips_a_refused_runtime_for_a_later_candidate() {
    let capabilities = capabilities(vec![
        runtime("hermes", Some(AgentKind::Hermes), true, Some(false)),
        runtime("shell-profile", Some(AgentKind::Shell), true, None),
        runtime("legacy-custom", None, true, None),
    ]);

    let selected = assistant::select_agent(&capabilities, None)
        .expect("available legacy custom runtime selected after the refused runtime");

    assert_eq!(selected.name, "legacy-custom");
}

#[test]
fn auto_agent_rejects_shell_backed_profiles() {
    let capabilities = capabilities(vec![runtime(
        "shell-profile",
        Some(AgentKind::Shell),
        true,
        None,
    )]);

    let err = assistant::select_agent(&capabilities, None)
        .expect_err("a renamed shell-backed profile cannot host the assistant");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn explicit_unknown_agent_base_fails_closed() {
    let capabilities = capabilities(vec![runtime(
        "future-profile",
        Some(AgentKind::Unknown("future".to_owned())),
        true,
        Some(true),
    )]);

    let err = assistant::select_agent(&capabilities, Some("future-profile"))
        .expect_err("unknown compiled agent base must fail closed");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn runtime_launchability_preserves_available_legacy_custom_profiles() {
    let legacy = runtime("legacy-custom", None, true, None);
    let missing = runtime("legacy-missing", None, false, None);

    assert!(runtime_is_launchable(&legacy));
    assert!(!runtime_is_launchable(&missing));
}

#[test]
fn explicit_non_hermes_agents_remain_daemon_authoritative() {
    let capabilities = caps(Vec::new());

    for requested in ["codex", "claude", "custom"] {
        let selected = assistant::select_agent(&capabilities, Some(requested))
            .unwrap_or_else(|err| panic!("explicit {requested} should pass through: {err}"));

        assert_eq!(selected.name, requested);
    }
}

#[test]
fn auto_agent_rejects_shell_only_hosts() {
    let err = assistant::select_agent(&caps(vec![("shell", true)]), None)
        .expect_err("shell is not a capable assistant runtime");

    assert_eq!(err.code, "no_capable_agent");
}
