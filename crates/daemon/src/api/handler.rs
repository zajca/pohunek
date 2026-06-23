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
    method, negotiate, HostDiscoverParams, IntegrationInstallParams, ProtocolError, Request,
    Response, SessionAttachParams, SessionDetachParams, SessionId, SessionInputParams,
    SessionListParams, SessionNewParams, SessionNewResult, SessionReportNativeIdParams,
    SessionResizeParams, PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::discovery::DiscoveryCache;
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
    /// TTL-cached NetBird host discovery, shared across connections.
    pub discovery: DiscoveryCache,
}

impl DaemonState {
    /// Construct shared daemon state.
    #[must_use]
    pub fn new(health: HealthInfo, sessions: SessionRegistry) -> Self {
        Self {
            health,
            sessions,
            discovery: DiscoveryCache::default(),
        }
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
    /// The client sent an attach prelude; the caller switches to raw PTY bytes.
    Attach(String),
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

    if let Some(stream_id) = parse_attach_prelude(trimmed) {
        return Dispatch::Attach(stream_id);
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
        method::SESSION_ATTACH => handle_session_attach(request, &state.sessions).await,
        method::SESSION_DETACH => handle_session_detach(request, &state.sessions).await,
        method::SESSION_RESIZE => handle_session_resize(request, &state.sessions).await,
        method::SESSION_INPUT => handle_session_input(request, &state.sessions).await,
        method::SESSION_REPORT_NATIVE_ID => {
            handle_session_report_native_id(request, &state.sessions).await
        }
        method::INTEGRATION_INSTALL => handle_integration_install(request),
        method::HOST_INSPECT => handle_host_inspect(request, &state.health),
        method::HOST_DISCOVER => handle_host_discover(request, &state.discovery).await,
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

/// `host.inspect`: report this host's live capability snapshot.
///
/// The snapshot is built fresh on each request (agent runtimes are probed
/// against `PATH`), so it always reflects the host as it is now. Transport
/// agnostic: the same handler answers over the local Unix socket and over a
/// NetBird TCP connection.
fn handle_host_inspect(request: &Request, health: &HealthInfo) -> Response {
    ok_value(
        request,
        &crate::capabilities::host_capabilities(&health.daemon_version),
    )
}

/// `host.discover`: enumerate NetBird peers and classify each daemon.
///
/// The probe is run inside the daemon and cached for a short TTL (see
/// [`DiscoveryCache`]), so repeated calls — e.g. every launcher keypress —
/// return the cached snapshot instantly; `force` bypasses the cache and
/// re-probes now. A NetBird-state failure surfaces as a typed
/// `discovery/netbird_state_unavailable` error rather than an empty result.
async fn handle_host_discover(request: &Request, discovery: &DiscoveryCache) -> Response {
    let params = match parse_optional_params::<HostDiscoverParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match discovery.records(params.force).await {
        Ok(records) => ok_value(request, &records),
        Err(err) => Response::err(
            request.id.clone(),
            ProtocolError::netbird_state_unavailable(err.to_string()),
        ),
    }
}

async fn handle_session_new(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionNewParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    // `create` only returns `Ok` after a requested initial input was injected
    // (it rolls back and errors otherwise), so a successful create with input
    // set means the input was applied. Echoing this lets a client detect an
    // older daemon that silently ignored `input` (which returns no flag).
    let requested_input = params.input.is_some();
    match sessions.create(params).await {
        Ok(session) => {
            let result = SessionNewResult {
                session,
                applied_input: requested_input.then_some(true),
            };
            ok_value(request, &result)
        }
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_list(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_optional_params::<SessionListParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let mut list = sessions.list().await;
    if !params.filters.is_empty() {
        list.retain(|session| params.filters.iter().all(|filter| filter.matches(session)));
    }
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

async fn handle_session_attach(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionAttachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.attach(&params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_detach(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionDetachParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.detach(&params.stream_id).await;
    ok_value(request, &result)
}

async fn handle_session_resize(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionResizeParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions
        .resize(&params.session_id, params.cols, params.rows)
        .await
    {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_input(request: &Request, sessions: &SessionRegistry) -> Response {
    let params = match parse_params::<SessionInputParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match sessions.input(params).await {
        Ok(result) => ok_value(request, &result),
        Err(err) => Response::err(request.id.clone(), err),
    }
}

async fn handle_session_report_native_id(
    request: &Request,
    sessions: &SessionRegistry,
) -> Response {
    let params = match parse_params::<SessionReportNativeIdParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    let result = sessions.report_native_id(params).await;
    ok_value(request, &result)
}

fn handle_integration_install(request: &Request) -> Response {
    let params = match parse_params::<IntegrationInstallParams>(request) {
        Ok(params) => params,
        Err(err) => return Response::err(request.id.clone(), err),
    };
    match crate::integration::install(params.agent) {
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

fn parse_optional_params<T>(request: &Request) -> Result<T, ProtocolError>
where
    T: serde::de::DeserializeOwned + Default,
{
    if request.params.is_null() {
        Ok(T::default())
    } else {
        parse_params(request)
    }
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
pub(crate) fn serialize_response(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|err| {
        warn!(error = %err, "failed to serialize response; sending fallback error");
        format!(
            r#"{{"v":{},"id":"{}","err":{{"class":"daemon","code":"serialize_failed","msg":"response serialization failed"}}}}"#,
            PROTOCOL_VERSION.get(),
            resp.id().replace('"', "")
        )
    })
}

fn parse_attach_prelude(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }

    match object.get("attach") {
        Some(Value::String(stream_id)) if !stream_id.is_empty() => Some(stream_id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_attach_prelude;

    #[test]
    fn attach_prelude_requires_exact_one_field_shape() {
        assert_eq!(
            parse_attach_prelude(r#"{"attach":"a-1"}"#),
            Some("a-1".to_owned())
        );
        assert_eq!(parse_attach_prelude(r#"{"attach":""}"#), None);
        assert_eq!(
            parse_attach_prelude(
                r#"{"v":1,"id":"req-1","method":"daemon.health","params":null,"attach":"a-1"}"#
            ),
            None
        );
    }
}
