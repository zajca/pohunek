//! Assistant agent selection exercised through the public client protocol.
//!
//! Every scenario prepares an assistant launch with
//! [`prepare_with_options`] against a minimal in-test daemon responder that
//! speaks real [`Request`]/[`Response`] envelopes on a Unix socket: the
//! production `pohunek-client` transport connects and the selection decision is
//! observed on the prepared launch. Two production components meet at the
//! protocol boundary in each scenario.
//!
//! The scenarios cover the previous helper-level behavior matrix:
//! - preference order of ranked agents and the custom-runtime fallback,
//! - explicit agent selection being daemon-authoritative when the host does
//!   not list the runtime,
//! - refused (`supported == Some(false)`) and unavailable runtimes,
//! - shell-backed profiles and shell-only hosts,
//! - historical agent bases failing closed, and grammar-valid third-party
//!   bases launching.
//!
//! The former `launchability_follows_availability_and_version_policy_only`
//! helper matrix maps onto the named scenarios below: every refusal row is an
//! individually diagnosable scenario, and every launchable row ends in a
//! selected runtime.

use std::path::Path;

use pohunek_assistant::launch::{
    prepare_with_options, AssistantPaths, Intent, LaunchParams, PreparedLaunch,
};
use pohunek_assistant::{AssistantError, ConnectionOptions, HostConfig};
use pohunek_client::protocol::method;
use pohunek_client::protocol::{
    self, AgentRuntime, ErrorClass, HostCapabilities, ProtocolError, Request, Response, RuntimeRef,
};
use pohunek_client::{Client, ClientOptions, OriginSource};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

/// One agent runtime entry a fixture host reports.
fn runtime(
    name: &str,
    agent_base: Option<RuntimeRef>,
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
        config_home_id: None,
    }
}

/// A full capability snapshot the fixture host answers `host.inspect` with.
fn capabilities(runtimes: Vec<AgentRuntime>) -> HostCapabilities {
    HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: protocol::PROTOCOL_VERSION,
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

/// The fixture shorthand for the common shape: name plus availability and
/// version-policy columns, with no agent base.
fn caps(runtimes: Vec<(&str, bool)>) -> Vec<AgentRuntime> {
    runtimes
        .into_iter()
        .map(|(name, available)| runtime(name, None, available, None))
        .collect()
}

/// Selection that arrives across the boundary, with the requests the
/// daemon responder saw.
#[derive(Debug)]
struct SelectionExchange {
    result: Result<PreparedLaunch, AssistantError>,
    requests: Vec<Request>,
}

impl SelectionExchange {
    /// The selected agent, failing the scenario otherwise.
    fn selected(self) -> String {
        assert_single_host_inspect(&self.requests);
        self.result
            .expect("the preparation succeeds for a launchable host")
            .selection
            .name
    }

    /// The typed selection refusal, failing the scenario otherwise.
    fn refused(self) -> ProtocolError {
        assert_single_host_inspect(&self.requests);
        let error = self.result.expect_err("selection must refuse this host");
        match error {
            AssistantError::Protocol(source) => source,
            other => panic!("expected the typed selection refusal, got {other:?}"),
        }
    }
}

/// Asserts a refusal carries the canonical `no_capable_agent` code.
fn assert_no_capable_agent(refused: &ProtocolError) {
    assert_eq!(refused.code, "no_capable_agent");
}

/// Asserts exactly one real `host.inspect` crossed the Unix socket boundary.
///
/// A future client transport may negotiate a version with `daemon.health`
/// probes on the same connection; those are tolerated here so an added probe
/// never invalidates the selection scenarios. Any other method means the
/// boundary no longer matches the preparation path and the scenario must be
/// updated on purpose.
fn assert_single_host_inspect(requests: &[Request]) {
    let inspect: Vec<&Request> = requests
        .iter()
        .filter(|request| request.method() == method::HOST_INSPECT)
        .collect();
    let [request] = inspect.as_slice() else {
        panic!("expected exactly one host.inspect across the boundary, got {requests:?}");
    };
    assert_eq!(
        request.params(),
        &Value::Null,
        "host.inspect carries canonical null parameters"
    );
    for request in requests
        .iter()
        .filter(|r| r.method() != method::HOST_INSPECT)
    {
        assert_eq!(
            request.method(),
            method::DAEMON_HEALTH,
            "only daemon.health version probes may accompany the capability exchange, \
             got: {request:?}"
        );
    }
}

#[tokio::test]
async fn auto_agent_selects_the_pohunek_assistant_runtime_first() {
    let exchange = prepare_selection(
        caps(vec![
            ("claude", true),
            ("codex", true),
            ("pohunek-assistant", true),
        ]),
        None,
    )
    .await;

    assert_eq!(exchange.selected(), "pohunek-assistant");
}

#[tokio::test]
async fn auto_agent_uses_hermes_when_codex_and_claude_are_absent() {
    let exchange = prepare_selection(
        vec![runtime(
            "hermes",
            Some(RuntimeRef::hermes()),
            true,
            Some(true),
        )],
        None,
    )
    .await;

    assert_eq!(exchange.selected(), "hermes");
}

#[tokio::test]
async fn auto_agent_falls_back_to_an_available_custom_profile() {
    let exchange = prepare_selection(caps(vec![("custom", true)]), None).await;

    assert_eq!(exchange.selected(), "custom");
}

#[tokio::test]
async fn auto_agent_uses_a_ranked_runtime_ignoring_an_unlisted_one() {
    let exchange = prepare_selection(
        caps(vec![("custom", true), ("codex", true), ("claude", true)]),
        None,
    )
    .await;

    assert_eq!(exchange.selected(), "codex");
}

#[tokio::test]
async fn explicit_agent_without_a_runtime_entry_is_daemon_authoritative() {
    let exchange = prepare_selection(Vec::new(), Some("hermes")).await;

    assert_eq!(exchange.selected(), "hermes");
}

#[tokio::test]
async fn explicit_non_hermes_agents_remain_daemon_authoritative() {
    for requested in ["codex", "claude", "custom"] {
        let exchange = prepare_selection(Vec::new(), Some(requested)).await;

        let selected = exchange.selected();
        assert_eq!(
            selected, requested,
            "explicit {requested} should pass through"
        );
    }
}

#[tokio::test]
async fn explicit_runtime_with_refused_version_policy_is_rejected() {
    for (name, base) in [
        ("hermes", Some(RuntimeRef::hermes())),
        ("pinned-tool", Some(RuntimeRef::codex())),
        ("unlabelled", None),
    ] {
        let exchange = prepare_selection(
            vec![runtime(name, base.clone(), true, Some(false))],
            Some(name),
        )
        .await;

        let refused = exchange.refused();
        assert_eq!(refused.code, "no_capable_agent", "{name}");
    }
}

#[tokio::test]
async fn explicit_runtime_with_confirmed_version_policy_is_selected() {
    for name in ["hermes", "hermes-review", "pinned-tool"] {
        let exchange = prepare_selection(
            vec![runtime(name, Some(RuntimeRef::hermes()), true, Some(true))],
            Some(name),
        )
        .await;

        let selected = exchange.selected();
        assert_eq!(selected, name);
    }
}

#[tokio::test]
async fn auto_agent_ignores_an_unavailable_runtime_whose_policy_accepts_it() {
    let exchange = prepare_selection(
        vec![runtime(
            "dormant-profile",
            Some(RuntimeRef::hermes()),
            false,
            Some(true),
        )],
        None,
    )
    .await;

    assert_no_capable_agent(&exchange.refused());
}

#[tokio::test]
async fn auto_agent_ignores_an_unavailable_runtime_without_a_policy() {
    let exchange =
        prepare_selection(vec![runtime("missing-profile", None, false, None)], None).await;

    assert_no_capable_agent(&exchange.refused());
}

#[tokio::test]
async fn auto_agent_skips_a_refused_runtime_for_a_later_candidate() {
    let exchange = prepare_selection(
        vec![
            runtime("hermes", Some(RuntimeRef::hermes()), true, Some(false)),
            runtime("shell-profile", Some(RuntimeRef::shell()), true, None),
            runtime("legacy-custom", None, true, None),
        ],
        None,
    )
    .await;

    assert_eq!(exchange.selected(), "legacy-custom");
}

#[tokio::test]
async fn auto_agent_rejects_shell_backed_profiles() {
    let exchange = prepare_selection(
        vec![runtime(
            "shell-profile",
            Some(RuntimeRef::shell()),
            true,
            None,
        )],
        None,
    )
    .await;

    assert_no_capable_agent(&exchange.refused());
}

#[tokio::test]
async fn auto_agent_rejects_shell_only_hosts() {
    let exchange = prepare_selection(vec![runtime("shell", None, true, None)], None).await;

    assert_no_capable_agent(&exchange.refused());
}

#[tokio::test]
async fn explicit_historical_agent_base_fails_closed() {
    let exchange = prepare_selection(
        vec![runtime(
            "future-profile",
            Some(RuntimeRef::from_wire("Future Agent")),
            true,
            Some(true),
        )],
        Some("future-profile"),
    )
    .await;

    assert_no_capable_agent(&exchange.refused());
}

#[tokio::test]
async fn grammar_valid_third_party_base_is_launchable() {
    let exchange = prepare_selection(
        vec![runtime(
            "acme-profile",
            Some(RuntimeRef::from_wire("acme")),
            true,
            Some(true),
        )],
        None,
    )
    .await;

    assert_eq!(exchange.selected(), "acme-profile");
}

#[tokio::test]
async fn responder_serves_a_version_negotiation_before_the_capability_exchange() {
    let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
    let socket_path = fixture.path().join("assistant.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind the fixture host socket");
    let capability_json = serde_json::to_value(capabilities(caps(vec![("codex", true)])))
        .expect("serialize the capability snapshot");
    let task = spawn_responder(listener, capability_json);

    // The transport negotiates a `daemon.health` probe first; the responder
    // answers it and only then hands over the capability snapshot.
    let mut client = Client::connect_local_with_options(
        &socket_path,
        ClientOptions::default().with_origin_source(OriginSource::Omitted),
    )
    .await
    .expect("connect the fixture host");
    let negotiated = client.handshake().await.expect("negotiate the protocol");
    assert_eq!(negotiated, protocol::PROTOCOL_VERSION);
    let inspected = client
        .call::<method::HostInspect>(())
        .await
        .expect("the capability exchange succeeds after the probe");
    drop(client);

    assert_eq!(
        inspected.supported_agents,
        vec!["codex".to_owned()],
        "the fixture snapshot crosses the negotiated connection unchanged"
    );
    let requests = pohunek_test_support::wait::guard("capability responder", task)
        .await
        .expect("capability responder task completed");
    assert_single_host_inspect(&requests);
    assert_eq!(
        requests.len(),
        2,
        "the probe and the snapshot are both observed on the wire: {requests:?}"
    );
    assert_eq!(requests[0].method(), method::DAEMON_HEALTH);
    assert_eq!(requests[1].method(), method::HOST_INSPECT);
}

/// The fixture responder's exchange loop: one line in, one reply out, until the
/// client hangs up — the discipline of the daemon's control connection.
fn spawn_responder(listener: UnixListener, capability_json: Value) -> JoinHandle<Vec<Request>> {
    tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("accept the assistant client");
        let mut reader = BufReader::new(stream);
        let mut requests = Vec::new();
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .await
                .expect("read the request line");
            if read == 0 {
                break; // the client hung up; the exchange is over
            }
            let request: Request =
                serde_json::from_str(trim_line_end(&line)).expect("parse the request envelope");
            requests.push(request.clone());
            reply_to(&mut reader, &request, &capability_json).await;
        }
        requests
    })
}

/// Runs one preparation against an in-test capability responder and returns the
/// decision together with the requests that crossed the socket.
///
/// The responder is the daemon's protocol half: it accepts one client
/// connection and answers each request until the client hangs up, so a future
/// transport may negotiate a version or keep the connection open without
/// breaking the scenarios. It returns every request it saw, so each scenario
/// proves its exchange really travels the public request/response envelopes.
///
/// # Panics
///
/// Panics when the fixture root, socket, responder, or wire exchange fail.
async fn prepare_selection(
    runtimes: Vec<AgentRuntime>,
    requested: Option<&str>,
) -> SelectionExchange {
    let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
    let socket_path = fixture.path().join("assistant.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind the fixture host socket");
    let capability_json =
        serde_json::to_value(capabilities(runtimes)).expect("serialize the capability snapshot");
    let task = spawn_responder(listener, capability_json);

    let result = prepare_with_options(
        &local_host(&socket_path),
        &assistant_paths(fixture.path()),
        launch_params(requested),
        no_origin_options(),
    )
    .await;

    // The task returns once the client hung up, which `prepare_with_options`
    // guarantees by dropping its connection when it returns.
    let requests = pohunek_test_support::wait::guard("capability responder", task)
        .await
        .expect("capability responder task completed");
    SelectionExchange { result, requests }
}

/// Replies to one request with its fixture payload or a typed error.
///
/// A version probe gets the canonical health payload, `host.inspect` gets the
/// fixture capability snapshot, and any other method gets a well-formed
/// `method_not_found` error, so the client fails deterministically and the
/// boundary assertion names the method that appeared.
async fn reply_to(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    request: &Request,
    capability_json: &Value,
) {
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
        .expect("the health response is a valid envelope"),
        method::HOST_INSPECT => Response::ok(
            protocol::PROTOCOL_VERSION,
            request.id(),
            capability_json.clone(),
        )
        .expect("the capability snapshot response is a valid envelope"),
        _ => Response::err(
            protocol::PROTOCOL_VERSION,
            request.id(),
            ProtocolError::new(
                ErrorClass::Daemon,
                "method_not_found",
                "the selection fixture only serves daemon.health and host.inspect",
                None,
            ),
        )
        .expect("the method_not_found response is a valid envelope"),
    };
    stream
        .write_all(
            serde_json::to_string(&response)
                .expect("serialize the response")
                .as_bytes(),
        )
        .await
        .expect("write the response line");
    stream
        .write_all(b"\n")
        .await
        .expect("write the response line end");
    stream.flush().await.expect("flush the response");
}

/// A local host config for the fixture responder socket.
fn local_host(socket_path: &Path) -> HostConfig {
    HostConfig::local("test-host", socket_path)
}

/// Explicit assistant paths in the hermetic fixture root.
///
/// Preparation takes no ambient state: every path is passed explicitly, so a
/// launch never writes outside the fixture even when the process environment
/// carries real user directories.
fn assistant_paths(fixture_root: &Path) -> AssistantPaths {
    AssistantPaths {
        runtime_dir: fixture_root.join("runtime"),
        data_dir: fixture_root.join("data"),
        log_dir: fixture_root.join("log"),
        cache_dir: fixture_root.join("cache"),
        config_dir: fixture_root.join("config"),
    }
}

/// Selection parameters that need no runtime binaries.
///
/// `no_snapshot` and `degraded` keep preparation to the capability snapshot
/// exchange plus local materialization, so a scenario exercises the selection
/// boundary without an agent runtime on the host.
fn launch_params(requested: Option<&str>) -> LaunchParams {
    LaunchParams {
        intent: Intent::Help,
        request: None,
        agent: requested.map(str::to_owned),
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        cols: 80,
        rows: 24,
        no_snapshot: true,
        degraded: true,
        auto_started_daemon: false,
    }
}

/// Connection options that never read the process environment.
///
/// `OriginSource::Omitted` keeps a scenario result independent of the
/// `POHUNEK_*` variables of the developer's own session.
fn no_origin_options() -> ConnectionOptions {
    ConnectionOptions {
        origin_source: OriginSource::Omitted,
        ..ConnectionOptions::default()
    }
}

fn trim_line_end(line: &str) -> &str {
    line.trim_end_matches('\n').trim_end_matches('\r')
}
