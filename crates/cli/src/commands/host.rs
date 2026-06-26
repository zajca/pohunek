//! `pohunek host` — discover, list, and inspect remote hosts over `NetBird`.
//!
//! `discover` and `list` enumerate the local host's `NetBird` peers and classify
//! each by probing its daemon control port, so the operator (and the rofi
//! switcher) sees which peers run a compatible daemon. That work now lives in the
//! **local daemon**, which caches the result for a short TTL so repeated calls
//! (e.g. every launcher keypress) return instantly; this command is a thin client
//! that asks the local daemon and renders the records. `inspect <host>` is a
//! *live* query against a specific host's daemon for its [`HostCapabilities`].
//!
//! Without a persistence store (out of scope for this milestone), the set of
//! "known hosts" is exactly the set of live `NetBird` peers, so `list` and
//! `discover` share one core; they differ only in presentation.

use std::fmt::Write as _;

use protocol::{method, HostCapabilities, HostClass, HostDiscoverParams, HostRecord};
use serde_json::Value;

use crate::client::Client;
use crate::commands::request_with_params;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Run `host discover`: ask the local daemon for its classified `NetBird` peers.
///
/// Discovery is inherently a *local* operation (it enumerates this machine's
/// mesh view), so it always dials the local daemon regardless of any `--host`
/// flag — the same rule `integration install` follows.
///
/// # Errors
///
/// Returns [`CliError`] when the local daemon is unreachable, rejects the request
/// (e.g. `NetBird` state cannot be read), or returns an unexpected payload.
pub(crate) async fn run_discover(paths: &Paths, json: bool) -> Result<(), CliError> {
    let records = fetch_records(paths).await?;
    if json {
        print!("{}", crate::commands::render_json(&records)?);
    } else {
        print!("{}", render_records_human(&records));
    }
    Ok(())
}

/// Run `host list`: the same discovery core as `discover`, emphasizing the
/// name / IP / classification / version columns.
///
/// Without a persistence store, "known hosts" are the live `NetBird` peers, so this
/// shares [`run_discover`]; the human rendering is identical for now.
///
/// # Errors
///
/// Same as [`run_discover`].
pub(crate) async fn run_list(paths: &Paths, json: bool) -> Result<(), CliError> {
    run_discover(paths, json).await
}

/// Ask the local daemon to enumerate and classify `NetBird` peers.
async fn fetch_records(paths: &Paths) -> Result<Vec<HostRecord>, CliError> {
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    // `force: false` uses the daemon's cached snapshot when fresh; the launcher
    // calls discover on every keypress, so the cache is what keeps it instant.
    let request = request_with_params(method::HOST_DISCOVER, &HostDiscoverParams { force: false })?;
    let result = client.request(&request).await?;
    Ok(serde_json::from_value(result)?)
}

/// Run `host inspect <host>`: a live capability query against the host's daemon.
///
/// # Errors
///
/// Returns [`CliError`] when the host cannot be resolved or reached, or the
/// daemon returns an error or an unexpected payload.
pub(crate) async fn run_inspect(host: &str, paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let request = request_with_params(method::HOST_INSPECT, &Value::Null)?;
    let result = client.request(&request).await?;
    let caps: HostCapabilities = serde_json::from_value(result)?;

    if json {
        print!("{}", crate::commands::render_json(&caps)?);
    } else {
        print!("{}", render_capabilities_human(host, &caps));
    }
    Ok(())
}

/// Render the discovered hosts as an aligned table.
fn render_records_human(records: &[HostRecord]) -> String {
    let name_of = |r: &HostRecord| r.name.clone().unwrap_or_else(|| "-".to_owned());
    let ip_of = |r: &HostRecord| r.netbird_ip.clone().unwrap_or_else(|| "-".to_owned());

    let name_width = records
        .iter()
        .map(|r| name_of(r).len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let ip_width = records
        .iter()
        .map(|r| ip_of(r).len())
        .max()
        .unwrap_or(0)
        .max("NETBIRD_IP".len());

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{:<name_width$}  {:<ip_width$}  STATUS         VERSION",
        "NAME", "NETBIRD_IP",
    );
    for r in records {
        let (status, version) = class_columns(&r.class);
        let _ = writeln!(
            output,
            "{:<name_width$}  {:<ip_width$}  {status:<13}  {version}",
            name_of(r),
            ip_of(r),
        );
    }
    output
}

/// The status label + version cell for a classification.
fn class_columns(class: &HostClass) -> (&'static str, String) {
    match class {
        HostClass::ReachableDaemon { daemon_version } => ("reachable", daemon_version.clone()),
        HostClass::VersionMismatch {
            daemon_protocol_version,
        } => ("version_skew", format!("proto {daemon_protocol_version}")),
        HostClass::Unreachable => ("unreachable", "-".to_owned()),
        HostClass::Candidate => ("candidate", "-".to_owned()),
    }
}

/// Render a host's capability snapshot as a human table.
fn render_capabilities_human(host: &str, caps: &HostCapabilities) -> String {
    let mut output = format!("host {host} capabilities\n");
    let _ = writeln!(output, "  daemon_version:     {}", caps.daemon_version);
    let _ = writeln!(output, "  protocol_version:   {}", caps.protocol_version);
    let _ = writeln!(output, "  git_available:      {}", caps.git_available);
    let _ = writeln!(output, "  worktree_supported: {}", caps.worktree_supported);
    output.push_str("  supported_agents:   ");
    // Agent names are free strings since Part C (base kinds + host profiles).
    output.push_str(&caps.supported_agents.join(", "));
    output.push('\n');
    output.push_str("  runtimes:\n");
    for rt in &caps.runtimes {
        let path = rt.path.as_deref().unwrap_or("-");
        let _ = writeln!(
            output,
            "    {:<8} available={:<5} path={path}",
            rt.agent, rt.available,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use protocol::{AgentRuntime, ProtocolVersion};

    use super::*;

    #[test]
    fn renders_capabilities_table() {
        let caps = HostCapabilities {
            daemon_version: "0.1.0".to_owned(),
            protocol_version: ProtocolVersion(1),
            supported_agents: vec!["shell".to_owned(), "codex".to_owned(), "claude".to_owned()],
            runtimes: vec![
                AgentRuntime {
                    agent: "shell".to_owned(),
                    available: true,
                    path: None,
                },
                AgentRuntime {
                    agent: "claude".to_owned(),
                    available: true,
                    path: Some("/usr/bin/claude".to_owned()),
                },
            ],
            git_available: true,
            worktree_supported: true,
        };
        let output = render_capabilities_human("host-b", &caps);
        assert!(output.contains("host host-b capabilities"));
        assert!(output.contains("daemon_version:     0.1.0"));
        assert!(output.contains("protocol_version:   1"));
        assert!(output.contains("shell, codex, claude"));
        assert!(output.contains("claude   available=true  path=/usr/bin/claude"));
        assert!(output.contains("shell    available=true  path=-"));
    }

    #[test]
    fn renders_discovery_table_with_each_classification() {
        let records = vec![
            HostRecord {
                name: Some("host-b".to_owned()),
                fqdn: Some("host-b.netbird.cloud".to_owned()),
                netbird_ip: Some("100.92.30.40".to_owned()),
                class: HostClass::ReachableDaemon {
                    daemon_version: "0.1.0".to_owned(),
                },
            },
            HostRecord {
                name: Some("host-c".to_owned()),
                fqdn: Some("host-c.netbird.cloud".to_owned()),
                netbird_ip: Some("100.92.30.41".to_owned()),
                class: HostClass::Candidate,
            },
        ];
        let output = render_records_human(&records);
        let header = output.lines().next().expect("header");
        for column in ["NAME", "NETBIRD_IP", "STATUS", "VERSION"] {
            assert!(header.contains(column), "header missing {column}: {header}");
        }
        assert!(output.contains("host-b"));
        assert!(output.contains("reachable"));
        assert!(output.contains("0.1.0"));
        assert!(output.contains("candidate"));
    }
}
