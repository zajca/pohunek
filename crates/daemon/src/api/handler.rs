//! Control-method dispatch.
//!
//! Parses a newline-delimited JSON request line into a [`protocol::Request`],
//! negotiates the protocol version, dispatches to the method handler, and
//! serializes a [`protocol::Response`] back to a single line.
//!
//! Milestone 2 handles `daemon.health` only. Unknown methods get a typed
//! `method_not_found` error so older daemons degrade predictably as the CLI
//! gains methods.

use protocol::{
    method, negotiate, ProtocolError, Request, Response, PROTOCOL_VERSION,
};
use serde_json::json;
use tracing::{debug, warn};

/// Static facts the daemon reports from `daemon.health`.
///
/// Cloned into each connection task. Cheap to clone (two short strings).
#[derive(Debug, Clone)]
pub struct HealthInfo {
    /// Daemon build version (e.g. crate version).
    pub daemon_version: String,
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

/// Parse one request line, dispatch it, and return the response line to write.
///
/// Never panics and never returns an error: malformed input and version
/// mismatches are turned into typed error responses so the connection can stay
/// open for the next request.
pub(crate) fn dispatch_line(line: &str, health: &HealthInfo) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Tolerate blank keep-alive lines; reply with a framing error tied to a
        // synthetic id so the client still gets a parseable line.
        let resp = Response::err("", ProtocolError::bad_request("empty request line"));
        return serialize_response(&resp);
    }

    let request: Request = match serde_json::from_str(trimmed) {
        Ok(req) => req,
        Err(err) => {
            warn!(error = %err, "failed to parse control request");
            // We cannot recover the request id from unparseable JSON; use empty.
            let resp =
                Response::err("", ProtocolError::bad_request(format!("invalid request JSON: {err}")));
            return serialize_response(&resp);
        }
    };

    let resp = handle_request(&request, health);
    serialize_response(&resp)
}

/// Dispatch a parsed request to its method handler.
///
/// Exposed within the crate (and re-exported) so integration tests can exercise
/// dispatch without a live socket.
#[must_use]
pub fn handle_request(request: &Request, health: &HealthInfo) -> Response {
    debug!(id = %request.id, method = %request.method, "control request");

    // Version negotiation first: an incompatible client gets a typed error
    // rather than a confusingly-shaped success.
    if let Err(err) = negotiate(request.v, PROTOCOL_VERSION) {
        return Response::err(request.id.clone(), err);
    }

    match request.method.as_str() {
        method::DAEMON_HEALTH => handle_health(request, health),
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
