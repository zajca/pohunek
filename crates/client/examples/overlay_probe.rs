//! Overlay SDK probe: one remote host reached through the public client API.
//!
//! The acceptance harness (`scripts/acceptance/tailscale-overlay`, issue #1112)
//! runs this binary on a host whose `overlays.toml` selects an overlay provider
//! and checks that an external SDK consumer, not the CLI, can resolve a peer,
//! connect, negotiate a protocol version, list sessions, read the daemon's
//! overlay listener checks and read a session screen.
//!
//! The binary prints exactly one JSON document on stdout. On success it is
//! `{"ok": true, ...}`; on failure `{"ok": false, "step": ..., "error": ...}`
//! with a non-zero exit status. Nothing secret is read or printed.
//!
//! Usage: `overlay_probe --host HOST --session SESSION_ID [--mark TEXT]
//! [--config-dir DIR] [--resolve-timeout-ms MS]`

use std::{path::Path, process::ExitCode, time::Duration};

use pohunek_client::{
    default_overlay_registry, overlay_registry_from, resolve_host_peer_in, resolve_host_target_in,
    Client, ResolvedHost,
};
use protocol::{SessionId, SessionListParams, SessionScreenParams};
use serde_json::{json, Value};

/// Upper bound on resolving the host name against the overlay provider. The
/// provider call is a local CLI invocation, so seconds are ample; a longer
/// wait would only hide a hung provider.
const DEFAULT_RESOLVE_TIMEOUT_MS: u64 = 15_000;

/// Doctor check names ending in this suffix report an overlay listener.
const LISTENER_CHECK_SUFFIX: &str = "_listener";

/// The local socket path handed to the connector. A remote host never opens
/// it: the probe refuses a host that resolves to this machine before it
/// connects.
const UNUSED_LOCAL_SOCKET: &str = "";

/// Parsed command line.
struct Args {
    host: String,
    session: String,
    mark: Option<String>,
    config_dir: Option<String>,
    resolve_timeout: Duration,
}

/// A failed probe step, reported as the `step` and `error` of the JSON result.
struct ProbeError {
    step: &'static str,
    message: String,
}

impl ProbeError {
    fn new(step: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            step,
            message: error.to_string(),
        }
    }
}

fn parse_args(raw: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut host = None;
    let mut session = None;
    let mut mark = None;
    let mut config_dir = None;
    let mut resolve_timeout_ms = DEFAULT_RESOLVE_TIMEOUT_MS;
    let mut raw = raw;
    while let Some(flag) = raw.next() {
        let mut value = |name: &str| raw.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--host" => host = Some(value("--host")?),
            "--session" => session = Some(value("--session")?),
            "--mark" => mark = Some(value("--mark")?),
            "--config-dir" => config_dir = Some(value("--config-dir")?),
            "--resolve-timeout-ms" => {
                resolve_timeout_ms = value("--resolve-timeout-ms")?
                    .parse()
                    .map_err(|error| format!("--resolve-timeout-ms: {error}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        host: host.ok_or("--host is required")?,
        session: session.ok_or("--session is required")?,
        mark,
        config_dir,
        resolve_timeout: Duration::from_millis(resolve_timeout_ms),
    })
}

/// Run every probe step against `args.host` and return the result document.
async fn probe(args: &Args) -> Result<Value, ProbeError> {
    let registry = match &args.config_dir {
        Some(dir) => overlay_registry_from(Path::new(dir)),
        None => default_overlay_registry(),
    }
    .map_err(|error| ProbeError::new("registry", error))?;

    let resolved = resolve_host_target_in(&args.host, args.resolve_timeout, &registry)
        .await
        .map_err(|error| ProbeError::new("resolve", error))?;
    let ResolvedHost::Remote { pinned_route, .. } = resolved else {
        return Err(ProbeError::new(
            "resolve",
            "the host resolves to this machine; the probe needs a remote host",
        ));
    };
    let peer = resolve_host_peer_in(&args.host, args.resolve_timeout, &registry)
        .await
        .map_err(|error| ProbeError::new("resolve", error))?
        .ok_or_else(|| ProbeError::new("resolve", "the host resolved to no peer"))?;

    let mut client =
        Client::connect_with_registry(&args.host, Path::new(UNUSED_LOCAL_SOCKET), registry)
            .await
            .map_err(|error| ProbeError::new("connect", error))?;
    let version = client
        .handshake()
        .await
        .map_err(|error| ProbeError::new("handshake", error))?;

    let sessions = client
        .call::<protocol::method::SessionList>(SessionListParams::default())
        .await
        .map_err(|error| ProbeError::new("session_list", error))?;
    let session_ids: Vec<String> = sessions
        .iter()
        .filter_map(|info| serde_json::to_value(info).ok())
        .filter_map(|info| info.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();

    let report = client
        .daemon_doctor()
        .await
        .map_err(|error| ProbeError::new("daemon_doctor", error))?;
    let listener_checks: Vec<Value> = report
        .checks
        .iter()
        .filter(|check| check.name.ends_with(LISTENER_CHECK_SUFFIX))
        .filter_map(|check| serde_json::to_value(check).ok())
        .collect();

    let screen = client
        .session_screen(SessionScreenParams::new(SessionId(args.session.clone())))
        .await
        .map_err(|error| ProbeError::new("session_screen", error))?;
    let mark_line = args.mark.as_deref().and_then(|mark| {
        screen
            .visible_lines
            .iter()
            .rev()
            .find(|line| line.contains(mark))
            .cloned()
    });

    Ok(json!({
        "ok": true,
        "route": {
            "overlay": peer.overlay,
            "peer_id": peer.peer_id,
            "fqdn": peer.fqdn,
            "address": peer.address.ip().to_string(),
            "port": peer.address.port(),
            "pinned_route": pinned_route,
        },
        "protocol_version": version.get(),
        "session_list": {
            "count": session_ids.len(),
            "contains_session": session_ids.contains(&args.session),
        },
        "listener_checks": listener_checks,
        "screen": {
            "session_id": args.session,
            "mark_line": mark_line,
        },
    }))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            println!(
                "{}",
                json!({"ok": false, "step": "arguments", "error": message})
            );
            return ExitCode::from(2);
        }
    };
    match probe(&args).await {
        Ok(document) => {
            println!("{document}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!(
                "{}",
                json!({"ok": false, "step": error.step, "error": error.message})
            );
            ExitCode::FAILURE
        }
    }
}
