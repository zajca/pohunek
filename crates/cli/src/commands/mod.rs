//! CLI command implementations.
//!
//! Milestone 2 commands: `doctor`, `daemon start`, and `health`/`status`. Each
//! command supports `--json` where the plan calls for machine-readable output
//! (see `docs/plan-phase-1.md` "CLI Grammar").

use serde::Serialize;

use crate::error::CliError;

pub(crate) mod attach;
pub(crate) mod daemon;
pub(crate) mod doctor;
pub(crate) mod health;
pub(crate) mod integration;
pub(crate) mod session;

/// Render a command result as the pretty-printed JSON document (with a trailing
/// newline) emitted under `--json`.
///
/// One place defines the `--json` success shape so every read/automation command
/// serializes its result type identically and a script receives exactly one JSON
/// document on stdout.
pub(crate) fn render_json<T: Serialize + ?Sized>(value: &T) -> Result<String, CliError> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}
