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
mod limits;
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
    ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH, ENV_WORKER_ID,
    ENV_WORKER_PROTOCOL_VERSION, ENV_WORKER_SOCKET_PATH,
};
#[doc(inline)]
pub use limits::{MAX_CONTROL_LINE_BYTES, MAX_SESSION_DIFF_BYTES};
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
    AgentActivity, AgentKind, AgentStateEvent, AttachEvent, AttachHeader, CwdSource, ForkCwdMode,
    RuntimeInventoryEntry, RuntimeInventoryEvent, RuntimeInventoryResult, RuntimeInventoryStatus,
    RuntimeState, SessionAttachParams, SessionAttachResult, SessionDetachParams,
    SessionDetachResult, SessionDiffParams, SessionDiffResult, SessionEvent, SessionForkParams,
    SessionForkResult, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionListFilter, SessionListParams, SessionNativeRecoveredEvent, SessionNewParams,
    SessionNewResult, SessionReleaseAgentParams, SessionReleaseAgentResult, SessionRemoveResult,
    SessionRenameParams, SessionRenameResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionResumeResult, SessionRuntime, SessionSetMetadataParams,
    SessionSetMetadataResult, SessionState, SessionStopResult, SessionWarning, SessionWarningKind,
    TerminalDimensions, TerminalDimensionsError,
};
#[doc(inline)]
pub use version::{negotiate, ProtocolVersion, PROTOCOL_VERSION};

/// Control-protocol event names.
///
/// These are the `event` values published on subscription connections. The
/// payload remains an open JSON object at the envelope layer.
pub mod event {
    /// Static TypeScript metadata for one protocol event payload.
    ///
    /// `cargo xtask ts generate` consumes this table to emit the discriminated
    /// event union from the same source that declares the wire event names.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EventSpec {
        /// Wire event name.
        pub name: &'static str,
        /// TypeScript payload type exported by the generated protocol bindings.
        pub payload_ts: &'static str,
    }

    const fn payload_ts<T>(name: &'static str) -> &'static str {
        let _ = core::mem::size_of::<Option<fn() -> T>>();
        name
    }

    macro_rules! event_table {
        (
            $(
                $constant:ident,
                $name:literal,
                $payload:ty,
                $payload_ts:literal
            );+ $(;)?
        ) => {
            $(
                pub const $constant: &str = $name;
            )+

            /// Event metadata used by TypeScript binding generation.
            pub const EVENT_SPECS: &[EventSpec] = &[
                $(
                    EventSpec {
                        name: $constant,
                        payload_ts: payload_ts::<$payload>($payload_ts),
                    },
                )+
            ];
        };
    }

    event_table!(
        AGENT_STATE, "agent_state", crate::AgentStateEvent, "AgentStateEvent";
        ATTACH_OPENED, "attach_opened", crate::AttachEvent, "AttachEvent";
        ATTACH_CLOSED, "attach_closed", crate::AttachEvent, "AttachEvent";
        NOTIFICATION_CREATED, "notification_created", crate::NotificationCreatedEvent, "NotificationCreatedEvent";
        NOTIFICATION_UPDATED, "notification_updated", crate::NotificationUpdatedEvent, "NotificationUpdatedEvent";
        NOTIFICATION_DELETED, "notification_deleted", crate::NotificationDeletedEvent, "NotificationDeletedEvent";
        SESSION_CREATED, "session_created", crate::SessionEvent, "SessionEvent";
        SESSION_UPDATED, "session_updated", crate::SessionEvent, "SessionEvent";
        SESSION_STOPPED, "session_stopped", crate::SessionEvent, "SessionEvent";
        SESSION_REMOVED, "session_removed", crate::SessionEvent, "SessionEvent";
        SESSION_RUNTIME_RECONNECTED, "session_runtime_reconnected", crate::SessionEvent, "SessionEvent";
        SESSION_RUNTIME_LOST, "session_runtime_lost", crate::SessionEvent, "SessionEvent";
        SESSION_RUNTIME_CONFLICT, "session_runtime_conflict", crate::SessionEvent, "SessionEvent";
        SESSION_RUNTIME_DISCOVERED, "session_runtime_discovered", crate::RuntimeInventoryEvent, "RuntimeInventoryEvent";
        SESSION_NATIVE_RECOVERED, "session_native_recovered", crate::SessionNativeRecoveredEvent, "SessionNativeRecoveredEvent";
    );

    #[cfg(test)]
    mod tests {
        use std::collections::BTreeSet;

        use super::*;

        #[test]
        fn event_specs_have_unique_wire_names() {
            let mut names = BTreeSet::new();
            for spec in EVENT_SPECS {
                assert!(
                    names.insert(spec.name),
                    "duplicate event spec {}",
                    spec.name
                );
            }
            assert_eq!(names.len(), EVENT_SPECS.len());
        }
    }
}
