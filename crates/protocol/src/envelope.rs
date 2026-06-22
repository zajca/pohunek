//! Wire envelopes: request, response (ok/err), and event.
//!
//! One JSON value per line. Shapes follow `docs/plan-phase-1.md`
//! "Control Protocol":
//!
//! ```jsonc
//! // request
//! {"v":1,"id":"req-7f3","method":"session.new","params":{...}}
//! // response (ok)
//! {"v":1,"id":"req-7f3","ok":{...}}
//! // response (typed error)
//! {"v":1,"id":"req-7f3","err":{"class":"runtime","code":"...","msg":"...","recover":"..."}}
//! // event (on a subscription connection)
//! {"v":1,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title","ts":"..."}
//! ```
//!
//! `params`, `ok`, and event payloads are kept as `serde_json::Value` for the
//! generic envelope so the typed per-method payloads can evolve in later
//! milestones without changing the envelope contract. Unknown fields are
//! ignored (additive evolution), so a newer peer's extra fields do not break an
//! older peer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProtocolError;
use crate::version::{ProtocolVersion, PROTOCOL_VERSION};

/// A control request: `{v, id, method, params}`.
///
/// `params` is method-specific; it is carried as an opaque JSON value at the
/// envelope layer and parsed into a typed struct by the method handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version of the sender.
    pub v: ProtocolVersion,
    /// Correlation id echoed by the response and related events.
    pub id: String,
    /// Method name (see [`crate::method`]).
    pub method: String,
    /// Method-specific parameters. Defaults to JSON `null` when absent.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Build a request at the current [`PROTOCOL_VERSION`].
    ///
    /// `params` may be any serializable value; pass `serde_json::Value::Null`
    /// (or `serde_json::json!({})`) for parameterless methods.
    #[must_use]
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// A control response: either `ok` or `err`, never both.
///
/// Modeled as an untagged enum over two variants so the JSON is exactly the
/// envelope sketch (`{v,id,ok}` or `{v,id,err}`) without an extra discriminant
/// field. Deserialization tries `ok` first, then `err`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    /// Successful response carrying a method-specific result.
    Ok {
        /// Protocol version of the responder.
        v: ProtocolVersion,
        /// Echoed correlation id from the request.
        id: String,
        /// Method-specific result payload.
        ok: Value,
    },
    /// Failed response carrying a typed error.
    Err {
        /// Protocol version of the responder.
        v: ProtocolVersion,
        /// Echoed correlation id from the request.
        id: String,
        /// Typed error body.
        err: ProtocolError,
    },
}

impl Response {
    /// Build a success response at the current [`PROTOCOL_VERSION`].
    #[must_use]
    pub fn ok(id: impl Into<String>, ok: Value) -> Self {
        Response::Ok {
            v: PROTOCOL_VERSION,
            id: id.into(),
            ok,
        }
    }

    /// Build an error response at the current [`PROTOCOL_VERSION`].
    #[must_use]
    pub fn err(id: impl Into<String>, err: ProtocolError) -> Self {
        Response::Err {
            v: PROTOCOL_VERSION,
            id: id.into(),
            err,
        }
    }

    /// The correlation id of this response, regardless of variant.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Response::Ok { id, .. } | Response::Err { id, .. } => id,
        }
    }

    /// The protocol version of this response, regardless of variant.
    #[must_use]
    pub fn version(&self) -> ProtocolVersion {
        match self {
            Response::Ok { v, .. } | Response::Err { v, .. } => *v,
        }
    }
}

/// The signal source for a published agent state.
///
/// Carried by state events so consumers can judge signal strength (see
/// `docs/plan-phase-1.md` "State Engine" and `docs/architecture.md`
/// "Agent state detection"). Defined here in milestone 1 because it is part of
/// the wire contract; the detection engine that produces it lands in a later
/// milestone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    /// Derived from the OSC terminal title.
    OscTitle,
    /// Derived from OSC progress reports.
    OscProgress,
    /// Derived from screen-content manifest matching.
    Screen,
    /// Derived from process/PTY activity.
    Process,
}

/// An asynchronous event pushed on a subscription connection.
///
/// `{v, event, ...payload}`. The event name and a flat payload are carried; the
/// payload is method/event-specific JSON merged at the same level (as in the
/// sketch's `agent_state` event). Carried as `serde_json::Value` at the envelope
/// layer for the same forward-compatibility reason as `Request::params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Protocol version of the sender.
    pub v: ProtocolVersion,
    /// Event name (e.g. `agent_state`).
    pub event: String,
    /// Optional correlation id, when the event relates to a prior request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Event-specific payload fields.
    ///
    /// Flattened so payload keys sit at the top level of the JSON object,
    /// matching the wire sketch.
    #[serde(flatten)]
    pub payload: Value,
}

impl Event {
    /// Build an event at the current [`PROTOCOL_VERSION`].
    ///
    /// `payload` must be a JSON object; its keys are flattened alongside `v` and
    /// `event` on the wire.
    #[must_use]
    pub fn new(event: impl Into<String>, payload: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            event: event.into(),
            id: None,
            payload,
        }
    }

    /// Attach a correlation id to this event.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}
