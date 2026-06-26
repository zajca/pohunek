//! pohunek control protocol.
//!
//! This crate defines the typed control envelopes exchanged between the CLI and
//! the daemon over the local Unix socket (and, in Phase 2, over `NetBird` TCP).
//! The wire format is newline-delimited JSON: exactly one JSON value per line
//! (see `docs/plan-phase-1.md` "Control Protocol" and `docs/architecture.md`
//! "Transport and Control Protocol").
//!
//! It is deliberately shared so the CLI and daemon cannot drift, and so Phase 2's
//! `NetBird` transport reuses it unchanged.
//!
//! Design rules carried from the plan:
//! - Every envelope carries `v` (protocol version). New fields are additive and
//!   unknown fields are ignored, so a newer peer and an older peer interoperate
//!   on the common subset.
//! - Requests carry an `id` correlating the response and any related events.
//! - Errors are typed (class + machine code + human message + optional recovery
//!   hint) so `--json` consumers and operator agents can branch on them.

#![forbid(unsafe_code)]

mod assistant;
mod capabilities;
mod discovery;
mod doctor;
mod envelope;
mod error;
mod integration;
mod project;
mod session;
mod version;

pub use assistant::{
    AssistantMaterializeParams, AssistantMaterializeResult, ConceptDeprecation, ConceptIntent,
    ConceptMeta, ConceptType,
};
pub use capabilities::{AgentRuntime, HostCapabilities};
pub use discovery::{HostClass, HostDiscoverParams, HostRecord};
pub use doctor::{DaemonDoctorResult, DoctorCheck, DoctorReport, DoctorStatus};
pub use envelope::{Event, Request, Response, StateSource};
pub use error::{ErrorClass, ProtocolError};
pub use integration::{
    IntegrationInstallParams, IntegrationInstallReport, IntegrationInstallResult, ENV_DAEMON_ID,
    ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
pub use project::{
    ActionSummary, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectListFilter, ProjectListParams,
    ProjectPromptParams, ProjectPromptResult, ProjectRemoveParams, ProjectRemoveResult,
    ProjectRenameParams, ProjectShowParams, ProjectShowResult, ProjectSource, ProjectWorktree,
    PromptLayer, ProviderKind,
};
pub use session::{
    AgentActivity, AgentKind, AttachHeader, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionDetachResult, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionListFilter, SessionListParams, SessionNewParams, SessionNewResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionState, SessionStopResult, SessionWarning, SessionWarningKind,
};
pub use version::{negotiate, ProtocolVersion, PROTOCOL_VERSION};

/// Control-protocol method names (Phase 1).
///
/// These are the `method` values a request may carry. They are kept as
/// constants rather than an enum because the wire field is an open string: an
/// older daemon must be able to receive a method it does not know and answer
/// with a typed `method_not_found` error instead of failing to deserialize.
///
/// See `docs/plan-phase-1.md` "Control Protocol" (Methods, Phase 1). Only
/// `daemon.health` is handled by the daemon in milestone 2; the rest are
/// declared here so the contract is stable as later milestones land.
pub mod method {
    /// Liveness/version probe. Implemented in milestone 2.
    pub const DAEMON_HEALTH: &str = "daemon.health";

    // --- Declared for later milestones (not yet handled by the daemon). ---
    pub const SESSION_NEW: &str = "session.new";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_INSPECT: &str = "session.inspect";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_ATTACH: &str = "session.attach";
    pub const SESSION_DETACH: &str = "session.detach";
    pub const SESSION_RESIZE: &str = "session.resize";
    pub const SESSION_INPUT: &str = "session.input";
    pub const STATUS: &str = "status";
    pub const SUBSCRIBE: &str = "subscribe";
    /// Fire-and-forget native-session-id capture from the agent hook.
    pub const SESSION_REPORT_NATIVE_ID: &str = "session.report_native_id";
    /// Install the per-agent `SessionStart` hook that captures the native id.
    pub const INTEGRATION_INSTALL: &str = "integration.install";
    /// Live host capability probe (Phase 2 / remote hosts over `NetBird`). Returns
    /// a [`HostCapabilities`](crate::HostCapabilities) snapshot.
    pub const HOST_INSPECT: &str = "host.inspect";
    /// Materialize the embedded assistant knowledge bundle on the agent host.
    pub const ASSISTANT_MATERIALIZE: &str = "assistant.materialize";
    /// Run daemon-local doctor checks on the host that owns the daemon.
    pub const DAEMON_DOCTOR: &str = "daemon.doctor";
    /// Enumerate and classify the local host's `NetBird` peers. Handled by the
    /// local daemon, which caches the result for a short TTL. Returns a
    /// `Vec<`[`HostRecord`](crate::HostRecord)`>`.
    pub const HOST_DISCOVER: &str = "host.discover";

    // --- Projects (git-repo awareness). Resolved per host against its own store.
    /// List known projects on the target host. Returns
    /// `Vec<`[`ProjectInfo`](crate::ProjectInfo)`>`.
    pub const PROJECT_LIST: &str = "project.list";
    /// Register (or re-add) a project by host-local path. Returns a
    /// [`ProjectInfo`](crate::ProjectInfo).
    pub const PROJECT_ADD: &str = "project.add";
    /// Show a project plus its live worktrees. Returns a
    /// [`ProjectShowResult`](crate::ProjectShowResult).
    pub const PROJECT_SHOW: &str = "project.show";
    /// Set a project's custom display name. Returns a
    /// [`ProjectInfo`](crate::ProjectInfo).
    pub const PROJECT_RENAME: &str = "project.rename";
    /// Forget a project record (optionally pruning owned worktrees). Returns a
    /// [`ProjectRemoveResult`](crate::ProjectRemoveResult).
    pub const PROJECT_REMOVE: &str = "project.remove";
    /// Resolve one prompt by name to its template content, fail-closed
    /// (`prompt_not_found`). Returns a [`ProjectPromptResult`](crate::ProjectPromptResult).
    pub const PROJECT_PROMPT: &str = "project.prompt";
    /// Resolve one action by name to its recipe plus prompt content, fail-closed
    /// (`action_not_found`/`template_not_found`). Returns a
    /// [`ProjectActionResult`](crate::ProjectActionResult).
    pub const PROJECT_ACTION: &str = "project.action";
    /// List available project actions after in-repo-over-host shadowing. Returns a
    /// [`ProjectActionsResult`](crate::ProjectActionsResult).
    pub const PROJECT_ACTIONS: &str = "project.actions";
}

/// Control-protocol event names.
///
/// These are the `event` values published on subscription connections. The
/// payload remains an open JSON object at the envelope layer.
pub mod event {
    pub const AGENT_STATE: &str = "agent_state";
    pub const ATTACH_OPENED: &str = "attach_opened";
    pub const ATTACH_CLOSED: &str = "attach_closed";
    pub const SESSION_CREATED: &str = "session_created";
    pub const SESSION_UPDATED: &str = "session_updated";
    pub const SESSION_STOPPED: &str = "session_stopped";
}
