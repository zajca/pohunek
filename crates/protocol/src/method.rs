//! Control-protocol method names and typed method contracts.
//!
//! The wire method remains an open string so older daemons can reject unknown
//! methods with a typed protocol error. The marker types in this module add the
//! compile-time pairing of method name, params, and result for SDK clients while
//! preserving the existing JSON envelope shape.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{
    AssistantMaterializeParams, AssistantMaterializeResult, DaemonDoctorResult, DaemonHealthResult,
    HostCapabilities, HostDiscoverParams, HostRecord, IntegrationInstallParams,
    IntegrationInstallResult, NotificationCreateParams, NotificationCreateResult,
    NotificationDeleteParams, NotificationDeleteResult, NotificationListParams,
    NotificationListResult, NotificationPolicyParams, NotificationPolicyResult,
    NotificationRetentionParams, NotificationRetentionResult, NotificationUpdateParams,
    NotificationUpdateResult, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectListParams, ProjectPromptParams,
    ProjectPromptResult, ProjectRemoveParams, ProjectRemoveResult, ProjectRenameParams,
    ProjectShowParams, ProjectShowResult, SessionAttachParams, SessionAttachResult,
    SessionDetachParams, SessionDetachResult, SessionId, SessionInfo, SessionInputParams,
    SessionInputResult, SessionListParams, SessionNewParams, SessionNewResult,
    SessionReleaseAgentParams, SessionReleaseAgentResult, SessionRemoveResult, SessionRenameParams,
    SessionRenameResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionResumeResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionStopResult, WorktreeRemoveParams, WorktreeRemoveResult,
};

/// A typed control-protocol method contract.
///
/// SDK callers use this to serialize the right params shape and deserialize the
/// right success payload for a method name. The daemon still receives the generic
/// [`crate::Request`] envelope and performs its own validation.
pub trait Method {
    /// Wire method name.
    const NAME: &'static str;
    /// Method-specific request params.
    type Params: Serialize + DeserializeOwned;
    /// Method-specific success payload.
    type Output: DeserializeOwned;
}

macro_rules! method_marker {
    (
        $(#[$docs:meta])*
        $marker:ident, $constant:ident, $name:literal, $params:ty, $output:ty
    ) => {
        $(#[$docs])*
        pub const $constant: &str = $name;

        $(#[$docs])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $marker;

        impl Method for $marker {
            const NAME: &'static str = $constant;
            type Params = $params;
            type Output = $output;
        }
    };
}

method_marker!(
    /// Liveness/version probe.
    DaemonHealth,
    DAEMON_HEALTH,
    "daemon.health",
    (),
    DaemonHealthResult
);

method_marker!(
    /// Start a new agent session.
    SessionNew,
    SESSION_NEW,
    "session.new",
    SessionNewParams,
    SessionNewResult
);

method_marker!(
    /// List sessions.
    SessionList,
    SESSION_LIST,
    "session.list",
    SessionListParams,
    Vec<SessionInfo>
);

method_marker!(
    /// Inspect one session.
    SessionInspect,
    SESSION_INSPECT,
    "session.inspect",
    SessionId,
    SessionInfo
);

method_marker!(
    /// Stop one session.
    SessionStop,
    SESSION_STOP,
    "session.stop",
    SessionId,
    SessionStopResult
);

method_marker!(
    /// Relaunch a terminal session from captured native resume metadata.
    SessionResume,
    SESSION_RESUME,
    "session.resume",
    SessionId,
    SessionResumeResult
);

method_marker!(
    /// Evict a session from the registry, stopping it first if still live.
    SessionRemove,
    SESSION_REMOVE,
    "session.remove",
    SessionId,
    SessionRemoveResult
);

method_marker!(
    /// Create an attach stream for a session.
    SessionAttach,
    SESSION_ATTACH,
    "session.attach",
    SessionAttachParams,
    SessionAttachResult
);

method_marker!(
    /// Detach an active attach stream.
    SessionDetach,
    SESSION_DETACH,
    "session.detach",
    SessionDetachParams,
    SessionDetachResult
);

method_marker!(
    /// Resize a session PTY.
    SessionResize,
    SESSION_RESIZE,
    "session.resize",
    SessionResizeParams,
    SessionResizeResult
);

method_marker!(
    /// Inject input into a session PTY.
    SessionInput,
    SESSION_INPUT,
    "session.input",
    SessionInputParams,
    SessionInputResult
);

/// Historical status method name. Kept as an open-string constant for
/// compatibility with older docs and consumers; current daemons answer
/// [`DAEMON_HEALTH`] for health/status checks.
pub const STATUS: &str = "status";

method_marker!(
    /// Subscribe to daemon events.
    Subscribe,
    SUBSCRIBE,
    "subscribe",
    (),
    serde_json::Value
);

method_marker!(
    /// Record native session metadata reported by an agent hook.
    SessionReportNativeId,
    SESSION_REPORT_NATIVE_ID,
    "session.report_native_id",
    SessionReportNativeIdParams,
    SessionReportNativeIdResult
);

method_marker!(
    /// Record active nested-agent metadata reported by an inherited hook.
    SessionReportAgent,
    SESSION_REPORT_AGENT,
    "session.report_agent",
    SessionReportAgentParams,
    SessionReportAgentResult
);

method_marker!(
    /// Release active nested-agent metadata reported by an inherited hook.
    SessionReleaseAgent,
    SESSION_RELEASE_AGENT,
    "session.release_agent",
    SessionReleaseAgentParams,
    SessionReleaseAgentResult
);

method_marker!(
    /// Merge owner-controlled metadata for a session.
    SessionSetMetadata,
    SESSION_SET_METADATA,
    "session.set_metadata",
    SessionSetMetadataParams,
    SessionSetMetadataResult
);

method_marker!(
    /// Set or clear a session's owner-set display name.
    SessionRename,
    SESSION_RENAME,
    "session.rename",
    SessionRenameParams,
    SessionRenameResult
);

method_marker!(
    /// Install per-agent native-session capture hooks.
    IntegrationInstall,
    INTEGRATION_INSTALL,
    "integration.install",
    IntegrationInstallParams,
    IntegrationInstallResult
);

method_marker!(
    /// Inspect live host capabilities.
    HostInspect,
    HOST_INSPECT,
    "host.inspect",
    (),
    HostCapabilities
);

method_marker!(
    /// Materialize the embedded assistant knowledge bundle on the agent host.
    AssistantMaterialize,
    ASSISTANT_MATERIALIZE,
    "assistant.materialize",
    AssistantMaterializeParams,
    AssistantMaterializeResult
);

method_marker!(
    /// Run daemon-local doctor checks.
    DaemonDoctor,
    DAEMON_DOCTOR,
    "daemon.doctor",
    (),
    DaemonDoctorResult
);

method_marker!(
    /// Enumerate and classify the local host's `NetBird` peers.
    HostDiscover,
    HOST_DISCOVER,
    "host.discover",
    HostDiscoverParams,
    Vec<HostRecord>
);

method_marker!(
    /// Create a durable notification record.
    NotificationCreate,
    NOTIFICATION_CREATE,
    "notification.create",
    NotificationCreateParams,
    NotificationCreateResult
);

method_marker!(
    /// List durable notification records.
    NotificationList,
    NOTIFICATION_LIST,
    "notification.list",
    NotificationListParams,
    NotificationListResult
);

method_marker!(
    /// Update a notification lifecycle status.
    NotificationUpdate,
    NOTIFICATION_UPDATE,
    "notification.update",
    NotificationUpdateParams,
    NotificationUpdateResult
);

method_marker!(
    /// Delete a notification record.
    NotificationDelete,
    NOTIFICATION_DELETE,
    "notification.delete",
    NotificationDeleteParams,
    NotificationDeleteResult
);

method_marker!(
    /// Read the notification policy.
    NotificationPolicyGet,
    NOTIFICATION_POLICY_GET,
    "notification.policy.get",
    (),
    NotificationPolicyResult
);

method_marker!(
    /// Replace the notification policy.
    NotificationPolicySet,
    NOTIFICATION_POLICY_SET,
    "notification.policy.set",
    NotificationPolicyParams,
    NotificationPolicyResult
);

method_marker!(
    /// Prune notifications through the retention policy.
    NotificationRetentionPrune,
    NOTIFICATION_RETENTION_PRUNE,
    "notification.retention.prune",
    NotificationRetentionParams,
    NotificationRetentionResult
);

method_marker!(
    /// List known projects on the target host.
    ProjectList,
    PROJECT_LIST,
    "project.list",
    ProjectListParams,
    Vec<ProjectInfo>
);

method_marker!(
    /// Register or re-add a project by host-local path.
    ProjectAdd,
    PROJECT_ADD,
    "project.add",
    ProjectAddParams,
    ProjectInfo
);

method_marker!(
    /// Show a project plus its live worktrees.
    ProjectShow,
    PROJECT_SHOW,
    "project.show",
    ProjectShowParams,
    ProjectShowResult
);

method_marker!(
    /// Set a project's custom display name.
    ProjectRename,
    PROJECT_RENAME,
    "project.rename",
    ProjectRenameParams,
    ProjectInfo
);

method_marker!(
    /// Forget a project record, optionally pruning owned worktrees.
    ProjectRemove,
    PROJECT_REMOVE,
    "project.remove",
    ProjectRemoveParams,
    ProjectRemoveResult
);

method_marker!(
    /// Resolve one prompt by name to template content.
    ProjectPrompt,
    PROJECT_PROMPT,
    "project.prompt",
    ProjectPromptParams,
    ProjectPromptResult
);

method_marker!(
    /// Resolve one action by name to launch recipe and prompt content.
    ProjectAction,
    PROJECT_ACTION,
    "project.action",
    ProjectActionParams,
    ProjectActionResult
);

method_marker!(
    /// List available project actions after layer shadowing.
    ProjectActions,
    PROJECT_ACTIONS,
    "project.actions",
    ProjectActionsParams,
    ProjectActionsResult
);

method_marker!(
    /// Remove one pohunek-owned worktree by path.
    WorktreeRemove,
    WORKTREE_REMOVE,
    "worktree.remove",
    WorktreeRemoveParams,
    WorktreeRemoveResult
);
