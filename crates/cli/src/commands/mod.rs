//! CLI command implementations.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. Each
//! command supports `--json` where the plan calls for machine-readable output
//! (see `docs/plan-phase-1.md` "CLI Grammar").

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::Request;
use serde::Serialize;

use crate::error::CliError;

pub(crate) mod assistant;
pub(crate) mod attach;
pub(crate) mod daemon;
pub(crate) mod doctor;
pub(crate) mod health;
pub(crate) mod host;
pub(crate) mod host_fanout;
pub(crate) mod integration;
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

/// A token, computed once per process, that disambiguates this CLI invocation
/// from any other.
///
/// Derived from the process id and the process-start wall-clock time so two
/// separate `pohunek` runs — even of the same command at the "same" second —
/// get different tokens. It carries no secret material (a pid and a timestamp).
fn run_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        // Nanosecond wall clock at first use; falls back to 0 only if the clock
        // is before the epoch (it never is in practice).
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("{:x}{:x}", std::process::id(), nanos)
    })
}

/// Build a unique correlation id for a single control request.
///
/// Format: `cli-<method>-<run-token>-<seq>`. The `<method>` keeps ids readable
/// in logs; `<run-token>-<seq>` makes every id unique — across the concurrent
/// probes a single `host discover` fires, and across separate CLI invocations —
/// so one command correlates to exactly its own lines in both the local and the
/// remote daemon's logs (`DoD` #7). A bare `cli-<method>` would alias every
/// repeated or concurrent call of that method.
pub(crate) fn request_id(method: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("cli-{method}-{}-{seq}", run_token())
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
        assert!(a.starts_with("cli-daemon.health-"), "id: {a}");
        assert!(b.starts_with("cli-daemon.health-"), "id: {b}");
        // Two calls (as in concurrent discover probes) never collide.
        assert_ne!(a, b, "request ids must be unique per call");
    }
}
