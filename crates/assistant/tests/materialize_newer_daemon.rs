//! Assistant preparation against a remote daemon of the same protocol version
//! that sends a concept type or intent this build does not know.
//!
//! A scripted trusted-TCP host answers the real `pohunek-client` transport, so
//! each outcome is what `pohunek assistant` observes: an unknown concept type
//! reaches the typed bundle-mismatch refusal instead of a decode error, and an
//! unknown intent is dropped without failing the launch.

// Rust guideline compliant 2026-10-29

use std::net::{Ipv4Addr, SocketAddr};

use pohunek_assistant::launch::{
    prepare_with_options, AssistantPaths, Intent, LaunchParams, PreparedLaunch,
};
use pohunek_assistant::{AssistantError, ConnectionOptions, HostConfig};
use pohunek_client::protocol::{
    self, method, ErrorClass, HostCapabilities, ProtocolError, Request, Response,
};
use pohunek_client::OriginSource;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

fn capabilities() -> Value {
    serde_json::to_value(HostCapabilities {
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
        session_idle_stop_supported: true,
        package_install_commands_supported: true,
        session_new_branch_source_supported: true,
    })
    .expect("serialize the capability snapshot")
}

/// The `assistant.materialize` result of a daemon whose bundle holds `concepts`
/// under `content_hash`.
fn materialize_result(content_hash: &str, concepts: &Value) -> Value {
    json!({
        "bundle_path": "/remote/bundle",
        "snapshot_path": "/remote/snapshot.json",
        "version": knowledge::BUNDLE_VERSION,
        "content_hash": content_hash,
        "concepts": concepts,
    })
}

fn concept(kind: &str, id: &str, intents: &Value) -> Value {
    json!({
        "type": kind,
        "id": id,
        "title": id,
        "description": format!("{id} description"),
        "intents": intents,
    })
}

/// Accepts connections until aborted and serves each one.
async fn serve(listener: TcpListener, materialize: Value) {
    loop {
        let (stream, _) = listener.accept().await.expect("accept the client");
        tokio::spawn(serve_connection(stream, materialize.clone()));
    }
}

/// Serves one connection: health and capability probes, and `materialize`.
async fn serve_connection(stream: tokio::net::TcpStream, materialize: Value) {
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
            method::ASSISTANT_MATERIALIZE => Response::ok(
                protocol::PROTOCOL_VERSION,
                request.id(),
                materialize.clone(),
            ),
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

async fn prepare_against(materialize: Value) -> Result<PreparedLaunch, AssistantError> {
    let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind the fixture host");
    let address: SocketAddr = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(serve(listener, materialize));

    let root = fixture.path();
    let result = prepare_with_options(
        &HostConfig::tcp("newer-host", address),
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
            no_snapshot: true,
            degraded: false,
            auto_started_daemon: false,
        },
        ConnectionOptions {
            origin_source: OriginSource::Omitted,
            ..ConnectionOptions::default()
        },
    )
    .await;
    server.abort();
    result
}

#[tokio::test]
async fn a_concept_type_from_a_newer_daemon_reaches_the_typed_bundle_mismatch() {
    let result = prepare_against(materialize_result(
        "a-content-hash-from-a-newer-bundle",
        &json!([concept(
            "a_type_from_a_newer_daemon",
            "newer",
            &json!(["help"])
        )]),
    ))
    .await;

    let Err(AssistantError::Protocol(error)) = result else {
        panic!("expected the typed bundle-mismatch refusal, got {result:?}");
    };
    assert_eq!(error.code, "assistant_bundle_mismatch", "{error:?}");
}

#[tokio::test]
async fn a_concept_intent_from_a_newer_daemon_is_dropped_without_failing_the_launch() {
    let prepared = prepare_against(materialize_result(
        knowledge::bundle_content_hash(),
        &json!([
            concept(
                "guide",
                "kept",
                &json!(["help", "an_intent_from_a_newer_daemon"])
            ),
            concept(
                "guide",
                "dropped",
                &json!(["an_intent_from_a_newer_daemon"])
            ),
        ]),
    ))
    .await
    .expect("the launch prepares past an unknown intent");

    assert!(
        prepared.prompt.contains("- kept —"),
        "a concept that keeps the requested intent is listed: {}",
        prepared.prompt
    );
    assert!(
        !prepared.prompt.contains("- dropped —"),
        "a concept left with no known intent is not listed for it: {}",
        prepared.prompt
    );
}
