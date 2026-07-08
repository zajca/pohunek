//! Host capability snapshot payloads.
//!
//! These types define the JSON shape carried inside the `ok` value of a
//! `host.inspect` response (see `crate::method::HOST_INSPECT`). A capability
//! snapshot is a live view of what a host can do: which protocol version its
//! daemon speaks, which agent kinds it supports, which agent runtimes are
//! actually installed there, and whether git-backed worktree sessions are
//! available. The CLI uses it to decide where a session can run before it asks
//! the host to start one.
//!
//! Like every other protocol payload, these are additive: unknown fields are
//! ignored and absent optional fields default, so a newer peer and an older peer
//! interoperate on the common subset.

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// One agent runtime's availability on a host.
///
/// Reports whether a given agent profile's backing binary is present on the
/// host, and, when known, the resolved path to it. The shell runtime is always
/// available and typically carries no path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "AgentRuntime.ts"))]
pub struct AgentRuntime {
    /// Agent profile or built-in base name this runtime entry describes.
    pub agent: String,
    /// Whether the agent's backing binary is available on the host.
    pub available: bool,
    /// Resolved path to the agent binary, when one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub path: Option<String>,
}

/// Live capability snapshot returned by `host.inspect`.
///
/// Built fresh on each request from the host's running daemon and a probe of its
/// `PATH`; it is never cached, so it always reflects the host as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostCapabilities.ts"))]
pub struct HostCapabilities {
    /// Version string of the daemon answering on the host.
    pub daemon_version: String,
    /// Protocol version the host's daemon speaks.
    pub protocol_version: ProtocolVersion,
    /// Agent profile and built-in base names the host's daemon knows how to launch.
    pub supported_agents: Vec<String>,
    /// Per-agent runtime availability on the host.
    pub runtimes: Vec<AgentRuntime>,
    /// Whether `git` is present on the host. When true, repo/worktree-bound
    /// sessions are supported there.
    pub git_available: bool,
    /// Whether worktree-per-session is supported on the host (currently implied
    /// by [`git_available`](Self::git_available)).
    pub worktree_supported: bool,
}
