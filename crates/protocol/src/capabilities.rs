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

use crate::{version::ProtocolVersion, RuntimeRef};

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
    /// Runtime identity this entry launches, when reported by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent_base: Option<RuntimeRef>,
    /// Whether the agent's backing binary is available on the host.
    pub available: bool,
    /// Resolved path to the agent binary, when one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub path: Option<String>,
    /// Normalized runtime version, when a provider-specific probe recognized it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub version: Option<String>,
    /// Whether the detected version is supported by this daemon.
    ///
    /// `None` means the runtime has no version policy. A present runtime with a
    /// policy always reports `Some(_)`: `Some(false)` for an unparseable or
    /// unaccepted version, so clients treat `Some(false)` as not launchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub supported: Option<bool>,
    /// Opaque, non-secret identifier of the config home a launch of this entry
    /// gives its agent, for grouping entries that share an account.
    ///
    /// Two entries carry the same value exactly when their launches resolve to
    /// the same directory. The value is a keyed digest under a secret of the
    /// reporting host: it is stable on that host, differs between hosts, and
    /// reveals neither the path nor any environment value, so it is not
    /// comparable across hosts. Absent when the runtime declares no config home,
    /// the home cannot be resolved, or the host cannot key the digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub config_home_id: Option<String>,
}

/// Optional parameters for `host.inspect`.
///
/// A daemon that predates the parameters ignores them and answers the full
/// inventory, so a client that sets one must still accept a complete answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostInspectParams.ts"))]
pub struct HostInspectParams {
    /// Answer only the feature flags, `daemon_version` and `git_available`:
    /// `supported_agents` and `runtimes` are empty and no agent runtime is
    /// resolved or probed. For a client that only gates a request on a
    /// capability flag. Absent or `false` keeps the full inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub skip_runtime_probes: Option<bool>,
}

/// Live capability snapshot returned by `host.inspect`.
///
/// Built from the host's running daemon on each request: the presence of every
/// agent program on its `PATH` is resolved afresh, and the version probes
/// behind `runtimes` are shared with earlier requests for as long as the probed
/// file is unchanged and the daemon's probe cache lifetime has not passed, so
/// a result may be up to that lifetime old.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the wire snapshot exposes independent host feature flags"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "HostCapabilities.ts"))]
pub struct HostCapabilities {
    /// Version string of the daemon answering on the host.
    pub daemon_version: String,
    /// Protocol version the host's daemon negotiated for the connection that asked.
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
    /// Whether managed sessions support point-in-time terminal snapshots.
    pub terminal_read_supported: bool,
    /// Whether managed sessions support bounded retained-output reads.
    pub output_read_supported: bool,
    /// Whether managed sessions support bounded `session.wait` long polls.
    pub session_wait_supported: bool,
    /// Whether the daemon serves `session.wait_turn` turn-event waits.
    ///
    /// A daemon that predates the method omits the field, which decodes as
    /// `false`; `false` is likewise omitted on the wire, so a protocol 3
    /// snapshot round-trips unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_wait_turn_supported: bool,
    /// Whether `session.new` starts an in-place session in the requested `cwd`:
    /// an absolute path of an existing directory, also below a checkout root
    /// and with `project` or `repo`, where a missing directory is refused.
    ///
    /// A daemon that predates the behavior omits the field, which decodes as
    /// `false`; it starts such a session in the checkout. `false` is likewise
    /// omitted on the wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_new_cwd_supported: bool,
    /// Whether `session.new` honors `branch_source` and `on_diverged`.
    ///
    /// A daemon that predates them ignores both and checks out the local branch
    /// as it is, so a client checks this flag before it sends either. Omitted
    /// when `false`, which is how an older daemon decodes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_new_branch_source_supported: bool,
    /// Whether `session.new` honors `input_ack_timeout_ms` and reports
    /// `prompt_delivered`.
    ///
    /// A daemon that predates the field omits it, which decodes as `false`; it
    /// would drop `input_ack_timeout_ms` silently. `false` is likewise omitted
    /// on the wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_new_input_ack_supported: bool,
    /// Whether the daemon serves `session.idle_stop.get` and
    /// `session.idle_stop.set` and stops sessions opted in to idle stop.
    ///
    /// Omitted when `false`, like [`Self::session_wait_turn_supported`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_idle_stop_supported: bool,
    /// Whether `package.install` and `package.link` accept `accept_commands`
    /// and install packages that declare commands.
    ///
    /// A daemon that predates the field omits it, which decodes as `false`; its
    /// params are strict and would reject `accept_commands`, so a client sends
    /// the field only when this is `true`. `false` is likewise omitted on the
    /// wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub package_install_commands_supported: bool,
}
