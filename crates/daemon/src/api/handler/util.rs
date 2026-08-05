//! Shared request-parsing, response-building, and blocking-offload helpers.
//!
//! Every method handler under [`super`] uses these to parse `request.params`
//! into a typed struct, serialize a typed result into a [`Response`], and push
//! fallible blocking work off the async runtime with a uniform panic-to-error
//! mapping. Keeping them here means the per-domain handler modules share one
//! implementation of each convention rather than re-deriving it.

use protocol::{
    negotiate, ProtocolError, ProtocolVersion, Request, Response, SUPPORTED_PROTOCOL_VERSIONS,
};
use serde::Serialize;
use serde_json::Value;

/// Run a fallible blocking operation off the async runtime and map its result to
/// a [`Response`]: a serialized value on success, the operation's typed error on
/// failure, and a daemon-class error built from `panic_code`/`panic_msg`/
/// `panic_hint` if the `spawn_blocking` task panics (the `JoinError` case).
pub(super) async fn run_blocking<T, F>(
    request: &Request,
    op: F,
    panic_code: &'static str,
    panic_msg: &'static str,
    panic_hint: Option<&'static str>,
) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T, protocol::ProtocolError> + Send + 'static,
{
    match tokio::task::spawn_blocking(op).await {
        Ok(Ok(value)) => ok_value(request, &value),
        Ok(Err(err)) => error_value(request, err),
        Err(_) => error_value(
            request,
            protocol::ProtocolError::new(
                protocol::ErrorClass::Daemon,
                panic_code,
                panic_msg,
                panic_hint.map(str::to_owned),
            ),
        ),
    }
}

/// Deserialize required `request.params` into `T`, or a typed `bad_request`.
pub(super) fn parse_params<T>(request: &Request) -> Result<T, protocol::ProtocolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value::<T>(request.params().clone()).map_err(|err| {
        protocol::ProtocolError::bad_request(format!(
            "invalid params for {}: {err}",
            request.method()
        ))
    })
}

/// Deserialize optional `request.params` into `T`, defaulting when `null`.
pub(super) fn parse_optional_params<T>(request: &Request) -> Result<T, protocol::ProtocolError>
where
    T: serde::de::DeserializeOwned + Default,
{
    if request.params().is_null() {
        Ok(T::default())
    } else {
        parse_params(request)
    }
}

/// Serialize a typed result into an `ok` [`Response`], or a daemon-class error.
pub(super) fn ok_value<T>(request: &Request, value: &T) -> Response
where
    T: Serialize,
{
    match serde_json::to_value(value) {
        Ok(value) => response_ok(request, value),
        Err(err) => error_value(
            request,
            protocol::ProtocolError::new(
                protocol::ErrorClass::Daemon,
                "serialize_failed",
                format!("failed to serialize response: {err}"),
                None,
            ),
        ),
    }
}

/// Return the highest protocol version shared by this request and the daemon.
///
/// Dispatch validates overlap before invoking a method handler. Keeping this
/// invariant in one helper ensures every success and error uses the selected
/// version rather than the daemon's maximum version.
pub(super) fn selected_version(request: &Request) -> ProtocolVersion {
    negotiate(request.version_range(), SUPPORTED_PROTOCOL_VERSIONS)
        .expect("dispatch negotiates the public protocol before handler execution")
}

/// Build a JSON success response at the selected protocol version.
pub(super) fn response_ok(request: &Request, value: Value) -> Response {
    Response::ok(selected_version(request), request.id(), value)
        .expect("deserialized request ids satisfy response validation")
}

/// Build a typed error response at the selected protocol version.
pub(super) fn error_value(request: &Request, error: ProtocolError) -> Response {
    Response::err(selected_version(request), request.id(), error)
        .expect("deserialized request ids satisfy response validation")
}

/// Return the single `attach` stream id if `line` is exactly an attach prelude.
///
/// The prelude is a one-field object `{"attach":"<stream-id>"}`; any other shape
/// (extra fields, empty id, non-string value) yields `None` so a normal request
/// is never mistaken for an attach handoff.
pub(super) fn parse_attach_prelude(line: &str) -> Option<String> {
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
