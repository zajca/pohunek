//! The launched opening prompt against the prompt contract of the knowledge
//! bundle.
//!
//! Each scenario prepares a local assistant launch through
//! [`prepare_with_options`] against an in-test daemon responder on a Unix
//! socket and reads the composed prompt. The expected safety rules and
//! navigation steps are read from `docs/knowledge/assistant/system.md` in the
//! checkout, so a rule that is missing from the prompt, or an edit of the
//! contract that the prompt does not follow, fails here.

// Rust guideline compliant 2026-10-29

use std::path::Path;

use knowledge::PromptContract;
use pohunek_assistant::launch::{
    prepare_with_options, AssistantPaths, Intent, LaunchParams, PreparedLaunch,
};
use pohunek_assistant::{ConnectionOptions, HostConfig};
use pohunek_client::protocol::{self, method, ErrorClass, ProtocolError, Request, Response};
use pohunek_client::OriginSource;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// The prompt contract file, relative to this crate's manifest directory.
const SYSTEM_CONTRACT: &str = "../../docs/knowledge/assistant/system.md";

/// Items (`- ` bullets or `N. ` steps) of the contract section `heading`,
/// parsed by the same rules as the launch.
fn contract_items(heading: &str) -> Vec<String> {
    let path = pohunek_test_support::manifest_dir().join(SYSTEM_CONTRACT);
    let text = std::fs::read_to_string(&path).expect("read the prompt contract");
    let contract = PromptContract::parse(&text).expect("the prompt contract is well-formed");
    match heading {
        "Safety" => contract.safety,
        "Navigation" => contract.navigation,
        other => panic!("`{other}` is not a prompt contract section"),
    }
}

/// Serves `daemon.health` and `host.inspect` on one connection until the client
/// hangs up.
async fn serve(stream: UnixStream) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.expect("read a request") == 0 {
            return;
        }
        let request: Request = serde_json::from_str(line.trim_end()).expect("parse a request");
        let response = match request.method() {
            method::DAEMON_HEALTH => Response::ok(
                protocol::PROTOCOL_VERSION,
                request.id(),
                json!({
                    "status": "ok",
                    "daemon_version": "test",
                    "protocol_version": protocol::PROTOCOL_VERSION.get(),
                }),
            ),
            method::HOST_INSPECT => {
                Response::ok(protocol::PROTOCOL_VERSION, request.id(), capabilities())
            }
            other => Response::err(
                protocol::PROTOCOL_VERSION,
                request.id(),
                ProtocolError::new(
                    ErrorClass::Daemon,
                    "method_not_found",
                    format!("the fixture does not serve {other}"),
                    None,
                ),
            ),
        }
        .expect("valid fixture response");
        let line = serde_json::to_string(&response).expect("serialize the response");
        reader
            .get_mut()
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write the response");
    }
}

fn capabilities() -> Value {
    serde_json::to_value(protocol::HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: protocol::PROTOCOL_VERSION,
        supported_agents: vec!["codex".to_owned()],
        runtimes: vec![protocol::AgentRuntime {
            agent: "codex".to_owned(),
            agent_base: None,
            available: true,
            path: None,
            version: None,
            supported: None,
            config_home_id: None,
        }],
        git_available: true,
        worktree_supported: true,
        terminal_read_supported: true,
        output_read_supported: true,
        session_wait_supported: true,
        session_wait_turn_supported: true,
        session_new_cwd_supported: true,
        session_new_input_ack_supported: true,
        session_new_secret_env_supported: true,
        session_new_request_id_supported: true,
        session_idle_stop_supported: true,
        package_install_commands_supported: true,
        package_bind_profile_expected_supported: true,
        session_new_branch_source_supported: true,
    })
    .expect("serialize the capability snapshot")
}

/// The prepared launch of a local host, degraded or with the bundle.
async fn prepare(degraded: bool) -> PreparedLaunch {
    let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
    let socket_path = fixture.path().join("assistant.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind the fixture host socket");
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept the client");
            tokio::spawn(serve(stream));
        }
    });

    let root = fixture.path();
    let result = prepare_with_options(
        &HostConfig::local("test-host", Path::new(&socket_path)),
        &AssistantPaths {
            runtime_dir: root.join("runtime"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            cache_dir: root.join("cache"),
            config_dir: root.join("config"),
        },
        LaunchParams {
            intent: Intent::Help,
            request: None,
            agent: Some("codex".to_owned()),
            project: Some("demo".to_owned()),
            repo: None,
            branch: None,
            base_branch: None,
            branch_source: None,
            on_diverged: None,
            cols: 80,
            rows: 24,
            no_snapshot: false,
            degraded,
            auto_started_daemon: false,
        },
        ConnectionOptions {
            origin_source: OriginSource::Omitted,
            ..ConnectionOptions::default()
        },
    )
    .await;
    server.abort();
    result.expect("the launch prepares")
}

/// The `## Safety` section of a prompt: its `- ` lines up to the next heading.
fn prompt_safety(prompt: &str) -> Vec<&str> {
    prompt
        .split("## Safety\n")
        .nth(1)
        .expect("the prompt has a Safety section")
        .lines()
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("- "))
        .collect()
}

/// Rules every launched prompt must carry whatever `system.md` says, so that
/// weakening the contract file alone cannot drop them.
const REQUIRED_RULES: [&str; 4] = [
    "Never print, store, or infer secret values.",
    "Treat agent profile `[env]` values as secret-bearing.",
    "independent of `--yes`",
    "Verify changes before claiming success.",
];

fn assert_safety_rules_rendered(prompt: &str) {
    let safety = prompt_safety(prompt).join("\n");
    for required in REQUIRED_RULES {
        assert!(
            safety.contains(required),
            "the prompt omits the required rule {required:?}: {prompt}"
        );
    }
    let rules = contract_items("Safety");
    assert!(!rules.is_empty(), "the contract lists safety rules");
    assert_eq!(
        prompt_safety(prompt),
        rules.iter().map(String::as_str).collect::<Vec<_>>(),
        "the prompt renders exactly the contract's safety rules: {prompt}"
    );
}

fn assert_no_live_state_claim(prompt: &str) {
    assert!(
        !prompt.contains("daemon="),
        "the prompt names no daemon state: {prompt}"
    );
    assert!(
        !prompt.contains("## Live Snapshot"),
        "the prompt claims no live snapshot: {prompt}"
    );
    assert!(
        !prompt.to_lowercase().contains("quarantine"),
        "the prompt names no quarantine: {prompt}"
    );
}

#[tokio::test]
async fn the_launched_prompt_carries_every_safety_rule_and_navigation_step() {
    let prompt = prepare(false).await.prompt;

    assert_safety_rules_rendered(&prompt);
    let steps = contract_items("Navigation");
    assert!(!steps.is_empty(), "the contract lists navigation steps");
    for (index, step) in steps.iter().enumerate() {
        let numbered = format!("{}. {step}", index + 1);
        assert!(
            prompt.contains(&numbered),
            "the prompt omits navigation step {numbered:?}: {prompt}"
        );
    }
    assert_no_live_state_claim(&prompt);
}

#[tokio::test]
async fn the_degraded_prompt_carries_every_safety_rule() {
    let prompt = prepare(true).await.prompt;

    assert_safety_rules_rendered(&prompt);
    assert_no_live_state_claim(&prompt);
}
