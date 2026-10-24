//! Parsing of `tailscale status --json` and the subprocess that produces it.
//!
//! The JSON is the `ipnstate.Status` of `tailscale/tailscale@v1.104.2`
//! (`ipn/ipnstate/ipnstate.go`), whose help text calls the format subject to
//! change. Parsing is defensive: unknown fields are ignored, every field the
//! mapping uses is optional, and a Go `nil` slice or pointer arrives as `null`
//! and reads as empty. A field that is absent reads as its zero value, which
//! is the safe side for `BackendState`, `TUN` and `InNetworkMap`; the
//! `omitempty` flags `Expired` and `ShareeNode` read as unset when absent, which
//! the format forces.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use pohunek_platform::shell_env::{
    fallback_search_path, resolve_executable, resolve_executable_in_path_value,
    DARWIN_FALLBACK_DIRECTORIES,
};
use serde::Deserialize;
use tokio::sync::Semaphore;

/// Subcommand and flag used to ask Tailscale for machine-readable status.
const TAILSCALE_STATUS_ARGS: [&str; 2] = ["status", "--json"];
/// Program name of the Tailscale CLI, resolved with the shared
/// trusted-executable policy: first against the process `PATH`, then (macOS
/// only) in the fixed install directories. A Finder-launched app carries a
/// minimal `PATH` that omits the Homebrew and package-installer prefixes.
const TAILSCALE_PROGRAM: &str = "tailscale";
/// Maximum number of bytes of captured CLI output to surface in an error
/// message. Bounds the size of a [`TailscaleError::StateUnavailable`] detail so
/// a misbehaving CLI cannot flood logs or the agent context.
const MAX_ERROR_DETAIL_BYTES: usize = 512;

/// The `BackendState` value of a node that is connected to its tailnet.
const BACKEND_RUNNING: &str = "Running";

/// Every `BackendState` value of `ipn/backend.go` at v1.104.2. An unlisted
/// value is reported as unrecognized instead of echoed.
const KNOWN_BACKEND_STATES: [&str; 7] = [
    "NoState",
    "InUseOtherUser",
    "NeedsLogin",
    "NeedsMachineAuth",
    "Stopped",
    "Starting",
    BACKEND_RUNNING,
];

/// Errors raised while reading or interpreting Tailscale's local state.
#[derive(Debug, thiserror::Error)]
pub enum TailscaleError {
    /// No executable `tailscale` that the shared trusted-executable policy
    /// accepts was found on `PATH`.
    #[error("the `tailscale` CLI was not found on PATH")]
    CliMissing,
    /// Tailscale is installed but its local state could not be read or is not
    /// usable (daemon down, not logged in, userspace networking, or a non-zero
    /// exit). Carries a short, bounded detail.
    #[error("Tailscale local state is unavailable: {0}")]
    StateUnavailable(String),
    /// The `tailscale status --json` output could not be parsed.
    #[error("failed to parse `tailscale status --json`: {0}")]
    Parse(String),
    /// The requested host name did not match any selectable Tailscale peer.
    #[error("host '{0}' was not found among Tailscale peers")]
    HostUnknown(String),
    /// The selector matched more than one current peer.
    #[error("host '{host}' is ambiguous among Tailscale peers")]
    HostAmbiguous {
        /// The selector that matched several hosts.
        host: String,
        /// The hosts it matched.
        candidates: Vec<overlay::CandidateIdentity>,
    },
}

/// One node of the status: this host (`Self`) or a peer.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Node {
    /// Stable node identity; survives re-keying (`StableNodeID`).
    #[serde(rename = "ID")]
    id: Option<String>,
    /// Fully qualified `MagicDNS` name with a trailing dot.
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    /// Addresses assigned to the node.
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<Option<String>>>,
    /// Whether the node is connected to the control plane.
    #[serde(rename = "Online")]
    online: Option<bool>,
    /// Whether the peer is in the latest network map.
    #[serde(rename = "InNetworkMap")]
    in_network_map: Option<bool>,
    /// Whether the peer's node key has expired.
    #[serde(rename = "Expired")]
    expired: Option<bool>,
    /// Whether the peer is a node of a user this tailnet shared a machine to.
    #[serde(rename = "ShareeNode")]
    sharee: Option<bool>,
}

impl Node {
    /// The non-empty stable node identity.
    pub(crate) fn id(&self) -> Option<&str> {
        self.id.as_deref().filter(|id| !id.is_empty())
    }

    /// The `MagicDNS` name without its trailing dot; `None` when absent or empty.
    pub(crate) fn fqdn(&self) -> Option<&str> {
        self.dns_name
            .as_deref()
            .map(|name| name.trim_end_matches('.'))
            .filter(|name| !name.is_empty())
    }

    /// Every listed address that parses, in status order.
    pub(crate) fn addresses(&self) -> impl Iterator<Item = IpAddr> + '_ {
        self.tailscale_ips
            .iter()
            .flatten()
            .flatten()
            .filter_map(|text| text.parse::<IpAddr>().ok())
    }

    pub(crate) fn online(&self) -> bool {
        self.online.unwrap_or(false)
    }

    pub(crate) fn in_network_map(&self) -> bool {
        self.in_network_map.unwrap_or(false)
    }

    pub(crate) fn expired(&self) -> bool {
        self.expired.unwrap_or(false)
    }

    pub(crate) fn sharee_node(&self) -> bool {
        self.sharee.unwrap_or(false)
    }
}

/// A parsed `tailscale status --json` snapshot.
///
/// Construct via [`parse_status`] or [`run_status_async`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TailscaleStatus {
    /// Whether a kernel interface carries the traffic; false is userspace mode.
    #[serde(rename = "TUN")]
    tun: Option<bool>,
    /// The `ipn.State` string of the backend.
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    /// This host. `null` before the node has any state.
    #[serde(rename = "Self")]
    self_node: Option<Node>,
    /// Peers keyed by their current node public key, which changes on re-key
    /// and is therefore never used as an identity.
    #[serde(rename = "Peer")]
    peer: Option<BTreeMap<String, Option<Node>>>,
}

impl TailscaleStatus {
    /// This host, when the status carries it.
    pub(crate) fn self_node(&self) -> Option<&Node> {
        self.self_node.as_ref()
    }

    /// Every peer entry, in key order. A `null` entry is skipped.
    pub(crate) fn peer_nodes(&self) -> impl Iterator<Item = &Node> {
        self.peer.iter().flat_map(BTreeMap::values).flatten()
    }

    /// True when Tailscale is connected to its tailnet.
    pub(crate) fn is_running(&self) -> bool {
        self.backend_state.as_deref() == Some(BACKEND_RUNNING)
    }

    /// True when a kernel interface carries the traffic.
    pub(crate) fn has_tun(&self) -> bool {
        self.tun.unwrap_or(false)
    }

    /// Why this status cannot carry overlay traffic, or `None` when it can.
    ///
    /// Overlay traffic needs a running node on a kernel interface. Without one,
    /// a dial to a `100.64.0.0/10` address leaves through the default route,
    /// where that range can belong to an unrelated carrier network.
    pub(crate) fn unusable_reason(&self) -> Option<String> {
        if !self.is_running() {
            let state = self
                .backend_state
                .as_deref()
                .filter(|state| KNOWN_BACKEND_STATES.contains(state))
                .unwrap_or("unrecognized");
            return Some(format!("Tailscale is not running (state: {state})"));
        }
        if !self.has_tun() {
            return Some(
                "Tailscale runs in userspace networking mode, which has no kernel interface"
                    .to_owned(),
            );
        }
        None
    }
}

/// Parse the JSON text of `tailscale status --json` into a [`TailscaleStatus`].
///
/// Pure and fixture-tested. Never panics on malformed input.
///
/// # Errors
///
/// Returns [`TailscaleError::Parse`] when `json` is not valid `tailscale
/// status` output. The message carries the parser's position, never a value.
pub fn parse_status(json: &str) -> Result<TailscaleStatus, TailscaleError> {
    serde_json::from_str(json).map_err(|error| {
        TailscaleError::Parse(format!(
            "invalid JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })
}

/// Asynchronously run `tailscale status --json` and parse its output.
///
/// The program is the first trusted `tailscale` on the process `PATH` or, on
/// macOS, in the trusted fixed install directories, and that absolute path is
/// what runs, so the kernel does no second `PATH` search. The spawned command
/// is killed if this future is dropped, so the daemon can stop a pending
/// status probe during shutdown instead of waiting for an unresponsive CLI.
///
/// # Errors
///
/// - [`TailscaleError::CliMissing`] when no trusted `tailscale` is found.
/// - [`TailscaleError::StateUnavailable`] on a non-zero exit (daemon down),
///   carrying a short trimmed detail from stderr or stdout.
/// - [`TailscaleError::Parse`] when the output is not valid status JSON.
pub async fn run_status_async() -> Result<TailscaleStatus, TailscaleError> {
    let permit = LOOKUP_GATE.acquire().await.map_err(|_closed| {
        TailscaleError::StateUnavailable("the tailscale lookup gate is closed".to_owned())
    })?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // The lookup stats and opens files along every `PATH` entry, so a stalled
    // filesystem must not hold the caller's task. The future stays cancellable
    // while it waits for the gate and while the lookup runs, and an abandoned
    // lookup finishes on its own thread: unlike a Tokio blocking-pool task, it
    // never delays the runtime's shutdown.
    std::thread::Builder::new()
        .name("tailscale-lookup".to_owned())
        .spawn(move || {
            // The permit is released when the lookup ends, even after the
            // caller stopped waiting.
            let _permit = permit;
            // The receiver is gone when the caller's future was dropped.
            let _ = sender.send(resolve_host_program());
        })
        .map_err(|error| {
            TailscaleError::StateUnavailable(format!("cannot start the tailscale lookup: {error}"))
        })?;
    let program = receiver.await.map_err(|_closed| {
        TailscaleError::StateUnavailable("the tailscale lookup ended without a result".to_owned())
    })??;
    run_status_at(&program).await
}

/// Admits one lookup thread at a time.
///
/// A lookup stuck in a filesystem call cannot be cancelled, so at most one such
/// thread exists. Later callers wait for the permit, cancellably and without a
/// thread of their own, and proceed as soon as the earlier lookup ends.
static LOOKUP_GATE: Semaphore = Semaphore::const_new(1);

/// Resolves the `tailscale` CLI for this host's environment.
fn resolve_host_program() -> Result<PathBuf, TailscaleError> {
    let fallback: &[&str] = if cfg!(target_os = "macos") {
        DARWIN_FALLBACK_DIRECTORIES
    } else {
        &[]
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_program(
        std::env::var_os("PATH").as_deref(),
        fallback,
        home.as_deref(),
    )
}

/// Resolves the `tailscale` CLI against a raw `PATH` value, then against the
/// trusted directories of `fallback` (`~/` entries expand against `home`).
fn resolve_program(
    path: Option<&OsStr>,
    fallback: &[&str],
    home: Option<&Path>,
) -> Result<PathBuf, TailscaleError> {
    let program = OsStr::new(TAILSCALE_PROGRAM);
    if let Ok(found) = resolve_executable_in_path_value(program, path) {
        return Ok(found);
    }
    let search = fallback_search_path(fallback, home)
        .map_err(|_unresolved| TailscaleError::CliMissing)?
        .path;
    resolve_executable(program, &search).map_err(|_unresolved| TailscaleError::CliMissing)
}

async fn run_status_at(program: &Path) -> Result<TailscaleStatus, TailscaleError> {
    let output = tokio::process::Command::new(program)
        .args(TAILSCALE_STATUS_ARGS)
        // The daemon may cancel this future while shutting down. Killing the
        // child prevents an unresponsive CLI from extending that shutdown.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| command_error(program, &error))?;
    status_from_output(&output)
}

fn command_error(program: &Path, error: &std::io::Error) -> TailscaleError {
    match error.kind() {
        std::io::ErrorKind::NotFound => TailscaleError::CliMissing,
        _ => TailscaleError::StateUnavailable(format!(
            "failed to run `{} status`: {error}",
            program.display()
        )),
    }
}

fn status_from_output(output: &std::process::Output) -> Result<TailscaleStatus, TailscaleError> {
    if !output.status.success() {
        return Err(TailscaleError::StateUnavailable(non_zero_detail(output)));
    }
    parse_status(&String::from_utf8_lossy(&output.stdout))
}

/// Build a bounded, human-readable detail for a non-zero `tailscale` exit.
///
/// Prefers stderr, falling back to stdout; trims whitespace and clamps the
/// length so a misbehaving CLI cannot flood the error path.
fn non_zero_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        stderr.to_owned()
    };
    if detail.is_empty() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        format!("exit {code}")
    } else {
        clamp(&detail, MAX_ERROR_DETAIL_BYTES)
    }
}

/// Clamp `text` to at most `max` bytes on a char boundary, appending an
/// ellipsis marker when truncated.
fn clamp(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
