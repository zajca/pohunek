//! CLI command implementations.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. Each
//! command supports `--json` where the plan calls for machine-readable output
//! (see `docs/plan-phase-1.md` "CLI Grammar").

use protocol::{ProtocolError, Request, SUPPORTED_PROTOCOL_VERSIONS};
use serde::Serialize;

use crate::error::CliError;

pub(crate) mod assistant;
pub(crate) mod attach;
pub(crate) mod daemon;
pub(crate) mod discovery_cache;
pub(crate) mod doctor;
pub(crate) mod health;
pub(crate) mod host;
pub(crate) mod host_fanout;
pub(crate) mod integration;
pub(crate) mod migration;
pub(crate) mod notifications;
pub(crate) mod project;
pub(crate) mod prompt;
pub(crate) mod session;
pub(crate) mod setup;

/// Render a command result as the pretty-printed JSON document (with a trailing
/// newline) emitted under `--json`.
///
/// One place defines the `--json` success shape so every read/automation command
/// serializes its result type identically and a script receives exactly one JSON
/// document on stdout.
pub(crate) fn render_json<T: Serialize + ?Sized>(value: &T) -> Result<String, CliError> {
    render_json_document(&JsonEnvelope {
        cli_version: env!("CARGO_PKG_VERSION"),
        protocol: SUPPORTED_PROTOCOL_VERSIONS,
        ok: Some(value),
        err: None,
    })
}

/// Render a typed error in the same versioned process envelope as successes.
pub(crate) fn render_json_error(error: &ProtocolError) -> Result<String, CliError> {
    render_json_document(&JsonEnvelope::<()> {
        cli_version: env!("CARGO_PKG_VERSION"),
        protocol: SUPPORTED_PROTOCOL_VERSIONS,
        ok: None,
        err: Some(error),
    })
}

#[derive(Serialize)]
struct JsonEnvelope<'a, T: ?Sized> {
    cli_version: &'static str,
    protocol: protocol::ProtocolVersionRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<&'a T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<&'a ProtocolError>,
}

fn render_json_document<T: Serialize + ?Sized>(value: &T) -> Result<String, CliError> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

#[cfg(test)]
pub(crate) fn parse_json_ok<T: serde::de::DeserializeOwned>(document: &str) -> T {
    let envelope: serde_json::Value = serde_json::from_str(document).expect("parse JSON envelope");
    serde_json::from_value(envelope["ok"].clone()).expect("parse JSON success payload")
}

/// Build a unique correlation id for a single control request.
///
/// Delegates to the public SDK helper so the CLI, GUI, and direct SDK callers
/// share one request-id convention.
pub(crate) fn request_id(method: &str) -> String {
    pohunek_client::next_request_id(method)
}

/// Build a control [`Request`] for `method` carrying `params` as its JSON body.
///
/// One place defines the request envelope every command sends: a fresh per-call
/// [`request_id`] plus the `serde_json`-serialized params. Centralizing it keeps
/// the id format and the params encoding identical across `session`, `project`,
/// `attach`, `host`, and `assistant`, so a daemon sees byte-identical envelopes
/// regardless of which command emitted them.
///
/// # Errors
///
/// Returns [`CliError`] when `params` cannot be serialized to JSON.
pub(crate) fn request_with_params<T>(method: &str, params: &T) -> Result<Request, CliError>
where
    T: Serialize + ?Sized,
{
    Ok(
        Request::new(request_id(method), method, serde_json::to_value(params)?)
            .map_err(pohunek_client::ClientError::from)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_prefixed_by_method_and_unique_per_call() {
        let a = request_id("daemon.health");
        let b = request_id("daemon.health");
        // Stable, human-readable prefix for log greps.
        assert!(a.starts_with("sdk-daemon.health-"), "id: {a}");
        assert!(b.starts_with("sdk-daemon.health-"), "id: {b}");
        // Two calls (as in concurrent discover probes) never collide.
        assert_ne!(a, b, "request ids must be unique per call");
    }

    #[test]
    fn json_success_envelope_is_versioned_and_has_one_result_arm() {
        let doc = render_json(&serde_json::json!({"value": 42})).expect("render envelope");
        let value: serde_json::Value = serde_json::from_str(&doc).expect("parse envelope");
        assert_eq!(value["cli_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["protocol"]["minimum"], 2);
        assert_eq!(value["protocol"]["maximum"], 2);
        assert_eq!(value["ok"]["value"], 42);
        assert!(value.get("err").is_none());
    }

    #[test]
    fn json_error_envelope_is_versioned_and_has_one_error_arm() {
        let error = ProtocolError::agent_binary_missing("codex");
        let doc = render_json_error(&error).expect("render envelope");
        let value: serde_json::Value = serde_json::from_str(&doc).expect("parse envelope");
        assert_eq!(value["err"]["code"], "agent_binary_missing");
        assert!(value.get("ok").is_none());
    }
}
