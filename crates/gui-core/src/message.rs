//! The [`DomainEvent`] enum emitted by async host/provider I/O and reduced by the workspace.

use std::path::PathBuf;

use protocol::{
    NotificationDeleteResult, NotificationUpdateResult, ProjectActionResult, ProjectActionsResult,
    ProjectInfo, ProjectPromptResult, ProjectRemoveResult, ProjectShowResult, SessionId,
    SessionInfo, SessionRemoveResult, SessionRenameResult, SessionResumeResult,
    SessionSetMetadataResult, SessionStopResult, WorktreeRemoveResult,
};

use crate::providers;
use crate::{
    GitHubProviderScope, GitHubPullRequestStatusKey, HostEvent, HostId, HostSnapshot,
    PromptPreview, ProviderOperation, ProviderRequestId, SessionLinkProvider,
};

/// Result of async daemon/provider I/O, reduced by [`Workspace::apply`].
///
/// This enum holds only outcomes of off-thread work — host/session/project
/// results and provider fetches, several guarded by a [`ProviderRequestId`]
/// staleness check. Pure UI pokes (active panel, filter, search, selection)
/// are not events; they are typed methods on [`Workspace`] the shell calls
/// directly instead of routing through here.
///
/// [`Workspace::apply`]: crate::Workspace::apply
/// [`Workspace`]: crate::Workspace
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainEvent {
    HostConnecting {
        host_id: HostId,
    },
    HostSnapshotLoaded {
        snapshot: HostSnapshot,
    },
    HostSubscribed {
        host_id: HostId,
    },
    HostEvent {
        host_id: HostId,
        event: HostEvent,
    },
    HostDisconnected {
        host_id: HostId,
        error: String,
    },
    HostUnreachable {
        host_id: HostId,
        error: String,
    },
    SessionCreated {
        host_id: HostId,
        session: SessionInfo,
    },
    SessionInspected {
        host_id: HostId,
        session: SessionInfo,
    },
    SessionResumed {
        host_id: HostId,
        result: SessionResumeResult,
    },
    SessionStopCompleted {
        host_id: HostId,
        session_id: SessionId,
        result: SessionStopResult,
    },
    SessionRemoveCompleted {
        host_id: HostId,
        session_id: SessionId,
        result: SessionRemoveResult,
    },
    SessionMetadataUpdated {
        host_id: HostId,
        result: SessionSetMetadataResult,
    },
    SessionRenamed {
        host_id: HostId,
        result: SessionRenameResult,
    },
    ProjectListLoaded {
        host_id: HostId,
        projects: Vec<ProjectInfo>,
    },
    ProjectAdded {
        host_id: HostId,
        project: ProjectInfo,
    },
    ProjectShown {
        host_id: HostId,
        result: ProjectShowResult,
    },
    ProjectRenamed {
        host_id: HostId,
        project: ProjectInfo,
    },
    ProjectRemoved {
        host_id: HostId,
        reference: String,
        result: ProjectRemoveResult,
    },
    WorktreeRemoved {
        host_id: HostId,
        project_id: String,
        path: PathBuf,
        result: WorktreeRemoveResult,
    },
    ProjectActionsLoaded {
        host_id: HostId,
        reference: String,
        result: ProjectActionsResult,
    },
    ProjectPromptResolved {
        host_id: HostId,
        prompt: ProjectPromptResult,
    },
    ProjectActionResolved {
        host_id: HostId,
        action: ProjectActionResult,
    },
    PromptPreviewRendered {
        host_id: HostId,
        preview: PromptPreview,
    },
    LinearProviderIssuesLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        filter_name: Option<String>,
        search: String,
        issues: Vec<providers::linear::LinearIssue>,
    },
    GitHubProviderPullRequestsLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        scope: GitHubProviderScope,
        pull_requests: Vec<providers::github::GitHubPullRequest>,
    },
    GitHubProviderIssuesLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        scope: GitHubProviderScope,
        issues: Vec<providers::github::GitHubIssue>,
    },
    GitHubProviderPullRequestStatusLoaded {
        host_id: HostId,
        request_id: ProviderRequestId,
        status_key: GitHubPullRequestStatusKey,
        status: providers::github::PullRequestStatus,
    },
    ProviderOperationFailed {
        host_id: HostId,
        provider: SessionLinkProvider,
        operation: ProviderOperation,
        request_id: Option<ProviderRequestId>,
        error: String,
    },
    HostOperationFailed {
        host_id: HostId,
        error: String,
    },
    NotificationUpdateCompleted {
        host_id: HostId,
        result: NotificationUpdateResult,
    },
    NotificationDeleteCompleted {
        host_id: HostId,
        result: NotificationDeleteResult,
    },
}
