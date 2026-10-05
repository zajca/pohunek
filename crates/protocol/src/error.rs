//! Typed protocol errors.
//!
//! Error envelopes carry a `class` (broad category), a machine-readable `code`,
//! a human `msg`, and an optional `recover` hint. The classes mirror the error
//! taxonomy in `docs/architecture.md` "Error Handling": configuration, daemon,
//! transport, runtime, discovery. Keeping them typed lets `--json` consumers and
//! operator agents branch on the failure instead of string-matching messages.
//!
//! The delegated task layer (`docs/design/delegated-task-runs-rfc.md` section
//! 13.1) has its own constructor family. Those errors take no arguments: their
//! message and recovery hint are fixed text, so they can never echo a prompt,
//! answer, path, task id or secret back to the caller.

use serde::{Deserialize, Serialize};

use crate::{
    runtime_id::RuntimeId,
    version::{ProtocolVersion, ProtocolVersionRange},
};

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

    /// The canonical `daemon/version_adapter_failed` error.
    ///
    /// The daemon could not express a payload in the older protocol version the
    /// connection negotiated. The message names only the version, never the
    /// payload. Code is stable: `version_adapter_failed`.
    #[must_use]
    pub fn version_adapter_failed(version: ProtocolVersion) -> Self {
        Self::new(
            ErrorClass::Daemon,
            "version_adapter_failed",
            format!("the daemon cannot express this payload in protocol version {version}"),
            Some(
                "upgrade the client to a build that speaks the daemon's current protocol version"
                    .to_owned(),
            ),
        )
    }

    /// The canonical `daemon/daemon_protocol_too_old` error.
    ///
    /// A client refuses a method before sending it because the daemon it is
    /// connected to negotiated a protocol version that never defined the
    /// method. `host` is the route of a remote daemon, `None` for the local
    /// daemon. Code is stable: `daemon_protocol_too_old`.
    #[must_use]
    pub fn daemon_protocol_too_old(
        host: Option<&str>,
        method: &str,
        daemon: ProtocolVersion,
        required: ProtocolVersion,
    ) -> Self {
        let target = daemon_target(host);
        Self::new(
            ErrorClass::Daemon,
            "daemon_protocol_too_old",
            format!("{target} runs protocol {daemon}, but `{method}` needs protocol {required}"),
            Some(format!(
                "upgrade pohunek on {target} to a release that speaks protocol {required}"
            )),
        )
    }

    /// The canonical `daemon/version_translation_failed` error.
    ///
    /// A client could not express a request in, or read a response of, the
    /// older protocol version its daemon negotiated. The message names only the
    /// target and the version, never the payload. Code is stable:
    /// `version_translation_failed`.
    #[must_use]
    pub fn version_translation_failed(host: Option<&str>, daemon: ProtocolVersion) -> Self {
        let target = daemon_target(host);
        Self::new(
            ErrorClass::Daemon,
            "version_translation_failed",
            format!("a payload cannot be translated for {target}, which runs protocol {daemon}"),
            Some(format!(
                "upgrade pohunek on {target} so both sides speak the same protocol version"
            )),
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

    /// The canonical `runtime/runtime_not_installed` error.
    ///
    /// The identity is a validated [`RuntimeId`], so echoing it is safe.
    #[must_use]
    pub fn runtime_not_installed(runtime: &RuntimeId) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "runtime_not_installed",
            format!("runtime `{runtime}` is not installed or not enabled on this host"),
            Some("install and enable the runtime, or choose an installed runtime".to_owned()),
        )
    }

    /// The canonical `runtime/runtime_incompatible` error.
    ///
    /// The runtime is installed, but the package root it launches from is
    /// missing, was modified, or no longer matches the identity recorded for
    /// the session. The state is read-only: nothing else stands in for it. The
    /// message carries no path and no reason text; the daemon log records the
    /// typed cause. The identity is a validated [`RuntimeId`], so echoing it is
    /// safe.
    #[must_use]
    pub fn runtime_incompatible(runtime: &RuntimeId) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "runtime_incompatible",
            format!("runtime `{runtime}` is installed but its package cannot be used"),
            Some("reinstall the runtime package and retry".to_owned()),
        )
    }

    /// The canonical `runtime/runtime_package_changed` error.
    ///
    /// The package version a launch resolved is no longer the one selected for
    /// its runtime, because a package change committed while the launch was
    /// starting. Nothing was launched; a new request resolves the version that
    /// is selected now. The identity is a validated [`RuntimeId`], so echoing it
    /// is safe.
    #[must_use]
    pub fn runtime_package_changed(runtime: &RuntimeId) -> Self {
        Self::new(
            ErrorClass::Runtime,
            "runtime_package_changed",
            format!(
                "runtime `{runtime}` changed its selected package while the session was starting"
            ),
            Some("retry the request".to_owned()),
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
        Self::new(
            ErrorClass::Runtime,
            "session_input_timeout",
            "the overall input deadline elapsed before delivery and requested activity were confirmed",
            Some(
                "delivery outcome may be unknown; inspect the current session before deciding whether to resend, and do not retry blindly"
                    .to_owned(),
            ),
        )
    }

    /// The canonical rejection for delayed framing that cannot safely confirm input.
    #[must_use]
    pub fn session_input_wait_unsupported() -> Self {
        Self::new(
            ErrorClass::Runtime,
            "session_input_wait_unsupported",
            "bounded input confirmation is unavailable for agents that require delayed submit framing",
            Some(
                "use fire-and-forget input only when safe, or select a zero-delay agent profile"
                    .to_owned(),
            ),
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

    /// The canonical error for detection metadata that cannot fit the response budget.
    #[must_use]
    pub fn session_detection_response_too_large() -> Self {
        Self::observation(
            "session_detection_response_too_large",
            "detection metadata exceeds the public response budget",
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

/// Delegated task layer errors (task RFC section 13.1).
///
/// Every constructor is argument-free and builds its message and hint from
/// fixed text. State conflicts, limits and policy refusals use
/// [`ErrorClass::Runtime`], matching the existing worktree, waiter-limit and
/// notification-policy analogues; the only exception is
/// [`ProtocolError::task_check_unconfined`], see its documentation.
impl ProtocolError {
    /// Builds one payload-free task-layer error from fixed text.
    fn fixed(
        class: ErrorClass,
        code: &'static str,
        msg: &'static str,
        recover: Option<&'static str>,
    ) -> Self {
        Self::new(class, code, msg, recover.map(str::to_owned))
    }

    /// The canonical `runtime/task_turn_open` error.
    ///
    /// Raised by `task.continue` and `task.answer` while the task's latest turn
    /// is still open, queued or already resumed (task RFC invariant 2, section
    /// 8.2). Code is stable: `task_turn_open`.
    #[must_use]
    pub fn task_turn_open() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_turn_open",
            "the task's latest turn is still open or already resumed",
            Some("wait for the turn to settle with task.wait, then retry"),
        )
    }

    /// The canonical `runtime/task_attention_open` error.
    ///
    /// Raised by `task.continue` when the latest turn settled `attention` and
    /// was not answered (task RFC invariant 2). Code is stable:
    /// `task_attention_open`.
    #[must_use]
    pub fn task_attention_open() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_attention_open",
            "the task's latest turn awaits an answer to its attention",
            Some("answer the pending attention with task.answer, or stop the task"),
        )
    }

    /// The canonical `runtime/task_agent_busy` error.
    ///
    /// Raised by `task.continue` while the agent is still visibly working on an
    /// earlier prompt (task RFC invariant 2, section 8.4). Code is stable:
    /// `task_agent_busy`.
    #[must_use]
    pub fn task_agent_busy() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_agent_busy",
            "the agent is still working on an earlier prompt",
            Some("wait with task.wait or extend the turn with task.extend, then retry; or stop the task"),
        )
    }

    /// The canonical `runtime/task_worktree_busy` error.
    ///
    /// Raised by `task.start` (including `worktree_of`), `task.continue`,
    /// `task.answer` and `task.extend` when another task occupies the worktree,
    /// when a live non-task session is bound to it, or when a handoff cannot
    /// prove the previous occupant's process scope empty (task RFC invariant
    /// 11, section 8.7). Code is stable: `task_worktree_busy`.
    #[must_use]
    pub fn task_worktree_busy() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_worktree_busy",
            "the worktree is occupied by another task or live session",
            Some("inspect the worktree users with task.inspect and retry after the occupant settles or is stopped"),
        )
    }

    /// The canonical `runtime/worktree_busy` error.
    ///
    /// Raised by `session.input`, attach with terminal control,
    /// `session.resume`, `session.fork` with `cwd_mode: "same"` and in-place
    /// `session.new` while a task occupies the session's worktree; only
    /// observation is admitted (task RFC invariant 11 write fence). Code is
    /// stable: `worktree_busy`.
    #[must_use]
    pub fn worktree_busy() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "worktree_busy",
            "a task occupies this worktree; only observation is admitted",
            Some("observe the session read-only, or retry after the occupying task settles or is stopped"),
        )
    }

    /// The canonical `runtime/task_worktree_unavailable` error.
    ///
    /// Raised by `task.start` and `task.continue` when the shared worktree no
    /// longer exists (task RFC section 8.7). Code is stable:
    /// `task_worktree_unavailable`.
    #[must_use]
    pub fn task_worktree_unavailable() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_worktree_unavailable",
            "the shared worktree no longer exists",
            Some("start a new task with its own worktree"),
        )
    }

    /// The canonical `runtime/task_worktree_mode_conflict` error.
    ///
    /// Raised by `task.start` when `worktree_of` is combined with `in_place` or
    /// `branch`. Semantic parameter validation uses the runtime class, as
    /// `session_input_invalid_wait` does. Code is stable:
    /// `task_worktree_mode_conflict`.
    #[must_use]
    pub fn task_worktree_mode_conflict() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_worktree_mode_conflict",
            "worktree_of cannot be combined with in_place or branch",
            Some("send worktree_of alone, or in_place or branch without worktree_of"),
        )
    }

    /// The canonical `runtime/task_session_ended` error.
    ///
    /// Raised by `session.resume`, `session.fork` and explicit session recovery
    /// when the session belongs to an `ended` task (task RFC section 6.1).
    /// Code is stable: `task_session_ended`.
    #[must_use]
    pub fn task_session_ended() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_session_ended",
            "the session belongs to an ended task and cannot be revived outside the task layer",
            Some("continue the work with task.start and worktree_of naming the ended task"),
        )
    }

    /// The canonical `runtime/task_session_unavailable` error.
    ///
    /// Raised by `task.continue` and `task.answer` when the task is `ended` or
    /// its agent runtime is not live. Code is stable:
    /// `task_session_unavailable`.
    #[must_use]
    pub fn task_session_unavailable() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_session_unavailable",
            "the task has ended or its agent runtime is not live",
            Some("inspect the task with task.inspect; continue ended work with task.start and worktree_of"),
        )
    }

    /// The canonical `runtime/task_turn_queued` error.
    ///
    /// Raised by `task.extend` on a turn queued behind a re-open window, which
    /// has no deadline until it is delivered (task RFC section 8.2). Code is
    /// stable: `task_turn_queued`.
    #[must_use]
    pub fn task_turn_queued() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_turn_queued",
            "the turn is queued and has no deadline until it is delivered",
            Some("wait for delivery with task.wait before extending the turn"),
        )
    }

    /// The canonical `runtime/task_worktree_via_investigate` error.
    ///
    /// Raised by an executor-mode `task.start` whose `worktree_of` names an
    /// investigate-mode task, so read access never becomes write access (task
    /// RFC section 8.7). Code is stable: `task_worktree_via_investigate`.
    #[must_use]
    pub fn task_worktree_via_investigate() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_worktree_via_investigate",
            "an executor task cannot join a worktree through an investigate-mode task",
            Some("name a task that is not in investigate mode in worktree_of, or start in investigate mode"),
        )
    }

    /// The canonical `runtime/task_turn_ceiling_reached` error.
    ///
    /// Raised by `task.extend` once the turn's total open time reached
    /// `tasks.turn_open_ceiling_ms` (task RFC section 6.3). Code is stable:
    /// `task_turn_ceiling_reached`.
    #[must_use]
    pub fn task_turn_ceiling_reached() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_turn_ceiling_reached",
            "the turn reached its total open-time ceiling",
            Some("answer or stop the turn, or continue with a new turn once the agent is ready"),
        )
    }

    /// The canonical `runtime/task_answer_unsupported` error.
    ///
    /// Raised by `task.answer` on a degraded attention whose detection manifest
    /// declares no input sequence for the requested approve or deny answer
    /// (task RFC section 8.5). Code is stable: `task_answer_unsupported`.
    #[must_use]
    pub fn task_answer_unsupported() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_answer_unsupported",
            "this attention cannot be answered through the task layer",
            Some("resolve the attention by terminal takeover, or stop the task"),
        )
    }

    /// The canonical `runtime/task_answer_unverifiable` error.
    ///
    /// Raised by `task.answer` for a keystroke answer sent without
    /// `allow_unverified_delivery`, because the daemon cannot prove the provider
    /// still waits on the named request (task RFC section 8.5). Code is stable:
    /// `task_answer_unverifiable`.
    #[must_use]
    pub fn task_answer_unverifiable() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_answer_unverifiable",
            "a keystroke answer cannot be verified against the provider's pending request",
            Some("set allow_unverified_delivery to accept an unverified answer, or resolve the attention in the terminal"),
        )
    }

    /// The canonical `runtime/task_payload_mismatch` error.
    ///
    /// Raised by a retried `task.start`, `task.continue` or `task.answer` whose
    /// resubmitted payload does not match the stored keyed fingerprint or
    /// ticket fingerprint; nothing is dispatched (task RFC section 8.8). Code
    /// is stable: `task_payload_mismatch`.
    #[must_use]
    pub fn task_payload_mismatch() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_payload_mismatch",
            "the resubmitted payload does not match the stored request fingerprint; nothing was dispatched",
            Some("resend the exact original payload with the same request key, or inspect the task first"),
        )
    }

    /// The canonical `runtime/task_stop_precondition_failed` error.
    ///
    /// Raised by `task.stop` when `if_latest_turn` or `require_idle` does not
    /// hold; the stop changes nothing (task RFC section 13.1). Code is stable:
    /// `task_stop_precondition_failed`.
    #[must_use]
    pub fn task_stop_precondition_failed() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_stop_precondition_failed",
            "the stop preconditions do not hold; nothing was changed",
            Some("inspect the task and retry the stop with current preconditions"),
        )
    }

    /// The canonical `runtime/worktree_users_changed` error.
    ///
    /// Raised by `session.remove` when `expected_worktree_users` differs from
    /// the current set of active user tasks (task RFC section 8.7). Code is
    /// stable: `worktree_users_changed`.
    #[must_use]
    pub fn worktree_users_changed() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "worktree_users_changed",
            "the expected worktree users differ from the current set",
            Some(
                "re-read the worktree users with task.inspect and retry with the exact current set",
            ),
        )
    }

    /// The canonical `runtime/task_attention_stale` error.
    ///
    /// Raised by `task.answer` when the named attention is not the current
    /// pending one at that settlement revision, or the provider already
    /// resolved it; nothing is written (task RFC section 8.5). Code is stable:
    /// `task_attention_stale`.
    #[must_use]
    pub fn task_attention_stale() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_attention_stale",
            "the named attention is not the current pending one; nothing was written",
            Some("inspect the task for the current attention and revision before answering again"),
        )
    }

    /// The canonical `runtime/task_result_unknown` error.
    ///
    /// Raised by `task.review` and `task.result` when no result with the given
    /// `result_id` exists (task RFC section 13.1). Code is stable:
    /// `task_result_unknown`.
    #[must_use]
    pub fn task_result_unknown() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_result_unknown",
            "no result with the requested result_id exists",
            Some("read the current result_id with task.result or task.inspect"),
        )
    }

    /// The canonical `runtime/task_snapshot_retired` error.
    ///
    /// Raised by `session.diff` with a `turn` selector when that turn's
    /// snapshots were retired with the session content (task RFC section 10).
    /// No recovery exists: retired snapshots are not reproducible. Code is
    /// stable: `task_snapshot_retired`.
    #[must_use]
    pub fn task_snapshot_retired() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_snapshot_retired",
            "the turn's snapshots were retired with the session content",
            None,
        )
    }

    /// The canonical `runtime/task_cursor_expired` error.
    ///
    /// Raised by `task.list` and `task.inspect` when the paging cursor is older
    /// than `tasks.cursor_ttl_ms` or from another daemon epoch (task RFC
    /// section 13.1). Code is stable: `task_cursor_expired`.
    #[must_use]
    pub fn task_cursor_expired() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_cursor_expired",
            "the paging cursor expired or belongs to another daemon epoch",
            Some("restart the walk from the first page without a cursor"),
        )
    }

    /// The canonical `runtime/worktree_in_use` error.
    ///
    /// Raised by `session.remove` when other active tasks use the session's
    /// worktree, including users outside the caller's host share (task RFC
    /// section 8.7), and by `worktree.remove` when a live session uses the
    /// worktree. The code is shared by both methods, so the text and the hint
    /// name no task-only remedy. Code is stable: `worktree_in_use`.
    #[must_use]
    pub fn worktree_in_use() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "worktree_in_use",
            "live sessions or active tasks use this worktree",
            Some("stop the sessions or tasks that use the worktree, then retry the removal"),
        )
    }

    /// The canonical `runtime/task_result_pending` error.
    ///
    /// Raised by `task.result` and `task.continue` while the turn settled but
    /// its checks have not finished, and by task-layer input while checks run
    /// (task RFC section 12). Code is stable: `task_result_pending`.
    #[must_use]
    pub fn task_result_pending() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_result_pending",
            "the turn settled but its checks have not finished",
            Some("wait for publication with task.wait, then retry"),
        )
    }

    /// The canonical `configuration/task_check_unconfined` error.
    ///
    /// Raised by `task.start` and `task.continue` when the platform offers no
    /// kernel-enforced containment for checks and the project's host
    /// configuration does not set `checks.allow_unconfined` (task RFC section
    /// 12). The caller's request is valid; only an owner configuration opt-in
    /// admits it, so the class is `configuration`. Code is stable:
    /// `task_check_unconfined`.
    #[must_use]
    pub fn task_check_unconfined() -> Self {
        Self::fixed(
            ErrorClass::Configuration,
            "task_check_unconfined",
            "checks cannot be contained on this platform and unconfined checks are not allowed",
            Some("run without checks, or have the host owner set checks.allow_unconfined for the project"),
        )
    }

    /// The canonical `runtime/task_check_not_permitted` error.
    ///
    /// Raised by `task.start` and `task.continue` when a requested check is not
    /// enabled for the project or not permitted for the caller's origin, before
    /// the turn is delivered (task RFC section 12). A policy refusal of the
    /// caller's request, like `notification_kind_disabled`. Code is stable:
    /// `task_check_not_permitted`.
    #[must_use]
    pub fn task_check_not_permitted() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_check_not_permitted",
            "a requested check is not enabled or not permitted for this caller",
            Some("request only checks enabled for the project and permitted for this caller"),
        )
    }

    /// The canonical `runtime/task_store_full` error.
    ///
    /// Raised by `task.start`, `task.continue` and `task.review` when a task
    /// store cap (`tasks.store_max_bytes`, `tasks.max_turns_per_task`,
    /// `tasks.max_tasks_retained`, `tasks.max_active_tasks`) would be exceeded
    /// (task RFC section 14). Code is stable: `task_store_full`.
    #[must_use]
    pub fn task_store_full() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_store_full",
            "a task store cap would be exceeded",
            Some("end finished tasks or wait for retention to free space; the host owner may raise the task store caps"),
        )
    }

    /// The canonical `runtime/task_review_limit_reached` error.
    ///
    /// Raised by `task.review` when `tasks.max_reviews_per_result` distinct
    /// reviewers already hold a verdict on the result (task RFC section 13.1).
    /// A reviewer that already holds a verdict replaces it instead. Code is
    /// stable: `task_review_limit_reached`.
    #[must_use]
    pub fn task_review_limit_reached() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_review_limit_reached",
            "the result already holds verdicts from the maximum number of reviewers",
            None,
        )
    }

    /// The canonical `runtime/task_waiter_limit_reached` error.
    ///
    /// Raised by `task.wait` when the task waiter pool (`tasks.max_waiters`,
    /// `tasks.max_waiters_per_task`) is full; session waits are unaffected
    /// (task RFC section 8.3). Code is stable: `task_waiter_limit_reached`.
    #[must_use]
    pub fn task_waiter_limit_reached() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_waiter_limit_reached",
            "the task waiter limit is currently reached",
            Some("retry the wait after another task wait completes"),
        )
    }

    /// The canonical `runtime/task_fingerprint_key_missing` error.
    ///
    /// Raised by a retried `task.start`, `task.continue` or `task.answer` when
    /// the key version a stored fingerprint names is unavailable, so the
    /// comparison is not attempted and nothing is dispatched (task RFC
    /// invariant 7). Code is stable: `task_fingerprint_key_missing`.
    #[must_use]
    pub fn task_fingerprint_key_missing() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_fingerprint_key_missing",
            "the fingerprint key version of the stored request is unavailable; nothing was compared or dispatched",
            Some("inspect the task; the host owner must restore the task fingerprint key before resubmitting"),
        )
    }

    /// The canonical `runtime/task_request_conflict` error.
    ///
    /// Raised by every idempotent task method when a reused request key
    /// carries different parameters; nothing is executed (task RFC invariant
    /// 3). Code is stable: `task_request_conflict`.
    #[must_use]
    pub fn task_request_conflict() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_request_conflict",
            "the request key was already used with different parameters; nothing was executed",
            Some("use a new client_request_id for a different request, or resend the original parameters"),
        )
    }

    /// The canonical `runtime/task_investigate_no_checks` error.
    ///
    /// Raised by `task.start` and `task.continue` when `checks` or
    /// `checks_baseline` is requested for an investigate-mode task (task RFC
    /// section 12). Code is stable: `task_investigate_no_checks`.
    #[must_use]
    pub fn task_investigate_no_checks() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_investigate_no_checks",
            "investigate-mode tasks accept neither checks nor checks_baseline",
            Some("omit checks and checks_baseline and rely on the executor task's checks"),
        )
    }

    /// The canonical `runtime/task_fork_unsupported` error.
    ///
    /// Raised by `session.fork` on a task session whose agent cannot fork its
    /// native session across per-session servers, as with `OpenCode` (task RFC
    /// section 9.3). Code is stable: `task_fork_unsupported`.
    #[must_use]
    pub fn task_fork_unsupported() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_fork_unsupported",
            "the task's agent cannot fork its session",
            Some("start a new task on the same worktree with worktree_of"),
        )
    }

    /// The canonical `runtime/task_investigate_unsupported` error.
    ///
    /// Raised by `task.start` with `mode: "investigate"` when the profile cannot
    /// enforce investigation mode; the daemon never downgrades silently (task
    /// RFC section 11.2). Code is stable: `task_investigate_unsupported`.
    #[must_use]
    pub fn task_investigate_unsupported() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "task_investigate_unsupported",
            "the selected profile cannot enforce investigation mode",
            Some("select a profile that can enforce investigation mode"),
        )
    }

    /// The canonical `runtime/check_cleanup_stuck` error.
    ///
    /// Raised by a worktree handoff (`task.start` with `worktree_of`) when a
    /// daemon-owned check process scope in the tree cannot be confirmed empty;
    /// the worktree stays occupied (task RFC invariant 11, section 12). Code is
    /// stable: `check_cleanup_stuck`.
    #[must_use]
    pub fn check_cleanup_stuck() -> Self {
        Self::fixed(
            ErrorClass::Runtime,
            "check_cleanup_stuck",
            "a daemon-owned check process in the worktree cannot be confirmed gone",
            Some("inspect the occupying task with task.inspect; the worktree stays occupied until its check processes are gone"),
        )
    }
}

/// Names the daemon an error is about: a remote host route or the local daemon.
fn daemon_target(host: Option<&str>) -> String {
    host.map_or_else(
        || "the local daemon".to_owned(),
        |host| format!("host '{host}'"),
    )
}

#[cfg(test)]
mod tests {
    use super::{ErrorClass, ProtocolError};
    use crate::version::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};

    #[test]
    fn daemon_protocol_too_old_names_the_host_both_versions_and_the_method() {
        let error = ProtocolError::daemon_protocol_too_old(
            Some("netbird:build-2"),
            "package.list",
            MIN_PROTOCOL_VERSION,
            PROTOCOL_VERSION,
        );
        assert_eq!(error.class, ErrorClass::Daemon);
        assert_eq!(error.code, "daemon_protocol_too_old");
        assert_eq!(
            error.msg,
            format!(
                "host 'netbird:build-2' runs protocol {MIN_PROTOCOL_VERSION}, but `package.list` needs protocol {PROTOCOL_VERSION}"
            )
        );
        let recover = error.recover.expect("an upgrade hint");
        assert!(recover.contains("netbird:build-2"), "{recover}");
        assert!(recover.contains(&PROTOCOL_VERSION.to_string()), "{recover}");
    }

    #[test]
    fn daemon_protocol_too_old_names_the_local_daemon_without_a_host() {
        let error = ProtocolError::daemon_protocol_too_old(
            None,
            "package.list",
            MIN_PROTOCOL_VERSION,
            PROTOCOL_VERSION,
        );
        assert!(
            error.msg.starts_with("the local daemon runs protocol"),
            "{}",
            error.msg
        );
    }

    #[test]
    fn version_translation_failed_names_only_the_target_and_the_version() {
        let error = ProtocolError::version_translation_failed(
            Some("netbird:build-2"),
            MIN_PROTOCOL_VERSION,
        );
        assert_eq!(error.class, ErrorClass::Daemon);
        assert_eq!(error.code, "version_translation_failed");
        assert!(error.msg.contains("netbird:build-2"), "{}", error.msg);
        assert!(error.recover.is_some());
    }
}
