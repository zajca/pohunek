//! `pohunek host` — discover, list, and inspect remote hosts over `NetBird`.
//!
//! `discover` and `list` enumerate the local host's `NetBird` peers and classify
//! each by probing its daemon control port, so the operator (and the rofi
//! switcher) sees which peers run a compatible daemon. Discovery is performed
//! directly by the CLI and uses its owner-private persistent cache, so it does
//! not require a local daemon. `inspect <host>` is a
//! *live* query against a specific host's daemon for its [`HostCapabilities`].
//!
//! Without a persistence store (out of scope for this milestone), the set of
//! "known hosts" is exactly the set of live `NetBird` peers, so `list` and
//! `discover` share one core; they differ only in presentation.

use std::fmt::Write as _;

use protocol::{method, HostCapabilities, HostClass, HostRecord};

use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;

/// Run `host discover` using local `NetBird` state and the standalone cache.
///
/// # Errors
///
/// Returns [`CliError`] when `NetBird` state cannot be read, remote-port
/// configuration is invalid, or the persistent cache cannot be safely used.
pub(crate) async fn run_discover(refresh: bool, json: bool) -> Result<(), CliError> {
    let cache_dir = Paths::cache_dir_only()?;
    let records = fetch_records(&cache_dir, refresh).await?;
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
pub(crate) async fn run_list(refresh: bool, json: bool) -> Result<(), CliError> {
    run_discover(refresh, json).await
}

/// Enumerate peers through standalone discovery without a local control socket.
pub(crate) async fn fetch_records(
    cache_dir: &std::path::Path,
    refresh: bool,
) -> Result<Vec<HostRecord>, CliError> {
    crate::commands::discovery_cache::records(cache_dir, refresh).await
}

/// Run `host inspect <host>`: a live capability query against the host's daemon.
///
/// # Errors
///
/// Returns [`CliError`] when the host cannot be resolved or reached, or the
/// daemon returns an error or an unexpected payload.
pub(crate) async fn run_inspect(host: &str, paths: &Paths, json: bool) -> Result<(), CliError> {
    let mut client = Client::connect(host, paths).await?;
    let caps: HostCapabilities = client.call::<method::HostInspect>(()).await?;

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
    let ip_of = |r: &HostRecord| r.address.clone().unwrap_or_else(|| "-".to_owned());

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
        let version = rt.version.as_deref().unwrap_or("-");
        let supported = rt
            .supported
            .map_or("-", |supported| if supported { "true" } else { "false" });
        let _ = writeln!(
            output,
            "    {:<8} available={:<5} supported={supported:<5} version={version} path={path}",
            rt.agent, rt.available,
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use protocol::{AgentKind, AgentRuntime, ProtocolVersion};

    use super::*;

    #[test]
    fn renders_capabilities_table() {
        let caps = HostCapabilities {
            daemon_version: "0.1.0".to_owned(),
            protocol_version: ProtocolVersion::new(1).expect("valid protocol version"),
            terminal_read_supported: true,
            output_read_supported: true,
            session_wait_supported: true,
            supported_agents: vec![
                "shell".to_owned(),
                "codex".to_owned(),
                "claude".to_owned(),
                "hermes".to_owned(),
            ],
            runtimes: vec![
                AgentRuntime {
                    agent: "shell".to_owned(),
                    agent_base: Some(AgentKind::Shell),
                    available: true,
                    path: None,
                    version: None,
                    supported: None,
                },
                AgentRuntime {
                    agent: "claude".to_owned(),
                    agent_base: Some(AgentKind::Claude),
                    available: true,
                    path: Some("/usr/bin/claude".to_owned()),
                    version: None,
                    supported: None,
                },
                AgentRuntime {
                    agent: "hermes".to_owned(),
                    agent_base: Some(AgentKind::Hermes),
                    available: true,
                    path: Some("/usr/bin/hermes".to_owned()),
                    version: Some("0.20.0".to_owned()),
                    supported: Some(true),
                },
            ],
            git_available: true,
            worktree_supported: true,
        };
        let output = render_capabilities_human("host-b", &caps);
        assert!(output.contains("host host-b capabilities"));
        assert!(output.contains("daemon_version:     0.1.0"));
        assert!(output.contains("protocol_version:   1"));
        assert!(output.contains("shell, codex, claude, hermes"));
        assert!(output
            .contains("claude   available=true  supported=-     version=- path=/usr/bin/claude"));
        assert!(output.contains("shell    available=true  supported=-     version=- path=-"));
        assert!(output.contains(
            "hermes   available=true  supported=true  version=0.20.0 path=/usr/bin/hermes"
        ));
    }

    #[test]
    fn renders_discovery_table_with_each_classification() {
        let records = vec![
            HostRecord {
                name: Some("host-b".to_owned()),
                fqdn: Some("host-b.netbird.cloud".to_owned()),
                address: Some("100.92.30.40".to_owned()),
                overlay: "netbird".to_owned(),
                class: HostClass::ReachableDaemon {
                    daemon_version: "0.1.0".to_owned(),
                },
            },
            HostRecord {
                name: Some("host-c".to_owned()),
                fqdn: Some("host-c.netbird.cloud".to_owned()),
                address: Some("100.92.30.41".to_owned()),
                overlay: "netbird".to_owned(),
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
