//! Edge adapters that serve the previous public protocol version.
//!
//! A daemon of this build negotiates `MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION`
//! (see [`crate::SUPPORTED_PROTOCOL_VERSIONS`]). Core types describe only the
//! current wire shape; each older version in the window has one adapter module
//! (`v3` today) that translates requests up to the current shape and results
//! and events back down to the older one. The negotiated version is fixed per
//! connection, so a connection is adapted for its whole life.
//!
//! Adapters translate shape only: key renames and moved fields, plus dropping
//! values the older version cannot decode (events and enum values it never
//! defined). A method added after the older version is refused for it with
//! [`CompatError::MethodNotDefined`], never served. A semantic change (a new required parameter, a removed method, a
//! changed error code or meaning) cannot be expressed as a shape translation.
//! It raises [`crate::MIN_PROTOCOL_VERSION`] instead and is announced as a
//! break. On the next [`crate::PROTOCOL_VERSION`] bump the oldest adapter is
//! deleted in the same change and an adapter for the version being left behind
//! is added.
//!
//! The functions here are the only entry points; the daemon calls the
//! envelope-level ones ([`upgrade_request`], [`downgrade_response`],
//! [`downgrade_event`]) at its request and event boundaries.

// Rust guideline compliant 2026-06-26

mod v3;

use serde_json::Value;
use thiserror::Error;

use crate::envelope::{EnvelopeError, Event, Request, Response};
use crate::version::{ProtocolVersion, PROTOCOL_VERSION};

/// Reports a request, result or event that an edge adapter cannot translate.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompatError {
    /// The version has no adapter and is not the current version.
    #[error("public protocol version {0} has no edge adapter")]
    UnsupportedVersion(ProtocolVersion),
    /// A key the translation would write already exists at the same place.
    ///
    /// A request that carries the current spelling of a renamed key is not a
    /// request of the older version; a result that carries both spellings
    /// cannot be expressed in the older shape.
    #[error("`{key}` already exists at {site} and cannot be translated")]
    Conflict {
        /// Location of the object being translated, for example `params.runtime`.
        site: &'static str,
        /// Key that was about to be written.
        key: &'static str,
    },
    /// The method was added after the older version and has no shape in it.
    ///
    /// The caller answers `method_not_found`; the method is never served to a
    /// connection of that version.
    #[error("method `{method}` is not defined in this protocol version")]
    MethodNotDefined {
        /// Method name the older version never defined.
        method: String,
    },
    /// Rebuilding the translated envelope failed.
    #[error("translated envelope is invalid: {0}")]
    Envelope(#[from] EnvelopeError),
}

/// Lists the methods `version` never defined, because they were added later.
///
/// Empty for the current version. These have no previous-release fixtures; the
/// adapter refuses them instead of translating (see
/// [`CompatError::MethodNotDefined`]). The list goes away with its adapter.
#[must_use]
pub fn introduced_methods(version: ProtocolVersion) -> &'static [&'static str] {
    match adapter(version) {
        Ok(Adapter::V3) => v3::INTRODUCED_METHODS,
        _ => &[],
    }
}

/// Translates request parameters of `version` into the current shape.
///
/// Returns `params` unchanged for the current version and for methods whose
/// parameters did not change.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, or [`CompatError::Conflict`] when `params` already carries a key of
/// the current shape that `version` does not define.
pub fn request_params(
    version: ProtocolVersion,
    method: &str,
    params: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(params),
        Adapter::V3 => v3::request_params(method, params),
    }
}

/// Translates a success payload from the current shape into `version`.
///
/// Returns `result` unchanged for the current version and for methods whose
/// results did not change.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, or [`CompatError::Conflict`] when `result` already carries a key of
/// the older shape that the translation would write.
pub fn result(version: ProtocolVersion, method: &str, result: Value) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(result),
        Adapter::V3 => v3::result(method, result),
    }
}

/// Translates an event payload from the current shape into `version`.
///
/// Returns `Ok(None)` when `version` never defined the event, so the caller
/// sends nothing to that subscriber.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, or [`CompatError::Conflict`] when `payload` already carries a key of
/// the older shape that the translation would write.
pub fn event_payload(
    version: ProtocolVersion,
    name: &str,
    payload: Value,
) -> Result<Option<Value>, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(Some(payload)),
        Adapter::V3 => v3::event_payload(name, payload),
    }
}

/// Rewrites a request of `version` so a current handler can parse it.
///
/// The envelope range, id and origin markers are kept; only the parameters
/// change.
///
/// # Errors
///
/// Returns the [`request_params`] errors.
pub fn upgrade_request(request: Request, version: ProtocolVersion) -> Result<Request, CompatError> {
    if version == PROTOCOL_VERSION {
        return Ok(request);
    }
    let params = request_params(version, request.method(), request.params().clone())?;
    Ok(request.with_params(params))
}

/// Rewrites a current response into the version it is stamped with.
///
/// `method` is the request method the response answers. Typed errors pass
/// through unchanged; only success payloads are translated.
///
/// # Errors
///
/// Returns the [`result`] errors.
pub fn downgrade_response(response: Response, method: &str) -> Result<Response, CompatError> {
    let version = response.version();
    if version == PROTOCOL_VERSION {
        return Ok(response);
    }
    let id = response.id().to_owned();
    match response.into_result() {
        Ok(value) => Ok(Response::ok(version, id, result(version, method, value)?)?),
        Err(error) => Ok(Response::err(version, id, error)?),
    }
}

/// Rewrites a current event for a subscriber that negotiated `version`.
///
/// Returns `Ok(None)` when `version` never defined the event.
///
/// # Errors
///
/// Returns the [`event_payload`] errors.
pub fn downgrade_event(
    event: Event,
    version: ProtocolVersion,
) -> Result<Option<Event>, CompatError> {
    if version == PROTOCOL_VERSION {
        return Ok(Some(event.with_version(version)));
    }
    let Some(payload) = event_payload(version, event.event(), event.payload().clone())? else {
        return Ok(None);
    };
    let mut adapted = Event::new(version, event.event(), payload)?;
    if let Some(id) = event.id() {
        adapted = adapted.with_id(id)?;
    }
    Ok(Some(adapted))
}

/// Adapter selected for a negotiated version.
enum Adapter {
    Current,
    V3,
}

fn adapter(version: ProtocolVersion) -> Result<Adapter, CompatError> {
    if version == PROTOCOL_VERSION {
        return Ok(Adapter::Current);
    }
    match version.get() {
        v3::VERSION => Ok(Adapter::V3),
        _ => Err(CompatError::UnsupportedVersion(version)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        downgrade_event, downgrade_response, event_payload, request_params, result,
        upgrade_request, CompatError,
    };
    use crate::envelope::{Event, Request, Response};
    use crate::version::{ProtocolVersion, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
    use crate::{method, ProtocolError};

    fn version(value: u32) -> ProtocolVersion {
        ProtocolVersion::new(value).expect("nonzero version")
    }

    #[test]
    fn every_version_in_the_window_has_an_adapter() {
        for value in MIN_PROTOCOL_VERSION.get()..=PROTOCOL_VERSION.get() {
            let probe = request_params(version(value), method::DAEMON_HEALTH, json!(null));
            assert!(
                probe.is_ok(),
                "protocol {value} is inside the window but has no adapter: {probe:?}"
            );
        }
    }

    #[test]
    fn a_version_outside_the_window_has_no_adapter() {
        let below = version(MIN_PROTOCOL_VERSION.get() - 1);
        assert_eq!(
            request_params(below, method::DAEMON_HEALTH, json!(null)),
            Err(CompatError::UnsupportedVersion(below))
        );
        assert_eq!(
            result(below, method::DAEMON_HEALTH, json!(null)),
            Err(CompatError::UnsupportedVersion(below))
        );
        assert_eq!(
            event_payload(below, "agent_state", json!({})),
            Err(CompatError::UnsupportedVersion(below))
        );
    }

    #[test]
    fn the_current_version_passes_through_untouched() {
        let params = json!({"runtime": {"worker_instance_id": "w-1", "runtime_generation": "1"}});
        assert_eq!(
            request_params(PROTOCOL_VERSION, method::SESSION_OUTPUT, params.clone()),
            Ok(params.clone())
        );
        assert_eq!(
            result(PROTOCOL_VERSION, method::SESSION_OUTPUT, params.clone()),
            Ok(params)
        );
    }

    #[test]
    fn upgrade_request_keeps_the_envelope_coordinates() {
        let request: Request = serde_json::from_value(json!({
            "v": {"minimum": 3, "maximum": 3},
            "id": "req-1",
            "method": "session.report_native_id",
            "params": {"runtime_id": "w-1"},
            "origin_session_id": "s-1",
            "origin_daemon_id": "d-1"
        }))
        .expect("valid protocol 3 request");
        let upgraded = upgrade_request(request, MIN_PROTOCOL_VERSION).expect("adapter");
        assert_eq!(upgraded.params(), &json!({"worker_instance_id": "w-1"}));
        assert_eq!(upgraded.id(), "req-1");
        assert_eq!(upgraded.method(), "session.report_native_id");
        assert_eq!(upgraded.version_range().minimum(), MIN_PROTOCOL_VERSION);
        assert_eq!(
            upgraded.origin_session_id().map(|id| id.0.as_str()),
            Some("s-1")
        );
        assert_eq!(upgraded.origin_daemon_id(), Some("d-1"));
    }

    #[test]
    fn downgrade_response_stamps_and_translates_success_payloads() {
        let response = Response::ok(
            MIN_PROTOCOL_VERSION,
            "req-1",
            json!({"entries": [{"runtime_slot": "slot", "worker_instance_id": "w-1", "status": "managed"}]}),
        )
        .expect("response");
        let adapted =
            downgrade_response(response, method::SESSION_RUNTIME_INVENTORY).expect("adapter");
        assert_eq!(adapted.version(), MIN_PROTOCOL_VERSION);
        assert_eq!(
            adapted.result().expect("ok"),
            &json!({"entries": [{"runtime_slot": "slot", "runtime_id": "w-1", "status": "managed"}]})
        );
    }

    #[test]
    fn downgrade_response_passes_typed_errors_through() {
        let error = ProtocolError::bad_request("nope");
        let response =
            Response::err(MIN_PROTOCOL_VERSION, "req-1", error.clone()).expect("response");
        let adapted = downgrade_response(response, method::SESSION_NEW).expect("adapter");
        assert_eq!(adapted.result().expect_err("typed error"), &error);
        assert_eq!(adapted.version(), MIN_PROTOCOL_VERSION);
    }

    #[test]
    fn downgrade_event_restamps_translates_and_keeps_the_correlation_id() {
        let event = Event::new(
            PROTOCOL_VERSION,
            "agent_state",
            json!({"session_id": "s-1", "runtime": {"worker_instance_id": "w-1", "runtime_generation": "1"}}),
        )
        .expect("event")
        .with_id("req-1")
        .expect("id");
        let adapted = downgrade_event(event, MIN_PROTOCOL_VERSION)
            .expect("adapter")
            .expect("agent_state exists in the previous version");
        assert_eq!(adapted.version(), MIN_PROTOCOL_VERSION);
        assert_eq!(adapted.id(), Some("req-1"));
        assert_eq!(
            adapted.payload(),
            &json!({"session_id": "s-1", "runtime": {"runtime_id": "w-1", "runtime_generation": "1"}})
        );
    }

    #[test]
    fn downgrade_event_drops_an_event_the_older_version_never_defined() {
        let event =
            Event::new(PROTOCOL_VERSION, "event_from_the_future", json!({})).expect("event");
        assert_eq!(downgrade_event(event, MIN_PROTOCOL_VERSION), Ok(None));
    }
}
