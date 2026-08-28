//! Wire envelopes: request, response (ok/err), and event.
//!
//! One JSON value per line. Shapes follow `docs/plan-phase-1.md`
//! "Control Protocol":
//!
//! ```jsonc
//! // request
//! {"v":{"minimum":3,"maximum":3},"id":"req-7f3","method":"session.new","params":{...}}
//! // response (ok)
//! {"v":3,"id":"req-7f3","ok":{...}}
//! // response (typed error)
//! {"v":3,"id":"req-7f3","err":{"class":"runtime","code":"...","msg":"...","recover":"..."}}
//! // event (on a subscription connection)
//! {"v":3,"event":"agent_state","session_id":"s-42","activity":"blocked","source":"osc_title","ts":"..."}
//! ```
//!
//! `params`, `ok`, and event payloads are kept as `serde_json::Value` for the
//! generic envelope so the typed per-method payloads can evolve in later
//! milestones without changing the envelope contract. The envelope coordinates
//! themselves are strict: unknown keys and malformed version ranges are
//! rejected before dispatch.

use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

use crate::error::ProtocolError;
use crate::version::{ProtocolVersion, ProtocolVersionRange, SUPPORTED_PROTOCOL_VERSIONS};
use crate::SessionId;
use crate::{MAX_REQUEST_ID_BYTES, MAX_SESSION_ID_BYTES};

/// A control request: `{v: {minimum, maximum}, id, method, params}`.
///
/// `params` is method-specific; it is carried as an opaque JSON value at the
/// envelope layer and parsed into a typed struct by the method handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    /// Inclusive public protocol range supported by the sender.
    ///
    /// The legacy integer `v` envelope is deliberately rejected at the M1
    /// boundary. Every public request now declares both endpoints so peers can
    /// select their highest shared version.
    v: ProtocolVersionRange,
    /// Correlation id echoed by the response and related events.
    id: String,
    /// Method name (see [`crate::method`]).
    method: String,
    /// Method-specific parameters. Defaults to JSON `null` when absent.
    #[serde(default)]
    params: Value,
    /// Session containing the caller process, when the client inherited one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_session_id: Option<SessionId>,
    /// Daemon instance paired with `origin_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_daemon_id: Option<String>,
}

impl Request {
    /// Builds a request for this build's supported range.
    ///
    /// `params` may be any serializable value; pass `serde_json::Value::Null`
    /// (or `serde_json::json!({})`) for parameterless methods.
    ///
    /// # Examples
    ///
    /// ```
    /// use protocol::{method, Request, SUPPORTED_PROTOCOL_VERSIONS};
    ///
    /// let request = Request::new("req-1", method::DAEMON_HEALTH, serde_json::Value::Null)?;
    /// assert_eq!(request.version_range(), SUPPORTED_PROTOCOL_VERSIONS);
    /// # Ok::<(), protocol::EnvelopeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] for an invalid correlation ID or empty method.
    pub fn new(
        id: impl Into<String>,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Self, EnvelopeError> {
        let id = id.into();
        let method = method.into();
        validate_request_id(&id)?;
        if method.is_empty() || method.chars().any(char::is_control) {
            return Err(EnvelopeError::InvalidMethod);
        }
        Ok(Self {
            v: SUPPORTED_PROTOCOL_VERSIONS,
            id,
            method,
            params,
            origin_session_id: None,
            origin_daemon_id: None,
        })
    }

    /// Attach inherited Pohunek origin markers to a request.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] unless both markers are absent or both are
    /// present, bounded, and safe for the control envelope.
    pub fn with_origin(
        mut self,
        session_id: Option<SessionId>,
        daemon_id: Option<String>,
    ) -> Result<Self, EnvelopeError> {
        validate_origin(session_id.as_ref(), daemon_id.as_deref())?;
        self.origin_session_id = session_id;
        self.origin_daemon_id = daemon_id;
        Ok(self)
    }

    /// Returns the caller's supported protocol range.
    #[must_use]
    pub const fn version_range(&self) -> ProtocolVersionRange {
        self.v
    }

    /// Returns the response correlation identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the open protocol method name.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Returns the opaque method parameters.
    #[must_use]
    pub const fn params(&self) -> &Value {
        &self.params
    }

    /// Returns the caller's inherited Pohunek session marker.
    #[must_use]
    pub const fn origin_session_id(&self) -> Option<&SessionId> {
        self.origin_session_id.as_ref()
    }

    /// Returns the caller's inherited Pohunek daemon marker.
    #[must_use]
    pub fn origin_daemon_id(&self) -> Option<&str> {
        self.origin_daemon_id.as_deref()
    }

    /// Splits the validated request into dispatch-owned parts.
    #[must_use]
    pub fn into_parts(self) -> (ProtocolVersionRange, String, String, Value) {
        (self.v, self.id, self.method, self.params)
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            v: ProtocolVersionRange,
            id: String,
            method: String,
            #[serde(default)]
            params: Value,
            #[serde(default)]
            origin_session_id: Option<SessionId>,
            #[serde(default)]
            origin_daemon_id: Option<String>,
        }
        let wire = WireRequest::deserialize(deserializer)?;
        validate_request_id(&wire.id).map_err(serde::de::Error::custom)?;
        if wire.method.is_empty() || wire.method.chars().any(char::is_control) {
            return Err(serde::de::Error::custom(EnvelopeError::InvalidMethod));
        }
        validate_origin(
            wire.origin_session_id.as_ref(),
            wire.origin_daemon_id.as_deref(),
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self {
            v: wire.v,
            id: wire.id,
            method: wire.method,
            params: wire.params,
            origin_session_id: wire.origin_session_id,
            origin_daemon_id: wire.origin_daemon_id,
        })
    }
}

/// Reports an invalid public control envelope.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnvelopeError {
    /// A request ID exceeded the bound or required JSON escaping.
    #[error("request id must contain 1 to {MAX_REQUEST_ID_BYTES} unescaped ASCII wire characters")]
    InvalidRequestId,
    /// A method was empty or contained a control character.
    #[error("control method must be nonempty and contain no control characters")]
    InvalidMethod,
    /// Origin coordinates were incomplete or unsafe.
    #[error(
        "origin markers must be absent together or contain paired bounded unescaped ASCII identifiers"
    )]
    InvalidOrigin,
    /// An event name was empty or contained a control character.
    #[error("event name must be nonempty and contain no control characters")]
    InvalidEvent,
    /// An event payload was not an object or attempted to replace envelope coordinates.
    #[error("event payload must be an object without reserved v, event, or id fields")]
    InvalidEventPayload,
}

fn validate_request_id(id: &str) -> Result<(), EnvelopeError> {
    if id.is_empty()
        || id.len() > MAX_REQUEST_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(EnvelopeError::InvalidRequestId)
    } else {
        Ok(())
    }
}

fn validate_origin(
    session_id: Option<&SessionId>,
    daemon_id: Option<&str>,
) -> Result<(), EnvelopeError> {
    match (session_id, daemon_id) {
        (None, None) => Ok(()),
        (Some(session_id), Some(daemon_id))
            if valid_origin_identifier(&session_id.0, MAX_SESSION_ID_BYTES)
                && valid_origin_identifier(daemon_id, MAX_SESSION_ID_BYTES) =>
        {
            Ok(())
        }
        _ => Err(EnvelopeError::InvalidOrigin),
    }
}

fn valid_origin_identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// A control response: either `ok` or `err`, never both.
///
/// Its representation is private so callers cannot construct an invalid ID or
/// bypass the negotiated version supplied to [`Self::ok`] and [`Self::err`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    version: ProtocolVersion,
    id: String,
    body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResponseBody {
    Ok(Value),
    Err(ProtocolError),
}

impl Response {
    /// Builds a success response at the selected negotiated version.
    ///
    /// The caller must pass the negotiated overlap, which may be lower than the
    /// responder's maximum supported version.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] when `id` violates the public envelope bound.
    pub fn ok(
        version: ProtocolVersion,
        id: impl Into<String>,
        ok: Value,
    ) -> Result<Self, EnvelopeError> {
        let id = id.into();
        validate_request_id(&id)?;
        Ok(Self {
            version,
            id,
            body: ResponseBody::Ok(ok),
        })
    }

    /// Builds an error response at the selected negotiated version.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] when `id` violates the public envelope bound.
    pub fn err(
        version: ProtocolVersion,
        id: impl Into<String>,
        err: ProtocolError,
    ) -> Result<Self, EnvelopeError> {
        let id = id.into();
        validate_request_id(&id)?;
        Ok(Self {
            version,
            id,
            body: ResponseBody::Err(err),
        })
    }

    /// The correlation id of this response, regardless of variant.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The protocol version of this response, regardless of variant.
    #[must_use]
    pub const fn version(&self) -> ProtocolVersion {
        self.version
    }

    /// Returns the success value, or the typed protocol error.
    pub const fn result(&self) -> Result<&Value, &ProtocolError> {
        match &self.body {
            ResponseBody::Ok(value) => Ok(value),
            ResponseBody::Err(error) => Err(error),
        }
    }

    /// Consumes the envelope and returns its success value or typed error.
    pub fn into_result(self) -> Result<Value, ProtocolError> {
        match self.body {
            ResponseBody::Ok(value) => Ok(value),
            ResponseBody::Err(error) => Err(error),
        }
    }

    /// Returns whether this is a success response.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self.body, ResponseBody::Ok(_))
    }
}

impl Serialize for Response {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("v", &self.version)?;
        map.serialize_entry("id", &self.id)?;
        match &self.body {
            ResponseBody::Ok(value) => map.serialize_entry("ok", value)?,
            ResponseBody::Err(error) => map.serialize_entry("err", error)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireResponse {
            Ok(WireOk),
            Err(WireErr),
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireOk {
            v: ProtocolVersion,
            id: String,
            ok: Value,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireErr {
            v: ProtocolVersion,
            id: String,
            err: ProtocolError,
        }

        match WireResponse::deserialize(deserializer)? {
            WireResponse::Ok(wire) => {
                Self::ok(wire.v, wire.id, wire.ok).map_err(serde::de::Error::custom)
            }
            WireResponse::Err(wire) => {
                Self::err(wire.v, wire.id, wire.err).map_err(serde::de::Error::custom)
            }
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "StateSource.ts"))]
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
    /// Reported by an agent hook running inside the session.
    Report,
}

/// An asynchronous event pushed on a subscription connection.
///
/// `{v, event, ...payload}`. The event name and a flat payload are carried; the
/// payload is method/event-specific JSON merged at the same level (as in the
/// sketch's `agent_state` event). Carried as `serde_json::Value` at the envelope
/// layer for the same forward-compatibility reason as `Request::params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// Protocol version of the sender.
    v: ProtocolVersion,
    /// Event name (e.g. `agent_state`).
    #[serde(rename = "event")]
    name: String,
    /// Optional correlation id, when the event relates to a prior request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// Event-specific payload fields.
    ///
    /// Flattened so payload keys sit at the top level of the JSON object,
    /// matching the wire sketch.
    #[serde(flatten)]
    payload: Value,
}

impl Event {
    /// Builds an event at the selected negotiated version.
    ///
    /// `payload` must be a JSON object; its keys are flattened alongside `v` and
    /// `event` on the wire.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] for an invalid event name, a non-object
    /// payload, or payload keys reserved by the envelope.
    pub fn new(
        version: ProtocolVersion,
        event: impl Into<String>,
        payload: Value,
    ) -> Result<Self, EnvelopeError> {
        let event = event.into();
        if event.is_empty() || event.chars().any(char::is_control) {
            return Err(EnvelopeError::InvalidEvent);
        }
        let Value::Object(fields) = &payload else {
            return Err(EnvelopeError::InvalidEventPayload);
        };
        if fields
            .keys()
            .any(|key| matches!(key.as_str(), "v" | "event" | "id"))
        {
            return Err(EnvelopeError::InvalidEventPayload);
        }
        Ok(Self {
            v: version,
            name: event,
            id: None,
            payload,
        })
    }

    /// Attach a correlation id to this event.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] when `id` violates the public envelope bound.
    pub fn with_id(mut self, id: impl Into<String>) -> Result<Self, EnvelopeError> {
        let id = id.into();
        validate_request_id(&id)?;
        self.id = Some(id);
        Ok(self)
    }

    /// Returns the selected protocol version.
    #[must_use]
    pub const fn version(&self) -> ProtocolVersion {
        self.v
    }

    /// Project this event onto a connection's negotiated protocol version.
    ///
    /// Daemon producers broadcast one internal event value to many subscribers;
    /// each transport applies its independently selected overlap before writing.
    #[must_use]
    pub fn with_version(mut self, version: ProtocolVersion) -> Self {
        self.v = version;
        self
    }

    /// Returns the event wire name.
    #[must_use]
    pub fn event(&self) -> &str {
        &self.name
    }

    /// Returns the optional response correlation identifier.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the open event payload object.
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireEvent {
            v: ProtocolVersion,
            event: String,
            #[serde(default)]
            id: Option<String>,
            #[serde(flatten)]
            payload: serde_json::Map<String, Value>,
        }

        let wire = WireEvent::deserialize(deserializer)?;
        let mut event = Self::new(wire.v, wire.event, Value::Object(wire.payload))
            .map_err(serde::de::Error::custom)?;
        if let Some(id) = wire.id {
            event = event.with_id(id).map_err(serde::de::Error::custom)?;
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{EnvelopeError, Request};
    use crate::SessionId;

    #[test]
    fn request_origin_requires_both_markers() {
        for value in [
            json!({
                "v": {"minimum": 2, "maximum": 2},
                "id": "request-1",
                "method": "daemon.health",
                "params": {},
                "origin_session_id": "s-origin"
            }),
            json!({
                "v": {"minimum": 2, "maximum": 2},
                "id": "request-1",
                "method": "daemon.health",
                "params": {},
                "origin_daemon_id": "d-origin"
            }),
        ] {
            let error = serde_json::from_value::<Request>(value)
                .expect_err("one origin marker must be rejected");
            assert!(error
                .to_string()
                .contains("origin markers must be absent together"));
        }
    }

    #[test]
    fn request_origin_rejects_unsafe_values_without_echoing_them() {
        let secret = "origin-secret/value";
        let value = json!({
            "v": {"minimum": 2, "maximum": 2},
            "id": "request-1",
            "method": "daemon.health",
            "params": {},
            "origin_session_id": "s-origin",
            "origin_daemon_id": secret
        });

        let error = serde_json::from_value::<Request>(value)
            .expect_err("unsafe origin marker must be rejected")
            .to_string();
        assert!(!error.contains(secret));
    }

    #[test]
    fn request_origin_builder_accepts_only_a_valid_pair() {
        let request = Request::new("request-1", "daemon.health", json!({}))
            .expect("valid request")
            .with_origin(
                Some(SessionId("s-origin".to_owned())),
                Some("d-origin".to_owned()),
            )
            .expect("valid origin markers");
        assert_eq!(
            request.origin_session_id(),
            Some(&SessionId("s-origin".to_owned()))
        );
        assert_eq!(request.origin_daemon_id(), Some("d-origin"));

        let error = Request::new("request-2", "daemon.health", json!({}))
            .expect("valid request")
            .with_origin(Some(SessionId("s-origin".to_owned())), None)
            .expect_err("partial origin must be rejected");
        assert_eq!(error, EnvelopeError::InvalidOrigin);
    }
}
