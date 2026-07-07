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
pub mod method;
mod notification;
mod project;
mod session;
mod version;

#[doc(inline)]
pub use assistant::{
    AssistantMaterializeParams, AssistantMaterializeResult, ConceptDeprecation, ConceptIntent,
    ConceptMeta, ConceptType,
};
#[doc(inline)]
pub use capabilities::{AgentRuntime, HostCapabilities};
#[doc(inline)]
pub use discovery::{HostClass, HostDiscoverParams, HostRecord};
#[doc(inline)]
pub use doctor::{DaemonDoctorResult, DaemonHealthResult, DoctorCheck, DoctorReport, DoctorStatus};
#[doc(inline)]
pub use envelope::{Event, Request, Response, StateSource};
#[doc(inline)]
pub use error::{ErrorClass, ProtocolError};
#[doc(inline)]
pub use integration::{
    IntegrationInstallParams, IntegrationInstallReport, IntegrationInstallResult, ENV_DAEMON_ID,
    ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
#[doc(inline)]
pub use method::Method;
#[doc(inline)]
pub use notification::{
    NotificationCreateParams, NotificationCreateResult, NotificationCreatedEvent,
    NotificationDeleteParams, NotificationDeleteResult, NotificationDeletedEvent, NotificationId,
    NotificationKind, NotificationKindPolicy, NotificationListParams, NotificationListResult,
    NotificationPolicy, NotificationPolicyParams, NotificationPolicyResult, NotificationRecord,
    NotificationRetentionParams, NotificationRetentionResult, NotificationSeverity,
    NotificationSource, NotificationStatus, NotificationUpdateParams, NotificationUpdateResult,
    NotificationUpdatedEvent,
};
#[doc(inline)]
pub use project::{
    ActionSummary, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectListFilter, ProjectListParams,
    ProjectPromptParams, ProjectPromptResult, ProjectRemoveParams, ProjectRemoveResult,
    ProjectRenameParams, ProjectShowParams, ProjectShowResult, ProjectSource, ProjectWorktree,
    PromptLayer, ProviderKind, WorktreeRemoveParams, WorktreeRemoveResult,
};
#[doc(inline)]
pub use session::{
    AgentActivity, AgentKind, AttachHeader, CwdSource, ForkCwdMode, SessionAttachParams,
    SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionForkParams,
    SessionForkResult, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionListFilter, SessionListParams, SessionNewParams, SessionNewResult,
    SessionReleaseAgentParams, SessionReleaseAgentResult, SessionRemoveResult, SessionRenameParams,
    SessionRenameResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionResumeResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionState, SessionStopResult, SessionWarning, SessionWarningKind,
};
#[doc(inline)]
pub use version::{negotiate, ProtocolVersion, PROTOCOL_VERSION};

/// Control-protocol event names.
///
/// These are the `event` values published on subscription connections. The
/// payload remains an open JSON object at the envelope layer.
pub mod event {
    pub const AGENT_STATE: &str = "agent_state";
    pub const ATTACH_OPENED: &str = "attach_opened";
    pub const ATTACH_CLOSED: &str = "attach_closed";
    pub const NOTIFICATION_CREATED: &str = "notification_created";
    pub const NOTIFICATION_UPDATED: &str = "notification_updated";
    pub const NOTIFICATION_DELETED: &str = "notification_deleted";
    pub const SESSION_CREATED: &str = "session_created";
    pub const SESSION_UPDATED: &str = "session_updated";
    pub const SESSION_STOPPED: &str = "session_stopped";
    pub const SESSION_REMOVED: &str = "session_removed";
}
