//! Assistant launch working directory, exercised through the protocol boundary.
//!
//! A local launch starts in `<data_dir>/assistant` (the launcher passes it as
//! `LaunchParams.workdir`); a remote-style launch passes `None` and keeps the
//! target rule. Each scenario prepares a launch against a minimal in-test
//! daemon responder on a Unix socket and reads the `cwd` back from the
//! prepared `SessionNewParams`, so the assertion names the boundary value.

// Rust guideline compliant 2026-11-19

use std::path::{Path, PathBuf};

use pohunek_assistant::launch::{prepare_with_options, Intent, LaunchParams};
use pohunek_assistant::{ConnectionOptions, HostConfig};
use pohunek_client::protocol::{
    self, AgentRuntime, HostCapabilities, ProtocolError, Request, Response,
};
use pohunek_client::{protocol::method, OriginSource};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

fn capabilities() -> Value {
    serde_json::to_value(HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: protocol::PROTOCOL_VERSION,
        supported_agents: vec!["codex".to_owned()],
        runtimes: vec![AgentRuntime {
            agent: "codex".to_owned(),
            agent_base: None,
            available: true,
            path: None,
            version: None,
            supported: None,
            config_home_id: None,
            display_name: None,
            audience: None,
            options: Vec::new(),
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
        session_new_context_pin_supported: true,
        session_new_fetch_ref_supported: true,
        session_attention_isolation_supported: true,
        session_list_tree_filters_supported: true,
        session_retention_selector_supported: true,
        session_remove_tree_supported: false,
        session_adopt_supported: false,
        package_install_commands_supported: true,
        package_install_index_supported: false,
        package_bind_profile_expected_supported: true,
        package_install_index_context_supported: false,
        package_index_apply_supported: false,
        package_create_profile_supported: true,
        package_create_profile_options_supported: true,
        package_create_preset_supported: true,
        package_remove_profile_supported: true,
        integration_agent_skill_supported: true,
        session_new_branch_source_supported: true,
        session_new_agent_options_supported: true,
        host_hooks: None,
        session_new_skip_repo_hooks_supported: false,
    })
    .expect("serialize the capability snapshot")
}

/// The bundle of the repository: a real directory with a readable `index.md`,
/// which is all a local launch checks before it builds the session params.
fn materialize_result() -> Value {
    let root = pohunek_test_support::manifest_dir().join("../../docs/knowledge");
    json!({
        "bundle_path": root,
        "snapshot_path": root.join("index.md"),
        "version": "1.0.0",
        "content_hash": "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "concepts": [],
        "prompt_contract": "## Safety\n- Never run destructive commands.\n\n## Navigation\n- Read the index first.\n",
    })
}

async fn serve_connection(stream: tokio::net::UnixStream) {
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
            )
            .expect("valid fixture response"),
            method::HOST_INSPECT => {
                Response::ok(protocol::PROTOCOL_VERSION, request.id(), capabilities())
                    .expect("valid fixture response")
            }
            method::ASSISTANT_MATERIALIZE => Response::ok(
                protocol::PROTOCOL_VERSION,
                request.id(),
                materialize_result(),
            )
            .expect("valid fixture response"),
            other => Response::err(
                protocol::PROTOCOL_VERSION,
                request.id(),
                ProtocolError::new(
                    protocol::ErrorClass::Daemon,
                    "method_not_found",
                    format!("the fixture does not serve {other}"),
                    None,
                ),
            )
            .expect("valid fixture response"),
        };
        let line = serde_json::to_string(&response).expect("serialize the response");
        reader
            .get_mut()
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write the response");
    }
}

fn spawn_responder(listener: UnixListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        serve_connection(stream).await;
    })
}

async fn prepared_cwd(workdir: Option<PathBuf>) -> Option<PathBuf> {
    let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
    let socket_path = fixture.path().join("assistant.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind the fixture host socket");
    let task = spawn_responder(listener);
    let prepared = prepare_with_options(
        &HostConfig::local("test-host", Path::new(&socket_path)),
        LaunchParams {
            intent: Intent::Help,
            request: None,
            agent: Some("codex".to_owned()),
            project: None,
            repo: None,
            branch: None,
            base_branch: None,
            branch_source: None,
            on_diverged: None,
            cols: 80,
            rows: 24,
            no_snapshot: true,
            auto_started_daemon: false,
            request_id: None,
            workdir: workdir.clone(),
        },
        ConnectionOptions {
            origin_source: OriginSource::Omitted,
            ..ConnectionOptions::default()
        },
    )
    .await
    .expect("the launch prepares");
    task.abort();
    prepared.session_params.cwd
}

#[tokio::test]
async fn a_local_workdir_reaches_the_session_params() {
    let workdir = PathBuf::from("/data/pohunek/assistant");
    assert_eq!(prepared_cwd(Some(workdir.clone())).await, Some(workdir));
}

#[tokio::test]
async fn a_remote_style_launch_keeps_no_session_cwd() {
    assert_eq!(prepared_cwd(None).await, None);
}
