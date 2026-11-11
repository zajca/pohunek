//! The assistant launch's idempotency key and its one retry, exercised through
//! the public client protocol.
//!
//! Every scenario prepares and starts an assistant launch with
//! [`prepare_with_options`] and [`start_prepared_with_options`] against a
//! scripted daemon that speaks real [`Request`]/[`Response`] envelopes on a Unix
//! socket. The script decides what each `session.new` attempt meets (no reply, a
//! dropped connection, a typed error, a session), and the daemon records the
//! `session.new` parameters it saw, so the scenarios observe the keys on the
//! wire and the number of requests. The scenarios that wait for a deadline run on
//! tokio's paused clock. A remote host is a scripted trusted-TCP daemon that
//! also serves `assistant.materialize`, the way a remote launch prepares.

#![cfg(unix)]

// Rust guideline compliant 2026-11-01

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pohunek_assistant::launch::{
    prepare_with_options, start_prepared_with_options, Intent, LaunchParams, LaunchResult,
    PreparedLaunch,
};
use pohunek_assistant::{AssistantError, ConnectionOptions, HostConfig};
use pohunek_client::protocol::method;
use pohunek_client::protocol::{
    self, ErrorClass, HostCapabilities, ProtocolError, Request, Response,
};
use pohunek_client::{ClientError, OriginSource};
use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};

/// The budget a scenario gives the creation: the explicit request timeout.
// timing-allowed: #1340 the budget split is the policy under test; every scenario runs on virtual time.
const BUDGET: Duration = Duration::from_secs(20);

/// The first attempt's share of [`BUDGET`]: the budget less the five seconds the
/// client keeps back for its retry.
const FIRST_ATTEMPT: Duration = Duration::from_secs(15);

/// A budget under twice the client's five-second reserve, so the reserve is
/// half of it.
// timing-allowed: #1877 the reserve split is the policy under test; the scenario runs on virtual time.
const SHORT_BUDGET: Duration = Duration::from_secs(8);

/// The project a remote launch names, since a remote host needs a target.
const REMOTE_PROJECT: &str = "demo";

/// The session a replay or an in-progress error names.
const SESSION_ID: &str = "s-01M6CBAKG927FAMDNECX60RGAC";

/// What a `session.new` attempt meets.
#[derive(Debug, Clone)]
enum Step {
    /// The request arrives and is never answered.
    Hang,
    /// The connection closes after the request.
    Drop,
    /// A typed daemon error.
    Error(ProtocolError),
    /// The session, flagged as a replay or not.
    Session { replayed: bool },
}

/// A scripted daemon: it answers `daemon.health` and `host.inspect` itself and
/// takes one [`Step`] from its script for each `session.new`.
struct Daemon {
    host: HostConfig,
    /// Whether the daemon is reached as a remote host.
    remote: bool,
    script: Arc<Mutex<VecDeque<Step>>>,
    creates: Arc<Mutex<Vec<Value>>>,
    /// One signal for every `session.new` the script leaves unanswered and for
    /// every delayed `host.inspect` answer.
    hangs: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// Delays the next `host.inspect` answer, as a slow daemon would.
    inspect_delay: Arc<Mutex<Option<Duration>>>,
    /// The `content_hash` `assistant.materialize` reports.
    content_hash: Arc<Mutex<String>>,
    /// The `host.inspect` answer, replaceable to model a daemon swapped for
    /// another release.
    capabilities: Arc<Mutex<Value>>,
    /// Keeps the fixture directory alive for the daemon's lifetime.
    _fixture: Box<dyn std::any::Any>,
}

/// The socket a scripted daemon accepts connections on.
enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Daemon {
    fn start(steps: Vec<Step>, request_id_supported: bool) -> Self {
        Self::start_pinned(steps, request_id_supported, true)
    }

    /// A local daemon that does or does not advertise
    /// `session_new_context_pin_supported`.
    fn start_pinned(steps: Vec<Step>, request_id_supported: bool, pin_supported: bool) -> Self {
        let fixture = pohunek_test_support::tempdir().expect("create the hermetic fixture root");
        let socket_path = fixture.path().join("assistant.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind the fixture host socket");
        Self::serve_on(
            HostConfig::local("test-host", &socket_path),
            Listener::Unix(listener),
            Box::new(fixture),
            steps,
            request_id_supported,
            pin_supported,
        )
    }

    /// A daemon reached over trusted TCP, which a launch treats as remote.
    async fn start_remote(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind the fixture remote host");
        let address: SocketAddr = listener.local_addr().expect("fixture address");
        Self::serve_on(
            HostConfig::tcp("remote-host", address),
            Listener::Tcp(listener),
            Box::new(()),
            steps,
            true,
            true,
        )
    }

    fn serve_on(
        host: HostConfig,
        listener: Listener,
        fixture: Box<dyn std::any::Any>,
        steps: Vec<Step>,
        request_id_supported: bool,
        pin_supported: bool,
    ) -> Self {
        let remote = matches!(listener, Listener::Tcp(_));
        let script = Arc::new(Mutex::new(VecDeque::from(steps)));
        let creates = Arc::new(Mutex::new(Vec::new()));
        let capabilities = Arc::new(Mutex::new(
            serde_json::to_value(capabilities(request_id_supported, pin_supported))
                .expect("serialize the capability snapshot"),
        ));
        let (hang_signal, hangs) = tokio::sync::mpsc::unbounded_channel();
        let inspect_delay = Arc::new(Mutex::new(None));
        let content_hash = Arc::new(Mutex::new(SERVED_DIGEST.to_owned()));
        let (accept_script, accept_creates) = (Arc::clone(&script), Arc::clone(&creates));
        let accept_delay = Arc::clone(&inspect_delay);
        let accept_hash = Arc::clone(&content_hash);
        let accept_capabilities = Arc::clone(&capabilities);
        tokio::spawn(async move {
            loop {
                match &listener {
                    Listener::Unix(listener) => {
                        let (stream, _) = listener.accept().await.expect("accept a connection");
                        tokio::spawn(serve(
                            stream,
                            Arc::clone(&accept_capabilities),
                            Arc::clone(&accept_script),
                            Arc::clone(&accept_creates),
                            hang_signal.clone(),
                            Arc::clone(&accept_delay),
                            Arc::clone(&accept_hash),
                        ));
                    }
                    Listener::Tcp(listener) => {
                        let (stream, _) = listener.accept().await.expect("accept a connection");
                        tokio::spawn(serve(
                            stream,
                            Arc::clone(&accept_capabilities),
                            Arc::clone(&accept_script),
                            Arc::clone(&accept_creates),
                            hang_signal.clone(),
                            Arc::clone(&accept_delay),
                            Arc::clone(&accept_hash),
                        ));
                    }
                }
            }
        });
        Self {
            host,
            remote,
            script,
            creates,
            hangs,
            inspect_delay,
            content_hash,
            capabilities,
            _fixture: fixture,
        }
    }

    fn host(&self) -> HostConfig {
        self.host.clone()
    }

    /// The `client_request_id` of every `session.new` the daemon received.
    fn keys(&self) -> Vec<Option<String>> {
        self.creates
            .lock()
            .expect("creates lock")
            .iter()
            .map(|params| params["client_request_id"].as_str().map(str::to_owned))
            .collect()
    }

    /// The `context_package_digest` of every `session.new` the daemon received.
    fn pins(&self) -> Vec<Option<String>> {
        self.creates
            .lock()
            .expect("creates lock")
            .iter()
            .map(|params| params["context_package_digest"].as_str().map(str::to_owned))
            .collect()
    }

    /// Makes `assistant.materialize` report `content_hash` from now on.
    fn serve_content_hash(&self, content_hash: &str) {
        content_hash.clone_into(&mut self.content_hash.lock().expect("hash lock"));
    }

    /// Makes every later `host.inspect` answer as a release that predates
    /// `session_new_context_pin_supported`, which omits the flag.
    fn answer_as_older_release(&self) {
        self.capabilities
            .lock()
            .expect("capabilities lock")
            .as_object_mut()
            .expect("capabilities are an object")
            .remove("session_new_context_pin_supported");
    }

    /// Makes the next `host.inspect` answer after `delay`.
    fn delay_next_inspect(&self, delay: Duration) {
        *self.inspect_delay.lock().expect("delay lock") = Some(delay);
    }

    /// Waits until a `session.new` is left unanswered.
    async fn next_hang(&mut self) {
        pohunek_test_support::wait::guard("an unanswered session.new", self.hangs.recv())
            .await
            .expect("the daemon is running");
    }

    fn unused_steps(&self) -> usize {
        self.script.lock().expect("script lock").len()
    }
}

async fn serve<S>(
    stream: S,
    capabilities: Arc<Mutex<Value>>,
    script: Arc<Mutex<VecDeque<Step>>>,
    creates: Arc<Mutex<Vec<Value>>>,
    hang_signal: tokio::sync::mpsc::UnboundedSender<()>,
    inspect_delay: Arc<Mutex<Option<Duration>>>,
    content_hash: Arc<Mutex<String>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.expect("read a request") == 0 {
            return;
        }
        let request: Request =
            serde_json::from_str(line.trim_end()).expect("parse the request envelope");
        let response = match request.method() {
            method::DAEMON_HEALTH => ok(
                &request,
                json!({
                    "status": "ok",
                    "daemon_version": "test",
                    "protocol_version": protocol::PROTOCOL_VERSION.get(),
                }),
            ),
            method::HOST_INSPECT => {
                let delay = inspect_delay.lock().expect("delay lock").take();
                if let Some(delay) = delay {
                    // Tells the scenario the answer is parked on the clock.
                    let _ = hang_signal.send(());
                    // timing-allowed: #1340 parked on the paused test clock until the scenario advances it.
                    tokio::time::sleep(delay).await;
                }
                let snapshot = capabilities.lock().expect("capabilities lock").clone();
                ok(&request, snapshot)
            }
            method::ASSISTANT_MATERIALIZE => {
                let hash = content_hash.lock().expect("hash lock").clone();
                ok(&request, materialize_result(&request, &hash))
            }
            method::SESSION_NEW => {
                creates
                    .lock()
                    .expect("creates lock")
                    .push(request.params().clone());
                let step = script
                    .lock()
                    .expect("script lock")
                    .pop_front()
                    .expect("the script has a step for every session.new");
                match step {
                    Step::Hang => {
                        let _ = hang_signal.send(());
                        std::future::pending().await
                    }
                    Step::Drop => return,
                    Step::Error(error) => {
                        Response::err(protocol::PROTOCOL_VERSION, request.id(), error)
                            .expect("a valid error response")
                    }
                    Step::Session { replayed } => ok(&request, session_result(replayed)),
                }
            }
            other => panic!("the launch sent an unexpected request: {other}"),
        };
        let mut bytes = serde_json::to_string(&response).expect("serialize the response");
        bytes.push('\n');
        reader
            .get_mut()
            .write_all(bytes.as_bytes())
            .await
            .expect("write the response");
    }
}

fn ok(request: &Request, result: Value) -> Response {
    Response::ok(protocol::PROTOCOL_VERSION, request.id(), result).expect("a valid response")
}

fn capabilities(request_id_supported: bool, pin_supported: bool) -> HostCapabilities {
    HostCapabilities {
        daemon_version: "test".to_owned(),
        protocol_version: protocol::PROTOCOL_VERSION,
        supported_agents: vec!["pohunek-assistant".to_owned()],
        runtimes: vec![protocol::AgentRuntime {
            agent: "pohunek-assistant".to_owned(),
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
        session_new_request_id_supported: request_id_supported,
        session_new_context_pin_supported: pin_supported,
        package_install_commands_supported: true,
        package_install_index_supported: false,
        package_bind_profile_expected_supported: true,
        package_install_index_context_supported: false,
        package_create_profile_supported: true,
        session_new_branch_source_supported: true,
    }
}

/// The bundle of the repository: a real directory with a readable `index.md`,
/// which is all a local launch checks before it starts the agent.
fn bundle_root() -> PathBuf {
    pohunek_test_support::manifest_dir().join("../../docs/knowledge")
}

/// The `assistant.materialize` result of the fixture daemon.
///
/// The package version carries the request id the daemon received, so a prompt
/// shows which id reached `assistant.materialize`.
fn materialize_result(request: &Request, content_hash: &str) -> Value {
    let root = bundle_root();
    let version = match request.params()["request_id"].as_str() {
        Some(id) => format!("1.0.0+{id}"),
        None => "1.0.0".to_owned(),
    };
    json!({
        "bundle_path": root,
        "snapshot_path": root.join("index.md"),
        "version": version,
        "content_hash": content_hash,
        "concepts": [],
        "prompt_contract": "## Safety\n- Never run destructive commands.\n\n## Navigation\n- Read the index first.\n",
    })
}

fn session_result(replayed: bool) -> Value {
    let mut result = json!({
        "agent": "pohunek-assistant",
        "agent_base": "pohunek-assistant",
        "capabilities": {"fork": false, "resume": false},
        "cols": 80,
        "rows": 24,
        "created_at": "2026-11-02T00:00:00Z",
        "updated_at": "2026-11-02T00:00:00Z",
        "cwd": "/fixture",
        "cwd_source": "launch",
        "external": false,
        "id": SESSION_ID,
        "state": "running",
        "state_source": "process",
        "pid": 4242,
        "applied_input": true,
    });
    if replayed {
        result["replayed"] = json!(true);
    }
    // `session.new` returns the session flattened next to the outcome fields.
    result
}

fn in_progress() -> ProtocolError {
    let mut error =
        ProtocolError::session_request_in_progress(&protocol::SessionId(SESSION_ID.to_owned()));
    error.recover = Some("retry the same request".to_owned());
    error
}

fn launch_params(request_id: Option<&str>) -> LaunchParams {
    LaunchParams {
        intent: Intent::Help,
        request: None,
        agent: None,
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
        request_id: request_id.map(str::to_owned),
    }
}

/// The launch of a daemon: a remote host needs a target.
fn launch_params_for(daemon: &Daemon, request_id: Option<&str>) -> LaunchParams {
    let params = launch_params(request_id);
    if daemon.remote {
        LaunchParams {
            project: Some(REMOTE_PROJECT.to_owned()),
            ..params
        }
    } else {
        params
    }
}

/// Connection options with the scenario's budget and no ambient origin.
fn options() -> ConnectionOptions {
    options_for(BUDGET)
}

fn options_for(budget: Duration) -> ConnectionOptions {
    ConnectionOptions {
        request_timeout: Some(budget),
        origin_source: OriginSource::Omitted,
        ..ConnectionOptions::default()
    }
}

async fn prepare(
    daemon: &Daemon,
    request_id: Option<&str>,
) -> Result<PreparedLaunch, AssistantError> {
    prepare_in(daemon, request_id).await
}

async fn prepare_in(
    daemon: &Daemon,
    request_id: Option<&str>,
) -> Result<PreparedLaunch, AssistantError> {
    prepare_with_options(
        &daemon.host(),
        launch_params_for(daemon, request_id),
        options(),
    )
    .await
}

async fn launch(daemon: &Daemon, request_id: Option<&str>) -> Result<LaunchResult, AssistantError> {
    let prepared = prepare(daemon, request_id)
        .await
        .expect("the launch prepares");
    start_prepared_with_options(&daemon.host(), prepared, options()).await
}

fn protocol_error(error: AssistantError) -> ProtocolError {
    match error {
        AssistantError::Protocol(source)
        | AssistantError::Client(ClientError::Protocol(source)) => source,
        AssistantError::Client(other) => other.to_protocol_error(),
        other => panic!("expected a typed error, got {other:?}"),
    }
}

/// Starts the prepared launch on its own task, so the scenario can move the
/// paused clock while the launch waits.
fn start(
    daemon: &Daemon,
    prepared: PreparedLaunch,
) -> tokio::task::JoinHandle<Result<LaunchResult, AssistantError>> {
    let host = daemon.host();
    tokio::spawn(async move { start_prepared_with_options(&host, prepared, options()).await })
}

async fn finished(
    task: tokio::task::JoinHandle<Result<LaunchResult, AssistantError>>,
) -> Result<LaunchResult, AssistantError> {
    pohunek_test_support::wait::guard("the launch", task)
        .await
        .expect("the launch task completes")
}

#[tokio::test(start_paused = true)]
async fn a_retry_that_meets_the_running_create_ends_the_launch_naming_session_and_key() {
    let mut daemon = Daemon::start(vec![Step::Hang, Step::Error(in_progress())], true);
    // Real I/O runs on this paused clock; time moves only through `advance`.
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    let started = tokio::time::Instant::now();
    let task = start(&daemon, prepared);

    daemon.next_hang().await;
    tokio::time::advance(FIRST_ATTEMPT + TIMER_TICK).await;
    let error = protocol_error(finished(task).await.expect_err("the create runs on"));

    let keys = daemon.keys();
    assert_eq!(keys.len(), 2, "one attempt and one retry: {keys:?}");
    let key = keys[0].clone().expect("the launch sent a generated key");
    assert_eq!(
        keys[1].as_deref(),
        Some(key.as_str()),
        "the retry repeats the key"
    );
    assert_eq!(error.code, "session_request_in_progress");
    assert!(error.msg.contains(SESSION_ID), "{}", error.msg);
    assert!(error.msg.contains(&key), "{}", error.msg);
    let recover = error.recover.expect("a recovery hint");
    assert!(
        recover.contains(&format!("--request-id {key}")),
        "{recover}"
    );
    assert!(
        started.elapsed() <= BUDGET,
        "both attempts fit the budget: {:?}",
        started.elapsed()
    );
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_lost_on_the_transport_is_retried_and_the_launch_succeeds_once() {
    let daemon = Daemon::start(vec![Step::Drop, Step::Session { replayed: true }], true);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");

    let launched = finished(start(&daemon, prepared))
        .await
        .expect("the retry replays");

    assert!(launched.replayed);
    assert_eq!(launched.session.id.0, SESSION_ID);
    assert_eq!(launched.applied_input, Some(true));
    let keys = daemon.keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys[0].is_some() && keys[0] == keys[1], "{keys:?}");
    assert_eq!(daemon.unused_steps(), 0);
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn the_first_attempt_gets_the_budget_less_the_reserve_and_the_retry_what_remains() {
    let mut daemon = Daemon::start(vec![Step::Hang, Step::Hang], true);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    let key = prepared
        .session_params
        .client_request_id
        .clone()
        .expect("a generated key");
    let task = start(&daemon, prepared);

    daemon.next_hang().await;
    // One tick short of the first attempt's deadline: the retry must not exist yet.
    tokio::time::advance(FIRST_ATTEMPT.saturating_sub(TIMER_TICK)).await;
    assert_eq!(daemon.keys().len(), 1, "the first attempt is still waiting");
    tokio::time::advance(2 * TIMER_TICK).await;
    daemon.next_hang().await;
    // The retry was given what remains of the budget, to the tick: the clock now
    // stands one tick past the budget, so a retry that got the full reserve
    // would still be waiting.
    let spent = FIRST_ATTEMPT + TIMER_TICK;
    tokio::time::advance(BUDGET.saturating_sub(spent) + TIMER_TICK).await;
    let error = protocol_error(
        finished(task)
            .await
            .expect_err("neither attempt is answered"),
    );

    assert_eq!(error.code, "request_timeout");
    assert!(error.msg.contains(&key), "{}", error.msg);
    let recover = error.recover.expect("a recovery hint");
    assert!(
        recover.starts_with("the request may have completed"),
        "the timeout keeps its own recovery first: {recover}"
    );
    assert!(
        recover.contains(&format!("--request-id {key}")),
        "{recover}"
    );
    assert_eq!(daemon.keys().len(), 2);
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_budget_under_ten_seconds_keeps_half_of_it_for_the_retry() {
    let mut daemon = Daemon::start(vec![Step::Hang, Step::Hang], true);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    let host = daemon.host();
    let task = tokio::spawn(async move {
        start_prepared_with_options(&host, prepared, options_for(SHORT_BUDGET)).await
    });
    let half = SHORT_BUDGET / 2;

    daemon.next_hang().await;
    // One tick short of half the budget: the retry must not exist yet.
    tokio::time::advance(half.saturating_sub(TIMER_TICK)).await;
    assert_eq!(daemon.keys().len(), 1, "the first attempt is still waiting");
    tokio::time::advance(2 * TIMER_TICK).await;
    daemon.next_hang().await;
    // The retry has the other half, to the tick: one tick past it the launch ends.
    tokio::time::advance(half).await;
    let error = protocol_error(
        finished(task)
            .await
            .expect_err("neither attempt is answered"),
    );

    assert_eq!(error.code, "request_timeout");
    assert_eq!(daemon.keys().len(), 2);
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_slow_capability_check_does_not_stretch_the_first_attempt_past_its_share() {
    let mut daemon = Daemon::start(vec![Step::Hang, Step::Error(in_progress())], true);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    // The dedicated connection's capability check answers one second before
    // the first attempt's deadline; its request then hangs.
    let slow = FIRST_ATTEMPT.saturating_sub(Duration::from_secs(1));
    daemon.delay_next_inspect(slow);
    let task = start(&daemon, prepared);

    // The check is parked on the clock, so the attempt has sent nothing yet.
    daemon.next_hang().await;
    tokio::time::advance(slow).await;
    daemon.next_hang().await;
    // The attempt's share ends one second later, whatever the request waits for.
    tokio::time::advance(Duration::from_secs(1) + TIMER_TICK).await;
    let error = protocol_error(finished(task).await.expect_err("the create runs on"));

    assert_eq!(error.code, "session_request_in_progress");
    assert_eq!(
        daemon.keys().len(),
        2,
        "the retry followed the first attempt"
    );
    inhibitor.release().await;
}

#[tokio::test]
async fn a_typed_daemon_error_is_not_retried() {
    let refusal = ProtocolError::new(
        ErrorClass::Runtime,
        "session_input_not_ready",
        "the agent is not ready for input",
        Some("wait for the agent prompt, then retry".to_owned()),
    );
    let daemon = Daemon::start(vec![Step::Error(refusal)], true);

    let error = protocol_error(launch(&daemon, None).await.expect_err("the daemon refuses"));

    assert_eq!(error.code, "session_input_not_ready");
    assert_eq!(daemon.keys().len(), 1, "exactly one request");
    let key = daemon.keys()[0].clone().expect("a generated key");
    assert!(error.msg.contains(&key), "{}", error.msg);
    let recover = error.recover.expect("a recovery hint");
    assert_eq!(
        recover, "wait for the agent prompt, then retry",
        "a refusal that rolled the create back keeps the daemon's own hint"
    );
}

#[tokio::test(start_paused = true)]
async fn a_daemon_without_the_flag_gets_no_key_and_no_retry() {
    let mut daemon = Daemon::start(vec![Step::Hang], false);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    assert_eq!(prepared.session_params.client_request_id, None);
    let task = start(&daemon, prepared);

    daemon.next_hang().await;
    tokio::time::advance(BUDGET + TIMER_TICK).await;
    let error = finished(task)
        .await
        .expect_err("the unanswered request ends the launch");

    assert!(
        matches!(
            error,
            AssistantError::Client(ClientError::RequestTimeout { .. })
        ),
        "{error:?}"
    );
    assert_eq!(daemon.keys(), vec![None], "one request without a key");
    inhibitor.release().await;
}

#[tokio::test]
async fn a_caller_key_is_sent_as_given_and_a_replay_is_reported() {
    let daemon = Daemon::start(vec![Step::Session { replayed: true }], true);

    let launched = launch(&daemon, Some("my-launch.1"))
        .await
        .expect("the daemon replays the earlier launch");

    assert!(launched.replayed);
    assert_eq!(daemon.keys(), vec![Some("my-launch.1".to_owned())]);
}

#[tokio::test]
async fn a_caller_key_the_daemon_cannot_honor_is_refused_before_any_create() {
    let daemon = Daemon::start(Vec::new(), false);

    let error = protocol_error(
        prepare(&daemon, Some("my-launch.1"))
            .await
            .expect_err("a daemon without the flag would drop the key"),
    );

    assert_eq!(error.code, "session_new_request_id_unsupported");
    assert!(daemon.keys().is_empty());
}

#[tokio::test]
async fn a_malformed_caller_key_is_refused_before_any_create() {
    let daemon = Daemon::start(Vec::new(), true);

    for key in ["", "with space", "a/b", &"k".repeat(129)] {
        let error = protocol_error(
            prepare(&daemon, Some(key))
                .await
                .expect_err("the key breaks the grammar"),
        );
        assert_eq!(error.code, "bad_request", "{key:?}");
    }
    assert!(daemon.keys().is_empty());
}

#[tokio::test]
async fn a_generated_key_is_asst_and_128_random_bits_and_differs_per_launch() {
    let daemon = Daemon::start(Vec::new(), true);

    let mut keys = Vec::new();
    for _ in 0..2 {
        let prepared = prepare(&daemon, None).await.expect("the launch prepares");
        keys.push(
            prepared
                .session_params
                .client_request_id
                .expect("a generated key"),
        );
    }

    for key in &keys {
        let digits = key.strip_prefix("asst-").expect("the asst- prefix");
        assert_eq!(digits.len(), 32, "{key}");
        assert!(
            digits
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{key}"
        );
        assert!(protocol::is_valid_client_request_id(key), "{key}");
    }
    assert_ne!(keys[0], keys[1]);
}

#[tokio::test]
async fn a_launch_repeated_under_a_request_id_renders_the_same_opening_prompt() {
    let daemon = Daemon::start(Vec::new(), true);
    let first = prepare_in(&daemon, Some("resume-me"))
        .await
        .expect("the launch prepares");
    let again = prepare_in(&daemon, Some("resume-me"))
        .await
        .expect("the repeated launch prepares");
    let other = prepare_in(&daemon, Some("resume-me-not"))
        .await
        .expect("another launch prepares");

    assert_eq!(
        first.session_params.input, again.session_params.input,
        "the same request id gives the daemon the same request"
    );
    assert_ne!(
        first.session_params.input, other.session_params.input,
        "another request id reaches assistant.materialize"
    );
}

#[tokio::test]
async fn an_error_returned_after_the_session_was_recorded_offers_the_request_id() {
    let reconnecting = ProtocolError::new(
        ErrorClass::Runtime,
        "session_runtime_reconnecting",
        "the session runtime is reconnecting",
        Some("wait, then inspect the session".to_owned()),
    );
    let daemon = Daemon::start(vec![Step::Error(reconnecting)], true);

    let error = protocol_error(launch(&daemon, None).await.expect_err("the daemon answers"));

    let key = daemon.keys()[0].clone().expect("a generated key");
    let recover = error.recover.expect("a recovery hint");
    assert!(
        recover.starts_with("wait, then inspect the session"),
        "{recover}"
    );
    assert!(
        recover.contains(&format!("--request-id {key}")),
        "{recover}"
    );
}

/// What a daemon says when a request id already names a session created with
/// other parameters.
fn conflict() -> ProtocolError {
    ProtocolError::session_request_conflict(&protocol::SessionId(SESSION_ID.to_owned()))
}

#[tokio::test(start_paused = true)]
async fn a_remote_launch_that_loses_its_reply_is_told_to_rerun_under_its_key() {
    let daemon = Daemon::start_remote(vec![Step::Drop, Step::Drop]).await;
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");

    let error = protocol_error(
        finished(start(&daemon, prepared))
            .await
            .expect_err("neither attempt is answered"),
    );

    let keys = daemon.keys();
    assert_eq!(keys.len(), 2, "one attempt and one retry: {keys:?}");
    let key = keys[0].clone().expect("the launch sent a generated key");
    assert_eq!(keys[1].as_deref(), Some(key.as_str()));
    assert!(error.msg.contains(&key), "{}", error.msg);
    let recover = error.recover.expect("a recovery hint");
    assert!(
        recover.contains(&format!("--request-id {key}")),
        "the daemon derives the snapshot path from the id, so a remote rerun replays: {recover}"
    );
    inhibitor.release().await;
}

#[tokio::test(start_paused = true)]
async fn a_local_conflict_keeps_the_daemons_own_recovery() {
    let daemon = Daemon::start(vec![Step::Error(conflict())], true);
    let inhibitor = AutoAdvanceInhibitor::new();
    let prepared = prepare(&daemon, Some("local-key"))
        .await
        .expect("the launch prepares");

    let error = protocol_error(
        finished(start(&daemon, prepared))
            .await
            .expect_err("the daemon refuses the repeat"),
    );

    assert_eq!(error.code, protocol::SESSION_REQUEST_CONFLICT_CODE);
    assert_eq!(
        error.recover.as_deref(),
        conflict().recover.as_deref(),
        "a local rerun replays, so a conflict there gets no remote explanation"
    );
    inhibitor.release().await;
}

/// The digest `assistant.materialize` reports as `content_hash`.
const SERVED_DIGEST: &str =
    "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

#[tokio::test]
async fn the_launch_pins_the_served_digest_only_on_a_host_that_advertises_the_pin() {
    let pinning = Daemon::start_pinned(vec![Step::Session { replayed: false }], true, true);
    launch(&pinning, None).await.expect("the pinned launch");
    assert_eq!(pinning.pins(), vec![Some(SERVED_DIGEST.to_owned())]);

    // A host without the flag would drop the field and leave the files
    // unpinned, so the launch does not send it.
    let unpinned = Daemon::start_pinned(vec![Step::Session { replayed: false }], true, false);
    launch(&unpinned, None).await.expect("the unpinned launch");
    assert_eq!(unpinned.pins(), vec![None]);
}

#[tokio::test]
async fn a_pin_capable_host_reporting_a_content_hash_that_is_no_digest_fails_the_launch_without_a_create(
) {
    let daemon = Daemon::start_pinned(vec![Step::Session { replayed: false }], true, true);
    daemon.serve_content_hash("sha256:fixture");

    let error = protocol_error(
        prepare(&daemon, None)
            .await
            .expect_err("a launch cannot pin a digest it cannot parse"),
    );

    assert_eq!(error.code, "materialization_failed");
    assert!(daemon.creates.lock().expect("creates lock").is_empty());
}

#[tokio::test]
async fn a_daemon_replaced_by_an_older_release_before_the_create_is_refused_before_session_new() {
    let daemon = Daemon::start_pinned(vec![Step::Session { replayed: false }], true, true);
    let prepared = prepare(&daemon, None).await.expect("the launch prepares");
    daemon.answer_as_older_release();

    let error = protocol_error(
        start_prepared_with_options(&daemon.host(), prepared, options())
            .await
            .expect_err("an older daemon would drop the pin and leave the files unpinned"),
    );

    assert_eq!(error.code, "session_new_context_pin_unsupported");
    assert_eq!(error.class, ErrorClass::Daemon);
    assert!(daemon.creates.lock().expect("creates lock").is_empty());
    assert_eq!(daemon.unused_steps(), 1);
}
