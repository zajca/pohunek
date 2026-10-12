//! Bounded loopback OpenAI-compatible endpoint for a real Hermes session, and
//! the private Hermes home that points a pinned Hermes at it.
//!
//! The endpoint accepts only the traffic the pinned Hermes sends: its model
//! discovery probes in their fixed order, chat completions for [`MODEL`] with
//! the placeholder key, and denied `CONNECT` probes. [`HermesProvider::start`]
//! serves one Hermes process and one turn with no hold. [`HermesProvider::held`]
//! serves a bounded number of processes and turns and holds every completion
//! until the test opens the gate, so a test can observe the working state and
//! a resumed conversation.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-12

use std::fs;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

/// Model the test configures for Hermes and the only model the endpoint serves.
///
/// The Hermes configuration names it as the provider's default model, so a
/// chat request for any other model is unexpected traffic.
pub(crate) const MODEL: &str = "pohunek-compat-v1";

/// Completion id of every deterministic response.
const COMPLETION_ID: &str = "chatcmpl-pohunek-hermes";

/// Placeholder key Hermes documents for named providers without credentials.
///
/// The test configures it as the provider's `api_key`, and the endpoint
/// accepts only `Authorization: Bearer <API_KEY>`; any other credential marks
/// the exchange as unexpected.
pub(crate) const API_KEY: &str = "no-key-required";

/// Authorization scheme Hermes sends in front of [`API_KEY`].
const BEARER_PREFIX: &str = "Bearer ";

/// Exact ordered probe sequence the pinned Hermes sends for a `custom:`
/// provider before its first completion.
///
/// Each probe receives `404 Not Found`, so Hermes falls back to the configured
/// model. Every endpoint refuses a probe out of this order at request time.
/// [`HermesProvider::start`] also refuses a second round then;
/// [`HermesProvider::held`] refuses more rounds than its processes, and
/// [`HermesProvider::assert_exchanges`] checks afterwards that each process
/// sent exactly one round, right before its completions.
const DISCOVERY: [&str; 5] = [
    "/api/v1/models",
    "/api/tags",
    "/v1/props",
    "/props",
    "/version",
];

/// Upper bound on the request line plus headers of one request.
///
/// Hermes sends a handful of short headers, so a few KiB suffice; the bound
/// keeps a misbehaving client from making the endpoint buffer without limit.
/// Lowering it far enough rejects legitimate Hermes requests as invalid.
const MAX_HEADERS: usize = 32 * 1024;

/// Upper bound on one chat-completion request body.
///
/// The single turn carries Hermes' system prompt and tool schemas, a few tens
/// of KiB for the pinned release; the margin absorbs prompt growth across
/// toolset changes while a body past it is rejected as unexpected traffic.
const MAX_BODY: usize = 1024 * 1024;

/// Read and write timeout on each accepted loopback connection.
///
/// The endpoint serves connections one at a time, so a client that stops
/// mid-request would otherwise block every later request until the test's
/// hang guard fires without provider diagnostics. A loopback exchange
/// completes in milliseconds; this leaves ample room on a loaded runner and
/// records a stalled exchange as a provider failure instead.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub Copilot hosts Hermes may probe through the proxy; each is denied.
const PROXY_PROBE_HOSTS: [&str; 2] = ["api.github.com:443", "api.githubcopilot.com:443"];

/// Most `CONNECT` probes to [`PROXY_PROBE_HOSTS`] the endpoint denies per
/// Hermes process it serves.
///
/// Hermes may probe those hosts through the configured fail-closed proxy at
/// startup; each probe receives `403 Forbidden`. The pinned release retries a
/// failed Copilot token exchange up to three times (`_EXCHANGE_MAX_ATTEMPTS` in
/// `hermes_cli/copilot_auth.py`), and the ceiling admits two such rounds per
/// process. More denials mean Hermes keeps retrying a denied host, which the
/// endpoint reports as unexpected traffic.
const PROXY_DENIALS_PER_PROCESS: usize = 6;

/// Most chat-completion bodies the endpoint keeps for
/// [`HermesProvider::bodies_containing`].
///
/// A scenario inspects at most a few turns; the bound keeps a misbehaving
/// client from growing memory.
const KEPT_BODIES: usize = 16;

/// Proxy variables that route every non-loopback request of a real Hermes
/// process to the fail-closed endpoint.
pub(crate) const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// Hosts a real Hermes process reaches directly instead of through the proxy.
pub(crate) const LOOPBACK_NO_PROXY: &str = "localhost,127.0.0.1";

/// Mode of the private Hermes home and its cache directory.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of each file of the private Hermes home.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// How much Hermes traffic one endpoint accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Exchanges {
    /// Hermes processes served. The pinned release runs one discovery round
    /// per process before its first completion. While serving, the endpoint
    /// accepts at most this many complete rounds and a completion only after
    /// a complete round; [`HermesProvider::assert_exchanges`] requires the
    /// exact per-process shape.
    pub(crate) processes: usize,
    /// Chat completions across every process.
    pub(crate) turns: usize,
}

#[derive(Debug, Default)]
struct State {
    discovery: Vec<String>,
    prompts: Vec<String>,
    bodies: Vec<String>,
    proxy_denials: Vec<String>,
    requests: Vec<String>,
    failure: Option<&'static str>,
    unexpected_authorization: bool,
}

/// What the serving thread and the test share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    exchanges: Exchanges,
    started: AtomicUsize,
    finished: AtomicUsize,
    gate_open: Mutex<bool>,
    gate_changed: Condvar,
}

/// One deterministic model endpoint. It accepts only pinned discovery and chat traffic.
pub(crate) struct HermesProvider {
    address: SocketAddr,
    shared: Arc<Shared>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HermesProvider {
    /// Starts the endpoint on IPv4 loopback for one Hermes process and one
    /// turn, answered at once.
    pub(crate) fn start(response: impl Into<String>) -> Self {
        Self::serve_on_loopback(
            response.into(),
            Exchanges {
                processes: 1,
                turns: 1,
            },
            true,
        )
    }

    /// Starts the endpoint on IPv4 loopback for `exchanges`, holding every
    /// completion until [`Self::open_gate`].
    pub(crate) fn held(response: impl Into<String>, exchanges: Exchanges) -> Self {
        Self::serve_on_loopback(response.into(), exchanges, false)
    }

    fn serve_on_loopback(response: String, exchanges: Exchanges, gate_open: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind Hermes provider");
        let address = listener.local_addr().expect("Hermes provider address");
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            exchanges,
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            gate_open: Mutex::new(gate_open),
            gate_changed: Condvar::new(),
        });
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stopping = Arc::clone(&stopping);
        let thread = thread::spawn(move || {
            serve(&listener, &thread_shared, &thread_stopping, &response);
        });
        Self {
            address,
            shared,
            stopping,
            thread: Some(thread),
        }
    }

    /// Provider API URL used by Hermes.
    pub(crate) fn api_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// Fail-closed HTTP proxy URL for non-local startup probes.
    pub(crate) fn proxy_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Environment entries that send every proxied Hermes request to this
    /// endpoint and let loopback traffic through directly.
    pub(crate) fn proxy_env(&self) -> Vec<(String, String)> {
        let proxy = self.proxy_url();
        PROXY_VARIABLES
            .iter()
            .map(|variable| ((*variable).to_owned(), proxy.clone()))
            .chain(
                ["NO_PROXY", "no_proxy"]
                    .iter()
                    .map(|variable| ((*variable).to_owned(), LOOPBACK_NO_PROXY.to_owned())),
            )
            .collect()
    }

    /// Sanitized request paths and safe headers for a failed launch assertion.
    pub(crate) fn diagnostics(&self) -> String {
        let state = self.shared.state.lock().expect("Hermes provider state");
        format!("failure={:?}, requests={:?}", state.failure, state.requests)
    }

    /// Panics with the provider diagnostics once the endpoint has refused any
    /// exchange, so a waiting scenario fails at the first unexpected request.
    pub(crate) fn assert_healthy(&self) {
        let state = self.shared.state.lock().expect("Hermes provider state");
        assert!(
            state.failure.is_none(),
            "the Hermes provider refused an exchange: failure={:?}, requests={:?}",
            state.failure,
            state.requests
        );
    }

    /// Checks the exact traffic of a [`Self::held`] endpoint once every
    /// process has finished its turns: one discovery round per process, each
    /// followed directly by that process's completions, the user prompts
    /// `expected_prompts` in order, and no foreign credential.
    ///
    /// # Panics
    ///
    /// Panics with the recorded requests when the traffic has another shape.
    pub(crate) fn assert_exchanges(&self, expected_prompts: &[&str]) {
        let state = self.shared.state.lock().expect("Hermes provider state");
        let processes = self.shared.exchanges.processes;
        assert!(
            state.failure.is_none(),
            "unexpected Hermes provider traffic: {:?}; requests: {:?}",
            state.failure,
            state.requests
        );
        assert_eq!(
            state.discovery,
            DISCOVERY.repeat(processes),
            "one discovery round per Hermes process: {:?}",
            state.requests
        );
        assert_eq!(state.prompts, expected_prompts, "the model turns in order");
        assert!(
            !state.unexpected_authorization,
            "Hermes used only its explicit test placeholder authorization"
        );
        // Each recorded request line starts with its method; the denied
        // `CONNECT` probes are bounded separately and may come at any time.
        let methods: Vec<&str> = state
            .requests
            .iter()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|method| *method != "CONNECT")
            .collect();
        let mut rounds = 0;
        let mut rest = methods.as_slice();
        while !rest.is_empty() {
            let (round, after) = rest.split_at(DISCOVERY.len().min(rest.len()));
            let completions = after.iter().take_while(|method| **method == "POST").count();
            assert!(
                round.iter().all(|method| *method == "GET") && completions > 0,
                "each discovery round is followed directly by its process's completions: {:?}",
                state.requests
            );
            rounds += 1;
            rest = &after[completions..];
        }
        assert_eq!(rounds, processes, "rounds: {:?}", state.requests);
    }

    /// Checks discovery order, one exact user prompt, and absence of credentials.
    pub(crate) fn assert_turn(&self, expected_prompt: &str) {
        let state = self.shared.state.lock().expect("Hermes provider state");
        assert!(
            state.failure.is_none(),
            "unexpected Hermes provider traffic: {:?}; requests: {:?}",
            state.failure,
            state.requests
        );
        assert_eq!(state.discovery, DISCOVERY, "model discovery sequence");
        assert_eq!(state.prompts, [expected_prompt], "one exact model turn");
        assert!(
            state.proxy_denials.len() <= PROXY_DENIALS_PER_PROCESS,
            "bounded Hermes startup proxy denials: {:?}",
            state.proxy_denials
        );
        assert!(
            !state.unexpected_authorization,
            "Hermes used only its explicit test placeholder authorization"
        );
    }

    /// Chat completions whose request was accepted, held or not.
    pub(crate) fn started(&self) -> usize {
        self.shared.started.load(Ordering::SeqCst)
    }

    /// Chat completions whose response was written.
    pub(crate) fn finished(&self) -> usize {
        self.shared.finished.load(Ordering::SeqCst)
    }

    /// The kept chat-completion bodies that carry `needle`, oldest first.
    pub(crate) fn bodies_containing(&self, needle: &str) -> Vec<String> {
        self.shared
            .state
            .lock()
            .expect("Hermes provider state")
            .bodies
            .iter()
            .filter(|body| body.contains(needle))
            .cloned()
            .collect()
    }

    /// Whether any request carried a credential other than the placeholder.
    pub(crate) fn saw_foreign_credential(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("Hermes provider state")
            .unexpected_authorization
    }

    /// Lets every held completion, and every later one, be answered.
    pub(crate) fn open_gate(&self) {
        *self.shared.gate_open.lock().expect("Hermes provider gate") = true;
        self.shared.gate_changed.notify_all();
    }
}

impl Drop for HermesProvider {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // The serving thread may be holding a completion; it must not block
        // the join.
        self.open_gate();
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("Hermes provider thread stops");
        }
    }
}

/// Writes one private model-free Hermes home whose only model provider is
/// `provider`, with the offline catalog, credential-source and update caches
/// the pinned release reads at startup.
pub(crate) fn write_home(home: &Path, provider: &HermesProvider) {
    fs::create_dir_all(home).expect("create isolated Hermes home");
    fs::set_permissions(home, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
        .expect("private Hermes home");
    let config = format!(
        "model:\n  default: {MODEL}\n  provider: custom:pohunek-compat\n  api_mode: chat_completions\n  context_length: 64000\nmodel_catalog:\n  enabled: false\nproviders:\n  pohunek-compat:\n    name: pohunek-compat\n    api: {}\n    api_key: {}\n    transport: chat_completions\n    default_model: {MODEL}\n    discover_models: false\n    models:\n      {MODEL}:\n        context_length: 64000\nfallback_providers: []\ntoolsets:\n  - terminal\napprovals:\n  mode: manual\n  timeout: 300\nsecurity:\n  tirith_enabled: false\n  allow_lazy_installs: false\nauxiliary:\n  title_generation:\n    enabled: false\ntelemetry:\n  shared_metrics:\n    enabled: false\n",
        provider.api_url(),
        API_KEY,
    );
    write_private(home.join("config.yaml"), config.as_bytes());
    write_private(
        home.join("models_dev_cache.json"),
        br#"{"pohunek-offline":{"name":"Pohunek offline compatibility cache","env":[],"api":"","doc":"","models":{}}}
"#,
    );
    fs::create_dir_all(home.join("cache")).expect("create model catalog cache");
    fs::set_permissions(
        home.join("cache"),
        fs::Permissions::from_mode(PRIVATE_DIR_MODE),
    )
    .expect("private model catalog cache");
    write_private(
        home.join("cache/model_catalog.json"),
        br#"{"version":1,"providers":{}}
"#,
    );
    write_private(
        home.join("auth.json"),
        br#"{"version":1,"providers":{},"suppressed_sources":{"copilot":["gh_cli","env:COPILOT_GITHUB_TOKEN","env:GH_TOKEN","env:GITHUB_TOKEN"]}}
"#,
    );
    write_private(
        home.join(".update_check"),
        br#"{"ts":4102444800,"behind":null,"rev":null,"ver":"0.20.0"}
"#,
    );
}

fn write_private(path: PathBuf, contents: &[u8]) {
    fs::write(&path, contents).expect("write isolated Hermes configuration");
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .expect("private Hermes configuration");
}

fn serve(listener: &TcpListener, shared: &Arc<Shared>, stopping: &AtomicBool, reply: &str) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        if let Err(error) = respond(&mut stream, shared, reply) {
            shared
                .state
                .lock()
                .expect("Hermes provider state")
                .failure
                .get_or_insert(if error.kind() == io::ErrorKind::InvalidData {
                    "invalid or unexpected request"
                } else {
                    "request I/O failed"
                });
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one bounded HTTP handler validates the entire hermetic provider exchange"
)]
fn respond(stream: &mut TcpStream, shared: &Shared, reply: &str) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.is_empty() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut headers = 0;
    let mut content_length = None;
    let mut chunked = false;
    let mut authorization = false;
    let mut has_authorization = false;
    let mut host = None;
    let mut content_type = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        headers += read;
        if read == 0 || headers > MAX_HEADERS {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "authorization" {
                has_authorization = true;
                authorization = value.strip_prefix(BEARER_PREFIX) == Some(API_KEY);
            } else if name == "proxy-authorization" {
                has_authorization = true;
                authorization = false;
            } else if name == "host" {
                host = Some(value.to_owned());
            } else if name == "content-length" {
                if content_length.is_some() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                content_length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_error| io::ErrorKind::InvalidData)?,
                );
            } else if name == "content-type" {
                content_type = Some(value.to_owned());
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }

    let mut state = shared.state.lock().expect("Hermes provider state");
    state.requests.push(format!(
        "{} host={:?} auth={} content-type={:?} content-length={:?} chunked={}",
        request_line.trim(),
        host,
        has_authorization,
        content_type,
        content_length,
        chunked
    ));
    if has_authorization && !authorization {
        state.unexpected_authorization = true;
    }
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().ok_or(io::ErrorKind::InvalidData)?;
    let path = request_parts.next().ok_or(io::ErrorKind::InvalidData)?;
    let exchanges = shared.exchanges;

    if method == "CONNECT" {
        if !PROXY_PROBE_HOSTS.contains(&path)
            || has_authorization
            || content_length.unwrap_or_default() != 0
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        state.proxy_denials.push(path.to_owned());
        if state.proxy_denials.len() > PROXY_DENIALS_PER_PROCESS * exchanges.processes {
            return Err(io::ErrorKind::InvalidData.into());
        }
        return write_response(stream, "403 Forbidden", "text/plain", "");
    }

    // Each process probes the discovery paths in their fixed order once.
    if method == "GET" {
        let probed = state.discovery.len();
        if probed >= DISCOVERY.len() * exchanges.processes
            || path != DISCOVERY[probed % DISCOVERY.len()]
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        state.discovery.push(path.to_owned());
        return write_response(stream, "404 Not Found", "application/json", "{}");
    }

    if method != "POST" || path != "/v1/chat/completions" || chunked {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if state.discovery.is_empty() || !state.discovery.len().is_multiple_of(DISCOVERY.len()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if !has_authorization
        || !authorization
        || host.as_deref() != Some(&format!("127.0.0.1:{}", stream.local_addr()?.port()))
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let length = content_length.ok_or(io::ErrorKind::InvalidData)?;
    if length > MAX_BODY {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let raw = String::from_utf8(body).map_err(|_error| io::ErrorKind::InvalidData)?;
    let body: Value = serde_json::from_str(&raw).map_err(|_error| io::ErrorKind::InvalidData)?;
    if body.get("model").and_then(Value::as_str) != Some(MODEL) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let prompt = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or(io::ErrorKind::InvalidData)?;
    state.prompts.push(prompt.to_owned());
    if state.prompts.len() > exchanges.turns {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if state.bodies.len() == KEPT_BODIES {
        state.bodies.remove(0);
    }
    state.bodies.push(raw);
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    drop(state);
    shared.started.fetch_add(1, Ordering::SeqCst);
    let mut open = shared.gate_open.lock().expect("Hermes provider gate");
    while !*open {
        open = shared
            .gate_changed
            .wait(open)
            .expect("Hermes provider gate wait");
    }
    drop(open);
    if streaming {
        let events = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":COMPLETION_ID,"object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{"role":"assistant","content":reply},"finish_reason":null}]}),
            json!({"id":COMPLETION_ID,"object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        );
        write_response(stream, "200 OK", "text/event-stream", &events)?;
    } else {
        let body = json!({
            "id": COMPLETION_ID,
            "object": "chat.completion",
            "created": 0,
            "model": MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}]
        });
        write_response(stream, "200 OK", "application/json", &body.to_string())?;
    }
    shared.finished.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )
}
