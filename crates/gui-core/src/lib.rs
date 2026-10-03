//! Headless state, SDK bridge logic, and command rendering for `pohunek-gui`.
//!
//! This crate intentionally has no Iced dependency. The native view layer wraps
//! these async helpers in Iced `Task` and `Subscription` values.

// Rust guideline compliant 2026-10-01
#![forbid(unsafe_code)]

pub mod providers;

/// Assistant launch orchestration, re-exported from `pohunek-assistant`.
pub mod assistant {
    pub use pohunek_assistant::launch::{
        launch_with_options, prepare_with_options, select_agent, start_prepared_with_options,
        AgentSelection, AssistantPaths, Intent, KnowledgeInfo, LaunchMeta, LaunchParams,
        LaunchResult, PreparedLaunch,
    };
}

mod connection;
mod error;
mod link;
mod message;
mod review;
mod sdk;
mod state;
mod ui_state;

use protocol::{
    AgentRuntime, DaemonHealthResult, NotificationRecord, ProjectInfo, ProtocolVersion, SessionInfo,
};
use serde::{Deserialize, Serialize};

pub use pohunek_assistant::{
    runtime_is_assistant_capable, runtime_is_launchable, ConnectionOptions, HostConfig, HostId,
    HostTransport,
};
pub use pohunek_prompt::{
    render as render_prompt, Error as PromptError, Provider as PromptProvider,
};

#[doc(inline)]
pub use connection::{
    discover_hosts, host_subscription_stream, render_attach_argv, render_attach_command,
    spawn_attach_command, validate_attach_argv_template, validate_attach_shell_template,
    workspace_connection_stream, AttachCommandSpawner, AttachSpawnError, AttachSpawnIntent,
    AttachTemplateError, AttachTemplateValues,
};
#[doc(inline)]
pub use error::CoreError;
#[doc(inline)]
pub use link::{
    preview_action_prompt, preview_prompt_content, session_link_metadata, session_metadata_rows,
    MetadataRow, PromptContext, PromptLaunchParams, PromptPreview, ProviderLaunchItem,
    ProviderLaunchParams, SessionLinkKind, SessionLinkMetadata, SessionLinkProvider,
};
#[doc(inline)]
pub use message::DomainEvent;
#[doc(inline)]
pub use review::{
    default_reviews_dir, dispatch_review, new_review_id, parse_unified_diff, render_review_prompt,
    DiffFile, DiffFileStatus, DiffHunk, DiffLine, DiffLineKind, DiffModel, Review, ReviewComment,
    ReviewDispatchParams, ReviewId, ReviewLoadError, ReviewSide, ReviewSource, ReviewStatus,
    ReviewStore, ReviewStoreError, REVIEW_DISPATCHED_AT_KEY, REVIEW_SOURCE_KEY,
};
#[doc(inline)]
pub use sdk::{
    add_project, add_project_with_options, create_session, create_session_with_options,
    delete_notification, delete_notification_with_options, diff_session, diff_session_with_options,
    fork_session, fork_session_with_options, get_notification_policy_with_options,
    inspect_host_governance, inspect_host_governance_with_options, inspect_session,
    inspect_session_with_options, integration_status, integration_status_with_options,
    launch_action_prompt_with_options, launch_provider_item_with_options, list_notifications,
    list_notifications_with_options, list_project_actions, list_project_actions_with_options,
    list_projects, list_projects_with_options, load_host, load_host_snapshot,
    load_host_snapshot_with_options, read_session_output, read_session_output_with_options,
    read_session_screen, read_session_screen_with_options, remove_project,
    remove_project_with_options, remove_session, remove_session_with_options, remove_worktree,
    remove_worktree_with_options, rename_project, rename_project_with_options, rename_session,
    rename_session_with_options, resolve_project_action, resolve_project_action_with_options,
    resolve_project_prompt, resolve_project_prompt_with_options, resume_session,
    resume_session_with_options, set_notification_policy_with_options, set_session_metadata,
    set_session_metadata_with_options, show_project, show_project_with_options, stop_session,
    stop_session_with_options, update_notification, update_notification_with_options,
    wait_for_session, wait_for_session_with_options,
};
#[doc(inline)]
pub use state::{
    project_choice_labels, AgentStateEvent, ConnState, GitHubProviderScope, GitHubProviderState,
    GitHubPullRequestStatusKey, GovernanceRequestError, GovernanceRequestId, GovernanceState,
    HostEvent, HostView, LinearProviderState, NotificationFilter, NotificationIntent,
    NotificationRow, NotificationScope, ProjectChoice, ProjectRef, PromptState, ProviderOperation,
    ProviderPanel, ProviderRequestId, ProviderState, ReviewCommentEditor, ReviewDiffStatus,
    ReviewDispatchModal, ReviewLineTarget, ReviewTabState, RuntimeContinuity, SessionAccess,
    SessionGroup, SessionObservation, SessionRow, Toast, Workspace,
};
#[doc(inline)]
pub use ui_state::{default_state_dir, Selection, UiState, UiStateError, WindowSize};

const UI_STATE_FILE: &str = "ui-state.toml";
/// Stable protocol code older daemons return for unknown optional methods.
const METHOD_NOT_FOUND_CODE: &str = "method_not_found";
const DEFAULT_WINDOW_WIDTH: u32 = 960;
const DEFAULT_WINDOW_HEIGHT: u32 = 640;
/// Per-query page size used to seed the inbox from `notification.list` on
/// connect and reconcile.
///
/// The seed runs bounded queries for unread, live-default, and deleted
/// tombstone records (see [`notification_seed_queries`]), so recent unread
/// records and recent deletes are never crowded out by a long read/archive
/// history. It bounds reconcile cost while keeping realistic per-host inboxes
/// accurate.
const GUI_NOTIFICATION_SEED_LIMIT: u32 = 200;

/// Minimal daemon health facts used by the spike UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSummary {
    pub status: String,
    pub daemon_version: String,
    pub protocol_version: ProtocolVersion,
}

impl From<DaemonHealthResult> for HealthSummary {
    fn from(result: DaemonHealthResult) -> Self {
        Self {
            status: result.status,
            daemon_version: result.daemon_version,
            protocol_version: result.protocol_version,
        }
    }
}

/// A host snapshot seeded by `daemon.health`, `session.list`, `project.list`,
/// and `notification.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshot {
    pub host_id: HostId,
    pub health: HealthSummary,
    pub sessions: Vec<SessionInfo>,
    pub projects: Vec<ProjectInfo>,
    pub project_error: Option<String>,
    /// Recent notification records seeded from `notification.list`.
    ///
    /// Empty when the host daemon does not implement notifications; seeding is
    /// non-fatal so a host without the notification surface still connects.
    pub notifications: Vec<NotificationRecord>,
    /// Agent and profile names known to the daemon, seeded from `host.inspect`.
    ///
    /// This compatibility-oriented name list is not evidence that a runtime is
    /// installed or version-compatible. Launch decisions must use `runtimes`.
    pub supported_agents: Vec<String>,
    /// Full runtime inventory reported by `host.inspect`.
    pub runtimes: Vec<AgentRuntime>,
    /// Provider names reported by the host's runtime inventory.
    pub notification_providers: Vec<String>,
    /// Provider-neutral observation features advertised by `host.inspect`.
    pub observation_capabilities: ObservationCapabilities,
}

/// Host-level provider-neutral terminal observation capabilities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservationCapabilities {
    /// Whether the host can return a parsed terminal screen snapshot.
    pub terminal_read: bool,
    /// Whether the host can return raw session output by byte cursor.
    pub output_read: bool,
    /// Whether the host can wait for provider-neutral session predicates.
    pub session_wait: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_attach_uses_the_policy_resolved_selector() {
        let addr = "127.0.0.1:17421".parse().expect("test socket address");
        let host = HostConfig::tcp_with_attach_host("memory:peer-a", addr, "memory:100.64.0.2");

        assert_eq!(host.attach_host(), "memory:100.64.0.2");
    }
}
