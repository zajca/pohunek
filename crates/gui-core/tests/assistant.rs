//! Tests for the shared GUI/CLI assistant launch core.

use pohunek_client::protocol::{AgentKind, AgentRuntime, HostCapabilities, PROTOCOL_VERSION};
use pohunek_gui_core::{assistant, runtime_is_launchable};

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
fn explicit_bare_hermes_requires_a_runtime_entry() {
    let err = assistant::select_agent(&caps(Vec::new()), Some("hermes"))
        .expect_err("missing Hermes runtime cannot launch the assistant");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn explicit_bare_hermes_requires_an_available_runtime() {
    let err = assistant::select_agent(&caps(vec![("hermes", false)]), Some("hermes"))
        .expect_err("unavailable Hermes runtime cannot launch the assistant");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn explicit_supported_hermes_profile_is_selected() {
    let capabilities = capabilities(vec![runtime(
        "hermes-review",
        Some(AgentKind::Hermes),
        true,
        Some(true),
    )]);

    let selected = assistant::select_agent(&capabilities, Some("hermes-review"))
        .expect("available supported Hermes profile selected explicitly");

    assert_eq!(selected.name, "hermes-review");
}

#[test]
fn explicit_unsupported_hermes_is_rejected() {
    let capabilities = capabilities(vec![runtime(
        "hermes",
        Some(AgentKind::Hermes),
        true,
        Some(false),
    )]);

    let err = assistant::select_agent(&capabilities, Some("hermes"))
        .expect_err("unsupported Hermes must not launch an assistant");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn explicit_bare_hermes_requires_positive_support_confirmation() {
    let capabilities = capabilities(vec![runtime("hermes", None, true, None)]);

    let err = assistant::select_agent(&capabilities, Some("hermes"))
        .expect_err("Hermes without version-policy confirmation must not launch");

    assert_eq!(err.code, "no_capable_agent");
}

#[test]
fn explicit_hermes_profile_requires_positive_support_confirmation() {
    for (available, supported) in [(false, Some(true)), (true, None), (true, Some(false))] {
        let capabilities = capabilities(vec![runtime(
            "hermes-review",
            Some(AgentKind::Hermes),
            available,
            supported,
        )]);

        let err = assistant::select_agent(&capabilities, Some("hermes-review"))
            .expect_err("unconfirmed or unsupported Hermes profile must not launch");

        assert_eq!(err.code, "no_capable_agent");
    }
}

#[test]
fn auto_agent_skips_unconfirmed_hermes_for_a_legacy_custom_runtime() {
    let capabilities = capabilities(vec![
        runtime("hermes", Some(AgentKind::Hermes), true, None),
        runtime("shell-profile", Some(AgentKind::Shell), true, None),
        runtime("legacy-custom", None, true, None),
    ]);

    let selected = assistant::select_agent(&capabilities, None)
        .expect("available legacy custom runtime selected after unconfirmed Hermes");

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
