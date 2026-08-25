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
    ProjectShowParams, ProjectShowResult, RuntimeInventoryResult, SessionAttachParams,
    SessionAttachResult, SessionDetachParams, SessionDetachResult, SessionDiffParams,
    SessionDiffResult, SessionForkParams, SessionForkResult, SessionId, SessionInfo,
    SessionInputParams, SessionInputResult, SessionListParams, SessionNewParams, SessionNewResult,
    SessionOutputParams, SessionOutputResult, SessionReadParams, SessionReadResult,
    SessionReleaseAgentParams, SessionReleaseAgentResult, SessionRemoveResult, SessionRenameParams,
    SessionRenameResult, SessionReportAgentParams, SessionReportAgentResult,
    SessionReportNativeIdParams, SessionReportNativeIdResult, SessionResizeParams,
    SessionResizeResult, SessionResumeResult, SessionScreenParams, SessionScreenResult,
    SessionSetMetadataParams, SessionSetMetadataResult, SessionStopResult, SessionWaitParams,
    SessionWaitResult, WorktreeRemoveParams, WorktreeRemoveResult,
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

/// Static TypeScript metadata for one control method.
///
/// The `params_ts` and `output_ts` strings are TypeScript type references used
/// by `cargo xtask ts generate` when it emits the SDK method map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    /// Wire method name.
    pub name: &'static str,
    /// TypeScript request params type.
    pub params_ts: &'static str,
    /// TypeScript success payload type.
    pub output_ts: &'static str,
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

macro_rules! method_table {
    (
        $(
            $(#[$docs:meta])*
            $marker:ident,
            $constant:ident,
            $name:literal,
            $params:ty,
            $output:ty,
            $params_ts:literal,
            $output_ts:literal
        );+ $(;)?
    ) => {
        $(
            method_marker!(
                $(#[$docs])*
                $marker,
                $constant,
                $name,
                $params,
                $output
            );
        )+

        /// TypeScript method-map metadata generated from the marker table.
        pub const METHOD_SPECS: &[MethodSpec] = &[
            $(
                MethodSpec {
                    name: $name,
                    params_ts: $params_ts,
                    output_ts: $output_ts,
                },
            )+
        ];
    };
}

/// Historical status method name. Kept as an open-string constant for
/// compatibility with older docs and consumers; current daemons answer
/// [`DAEMON_HEALTH`] for health/status checks.
pub const STATUS: &str = "status";

method_table!(
    /// Liveness/version probe.
    DaemonHealth,
    DAEMON_HEALTH,
    "daemon.health",
    (),
    DaemonHealthResult,
    "null",
    "DaemonHealthResult";

    /// Start a new agent session.
    SessionNew,
    SESSION_NEW,
    "session.new",
    SessionNewParams,
    SessionNewResult,
    "SessionNewParams",
    "SessionNewResult";

    /// List sessions.
    SessionList,
    SESSION_LIST,
    "session.list",
    SessionListParams,
    Vec<SessionInfo>,
    "SessionListParams",
    "SessionInfo[]";

    /// List authenticated and quarantined durable worker endpoints.
    SessionRuntimeInventory,
    SESSION_RUNTIME_INVENTORY,
    "session.runtime_inventory",
    (),
    RuntimeInventoryResult,
    "null",
    "RuntimeInventoryResult";

    /// Inspect one session.
    SessionInspect,
    SESSION_INSPECT,
    "session.inspect",
    SessionId,
    SessionInfo,
    "SessionId",
    "SessionInfo";

    /// Stop one session.
    SessionStop,
    SESSION_STOP,
    "session.stop",
    SessionId,
    SessionStopResult,
    "SessionId",
    "SessionStopResult";

    /// Relaunch a terminal session from captured native resume metadata.
    SessionResume,
    SESSION_RESUME,
    "session.resume",
    SessionId,
    SessionResumeResult,
    "SessionId",
    "SessionResumeResult";

    /// Fork a native agent conversation into a new pohunek session.
    SessionFork,
    SESSION_FORK,
    "session.fork",
    SessionForkParams,
    SessionForkResult,
    "SessionForkParams",
    "SessionForkResult";

    /// Evict a session from the registry, stopping it first if still live.
    SessionRemove,
    SESSION_REMOVE,
    "session.remove",
    SessionId,
    SessionRemoveResult,
    "SessionId",
    "SessionRemoveResult";

    /// Create an attach stream for a session.
    SessionAttach,
    SESSION_ATTACH,
    "session.attach",
    SessionAttachParams,
    SessionAttachResult,
    "SessionAttachParams",
    "SessionAttachResult";

    /// Detach an active attach stream.
    SessionDetach,
    SESSION_DETACH,
    "session.detach",
    SessionDetachParams,
    SessionDetachResult,
    "SessionDetachParams",
    "SessionDetachResult";

    /// Resize a session PTY.
    SessionResize,
    SESSION_RESIZE,
    "session.resize",
    SessionResizeParams,
    SessionResizeResult,
    "SessionResizeParams",
    "SessionResizeResult";

    /// Inject input into a session PTY.
    SessionInput,
    SESSION_INPUT,
    "session.input",
    SessionInputParams,
    SessionInputResult,
    "SessionInputParams",
    "SessionInputResult";

    /// Read a bounded point-in-time rendered terminal snapshot.
    SessionScreen,
    SESSION_SCREEN,
    "session.screen",
    SessionScreenParams,
    SessionScreenResult,
    "SessionScreenParams",
    "SessionScreenResult";

    /// Read bounded retained PTY output without taking attach ownership.
    SessionOutput,
    SESSION_OUTPUT,
    "session.output",
    SessionOutputParams,
    SessionOutputResult,
    "SessionOutputParams",
    "SessionOutputResult";

    /// Read a bounded point-in-time terminal text capture.
    SessionRead,
    SESSION_READ,
    "session.read",
    SessionReadParams,
    SessionReadResult,
    "SessionReadParams",
    "SessionReadResult";

    /// Wait on a bounded dedicated connection for session activity.
    SessionWait,
    SESSION_WAIT,
    "session.wait",
    SessionWaitParams,
    SessionWaitResult,
    "SessionWaitParams",
    "SessionWaitResult";

    /// Subscribe to daemon events.
    Subscribe,
    SUBSCRIBE,
    "subscribe",
    (),
    serde_json::Value,
    "null",
    "JsonValue";

    /// Record native session metadata reported by an agent hook.
    SessionReportNativeId,
    SESSION_REPORT_NATIVE_ID,
    "session.report_native_id",
    SessionReportNativeIdParams,
    SessionReportNativeIdResult,
    "SessionReportNativeIdParams",
    "SessionReportNativeIdResult";

    /// Record active nested-agent metadata reported by an inherited hook.
    SessionReportAgent,
    SESSION_REPORT_AGENT,
    "session.report_agent",
    SessionReportAgentParams,
    SessionReportAgentResult,
    "SessionReportAgentParams",
    "SessionReportAgentResult";

    /// Release active nested-agent metadata reported by an inherited hook.
    SessionReleaseAgent,
    SESSION_RELEASE_AGENT,
    "session.release_agent",
    SessionReleaseAgentParams,
    SessionReleaseAgentResult,
    "SessionReleaseAgentParams",
    "SessionReleaseAgentResult";

    /// Merge owner-controlled metadata for a session.
    SessionSetMetadata,
    SESSION_SET_METADATA,
    "session.set_metadata",
    SessionSetMetadataParams,
    SessionSetMetadataResult,
    "SessionSetMetadataParams",
    "SessionSetMetadataResult";

    /// Set or clear a session's owner-set display name.
    SessionRename,
    SESSION_RENAME,
    "session.rename",
    SessionRenameParams,
    SessionRenameResult,
    "SessionRenameParams",
    "SessionRenameResult";

    /// Compute a unified diff of a session's worktree against its base.
    SessionDiff,
    SESSION_DIFF,
    "session.diff",
    SessionDiffParams,
    SessionDiffResult,
    "SessionDiffParams",
    "SessionDiffResult";

    /// Install per-agent native-session capture hooks.
    IntegrationInstall,
    INTEGRATION_INSTALL,
    "integration.install",
    IntegrationInstallParams,
    IntegrationInstallResult,
    "IntegrationInstallParams",
    "IntegrationInstallResult";

    /// Inspect live host capabilities.
    HostInspect,
    HOST_INSPECT,
    "host.inspect",
    (),
    HostCapabilities,
    "null",
    "HostCapabilities";

    /// Materialize the embedded assistant knowledge bundle on the agent host.
    AssistantMaterialize,
    ASSISTANT_MATERIALIZE,
    "assistant.materialize",
    AssistantMaterializeParams,
    AssistantMaterializeResult,
    "AssistantMaterializeParams",
    "AssistantMaterializeResult";

    /// Run daemon-local doctor checks.
    DaemonDoctor,
    DAEMON_DOCTOR,
    "daemon.doctor",
    (),
    DaemonDoctorResult,
    "null",
    "DaemonDoctorResult";

    /// Enumerate and classify the local host's `NetBird` peers.
    HostDiscover,
    HOST_DISCOVER,
    "host.discover",
    HostDiscoverParams,
    Vec<HostRecord>,
    "HostDiscoverParams",
    "HostRecord[]";

    /// Create a durable notification record.
    NotificationCreate,
    NOTIFICATION_CREATE,
    "notification.create",
    NotificationCreateParams,
    NotificationCreateResult,
    "NotificationCreateParams",
    "NotificationCreateResult";

    /// List durable notification records.
    NotificationList,
    NOTIFICATION_LIST,
    "notification.list",
    NotificationListParams,
    NotificationListResult,
    "NotificationListParams",
    "NotificationListResult";

    /// Update a notification lifecycle status.
    NotificationUpdate,
    NOTIFICATION_UPDATE,
    "notification.update",
    NotificationUpdateParams,
    NotificationUpdateResult,
    "NotificationUpdateParams",
    "NotificationUpdateResult";

    /// Delete a notification record.
    NotificationDelete,
    NOTIFICATION_DELETE,
    "notification.delete",
    NotificationDeleteParams,
    NotificationDeleteResult,
    "NotificationDeleteParams",
    "NotificationDeleteResult";

    /// Read the notification policy.
    NotificationPolicyGet,
    NOTIFICATION_POLICY_GET,
    "notification.policy.get",
    (),
    NotificationPolicyResult,
    "null",
    "NotificationPolicyResult";

    /// Replace the notification policy.
    NotificationPolicySet,
    NOTIFICATION_POLICY_SET,
    "notification.policy.set",
    NotificationPolicyParams,
    NotificationPolicyResult,
    "NotificationPolicyParams",
    "NotificationPolicyResult";

    /// Prune notifications through the retention policy.
    NotificationRetentionPrune,
    NOTIFICATION_RETENTION_PRUNE,
    "notification.retention.prune",
    NotificationRetentionParams,
    NotificationRetentionResult,
    "NotificationRetentionParams",
    "NotificationRetentionResult";

    /// List known projects on the target host.
    ProjectList,
    PROJECT_LIST,
    "project.list",
    ProjectListParams,
    Vec<ProjectInfo>,
    "ProjectListParams",
    "ProjectInfo[]";

    /// Register or re-add a project by host-local path.
    ProjectAdd,
    PROJECT_ADD,
    "project.add",
    ProjectAddParams,
    ProjectInfo,
    "ProjectAddParams",
    "ProjectInfo";

    /// Show a project plus its live worktrees.
    ProjectShow,
    PROJECT_SHOW,
    "project.show",
    ProjectShowParams,
    ProjectShowResult,
    "ProjectShowParams",
    "ProjectShowResult";

    /// Set a project's custom display name.
    ProjectRename,
    PROJECT_RENAME,
    "project.rename",
    ProjectRenameParams,
    ProjectInfo,
    "ProjectRenameParams",
    "ProjectInfo";

    /// Forget a project record, optionally pruning owned worktrees.
    ProjectRemove,
    PROJECT_REMOVE,
    "project.remove",
    ProjectRemoveParams,
    ProjectRemoveResult,
    "ProjectRemoveParams",
    "ProjectRemoveResult";

    /// Resolve one prompt by name to template content.
    ProjectPrompt,
    PROJECT_PROMPT,
    "project.prompt",
    ProjectPromptParams,
    ProjectPromptResult,
    "ProjectPromptParams",
    "ProjectPromptResult";

    /// Resolve one action by name to launch recipe and prompt content.
    ProjectAction,
    PROJECT_ACTION,
    "project.action",
    ProjectActionParams,
    ProjectActionResult,
    "ProjectActionParams",
    "ProjectActionResult";

    /// List available project actions after layer shadowing.
    ProjectActions,
    PROJECT_ACTIONS,
    "project.actions",
    ProjectActionsParams,
    ProjectActionsResult,
    "ProjectActionsParams",
    "ProjectActionsResult";

    /// Remove one pohunek-owned worktree by path.
    WorktreeRemove,
    WORKTREE_REMOVE,
    "worktree.remove",
    WorktreeRemoveParams,
    WorktreeRemoveResult,
    "WorktreeRemoveParams",
    "WorktreeRemoveResult";
);

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn spec(name: &str) -> &'static MethodSpec {
        METHOD_SPECS
            .iter()
            .find(|spec| spec.name == name)
            .expect("method spec exists")
    }

    #[test]
    fn method_specs_pin_special_ts_mappings() {
        assert_eq!(spec(DAEMON_HEALTH).params_ts, "null");
        assert_eq!(spec(DAEMON_HEALTH).output_ts, "DaemonHealthResult");
        assert_eq!(spec(SUBSCRIBE).params_ts, "null");
        assert_eq!(spec(SUBSCRIBE).output_ts, "JsonValue");
        assert_eq!(spec(SESSION_LIST).params_ts, "SessionListParams");
        assert_eq!(spec(SESSION_LIST).output_ts, "SessionInfo[]");
        assert_eq!(spec(HOST_DISCOVER).output_ts, "HostRecord[]");
    }

    #[test]
    fn method_specs_have_unique_wire_names() {
        let mut names = BTreeSet::new();
        for spec in METHOD_SPECS {
            assert!(
                names.insert(spec.name),
                "duplicate method spec {}",
                spec.name
            );
        }
        assert_eq!(names.len(), METHOD_SPECS.len());
    }
}
