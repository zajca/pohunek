//! Edge adapters between the current public protocol and an older version.
//!
//! The window is [`crate::SUPPORTED_PROTOCOL_VERSIONS`]. Core types describe only
//! the current wire shape; an older version in the window has one adapter module
//! that translates in both directions. A daemon translates an older peer's
//! requests up to the current shape and its own results and events down
//! ([`upgrade_request`], [`downgrade_response`], [`downgrade_event`]). A client
//! talking to an older daemon does the inverse ([`downgrade_request`],
//! [`upgrade_result`], [`upgrade_event`]). The negotiated version is fixed per
//! connection, so a connection is adapted for its whole life.
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
//! The window holds the current version only since protocol 5 removed the
//! notification update, delete, policy and retention methods, so no adapter
//! exists and every older version fails with [`CompatError::UnsupportedVersion`].
//! The entry points stay so the next window brings its adapter without touching
//! the callers.
//!
//! Observable behavior is asserted at supported boundaries only: the real
//! daemon in `crates/daemon/tests/protocol_window.rs`.

// Rust guideline compliant 2026-10-09

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
    let _ = version;
    &[]
}

/// Lists the keys `version` spelled differently, as `(older, current)`.
///
/// Empty for the current version. Clients that cannot link the adapter (the
/// TypeScript SDK) generate their key mapping from this table.
#[must_use]
pub fn renamed_keys(version: ProtocolVersion) -> &'static [(&'static str, &'static str)] {
    let _ = version;
    &[]
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
    _method: &str,
    params: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(params),
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
pub fn result(
    version: ProtocolVersion,
    _method: &str,
    result: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(result),
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
    _name: &str,
    payload: Value,
) -> Result<Option<Value>, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(Some(payload)),
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
    _method: &str,
    params: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(params),
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
    _method: &str,
    result: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(result),
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
    _name: &str,
    payload: Value,
) -> Result<Value, CompatError> {
    match adapter(version)? {
        Adapter::Current => Ok(payload),
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
}

fn adapter(version: ProtocolVersion) -> Result<Adapter, CompatError> {
    if version == PROTOCOL_VERSION {
        return Ok(Adapter::Current);
    }
    Err(CompatError::UnsupportedVersion(version))
}
