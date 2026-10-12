//! A loopback Anthropic Messages endpoint for tests that drive a real Claude
//! Code without a model provider.
//!
//! The stub serves `POST /v1/messages` on an ephemeral IPv4 loopback port, as a
//! server-sent event stream when the request asks for one and as a JSON message
//! otherwise (Claude Code also issues short non-streaming helper calls). A
//! request whose body carries [`HOLD_MARKER`] gets no text until the test opens
//! the start gate, so the agent stays in a stable working state (status line,
//! no output) until the test releases it. A request carrying
//! [`STREAM_HOLD_MARKER`] gets its first text chunk at once and the rest only
//! after the test opens the stream gate, which holds the agent mid-stream. A request carrying
//! [`APPROVAL_MARKER`], [`QUESTION_MARKER`] or [`SUBAGENT_MARKER`] is answered
//! with a tool call (a shell command that needs approval, a multiple-choice
//! question, a subagent task) until the conversation holds a tool result. Every
//! other request gets a complete text reply.
//!
//! A user turn that carries [`BACKGROUND_MARKER`] or [`RESUME_MARKER`] is
//! answered with a tool call that launches a background agent or resumes one
//! by id. Only the conversation's last message decides those replies, so a
//! later turn of the same conversation is not answered with an earlier turn's
//! tool call. Every model request of that background agent waits on a run
//! gate the test opens: [`MessagesStub::open_run_gate`] releases its first run
//! and [`MessagesStub::open_resume_gate`] the run a resume starts, so the test
//! observes each run while it is in progress. The main agent's request that
//! carries the launch call's result waits on the launch-result gate, and the
//! one that carries the resume call's result on the resume-result gate, so
//! the main turn that launched or resumed the agent ends, and runs its `Stop`
//! hooks, only once the test opens that gate.
//!
//! The stub never sees a credential: a request may carry only the dummy
//! [`API_KEY`] in `x-api-key`, and the stub records whether any request carried
//! an `Authorization` header or another key; a test asserts that none did.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-12

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

/// Text streamed before the stream gate, visible while the response is held
/// mid-stream.
pub(crate) const FIRST_CHUNK: &str = "working";

/// Paragraph break after the first chunk: Claude Code draws streamed text only
/// once its paragraph is complete.
pub(crate) const CHUNK_BREAK: &str = "\n\n";

/// Text streamed after the stream gate opens.
pub(crate) const SECOND_CHUNK: &str = "done";

/// Prompt text that makes the stub send no text until the start gate opens.
pub(crate) const HOLD_MARKER: &str = "[hold]";

/// Prompt text that makes the stub hold the reply after its first chunk until
/// the stream gate opens.
pub(crate) const STREAM_HOLD_MARKER: &str = "[stream-hold]";

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
/// Claude Code 2.1.289 and 2.1.296 declare the subagent tool as `Agent` and
/// accept `Task` as an alias of it.
pub(crate) const SUBAGENT_TOOL: &str = "Task";

/// Prompt text that makes the stub answer with a background agent task.
///
/// The reply passes no `run_in_background`: Claude Code 2.1.289 and 2.1.296
/// run an `Agent` call in the background by default, and their `Agent` input
/// schema admits no such property (`additionalProperties: false`).
pub(crate) const BACKGROUND_MARKER: &str = "[background]";

/// Prompt text of the background agent task. A request whose first message
/// carries it is the background agent's own and waits on a run gate.
pub(crate) const BACKGROUND_PROMPT: &str = "background job: reply with one word";

/// Name of the tool the background reply calls: the name Claude Code 2.1.289
/// and 2.1.296 declare.
pub(crate) const BACKGROUND_TOOL: &str = "Agent";

/// Prompt text that makes the stub answer with a message to a background
/// agent; the agent id follows the marker after one space.
pub(crate) const RESUME_MARKER: &str = "[resume]";

/// Message the resume reply sends. The resumed agent's request carries it in
/// its last message and waits on the resume gate.
pub(crate) const RESUME_MESSAGE: &str = "background follow-up: reply with one word";

/// Name of the tool the resume reply calls; Claude Code resumes a finished
/// background agent that it addresses by id.
pub(crate) const RESUME_TOOL: &str = "SendMessage";

/// Opening of the last message of a prompt-suggestion request, the helper
/// Claude Code runs after a main turn when prompt suggestions are enabled.
pub(crate) const SUGGESTION_OPENING: &str = "[SUGGESTION MODE:";

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

/// Prefix of the `tool_use` id of every tool reply; a serial number follows,
/// so two tool calls of one conversation never share an id.
const TOOL_USE_PREFIX: &str = "toolu_stub_";

/// Prefix of the id of every reply message; a serial number follows. Claude
/// Code joins assistant messages that share an id into one, so a fixed id
/// would fold one turn's reply into an earlier turn's.
const MESSAGE_PREFIX: &str = "msg_stub_";

/// Prefix of the `tool_use` id of a background launch reply, so the request
/// carrying that call's result is recognized by the id its tool result names.
const LAUNCH_TOOL_USE_PREFIX: &str = "toolu_stub_launch_";

/// Prefix of the `tool_use` id of a resume reply, so the request carrying that
/// call's result is recognized by the id its tool result names.
const RESUME_TOOL_USE_PREFIX: &str = "toolu_stub_resume_";

/// A closed-until-opened barrier a response thread waits on.
#[derive(Debug, Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn set(&self, open: bool) {
        *self.open.lock().expect("gate lock") = open;
        self.changed.notify_all();
    }

    fn wait_open(&self) {
        let mut open = self.open.lock().expect("gate lock");
        while !*open {
            open = self.changed.wait(open).expect("gate wait");
        }
    }
}

/// Shared state between the acceptor, the response threads and the test.
#[derive(Debug, Default)]
struct State {
    started: AtomicUsize,
    finished: AtomicUsize,
    unexpected_credential: AtomicBool,
    stopping: AtomicBool,
    start_gate: Gate,
    stream_gate: Gate,
    run_gate: Gate,
    resume_gate: Gate,
    launch_result_gate: Gate,
    resume_result_gate: Gate,
    background_runs: AtomicUsize,
    resumed_runs: AtomicUsize,
    paths: Mutex<Vec<String>>,
    bodies: Mutex<Vec<String>>,
    /// Last-message summaries of the kept bodies, parsed once on receipt.
    endings: Mutex<Vec<Ending>>,
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

    /// The kept request bodies that carry `needle`, oldest first.
    pub(crate) fn bodies_containing(&self, needle: &str) -> Vec<String> {
        self.state
            .bodies
            .lock()
            .expect("bodies lock")
            .iter()
            .filter(|body| body.contains(needle))
            .cloned()
            .collect()
    }

    /// Opens the start gate: every response held before its first text, and
    /// every later one, starts.
    pub(crate) fn open_gate(&self) {
        self.state.start_gate.set(true);
    }

    /// Opens the stream gate: every response held mid-stream, and every later
    /// one, completes.
    pub(crate) fn open_stream_gate(&self) {
        self.state.stream_gate.set(true);
    }

    /// Opens the run gate: the background agent's requests of its first run,
    /// held before any response byte, are answered.
    pub(crate) fn open_run_gate(&self) {
        self.state.run_gate.set(true);
    }

    /// Opens the resume gate: the background agent's requests of the run a
    /// resume started, held before any response byte, are answered.
    pub(crate) fn open_resume_gate(&self) {
        self.state.resume_gate.set(true);
    }

    /// Opens the launch-result gate: the main agent's request that carries the
    /// background launch call's result is answered.
    pub(crate) fn open_launch_result_gate(&self) {
        self.state.launch_result_gate.set(true);
    }

    /// Opens the resume-result gate: the main agent's request that carries the
    /// resume call's result is answered.
    pub(crate) fn open_resume_result_gate(&self) {
        self.state.resume_result_gate.set(true);
    }

    /// Requests of the background agent's first run the stub received.
    pub(crate) fn background_requests(&self) -> usize {
        self.state.background_runs.load(Ordering::SeqCst)
    }

    /// Requests of the background agent's resumed run the stub received.
    pub(crate) fn resumed_requests(&self) -> usize {
        self.state.resumed_runs.load(Ordering::SeqCst)
    }

    /// How many of the most recent message requests end their conversation
    /// with the user prompt `prompt`: a text block of the last message is
    /// exactly `prompt`. Claude Code's title request quotes the prompt inside
    /// a longer text, so it is not counted.
    pub(crate) fn requests_with_prompt(&self, prompt: &str) -> usize {
        self.state
            .endings
            .lock()
            .expect("endings lock")
            .iter()
            .filter(|ending| ending.prompts.iter().any(|text| text == prompt))
            .count()
    }

    /// How many of the most recent message requests carried `needle` in the
    /// last message of their conversation.
    pub(crate) fn last_messages_containing(&self, needle: &str) -> usize {
        self.state
            .endings
            .lock()
            .expect("endings lock")
            .iter()
            .filter(|ending| ending.last.contains(needle))
            .count()
    }
}

impl Drop for MessagesStub {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        self.open_gate();
        self.open_stream_gate();
        self.open_run_gate();
        self.open_resume_gate();
        self.open_launch_result_gate();
        self.open_resume_result_gate();
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
    let serial = state.started.fetch_add(1, Ordering::SeqCst);
    let conversation = Conversation::parse(&request.body);
    keep_body(state, &request.body, &conversation);
    match conversation.hold() {
        Some(Hold::FirstRun) => {
            state.background_runs.fetch_add(1, Ordering::SeqCst);
            state.run_gate.wait_open();
        }
        Some(Hold::ResumedRun) => {
            state.resumed_runs.fetch_add(1, Ordering::SeqCst);
            state.resume_gate.wait_open();
        }
        Some(Hold::LaunchResult) => state.launch_result_gate.wait_open(),
        Some(Hold::ResumeResult) => state.resume_result_gate.wait_open(),
        None => {}
    }
    let reply = reply_for(&request.body, &conversation, serial);
    if request.wants_stream() {
        stream_reply(
            &mut writer,
            state,
            &format!("{MESSAGE_PREFIX}{serial}"),
            &reply,
            &Holds {
                before_text: request.body.contains(HOLD_MARKER),
                mid_stream: request.body.contains(STREAM_HOLD_MARKER),
            },
        )?;
    } else {
        write_json(
            &mut writer,
            &message_json(&format!("{MESSAGE_PREFIX}{serial}"), &reply.blocks()),
        )?;
    }
    writer.flush()?;
    state.finished.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Remembers `body` for [`MessagesStub::requests_containing`] and the ending
/// of its `conversation` for the last-message counters, dropping the oldest
/// of each beyond [`KEPT_BODIES`].
fn keep_body(state: &State, body: &str, conversation: &Conversation) {
    let mut bodies = state.bodies.lock().expect("bodies lock");
    if bodies.len() == KEPT_BODIES {
        bodies.remove(0);
    }
    bodies.push(body.to_owned());
    drop(bodies);
    let mut endings = state.endings.lock().expect("endings lock");
    if endings.len() == KEPT_BODIES {
        endings.remove(0);
    }
    endings.push(Ending {
        last: conversation.last.clone(),
        prompts: conversation.prompts.clone(),
    });
}

/// The last message of one kept request: serialized, and its text blocks.
#[derive(Debug, Default)]
struct Ending {
    last: String,
    prompts: Vec<String>,
}

/// What the stub answers one request with.
enum Reply {
    Text,
    Tool {
        id: String,
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
            Self::Tool { id, name, input } => vec![serde_json::json!({
                "type": "tool_use",
                "id": id,
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

/// The gate a request waits on before any response byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Hold {
    /// A request of the background agent's first run: the run gate.
    FirstRun,
    /// A request of the run a resume message started: the resume gate.
    ResumedRun,
    /// The main agent's request carrying the launch call's result: the
    /// launch-result gate.
    LaunchResult,
    /// The main agent's request carrying the resume call's result: the
    /// resume-result gate.
    ResumeResult,
}

/// The first and last message of a request's conversation, serialized, and
/// the text blocks of the last message; all empty when the body is not a
/// Messages request.
struct Conversation {
    first: String,
    last: String,
    prompts: Vec<String>,
}

impl Conversation {
    fn parse(body: &str) -> Self {
        let parsed = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
        let messages = parsed["messages"].as_array().cloned().unwrap_or_default();
        let text = |message: Option<&serde_json::Value>| {
            message.map(ToString::to_string).unwrap_or_default()
        };
        let prompts = messages
            .last()
            .and_then(|message| message["content"].as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            first: text(messages.first()),
            last: text(messages.last()),
            prompts,
        }
    }

    /// The gate this request waits on. A background agent's conversation
    /// opens with the background task; its resumed run's last message carries
    /// the resume message.
    fn hold(&self) -> Option<Hold> {
        if self.first.contains(BACKGROUND_PROMPT) {
            return Some(if self.last.contains(RESUME_MESSAGE) {
                Hold::ResumedRun
            } else {
                Hold::FirstRun
            });
        }
        if !self.last.contains(TOOL_RESULT_MARKER) {
            return None;
        }
        if self.last.contains(LAUNCH_TOOL_USE_PREFIX) {
            return Some(Hold::LaunchResult);
        }
        self.last
            .contains(RESUME_TOOL_USE_PREFIX)
            .then_some(Hold::ResumeResult)
    }

    /// The tool call a marked user turn asks for: a last message that carries
    /// [`BACKGROUND_MARKER`] or [`RESUME_MARKER`] and no tool result. `serial`
    /// numbers the call's `tool_use` id.
    fn turn_reply(&self, serial: usize) -> Option<Reply> {
        if self.last.contains(TOOL_RESULT_MARKER) {
            return None;
        }
        if self.last.contains(BACKGROUND_MARKER) {
            return Some(Reply::Tool {
                id: format!("{LAUNCH_TOOL_USE_PREFIX}{serial}"),
                name: BACKGROUND_TOOL,
                input: serde_json::json!({
                    "description": "Stub background agent",
                    "prompt": BACKGROUND_PROMPT,
                    "subagent_type": "general-purpose",
                }),
            });
        }
        let target = resume_target(&self.last)?;
        Some(Reply::Tool {
            id: format!("{RESUME_TOOL_USE_PREFIX}{serial}"),
            name: RESUME_TOOL,
            input: serde_json::json!({
                "to": target,
                "message": RESUME_MESSAGE,
                "summary": "Resume the background agent",
            }),
        })
    }
}

/// The agent id after [`RESUME_MARKER`] and one space in `text`.
fn resume_target(text: &str) -> Option<&str> {
    let (_before, after) = text.split_once(RESUME_MARKER)?;
    let id = after.strip_prefix(' ')?;
    let end = id
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(id.len());
    (end > 0).then(|| &id[..end])
}

/// The reply for request number `serial`: a marked last user turn gets its
/// tool call; else a marked prompt without a tool result in the conversation
/// gets its tool call; everything else plain text.
fn reply_for(body: &str, conversation: &Conversation, serial: usize) -> Reply {
    if let Some(reply) = conversation.turn_reply(serial) {
        return reply;
    }
    if body.contains(TOOL_RESULT_MARKER) {
        return Reply::Text;
    }
    let id = format!("{TOOL_USE_PREFIX}{serial}");
    if body.contains(APPROVAL_MARKER) {
        return Reply::Tool {
            id,
            name: "Bash",
            input: serde_json::json!({
                "command": APPROVAL_COMMAND,
                "description": "Create the marker file",
            }),
        };
    }
    if body.contains(QUESTION_MARKER) {
        return Reply::Tool {
            id,
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
            id,
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

fn message_json(id: &str, content: &[serde_json::Value]) -> serde_json::Value {
    let stop_reason = if content.iter().any(|block| block["type"] == "tool_use") {
        "tool_use"
    } else {
        "end_turn"
    };
    serde_json::json!({
        "id": id,
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

/// Where a streamed text reply waits for a gate.
struct Holds {
    /// Before the first chunk, on the start gate.
    before_text: bool,
    /// After the first chunk, on the stream gate.
    mid_stream: bool,
}

/// Streams `reply` as Messages events of message `message_id`; a held text
/// reply waits on its gates.
fn stream_reply(
    writer: &mut TcpStream,
    state: &State,
    message_id: &str,
    reply: &Reply,
    holds: &Holds,
) -> io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    let mut start = message_json(message_id, &[]);
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
            if holds.before_text {
                state.start_gate.wait_open();
            }
            send_text_delta(writer, &format!("{FIRST_CHUNK}{CHUNK_BREAK}"))?;
            if holds.mid_stream {
                state.stream_gate.wait_open();
            }
            send_text_delta(writer, SECOND_CHUNK)?;
        }
        Reply::Tool { id, name, input } => {
            send_event(
                writer,
                "content_block_start",
                &serde_json::json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
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
