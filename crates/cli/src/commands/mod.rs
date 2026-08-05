//! CLI command implementations.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. Each
//! command supports `--json` where the plan calls for machine-readable output
//! (see `docs/plan-phase-1.md` "CLI Grammar").

use protocol::Request;
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
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
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
    Ok(Request::new(
        request_id(method),
        method,
        serde_json::to_value(params)?,
    ))
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
}
