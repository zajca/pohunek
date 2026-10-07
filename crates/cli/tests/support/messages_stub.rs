//! A loopback Anthropic Messages endpoint for tests that drive a real Claude
//! Code without a model provider.
//!
//! The stub serves `POST /v1/messages` on an ephemeral IPv4 loopback port, as a
//! server-sent event stream when the request asks for one and as a JSON message
//! otherwise (Claude Code also issues short non-streaming helper calls). A
//! request whose body carries [`HOLD_MARKER`] gets its first text chunk at once
//! and the rest only after the test opens the gate, so a test can hold the agent
//! in its working state and release it deterministically. A request carrying
//! [`APPROVAL_MARKER`], [`QUESTION_MARKER`] or [`SUBAGENT_MARKER`] is answered
//! with a tool call (a shell command that needs approval, a multiple-choice
//! question, a subagent task) until the conversation holds a tool result. Every
//! other request gets a complete text reply.
//!
//! The stub never sees a credential: a request may carry only the dummy
//! [`API_KEY`] in `x-api-key`, and the stub records whether any request carried
//! an `Authorization` header or another key; a test asserts that none did.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-06

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Model id the stub serves.
pub(crate) const MODEL_ID: &str = "claude-sonnet-4-5";

/// The key the tests give Claude Code. It authorizes nothing: the stub accepts
/// every request and only checks that no other key arrives.
pub(crate) const API_KEY: &str = "pohunek-stub-key-00000000000000000000";

/// Text streamed before the gate, visible while the response is held.
pub(crate) const FIRST_CHUNK: &str = "working";

/// Paragraph break after the first chunk: Claude Code draws streamed text only
/// once its paragraph is complete.
pub(crate) const CHUNK_BREAK: &str = "\n\n";

/// Text streamed after the gate opens.
pub(crate) const SECOND_CHUNK: &str = "done";

/// Prompt text that makes the stub hold the reply until the gate opens.
pub(crate) const HOLD_MARKER: &str = "[hold]";

/// Prompt text that makes the stub answer with a command that needs approval.
pub(crate) const APPROVAL_MARKER: &str = "[approval]";

/// Prompt text that makes the stub answer with a multiple-choice question.
pub(crate) const QUESTION_MARKER: &str = "[question]";

/// Prompt text that makes the stub answer with a subagent task.
pub(crate) const SUBAGENT_MARKER: &str = "[subagent]";

/// Prompt text of the subagent task; the subagent's own request carries it and
/// is answered with plain text.
pub(crate) const SUBAGENT_PROMPT: &str = "subagent job: reply with one word";

/// Shell command the approval reply asks to run.
pub(crate) const APPROVAL_COMMAND: &str = "touch approved.txt";

/// Question the question reply asks.
pub(crate) const QUESTION_TEXT: &str = "Which flavor do you want?";

/// Name of the tool the subagent reply calls.
///
/// Claude Code 2.1.289 names its subagent tool `Task`.
pub(crate) const SUBAGENT_TOOL: &str = "Task";

/// Largest request body the stub reads, in bytes.
///
/// Agent requests carry the system prompt, tool declarations and the
/// conversation, a few hundred kilobytes at most; the bound keeps a
/// misbehaving client from growing memory.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Largest request header block the stub reads, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Substring of a request body that marks a tool result in the conversation.
const TOOL_RESULT_MARKER: &str = "\"tool_result\"";

/// Shared state between the acceptor, the response threads and the test.
#[derive(Debug, Default)]
struct State {
    started: AtomicUsize,
    finished: AtomicUsize,
    unexpected_credential: AtomicBool,
    stopping: AtomicBool,
    gate_open: Mutex<bool>,
    gate_changed: Condvar,
    paths: Mutex<Vec<String>>,
    bodies: Mutex<Vec<String>>,
}

/// Most recent `/v1/messages` request bodies the stub keeps for
/// [`MessagesStub::requests_containing`].
///
/// Bounded so a long session does not grow memory; the tests inspect requests
/// that are at most a few turns old.
const KEPT_BODIES: usize = 64;

/// A running stub endpoint.
#[derive(Debug)]
pub(crate) struct MessagesStub {
    address: SocketAddr,
    state: Arc<State>,
    acceptor: Option<JoinHandle<()>>,
}

impl MessagesStub {
    /// Binds an ephemeral loopback port and starts serving. The gate starts
    /// closed.
    ///
    /// # Panics
    ///
    /// Panics when the loopback socket cannot be bound.
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind the stub");
        let address = listener.local_addr().expect("stub address");
        let state = Arc::new(State::default());
        let acceptor = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || accept_loop(&listener, &state))
        };
        Self {
            address,
            state,
            acceptor: Some(acceptor),
        }
    }

    /// The `ANTHROPIC_BASE_URL` Claude Code is pointed at.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Requests whose response has begun.
    pub(crate) fn started(&self) -> usize {
        self.state.started.load(Ordering::SeqCst)
    }

    /// Requests whose response completed.
    pub(crate) fn finished(&self) -> usize {
        self.state.finished.load(Ordering::SeqCst)
    }

    /// Whether any request carried an `Authorization` header or an API key
    /// other than [`API_KEY`].
    pub(crate) fn saw_credential(&self) -> bool {
        self.state.unexpected_credential.load(Ordering::SeqCst)
    }

    /// Request paths the stub received, in order, for the diagnostics of a
    /// failing test.
    pub(crate) fn paths(&self) -> Vec<String> {
        self.state.paths.lock().expect("paths lock").clone()
    }

    /// How many of the most recent message requests carried `needle` in their
    /// body.
    pub(crate) fn requests_containing(&self, needle: &str) -> usize {
        self.state
            .bodies
            .lock()
            .expect("bodies lock")
            .iter()
            .filter(|body| body.contains(needle))
            .count()
    }

    /// Lets every held response, and every later one, complete.
    pub(crate) fn open_gate(&self) {
        *self.state.gate_open.lock().expect("gate lock") = true;
        self.state.gate_changed.notify_all();
    }

    /// Holds the second chunk of every marked response that starts from now on.
    pub(crate) fn close_gate(&self) {
        *self.state.gate_open.lock().expect("gate lock") = false;
    }
}

impl Drop for MessagesStub {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        self.open_gate();
        // A throwaway connection wakes the blocked `accept`.
        let _ = TcpStream::connect(self.address);
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, state: &Arc<State>) {
    for stream in listener.incoming() {
        if state.stopping.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(state);
        // Responder threads are detached: a client that disconnects
        // mid-response is the agent being stopped, and a connection that never
        // sends a request must not block shutdown.
        std::thread::spawn(move || {
            let _ = respond(stream, &state);
        });
    }
}

/// One parsed request.
struct Request {
    method: String,
    path: String,
    body: String,
}

impl Request {
    fn is_messages_call(&self) -> bool {
        self.method == "POST" && self.path.split('?').next() == Some("/v1/messages")
    }

    fn is_token_count(&self) -> bool {
        self.method == "POST" && self.path.split('?').next() == Some("/v1/messages/count_tokens")
    }

    fn wants_stream(&self) -> bool {
        serde_json::from_str::<serde_json::Value>(&self.body)
            .is_ok_and(|body| body["stream"] == true)
    }
}

fn respond(stream: TcpStream, state: &State) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let request = read_request(&mut reader, state)?;
    state
        .paths
        .lock()
        .expect("paths lock")
        .push(format!("{} {}", request.method, request.path));
    if request.is_token_count() {
        return write_json(&mut writer, &serde_json::json!({ "input_tokens": 1 }));
    }
    if !request.is_messages_call() {
        return writer.write_all(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    }
    state.started.fetch_add(1, Ordering::SeqCst);
    keep_body(state, &request.body);
    let reply = reply_for(&request.body);
    if request.wants_stream() {
        stream_reply(
            &mut writer,
            state,
            &reply,
            request.body.contains(HOLD_MARKER),
        )?;
    } else {
        write_json(&mut writer, &message_json(&reply.blocks()))?;
    }
    writer.flush()?;
    state.finished.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Remembers `body` for [`MessagesStub::requests_containing`], dropping the
/// oldest one beyond [`KEPT_BODIES`].
fn keep_body(state: &State, body: &str) {
    let mut bodies = state.bodies.lock().expect("bodies lock");
    if bodies.len() == KEPT_BODIES {
        bodies.remove(0);
    }
    bodies.push(body.to_owned());
}

/// What the stub answers one request with.
enum Reply {
    Text,
    Tool {
        name: &'static str,
        input: serde_json::Value,
    },
}

impl Reply {
    fn blocks(&self) -> Vec<serde_json::Value> {
        match self {
            Self::Text => vec![serde_json::json!({
                "type": "text",
                "text": format!("{FIRST_CHUNK}{CHUNK_BREAK}{SECOND_CHUNK}"),
            })],
            Self::Tool { name, input } => vec![serde_json::json!({
                "type": "tool_use",
                "id": "toolu_stub",
                "name": name,
                "input": input,
            })],
        }
    }

    fn stop_reason(&self) -> &'static str {
        match self {
            Self::Text => "end_turn",
            Self::Tool { .. } => "tool_use",
        }
    }
}

/// The reply for a request body: a marked prompt without a tool result in the
/// conversation gets its tool call, everything else plain text.
fn reply_for(body: &str) -> Reply {
    if body.contains(TOOL_RESULT_MARKER) {
        return Reply::Text;
    }
    if body.contains(APPROVAL_MARKER) {
        return Reply::Tool {
            name: "Bash",
            input: serde_json::json!({
                "command": APPROVAL_COMMAND,
                "description": "Create the marker file",
            }),
        };
    }
    if body.contains(QUESTION_MARKER) {
        return Reply::Tool {
            name: "AskUserQuestion",
            input: serde_json::json!({
                "questions": [{
                    "question": QUESTION_TEXT,
                    "header": "Flavor",
                    "multiSelect": false,
                    "options": [
                        { "label": "Vanilla", "description": "Plain" },
                        { "label": "Chocolate", "description": "Sweet" },
                    ],
                }],
            }),
        };
    }
    if body.contains(SUBAGENT_MARKER) {
        return Reply::Tool {
            name: SUBAGENT_TOOL,
            input: serde_json::json!({
                "description": "Stub subagent",
                "prompt": SUBAGENT_PROMPT,
                "subagent_type": "general-purpose",
            }),
        };
    }
    Reply::Text
}

fn message_json(content: &[serde_json::Value]) -> serde_json::Value {
    let stop_reason = if content.iter().any(|block| block["type"] == "tool_use") {
        "tool_use"
    } else {
        "end_turn"
    };
    serde_json::json!({
        "id": "msg_stub",
        "type": "message",
        "role": "assistant",
        "model": MODEL_ID,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": { "input_tokens": 1, "output_tokens": 1 },
    })
}

fn write_json(writer: &mut TcpStream, document: &serde_json::Value) -> io::Result<()> {
    let body = document.to_string();
    writer.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// Streams `reply` as Messages events; a held text reply waits for the gate
/// after its first chunk.
fn stream_reply(
    writer: &mut TcpStream,
    state: &State,
    reply: &Reply,
    held: bool,
) -> io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    let mut start = message_json(&[]);
    start["stop_reason"] = serde_json::Value::Null;
    send_event(
        writer,
        "message_start",
        &serde_json::json!({ "type": "message_start", "message": start }),
    )?;
    match reply {
        Reply::Text => {
            send_event(
                writer,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" },
                }),
            )?;
            send_text_delta(writer, &format!("{FIRST_CHUNK}{CHUNK_BREAK}"))?;
            if held {
                let mut open = state.gate_open.lock().expect("gate lock");
                while !*open {
                    open = state.gate_changed.wait(open).expect("gate wait");
                }
            }
            send_text_delta(writer, SECOND_CHUNK)?;
        }
        Reply::Tool { name, input } => {
            send_event(
                writer,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_stub",
                        "name": name,
                        "input": {},
                    },
                }),
            )?;
            send_event(
                writer,
                "content_block_delta",
                &serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "input_json_delta", "partial_json": input.to_string() },
                }),
            )?;
        }
    }
    send_event(
        writer,
        "content_block_stop",
        &serde_json::json!({ "type": "content_block_stop", "index": 0 }),
    )?;
    send_event(
        writer,
        "message_delta",
        &serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": reply.stop_reason(), "stop_sequence": null },
            "usage": { "output_tokens": 1 },
        }),
    )?;
    send_event(
        writer,
        "message_stop",
        &serde_json::json!({ "type": "message_stop" }),
    )
}

fn send_text_delta(writer: &mut TcpStream, text: &str) -> io::Result<()> {
    send_event(
        writer,
        "content_block_delta",
        &serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": text },
        }),
    )
}

fn send_event(writer: &mut TcpStream, name: &str, payload: &serde_json::Value) -> io::Result<()> {
    writer.write_all(format!("event: {name}\ndata: {payload}\n\n").as_bytes())?;
    writer.flush()
}

/// Reads one HTTP request, noting any unexpected credential, and returns it.
fn read_request(reader: &mut BufReader<TcpStream>, state: &State) -> io::Result<Request> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0_usize;
    let mut chunked = false;
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        header_bytes += read;
        if read == 0 || header_bytes > MAX_HEADER_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            state.unexpected_credential.store(true, Ordering::SeqCst);
        }
        if let Some(value) = lower.strip_prefix("x-api-key:") {
            if value.trim() != API_KEY.to_ascii_lowercase() {
                state.unexpected_credential.store(true, Ordering::SeqCst);
            }
        }
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value
                .trim()
                .parse()
                .map_err(|_error| io::Error::from(io::ErrorKind::InvalidData))?;
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let body = if chunked {
        read_chunked_body(reader)?
    } else if content_length > MAX_BODY_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    } else {
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body)?;
        body
    };
    Ok(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_error| io::Error::from(io::ErrorKind::InvalidData))?;
        if body.len() + size > MAX_BODY_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut chunk = vec![0_u8; size + 2];
        reader.read_exact(&mut chunk)?;
        if size == 0 {
            return Ok(body);
        }
        body.extend_from_slice(&chunk[..size]);
    }
}
