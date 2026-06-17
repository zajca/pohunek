//! `zagentmesh health` (alias `status`) — query the daemon over the socket.
//!
//! Connects to the control socket, issues `daemon.health`, and prints the
//! daemon version + protocol version as a table or, with `--json`, the raw
//! payload (see `docs/plan-phase-1.md` "CLI Grammar": `--json` on `status`).

use protocol::{method, Request};
use serde_json::Value;

use crate::client::LocalClient;
use crate::error::CliError;
use crate::paths::Paths;

/// A simple monotonic-ish request id for correlation in logs. Phase 1 is
/// single-shot, so a per-invocation prefix plus the method is sufficient.
fn request_id(method: &str) -> String {
    format!("cli-{method}")
}

/// Run `health`/`status` against the local daemon.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable or returns an error.
pub(crate) async fn run(paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = LocalClient::connect(&paths.socket).await?;

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
