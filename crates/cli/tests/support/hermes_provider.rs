//! Bounded loopback OpenAI-compatible endpoint for a real Hermes session.

// Rust guideline compliant 2026-10-12

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// model; a probe out of this order, or a repeated round, is unexpected traffic.
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

/// Most `CONNECT` probes to [`PROXY_PROBE_HOSTS`] the endpoint denies.
///
/// Hermes may probe those hosts through the configured fail-closed proxy at
/// startup; each probe receives `403 Forbidden`. The pinned release retries a
/// failed Copilot token exchange up to three times (`_EXCHANGE_MAX_ATTEMPTS` in
/// `hermes_cli/copilot_auth.py`), and the ceiling admits two such rounds. More
/// denials mean Hermes keeps retrying a denied host, which the endpoint
/// reports as unexpected traffic.
const MAX_PROXY_DENIALS: usize = 6;

#[derive(Debug, Default)]
struct State {
    discovery: Vec<String>,
    prompts: Vec<String>,
    proxy_denials: Vec<String>,
    requests: Vec<String>,
    failure: Option<&'static str>,
    unexpected_authorization: bool,
}

/// One deterministic model endpoint. It accepts only pinned discovery and chat traffic.
pub(crate) struct HermesProvider {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HermesProvider {
    /// Starts the endpoint on IPv4 loopback.
    pub(crate) fn start(response: impl Into<String>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind Hermes provider");
        let address = listener.local_addr().expect("Hermes provider address");
        let state = Arc::new(Mutex::new(State::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stopping = Arc::clone(&stopping);
        let response = response.into();
        let thread_response = response.clone();
        let thread = thread::spawn(move || {
            serve(&listener, &thread_state, &thread_stopping, &thread_response);
        });
        Self {
            address,
            state,
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

    /// Sanitized request paths and safe headers for a failed launch assertion.
    pub(crate) fn diagnostics(&self) -> String {
        let state = self.state.lock().expect("Hermes provider state");
        format!("failure={:?}, requests={:?}", state.failure, state.requests)
    }

    /// Panics with the provider diagnostics once the endpoint has refused any
    /// exchange, so a waiting scenario fails at the first unexpected request.
    pub(crate) fn assert_healthy(&self) {
        let state = self.state.lock().expect("Hermes provider state");
        assert!(
            state.failure.is_none(),
            "the Hermes provider refused an exchange: failure={:?}, requests={:?}",
            state.failure,
            state.requests
        );
    }

    /// Checks discovery order, one exact user prompt, and absence of credentials.
    pub(crate) fn assert_turn(&self, expected_prompt: &str) {
        let state = self.state.lock().expect("Hermes provider state");
        assert!(
            state.failure.is_none(),
            "unexpected Hermes provider traffic: {:?}; requests: {:?}",
            state.failure,
            state.requests
        );
        assert_eq!(state.discovery, DISCOVERY, "model discovery sequence");
        assert_eq!(state.prompts, [expected_prompt], "one exact model turn");
        assert!(
            state.proxy_denials.len() <= MAX_PROXY_DENIALS,
            "bounded Hermes startup proxy denials: {:?}",
            state.proxy_denials
        );
        assert!(
            !state.unexpected_authorization,
            "Hermes used only its explicit test placeholder authorization"
        );
    }
}

impl Drop for HermesProvider {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("Hermes provider thread stops");
        }
    }
}

fn serve(listener: &TcpListener, state: &Arc<Mutex<State>>, stopping: &AtomicBool, reply: &str) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            break;
        }
        let Ok(mut stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        if let Err(error) = respond(&mut stream, state, reply) {
            state
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
fn respond(stream: &mut TcpStream, state: &Arc<Mutex<State>>, reply: &str) -> io::Result<()> {
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

    state
        .lock()
        .expect("Hermes provider state")
        .requests
        .push(format!(
            "{} host={:?} auth={} content-type={:?} content-length={:?} chunked={}",
            request_line.trim(),
            host,
            has_authorization,
            content_type,
            content_length,
            chunked
        ));
    if has_authorization && !authorization {
        state
            .lock()
            .expect("Hermes provider state")
            .unexpected_authorization = true;
    }
    let mut state = state.lock().expect("Hermes provider state");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().ok_or(io::ErrorKind::InvalidData)?;
    let path = request_parts.next().ok_or(io::ErrorKind::InvalidData)?;

    if method == "CONNECT" {
        if !PROXY_PROBE_HOSTS.contains(&path)
            || has_authorization
            || content_length.unwrap_or_default() != 0
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        state.proxy_denials.push(path.to_owned());
        if state.proxy_denials.len() > MAX_PROXY_DENIALS {
            return Err(io::ErrorKind::InvalidData.into());
        }
        return write_response(stream, "403 Forbidden", "text/plain", "");
    }

    if method == "GET" {
        if path != DISCOVERY.get(state.discovery.len()).copied().unwrap_or("") {
            return Err(io::ErrorKind::InvalidData.into());
        }
        state.discovery.push(path.to_owned());
        return write_response(stream, "404 Not Found", "application/json", "{}");
    }

    if method != "POST" || path != "/v1/chat/completions" || chunked {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if state.discovery != DISCOVERY {
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
    let body: Value = serde_json::from_slice(&body).map_err(|_error| io::ErrorKind::InvalidData)?;
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
    if state.prompts.len() != 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    drop(state);
    if streaming {
        let events = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":COMPLETION_ID,"object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{"role":"assistant","content":reply},"finish_reason":null}]}),
            json!({"id":COMPLETION_ID,"object":"chat.completion.chunk","created":0,"model":MODEL,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        );
        write_response(stream, "200 OK", "text/event-stream", &events)
    } else {
        let body = json!({
            "id": COMPLETION_ID,
            "object": "chat.completion",
            "created": 0,
            "model": MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}]
        });
        write_response(stream, "200 OK", "application/json", &body.to_string())
    }
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
