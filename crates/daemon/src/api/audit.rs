//! Audit entries for control requests that change a session.
//!
//! One `info` event per audited request, on the [`AUDIT_TARGET`] tracing target,
//! naming who asked (transport and peer), what kind of request it was, which
//! session it addressed and how it ended. The entry is built from the method
//! name, ids and the error code only: request params such as input text, keys,
//! environment values or attach stream ids are never read into it.

use std::net::SocketAddr;

use protocol::{method, Request, Response};
use tracing::info;

use super::handler::{ControlTransport, DaemonState};

/// Tracing target of every audit entry, so a log filter can select them.
pub(crate) const AUDIT_TARGET: &str = "audit.session_change";

/// Kind recorded for the raw-byte attach stream, whose keys never reach a request.
const ATTACH_STREAM_KIND: &str = "attach.stream";

/// Upper bound, in characters, of a caller-supplied string written to the log.
///
/// A session id is chosen by the client; the bound keeps a hostile value from
/// inflating or masquerading as part of the entry.
const MAX_LOGGED_ID_CHARS: usize = 128;

/// Who a control connection is, as far as the daemon can attest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    /// A remote overlay peer: the TCP peer address of the connection.
    Remote(SocketAddr),
    /// A local Unix socket peer attested by the kernel.
    Local { pid: u32, uid: u32 },
    /// A local connection without an attested peer, such as in-process dispatch.
    LocalUnattested,
}

impl Caller {
    /// The caller of the connection `state` serves.
    pub(crate) fn of(state: &DaemonState) -> Self {
        match (
            state.transport,
            state.remote_peer,
            state.control_peer.as_deref(),
        ) {
            (ControlTransport::Remote, Some(addr), _) => Self::Remote(addr),
            (_, _, Some(peer)) => {
                let (pid, uid) = peer.audit_identity();
                Self::Local { pid, uid }
            }
            _ => Self::LocalUnattested,
        }
    }

    const fn transport(self) -> &'static str {
        match self {
            Self::Remote(_) => "remote",
            Self::Local { .. } | Self::LocalUnattested => "local",
        }
    }
}

/// Whether `method` changes a session and is therefore audited.
///
/// `session.resize` is deliberately absent.
fn changes_session(method: &str) -> bool {
    matches!(
        method,
        method::SESSION_NEW
            | method::SESSION_STOP
            | method::SESSION_RESUME
            | method::SESSION_REMOVE
            | method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
            | method::SESSION_REMOVE_TREE
            | method::SESSION_INPUT
            | method::SESSION_DETACH
            | method::SESSION_SET_METADATA
            | method::SESSION_RENAME
            | method::SESSION_RETENTION_SWEEP
    )
}

/// Writes the audit entry of a finished request when it changes a session.
///
/// `stream_session` is the session of the attach stream a `session.detach`
/// request names, resolved by the caller because the request carries only the
/// stream id.
pub(crate) fn record_request(
    request: &Request,
    response: &Response,
    state: &DaemonState,
    stream_session: Option<&str>,
) {
    if !changes_session(request.method()) {
        return;
    }
    // A detach request names only a stream, so its session comes from the
    // stream alone and never from fields the caller adds to the params.
    let session = if request.method() == method::SESSION_DETACH {
        stream_session.map(str::to_owned)
    } else {
        session_of(request, response)
    };
    let error_code = response.result().err().map(|error| error.code.as_str());
    emit(
        Caller::of(state),
        request.method(),
        session.as_deref(),
        error_code,
        (
            request.origin_session_id().map(|id| id.0.as_str()),
            request.origin_daemon_id(),
        ),
    );
}

/// Writes the audit entry of an attach stream redemption.
///
/// Keys and answers typed on the stream are never logged; the one entry says
/// who opened the stream that carries them.
pub(crate) fn record_attach(caller: Caller, session: Option<&str>, error_code: Option<&str>) {
    emit(
        caller,
        ATTACH_STREAM_KIND,
        session,
        error_code,
        (None, None),
    );
}

fn emit(
    caller: Caller,
    kind: &str,
    session: Option<&str>,
    error_code: Option<&str>,
    (origin_session, origin_daemon): (Option<&str>, Option<&str>),
) {
    let outcome = if error_code.is_none() { "ok" } else { "error" };
    let peer = match caller {
        Caller::Remote(addr) => addr.to_string(),
        Caller::Local { pid, uid } => format!("pid={pid} uid={uid}"),
        Caller::LocalUnattested => "unattested".to_owned(),
    };
    let session = session.map(bounded);
    let origin_session = origin_session.map(bounded);
    let origin_daemon = origin_daemon.map(bounded);
    info!(
        target: AUDIT_TARGET,
        transport = %caller.transport(),
        peer = %peer,
        kind = %kind,
        session = session.as_deref(),
        outcome = %outcome,
        error_code,
        claimed_origin_session = origin_session.as_deref(),
        claimed_origin_daemon = origin_daemon.as_deref(),
        "session change request"
    );
}

/// The session a request addressed: from its params, else from a successful result.
fn session_of(request: &Request, response: &Response) -> Option<String> {
    let params = request.params();
    // Only these methods take the session id as the bare params value.
    let bare = matches!(
        request.method(),
        method::SESSION_STOP
            | method::SESSION_RESUME
            | method::SESSION_REMOVE
            | method::SESSION_REMOVE_ACCEPTING_UNCONFIRMED
    )
    .then(|| params.as_str())
    .flatten();
    let from_params = bare.or_else(|| params.get("session_id").and_then(serde_json::Value::as_str));
    from_params
        .or_else(|| {
            response
                .result()
                .ok()
                .and_then(|result| result.get("id").and_then(serde_json::Value::as_str))
        })
        .map(str::to_owned)
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_LOGGED_ID_CHARS).collect()
}
