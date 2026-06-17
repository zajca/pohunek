//! Control-method dispatch.
//!
//! Parses a newline-delimited JSON request line into a [`protocol::Request`],
//! negotiates the protocol version, dispatches to the method handler, and
//! serializes a [`protocol::Response`] back to a single line.
//!
//! Handles `daemon.health` (milestone 2) and the `session.*` lifecycle methods
//! (milestone 3); a `subscribe` request is dispatched specially so the caller can
//! turn the connection into a one-way event stream. Unknown methods get a typed
//! `method_not_found` error so older daemons degrade predictably as the CLI gains
//! methods.

use protocol::{
    method, negotiate, ProtocolError, Request, Response, SessionId, SessionNewParams,
    PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::session::SessionRegistry;

/// Static facts the daemon reports from `daemon.health`.
///
/// Cloned into each connection task. Cheap to clone (two short strings).
#[derive(Debug, Clone)]
pub struct HealthInfo {
    /// Daemon build version (e.g. crate version).
    pub daemon_version: String,
}

/// Shared daemon state available to every control connection.
#[derive(Debug, Clone)]
pub struct DaemonState {
    /// Static health metadata.
    pub health: HealthInfo,
    /// In-memory session registry.
    pub sessions: SessionRegistry,
}

impl DaemonState {
    /// Construct shared daemon state.
    #[must_use]
    pub fn new(health: HealthInfo, sessions: SessionRegistry) -> Self {
        Self { health, sessions }
    }
}

impl HealthInfo {
    /// Construct health info from a daemon version string.
    #[must_use]
    pub fn new(daemon_version: impl Into<String>) -> Self {
        Self {
            daemon_version: daemon_version.into(),
        }
    }
}

/// Outcome of dispatching one request line.
///
/// Most requests are one-shot (`Reply`), but a `subscribe` request asks the
/// connection to become a one-way event stream after an OK ack (`Subscribe`).
#[derive(Debug)]
pub(crate) enum Dispatch {
    /// One-shot: send this response line, then keep reading requests.
    Reply(String),
    /// The client asked to subscribe; send this OK ack line, then the caller
    /// streams session events on this connection until the client disconnects.
    Subscribe(String),
}

/// Parse one request line and decide how the connection should proceed.
///
/// Never panics and never returns an error: malformed input and version
/// mismatches are turned into typed error responses (`Reply`) so the connection
/// can stay open for the next request. A valid `subscribe` request yields
/// `Subscribe` with the OK ack line for the caller to write before streaming.
pub(crate) async fn dispatch_line(line: &str, state: &DaemonState) -> Dispatch {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Tolerate blank keep-alive lines; reply with a framing error tied to a
        // synthetic id so the client still gets a parseable line.
        let resp = Response::err("", ProtocolError::bad_request("empty request line"));
        return Dispatch::Reply(serialize_response(&resp));
    }

    let request: Request = match serde_json::from_str(trimmed) {
        Ok(req) => req,
        Err(err) => {
            warn!(error = %err, "failed to parse control request");
            // We cannot recover the request id from unparseable JSON; use empty.
            let resp = Response::err(
                "",
                ProtocolError::bad_request(format!("invalid request JSON: {err}")),
            );
            return Dispatch::Reply(serialize_response(&resp));
        }
    };

    // Version negotiation first, before treating `subscribe` specially: an
    // incompatible client gets a typed error rather than a long-lived stream.
    if let Err(err) = negotiate(request.v, PROTOCOL_VERSION) {
        let resp = Response::err(request.id.clone(), err);
        return Dispatch::Reply(serialize_response(&resp));
    }

    if request.method == method::SUBSCRIBE {
        let ack = Response::ok(request.id.clone(), json!({ "subscribed": true }));
        return Dispatch::Subscribe(serialize_response(&ack));
    }

    let resp = handle_request(&request, state).await;
    Dispatch::Reply(serialize_response(&resp))
}

/// Dispatch a parsed request to its method handler.
///
/// Exposed within the crate (and re-exported) so integration tests can exercise
/// dispatch without a live socket.
#[must_use]
pub async fn handle_request(request: &Request, state: &DaemonState) -> Response {
    debug!(id = %request.id, method = %request.method, "control request");

    // Version negotiation first: an incompatible client gets a typed error
    // rather than a confusingly-shaped success.
    if let Err(err) = negotiate(request.v, PROTOCOL_VERSION) {
        return Response::err(request.id.clone(), err);
    }

    match request.method.as_str() {
        method::DAEMON_HEALTH => handle_health(request, &state.health),
        method::SESSION_NEW => handle_session_new(request, &state.sessions).await,
        method::SESSION_LIST => handle_session_list(request, &state.sessions).await,
        method::SESSION_INSPECT => handle_session_inspect(request, &state.sessions).await,
        method::SESSION_STOP => handle_session_stop(request, &state.sessions).await,
        other => Response::err(request.id.clone(), ProtocolError::method_not_found(other)),
    }
}

/// `daemon.health`: report daemon version + protocol version.
fn handle_health(request: &Request, health: &HealthInfo) -> Response {
    Response::ok(
        request.id.clone(),
        json!({
            "status": "ok",
            "daemon_version": health.daemon_version,
            "protocol_version": PROTOCOL_VERSION,
        }),
    )
}

async fn handle_session_new(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionNewParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.create(params).await {
        Ok(info) => ok_value(request, &info),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_list(request: &Request, sessions: &SessionRegistry) -> Response {
    let list = sessions.list().await;
    ok_value(request, &list)
}

async fn handle_session_inspect(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.inspect(&id).await {
        Ok(info) => ok_value(request, &info),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_stop(request: &Request, sessions: &SessionRegistry) -> Response {
    let id = match parse_params::<SessionId>(request) {
        Ok(id) => id,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.stop(&id).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

fn parse_params<T>(request: &Request) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value::<T>(request.params.clone()).map_err(|err| {
        ProtocolError::bad_request(format!("invalid params for {}: {err}", request.method))
    })
}

fn ok_value<T>(request: &Request, value: &T) -> Response
where
    T: Serialize,
{
    match serde_json::to_value(value) {
        Ok(value) => Response::ok(request.id.clone(), value),
        Err(err) => Response::err(
            request.id.clone(),
            ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "serialize_failed",
                format!("failed to serialize response: {err}"),
                None,
            ),
        ),
    }
}

/// Serialize a response to a single JSON line.
///
/// Serialization of our own typed envelopes cannot fail in practice; if it ever
/// did we fall back to a minimal hand-built error line rather than panicking.
fn serialize_response(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|err| {
        warn!(error = %err, "failed to serialize response; sending fallback error");
        format!(
            r#"{{"v":{},"id":"{}","err":{{"class":"daemon","code":"serialize_failed","msg":"response serialization failed"}}}}"#,
            PROTOCOL_VERSION.get(),
            resp.id().replace('"', "")
        )
    })
}
