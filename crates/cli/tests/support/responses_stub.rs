//! A loopback `OpenAI` Responses endpoint for tests that drive a real Codex
//! without a model provider.
//!
//! The stub serves `POST /v1/responses` as a server-sent event stream on an
//! ephemeral IPv4 loopback port. A request whose body carries [`HOLD_MARKER`]
//! gets its first text chunk at once and the rest only after the test opens the
//! gate, so a test can hold the agent in its working state and release it
//! deterministically. A request carrying [`APPROVAL_MARKER`] is answered with a
//! shell tool call that needs approval, until the conversation holds a tool
//! result. Every other request gets a complete text reply.
//!
//! A prompt carrying `[flow:NAME]` (see [`FLOWS`]) scripts a multi-agent
//! conversation: the root agent answers each of its requests with the next tool
//! call of the flow, counted by the tool results in its conversation, and every
//! other request, a child agent's included, gets a text reply. A child whose
//! task message carries [`CHILD_HOLD_MARKER`] is held until the gate opens.
//!
//! The stub never sees a credential: it records whether any request carried an
//! `Authorization` header, and a test asserts that none did.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-05

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Model id the stub serves.
pub(crate) const MODEL_ID: &str = "stub-model";

/// Text streamed before the gate, visible while the response is held.
pub(crate) const FIRST_CHUNK: &str = "working";

/// Text streamed after the gate opens.
pub(crate) const SECOND_CHUNK: &str = " done";

/// Prompt text that makes the stub hold the reply until the gate opens.
pub(crate) const HOLD_MARKER: &str = "[hold]";

/// Prompt text that makes the stub hold the response before its first event
/// (`response.created`) until the gate opens, so the agent has sent the turn
/// and the server has not acknowledged it.
pub(crate) const PRE_ACK_HOLD_MARKER: &str = "[preack]";

/// Prompt text that makes the stub answer with a command that needs approval.
pub(crate) const APPROVAL_MARKER: &str = "[approval]";

/// Shell command the approval reply asks to run.
pub(crate) const APPROVAL_COMMAND: &str = "touch approved.txt";

/// Prompt text that makes the stub answer with a command that keeps running
/// after Codex stops waiting for it: a background terminal.
pub(crate) const BACKGROUND_MARKER: &str = "[background]";

/// Shell command the background reply starts.
pub(crate) const BACKGROUND_COMMAND: &str = "sleep 3600";

/// How long Codex waits for the background command before it returns the turn
/// to the model, in milliseconds.
const BACKGROUND_YIELD_MS: u64 = 1000;

/// Prefix of the prompt text that selects a scripted multi-agent flow.
pub(crate) const FLOW_PREFIX: &str = "[flow:";

/// Task text that makes the stub hold a child agent's reply until the gate
/// opens, so the child's turn is running while the root acts on it.
pub(crate) const CHILD_HOLD_MARKER: &str = "[child-hold]";

/// Task text of a child agent that answers at once.
pub(crate) const CHILD_TASK: &str = "[child-task]";

/// Task name of the child in the flows of the `multi_agent_v2` tools.
const V2_TASK_NAME: &str = "worker_one";

/// Timeout the flows give a `wait_agent`, in milliseconds: the hang guard, since
/// every flow waits only for a child that finishes.
fn wait_agent_ms() -> u64 {
    u64::try_from(pohunek_test_support::wait::HANG_GUARD.as_millis()).unwrap_or(u64::MAX)
}

/// A flow step: the tool namespace, the tool and its arguments, where the
/// string `$ID` stands for the child's agent id (its task name in the
/// `multi_agent_v2` flows).
type Step = (&'static str, &'static str, fn() -> serde_json::Value);

/// The scripted flows: the name in `[flow:NAME]` and the root's tool calls in
/// order.
pub(crate) const FLOW_NAMES: [&str; 7] = [
    "spawn_complete",
    "followup_v1",
    "followup_v2",
    "resume_after_close",
    "interrupted_v1",
    "interrupted_v2",
    "close_running_v1",
];

fn flow_steps(name: &str) -> &'static [Step] {
    const V1: &str = "multi_agent_v1";
    const V2: &str = "collaboration";
    fn spawn_v1(message: &str) -> serde_json::Value {
        serde_json::json!({ "message": message })
    }
    fn spawn_v2(message: &str) -> serde_json::Value {
        serde_json::json!({ "task_name": V2_TASK_NAME, "message": message, "fork_turns": "none" })
    }
    fn wait_v1() -> serde_json::Value {
        serde_json::json!({ "targets": ["$ID"], "timeout_ms": wait_agent_ms() })
    }
    fn wait_v2() -> serde_json::Value {
        serde_json::json!({ "timeout_ms": wait_agent_ms() })
    }
    match name {
        "spawn_complete" => &[
            (V1, "spawn_agent", || spawn_v1(&format!("{CHILD_TASK} one"))),
            (V1, "wait_agent", wait_v1),
        ],
        "followup_v1" => &[
            (V1, "spawn_agent", || spawn_v1(&format!("{CHILD_TASK} one"))),
            (V1, "wait_agent", wait_v1),
            (
                V1,
                "send_input",
                || serde_json::json!({ "target": "$ID", "message": format!("{CHILD_TASK} two") }),
            ),
            (V1, "wait_agent", wait_v1),
        ],
        "followup_v2" => &[
            (V2, "spawn_agent", || spawn_v2(&format!("{CHILD_TASK} one"))),
            (V2, "wait_agent", wait_v2),
            (
                V2,
                "followup_task",
                || serde_json::json!({ "target": "$ID", "message": format!("{CHILD_TASK} two") }),
            ),
            (V2, "wait_agent", wait_v2),
        ],
        "resume_after_close" => &[
            (V1, "spawn_agent", || spawn_v1(&format!("{CHILD_TASK} one"))),
            (V1, "wait_agent", wait_v1),
            (V1, "close_agent", || serde_json::json!({ "target": "$ID" })),
            (V1, "resume_agent", || serde_json::json!({ "id": "$ID" })),
            (
                V1,
                "send_input",
                || serde_json::json!({ "target": "$ID", "message": format!("{CHILD_TASK} two") }),
            ),
            (V1, "wait_agent", wait_v1),
        ],
        "interrupted_v1" => &[
            (V1, "spawn_agent", || {
                spawn_v1(&format!("{CHILD_HOLD_MARKER} one"))
            }),
            (V1, "send_input", || {
                serde_json::json!({
                    "target": "$ID",
                    "message": format!("{CHILD_TASK} two"),
                    "interrupt": true,
                })
            }),
            (V1, "wait_agent", wait_v1),
        ],
        "interrupted_v2" => &[
            (V2, "spawn_agent", || {
                spawn_v2(&format!("{CHILD_HOLD_MARKER} one"))
            }),
            (
                V2,
                "interrupt_agent",
                || serde_json::json!({ "target": "$ID" }),
            ),
            (V2, "list_agents", || serde_json::json!({})),
        ],
        "close_running_v1" => &[
            (V1, "spawn_agent", || {
                spawn_v1(&format!("{CHILD_HOLD_MARKER} one"))
            }),
            (V1, "close_agent", || serde_json::json!({ "target": "$ID" })),
            (V1, "wait_agent", wait_v1),
        ],
        _ => &[],
    }
}

/// Largest request body the stub reads, in bytes.
///
/// Agent requests carry the system prompt, tool declarations and the
/// conversation, a few tens of kilobytes; the bound keeps a misbehaving client
/// from growing memory.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Largest request header block the stub reads, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Substring of a request body that marks a tool result in the conversation.
const TOOL_RESULT_MARKER: &str = "function_call_output";

/// Shared state between the acceptor, the response threads and the test.
#[derive(Debug, Default)]
struct State {
    started: AtomicUsize,
    finished: AtomicUsize,
    authorized: AtomicBool,
    stopping: AtomicBool,
    gate_open: Mutex<bool>,
    gate_changed: Condvar,
    held_started: Mutex<usize>,
    held_changed: Condvar,
    bodies: Mutex<Vec<String>>,
}

/// Most recent Responses request bodies the stub keeps for
/// [`ResponsesStub::bodies_containing`].
///
/// Bounded so a long session does not grow memory; tests inspect requests that
/// are at most a few turns old.
const KEPT_BODIES: usize = 64;

/// A running stub endpoint.
#[derive(Debug)]
pub(crate) struct ResponsesStub {
    address: SocketAddr,
    state: Arc<State>,
    acceptor: Option<JoinHandle<()>>,
}

impl ResponsesStub {
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

    /// The `base_url` a Codex model provider names.
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// Requests whose response has begun.
    pub(crate) fn started(&self) -> usize {
        self.state.started.load(Ordering::SeqCst)
    }

    /// Requests whose response completed.
    pub(crate) fn finished(&self) -> usize {
        self.state.finished.load(Ordering::SeqCst)
    }

    /// Whether any request carried an `Authorization` header.
    pub(crate) fn saw_credential(&self) -> bool {
        self.state.authorized.load(Ordering::SeqCst)
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

    /// The outputs of the tool calls in the newest request of the root agent of
    /// a flow, oldest first: once the script is done, the output of every call
    /// it made.
    pub(crate) fn flow_tool_outputs(&self) -> Vec<String> {
        let bodies = self.state.bodies.lock().expect("bodies lock");
        let root = bodies.iter().rev().find_map(|body| {
            let request: serde_json::Value = serde_json::from_str(body).ok()?;
            user_messages(&request)
                .iter()
                .any(|message| message.contains(FLOW_PREFIX))
                .then_some(request)
        });
        root.and_then(|request| {
            request["input"].as_array().map(|input| {
                input
                    .iter()
                    .filter(|item| item["type"] == "function_call_output")
                    .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
                    .collect()
            })
        })
        .unwrap_or_default()
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

impl Drop for ResponsesStub {
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

/// One parsed request: whether it is a Responses call and what it carried.
struct Request {
    is_responses_call: bool,
    body: String,
}

fn respond(stream: TcpStream, state: &State) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let request = read_request(&mut reader, state)?;
    if !request.is_responses_call {
        return writer.write_all(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    }
    keep_body(state, &request.body);
    state.started.fetch_add(1, Ordering::SeqCst);
    writer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    if request.body.contains(PRE_ACK_HOLD_MARKER) {
        wait_for_gate(state);
    }
    send_event(
        &mut writer,
        "response.created",
        &serde_json::json!({ "type": "response.created", "response": { "id": "resp_stub" } }),
    )?;
    let no_tool_result = !request.body.contains(TOOL_RESULT_MARKER);
    let flow = flow_reply(&request.body);
    if let FlowReply::Call(call) = &flow {
        // A flow that holds its child acts on the child only once the child's
        // turn is running.
        if request.body.contains(CHILD_HOLD_MARKER) {
            wait_for_held_child(state);
        }
        send_function_call(&mut writer, call)?;
    } else if let FlowReply::HeldText = flow {
        send_text(&mut writer, state, true)?;
    } else if matches!(flow, FlowReply::Text) {
        send_text(&mut writer, state, false)?;
    } else if request.body.contains(APPROVAL_MARKER) && no_tool_result {
        send_approval_command(&mut writer)?;
    } else if request.body.contains(BACKGROUND_MARKER) && no_tool_result {
        send_background_command(&mut writer)?;
    } else {
        send_text(&mut writer, state, request.body.contains(HOLD_MARKER))?;
    }
    send_event(
        &mut writer,
        "response.completed",
        &serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_stub",
                "usage": {
                    "input_tokens": 1,
                    "input_tokens_details": null,
                    "output_tokens": 1,
                    "output_tokens_details": null,
                    "total_tokens": 2,
                },
            },
        }),
    )?;
    writer.flush()?;
    state.finished.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// What a request of a scripted flow gets.
enum FlowReply {
    /// The request is not part of a flow.
    NotAFlow,
    /// The next tool call of the root agent's script.
    Call(serde_json::Value),
    /// A child agent's reply, held until the gate opens.
    HeldText,
    /// A text reply: a child's task, or the root once its script is done.
    Text,
}

/// The text of the user messages in a Responses request body.
fn user_messages(request: &serde_json::Value) -> Vec<String> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .map(|item| {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// The first agent id (a UUID) in `text`.
fn first_agent_id(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(35)).find_map(|start| {
        let candidate = text.get(start..start + 36)?;
        let shape_ok = candidate.char_indices().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        });
        shape_ok.then(|| candidate.to_owned())
    })
}

/// Decides the reply of a flow request: the root (a conversation whose user
/// messages name a flow) gets its next scripted call, a child whose task
/// carries the hold marker is held, anything else gets text.
fn flow_reply(body: &str) -> FlowReply {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
        return FlowReply::NotAFlow;
    };
    let messages = user_messages(&request);
    let flow = messages.iter().find_map(|message| {
        let start = message.find(FLOW_PREFIX)? + FLOW_PREFIX.len();
        let end = message[start..].find(']')?;
        Some(message[start..start + end].to_owned())
    });
    let Some(flow) = flow else {
        let last = request["input"]
            .as_array()
            .and_then(|input| input.last())
            .map(ToString::to_string)
            .unwrap_or_default();
        return if last.contains(CHILD_HOLD_MARKER) {
            FlowReply::HeldText
        } else {
            FlowReply::NotAFlow
        };
    };
    let outputs: Vec<&str> = request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or(""))
        .collect();
    let steps = flow_steps(&flow);
    let Some((namespace, tool, arguments)) = steps.get(outputs.len()) else {
        return FlowReply::Text;
    };
    let target = if *namespace == "collaboration" {
        Some(V2_TASK_NAME.to_owned())
    } else {
        outputs.first().and_then(|output| first_agent_id(output))
    };
    // A step that names the child cannot be built before the child exists; the
    // flow then ends with text and the test fails on the missing evidence.
    let arguments = arguments().to_string();
    let arguments = match target {
        Some(target) => arguments.replace("$ID", &target),
        None if arguments.contains("$ID") => return FlowReply::Text,
        None => arguments,
    };
    FlowReply::Call(serde_json::json!({
        "type": "function_call",
        "id": format!("fc_stub_{}", outputs.len()),
        "call_id": format!("call_stub_{}", outputs.len()),
        "namespace": namespace,
        "name": tool,
        "arguments": arguments,
    }))
}

/// Streams one tool call of a flow.
fn send_function_call(writer: &mut TcpStream, call: &serde_json::Value) -> io::Result<()> {
    for name in ["response.output_item.added", "response.output_item.done"] {
        send_event(
            writer,
            name,
            &serde_json::json!({ "type": name, "output_index": 0, "item": call }),
        )?;
    }
    Ok(())
}

fn keep_body(state: &State, body: &str) {
    let mut bodies = state.bodies.lock().expect("bodies lock");
    if bodies.len() == KEPT_BODIES {
        bodies.remove(0);
    }
    bodies.push(body.to_owned());
}

/// Blocks until a held reply has started streaming.
fn wait_for_held_child(state: &State) {
    let mut started = state.held_started.lock().expect("held lock");
    while *started == 0 {
        started = state.held_changed.wait(started).expect("held wait");
    }
}

/// Blocks until the gate is open.
fn wait_for_gate(state: &State) {
    let mut open = state.gate_open.lock().expect("gate lock");
    while !*open {
        open = state.gate_changed.wait(open).expect("gate wait");
    }
}

/// Streams a text reply; a held reply waits for the gate after its first chunk.
fn send_text(writer: &mut TcpStream, state: &State, held: bool) -> io::Result<()> {
    let item = |text: &str| {
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "id": "msg_stub",
            "content": [{ "type": "output_text", "text": text }],
        })
    };
    send_event(
        writer,
        "response.output_item.added",
        &serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "type": "message", "role": "assistant", "id": "msg_stub", "content": [] },
        }),
    )?;
    send_delta(writer, FIRST_CHUNK)?;
    if held {
        *state.held_started.lock().expect("held lock") += 1;
        state.held_changed.notify_all();
        wait_for_gate(state);
    }
    send_delta(writer, SECOND_CHUNK)?;
    send_event(
        writer,
        "response.output_item.done",
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": item(&format!("{FIRST_CHUNK}{SECOND_CHUNK}")),
        }),
    )
}

fn send_delta(writer: &mut TcpStream, text: &str) -> io::Result<()> {
    send_event(
        writer,
        "response.output_text.delta",
        &serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg_stub",
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        }),
    )
}

/// Streams a shell tool call that asks for an unsandboxed run, which Codex
/// answers with an approval prompt when its approval policy is `on-request`.
fn send_approval_command(writer: &mut TcpStream) -> io::Result<()> {
    let call = serde_json::json!({
        "type": "function_call",
        "id": "fc_stub",
        "call_id": "call_stub",
        "name": "exec_command",
        "arguments": serde_json::json!({
            "cmd": APPROVAL_COMMAND,
            "sandbox_permissions": "require_escalated",
            "justification": "Create the marker file",
        })
        .to_string(),
    });
    for name in ["response.output_item.added", "response.output_item.done"] {
        send_event(
            writer,
            name,
            &serde_json::json!({ "type": name, "output_index": 0, "item": call }),
        )?;
    }
    Ok(())
}

/// Streams a shell tool call whose command outlives the wait Codex gives it, so
/// Codex keeps it as a background terminal after the turn.
fn send_background_command(writer: &mut TcpStream) -> io::Result<()> {
    let call = serde_json::json!({
        "type": "function_call",
        "id": "fc_stub",
        "call_id": "call_stub",
        "name": "exec_command",
        "arguments": serde_json::json!({
            "cmd": BACKGROUND_COMMAND,
            "yield_time_ms": BACKGROUND_YIELD_MS,
        })
        .to_string(),
    });
    for name in ["response.output_item.added", "response.output_item.done"] {
        send_event(
            writer,
            name,
            &serde_json::json!({ "type": name, "output_index": 0, "item": call }),
        )?;
    }
    Ok(())
}

fn send_event(writer: &mut TcpStream, name: &str, payload: &serde_json::Value) -> io::Result<()> {
    writer.write_all(format!("event: {name}\ndata: {payload}\n\n").as_bytes())?;
    writer.flush()
}

/// Reads one HTTP request, noting any credential header, and returns its body.
fn read_request(reader: &mut BufReader<TcpStream>, state: &State) -> io::Result<Request> {
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let is_responses_call = request_line.starts_with("POST ")
        && request_line
            .split_whitespace()
            .nth(1)
            .is_some_and(|path| path.ends_with("/responses"));
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
        let line = line.trim_end().to_ascii_lowercase();
        if line.is_empty() {
            break;
        }
        if line.starts_with("authorization:") {
            state.authorized.store(true, Ordering::SeqCst);
        }
        if let Some(value) = line.strip_prefix("content-length:") {
            content_length = value
                .trim()
                .parse()
                .map_err(|_error| io::Error::from(io::ErrorKind::InvalidData))?;
        }
        if line.starts_with("transfer-encoding:") && line.contains("chunked") {
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
        is_responses_call,
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
