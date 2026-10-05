//! Edge adapters between the current public protocol and the previous version.
//!
//! The window is [`crate::SUPPORTED_PROTOCOL_VERSIONS`], the previous version
//! and the current one. Core types describe only the current wire shape; each
//! older version in the window has one adapter module (`v3` today) that
//! translates in both directions. A daemon translates a previous-version
//! peer's requests up to the current shape and its own results and events down
//! ([`upgrade_request`], [`downgrade_response`], [`downgrade_event`]). A client
//! of this build talking to a previous-version daemon does the inverse
//! ([`downgrade_request`], [`upgrade_result`], [`upgrade_event`]). The
//! negotiated version is fixed per connection, so a connection is adapted for
//! its whole life.
//!
//! Adapters translate shape only: key renames and moved fields, plus dropping
//! values the older version cannot decode (events and enum values it never
//! defined). A method added after the older version is refused for it with
//! [`CompatError::MethodNotDefined`], never sent or served. A semantic change (a
//! new required parameter, a removed method, a changed error code or meaning)
//! cannot be expressed as a shape translation. It raises
//! [`crate::MIN_PROTOCOL_VERSION`] instead and is announced as a break. On the
//! next [`crate::PROTOCOL_VERSION`] bump the oldest adapter is deleted in the
//! same change and an adapter for the version being left behind is added.
//!
//! The functions here are the only entry points. Both directions share one
//! adapter per version, so the sites that carry a renamed key exist once.

// Rust guideline compliant 2026-06-26

mod v3;

use serde_json::Value;
use thiserror::Error;

use crate::envelope::{EnvelopeError, Event, Request, Response};
use crate::version::{ProtocolVersion, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};

/// Methods whose handler forwards the asking client's version range to other
/// hosts.
///
/// `host.discover` probes and classifies peers for the range the request
/// advertised, so a client keeps advertising its whole window on it even after
/// its connection selected a version: pinning the envelope to the selected
/// version would classify compatible older peers as incompatible. The methods
/// are read-only, so a daemon that answers in another version than the one
/// selected earlier has no side effect to refuse, and their results are the same
/// shape in every version of the window.
pub const RANGE_FORWARDING_METHODS: &[&str] = &[crate::method::HOST_DISCOVER];

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

/// Lists the keys `version` spelled differently, as `(older, current)`.
///
/// Empty for the current version. Clients that cannot link the adapter (the
/// TypeScript SDK) generate their key mapping from this table.
#[must_use]
pub fn renamed_keys(version: ProtocolVersion) -> &'static [(&'static str, &'static str)] {
    match adapter(version) {
        Ok(Adapter::V3) => v3::RENAMED_KEYS,
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

/// Translates current request parameters into the shape of `version`.
///
/// Returns `params` unchanged for the current version and for methods whose
/// parameters did not change.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, [`CompatError::MethodNotDefined`] when `version` never defined
/// `method`, or [`CompatError::Conflict`] when `params` already carries a key
/// of the older shape that the translation would write.
pub fn downgrade_request_params(
    version: ProtocolVersion,
    method: &str,
    params: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(params),
        Adapter::V3 => v3::downgrade_request_params(method, params),
    }
}

/// Translates a success payload of `version` into the current shape.
///
/// Returns `result` unchanged for the current version and for methods whose
/// results did not change.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, [`CompatError::MethodNotDefined`] when `version` never defined
/// `method`, or [`CompatError::Conflict`] when `result` already carries a key
/// of the current shape that `version` does not define.
pub fn upgrade_result(
    version: ProtocolVersion,
    method: &str,
    result: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(result),
        Adapter::V3 => v3::upgrade_result(method, result),
    }
}

/// Translates an event payload of `version` into the current shape.
///
/// # Errors
///
/// Returns [`CompatError::UnsupportedVersion`] when `version` is outside the
/// window, or [`CompatError::Conflict`] when `payload` already carries a key of
/// the current shape that `version` does not define.
pub fn upgrade_event_payload(
    version: ProtocolVersion,
    name: &str,
    payload: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(payload),
        Adapter::V3 => v3::upgrade_event_payload(name, payload),
    }
}

/// Rewrites a current request into the shape of `version`.
///
/// The request is pinned to exactly `version`; the id and origin markers are
/// kept and only the parameters change. A client calls it once its connection
/// has selected an older version.
///
/// # Errors
///
/// Returns the [`downgrade_request_params`] errors.
pub fn downgrade_request(
    request: Request,
    version: ProtocolVersion,
) -> Result<Request, CompatError> {
    if version == PROTOCOL_VERSION {
        return Ok(request);
    }
    let params = downgrade_request_params(version, request.method(), request.params().clone())?;
    Ok(request.with_params(params).with_exact_version(version))
}

/// Rewrites an event of an older version into the current shape.
///
/// The event is restamped with [`PROTOCOL_VERSION`] because its payload now has
/// the current shape; the correlation id is kept.
///
/// # Errors
///
/// Returns the [`upgrade_event_payload`] errors.
pub fn upgrade_event(event: Event) -> Result<Event, CompatError> {
    let version = event.version();
    if version == PROTOCOL_VERSION {
        return Ok(event);
    }
    let payload = upgrade_event_payload(version, event.event(), event.payload().clone())?;
    let mut upgraded = Event::new(PROTOCOL_VERSION, event.event(), payload)?;
    if let Some(id) = event.id() {
        upgraded = upgraded.with_id(id)?;
    }
    Ok(upgraded)
}

/// Reports whether a current request means the same in every older version.
///
/// A client that has not yet selected a version can send such a request
/// without learning the daemon's version first. It is false when the request
/// changes under any older version, or when an older version never defined the
/// method.
#[must_use]
pub fn request_is_version_independent(method: &str, params: &Value) -> bool {
    (MIN_PROTOCOL_VERSION.get()..PROTOCOL_VERSION.get()).all(|value| {
        ProtocolVersion::new(value).is_ok_and(|version| {
            downgrade_request_params(version, method, params.clone())
                .is_ok_and(|downgraded| downgraded == *params)
        })
    })
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
        downgrade_event, downgrade_request, downgrade_request_params, downgrade_response,
        event_payload, request_is_version_independent, request_params, result, upgrade_event,
        upgrade_event_payload, upgrade_request, upgrade_result, CompatError,
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

    #[test]
    fn downgrade_request_pins_the_version_and_keeps_the_envelope_coordinates() {
        let request = Request::new(
            "req-1",
            method::SESSION_REPORT_NATIVE_ID,
            json!({"worker_instance_id": "w-1"}),
        )
        .expect("request")
        .with_origin(
            Some(crate::SessionId("s-1".to_owned())),
            Some("d-1".to_owned()),
        )
        .expect("origin");
        let adapted = downgrade_request(request, MIN_PROTOCOL_VERSION).expect("adapter");
        assert_eq!(adapted.params(), &json!({"runtime_id": "w-1"}));
        assert_eq!(adapted.id(), "req-1");
        assert_eq!(adapted.version_range().minimum(), MIN_PROTOCOL_VERSION);
        assert_eq!(adapted.version_range().maximum(), MIN_PROTOCOL_VERSION);
        assert_eq!(adapted.origin_daemon_id(), Some("d-1"));
    }

    #[test]
    fn downgrade_request_leaves_the_current_version_untouched() {
        let request = Request::new("req-1", method::SESSION_LIST, json!({})).expect("request");
        let adapted = downgrade_request(request.clone(), PROTOCOL_VERSION).expect("current");
        assert_eq!(adapted, request);
    }

    #[test]
    fn downgrade_request_refuses_a_method_the_older_version_never_defined() {
        let request = Request::new("req-1", method::PACKAGE_LIST, json!(null)).expect("request");
        assert_eq!(
            downgrade_request(request, MIN_PROTOCOL_VERSION),
            Err(CompatError::MethodNotDefined {
                method: method::PACKAGE_LIST.to_owned()
            })
        );
    }

    #[test]
    fn downgrade_request_params_rejects_a_version_outside_the_window() {
        let below = version(MIN_PROTOCOL_VERSION.get() - 1);
        assert_eq!(
            downgrade_request_params(below, method::SESSION_LIST, json!({})),
            Err(CompatError::UnsupportedVersion(below))
        );
        assert_eq!(
            upgrade_result(below, method::SESSION_LIST, json!([])),
            Err(CompatError::UnsupportedVersion(below))
        );
        assert_eq!(
            upgrade_event_payload(below, "agent_state", json!({})),
            Err(CompatError::UnsupportedVersion(below))
        );
    }

    #[test]
    fn upgrade_result_translates_an_older_payload_and_passes_the_current_one() {
        let older = json!({"runtime_id": "w-1", "runtime_generation": "2"});
        assert_eq!(
            upgrade_result(MIN_PROTOCOL_VERSION, method::SESSION_READ, older),
            Ok(json!({"worker_instance_id": "w-1", "runtime_generation": "2"}))
        );
        let current = json!({"worker_instance_id": "w-1"});
        assert_eq!(
            upgrade_result(PROTOCOL_VERSION, method::SESSION_READ, current.clone()),
            Ok(current)
        );
    }

    #[test]
    fn upgrade_result_rejects_a_payload_that_already_has_the_current_spelling() {
        let error = upgrade_result(
            MIN_PROTOCOL_VERSION,
            method::SESSION_READ,
            json!({"runtime_id": "w-1", "worker_instance_id": "w-2"}),
        )
        .expect_err("a previous-version daemon never writes the current key");
        assert_eq!(
            error,
            CompatError::Conflict {
                site: "result",
                key: "worker_instance_id"
            }
        );
    }

    #[test]
    fn upgrade_event_restamps_translates_and_keeps_the_correlation_id() {
        let event = Event::new(
            MIN_PROTOCOL_VERSION,
            "agent_state",
            json!({"session_id": "s-1", "runtime": {"runtime_id": "w-1", "runtime_generation": "1"}}),
        )
        .expect("event")
        .with_id("req-1")
        .expect("id");
        let upgraded = upgrade_event(event).expect("adapter");
        assert_eq!(upgraded.version(), PROTOCOL_VERSION);
        assert_eq!(upgraded.id(), Some("req-1"));
        assert_eq!(
            upgraded.payload(),
            &json!({"session_id": "s-1", "runtime": {"worker_instance_id": "w-1", "runtime_generation": "1"}})
        );
    }

    #[test]
    fn upgrade_event_passes_an_event_without_renamed_keys_through() {
        let event = Event::new(
            MIN_PROTOCOL_VERSION,
            "event_from_the_future",
            json!({"runtime_id": "kept"}),
        )
        .expect("event");
        let upgraded = upgrade_event(event).expect("adapter");
        assert_eq!(upgraded.payload(), &json!({"runtime_id": "kept"}));
    }

    #[test]
    fn requests_that_change_or_are_refused_need_the_version_first() {
        assert!(request_is_version_independent(
            method::SESSION_LIST,
            &json!({})
        ));
        assert!(request_is_version_independent(
            method::SESSION_OUTPUT,
            &json!({"session_id": "s-1"})
        ));
        assert!(!request_is_version_independent(
            method::SESSION_OUTPUT,
            &json!({"session_id": "s-1", "runtime": {"worker_instance_id": "w-1", "runtime_generation": "1"}})
        ));
        assert!(!request_is_version_independent(
            method::SESSION_REPORT_NATIVE_ID,
            &json!({"worker_instance_id": "w-1"})
        ));
        assert!(!request_is_version_independent(
            method::PACKAGE_LIST,
            &json!(null)
        ));
    }

    #[test]
    fn renamed_keys_list_the_older_spellings_only_for_an_older_version() {
        assert_eq!(
            super::renamed_keys(MIN_PROTOCOL_VERSION),
            [
                ("runtime_id", "worker_instance_id"),
                ("previous_runtime_id", "previous_worker_instance_id")
            ]
        );
        assert!(super::renamed_keys(PROTOCOL_VERSION).is_empty());
    }

    #[test]
    fn range_forwarding_methods_are_registered_read_only_methods_without_adapter_sites() {
        for name in super::RANGE_FORWARDING_METHODS {
            assert!(
                method::METHOD_SPECS.iter().any(|spec| spec.name == *name),
                "{name} is not a method"
            );
            assert!(
                request_is_version_independent(name, &json!({"force": false})),
                "{name} would need a translated request"
            );
            let result = json!([]);
            assert_eq!(
                upgrade_result(MIN_PROTOCOL_VERSION, name, result.clone()),
                Ok(result),
                "{name} would need a translated result"
            );
        }
    }
}
