//! `zagentmesh health` (alias `status`) — query a daemon over the control plane.
//!
//! Connects to the daemon for the effective host (local Unix socket or a remote
//! NetBird TCP connection), issues `daemon.health`, and prints the daemon and
//! protocol versions as a table or, with `--json`, the raw payload (see
//! `docs/plan-phase-1.md` "CLI Grammar": `--json` on `status`).

use protocol::{method, Request};
use serde_json::Value;

use crate::client::Client;
use crate::commands::request_id;
use crate::error::CliError;
use crate::paths::Paths;

/// Run `health`/`status` against the daemon for `host`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, the host cannot be
/// resolved, or the daemon returns an error.
pub(crate) async fn run(host: &str, paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;

    let request = Request::new(
        request_id(method::DAEMON_HEALTH),
        method::DAEMON_HEALTH,
        Value::Null,
    );
    let result = client.request(&request).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    // Human table. Pull known fields defensively; unknown shapes still print.
    let daemon_version = result
        .get("daemon_version")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let protocol_version = result
        .get("protocol_version")
        .map(value_to_string)
        .unwrap_or_else(|| "<unknown>".to_owned());
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");

    println!("FIELD             VALUE");
    println!("status            {status}");
    println!("daemon_version    {daemon_version}");
    println!("protocol_version  {protocol_version}");

    Ok(())
}

/// Stringify a JSON scalar without surrounding quotes for table display.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
