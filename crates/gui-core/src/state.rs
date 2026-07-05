//! Headless workspace state machine and derived views for `gui-core`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use protocol::{
    AgentActivity, Event, NotificationId, NotificationKind, NotificationRecord,
    NotificationSeverity, NotificationStatus, ProjectActionResult, ProjectActionsResult,
    ProjectInfo, ProjectPromptResult, ProjectShowResult, SessionId, SessionInfo, SessionState,
    StateSource,
};

use crate::providers;
use crate::{DomainEvent, HealthSummary, HostId, PromptPreview, Selection, SessionLinkProvider};

/// Prompt/action browse and preview state for one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptState {
    pub actions_by_project: BTreeMap<String, ProjectActionsResult>,
    pub resolved_prompt: Option<ProjectPromptResult>,
    pub resolved_action: Option<ProjectActionResult>,
    pub preview: Option<PromptPreview>,
}

/// Active provider browser panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProviderPanel {
    /// Linear issues.
    #[default]
    Linear,
    /// GitHub issues and pull requests.
    GitHub,
}

/// Provider browser state owned by gui-core.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderState {
    pub active_panel: ProviderPanel,
    pub linear: LinearProviderState,
    pub github: GitHubProviderState,
}

/// Monotonic id for one provider request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderRequestId(u64);

impl ProviderRequestId {
    /// Borrow the numeric request id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Provider operation used to reject stale async completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperation {
    /// Linear assigned issue fetch.
    LinearIssues,
    /// GitHub pull request list fetch.
    GitHubPullRequests,
    /// GitHub issue list fetch.
    GitHubIssues,
    /// GitHub PR status fetch.
    GitHubPullRequestStatus,
    /// Provider launch action.
    Launch,
}

/// Linear provider browser state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinearProviderState {
    /// Name of the picked predefined filter; `None` until one is chosen.
    pub selected_filter: Option<String>,
    pub search: String,
    pub issues: Vec<providers::linear::LinearIssue>,
    pub selected_issue_id: Option<String>,
    pub active_request: Option<ProviderRequestId>,
    pub last_error: Option<String>,
}

/// GitHub provider browser state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitHubProviderState {
    pub scope: Option<GitHubProviderScope>,
    /// Name of the picked predefined pull request filter; `None` until chosen.
    pub selected_filter: Option<String>,
    pub search: String,
    pub pull_requests: Vec<providers::github::GitHubPullRequest>,
    pub issues: Vec<providers::github::GitHubIssue>,
    pub selected_pull_request: Option<u64>,
    pub selected_issue: Option<u64>,
    pub pull_requests_request: Option<ProviderRequestId>,
    pub issues_request: Option<ProviderRequestId>,
    pub pull_request_status_request: Option<ProviderRequestId>,
    pub pull_request_statuses:
        BTreeMap<GitHubPullRequestStatusKey, providers::github::PullRequestStatus>,
    pub last_error: Option<String>,
}

/// GitHub repository scope for provider data loaded through `gh`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitHubProviderScope {
    pub project_id: String,
    pub repo_root: PathBuf,
}

impl GitHubProviderScope {
    /// Construct a GitHub provider scope from the selected project identity.
    #[must_use]
    pub fn new(project_id: impl Into<String>, repo_root: impl Into<PathBuf>) -> Self {
        Self {
            project_id: project_id.into(),
            repo_root: repo_root.into(),
        }
    }

    /// Construct a GitHub provider scope from a daemon project record.
    #[must_use]
    pub fn from_project(project: &ProjectInfo) -> Self {
        Self::new(project.id.clone(), project.repo_root.clone())
    }
}

/// Cache key for GitHub PR status loaded for a specific repository scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitHubPullRequestStatusKey {
    pub scope: GitHubProviderScope,
    pub url: String,
}

impl GitHubPullRequestStatusKey {
    /// Construct a PR status cache key.
    #[must_use]
    pub fn new(scope: GitHubProviderScope, url: impl Into<String>) -> Self {
        Self {
            scope,
            url: url.into(),
        }
    }
}

/// Parsed `agent_state` event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStateEvent {
    pub session_id: SessionId,
    pub activity: AgentActivity,
    pub source: StateSource,
    pub raw: Event,
}

/// Protocol events surfaced by a host subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    AgentState(AgentStateEvent),
    SessionCreated(SessionInfo),
    SessionUpdated(SessionInfo),
    SessionStopped(SessionInfo),
    SessionRemoved(SessionInfo),
    /// A durable notification record was created on the host.
    NotificationCreated(NotificationRecord),
    /// A durable notification record changed lifecycle status or content.
    NotificationUpdated(NotificationRecord),
    /// A durable notification record was hard-deleted on the host.
    NotificationDeleted(NotificationId),
    Other(Event),
}

/// Per-host connection state for the headless workspace model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
    Unreachable,
}

/// OS notification requested by core state transitions.
///
/// Raised from durable `notification_created` events whose severity warrants an
/// immediate desktop notification (see [`notification_raises_intent`]). The
/// monotonic `id` lets the shell consume new intents with a cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationIntent {
    pub id: u64,
    pub host_id: HostId,
    /// Durable notification that raised this intent, so the shell can open its
    /// inbox detail on click.
    pub notification_id: Option<NotificationId>,
    /// Linked session, when the source notification is bound to one.
    pub session_id: Option<SessionId>,
    pub title: String,
    pub body: String,
}

/// In-app notification requested by core state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub host_id: HostId,
    pub session_id: SessionId,
    pub message: String,
}

/// Filter for the inbox notification-list selectors.
///
/// Every `Some` field must match; `None` fields do not constrain the result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationFilter {
    pub host_id: Option<HostId>,
    pub status: Option<NotificationStatus>,
    pub severity: Option<NotificationSeverity>,
    pub kind: Option<NotificationKind>,
    pub provider: Option<String>,
}

impl NotificationFilter {
    /// Whether `record` passes the non-host constraints of this filter.
    ///
    /// The host constraint is applied by the caller, which already iterates
    /// hosts and can skip whole hosts without inspecting their records.
    fn matches(&self, record: &NotificationRecord) -> bool {
        if self.status.is_some_and(|status| status != record.status) {
            return false;
        }
        if self
            .severity
            .is_some_and(|severity| severity != record.severity)
        {
            return false;
        }
        if self.kind.is_some_and(|kind| kind != record.kind) {
            return false;
        }
        if self
            .provider
            .as_ref()
            .is_some_and(|provider| provider != &record.source.provider)
        {
            return false;
        }
        true
    }
}

/// One notification row for the inbox list, tagged with its owning host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationRow {
    pub host_id: HostId,
    pub record: NotificationRecord,
}

/// Coarse inbox modal scope, replacing the five per-axis filter-chip rows.
///
/// Unlike [`NotificationFilter`] (an AND of independent axes), `NeedsAction`
/// is an OR over status and severity, so it is modeled as its own selector
/// rather than as another `NotificationFilter` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationScope {
    /// Unread notifications, plus read ones severe enough to still demand
    /// attention (`ActionRequired`/`Error`). The inbox modal's default view.
    #[default]
    NeedsAction,
    /// Every non-deleted notification, regardless of lifecycle status.
    All,
    /// Only archived notifications.
    Archived,
}

impl NotificationScope {
    /// Whether `record` is visible under this scope.
    #[must_use]
    pub fn matches(self, record: &NotificationRecord) -> bool {
        match self {
            Self::NeedsAction => notification_needs_action(record),
            Self::All => true,
            Self::Archived => record.status == NotificationStatus::Archived,
        }
    }
}

/// Whether `record` belongs in the inbox modal's default "Needs action" scope.
///
/// True for anything still unread, or anything severe enough
/// (`ActionRequired`/`Error`) to keep demanding attention even once read.
/// Archiving a record always drops it out of scope, since the operator has
/// already dealt with it.
#[must_use]
fn notification_needs_action(record: &NotificationRecord) -> bool {
    record.status != NotificationStatus::Archived
        && (record.status == NotificationStatus::Unread || notification_raises_intent(record))
}

/// Sort tier for the inbox modal: unresolved agent/approval prompts pinned to
/// the top, then unread, then read; recency breaks ties within a tier.
fn inbox_row_tier(record: &NotificationRecord) -> u8 {
    if matches!(
        record.kind,
        NotificationKind::AgentBlocked | NotificationKind::ApprovalRequired
    ) {
        0
    } else if record.status == NotificationStatus::Unread {
        1
    } else {
        2
    }
}

/// GUI-facing state for one daemon host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostView {
    pub conn: ConnState,
    pub health: Option<HealthSummary>,
    pub sessions: BTreeMap<String, SessionInfo>,
    pub projects: BTreeMap<String, ProjectInfo>,
    pub project_details: BTreeMap<String, ProjectShowResult>,
    pub prompt: PromptState,
    pub provider: ProviderState,
    /// Durable notification records for this host, keyed by notification id.
    pub notifications: BTreeMap<String, NotificationRecord>,
    pub last_agent_state: Option<AgentStateEvent>,
    pub last_error: Option<String>,
}

impl HostView {
    fn connecting() -> Self {
        Self {
            conn: ConnState::Connecting,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            project_details: BTreeMap::new(),
            prompt: PromptState::default(),
            provider: ProviderState::default(),
            notifications: BTreeMap::new(),
            last_agent_state: None,
            last_error: None,
        }
    }
}

/// Headless workspace model owned by `gui-core`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub hosts: BTreeMap<HostId, HostView>,
    pub selection: Option<Selection>,
    pub notification_intents: Vec<NotificationIntent>,
    pub toasts: Vec<Toast>,
    next_intent_id: u64,
    next_provider_request_id: u64,
}

impl Workspace {
    fn next_provider_request_id(&mut self) -> ProviderRequestId {
        self.next_provider_request_id = self.next_provider_request_id.saturating_add(1);
        ProviderRequestId(self.next_provider_request_id)
    }

    /// Mark a Linear issue fetch as the current request for `host_id`.
    pub fn begin_linear_issues_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.linear.active_request = Some(request_id);
        host.provider.linear.last_error = None;
        request_id
    }

    /// Mark a GitHub pull request list fetch as the current request for `host_id`.
    pub fn begin_github_pull_requests_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.pull_requests_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Mark a GitHub issue list fetch as the current request for `host_id`.
    pub fn begin_github_issues_request(&mut self, host_id: HostId) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.issues_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Mark a GitHub pull request status fetch as the current request for `host_id`.
    pub fn begin_github_pull_request_status_request(
        &mut self,
        host_id: HostId,
    ) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let host = self
            .hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting);
        host.provider.github.pull_request_status_request = Some(request_id);
        host.provider.github.last_error = None;
        request_id
    }

    /// Invalidate pending GitHub provider requests for `host_id`.
    pub fn invalidate_github_provider_requests(&mut self, host_id: &HostId) {
        if let Some(host) = self.hosts.get_mut(host_id) {
            host.provider.github.pull_requests_request = None;
            host.provider.github.issues_request = None;
            host.provider.github.pull_request_status_request = None;
        }
    }

    fn selected_github_scope(&self, host_id: &HostId) -> Option<GitHubProviderScope> {
        match self.selection.as_ref()? {
            Selection::Project {
                host_id: selected_host,
                project_id,
            } if selected_host == host_id => self
                .hosts
                .get(host_id)?
                .projects
                .get(project_id)
                .map(GitHubProviderScope::from_project),
            Selection::Session {
                host_id: selected_host,
                session_id,
            } if selected_host == host_id => {
                let host = self.hosts.get(host_id)?;
                let project_id = host.sessions.get(&session_id.0)?.project_id.as_ref()?;
                host.projects
                    .get(project_id)
                    .map(GitHubProviderScope::from_project)
            }
            _ => None,
        }
    }

    /// Reduce one domain event (async daemon/provider I/O result) into state.
    #[expect(
        clippy::too_many_lines,
        reason = "workspace updates are centralized so GUI transitions stay deterministic and testable"
    )]
    pub fn apply(&mut self, event: DomainEvent) {
        match event {
            DomainEvent::HostConnecting { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connecting;
                        host.last_error = None;
                    })
                    .or_insert_with(HostView::connecting);
            }
            DomainEvent::HostSnapshotLoaded { snapshot } => {
                let host_id = snapshot.host_id.clone();
                let sessions = snapshot
                    .sessions
                    .iter()
                    .cloned()
                    .map(|session| (session.id.0.clone(), session))
                    .collect();
                let projects = snapshot
                    .projects
                    .iter()
                    .cloned()
                    .map(|project| (project.id.clone(), project))
                    .collect();
                let previous_details = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(BTreeMap::new, |host| host.project_details.clone());
                let previous_prompt = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(PromptState::default, |host| host.prompt.clone());
                let previous_provider = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(ProviderState::default, |host| host.provider.clone());
                // Notifications reconcile by merge, not wholesale replacement: the
                // seed query returns a bounded recent window, so previously
                // received records are preserved and missed records are folded in
                // (deduped by id). Seeding never raises OS intents.
                let mut notifications = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(BTreeMap::new, |host| host.notifications.clone());
                for record in snapshot.notifications {
                    upsert_notification(&mut notifications, record);
                }
                self.hosts.insert(
                    snapshot.host_id,
                    HostView {
                        conn: ConnState::Connected,
                        health: Some(snapshot.health),
                        sessions,
                        projects,
                        project_details: previous_details,
                        prompt: previous_prompt,
                        provider: previous_provider,
                        notifications,
                        last_agent_state: None,
                        last_error: snapshot.project_error,
                    },
                );
            }
            DomainEvent::HostSubscribed { host_id } => {
                self.hosts
                    .entry(host_id)
                    .and_modify(|host| {
                        host.conn = ConnState::Connected;
                        host.last_error = None;
                    })
                    .or_insert_with(|| HostView {
                        conn: ConnState::Connected,
                        ..HostView::connecting()
                    });
            }
            DomainEvent::HostEvent { host_id, event } => {
                let Some(host) = self.hosts.get_mut(&host_id) else {
                    trace_ignored_unknown_host(&host_id, "host event");
                    return;
                };
                host.conn = ConnState::Connected;
                host.last_error = None;
                apply_host_event(
                    host,
                    &host_id,
                    event,
                    &mut self.notification_intents,
                    &mut self.toasts,
                    &mut self.next_intent_id,
                );
            }
            DomainEvent::HostDisconnected { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host disconnected") else {
                    return;
                };
                host.conn = ConnState::Disconnected;
                host.last_error = Some(error);
            }
            DomainEvent::HostUnreachable { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host unreachable") else {
                    return;
                };
                host.conn = ConnState::Unreachable;
                host.last_error = Some(error);
            }
            DomainEvent::SessionCreated { host_id, session }
            | DomainEvent::SessionInspected { host_id, session } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session result") else {
                    return;
                };
                host.sessions.insert(session.id.0.clone(), session);
            }
            DomainEvent::SessionResumed { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session resume result") else {
                    return;
                };
                let session = result.session;
                host.sessions.insert(session.id.0.clone(), session);
            }
            DomainEvent::SessionStopCompleted {
                host_id,
                session_id,
                result,
            } => {
                if !result.stopped {
                    return;
                }
                let Some(host) = self.host_mut_if_known(&host_id, "session stop result") else {
                    return;
                };
                if let Some(session) = host.sessions.get_mut(&session_id.0) {
                    session.state = SessionState::Stopped;
                    session.activity = None;
                }
            }
            DomainEvent::SessionRemoveCompleted {
                host_id,
                session_id,
                result,
            } => {
                if !result.removed {
                    return;
                }
                let Some(host) = self.host_mut_if_known(&host_id, "session remove result") else {
                    return;
                };
                host.sessions.remove(&session_id.0);
            }
            DomainEvent::SessionMetadataUpdated { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session metadata result") else {
                    return;
                };
                host.sessions
                    .insert(result.session.id.0.clone(), result.session);
            }
            DomainEvent::SessionRenamed { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session renamed result") else {
                    return;
                };
                host.sessions
                    .insert(result.session.id.0.clone(), result.session);
            }
            DomainEvent::ProjectListLoaded { host_id, projects } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project list result") else {
                    return;
                };
                host.projects = projects
                    .iter()
                    .cloned()
                    .map(|project| (project.id.clone(), project))
                    .collect();
                host.project_details
                    .retain(|id, _details| host.projects.contains_key(id));
            }
            DomainEvent::ProjectAdded { host_id, project }
            | DomainEvent::ProjectRenamed { host_id, project } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project change result") else {
                    return;
                };
                host.projects.insert(project.id.clone(), project);
            }
            DomainEvent::ProjectShown { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project shown result") else {
                    return;
                };
                host.projects
                    .insert(result.project.id.clone(), result.project.clone());
                host.project_details
                    .insert(result.project.id.clone(), result);
            }
            DomainEvent::ProjectRemoved {
                host_id,
                reference,
                result,
            } => {
                if !result.removed {
                    return;
                }
                if let Some(host) = self.hosts.get_mut(&host_id) {
                    let removed_ids = host
                        .projects
                        .iter()
                        .filter(|(id, project)| *id == &reference || project.label == reference)
                        .map(|(id, _project)| id.clone())
                        .collect::<Vec<_>>();
                    for id in removed_ids {
                        host.projects.remove(&id);
                        host.project_details.remove(&id);
                    }
                }
            }
            DomainEvent::WorktreeRemoved {
                host_id,
                project_id,
                path,
                result,
            } => {
                if !result.removed {
                    return;
                }
                // Drop the removed worktree from the cached project detail so the
                // row disappears immediately, without waiting for a refresh.
                if let Some(details) = self
                    .hosts
                    .get_mut(&host_id)
                    .and_then(|host| host.project_details.get_mut(&project_id))
                {
                    details.worktrees.retain(|worktree| worktree.path != path);
                }
            }
            DomainEvent::ProjectActionsLoaded {
                host_id,
                reference,
                result,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project actions result") else {
                    return;
                };
                host.prompt.actions_by_project.insert(reference, result);
                host.last_error = None;
            }
            DomainEvent::ProjectPromptResolved { host_id, prompt } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project prompt result") else {
                    return;
                };
                host.prompt.resolved_prompt = Some(prompt);
                host.prompt.preview = None;
                host.last_error = None;
            }
            DomainEvent::ProjectActionResolved { host_id, action } => {
                let Some(host) = self.host_mut_if_known(&host_id, "project action result") else {
                    return;
                };
                host.prompt.resolved_action = Some(action);
                host.prompt.preview = None;
                host.last_error = None;
            }
            DomainEvent::PromptPreviewRendered { host_id, preview } => {
                let Some(host) = self.host_mut_if_known(&host_id, "prompt preview result") else {
                    return;
                };
                host.prompt.preview = Some(preview);
                host.last_error = None;
            }
            DomainEvent::LinearProviderIssuesLoaded {
                host_id,
                request_id,
                filter_name,
                search,
                issues,
            } => {
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "linear issues result") else {
                    return;
                };
                if host.provider.linear.active_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                if host.provider.linear.selected_filter != filter_name
                    || host.provider.linear.search != search
                {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        request_id,
                        "filter_changed",
                    );
                    return;
                }
                host.provider.linear.active_request = None;
                host.provider.linear.issues = issues;
                host.provider.linear.selected_issue_id = None;
                host.provider.linear.last_error = None;
            }
            DomainEvent::GitHubProviderPullRequestsLoaded {
                host_id,
                request_id,
                scope,
                pull_requests,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "github pull requests result")
                else {
                    return;
                };
                if host.provider.github.pull_requests_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.pull_requests_request = None;
                if host.provider.github.scope.as_ref() != Some(&scope) {
                    host.provider.github.issues.clear();
                    host.provider.github.selected_issue = None;
                }
                host.provider.github.scope = Some(scope);
                host.provider.github.pull_requests = pull_requests;
                host.provider.github.selected_pull_request = None;
                host.provider.github.last_error = None;
            }
            DomainEvent::GitHubProviderIssuesLoaded {
                host_id,
                request_id,
                scope,
                issues,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubIssues,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) = self.host_mut_if_known(&host_id, "github issues result") else {
                    return;
                };
                if host.provider.github.issues_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubIssues,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.issues_request = None;
                if host.provider.github.scope.as_ref() != Some(&scope) {
                    host.provider.github.pull_requests.clear();
                    host.provider.github.selected_pull_request = None;
                }
                host.provider.github.scope = Some(scope);
                host.provider.github.issues = issues;
                host.provider.github.selected_issue = None;
                host.provider.github.last_error = None;
            }
            DomainEvent::GitHubProviderPullRequestStatusLoaded {
                host_id,
                request_id,
                status_key,
                status,
            } => {
                if self
                    .selected_github_scope(&host_id)
                    .as_ref()
                    .is_some_and(|current| current != &status_key.scope)
                {
                    trace_ignored_provider_result(
                        &host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequestStatus,
                        request_id,
                        "selection_changed",
                    );
                    return;
                }
                let trace_host_id = host_id.clone();
                let Some(host) =
                    self.host_mut_if_known(&host_id, "github pull request status result")
                else {
                    return;
                };
                if host.provider.github.pull_request_status_request != Some(request_id) {
                    trace_ignored_provider_result(
                        &trace_host_id,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequestStatus,
                        request_id,
                        "stale_request",
                    );
                    return;
                }
                host.provider.github.pull_request_status_request = None;
                host.provider
                    .github
                    .pull_request_statuses
                    .insert(status_key, status);
                host.provider.github.last_error = None;
            }
            DomainEvent::ProviderOperationFailed {
                host_id,
                provider,
                operation,
                request_id,
                error,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "provider operation failure")
                else {
                    return;
                };
                if !apply_provider_request_failure(&host_id, host, provider, operation, request_id)
                {
                    return;
                }
                match provider {
                    SessionLinkProvider::Linear => host.provider.linear.last_error = Some(error),
                    SessionLinkProvider::GitHub => host.provider.github.last_error = Some(error),
                }
            }
            DomainEvent::HostOperationFailed { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "host operation failure") else {
                    return;
                };
                host.last_error = Some(error);
            }
            DomainEvent::NotificationUpdateCompleted { host_id, result } => {
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                upsert_notification(&mut host.notifications, result.record);
            }
            DomainEvent::NotificationDeleteCompleted { host_id, result } => {
                if !result.deleted {
                    return;
                }
                let host = self
                    .hosts
                    .entry(host_id)
                    .or_insert_with(HostView::connecting);
                remove_notification(&mut host.notifications, &result.id);
            }
        }
    }

    fn host_mut_if_known(
        &mut self,
        host_id: &HostId,
        reason: &'static str,
    ) -> Option<&mut HostView> {
        let Some(host) = self.hosts.get_mut(host_id) else {
            trace_ignored_unknown_host(host_id, reason);
            return None;
        };
        Some(host)
    }

    fn host_for_ui(&mut self, host_id: HostId) -> &mut HostView {
        self.hosts
            .entry(host_id)
            .or_insert_with(HostView::connecting)
    }

    /// Switch the active provider browser panel for `host_id`.
    pub fn set_active_panel(&mut self, host_id: HostId, panel: ProviderPanel) {
        let host = self.host_for_ui(host_id);
        host.provider.active_panel = panel;
    }

    /// Pick the named Linear filter for `host_id`, dropping any in-flight
    /// issues request so a stale response cannot land under the new filter.
    pub fn select_linear_filter(&mut self, host_id: HostId, name: String) {
        let host = self.host_for_ui(host_id);
        host.provider.linear.selected_filter = Some(name);
        host.provider.linear.active_request = None;
    }

    /// Update the Linear search box text for `host_id`, dropping any
    /// in-flight issues request so a stale response cannot land under the new
    /// search.
    pub fn set_linear_search(&mut self, host_id: HostId, value: String) {
        let host = self.host_for_ui(host_id);
        host.provider.linear.search = value;
        host.provider.linear.active_request = None;
    }

    /// Select a Linear issue in the provider browser for `host_id`.
    pub fn select_linear_issue(&mut self, host_id: HostId, issue_id: String) {
        let host = self.host_for_ui(host_id);
        host.provider.linear.selected_issue_id = Some(issue_id);
    }

    /// Pick the named GitHub pull request filter for `host_id`, dropping any
    /// in-flight pull requests request so a stale response cannot land under
    /// the new filter.
    pub fn select_github_filter(&mut self, host_id: HostId, name: String) {
        let host = self.host_for_ui(host_id);
        host.provider.github.selected_filter = Some(name);
        host.provider.github.pull_requests_request = None;
    }

    /// Update the GitHub search box text for `host_id`.
    pub fn set_github_search(&mut self, host_id: HostId, value: String) {
        let host = self.host_for_ui(host_id);
        host.provider.github.search = value;
    }

    /// Select a GitHub pull request in the provider browser for `host_id`.
    pub fn select_github_pull_request(&mut self, host_id: HostId, number: u64) {
        let host = self.host_for_ui(host_id);
        host.provider.github.selected_pull_request = Some(number);
    }

    /// Select a GitHub issue in the provider browser for `host_id`.
    pub fn select_github_issue(&mut self, host_id: HostId, number: u64) {
        let host = self.host_for_ui(host_id);
        host.provider.github.selected_issue = Some(number);
    }

    /// Select a session in the detail pane.
    pub fn select_session(&mut self, host_id: HostId, session_id: SessionId) {
        self.invalidate_github_provider_requests(&host_id);
        self.selection = Some(Selection::Session {
            host_id,
            session_id,
        });
    }

    /// Select a project in the detail pane.
    pub fn select_project(&mut self, host_id: HostId, project_id: String) {
        self.invalidate_github_provider_requests(&host_id);
        self.selection = Some(Selection::Project {
            host_id,
            project_id,
        });
    }

    /// Selects the session linked to a notification, when that session is
    /// still live.
    ///
    /// Returns `true` and updates [`Workspace::selection`] when a live linked
    /// session was found; returns `false` and leaves the selection untouched
    /// otherwise. The inbox modal is the only route to notification detail, so
    /// there is no selection-based fallback to fall back to here — callers
    /// (the modal's "Open session" action) only invoke this once the session
    /// is known live from the same [`HostView`] data.
    pub fn select_notification_session(
        &mut self,
        host_id: &HostId,
        notification_id: &NotificationId,
    ) -> bool {
        let linked_session = self.hosts.get(host_id).and_then(|host| {
            let record = host.notifications.get(&notification_id.0)?;
            let session_id = record.session_id.as_ref()?;
            host.sessions
                .contains_key(&session_id.0)
                .then(|| session_id.clone())
        });
        let Some(session_id) = linked_session else {
            return false;
        };
        self.invalidate_github_provider_requests(host_id);
        self.selection = Some(Selection::Session {
            host_id: host_id.clone(),
            session_id,
        });
        true
    }

    /// Total unread notifications across all hosts.
    #[must_use]
    pub fn unread_notification_count(&self) -> usize {
        self.hosts.values().map(host_unread_count).sum()
    }

    /// Unread notifications for a single host.
    #[must_use]
    pub fn host_unread_notification_count(&self, host_id: &HostId) -> usize {
        self.hosts.get(host_id).map_or(0, host_unread_count)
    }

    /// Notifications matching `filter`, newest first.
    ///
    /// Ordering is by the stable `(created_at desc, id)` identity, never by the
    /// volatile lifecycle status, so marking a record read does not reshuffle
    /// rows under the operator's cursor.
    #[must_use]
    pub fn notifications(&self, filter: &NotificationFilter) -> Vec<NotificationRow> {
        let mut rows = Vec::new();
        for (host_id, host) in &self.hosts {
            if filter
                .host_id
                .as_ref()
                .is_some_and(|wanted| wanted != host_id)
            {
                continue;
            }
            for record in host.notifications.values() {
                if filter.matches(record) {
                    rows.push(NotificationRow {
                        host_id: host_id.clone(),
                        record: record.clone(),
                    });
                }
            }
        }
        rows.sort_by(|left, right| {
            right
                .record
                .created_at
                .cmp(&left.record.created_at)
                .then_with(|| left.record.id.0.cmp(&right.record.id.0))
        });
        rows
    }

    /// Notification rows for the inbox modal: `scope` narrows by
    /// lifecycle/severity, `filter` narrows by host as with
    /// [`Workspace::notifications`], and rows are sorted for triage —
    /// unresolved agent/approval prompts first, then unread by recency, then
    /// read.
    #[must_use]
    pub fn inbox_rows(
        &self,
        scope: NotificationScope,
        filter: &NotificationFilter,
    ) -> Vec<NotificationRow> {
        let mut rows: Vec<NotificationRow> = self
            .notifications(filter)
            .into_iter()
            .filter(|row| scope.matches(&row.record))
            .collect();
        rows.sort_by(|left, right| {
            inbox_row_tier(&left.record)
                .cmp(&inbox_row_tier(&right.record))
                .then_with(|| right.record.created_at.cmp(&left.record.created_at))
                .then_with(|| left.record.id.0.cmp(&right.record.id.0))
        });
        rows
    }

    /// Look up one notification record by host and id.
    #[must_use]
    pub fn notification(
        &self,
        host_id: &HostId,
        id: &NotificationId,
    ) -> Option<&NotificationRecord> {
        self.hosts.get(host_id)?.notifications.get(&id.0)
    }

    /// Build the agents monitor model from current host state.
    #[must_use]
    pub fn agent_monitor(&self) -> AgentMonitor {
        let mut monitor = AgentMonitor::default();
        for (host_id, host) in &self.hosts {
            for session in host.sessions.values() {
                match session.activity {
                    Some(AgentActivity::Blocked) => monitor.blocked += 1,
                    Some(AgentActivity::Working) => monitor.working += 1,
                    Some(AgentActivity::Idle) => monitor.idle += 1,
                    None => monitor.unknown += 1,
                }
                monitor.sessions.push(AgentRow {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                    name: session.name.clone(),
                    project_id: session.project_id.clone(),
                    project_label: session.project_label.clone(),
                    agent: session.agent.clone(),
                    activity: session.activity,
                    state: session.state.as_str().to_owned(),
                    branch: session.branch.clone(),
                });
            }
        }
        // Order by the stable (host, session) identity only — NEVER by the
        // volatile activity. Activity flips as agents work/block/idle on every
        // poll, and sorting on it would reshuffle rows under the operator's
        // cursor, making the list impossible to click. The activity is conveyed
        // by the per-row dot and the header counts instead.
        monitor.sessions.sort_by(|left, right| {
            left.host_id
                .cmp(&right.host_id)
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        monitor
    }
}

fn apply_provider_request_failure(
    host_id: &HostId,
    host: &mut HostView,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: Option<ProviderRequestId>,
) -> bool {
    let Some(request_id) = request_id else {
        return true;
    };
    match (provider, operation) {
        (SessionLinkProvider::Linear, ProviderOperation::LinearIssues) => {
            if host.provider.linear.active_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.linear.active_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequests) => {
            if host.provider.github.pull_requests_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.pull_requests_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubIssues) => {
            if host.provider.github.issues_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.issues_request = None;
            true
        }
        (SessionLinkProvider::GitHub, ProviderOperation::GitHubPullRequestStatus) => {
            if host.provider.github.pull_request_status_request != Some(request_id) {
                trace_ignored_provider_failure(
                    host_id,
                    provider,
                    operation,
                    request_id,
                    "stale_request",
                );
                return false;
            }
            host.provider.github.pull_request_status_request = None;
            true
        }
        (_, ProviderOperation::Launch) => true,
        _ => false,
    }
}

fn trace_ignored_unknown_host(host_id: &HostId, reason: &'static str) {
    tracing::event!(
        name: "gui.host.result.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        reason,
        "ignoring result for unknown host"
    );
}

fn trace_ignored_provider_result(
    host_id: &HostId,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: ProviderRequestId,
    reason: &'static str,
) {
    tracing::event!(
        name: "gui.provider.result.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        provider = ?provider,
        operation = ?operation,
        request_id = request_id.get(),
        reason,
        "ignoring provider result"
    );
}

fn trace_ignored_provider_failure(
    host_id: &HostId,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: ProviderRequestId,
    reason: &'static str,
) {
    tracing::event!(
        name: "gui.provider.failure.ignored",
        tracing::Level::DEBUG,
        host_id = %host_id,
        provider = ?provider,
        operation = ?operation,
        request_id = request_id.get(),
        reason,
        "ignoring provider failure"
    );
}

/// Derived row for the flat agents monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub host_id: HostId,
    pub session_id: SessionId,
    /// Owner-set display name, or `None` to show the row by its session id.
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub project_label: Option<String>,
    pub agent: String,
    pub activity: Option<AgentActivity>,
    pub state: String,
    /// Branch checked out in the session's worktree, when bound.
    pub branch: Option<String>,
}

/// Derived agents monitor counts and rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMonitor {
    pub blocked: usize,
    pub working: usize,
    pub idle: usize,
    pub unknown: usize,
    pub sessions: Vec<AgentRow>,
}

impl AgentMonitor {
    /// Returns the `index`-th blocked session in stable `sessions` order.
    ///
    /// `index` wraps around the blocked-session count, so a caller that
    /// increments it on every call cycles through every blocked agent
    /// instead of getting stuck on the first one. Backs the GUI's `b`
    /// keyboard shortcut. Returns `None` when no agent is blocked.
    #[must_use]
    pub fn blocked_at(&self, index: usize) -> Option<(HostId, SessionId)> {
        let blocked: Vec<&AgentRow> = self
            .sessions
            .iter()
            .filter(|row| row.activity == Some(AgentActivity::Blocked))
            .collect();
        let row = *blocked.get(index.checked_rem(blocked.len())?)?;
        Some((row.host_id.clone(), row.session_id.clone()))
    }
}

fn apply_host_event(
    host: &mut HostView,
    host_id: &HostId,
    event: HostEvent,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    match event {
        HostEvent::AgentState(state) => {
            if let Some(session) = host.sessions.get_mut(&state.session_id.0) {
                session.activity = Some(state.activity);
                session.state_source = state.source;
            }
            host.last_agent_state = Some(state);
        }
        HostEvent::SessionCreated(session)
        | HostEvent::SessionUpdated(session)
        | HostEvent::SessionStopped(session) => {
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::SessionRemoved(session) => {
            host.sessions.remove(&session.id.0);
        }
        HostEvent::NotificationCreated(record) => {
            // A freshly created durable notification is the single source of OS
            // notifications: the daemon projector emits a durable `agent_blocked`
            // for a blocked session, which replaces the removed transient path.
            push_notification_effects(&record, host_id, notifications, toasts, next_intent_id);
            upsert_notification(&mut host.notifications, record);
        }
        HostEvent::NotificationUpdated(record) => {
            // Lifecycle/content changes (read, ack, archive, supersede) update the
            // stored record but never re-raise an OS intent.
            upsert_notification(&mut host.notifications, record);
        }
        HostEvent::NotificationDeleted(id) => {
            remove_notification(&mut host.notifications, &id);
        }
        HostEvent::Other(_) => {}
    }
}

/// Store or replace a notification record, dropping it when the daemon reports a
/// deleted lifecycle status so a hard-removed record cannot linger in the inbox.
fn upsert_notification(
    store: &mut BTreeMap<String, NotificationRecord>,
    record: NotificationRecord,
) {
    if record.status == NotificationStatus::Deleted {
        store.remove(&record.id.0);
    } else {
        store.insert(record.id.0.clone(), record);
    }
}

/// Remove a notification record by id.
fn remove_notification(store: &mut BTreeMap<String, NotificationRecord>, id: &NotificationId) {
    store.remove(&id.0);
}

/// Count unread notifications held by one host.
fn host_unread_count(host: &HostView) -> usize {
    host.notifications
        .values()
        .filter(|record| record.status == NotificationStatus::Unread)
        .count()
}

/// Whether a freshly created notification warrants a desktop OS notification.
///
/// Only durable action-required and error notifications interrupt the operator;
/// informational, success, and warning records land in the inbox silently.
fn notification_raises_intent(record: &NotificationRecord) -> bool {
    matches!(
        record.severity,
        NotificationSeverity::ActionRequired | NotificationSeverity::Error
    )
}

/// Append the OS intent (and, when session-linked, the in-app toast) for a newly
/// created notification that warrants an interrupt.
fn push_notification_effects(
    record: &NotificationRecord,
    host_id: &HostId,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
) {
    if !notification_raises_intent(record) {
        return;
    }
    let id = *next_intent_id;
    *next_intent_id += 1;
    notifications.push(NotificationIntent {
        id,
        host_id: host_id.clone(),
        notification_id: Some(record.id.clone()),
        session_id: record.session_id.clone(),
        title: record.title.clone(),
        body: record.body.clone(),
    });
    if let Some(session_id) = record.session_id.clone() {
        toasts.push(Toast {
            id,
            host_id: host_id.clone(),
            session_id,
            message: record.body.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::{
        event, method, HostClass, HostRecord, NotificationCreatedEvent, NotificationDeleteResult,
        NotificationDeletedEvent, NotificationSource, NotificationUpdateResult,
        NotificationUpdatedEvent, ProviderKind, SessionRemoveResult, SessionResumeResult,
    };
    use serde_json::Value;

    use crate::connection::{
        discovered_transport_host, parse_agent_state, parse_event_message, subscribe_request,
        Backoff,
    };
    use crate::link::action_prompt_provider;
    use crate::sdk::notification_seed_queries;
    use crate::{
        render_attach_command, AttachTemplateValues, ConnectionOptions, CoreError, HostSnapshot,
        DEFAULT_BACKOFF_MAX,
    };

    use super::*;

    #[test]
    fn workspace_applies_agent_state_to_known_session() {
        let mut workspace = Workspace::default();
        let session = session("s-1", None);
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "blocked",
                "source": "osc_title"
            }),
        );
        workspace.apply(DomainEvent::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(
            host.sessions
                .get("s-1")
                .and_then(|session| session.activity),
            Some(AgentActivity::Blocked)
        );
        assert_eq!(
            host.last_agent_state
                .as_ref()
                .map(|event| event.session_id.0.as_str()),
            Some("s-1")
        );
    }

    #[test]
    fn workspace_accepts_report_agent_state_source() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None)]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "working",
                "source": "report"
            }),
        );
        workspace.apply(DomainEvent::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(
            host.last_agent_state.as_ref().map(|event| event.source),
            Some(StateSource::Report)
        );
        assert_eq!(
            host.sessions
                .get("s-1")
                .and_then(|session| session.activity),
            Some(AgentActivity::Working)
        );
    }

    #[test]
    fn session_remove_completed_drops_the_session() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None), session("s-2", None)]),
        });

        workspace.apply(DomainEvent::SessionRemoveCompleted {
            host_id: HostId::new("local"),
            session_id: SessionId("s-1".to_owned()),
            result: SessionRemoveResult {
                removed: true,
                stopped: true,
            },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(
            !host.sessions.contains_key("s-1"),
            "removed session is gone"
        );
        assert!(host.sessions.contains_key("s-2"), "other session remains");
    }

    #[test]
    fn agent_monitor_orders_by_identity_not_activity_and_carries_name() {
        let mut named = session("s-2", Some(AgentActivity::Working));
        named.name = Some("triage build".to_owned());
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![
                    session("s-3", Some(AgentActivity::Idle)),
                    named,
                    session("s-1", Some(AgentActivity::Blocked)),
                ],
            ),
        });

        let monitor = workspace.agent_monitor();
        let ids: Vec<&str> = monitor
            .sessions
            .iter()
            .map(|row| row.session_id.0.as_str())
            .collect();
        // Stable (host, id) order regardless of activity, so a row never jumps
        // out from under the cursor when its activity flips.
        assert_eq!(ids, ["s-1", "s-2", "s-3"]);
        assert_eq!(monitor.blocked, 1);
        assert_eq!(monitor.working, 1);
        assert_eq!(monitor.idle, 1);
        assert_eq!(monitor.sessions[1].name.as_deref(), Some("triage build"));
    }

    #[test]
    fn blocked_at_cycles_through_blocked_sessions_only() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![
                    session("s-1", Some(AgentActivity::Blocked)),
                    session("s-2", Some(AgentActivity::Working)),
                    session("s-3", Some(AgentActivity::Blocked)),
                ],
            ),
        });
        let monitor = workspace.agent_monitor();
        let host_id = HostId::new("local");

        assert_eq!(
            monitor.blocked_at(0),
            Some((host_id.clone(), SessionId("s-1".to_owned())))
        );
        assert_eq!(
            monitor.blocked_at(1),
            Some((host_id.clone(), SessionId("s-3".to_owned())))
        );
        // Wraps back to the first blocked session rather than stopping.
        assert_eq!(
            monitor.blocked_at(2),
            Some((host_id, SessionId("s-1".to_owned())))
        );
    }

    #[test]
    fn blocked_at_is_none_without_any_blocked_session() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Working))]),
        });

        assert_eq!(workspace.agent_monitor().blocked_at(0), None);
    }

    #[test]
    fn session_remove_completed_keeps_session_when_not_removed() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None)]),
        });

        // A `removed: false` result (a concurrent remove won the race) must not
        // touch the local view.
        workspace.apply(DomainEvent::SessionRemoveCompleted {
            host_id: HostId::new("local"),
            session_id: SessionId("s-1".to_owned()),
            result: SessionRemoveResult {
                removed: false,
                stopped: false,
            },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.sessions.contains_key("s-1"));
    }

    #[test]
    fn session_resumed_replaces_terminal_snapshot() {
        let mut stopped = session("s-1", None);
        stopped.state = SessionState::Stopped;
        let mut resumed = stopped.clone();
        resumed.state = SessionState::Running;
        resumed.pid = 99;

        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![stopped]),
        });

        workspace.apply(DomainEvent::SessionResumed {
            host_id: HostId::new("local"),
            result: SessionResumeResult { session: resumed },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        let session = host.sessions.get("s-1").expect("resumed session");
        assert_eq!(session.state, SessionState::Running);
        assert_eq!(session.pid, 99);
    }

    #[test]
    fn session_removed_event_drops_the_session() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", None), session("s-2", None)]),
        });

        workspace.apply(DomainEvent::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::SessionRemoved(session("s-1", None)),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(
            !host.sessions.contains_key("s-1"),
            "removed session is gone"
        );
        assert!(host.sessions.contains_key("s-2"), "other session remains");
    }

    #[test]
    fn session_removed_wire_event_parses_to_host_event() {
        let raw = Event::new(
            event::SESSION_REMOVED,
            serde_json::json!({ "session": session("s-1", None) }),
        );
        let line = serde_json::to_string(&raw).expect("serialize event");

        let message =
            parse_event_message(&HostId::new("local"), &line).expect("parse session_removed");

        match message {
            DomainEvent::HostEvent {
                event: HostEvent::SessionRemoved(info),
                ..
            } => assert_eq!(info.id, SessionId("s-1".to_owned())),
            other => panic!("expected SessionRemoved host event, got {other:?}"),
        }
    }

    #[test]
    fn workspace_applies_provider_browser_state() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_active_panel(host_id.clone(), ProviderPanel::GitHub);
        workspace.select_linear_filter(host_id.clone(), "Assigned to me".to_owned());
        workspace.select_github_filter(host_id.clone(), "My PRs".to_owned());
        workspace.set_linear_search(host_id.clone(), "launcher".to_owned());
        let request_id = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(DomainEvent::LinearProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id,
            filter_name: Some("Assigned to me".to_owned()),
            search: "launcher".to_owned(),
            issues: vec![providers::linear::LinearIssue {
                id: "opaque".to_owned(),
                identifier: "LIN-123".to_owned(),
                title: "Fix launcher".to_owned(),
                body: "Issue body".to_owned(),
                branch: "lin-123-fix-launcher".to_owned(),
                url: "https://linear.test/LIN-123".to_owned(),
                state: None,
                state_type: None,
                assignee: None,
                updated_at: None,
            }],
        });
        workspace.select_linear_issue(host_id.clone(), "LIN-123".to_owned());

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(host.provider.active_panel, ProviderPanel::GitHub);
        assert_eq!(host.provider.linear.search, "launcher");
        assert_eq!(
            host.provider.linear.selected_filter.as_deref(),
            Some("Assigned to me")
        );
        assert_eq!(
            host.provider.github.selected_filter.as_deref(),
            Some("My PRs")
        );
        assert_eq!(
            host.provider.linear.selected_issue_id.as_deref(),
            Some("LIN-123")
        );
    }

    #[test]
    fn workspace_records_github_pr_status_without_session_mutation() {
        let host_id = HostId::new("local");
        let scope = GitHubProviderScope::new("project-a", "/repo/a");
        let status_key =
            GitHubPullRequestStatusKey::new(scope, "https://github.example/repo/pull/7");
        let mut workspace = Workspace::default();
        let request_id = workspace.begin_github_pull_request_status_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderPullRequestStatusLoaded {
            host_id: host_id.clone(),
            request_id,
            status_key: status_key.clone(),
            status: providers::github::PullRequestStatus {
                review_decision: providers::github::ReviewDecision::Approved,
                checks: vec![providers::github::CheckRun {
                    name: "test".to_owned(),
                    status: "SUCCESS".to_owned(),
                    conclusion: Some("pass".to_owned()),
                    details_url: None,
                }],
            },
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.sessions.is_empty());
        assert!(host
            .provider
            .github
            .pull_request_statuses
            .contains_key(&status_key));
    }

    #[test]
    fn workspace_ignores_stale_linear_provider_results() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let request_id = workspace.begin_linear_issues_request(host_id.clone());
        workspace.set_linear_search(host_id.clone(), "new".to_owned());
        workspace.apply(DomainEvent::LinearProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id,
            filter_name: None,
            search: "old".to_owned(),
            issues: vec![providers::linear::LinearIssue {
                id: "opaque".to_owned(),
                identifier: "LIN-123".to_owned(),
                title: "Stale issue".to_owned(),
                body: String::new(),
                branch: "lin-123".to_owned(),
                url: "https://linear.test/LIN-123".to_owned(),
                state: None,
                state_type: None,
                assignee: None,
                updated_at: None,
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.linear.issues.is_empty());
    }

    #[test]
    fn github_provider_lists_are_scoped_to_project() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let pull_requests_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: pull_requests_request,
            scope: GitHubProviderScope::new("project-a", "/repo/a"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "A",
                "",
                "feature/a",
                "https://github.example/a/pull/7",
            )],
        });
        let issues_request = workspace.begin_github_issues_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id: issues_request,
            scope: GitHubProviderScope::new("project-b", "/repo/b"),
            issues: vec![providers::github::GitHubIssue {
                number: 7,
                title: "B".to_owned(),
                body: String::new(),
                url: "https://github.example/b/issues/7".to_owned(),
                branch: None,
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(
            host.provider.github.scope.as_ref(),
            Some(&GitHubProviderScope::new("project-b", "/repo/b"))
        );
        assert!(host.provider.github.pull_requests.is_empty());
        assert_eq!(host.provider.github.issues.len(), 1);
    }

    #[test]
    fn github_provider_lists_are_scoped_to_repo_root() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let pull_requests_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: pull_requests_request,
            scope: GitHubProviderScope::new("project-a", "/repo/a"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "A",
                "",
                "feature/a",
                "https://github.example/a/pull/7",
            )],
        });
        let issues_request = workspace.begin_github_issues_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderIssuesLoaded {
            host_id: host_id.clone(),
            request_id: issues_request,
            scope: GitHubProviderScope::new("project-a", "/repo/b"),
            issues: vec![providers::github::GitHubIssue {
                number: 8,
                title: "B".to_owned(),
                body: String::new(),
                url: "https://github.example/b/issues/8".to_owned(),
                branch: None,
            }],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(
            host.provider.github.scope.as_ref(),
            Some(&GitHubProviderScope::new("project-a", "/repo/b"))
        );
        assert!(host.provider.github.pull_requests.is_empty());
        assert_eq!(host.provider.github.issues.len(), 1);
    }

    #[test]
    fn github_provider_ignores_stale_repo_scope_response() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: HostSnapshot {
                host_id: host_id.clone(),
                health: HealthSummary {
                    status: "ok".to_owned(),
                    daemon_version: "0.0.0".to_owned(),
                    protocol_version: protocol::PROTOCOL_VERSION,
                },
                sessions: Vec::new(),
                projects: vec![project("project-a", "/repo/current")],
                project_error: None,
                notifications: Vec::new(),
            },
        });
        workspace.selection = Some(Selection::Project {
            host_id: host_id.clone(),
            project_id: "project-a".to_owned(),
        });
        let request_id = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id,
            scope: GitHubProviderScope::new("project-a", "/repo/stale"),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "Stale",
                "",
                "feature/stale",
                "https://github.example/stale/pull/7",
            )],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.github.scope.is_none());
        assert!(host.provider.github.pull_requests.is_empty());
    }

    #[test]
    fn github_provider_ignores_stale_same_scope_success() {
        let host_id = HostId::new("local");
        let scope = GitHubProviderScope::new("project-a", "/repo/current");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_github_pull_requests_request(host_id.clone());
        let current_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.apply(DomainEvent::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: current_request,
            scope: scope.clone(),
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                9,
                "Current",
                "",
                "feature/current",
                "https://github.example/current/pull/9",
            )],
        });
        workspace.apply(DomainEvent::GitHubProviderPullRequestsLoaded {
            host_id: host_id.clone(),
            request_id: stale_request,
            scope,
            pull_requests: vec![providers::github::GitHubPullRequest::new(
                7,
                "Stale",
                "",
                "feature/stale",
                "https://github.example/stale/pull/7",
            )],
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(host.provider.github.pull_requests.len(), 1);
        assert_eq!(host.provider.github.pull_requests[0].number, 9);
    }

    #[test]
    fn provider_operation_failed_ignores_stale_request() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_linear_issues_request(host_id.clone());
        let _current_request = workspace.begin_linear_issues_request(host_id.clone());
        workspace.apply(DomainEvent::ProviderOperationFailed {
            host_id: host_id.clone(),
            provider: SessionLinkProvider::Linear,
            operation: ProviderOperation::LinearIssues,
            request_id: Some(stale_request),
            error: "stale failure".to_owned(),
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.linear.last_error.is_none());
    }

    #[test]
    fn provider_operation_failed_ignores_request_invalidated_by_selection() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let stale_request = workspace.begin_github_pull_requests_request(host_id.clone());
        workspace.select_project(host_id.clone(), "project-a".to_owned());
        workspace.apply(DomainEvent::ProviderOperationFailed {
            host_id: host_id.clone(),
            provider: SessionLinkProvider::GitHub,
            operation: ProviderOperation::GitHubPullRequests,
            request_id: Some(stale_request),
            error: "stale failure".to_owned(),
        });

        let host = workspace.hosts.get(&host_id).expect("host");
        assert!(host.provider.github.last_error.is_none());
    }

    #[test]
    fn snapshot_seed_does_not_notify_existing_blocked_session() {
        let mut workspace = Workspace::default();
        let host_id = HostId::new("local");
        workspace.apply(DomainEvent::HostConnecting {
            host_id: host_id.clone(),
        });
        workspace.apply(DomainEvent::HostSubscribed {
            host_id: host_id.clone(),
        });

        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Blocked))]),
        });

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn blocked_session_agent_state_no_longer_emits_transient_intent() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session("s-1", Some(AgentActivity::Working))]),
        });

        let raw = Event::new(
            event::AGENT_STATE,
            serde_json::json!({
                "session_id": "s-1",
                "activity": "blocked",
                "source": "osc_title"
            }),
        );
        workspace.apply(DomainEvent::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::AgentState(parse_agent_state(raw).expect("agent state")),
        });

        // The transient blocked-session OS notification path was removed; OS
        // intents now originate from durable `notification_created` events that
        // the daemon projector produces for a blocked session.
        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn host_snapshot_carries_notification_records() {
        let mut workspace = Workspace::default();
        let snapshot = snapshot_with_notifications(
            "local",
            vec![session("s-1", None)],
            vec![notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            )],
        );
        workspace.apply(DomainEvent::HostSnapshotLoaded { snapshot });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.notifications.contains_key("n-1"));
    }

    #[test]
    fn notification_created_event_parses_from_subscription_line() {
        let record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        let event = Event::new(
            event::NOTIFICATION_CREATED,
            serde_json::to_value(NotificationCreatedEvent {
                record: record.clone(),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            DomainEvent::HostEvent {
                event: HostEvent::NotificationCreated(parsed),
                ..
            } => assert_eq!(parsed, record),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn notification_updated_event_parses_from_subscription_line() {
        let record = notification_record(
            "n-1",
            NotificationStatus::Read,
            NotificationSeverity::ActionRequired,
        );
        let event = Event::new(
            event::NOTIFICATION_UPDATED,
            serde_json::to_value(NotificationUpdatedEvent {
                record: record.clone(),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            DomainEvent::HostEvent {
                event: HostEvent::NotificationUpdated(parsed),
                ..
            } => assert_eq!(parsed, record),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn notification_deleted_event_parses_from_subscription_line() {
        let event = Event::new(
            event::NOTIFICATION_DELETED,
            serde_json::to_value(NotificationDeletedEvent {
                notification_id: NotificationId("n-9".to_owned()),
            })
            .expect("payload"),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            DomainEvent::HostEvent {
                event: HostEvent::NotificationDeleted(id),
                ..
            } => assert_eq!(id, NotificationId("n-9".to_owned())),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn workspace_stores_notifications_per_host() {
        let mut workspace = Workspace::default();
        // Events only apply to hosts already known through the connect path; a
        // snapshot seeds each host before its notification events arrive.
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-a", vec![]),
        });
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-b", vec![]),
        });
        workspace.apply(notification_created(
            "host-a",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "host-b",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));

        let host_a = workspace.hosts.get(&HostId::new("host-a")).expect("host-a");
        let host_b = workspace.hosts.get(&HostId::new("host-b")).expect("host-b");
        assert!(host_a.notifications.contains_key("n-1"));
        assert!(!host_a.notifications.contains_key("n-2"));
        assert!(host_b.notifications.contains_key("n-2"));
    }

    #[test]
    fn unread_counts_track_lifecycle_transitions() {
        let mut workspace = Workspace::default();
        let host = HostId::new("local");
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 2);
        assert_eq!(workspace.host_unread_notification_count(&host), 2);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Read,
                NotificationSeverity::ActionRequired,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 1);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-2",
                NotificationStatus::Acknowledged,
                NotificationSeverity::Error,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 0);

        workspace.apply(notification_updated(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Archived,
                NotificationSeverity::ActionRequired,
            ),
        ));
        assert_eq!(workspace.unread_notification_count(), 0);

        workspace.apply(DomainEvent::HostEvent {
            host_id: HostId::new("local"),
            event: HostEvent::NotificationDeleted(NotificationId("n-2".to_owned())),
        });
        assert!(!workspace
            .hosts
            .get(&host)
            .expect("host")
            .notifications
            .contains_key("n-2"));
    }

    #[test]
    fn reconnect_reconciliation_merges_missed_notifications_without_duplicates() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        let intents_after_live = workspace.notification_intents.len();

        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![
                    notification_record(
                        "n-1",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                    notification_record(
                        "n-2",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                ],
            ),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert_eq!(host.notifications.len(), 2);
        assert!(host.notifications.contains_key("n-1"));
        assert!(host.notifications.contains_key("n-2"));
        // Seeding never emits OS intents; otherwise the periodic reconcile tick
        // would re-notify every still-unread record on every reload.
        assert_eq!(workspace.notification_intents.len(), intents_after_live);
    }

    #[test]
    fn reconnect_reconciliation_removes_seeded_deleted_notifications() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-deleted",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-live",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        let intents_after_live = workspace.notification_intents.len();

        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![
                    notification_record(
                        "n-live",
                        NotificationStatus::Unread,
                        NotificationSeverity::ActionRequired,
                    ),
                    notification_record(
                        "n-deleted",
                        NotificationStatus::Deleted,
                        NotificationSeverity::ActionRequired,
                    ),
                ],
            ),
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.notifications.contains_key("n-live"));
        assert!(!host.notifications.contains_key("n-deleted"));
        assert_eq!(workspace.notification_intents.len(), intents_after_live);
    }

    #[test]
    fn notification_seed_queries_include_deleted_tombstone_window() {
        let queries = notification_seed_queries();

        assert!(
            queries
                .iter()
                .any(|params| params.status == Some(NotificationStatus::Deleted)),
            "reconnect seed must include deleted tombstones"
        );
    }

    #[test]
    fn needs_action_scope_excludes_archived_and_untroubled_read_records() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let unread = notification_record(
            "n-unread",
            NotificationStatus::Unread,
            NotificationSeverity::Info,
        );
        let mut read_error = notification_record(
            "n-read-error",
            NotificationStatus::Read,
            NotificationSeverity::Error,
        );
        read_error.kind = NotificationKind::System;
        let mut archived_error = notification_record(
            "n-archived-error",
            NotificationStatus::Archived,
            NotificationSeverity::Error,
        );
        archived_error.kind = NotificationKind::System;
        let mut read_info = notification_record(
            "n-read-info",
            NotificationStatus::Read,
            NotificationSeverity::Info,
        );
        read_info.kind = NotificationKind::System;
        for record in [unread, read_error, archived_error, read_info] {
            workspace.apply(notification_created("local", record));
        }

        let rows = workspace.inbox_rows(
            NotificationScope::NeedsAction,
            &NotificationFilter::default(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.record.id.0.as_str()).collect();

        assert!(ids.contains(&"n-unread"));
        assert!(ids.contains(&"n-read-error"));
        assert!(!ids.contains(&"n-archived-error"));
        assert!(!ids.contains(&"n-read-info"));
    }

    #[test]
    fn inbox_rows_pin_action_kinds_above_unread_above_read() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let mut blocked_but_read = notification_record(
            "n-blocked",
            NotificationStatus::Read,
            NotificationSeverity::Info,
        );
        blocked_but_read.kind = NotificationKind::AgentBlocked;
        let mut unread_system = notification_record(
            "n-unread",
            NotificationStatus::Unread,
            NotificationSeverity::Info,
        );
        unread_system.kind = NotificationKind::System;
        let mut read_system = notification_record(
            "n-read",
            NotificationStatus::Read,
            NotificationSeverity::Info,
        );
        read_system.kind = NotificationKind::System;
        for record in [read_system, unread_system, blocked_but_read] {
            workspace.apply(notification_created("local", record));
        }

        let rows = workspace.inbox_rows(NotificationScope::All, &NotificationFilter::default());
        let ids: Vec<&str> = rows.iter().map(|row| row.record.id.0.as_str()).collect();

        assert_eq!(ids, vec!["n-blocked", "n-unread", "n-read"]);
    }

    #[test]
    fn inbox_rows_apply_host_filter() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-a", vec![]),
        });
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-b", vec![]),
        });
        workspace.apply(notification_created(
            "host-a",
            notification_record(
                "n-a",
                NotificationStatus::Unread,
                NotificationSeverity::Info,
            ),
        ));
        workspace.apply(notification_created(
            "host-b",
            notification_record(
                "n-b",
                NotificationStatus::Unread,
                NotificationSeverity::Info,
            ),
        ));

        let filter = NotificationFilter {
            host_id: Some(HostId::new("host-a")),
            ..NotificationFilter::default()
        };
        let rows = workspace.inbox_rows(NotificationScope::All, &filter);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record.id.0, "n-a");
    }

    #[test]
    fn selecting_linked_notification_selects_existing_session() {
        let mut workspace = Workspace::default();
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-1".to_owned()));
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![session("s-1", Some(AgentActivity::Blocked))],
                vec![record],
            ),
        });

        let selected = workspace
            .select_notification_session(&HostId::new("local"), &NotificationId("n-1".to_owned()));

        assert!(selected);
        assert_eq!(
            workspace.selection,
            Some(Selection::Session {
                host_id: HostId::new("local"),
                session_id: SessionId("s-1".to_owned()),
            })
        );
    }

    #[test]
    fn selecting_notification_without_live_session_leaves_selection_untouched() {
        let mut workspace = Workspace::default();
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-gone".to_owned()));
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications("local", vec![], vec![record]),
        });

        let selected = workspace
            .select_notification_session(&HostId::new("local"), &NotificationId("n-1".to_owned()));

        assert!(!selected);
        assert_eq!(workspace.selection, None);
    }

    #[test]
    fn action_required_notification_created_emits_single_os_intent() {
        let mut workspace = Workspace::default();
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let mut record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        record.session_id = Some(SessionId("s-1".to_owned()));
        workspace.apply(notification_created("local", record));

        assert_eq!(workspace.notification_intents.len(), 1);
        let intent = &workspace.notification_intents[0];
        assert_eq!(
            intent.notification_id,
            Some(NotificationId("n-1".to_owned()))
        );
        assert_eq!(intent.session_id, Some(SessionId("s-1".to_owned())));
        // A session-linked notification also surfaces an in-app toast.
        assert_eq!(workspace.toasts.len(), 1);
    }

    #[test]
    fn informational_notification_created_does_not_emit_os_intent() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::Info,
            ),
        ));

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace.toasts.is_empty());
    }

    #[test]
    fn snapshot_seed_does_not_emit_notification_intents() {
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![],
                vec![notification_record(
                    "n-1",
                    NotificationStatus::Unread,
                    NotificationSeverity::ActionRequired,
                )],
            ),
        });

        assert!(workspace.notification_intents.is_empty());
        assert!(workspace
            .hosts
            .get(&HostId::new("local"))
            .expect("host")
            .notifications
            .contains_key("n-1"));
    }

    #[test]
    fn notification_update_completed_message_updates_store() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(DomainEvent::NotificationUpdateCompleted {
            host_id: HostId::new("local"),
            result: NotificationUpdateResult {
                record: notification_record(
                    "n-1",
                    NotificationStatus::Read,
                    NotificationSeverity::ActionRequired,
                ),
            },
        });

        assert_eq!(workspace.unread_notification_count(), 0);
        assert_eq!(
            workspace
                .hosts
                .get(&HostId::new("local"))
                .expect("host")
                .notifications
                .get("n-1")
                .expect("n-1")
                .status,
            NotificationStatus::Read
        );
    }

    #[test]
    fn notification_delete_completed_message_removes_record() {
        let mut workspace = Workspace::default();
        workspace.apply(notification_created(
            "local",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(DomainEvent::NotificationDeleteCompleted {
            host_id: HostId::new("local"),
            result: NotificationDeleteResult {
                id: NotificationId("n-1".to_owned()),
                deleted: true,
            },
        });

        assert!(!workspace
            .hosts
            .get(&HostId::new("local"))
            .expect("host")
            .notifications
            .contains_key("n-1"));
    }

    #[test]
    fn notifications_selector_filters_and_orders_newest_first() {
        let mut workspace = Workspace::default();
        // Seed the host through the connect path so its events are not dropped.
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![]),
        });
        let mut older = notification_record(
            "n-old",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        older.created_at = "2026-07-01T00:00:00Z".to_owned();
        let mut newer = notification_record(
            "n-new",
            NotificationStatus::Read,
            NotificationSeverity::Error,
        );
        newer.created_at = "2026-07-03T00:00:00Z".to_owned();
        newer.kind = NotificationKind::Error;
        newer.source.provider = "claude".to_owned();
        workspace.apply(notification_created("local", older));
        workspace.apply(notification_created("local", newer));

        let rows = workspace.notifications(&NotificationFilter::default());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].record.id, NotificationId("n-new".to_owned()));
        assert_eq!(rows[1].record.id, NotificationId("n-old".to_owned()));

        let unread = workspace.notifications(&NotificationFilter {
            status: Some(NotificationStatus::Unread),
            ..NotificationFilter::default()
        });
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].record.id, NotificationId("n-old".to_owned()));

        let errors = workspace.notifications(&NotificationFilter {
            severity: Some(NotificationSeverity::Error),
            ..NotificationFilter::default()
        });
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].record.id, NotificationId("n-new".to_owned()));

        let claude = workspace.notifications(&NotificationFilter {
            provider: Some("claude".to_owned()),
            ..NotificationFilter::default()
        });
        assert_eq!(claude.len(), 1);

        let error_kind = workspace.notifications(&NotificationFilter {
            kind: Some(NotificationKind::Error),
            ..NotificationFilter::default()
        });
        assert_eq!(error_kind.len(), 1);
    }

    #[test]
    fn notifications_selector_filters_by_host() {
        let mut workspace = Workspace::default();
        // Seed both hosts through the connect path so their events are not dropped.
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-a", vec![]),
        });
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("host-b", vec![]),
        });
        workspace.apply(notification_created(
            "host-a",
            notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            ),
        ));
        workspace.apply(notification_created(
            "host-b",
            notification_record(
                "n-2",
                NotificationStatus::Unread,
                NotificationSeverity::Error,
            ),
        ));

        let rows = workspace.notifications(&NotificationFilter {
            host_id: Some(HostId::new("host-b")),
            ..NotificationFilter::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host_id, HostId::new("host-b"));
        assert_eq!(rows[0].record.id, NotificationId("n-2".to_owned()));
    }

    #[test]
    fn discovered_transport_prefers_netbird_ip_over_short_name() {
        let record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: Some("dev.example.netbird.cloud".to_owned()),
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };

        assert_eq!(
            discovered_transport_host(&record).expect("transport host"),
            "100.92.30.40"
        );
    }

    #[test]
    fn reconnect_backoff_is_capped_at_thirty_seconds() {
        let backoff = Backoff::new(ConnectionOptions {
            backoff_initial: Duration::from_secs(45),
            backoff_max: DEFAULT_BACKOFF_MAX.saturating_mul(4),
            ..ConnectionOptions::default()
        });

        assert_eq!(backoff.current, Duration::from_secs(30));
        assert_eq!(backoff.max, Duration::from_secs(30));
    }

    #[test]
    fn attach_command_replaces_declared_tokens() {
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "pohunek".to_owned(),
                host: "devbox".to_owned(),
                id: "s-7".to_owned(),
            },
        );

        assert_eq!(command, "pohunek attach --host devbox s-7");
    }

    #[test]
    fn attach_command_shell_escapes_substituted_tokens() {
        let command = render_attach_command(
            "{bin} attach --host {host} {id}",
            &AttachTemplateValues {
                bin: "/opt/pohunek bin".to_owned(),
                host: "devbox; touch /tmp/pwn".to_owned(),
                id: "s-7'$(touch /tmp/pwn)".to_owned(),
            },
        );

        assert_eq!(
            command,
            "'/opt/pohunek bin' attach --host 'devbox; touch /tmp/pwn' 's-7'\\''$(touch /tmp/pwn)'"
        );
    }

    #[test]
    fn provider_none_conversion_returns_error() {
        let err = action_prompt_provider(&ProviderKind::None).expect_err("provider none");

        assert!(matches!(
            err,
            CoreError::UnsupportedPromptProvider { provider: "none" }
        ));
    }

    #[test]
    fn subscribe_request_uses_sdk_request_id_generator() {
        let first = subscribe_request();
        let second = subscribe_request();

        assert_eq!(first.method, method::SUBSCRIBE);
        assert_eq!(first.params, Value::Null);
        assert!(first.id.starts_with("sdk-subscribe-"));
        assert!(second.id.starts_with("sdk-subscribe-"));
        assert_ne!(first.id, second.id);
    }

    fn snapshot(host_id: &str, sessions: Vec<SessionInfo>) -> HostSnapshot {
        HostSnapshot {
            host_id: HostId::new(host_id),
            health: HealthSummary {
                status: "ok".to_owned(),
                daemon_version: "0.0.0".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            },
            sessions,
            projects: Vec::new(),
            project_error: None,
            notifications: Vec::new(),
        }
    }

    fn session(id: &str, activity: Option<AgentActivity>) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/repo"),
            pid: 42,
            cols: 80,
            rows: 24,
            state: protocol::SessionState::Running,
            state_source: StateSource::Process,
            activity,
            native_session_id: None,
            native_session_path: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            project_id: None,
            project_label: None,
            is_linked_worktree: None,
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn project(id: &str, repo_root: &str) -> ProjectInfo {
        ProjectInfo {
            id: id.to_owned(),
            label: id.to_owned(),
            repo_root: PathBuf::from(repo_root),
            git_common_dir: PathBuf::from(repo_root).join(".git"),
            origin_url: None,
            default_base_branch: None,
            source: protocol::ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-01-01T00:00:00Z".to_owned(),
            last_used_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    fn snapshot_with_notifications(
        host_id: &str,
        sessions: Vec<SessionInfo>,
        notifications: Vec<NotificationRecord>,
    ) -> HostSnapshot {
        HostSnapshot {
            notifications,
            ..snapshot(host_id, sessions)
        }
    }

    fn notification_record(
        id: &str,
        status: NotificationStatus,
        severity: NotificationSeverity,
    ) -> NotificationRecord {
        NotificationRecord {
            id: NotificationId(id.to_owned()),
            source: NotificationSource {
                provider: "codex".to_owned(),
                provider_event: "agent_blocked".to_owned(),
                host_local_source_id: format!("src-{id}"),
            },
            kind: NotificationKind::AgentBlocked,
            severity,
            status,
            title: format!("Notification {id}"),
            body: "Agent is waiting for input".to_owned(),
            metadata: BTreeMap::new(),
            created_at: "2026-07-03T00:00:00Z".to_owned(),
            session_id: None,
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }

    fn notification_created(host: &str, record: NotificationRecord) -> DomainEvent {
        DomainEvent::HostEvent {
            host_id: HostId::new(host),
            event: HostEvent::NotificationCreated(record),
        }
    }

    fn notification_updated(host: &str, record: NotificationRecord) -> DomainEvent {
        DomainEvent::HostEvent {
            host_id: HostId::new(host),
            event: HostEvent::NotificationUpdated(record),
        }
    }
}
