//! Provides a bounded deterministic local Hermes provider mock.
//!
//! The mock exposes only the exact `OpenAI`-compatible endpoints required by
//! each scenario on IPv4 loopback. Capture scenarios deny up to the pinned
//! three-attempt Copilot startup budget while accepting scenario-specific
//! traffic. Regular scenarios accept one chat request; local discovery requires
//! five ordered detection requests and then one chat request. Proxy denials
//! complete before TLS, so the mock never receives a credential, and all
//! protocol failures are redacted.

// Rust guideline compliant 2026-08-05

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

pub(crate) const MODEL_ID: &str = "pohunek-compat-v1";
const CHAT_PATH: &str = "/v1/chat/completions";
const COPILOT_EXCHANGE_AUTHORITY: &str = "api.github.com:443";
const COPILOT_API_AUTHORITY: &str = "api.githubcopilot.com:443";
/// The pinned Hermes release retries transient Copilot token exchange failures three times.
const COPILOT_EXCHANGE_ATTEMPTS: usize = 3;
/// The pinned provider scan retries transient Copilot API fallback failures three times.
const COPILOT_API_ATTEMPTS: usize = 3;
const DISCOVERY_PATHS: [&str; 5] = [
    "/api/v1/models",
    "/api/tags",
    "/v1/props",
    "/props",
    "/version",
];
/// Loopback polling keeps shutdown bounded without leaving a blocked listener thread.
const ACCEPT_POLL: Duration = Duration::from_millis(10);
/// The pinned local `OpenAI SDK` must finish local request I/O promptly.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
/// Allows one timed-out request plus bounded control-loop scheduling overhead.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(3);
/// Hermes requests include system instructions and tool schemas, but remain bounded.
const MAX_HEADER_BYTES: usize = 32 * 1024;
/// This accepts the pinned request envelope while preventing unbounded allocation.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Enough headers for the pinned `OpenAI SDK` without accepting arbitrary header growth.
const MAX_HEADERS: usize = 64;
/// Scenario identifiers are short repository-owned labels safe for diagnostics.
const MAX_SCENARIO_NAME_BYTES: usize = 64;

/// Describes one bounded request expected from a Hermes PTY scenario.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Scenario {
    name: String,
    expected: Expected,
    discovery: DiscoveryPolicy,
    copilot: CopilotPolicy,
}

impl Scenario {
    /// Expects no model request for one Hermes scenario.
    pub(crate) fn no_request(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            expected: Expected::None,
            discovery: DiscoveryPolicy::Forbidden,
            copilot: CopilotPolicy::Forbidden,
        }
    }

    /// Expects one prompt and returns deterministic assistant text.
    #[cfg(test)]
    pub(crate) fn text(
        name: impl Into<String>,
        prompt: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            expected: Expected::Reply {
                prompt: prompt.into(),
                reply: Reply::Text(text.into()),
            },
            discovery: DiscoveryPolicy::Forbidden,
            copilot: CopilotPolicy::Forbidden,
        }
    }

    /// Requires the pinned local-discovery waterfall before one text response.
    ///
    /// The sequence is five ordered detection `GET` requests and then one chat
    /// `POST`. No step is optional or repeatable.
    pub(crate) fn text_with_local_discovery(
        name: impl Into<String>,
        prompt: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            expected: Expected::Reply {
                prompt: prompt.into(),
                reply: Reply::Text(text.into()),
            },
            discovery: DiscoveryPolicy::Required,
            copilot: CopilotPolicy::Forbidden,
        }
    }

    /// Expects one prompt and returns a deterministic terminal tool call.
    #[cfg(test)]
    pub(crate) fn terminal(
        name: impl Into<String>,
        prompt: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            expected: Expected::Reply {
                prompt: prompt.into(),
                reply: Reply::Terminal(command.into()),
            },
            discovery: DiscoveryPolicy::Forbidden,
            copilot: CopilotPolicy::Forbidden,
        }
    }

    /// Requires the pinned local-discovery waterfall before one terminal call.
    ///
    /// The sequence is five ordered detection `GET` requests and then one chat
    /// `POST`. No step is optional or repeatable.
    pub(crate) fn terminal_with_local_discovery(
        name: impl Into<String>,
        prompt: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            expected: Expected::Reply {
                prompt: prompt.into(),
                reply: Reply::Terminal(command.into()),
            },
            discovery: DiscoveryPolicy::Required,
            copilot: CopilotPolicy::Forbidden,
        }
    }

    /// Guards the optional pinned Copilot startup probes before scenario traffic.
    pub(crate) fn with_copilot_probe_denials(mut self) -> Self {
        self.copilot = CopilotPolicy::Guarded;
        self
    }
}

fn is_safe_scenario_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SCENARIO_NAME_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Expected {
    None,
    Reply { prompt: String, reply: Reply },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryPolicy {
    Forbidden,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopilotPolicy {
    Forbidden,
    Guarded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Reply {
    Text(String),
    Terminal(String),
}

#[derive(Debug)]
struct Active {
    scenario: Scenario,
    copilot_exchange_connects: usize,
    copilot_api_connects: usize,
    discovery_requests: usize,
    requests: usize,
}

#[derive(Debug, Default)]
struct State {
    active: Option<Active>,
    failure: Option<Failure>,
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    UnexpectedRequest,
    ForbiddenRequest,
    UnexpectedDiscoveryRequest,
    DuplicateDiscoveryRequest,
    ExtraDiscoveryRequest,
    DiscoveryOutOfOrder,
    MissingDiscoveryRequest,
    ChatBeforeDiscovery,
    UnexpectedCopilotProbe,
    ExtraCopilotProbe,
    InvalidCopilotProbe,
    GetBody,
    MissingRequest,
    ExtraRequest,
    InvalidMethod,
    InvalidPath,
    ProxyConnect,
    ExternalProxyRequest,
    InvalidModel,
    InvalidPrompt,
    MissingTerminalTool,
    InvalidScenario,
    MalformedHttp,
    IncompleteRequest,
    MissingContentLength,
    InvalidContentLength,
    TransferEncoding,
    InvalidJson,
    TrailingBytes,
    RequestTooLarge,
    ResponseEncoding,
    Connection,
    StatePoisoned,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnexpectedRequest => "received a request without an active scenario",
            Self::ForbiddenRequest => "received a model request for a no-request scenario",
            Self::UnexpectedDiscoveryRequest => {
                "received a local discovery request for a scenario that does not allow it"
            }
            Self::DuplicateDiscoveryRequest => "received a duplicate local discovery request",
            Self::ExtraDiscoveryRequest => "received too many local discovery requests",
            Self::DiscoveryOutOfOrder => "received local discovery requests out of order",
            Self::MissingDiscoveryRequest => {
                "did not receive the complete local discovery request sequence"
            }
            Self::ChatBeforeDiscovery => {
                "received a chat completion before completing local discovery"
            }
            Self::UnexpectedCopilotProbe => {
                "received a Copilot startup probe for a scenario that does not allow it"
            }
            Self::ExtraCopilotProbe => "received too many Copilot startup probes",
            Self::InvalidCopilotProbe => {
                "received a Copilot startup probe with an invalid proxy envelope"
            }
            Self::GetBody => "received a local GET request with a body",
            Self::MissingRequest => "did not receive the required model request",
            Self::ExtraRequest => "received more requests than the scenario allows",
            Self::InvalidMethod => "received an unsupported HTTP method",
            Self::InvalidPath => "received an unsupported HTTP path",
            Self::ProxyConnect => "blocked an outbound HTTPS proxy CONNECT request",
            Self::ExternalProxyRequest => "blocked an outbound absolute-form proxy request",
            Self::InvalidModel => "received an unexpected model identifier",
            Self::InvalidPrompt => "received an unexpected prompt",
            Self::MissingTerminalTool => "did not receive the required terminal tool",
            Self::InvalidScenario => "received an invalid scenario definition",
            Self::MalformedHttp => "received malformed HTTP headers",
            Self::IncompleteRequest => "received an incomplete HTTP request",
            Self::MissingContentLength => "received a request without Content-Length",
            Self::InvalidContentLength => "received an invalid Content-Length",
            Self::TransferEncoding => "received unsupported Transfer-Encoding",
            Self::InvalidJson => "received an invalid JSON body",
            Self::TrailingBytes => "received trailing or pipelined request bytes",
            Self::RequestTooLarge => "received an oversized HTTP request",
            Self::ResponseEncoding => "failed to encode a deterministic response",
            Self::Connection => "encountered a local mock connection error",
            Self::StatePoisoned => "encountered a poisoned mock synchronization lock",
        };
        f.write_str(message)
    }
}

/// Reports a redacted local mock failure.
#[derive(Debug)]
pub(crate) struct MockError(Failure, Option<String>);

impl MockError {
    fn new(failure: Failure) -> Self {
        Self(failure, None)
    }

    fn with_scenario(mut self, scenario: Option<String>) -> Self {
        self.1 = scenario;
        self
    }
}

impl fmt::Display for MockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(scenario) = &self.1 {
            write!(
                f,
                "Hermes compatibility mock scenario `{scenario}` {}",
                self.0
            )
        } else {
            write!(f, "Hermes compatibility mock {}", self.0)
        }
    }
}

impl std::error::Error for MockError {}

/// Hosts deterministic OpenAI-compatible responses on IPv4 loopback.
#[derive(Debug)]
pub(crate) struct Mock {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    lifecycle: Mutex<()>,
    controls: Sender<Control>,
    #[cfg(test)]
    test_events: Mutex<Receiver<TestEvent>>,
    #[cfg(test)]
    test_hooks: Hooks,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum Control {
    Barrier(Sender<()>),
    Resume,
}

enum ControlPoll {
    Pending,
    Disconnected,
}

#[derive(Clone, Debug, Default)]
struct Hooks {
    #[cfg(test)]
    events: Option<Sender<TestEvent>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestEvent {
    BarrierConnectionAccepted,
    BarrierDrainStarted,
    BarrierQueued,
    BodyReadPending,
}

#[cfg(test)]
impl Hooks {
    fn test_channel() -> (Self, Receiver<TestEvent>) {
        let (events, receiver) = mpsc::channel();
        (
            Self {
                events: Some(events),
            },
            receiver,
        )
    }

    fn barrier_drain_started(&self) {
        self.emit(TestEvent::BarrierDrainStarted);
    }

    fn barrier_connection_accepted(&self) {
        self.emit(TestEvent::BarrierConnectionAccepted);
    }

    fn barrier_queued(&self) {
        self.emit(TestEvent::BarrierQueued);
    }

    fn body_read_pending(&self) {
        self.emit(TestEvent::BodyReadPending);
    }

    fn emit(&self, event: TestEvent) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }
}

impl Mock {
    /// Starts the local mock server.
    ///
    /// # Errors
    /// Returns an error when the IPv4 loopback listener cannot be created.
    pub(crate) fn start() -> Result<Self, MockError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_error| MockError::new(Failure::Connection))?;
        let address = listener
            .local_addr()
            .map_err(|_error| MockError::new(Failure::Connection))?;
        listener
            .set_nonblocking(true)
            .map_err(|_error| MockError::new(Failure::Connection))?;

        let state = Arc::new(Mutex::new(State::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let (controls, control_receiver) = mpsc::channel();
        #[cfg(test)]
        let (hooks, test_events) = Hooks::test_channel();
        #[cfg(test)]
        let test_hooks = hooks.clone();
        #[cfg(not(test))]
        let hooks = Hooks::default();
        let thread_state = Arc::clone(&state);
        let thread_stopping = Arc::clone(&stopping);
        let thread = thread::spawn(move || {
            serve(
                &listener,
                &thread_state,
                &thread_stopping,
                &control_receiver,
                &hooks,
            );
        });

        Ok(Self {
            address,
            state,
            lifecycle: Mutex::new(()),
            controls,
            #[cfg(test)]
            test_events: Mutex::new(test_events),
            #[cfg(test)]
            test_hooks,
            stopping,
            thread: Some(thread),
        })
    }

    /// Returns the only endpoint accepted by this mock.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// Returns the loopback endpoint used as the harness-owned fail-closed proxy.
    pub(crate) fn proxy_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Arms one exact scenario before launching its Hermes PTY.
    ///
    /// # Errors
    /// Returns an error when the prior scenario was not verified or state was poisoned.
    pub(crate) fn begin(&self, scenario: Scenario) -> Result<(), MockError> {
        if !is_safe_scenario_name(&scenario.name) {
            return Err(MockError::new(Failure::InvalidScenario));
        }
        let _lifecycle = self.lock_lifecycle()?;
        {
            let state = lock_state(&self.state)?;
            if let Some(active) = &state.active {
                return Err(MockError::new(Failure::ExtraRequest)
                    .with_scenario(Some(active.scenario.name.clone())));
            }
        }
        self.barrier()?;
        let mut state = lock_state(&self.state)?;
        if let Some(failure) = state.failure.take() {
            return Err(MockError::new(failure));
        }
        let scenario_name = scenario.name.clone();
        state.active = Some(Active {
            scenario,
            copilot_exchange_connects: 0,
            copilot_api_connects: 0,
            discovery_requests: 0,
            requests: 0,
        });
        drop(state);
        self.controls.send(Control::Resume).map_err(|_error| {
            MockError::new(Failure::Connection).with_scenario(Some(scenario_name))
        })?;
        Ok(())
    }

    /// Verifies that the armed scenario made exactly its permitted request count.
    ///
    /// # Errors
    /// Returns a redacted error when Hermes made an unexpected or missing request.
    pub(crate) fn finish(&self) -> Result<(), MockError> {
        let _lifecycle = self.lock_lifecycle()?;
        let scenario = lock_state(&self.state)?
            .active
            .as_ref()
            .map(|active| active.scenario.name.clone());
        self.barrier()
            .map_err(|error| error.with_scenario(scenario.clone()))?;
        let mut state = lock_state(&self.state)?;
        if let Some(failure) = state.failure.take() {
            let scenario = state
                .active
                .as_ref()
                .map(|active| active.scenario.name.clone());
            state.active = None;
            return Err(MockError::new(failure).with_scenario(scenario));
        }
        let Some(active) = state.active.take() else {
            return Err(MockError::new(Failure::UnexpectedRequest));
        };
        let scenario = Some(active.scenario.name.clone());
        let expected_discovery_requests = if active.scenario.discovery == DiscoveryPolicy::Required
        {
            DISCOVERY_PATHS.len()
        } else {
            0
        };
        if active.discovery_requests != expected_discovery_requests {
            return Err(MockError::new(Failure::MissingDiscoveryRequest).with_scenario(scenario));
        }
        let expected_requests = usize::from(!matches!(active.scenario.expected, Expected::None));
        if active.requests != expected_requests {
            return Err(MockError::new(Failure::MissingRequest).with_scenario(scenario));
        }
        Ok(())
    }

    fn barrier(&self) -> Result<(), MockError> {
        let (acknowledge, acknowledged) = mpsc::channel();
        self.controls
            .send(Control::Barrier(acknowledge))
            .map_err(|_error| MockError::new(Failure::Connection))?;
        #[cfg(test)]
        self.test_hooks.barrier_queued();
        acknowledged
            .recv_timeout(BARRIER_TIMEOUT)
            .map_err(|_error| MockError::new(Failure::Connection))
    }

    fn lock_lifecycle(&self) -> Result<MutexGuard<'_, ()>, MockError> {
        self.lifecycle
            .lock()
            .map_err(|_poison| MockError::new(Failure::StatePoisoned))
    }

    #[cfg(test)]
    fn wait_for_test_event(&self, expected: TestEvent) {
        let events = self.test_events.lock().expect("lock test events");
        loop {
            let event = events
                .recv_timeout(BARRIER_TIMEOUT)
                .expect("receive test server event");
            if event == expected {
                return;
            }
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    listener: &TcpListener,
    state: &Arc<Mutex<State>>,
    stopping: &Arc<AtomicBool>,
    controls: &Receiver<Control>,
    hooks: &Hooks,
) {
    let mut paused = false;
    while !stopping.load(Ordering::Acquire) {
        let control = if paused {
            controls
                .recv_timeout(ACCEPT_POLL)
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => ControlPoll::Pending,
                    RecvTimeoutError::Disconnected => ControlPoll::Disconnected,
                })
        } else {
            controls.try_recv().map_err(|error| match error {
                TryRecvError::Empty => ControlPoll::Pending,
                TryRecvError::Disconnected => ControlPoll::Disconnected,
            })
        };
        match control {
            Ok(control) => {
                match control {
                    Control::Barrier(acknowledge) => {
                        drain_connections(listener, state, stopping, hooks);
                        paused = true;
                        let _ = acknowledge.send(());
                    }
                    Control::Resume => paused = false,
                }
                continue;
            }
            Err(ControlPoll::Pending) => {}
            Err(ControlPoll::Disconnected) => break,
        }
        if paused {
            continue;
        }
        match listener.accept() {
            Ok((stream, address)) => process_connection(stream, address, state, stopping, hooks),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(_error) => {
                record_failure(state, Failure::Connection);
                break;
            }
        }
    }
}

fn drain_connections(
    listener: &TcpListener,
    state: &Arc<Mutex<State>>,
    stopping: &Arc<AtomicBool>,
    hooks: &Hooks,
) {
    #[cfg(test)]
    hooks.barrier_drain_started();
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, address)) => {
                #[cfg(test)]
                hooks.barrier_connection_accepted();
                process_connection(stream, address, state, stopping, hooks);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_error) => {
                record_failure(state, Failure::Connection);
                break;
            }
        }
    }
}

fn process_connection(
    mut stream: TcpStream,
    address: SocketAddr,
    state: &Arc<Mutex<State>>,
    stopping: &Arc<AtomicBool>,
    hooks: &Hooks,
) {
    if stopping.load(Ordering::Acquire) {
        return;
    }
    if !address.ip().is_loopback() {
        record_failure(state, Failure::UnexpectedRequest);
        return;
    }
    if let Err(error) = handle(&mut stream, state, hooks) {
        record_failure(state, error.0);
    }
}

fn handle(
    stream: &mut TcpStream,
    state: &Arc<Mutex<State>>,
    hooks: &Hooks,
) -> Result<(), MockError> {
    stream
        .set_read_timeout(Some(CONNECTION_TIMEOUT))
        .map_err(|_error| MockError::new(Failure::Connection))?;
    stream
        .set_write_timeout(Some(CONNECTION_TIMEOUT))
        .map_err(|_error| MockError::new(Failure::Connection))?;
    let request = read_request(stream, hooks)?;
    let response = accept_request(state, &request)?;
    write_response(stream, response)
}

enum Request {
    Chat { stream: bool, body: Value },
    CopilotConnect(CopilotEndpoint),
    Discovery(usize),
}

enum Response {
    Chat { streaming: bool, reply: Reply },
    ConnectDenied,
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endpoint {
    Chat,
    CopilotConnect(CopilotEndpoint),
    Discovery(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopilotEndpoint {
    Exchange,
    Api,
}

struct ParsedHeaders {
    endpoint: Endpoint,
    content_length: Option<usize>,
}

fn read_request(stream: &mut TcpStream, hooks: &Hooks) -> Result<Request, MockError> {
    #[cfg(not(test))]
    let _ = hooks;
    let mut bytes = Vec::with_capacity(MAX_HEADER_BYTES);
    let header_end = loop {
        if let Some(index) = header_end(&bytes) {
            break index;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(MockError::new(Failure::RequestTooLarge));
        }
        let mut buffer = [0_u8; 4096];
        let count = read_chunk(stream, &mut buffer)?;
        if count == 0 {
            return Err(MockError::new(Failure::IncompleteRequest));
        }
        bytes.extend_from_slice(&buffer[..count]);
    };

    let headers = parse_headers(&bytes[..header_end + 4])?;
    let body_start = header_end + 4;
    if let Endpoint::CopilotConnect(endpoint) = headers.endpoint {
        if bytes.len() != body_start || has_available_trailing_byte(stream)? {
            return Err(MockError::new(Failure::TrailingBytes));
        }
        return Ok(Request::CopilotConnect(endpoint));
    }
    if let Endpoint::Discovery(step) = headers.endpoint {
        if headers.content_length.is_some_and(|length| length != 0) {
            return Err(MockError::new(Failure::GetBody));
        }
        if bytes.len() != body_start || has_available_trailing_byte(stream)? {
            return Err(MockError::new(Failure::TrailingBytes));
        }
        return Ok(Request::Discovery(step));
    }
    let content_length = headers
        .content_length
        .ok_or_else(|| MockError::new(Failure::MissingContentLength))?;
    if content_length > MAX_BODY_BYTES {
        return Err(MockError::new(Failure::RequestTooLarge));
    }
    let body_end = body_start
        .checked_add(content_length)
        .ok_or_else(|| MockError::new(Failure::RequestTooLarge))?;
    if bytes.len() > body_end {
        return Err(MockError::new(Failure::TrailingBytes));
    }
    while bytes.len().saturating_sub(body_start) < content_length {
        if bytes.len() >= body_start + MAX_BODY_BYTES {
            return Err(MockError::new(Failure::RequestTooLarge));
        }
        let remaining = content_length - bytes.len().saturating_sub(body_start);
        let mut buffer = vec![0_u8; remaining.min(4096)];
        #[cfg(test)]
        hooks.body_read_pending();
        let count = read_chunk(stream, &mut buffer)?;
        if count == 0 {
            return Err(MockError::new(Failure::IncompleteRequest));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != body_end {
        return Err(MockError::new(Failure::IncompleteRequest));
    }
    if has_available_trailing_byte(stream)? {
        return Err(MockError::new(Failure::TrailingBytes));
    }
    let body: Value = serde_json::from_slice(&bytes[body_start..body_end])
        .map_err(|_error| MockError::new(Failure::InvalidJson))?;
    Ok(Request::Chat {
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        body,
    })
}

fn read_chunk(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<usize, MockError> {
    stream.read(buffer).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::UnexpectedEof => {
            MockError::new(Failure::IncompleteRequest)
        }
        _ => MockError::new(Failure::Connection),
    })
}

fn has_available_trailing_byte(stream: &TcpStream) -> Result<bool, MockError> {
    stream
        .set_nonblocking(true)
        .map_err(|_error| MockError::new(Failure::Connection))?;
    let mut byte = [0_u8; 1];
    let peek = stream.peek(&mut byte);
    stream
        .set_nonblocking(false)
        .map_err(|_error| MockError::new(Failure::Connection))?;
    match peek {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(_error) => Err(MockError::new(Failure::Connection)),
    }
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_headers(bytes: &[u8]) -> Result<ParsedHeaders, MockError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    let httparse::Status::Complete(_used) = request
        .parse(bytes)
        .map_err(|_error| MockError::new(Failure::MalformedHttp))?
    else {
        return Err(MockError::new(Failure::MalformedHttp));
    };
    let method = request
        .method
        .ok_or_else(|| MockError::new(Failure::MalformedHttp))?;
    let path = request
        .path
        .ok_or_else(|| MockError::new(Failure::MalformedHttp))?;
    if method == "CONNECT" {
        let endpoint = match path {
            COPILOT_EXCHANGE_AUTHORITY => CopilotEndpoint::Exchange,
            COPILOT_API_AUTHORITY => CopilotEndpoint::Api,
            _ => return Err(MockError::new(Failure::ProxyConnect)),
        };
        if request.version != Some(1) {
            return Err(MockError::new(Failure::InvalidCopilotProbe));
        }
        if request.headers.len() != 1
            || !request.headers[0].name.eq_ignore_ascii_case("host")
            || request.headers[0].value != path.as_bytes()
        {
            return Err(MockError::new(Failure::InvalidCopilotProbe));
        }
        return Ok(ParsedHeaders {
            endpoint: Endpoint::CopilotConnect(endpoint),
            content_length: None,
        });
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        return Err(MockError::new(Failure::ExternalProxyRequest));
    }
    let endpoint = match method {
        "POST" if path == CHAT_PATH => Endpoint::Chat,
        "GET" => DISCOVERY_PATHS
            .iter()
            .position(|expected| *expected == path)
            .map(Endpoint::Discovery)
            .ok_or_else(|| MockError::new(Failure::InvalidPath))?,
        "POST" => return Err(MockError::new(Failure::InvalidPath)),
        _ => return Err(MockError::new(Failure::InvalidMethod)),
    };
    let mut content_length = None;
    for header in request.headers {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(MockError::new(Failure::TransferEncoding));
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(MockError::new(Failure::InvalidContentLength));
            }
            let value = std::str::from_utf8(header.value)
                .map_err(|_error| MockError::new(Failure::InvalidContentLength))?;
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_error| MockError::new(Failure::InvalidContentLength))?,
            );
        }
    }
    Ok(ParsedHeaders {
        endpoint,
        content_length,
    })
}

fn accept_request(state: &Arc<Mutex<State>>, request: &Request) -> Result<Response, MockError> {
    match request {
        Request::Discovery(step) => accept_discovery_request(state, *step),
        Request::CopilotConnect(endpoint) => accept_copilot_connect(state, *endpoint),
        Request::Chat { stream, body } => accept_chat_request(state, *stream, body),
    }
}

fn accept_copilot_connect(
    state: &Arc<Mutex<State>>,
    endpoint: CopilotEndpoint,
) -> Result<Response, MockError> {
    let mut state = lock_state(state)?;
    let Some(active) = state.active.as_mut() else {
        state.failure.get_or_insert(Failure::UnexpectedRequest);
        return Err(MockError::new(Failure::UnexpectedRequest));
    };
    if active.scenario.copilot != CopilotPolicy::Guarded {
        state.failure.get_or_insert(Failure::UnexpectedCopilotProbe);
        return Err(MockError::new(Failure::UnexpectedCopilotProbe));
    }
    let (connects, limit) = match endpoint {
        CopilotEndpoint::Exchange => (
            &mut active.copilot_exchange_connects,
            COPILOT_EXCHANGE_ATTEMPTS,
        ),
        CopilotEndpoint::Api => (&mut active.copilot_api_connects, COPILOT_API_ATTEMPTS),
    };
    if *connects >= limit {
        state.failure.get_or_insert(Failure::ExtraCopilotProbe);
        return Err(MockError::new(Failure::ExtraCopilotProbe));
    }
    *connects += 1;
    Ok(Response::ConnectDenied)
}

fn accept_discovery_request(
    state: &Arc<Mutex<State>>,
    requested_step: usize,
) -> Result<Response, MockError> {
    let mut state = lock_state(state)?;
    let Some(active) = state.active.as_mut() else {
        state.failure.get_or_insert(Failure::UnexpectedRequest);
        return Err(MockError::new(Failure::UnexpectedRequest));
    };
    if active.scenario.discovery != DiscoveryPolicy::Required {
        state
            .failure
            .get_or_insert(Failure::UnexpectedDiscoveryRequest);
        return Err(MockError::new(Failure::UnexpectedDiscoveryRequest));
    }
    if active.discovery_requests >= DISCOVERY_PATHS.len() {
        state.failure.get_or_insert(Failure::ExtraDiscoveryRequest);
        return Err(MockError::new(Failure::ExtraDiscoveryRequest));
    }
    if requested_step < active.discovery_requests {
        state
            .failure
            .get_or_insert(Failure::DuplicateDiscoveryRequest);
        return Err(MockError::new(Failure::DuplicateDiscoveryRequest));
    }
    if requested_step != active.discovery_requests {
        state.failure.get_or_insert(Failure::DiscoveryOutOfOrder);
        return Err(MockError::new(Failure::DiscoveryOutOfOrder));
    }
    active.discovery_requests += 1;
    Ok(Response::NotFound)
}

fn accept_chat_request(
    state: &Arc<Mutex<State>>,
    streaming: bool,
    body: &Value,
) -> Result<Response, MockError> {
    let mut state = lock_state(state)?;
    let Some(active) = state.active.as_mut() else {
        state.failure.get_or_insert(Failure::UnexpectedRequest);
        return Err(MockError::new(Failure::UnexpectedRequest));
    };
    if active.requests != 0 {
        state.failure.get_or_insert(Failure::ExtraRequest);
        return Err(MockError::new(Failure::ExtraRequest));
    }
    if active.scenario.discovery == DiscoveryPolicy::Required
        && active.discovery_requests != DISCOVERY_PATHS.len()
    {
        state.failure.get_or_insert(Failure::ChatBeforeDiscovery);
        return Err(MockError::new(Failure::ChatBeforeDiscovery));
    }
    let Expected::Reply { prompt, reply } = &active.scenario.expected else {
        state.failure.get_or_insert(Failure::ForbiddenRequest);
        return Err(MockError::new(Failure::ForbiddenRequest));
    };
    if body.get("model").and_then(Value::as_str) != Some(MODEL_ID) {
        state.failure.get_or_insert(Failure::InvalidModel);
        return Err(MockError::new(Failure::InvalidModel));
    }
    if last_user_prompt(body).as_deref() != Some(prompt) {
        state.failure.get_or_insert(Failure::InvalidPrompt);
        return Err(MockError::new(Failure::InvalidPrompt));
    }
    if matches!(reply, Reply::Terminal(_)) && !has_terminal_tool(body) {
        state.failure.get_or_insert(Failure::MissingTerminalTool);
        return Err(MockError::new(Failure::MissingTerminalTool));
    }
    active.requests += 1;
    Ok(Response::Chat {
        streaming,
        reply: reply.clone(),
    })
}

fn last_user_prompt(body: &Value) -> Option<String> {
    body.get("messages")?
        .as_array()?
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))?
        .get("content")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn has_terminal_tool(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.get("type").and_then(Value::as_str) == Some("function")
                    && tool
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        == Some("terminal")
            })
        })
}

fn write_response(stream: &mut TcpStream, response: Response) -> Result<(), MockError> {
    match response {
        Response::Chat { streaming, reply } if streaming => write_stream(stream, &reply),
        Response::Chat { reply, .. } => write_json(stream, &reply),
        Response::ConnectDenied => write_http(stream, "403 Forbidden", "text/plain", ""),
        Response::NotFound => write_http(stream, "404 Not Found", "application/json", ""),
    }
}

fn write_stream(stream: &mut TcpStream, reply: &Reply) -> Result<(), MockError> {
    let mut body = String::new();
    match reply {
        Reply::Text(text) => {
            push_event(
                &mut body,
                &json!({
                    "id": "chatcmpl-pohunek-compat",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}]
                }),
            )?;
            push_event(
                &mut body,
                &json!({
                    "id": "chatcmpl-pohunek-compat",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                }),
            )?;
        }
        Reply::Terminal(command) => {
            push_event(
                &mut body,
                &json!({
                    "id": "chatcmpl-pohunek-compat",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]
                }),
            )?;
            push_event(
                &mut body,
                &json!({
                    "id": "chatcmpl-pohunek-compat",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_pohunek_compat", "type": "function", "function": {"name": "terminal", "arguments": json!({"command": command}).to_string()}}]}, "finish_reason": null}]
                }),
            )?;
            push_event(
                &mut body,
                &json!({
                    "id": "chatcmpl-pohunek-compat",
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": MODEL_ID,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
                }),
            )?;
        }
    }
    body.push_str("data: [DONE]\n\n");
    write_http(stream, "200 OK", "text/event-stream", &body)
}

fn push_event(body: &mut String, value: &Value) -> Result<(), MockError> {
    let encoded =
        serde_json::to_string(value).map_err(|_error| MockError::new(Failure::ResponseEncoding))?;
    body.push_str("data: ");
    body.push_str(&encoded);
    body.push_str("\n\n");
    Ok(())
}

fn write_json(stream: &mut TcpStream, reply: &Reply) -> Result<(), MockError> {
    let message = match reply {
        Reply::Text(text) => json!({"role": "assistant", "content": text}),
        Reply::Terminal(command) => json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_pohunek_compat",
                "type": "function",
                "function": {"name": "terminal", "arguments": json!({"command": command}).to_string()}
            }]
        }),
    };
    let finish_reason = match reply {
        Reply::Text(_) => "stop",
        Reply::Terminal(_) => "tool_calls",
    };
    let body = serde_json::to_string(&json!({
        "id": "chatcmpl-pohunek-compat",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL_ID,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}]
    }))
    .map_err(|_error| MockError::new(Failure::ResponseEncoding))?;
    write_http(stream, "200 OK", "application/json", &body)
}

fn write_http(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), MockError> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )
    .map_err(|_error| MockError::new(Failure::Connection))?;
    stream
        .flush()
        .map_err(|_error| MockError::new(Failure::Connection))
}

fn lock_state(state: &Arc<Mutex<State>>) -> Result<std::sync::MutexGuard<'_, State>, MockError> {
    state
        .lock()
        .map_err(|_poison| MockError::new(Failure::StatePoisoned))
}

fn record_failure(state: &Arc<Mutex<State>>, failure: Failure) {
    if let Ok(mut state) = state.lock() {
        state.failure.get_or_insert(failure);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::thread;

    use serde_json::{json, Value};

    use super::{
        Mock, Scenario, TestEvent, CHAT_PATH, COPILOT_API_ATTEMPTS, COPILOT_API_AUTHORITY,
        COPILOT_EXCHANGE_ATTEMPTS, COPILOT_EXCHANGE_AUTHORITY, DISCOVERY_PATHS, MODEL_ID,
    };

    fn request(mock: &Mock, body: &Value) -> String {
        raw_request(mock, &encoded_request(body))
    }

    fn raw_request(mock: &Mock, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(mock.address).expect("connect mock");
        stream.write_all(request).expect("write request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
    }

    fn response_body(response: &str) -> &str {
        response
            .split_once("\r\n\r\n")
            .map(|(_headers, body)| body)
            .expect("HTTP response body")
    }

    fn encoded_request(body: &Value) -> Vec<u8> {
        let body = serde_json::to_string(body).expect("serialize request");
        format!(
            "POST {CHAT_PATH} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn encoded_get_request(path: &str, with_zero_length: bool) -> Vec<u8> {
        let length = if with_zero_length {
            "Content-Length: 0\r\n"
        } else {
            ""
        };
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{length}\r\n").into_bytes()
    }

    fn encoded_copilot_connect(authority: &str, extra_header: &str) -> Vec<u8> {
        format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{extra_header}\r\n")
            .into_bytes()
    }

    fn complete_copilot_probes(mock: &Mock) {
        for _attempt in 0..COPILOT_EXCHANGE_ATTEMPTS {
            let response = raw_request(
                mock,
                &encoded_copilot_connect(COPILOT_EXCHANGE_AUTHORITY, ""),
            );
            assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
            assert!(response_body(&response).is_empty());
        }
        for _attempt in 0..COPILOT_API_ATTEMPTS {
            let response = raw_request(mock, &encoded_copilot_connect(COPILOT_API_AUTHORITY, ""));
            assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
            assert!(response_body(&response).is_empty());
        }
    }

    fn finish_raw_failure(name: &str, prompt: &str, request: &[u8]) -> String {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::text(name, prompt, "response"))
            .expect("arm scenario");
        let mut stream = TcpStream::connect(mock.address).expect("connect mock");
        stream.write_all(request).expect("write raw request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish raw request");
        mock.finish()
            .expect_err("raw request must fail")
            .to_string()
    }

    fn text_request(prompt: &str, streaming: bool) -> Value {
        json!({
            "model": MODEL_ID,
            "stream": streaming,
            "messages": [{"role": "user", "content": prompt}],
            "tools": []
        })
    }

    fn terminal_request(prompt: &str) -> Value {
        json!({
            "model": MODEL_ID,
            "stream": true,
            "messages": [{"role": "user", "content": prompt}],
            "tools": [{"type": "function", "function": {"name": "terminal", "parameters": {"type": "object"}}}]
        })
    }

    #[test]
    fn text_scenario_serves_sse_and_verifies_one_request() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::text("short", "prompt", "response"))
            .expect("arm scenario");

        let response = request(&mock, &text_request("prompt", true));

        assert!(response.contains("Content-Type: text/event-stream"));
        assert!(response.contains("response"));
        assert!(response.contains("data: [DONE]"));
        mock.finish().expect("verify scenario");
    }

    #[test]
    fn terminal_scenario_requires_terminal_tool_and_returns_tool_call() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::terminal("working", "prompt", "sleep 8"))
            .expect("arm scenario");

        let response = request(&mock, &terminal_request("prompt"));

        assert!(response.contains("tool_calls"));
        assert!(response.contains("sleep 8"));
        mock.finish().expect("verify scenario");
    }

    #[test]
    fn missing_terminal_tool_fails_closed_without_prompt_diagnostic() {
        let mock = Mock::start().expect("start mock");
        let secret_like_prompt = "do not disclose this compatibility prompt";
        mock.begin(Scenario::terminal("working", secret_like_prompt, "sleep 8"))
            .expect("arm scenario");

        let response = request(&mock, &text_request(secret_like_prompt, true));
        let error = mock.finish().expect_err("missing terminal tool fails");

        assert!(response.is_empty() || !response.contains(secret_like_prompt));
        assert!(error.to_string().contains("terminal tool"));
        assert!(!error.to_string().contains(secret_like_prompt));
    }

    #[test]
    fn zero_request_scenario_rejects_any_model_call() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::no_request("prompt-ready"))
            .expect("arm scenario");

        let _response = request(&mock, &text_request("prompt", false));
        let error = mock.finish().expect_err("unexpected request fails");

        assert!(error.to_string().contains("scenario `prompt-ready`"));
        assert!(error.to_string().contains("no-request scenario"));
    }

    #[test]
    fn extra_request_fails_closed() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::text("short", "prompt", "response"))
            .expect("arm scenario");

        let _first = request(&mock, &text_request("prompt", false));
        let _second = request(&mock, &text_request("prompt", false));
        let error = mock.finish().expect_err("second request fails");

        assert!(error.to_string().contains("more requests"));
    }

    #[test]
    fn local_discovery_then_chat_is_exact() {
        for (name, with_zero_length) in [
            ("discovery-absent-length", false),
            ("discovery-zero-length", true),
        ] {
            let mock = Mock::start().expect("start mock");
            mock.begin(Scenario::text_with_local_discovery(
                name, "prompt", "response",
            ))
            .expect("arm discovery scenario");

            for path in DISCOVERY_PATHS {
                let detection = raw_request(&mock, &encoded_get_request(path, with_zero_length));
                assert!(detection.starts_with("HTTP/1.1 404 Not Found\r\n"));
                assert!(response_body(&detection).is_empty());
                assert!(!detection.contains(MODEL_ID));
            }

            let chat = request(&mock, &text_request("prompt", false));

            assert!(chat.contains("response"));
            mock.finish().expect("verify discovery scenario");
        }
    }

    #[test]
    fn copilot_probes_are_denied_before_exact_scenario_traffic() {
        let mock = Mock::start().expect("start mock");
        mock.begin(
            Scenario::text_with_local_discovery("copilot-probes", "prompt", "response")
                .with_copilot_probe_denials(),
        )
        .expect("arm Copilot-probed scenario");

        complete_copilot_probes(&mock);
        for path in DISCOVERY_PATHS {
            let response = raw_request(&mock, &encoded_get_request(path, false));
            assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
        }
        let response = request(&mock, &text_request("prompt", false));

        assert!(response.contains("response"));
        mock.finish().expect("verify Copilot-probed scenario");
    }

    #[test]
    fn copilot_probe_budget_fails_closed() {
        let extra = Mock::start().expect("start extra mock");
        extra
            .begin(Scenario::no_request("extra-copilot").with_copilot_probe_denials())
            .expect("arm extra scenario");
        complete_copilot_probes(&extra);
        let _response = raw_request(
            &extra,
            &encoded_copilot_connect(COPILOT_EXCHANGE_AUTHORITY, ""),
        );
        let extra_error = extra
            .finish()
            .expect_err("extra Copilot probe must fail")
            .to_string();

        let interleaved = Mock::start().expect("start interleaved mock");
        interleaved
            .begin(
                Scenario::text("interleaved-copilot", "private-prompt", "response")
                    .with_copilot_probe_denials(),
            )
            .expect("arm interleaved scenario");
        let _response = raw_request(
            &interleaved,
            &encoded_copilot_connect(COPILOT_EXCHANGE_AUTHORITY, ""),
        );
        let response = request(&interleaved, &text_request("private-prompt", false));
        let _response = raw_request(
            &interleaved,
            &encoded_copilot_connect(COPILOT_API_AUTHORITY, ""),
        );

        assert!(extra_error.contains("too many Copilot startup probes"));
        assert!(response.contains("response"));
        interleaved
            .finish()
            .expect("bounded background probes may interleave with scenario traffic");
    }

    #[test]
    fn copilot_probe_contract_rejects_unexpected_or_sensitive_headers() {
        let unexpected = Mock::start().expect("start unexpected mock");
        unexpected
            .begin(Scenario::no_request("unexpected-copilot"))
            .expect("arm regular scenario");
        let _response = raw_request(
            &unexpected,
            &encoded_copilot_connect(COPILOT_EXCHANGE_AUTHORITY, ""),
        );
        let unexpected_error = unexpected
            .finish()
            .expect_err("regular scenario must reject Copilot probe")
            .to_string();

        let secret = "private-proxy-credential";
        let header = format!("Proxy-Authorization: Bearer {secret}\r\n");
        let sensitive = finish_raw_failure(
            "sensitive-connect",
            "private-prompt",
            &encoded_copilot_connect(COPILOT_EXCHANGE_AUTHORITY, &header),
        );

        assert!(unexpected_error.contains("does not allow it"));
        assert!(sensitive.contains("invalid proxy envelope"));
        assert!(!sensitive.contains(secret));
        assert!(!sensitive.contains("private-prompt"));
    }

    #[test]
    fn discovery_gets_are_rejected_by_regular_scenarios() {
        for (name, scenario) in [
            (
                "regular-text",
                Scenario::text("regular-text", "private-prompt", "response"),
            ),
            (
                "regular-terminal",
                Scenario::terminal("regular-terminal", "private-prompt", "sleep 1"),
            ),
        ] {
            let mock = Mock::start().expect("start mock");
            mock.begin(scenario).expect("arm regular scenario");

            let response = raw_request(&mock, &encoded_get_request(DISCOVERY_PATHS[0], false));
            let error = mock
                .finish()
                .expect_err("regular scenario must reject discovery")
                .to_string();

            assert!(response.is_empty());
            assert!(error.contains(&format!("scenario `{name}`")));
            assert!(error.contains("does not allow it"));
            assert!(!error.contains("private-prompt"));
        }
    }

    #[test]
    fn discovery_order_duplicate_extra_and_missing_are_rejected() {
        let duplicate = Mock::start().expect("start duplicate mock");
        duplicate
            .begin(Scenario::text_with_local_discovery(
                "duplicate-discovery",
                "prompt",
                "response",
            ))
            .expect("arm duplicate scenario");
        let _first = raw_request(&duplicate, &encoded_get_request(DISCOVERY_PATHS[0], false));
        let _duplicate = raw_request(&duplicate, &encoded_get_request(DISCOVERY_PATHS[0], false));
        let duplicate_error = duplicate
            .finish()
            .expect_err("duplicate discovery must fail")
            .to_string();

        let out_of_order = Mock::start().expect("start order mock");
        out_of_order
            .begin(Scenario::text_with_local_discovery(
                "discovery-order",
                "prompt",
                "response",
            ))
            .expect("arm order scenario");
        let _wrong = raw_request(
            &out_of_order,
            &encoded_get_request(DISCOVERY_PATHS[1], false),
        );
        let order_error = out_of_order
            .finish()
            .expect_err("out-of-order discovery must fail")
            .to_string();

        let extra = Mock::start().expect("start extra mock");
        extra
            .begin(Scenario::text_with_local_discovery(
                "extra-discovery",
                "prompt",
                "response",
            ))
            .expect("arm extra scenario");
        for path in DISCOVERY_PATHS {
            let _response = raw_request(&extra, &encoded_get_request(path, false));
        }
        let _extra = raw_request(&extra, &encoded_get_request(DISCOVERY_PATHS[0], false));
        let extra_error = extra
            .finish()
            .expect_err("extra discovery must fail")
            .to_string();

        let missing = Mock::start().expect("start missing mock");
        missing
            .begin(Scenario::text_with_local_discovery(
                "missing-discovery",
                "prompt",
                "response",
            ))
            .expect("arm missing scenario");
        let missing_error = missing
            .finish()
            .expect_err("missing discovery must fail")
            .to_string();

        assert!(duplicate_error.contains("scenario `duplicate-discovery`"));
        assert!(duplicate_error.contains("duplicate local discovery request"));
        assert!(order_error.contains("scenario `discovery-order`"));
        assert!(order_error.contains("out of order"));
        assert!(extra_error.contains("scenario `extra-discovery`"));
        assert!(extra_error.contains("too many local discovery requests"));
        assert!(missing_error.contains("scenario `missing-discovery`"));
        assert!(missing_error.contains("complete local discovery request sequence"));
    }

    #[test]
    fn chat_requires_complete_discovery_and_models_path_is_invalid() {
        let early_chat = Mock::start().expect("start early chat mock");
        early_chat
            .begin(Scenario::text_with_local_discovery(
                "early-chat",
                "prompt",
                "response",
            ))
            .expect("arm early chat scenario");
        let _chat = request(&early_chat, &text_request("prompt", false));
        let early_chat_error = early_chat
            .finish()
            .expect_err("chat before discovery must fail")
            .to_string();

        let invalid_models = Mock::start().expect("start invalid-models mock");
        invalid_models
            .begin(Scenario::text_with_local_discovery(
                "invalid-models-path",
                "private-prompt",
                "response",
            ))
            .expect("arm invalid-models scenario");
        let response = raw_request(
            &invalid_models,
            b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let invalid_models_error = invalid_models
            .finish()
            .expect_err("models path must fail")
            .to_string();

        assert!(early_chat_error.contains("chat completion before completing local discovery"));
        assert!(response.is_empty());
        assert!(invalid_models_error.contains("scenario `invalid-models-path`"));
        assert!(invalid_models_error.contains("unsupported HTTP path"));
        assert!(!invalid_models_error.contains("private-prompt"));
        assert!(!invalid_models_error.contains("/v1/models"));
    }

    #[test]
    fn local_discovery_supports_terminal_scenarios() {
        let mock = Mock::start().expect("start terminal discovery mock");
        mock.begin(Scenario::terminal_with_local_discovery(
            "terminal-discovery",
            "prompt",
            "sleep 8",
        ))
        .expect("arm terminal discovery scenario");
        for path in DISCOVERY_PATHS {
            let detection = raw_request(&mock, &encoded_get_request(path, false));
            assert!(detection.starts_with("HTTP/1.1 404 Not Found\r\n"));
        }
        let response = request(&mock, &terminal_request("prompt"));

        assert!(response.contains("tool_calls"));
        assert!(response.contains("sleep 8"));
        mock.finish().expect("verify terminal discovery scenario");
    }

    #[test]
    fn invalid_local_get_requests_are_rejected_and_redacted() {
        let cases = [
            (
                "discovery-body",
                format!(
                    "GET {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 19\r\n\r\nprivate-body-marker",
                    DISCOVERY_PATHS[0]
                ),
                "with a body",
                "private-body-marker",
            ),
            (
                "discovery-transfer",
                format!(
                    "GET {} HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
                    DISCOVERY_PATHS[0]
                ),
                "unsupported Transfer-Encoding",
                "chunked",
            ),
            (
                "discovery-trailing",
                format!(
                    "GET {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\nprivate-trailing-marker",
                    DISCOVERY_PATHS[0]
                ),
                "trailing or pipelined",
                "private-trailing-marker",
            ),
            (
                "discovery-path",
                "GET /private-model-path HTTP/1.1\r\nHost: localhost\r\n\r\n".to_owned(),
                "unsupported HTTP path",
                "/private-model-path",
            ),
        ];

        for (name, request, category, private_marker) in cases {
            let mock = Mock::start().expect("start mock");
            mock.begin(Scenario::text_with_local_discovery(
                name,
                "private-discovery-prompt",
                "response",
            ))
            .expect("arm discovery scenario");
            let response = raw_request(&mock, request.as_bytes());
            let error = mock
                .finish()
                .expect_err("invalid discovery request must fail")
                .to_string();

            assert!(response.is_empty());
            assert!(error.contains(&format!("scenario `{name}`")));
            assert!(error.contains(category));
            assert!(!error.contains(private_marker));
            assert!(!error.contains("private-discovery-prompt"));
        }
    }

    #[test]
    fn finish_waits_for_an_open_incomplete_request() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::no_request("prompt-ready"))
            .expect("arm scenario");
        let mut stream = TcpStream::connect(mock.address).expect("connect mock");
        stream
            .write_all(
                b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\n{",
            )
            .expect("write incomplete request");

        let error = mock
            .finish()
            .expect_err("incomplete request must reach finish");

        assert!(error.to_string().contains("scenario `prompt-ready`"));
        assert!(error.to_string().contains("incomplete HTTP request"));
        drop(stream);
        mock.begin(Scenario::no_request("next"))
            .expect("late failure was reported, not erased");
        mock.finish().expect("verify next scenario");
    }

    #[test]
    fn finish_drains_a_queued_extra_request() {
        let mock = Arc::new(Mock::start().expect("start mock"));
        mock.begin(Scenario::text("short", "prompt", "response"))
            .expect("arm scenario");
        let encoded = encoded_request(&text_request("prompt", false));
        let split = encoded.len() - 1;
        let mut first = TcpStream::connect(mock.address).expect("connect first request");
        first
            .write_all(&encoded[..split])
            .expect("write partial first request");
        mock.wait_for_test_event(TestEvent::BodyReadPending);
        let mut second = TcpStream::connect(mock.address).expect("connect queued request");
        second.write_all(&encoded).expect("write queued request");
        second
            .shutdown(std::net::Shutdown::Write)
            .expect("finish queued request");
        let finishing_mock = Arc::clone(&mock);
        let finish_thread = thread::spawn(move || finishing_mock.finish());
        mock.wait_for_test_event(TestEvent::BarrierQueued);
        first
            .write_all(&encoded[split..])
            .expect("complete first request");
        first
            .shutdown(std::net::Shutdown::Write)
            .expect("finish first request");
        mock.wait_for_test_event(TestEvent::BarrierConnectionAccepted);

        let error = finish_thread
            .join()
            .expect("join finish")
            .expect_err("queued second request must reach finish");

        assert!(error.to_string().contains("more requests"));
    }

    #[test]
    fn segmented_pipelined_request_bytes_are_rejected() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::text("short", "prompt", "response"))
            .expect("arm scenario");
        let encoded = encoded_request(&text_request("prompt", false));
        let split = encoded.len() - 1;
        let mut stream = TcpStream::connect(mock.address).expect("connect mock");
        stream
            .write_all(&encoded[..split])
            .expect("write request except final body byte");
        mock.wait_for_test_event(TestEvent::BodyReadPending);
        let mut final_segment = Vec::with_capacity(encoded.len() + 1);
        final_segment.extend_from_slice(&encoded[split..]);
        final_segment.extend_from_slice(&encoded);
        stream
            .write_all(&final_segment)
            .expect("write final body byte and pipelined request together");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish segmented request");

        let error = mock
            .finish()
            .expect_err("segmented pipelined request bytes must fail");

        assert!(error.to_string().contains("scenario `short`"));
        assert!(error.to_string().contains("trailing or pipelined"));
    }

    #[test]
    fn pipelined_request_bytes_are_rejected() {
        let mock = Mock::start().expect("start mock");
        mock.begin(Scenario::text("short", "prompt", "response"))
            .expect("arm scenario");
        let encoded = encoded_request(&text_request("prompt", false));
        let mut pipelined = Vec::with_capacity(encoded.len() * 2);
        pipelined.extend_from_slice(&encoded);
        pipelined.extend_from_slice(&encoded);
        let mut stream = TcpStream::connect(mock.address).expect("connect mock");
        stream
            .write_all(&pipelined)
            .expect("write pipelined requests once");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish pipelined requests");

        let error = mock
            .finish()
            .expect_err("pipelined request bytes must fail");

        assert!(error.to_string().contains("scenario `short`"));
        assert!(error.to_string().contains("trailing or pipelined"));
    }

    #[test]
    fn parser_failure_reports_scenario_and_redacts_payload() {
        let private_prompt = "private-expected-prompt-marker";
        let private_body = "private-invalid-json-body-marker";
        let private_header = "private-header-marker";
        let request = format!(
            "POST {CHAT_PATH} HTTP/1.1\r\nHost: localhost\r\nX-Private: {private_header}\r\nContent-Length: {}\r\n\r\n{private_body}",
            private_body.len()
        );

        let error = finish_raw_failure("invalid-json", private_prompt, request.as_bytes());

        assert!(error.contains("scenario `invalid-json`"));
        assert!(error.contains("invalid JSON body"));
        assert!(!error.contains(private_prompt));
        assert!(!error.contains(private_body));
        assert!(!error.contains(private_header));
    }

    #[test]
    fn parser_categories_are_safe_and_actionable() {
        let prompt = "private-parser-prompt";
        let missing_length = finish_raw_failure(
            "missing-length",
            prompt,
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let invalid_length = finish_raw_failure(
            "invalid-length",
            prompt,
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Length: nope\r\n\r\n",
        );
        let transfer_encoding = finish_raw_failure(
            "transfer-encoding",
            prompt,
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        let unsupported_method = finish_raw_failure(
            "unsupported-method",
            prompt,
            b"PUT /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let private_path = "/private-path-marker";
        let path_request = format!("POST {private_path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let unsupported_path =
            finish_raw_failure("unsupported-path", prompt, path_request.as_bytes());

        assert!(missing_length.contains("scenario `missing-length`"));
        assert!(missing_length.contains("without Content-Length"));
        assert!(invalid_length.contains("invalid Content-Length"));
        assert!(transfer_encoding.contains("unsupported Transfer-Encoding"));
        assert!(unsupported_method.contains("unsupported HTTP method"));
        assert!(unsupported_path.contains("unsupported HTTP path"));
        for error in [
            missing_length,
            invalid_length,
            transfer_encoding,
            unsupported_method,
            unsupported_path,
        ] {
            assert!(!error.contains(prompt));
            assert!(!error.contains(private_path));
        }
    }

    #[test]
    fn proxy_egress_attempts_fail_closed_and_are_redacted() {
        let prompt = "private-proxy-prompt";
        let connect = finish_raw_failure(
            "proxy-connect",
            prompt,
            b"CONNECT private.example:443 HTTP/1.1\r\nHost: private.example:443\r\n\r\n",
        );
        let absolute = finish_raw_failure(
            "proxy-absolute",
            prompt,
            b"GET http://private.example/metadata HTTP/1.1\r\nHost: private.example\r\n\r\n",
        );

        assert!(connect.contains("blocked an outbound HTTPS proxy CONNECT"));
        assert!(absolute.contains("blocked an outbound absolute-form proxy request"));
        for error in [connect, absolute] {
            assert!(!error.contains(prompt));
            assert!(!error.contains("private.example"));
        }
    }

    #[test]
    fn begin_error_names_only_the_active_safe_scenario() {
        let mock = Mock::start().expect("start mock");
        let private_prompt = "private-active-prompt";
        mock.begin(Scenario::text(
            "active-scenario",
            private_prompt,
            "response",
        ))
        .expect("arm scenario");

        let error = mock
            .begin(Scenario::no_request("replacement"))
            .expect_err("second active scenario must fail")
            .to_string();

        assert!(error.contains("scenario `active-scenario`"));
        assert!(error.contains("more requests than the scenario allows"));
        assert!(!error.contains(private_prompt));
    }

    #[test]
    fn base_url_is_loopback_only() {
        let mock = Mock::start().expect("start mock");

        assert!(mock.base_url().starts_with("http://127.0.0.1:"));
        assert!(mock.base_url().ends_with("/v1"));
    }
}
