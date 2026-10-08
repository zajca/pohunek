//! A loopback chat-completions endpoint for tests that drive a real agent CLI
//! without a model provider.
//!
//! The stub speaks the streaming `OpenAI` chat-completions wire shape on an
//! ephemeral IPv4 loopback port and answers every request with one fixed
//! assistant message. Each response sends its first chunk at once and the rest
//! only after the test opens the gate, so a test can hold an agent in its
//! "working" state for as long as it needs and release it deterministically.
//! It never sees a credential: the agent is configured with a placeholder key
//! that only has to exist.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-04

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Model id the stub serves.
pub(crate) const MODEL_ID: &str = "stub-model";

/// Text of the first streamed chunk, visible while the response is held.
pub(crate) const FIRST_CHUNK: &str = "working";

/// Text of the chunk sent when the gate opens.
pub(crate) const SECOND_CHUNK: &str = " done";

/// Largest request body the stub drains, in bytes.
///
/// Agent requests carry the system prompt and tool declarations, a few tens of
/// kilobytes; the bound keeps a misbehaving client from growing memory.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Largest request header block the stub reads, in bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Shared state between the acceptor, the response threads and the test.
#[derive(Debug, Default)]
struct State {
    started: AtomicUsize,
    finished: AtomicUsize,
    stopping: AtomicBool,
    gate_open: Mutex<bool>,
    gate_changed: Condvar,
    bodies: Mutex<Vec<String>>,
}

/// Most recent request bodies the stub keeps for [`ModelStub::bodies_containing`].
///
/// Bounded so a long session does not grow memory; tests inspect requests that
/// are at most a few turns old.
const KEPT_BODIES: usize = 64;

/// A running stub endpoint.
#[derive(Debug)]
pub(crate) struct ModelStub {
    address: SocketAddr,
    state: Arc<State>,
    acceptor: Option<JoinHandle<()>>,
}

impl ModelStub {
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

    /// The `baseUrl` an agent's provider configuration names.
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

    /// Lets every held response, and every later one, complete.
    pub(crate) fn open_gate(&self) {
        *self.state.gate_open.lock().expect("gate lock") = true;
        self.state.gate_changed.notify_all();
    }

    /// Holds the second chunk of every response that starts from now on.
    pub(crate) fn close_gate(&self) {
        *self.state.gate_open.lock().expect("gate lock") = false;
    }
}

impl Drop for ModelStub {
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

fn respond(stream: TcpStream, state: &State) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let body = read_request(&mut reader)?;
    keep_body(state, &body);
    state.started.fetch_add(1, Ordering::SeqCst);
    writer.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    send_chunk(&mut writer, &delta(Some("assistant"), FIRST_CHUNK))?;
    let mut open = state.gate_open.lock().expect("gate lock");
    while !*open {
        open = state.gate_changed.wait(open).expect("gate wait");
    }
    drop(open);
    send_chunk(&mut writer, &delta(None, SECOND_CHUNK))?;
    send_chunk(&mut writer, &finish())?;
    writer.write_all(b"data: [DONE]\n\n")?;
    writer.flush()?;
    state.finished.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn keep_body(state: &State, body: &[u8]) {
    let mut bodies = state.bodies.lock().expect("bodies lock");
    if bodies.len() == KEPT_BODIES {
        bodies.remove(0);
    }
    bodies.push(String::from_utf8_lossy(body).into_owned());
}

/// Reads one HTTP request (headers and body) and returns the body.
fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let mut content_length = 0_usize;
    let mut chunked = false;
    let mut header_bytes = 0_usize;
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
    if chunked {
        read_chunked_body(reader)
    } else if content_length > MAX_BODY_BYTES {
        Err(io::ErrorKind::InvalidData.into())
    } else {
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body)?;
        Ok(body)
    }
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

fn send_chunk(writer: &mut TcpStream, payload: &str) -> io::Result<()> {
    writer.write_all(format!("data: {payload}\n\n").as_bytes())?;
    writer.flush()
}

fn delta(role: Option<&str>, content: &str) -> String {
    let mut delta = serde_json::json!({ "content": content });
    if let Some(role) = role {
        delta["role"] = serde_json::Value::String(role.to_owned());
    }
    serde_json::json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": MODEL_ID,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": null }],
    })
    .to_string()
}

fn finish() -> String {
    serde_json::json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": MODEL_ID,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 },
    })
    .to_string()
}
