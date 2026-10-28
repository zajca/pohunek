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
//! - Requests carry a supported `v` range. Responses and events carry the
//!   highest mutually selected exact version.
//! - Public observation and mutation DTOs reject unknown fields and invalid
//!   cursor coordinates before dispatch.
//! - Requests carry an `id` correlating the response and any related events.
//! - Errors are typed (class + machine code + human message + optional recovery
//!   hint) so `--json` consumers and operator agents can branch on them.

#![forbid(unsafe_code)]

mod assistant;
mod capabilities;
mod command;
pub mod compat;
mod decimal;
mod discovery;
mod doctor;
mod envelope;
mod error;
mod governance;
mod idle_stop;
mod integration;
mod limits;
pub mod method;
mod notification;
mod package;
mod project;
mod runtime_id;
mod secret_env;
mod session;
pub mod tolerant;
mod turn_wait;
mod version;

#[doc(inline)]
pub use assistant::{
    AssistantMaterializeParams, AssistantMaterializeResult, ConceptDeprecation, ConceptIntent,
    ConceptMeta, ConceptType,
};
#[doc(inline)]
pub use capabilities::{AgentRuntime, HostCapabilities, HostInspectParams};
#[doc(inline)]
pub use command::{CommandErrorKind, CommandRunParams, CommandRunResult};
#[doc(inline)]
pub use decimal::{
    ActivityRevision, BackgroundRevision, DecimalWireError, OutputOffset, ProcessStartIdentity,
    ReportSequence, RuntimeGeneration, SubagentRevision, TerminalWatermark,
};
#[doc(inline)]
pub use discovery::{HostClass, HostDiscoverParams, HostRecord, HostResolution, HostResolveParams};
#[doc(inline)]
pub use doctor::{DaemonDoctorResult, DaemonHealthResult, DoctorCheck, DoctorReport, DoctorStatus};
#[doc(inline)]
pub use envelope::{EnvelopeError, Event, Request, Response, StateSource};
#[doc(inline)]
pub use error::{
    official_package_id, official_package_install_only_step, official_package_install_step,
    ErrorClass, ErrorDetails, InputNotSubmittedReason, ProcessExit, ProtocolError,
};
#[doc(inline)]
pub use governance::{
    ApprovalKeyReference, EnrollmentInfo, EnrollmentRevision, EnrollmentStatus,
    GovernanceWireError, HostApprovalAlgorithm, HostApprovalSignature, HostGovernanceStatus,
    HostId, HostOwner, HostOwnerKind, OwnerRevision, PrincipalId, ProposalExpiry, ProposalId,
    ProposalNonce, QuarantineReason, RelayId, ShareSuspensionIntent, SignedTransferOutcome, TeamId,
    TransferCoordinates, TransferOutcomeCandidate, TransferOutcomeError, TransferOutcomeId,
    TransferProposal,
};
#[doc(inline)]
pub use idle_stop::{
    IdleStopOptIn, IdleStopValueError, SessionIdleStopParams, SessionIdleStopResult,
    IDLE_STOP_DEFAULT_VALUE, IDLE_STOP_METADATA_KEY,
};
#[doc(inline)]
pub use integration::{
    IntegrationAgentDoctor, IntegrationAgentStatus, IntegrationDoctorParams,
    IntegrationDoctorResult, IntegrationFinding, IntegrationFindingCode,
    IntegrationFindingSeverity, IntegrationHome, IntegrationHomeFailure, IntegrationInstallParams,
    IntegrationInstallReport, IntegrationInstallResult, IntegrationInstallState,
    IntegrationRecovery, IntegrationSelector, IntegrationStatusParams, IntegrationStatusResult,
    IntegrationUninstallParams, IntegrationUninstallReport, IntegrationUninstallResult,
    IntegrationUninstallState, ENV_DAEMON_ID, ENV_FLAG, ENV_HOOK_ADMISSION_RETRY,
    ENV_HOOK_RETRY_BACKOFF_MAX_MS, ENV_HOOK_RETRY_BACKOFF_MS, ENV_HOOK_RETRY_DEADLINE_MS,
    ENV_HOOK_TRACE_MAX_BYTES, ENV_HOOK_TRACE_PATH, ENV_PROTOCOL_VERSION, ENV_SESSION_ID,
    ENV_SOCKET_PATH, ENV_WORKER_ID, ENV_WORKER_PROTOCOL_VERSION, ENV_WORKER_SOCKET_PATH,
    EXPECTED_INTEGRATION_VERSION,
};
#[doc(inline)]
pub use limits::{
    ED25519_SIGNATURE_BYTES, ED25519_SIGNATURE_PAYLOAD_BYTES, GOVERNANCE_ID_PAYLOAD_BYTES,
    MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_TIMEOUT_MS, MAX_CONTROL_LINE_BYTES,
    MAX_IDENTITY_CLAIM_TTL_SECS, MAX_REQUEST_ID_BYTES, MAX_SESSION_DIFF_BYTES,
    MAX_SESSION_ID_BYTES, MAX_SESSION_INPUT_BYTES, MAX_SESSION_OUTPUT_BYTES,
    MAX_SESSION_READ_LINES, MAX_SESSION_READ_RESPONSE_BYTES, MAX_SESSION_SCREEN_RESPONSE_BYTES,
    MAX_SESSION_SECRET_ENV_ENTRIES, MAX_SESSION_SECRET_ENV_NAME_BYTES,
    MAX_SESSION_SECRET_ENV_VALUE_BYTES, MAX_SESSION_TURN_CURSOR_BYTES, MAX_SESSION_WAIT_MS,
    MAX_SESSION_WAIT_TURN_MS, MAX_SESSION_WAIT_TURN_TARGETS, MAX_SUCCESS_RESPONSE_ENVELOPE_BYTES,
    MAX_WORKER_INSTANCE_ID_BYTES, OBSERVATION_RESPONSE_ENVELOPE_HEADROOM_BYTES,
    SESSION_OUTPUT_METADATA_HEADROOM_BYTES,
};
#[doc(inline)]
pub use method::Method;
#[doc(inline)]
pub use notification::{
    NotificationCreateParams, NotificationCreateResult, NotificationCreatedEvent,
    NotificationDeleteParams, NotificationDeleteResult, NotificationDeletedEvent, NotificationId,
    NotificationKind, NotificationKindPolicy, NotificationListParams, NotificationListResult,
    NotificationPolicy, NotificationPolicyParams, NotificationPolicyResult, NotificationRecord,
    NotificationRetentionParams, NotificationRetentionPolicy, NotificationRetentionResult,
    NotificationSeverity, NotificationSource, NotificationStatus, NotificationUpdateParams,
    NotificationUpdateResult, NotificationUpdatedEvent,
};
#[doc(inline)]
pub use package::{
    CommandExposure, CommandInfo, PackageBindProfileParams, PackageBindProfileResult,
    PackageBindStatus, PackageChangeResult, PackageDoctorParams, PackageDoctorResult,
    PackageErrorKind, PackageFault, PackageFinding, PackageFindingKind, PackageInfo,
    PackageInspectParams, PackageInspectResult, PackageInstallParams, PackageInstallResult,
    PackageInstallStatus, PackageLinkParams, PackageListResult, PackageOrigin, PackageRuntimeInfo,
    PackageSelectParams, PackageSelectionBlock, PackageSelectionBlockReason,
    PackageSetEnabledParams, PackageTrust, PackageUninstallParams, PackageUninstallResult,
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
pub use runtime_id::{
    BindingFieldError, BindingProvenance, CommandName, CommandNameError, DescriptorDigest,
    HistoricalRuntime, LaunchBinding, PackageDigest, PackageId, PackageIdentity, PackageVersion,
    RuntimeId, RuntimeIdError, RuntimeRef, MAX_AGENT_RUNTIME_ID_BYTES, MAX_COMMAND_NAME_BYTES,
    MAX_PACKAGE_ID_BYTES, MAX_PACKAGE_VERSION_BYTES,
};
#[doc(inline)]
pub use secret_env::{
    SessionSecretEnv, SessionSecretEnvError, SECRET_ENV_INVALID_CODE, SECRET_ENV_INVALID_MESSAGE,
    SECRET_ENV_MISSING_CODE, SECRET_ENV_NOT_DECLARED_CODE, SESSION_NEW_SECRET_ENV_UNSUPPORTED_CODE,
};
#[doc(inline)]
pub use session::{
    AgentActivity, AgentStateEvent, AttachEvent, AttachHeader, BackgroundItem, BackgroundKind,
    BackgroundStateEvent, BackgroundStatus, BranchSource, CwdSource, DetectionRegionKind,
    DetectionRegionPreview, ForkCwdMode, ObservationParamsError, OnDiverged, RuntimeExit,
    RuntimeInventoryEntry, RuntimeInventoryEvent, RuntimeInventoryResult, RuntimeInventoryStatus,
    RuntimeState, SessionAttachParams, SessionAttachResult, SessionBackground, SessionCapabilities,
    SessionCapabilityRefusals, SessionDetachParams, SessionDetachResult, SessionDetectionParams,
    SessionDetectionResult, SessionDiffParams, SessionDiffResult, SessionEvent, SessionForkParams,
    SessionForkResult, SessionId, SessionInfo, SessionInputParams, SessionInputResult,
    SessionInputWait, SessionListFilter, SessionListParams, SessionNativeRecoveredEvent,
    SessionNewParams, SessionNewResult, SessionOutputGap, SessionOutputParams, SessionOutputResult,
    SessionPolicyParams, SessionPolicyResult, SessionReadFormat, SessionReadParams,
    SessionReadResult, SessionReadSource, SessionReleaseAgentParams, SessionReleaseAgentResult,
    SessionRemoveResult, SessionRenameParams, SessionRenameResult, SessionReportAgentParams,
    SessionReportAgentResult, SessionReportNativeIdParams, SessionReportNativeIdResult,
    SessionResizeParams, SessionResizeResult, SessionResumeParams, SessionResumeResult,
    SessionRetentionCandidate, SessionRetentionHold, SessionRetentionParams,
    SessionRetentionPolicy, SessionRetentionReason, SessionRetentionResult, SessionRuntime,
    SessionRuntimeIdentity, SessionScreenParams, SessionScreenResult, SessionSetMetadataParams,
    SessionSetMetadataResult, SessionState, SessionStopResult, SessionWaitParams,
    SessionWaitReason, SessionWaitResult, SessionWarning, SessionWarningKind, SubagentInfo,
    SubagentLifecycle, SubagentStateEvent, TerminalCursor, TerminalDimensions,
    TerminalDimensionsError, UnconfirmedProcess, WorktreeSource,
};
#[doc(inline)]
pub use turn_wait::{
    SessionTurnCursor, SessionTurnEvent, SessionTurnEventKind, SessionTurnProgress,
    SessionWaitTurnParams, SessionWaitTurnReason, SessionWaitTurnResult, TurnWaitParamsError,
};
#[doc(inline)]
pub use version::{
    negotiate, ProtocolVersion, ProtocolVersionError, ProtocolVersionRange,
    CLIENT_PROTOCOL_VERSIONS, CURRENT_PROTOCOL_VERSIONS, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};

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
        SUBAGENT_STATE, "subagent_state", crate::SubagentStateEvent, "SubagentStateEvent";
        BACKGROUND_STATE, "background_state", crate::BackgroundStateEvent, "BackgroundStateEvent";
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
