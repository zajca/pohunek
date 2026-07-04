//! `pohunek health` (alias `status`) — query a daemon over the control plane.
//!
//! Connects to the daemon for the effective host (local Unix socket or a remote
//! `NetBird` TCP connection), issues `daemon.health`, and prints the daemon and
//! protocol versions as a table or, with `--json`, the raw payload (see
//! `docs/plan-phase-1.md` "CLI Grammar": `--json` on `status`).

use protocol::method;

use crate::client::Client;
use crate::commands::render_json;
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
    let result = client.call::<method::DaemonHealth>(()).await?;

    if json {
        print!("{}", render_json(&result)?);
        return Ok(());
    }

    println!("FIELD             VALUE");
    println!("status            {}", result.status);
    println!("daemon_version    {}", result.daemon_version);
    println!("protocol_version  {}", result.protocol_version);

    Ok(())
}
