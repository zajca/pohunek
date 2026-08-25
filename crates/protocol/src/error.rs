//! Typed protocol errors.
//!
//! Error envelopes carry a `class` (broad category), a machine-readable `code`,
//! a human `msg`, and an optional `recover` hint. The classes mirror the error
//! taxonomy in `docs/architecture.md` "Error Handling": configuration, daemon,
//! transport, runtime, discovery. Keeping them typed lets `--json` consumers and
//! operator agents branch on the failure instead of string-matching messages.

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersionRange;

/// Broad error category for a control-protocol error.
///
/// Serialized in lowercase snake form on the wire (e.g. `"daemon"`). Mirrors the
/// distinctions in `docs/architecture.md` "Error Handling".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ErrorClass.ts"))]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Missing or invalid required configuration.
    Configuration,
    /// Daemon-level failures: unavailable, version mismatch, framing.
    Daemon,
    /// Transport failures: `NetBird` unreachable, connection lost.
    Transport,
    /// Runtime failures: agent binary missing, PTY allocation, process exit,
    /// worktree conflict.
    Runtime,
    /// Discovery failures: `NetBird` CLI missing, local state unavailable.
    Discovery,
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorClass::Configuration => "configuration",
            ErrorClass::Daemon => "daemon",
            ErrorClass::Transport => "transport",
            ErrorClass::Runtime => "runtime",
            ErrorClass::Discovery => "discovery",
        };
        f.write_str(s)
    }
}

/// A typed control-protocol error.
///
/// This is both the body carried inside an error [`Response`](crate::Response)
/// and a Rust `Error` (via `thiserror`) so daemon code can return it directly
/// and the API layer can serialize it into the `err` field of a response.
///
/// Machine codes are stable strings; see the constructors for the canonical
/// ones used in Phase 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "ProtocolError.ts"))]
#[serde(deny_unknown_fields)]
#[error("{class}/{code}: {msg}")]
pub struct ProtocolError {
    /// Broad category, for coarse branching.
    pub class: ErrorClass,
    /// Stable machine-readable code, for precise branching.
    pub code: String,
    /// Human-readable message. Never contains secrets or terminal content.
    pub msg: String,
    /// Optional suggested recovery action (e.g. "install claude").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub recover: Option<String>,
}

impl ProtocolError {
    /// Construct an error with all fields explicit.
    #[must_use]
    pub fn new(
        class: ErrorClass,
        code: impl Into<String>,
        msg: impl Into<String>,
        recover: Option<String>,
    ) -> Self {
        Self {
            class,
            code: code.into(),
            msg: msg.into(),
            recover,
        }
    }

    /// The canonical `daemon/version_mismatch` error.
    ///
    /// Carries both versions in the message so the operator sees exactly what to
    /// upgrade. Code is stable: `version_mismatch`.
    #[must_use]
    pub fn version_mismatch(client: ProtocolVersionRange, daemon: ProtocolVersionRange) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "version_mismatch",
            format!(
                "client protocol range {}..={} does not overlap daemon protocol range {}..={}",
                client.minimum(), client.maximum(), daemon.minimum(), daemon.maximum()
            ),
            Some("upgrade the older side so the client and daemon support an overlapping protocol version".to_owned()),
        )
    }

    /// The canonical `daemon/agent_kind_unsupported` error.
    #[must_use]
    pub fn agent_kind_unsupported(agent: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "agent_kind_unsupported",
            format!("agent kind `{agent}` is presentation-only and cannot be mutated or persisted"),
            Some(
                "upgrade the daemon to a version that explicitly supports this agent kind"
                    .to_owned(),
            ),
        )
    }

    /// Creates one payload-free M1 observation error.
    #[must_use]
    pub fn observation(code: &'static str, msg: &'static str) -> Self {
        Self::new(ErrorClass::Runtime, code, msg, None)
    }

    /// The canonical `runtime/agent_fork_unsupported` error.
    #[must_use]
    pub fn agent_fork_unsupported() -> Self {
        Self::observation(
            "agent_fork_unsupported",
            "the selected agent does not support fork",
        )
    }

    /// The canonical rejection for input outside an agent's safe-text contract.
    #[must_use]
    pub fn session_input_rejected() -> Self {
        Self::observation(
            "session_input_rejected",
            "the input does not satisfy the selected agent's safe-text contract",
        )
    }

    /// The canonical rejection for input while an agent awaits owner action.
    #[must_use]
    pub fn session_input_blocked() -> Self {
        Self::observation(
            "session_input_blocked",
            "programmatic input is disabled while the agent awaits owner action",
        )
    }

    /// The canonical rejection for confirmed delivery into a blocked agent.
    #[must_use]
    pub fn session_agent_blocked() -> Self {
        Self::observation(
            "session_agent_blocked",
            "the agent awaits owner action; delivery cannot be confirmed",
        )
    }

    /// The canonical rejection for a bounded input wait whose deadline elapsed.
    #[must_use]
    pub fn session_input_timeout() -> Self {
        Self::observation(
            "session_input_timeout",
            "the bounded input wait timed out before the agent reached a requested state",
        )
    }

    /// The canonical rejection for a missing or incompatible managed runtime.
    #[must_use]
    pub fn agent_runtime_unsupported() -> Self {
        Self::observation(
            "agent_runtime_unsupported",
            "the selected agent runtime is unavailable or incompatible with this daemon",
        )
    }

    /// The canonical `runtime/session_terminal_unavailable` error.
    #[must_use]
    pub fn session_terminal_unavailable() -> Self {
        Self::observation(
            "session_terminal_unavailable",
            "the session terminal is unavailable",
        )
    }

    /// The canonical `runtime/session_has_no_managed_terminal` error.
    #[must_use]
    pub fn session_has_no_managed_terminal() -> Self {
        Self::observation(
            "session_has_no_managed_terminal",
            "the session has no pohunek-managed terminal",
        )
    }

    /// The canonical `runtime/session_runtime_changed` error.
    #[must_use]
    pub fn session_runtime_changed() -> Self {
        Self::observation(
            "session_runtime_changed",
            "the session runtime changed; restart observation with the current runtime identity",
        )
    }

    /// The canonical `runtime/session_output_limit_exceeded` error.
    #[must_use]
    pub fn session_output_limit_exceeded() -> Self {
        Self::observation(
            "session_output_limit_exceeded",
            "the requested output limit exceeds the configured maximum",
        )
    }

    /// The canonical `runtime/session_wait_limit_exceeded` error.
    #[must_use]
    pub fn session_wait_limit_exceeded() -> Self {
        Self::observation(
            "session_wait_limit_exceeded",
            "the requested wait exceeds the configured maximum",
        )
    }

    /// The canonical `runtime/session_waiter_limit_reached` error.
    #[must_use]
    pub fn session_waiter_limit_reached() -> Self {
        Self::observation(
            "session_waiter_limit_reached",
            "the session waiter limit is currently reached",
        )
    }

    /// The canonical `runtime/worker_feature_unavailable` error.
    #[must_use]
    pub fn worker_feature_unavailable() -> Self {
        Self::observation(
            "worker_feature_unavailable",
            "the live worker does not support this control-plane feature",
        )
    }

    /// The canonical origin-session mutation denial.
    #[must_use]
    pub fn plugin_self_target_denied() -> Self {
        Self::observation(
            "plugin_self_target_denied",
            "a process running inside a session cannot mutate that origin session",
        )
    }

    /// The canonical `daemon/method_not_found` error for an unknown method.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "method_not_found",
            format!("unknown control method: {method}"),
            None,
        )
    }

    /// The canonical `daemon/bad_request` error for a malformed request body.
    #[must_use]
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorClass::Daemon, "bad_request", msg, None)
    }

    /// The canonical `runtime/notification_kind_disabled` error.
    ///
    /// Raised when daemon notification policy disables a create request's
    /// notification kind for the producer provider. Code is stable:
    /// `notification_kind_disabled`.
    #[must_use]
    pub fn notification_kind_disabled(provider: &str, kind: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "notification_kind_disabled",
            format!("notification kind `{kind}` is disabled for provider `{provider}`"),
            Some("enable the notification kind in policy before creating this record".to_owned()),
        )
    }

    /// The canonical `runtime/agent_binary_missing` error.
    ///
    /// Names the missing binary so the operator (or an operator agent) sees
    /// exactly what to install, and carries a `recover` hint pointing at the fix.
    /// Raised both when resolving an agent binary on `PATH` before launch and when
    /// a PTY spawn fails because the program is absent (ENOENT). Code is stable:
    /// `agent_binary_missing`.
    #[must_use]
    pub fn agent_binary_missing(binary: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "agent_binary_missing",
            format!("agent binary not found on PATH: {binary}"),
            Some(format!(
                "install the {binary} CLI and ensure it is on PATH; run `pohunek doctor` to verify"
            )),
        )
    }

    /// The canonical `discovery/netbird_cli_missing` error.
    ///
    /// Raised when the local `netbird` CLI cannot be found on `PATH`, so remote
    /// host discovery and remote sessions over `NetBird` are unavailable. Carries a
    /// `recover` hint pointing at installing `NetBird` and verifying with the
    /// doctor. Code is stable: `netbird_cli_missing`.
    #[must_use]
    pub fn netbird_cli_missing() -> Self {
        Self::new(
            ErrorClass::Discovery,
            "netbird_cli_missing",
            "the `netbird` CLI was not found on PATH".to_owned(),
            Some(
                "install the NetBird CLI and ensure it is on PATH; run `pohunek doctor` to verify"
                    .to_owned(),
            ),
        )
    }

    /// The canonical `discovery/netbird_state_unavailable` error.
    ///
    /// Raised when the `netbird` CLI is present but its local state could not be
    /// read (the `NetBird` daemon is down, or this host is not logged in). Carries a
    /// short `detail` in the message and a `recover` hint. Code is stable:
    /// `netbird_state_unavailable`.
    #[must_use]
    pub fn netbird_state_unavailable(detail: impl Into<String>) -> Self {
        Self::new(
            ErrorClass::Discovery,
            "netbird_state_unavailable",
            format!("NetBird local state is unavailable: {}", detail.into()),
            Some(
                "ensure the NetBird daemon is running and this host is logged in; run `pohunek doctor` to verify"
                    .to_owned(),
            ),
        )
    }

    /// The canonical `discovery/host_unknown` error.
    ///
    /// Raised when the requested host name did not match any `NetBird` peer (by
    /// fqdn, short hostname, or `NetBird` IP). Names the host so the operator sees
    /// exactly what failed to resolve. Code is stable: `host_unknown`.
    #[must_use]
    pub fn host_unknown(host: &str) -> Self {
        Self::new(
            ErrorClass::Discovery,
            "host_unknown",
            format!("host '{host}' was not found among NetBird peers"),
            Some(
                "run `pohunek host list` to see reachable peers and check the host name".to_owned(),
            ),
        )
    }

    /// The canonical `transport/host_unreachable` error.
    ///
    /// Raised when a `NetBird` TCP connection to the host's daemon control port
    /// could not be opened (the peer is offline or the port is closed). Names the
    /// host and carries a `recover` hint. Code is stable: `host_unreachable`.
    #[must_use]
    pub fn host_unreachable(host: &str) -> Self {
        Self::new(
            ErrorClass::Transport,
            "host_unreachable",
            format!("could not open a NetBird connection to host '{host}'"),
            Some("check that the host is online and its pohunek daemon is running".to_owned()),
        )
    }

    /// The canonical `daemon/remote_daemon_unavailable` error.
    ///
    /// Raised when a `NetBird` TCP connection to the host opened, but no compatible
    /// pohunek daemon answered on the control port. Names the host so the
    /// operator can investigate that specific peer. Code is stable:
    /// `remote_daemon_unavailable`.
    #[must_use]
    pub fn remote_daemon_unavailable(host: &str) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "remote_daemon_unavailable",
            format!("connected to host '{host}' but no compatible pohunek daemon answered"),
            Some("ensure a matching pohunek daemon is running on the host".to_owned()),
        )
    }

    /// The canonical `runtime/no_capable_agent` error for assistant launch.
    ///
    /// Raised when no available runtime can satisfy the assistant's requirement
    /// for a capable coding agent. Code is stable: `no_capable_agent`.
    #[must_use]
    pub fn no_capable_agent() -> Self {
        Self::new(
            ErrorClass::Runtime,
            "no_capable_agent",
            "no capable assistant agent runtime is available",
            Some(
                "install or configure a codex/claude runtime, or pass --agent with a capable host profile"
                    .to_owned(),
            ),
        )
    }

    /// The canonical `runtime/bundle_unavailable` error for missing assistant
    /// knowledge.
    ///
    /// Raised before session launch when the materialized bundle path is absent
    /// or otherwise unavailable. Code is stable: `bundle_unavailable`.
    #[must_use]
    pub fn bundle_unavailable(path: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "bundle_unavailable",
            format!("assistant knowledge bundle is unavailable at {path}"),
            Some("rebuild or reinstall pohunek so the assistant knowledge bundle can be materialized".to_owned()),
        )
    }

    /// The canonical `runtime/assistant_bundle_mismatch` error for remote
    /// materialization that returned a bundle from a different binary build.
    ///
    /// Code is stable: `assistant_bundle_mismatch`.
    #[must_use]
    pub fn assistant_bundle_mismatch(
        expected_version: &str,
        expected_hash: &str,
        actual_version: &str,
        actual_hash: &str,
    ) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "assistant_bundle_mismatch",
            format!(
                "remote assistant bundle {actual_version}/{actual_hash} does not match local binary {expected_version}/{expected_hash}"
            ),
            Some("upgrade the older pohunek side so the CLI and daemon use the same assistant knowledge bundle".to_owned()),
        )
    }

    /// The canonical `runtime/materialization_failed` error for assistant
    /// bundle extraction or snapshot persistence failures.
    ///
    /// Code is stable: `materialization_failed`.
    #[must_use]
    pub fn materialization_failed(path: &str, detail: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "materialization_failed",
            format!("failed to materialize assistant knowledge at {path}: {detail}"),
            Some(
                "check filesystem permissions and available space, then retry the assistant launch"
                    .to_owned(),
            ),
        )
    }

    /// The canonical `runtime/agent_cannot_read_bundle` error.
    ///
    /// Raised when preflight proves the selected agent cannot read the bundle or
    /// snapshot path. Code is stable: `agent_cannot_read_bundle`.
    #[must_use]
    pub fn agent_cannot_read_bundle(path: &str, constraint: &str) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "agent_cannot_read_bundle",
            format!("selected agent cannot read assistant knowledge at {path}: {constraint}"),
            Some("materialize the bundle inside the agent-readable root, relax the profile filesystem constraint, or choose another profile".to_owned()),
        )
    }

    /// The canonical `daemon/assistant_method_unsupported` error.
    ///
    /// CLI code uses this when an older daemon reports `method_not_found` for an
    /// assistant-specific method. Code is stable:
    /// `assistant_method_unsupported`.
    #[must_use]
    pub fn assistant_method_unsupported(method: &str) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "assistant_method_unsupported",
            format!("daemon does not support assistant method: {method}"),
            Some(
                "upgrade the daemon to a pohunek version with universal assistant support"
                    .to_owned(),
            ),
        )
    }
}
