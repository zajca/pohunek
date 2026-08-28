//! Headless workspace state machine and derived views for `gui-core`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use base64::prelude::{Engine as _, BASE64_STANDARD};
use protocol::{
    AgentActivity, Event, NotificationId, NotificationKind, NotificationKindPolicy,
    NotificationPolicy, NotificationRecord, NotificationSeverity, NotificationStatus, OutputOffset,
    ProjectActionResult, ProjectActionsResult, ProjectInfo, ProjectPromptResult, ProjectShowResult,
    RuntimeState, SessionId, SessionInfo, SessionRuntimeIdentity, SessionScreenResult,
    SessionState, SessionWaitResult, StateSource,
};

use crate::providers;
use crate::{
    parse_unified_diff, CoreError, DiffModel, DomainEvent, HealthSummary, HostId,
    ObservationCapabilities, PromptPreview, Review, ReviewComment, ReviewSide, ReviewSource,
    ReviewStatus, ReviewStore, Selection, SessionLinkProvider,
};

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

/// Keyboard selection target in the combined GitHub provider list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubProviderSelection {
    /// A pull request row.
    PullRequest(u64),
    /// An issue row.
    Issue(u64),
}

/// Diff fetch/parse status for the Review tab (`docs/design/track-d-ui-brief.md`
/// §3.9, UI-brief §5 loading/empty/error states).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReviewDiffStatus {
    /// No review opened yet for this host.
    #[default]
    Idle,
    /// A `session.diff`/`gh pr diff` fetch is in flight.
    Fetching,
    /// Diff fetched and parsed; the change set has at least one file.
    Loaded {
        model: DiffModel,
        /// Base ref the diff was actually computed against.
        base: String,
        /// Whether the daemon/`gh` truncated the diff at a file boundary.
        truncated: bool,
    },
    /// Diff fetched and parsed, but the change set touched no files.
    Empty { base: String },
    /// The fetch failed; `String` is the error message to display.
    Error(String),
}

/// Identifies one selectable diff line: a file/hunk/line triple into
/// [`ReviewDiffStatus::Loaded`]'s model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewLineTarget {
    pub file_index: usize,
    pub hunk_index: usize,
    pub line_index: usize,
}

/// Inline comment editor state: which line it targets and its draft text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCommentEditor {
    pub path: String,
    pub side: ReviewSide,
    pub line: u32,
    pub draft_text: String,
    /// `Some(index)` into `Review::comments` when editing an existing
    /// comment in place; `None` when composing a new one.
    pub editing_index: Option<usize>,
}

/// "Dispatch as session…" modal state.
///
/// `agent` is the operator's current pick from the modal's agent picker,
/// seeded from the source session's own profile when the modal opens
/// (see [`Workspace::open_review_dispatch_modal`]) and changed via
/// [`Workspace::set_review_dispatch_agent`]. It flows into
/// [`crate::ReviewDispatchParams::agent`] at confirm time, overriding
/// `session_info.agent` for the dispatched session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDispatchModal {
    /// Rendered prompt preview, or the render error message
    /// (`render_review_prompt` failure, e.g. a missing `review.tmpl`).
    pub prompt_preview: Result<String, String>,
    /// Wire agent name the dispatched session will run: the source
    /// session's profile by default, or the operator's picked override.
    pub agent: String,
    /// Whether the source session's agent is currently `Working`.
    pub source_working: bool,
    /// Set after a failed dispatch attempt; the draft stays untouched and
    /// the modal stays open showing this message.
    pub dispatch_error: Option<String>,
}

/// Review tab state owned by gui-core: diff fetch status, the active draft
/// review, file/line selection, and modal state.
///
/// One active review per host at a time, matching how
/// [`GitHubProviderState`]/[`LinearProviderState`] scope to a single active
/// browse target rather than per-session slots — opening a review from a
/// different session or pull request replaces this state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewTabState {
    pub diff: ReviewDiffStatus,
    pub active_review: Option<Review>,
    pub selected_file: Option<usize>,
    pub selected_line: Option<ReviewLineTarget>,
    pub comment_editor: Option<ReviewCommentEditor>,
    pub dispatch: Option<ReviewDispatchModal>,
    /// Request id of the in-flight diff fetch, guarding a stale completion
    /// (same pattern as the Linear/GitHub provider fetch requests).
    pub diff_request: Option<ProviderRequestId>,
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
    /// The replacement daemon adopted the same PTY runtime generation.
    RuntimeReconnected(SessionInfo),
    /// The worker-backed PTY runtime is no longer available.
    RuntimeLost(SessionInfo),
    /// More than one worker claims the logical session.
    RuntimeConflict(SessionInfo),
    /// Explicit provider-native recovery created a different PTY generation.
    NativeRecovered(SessionInfo),
    /// A durable notification record was created on the host.
    NotificationCreated(NotificationRecord),
    /// A durable notification record changed lifecycle status or content.
    NotificationUpdated(NotificationRecord),
    /// A durable notification record was hard-deleted on the host.
    NotificationDeleted(NotificationId),
    Other(Event),
}

/// Relationship between the current PTY generation and the previous one seen
/// by the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeContinuity {
    /// The daemon reconnected to the exact same worker/runtime generation.
    Reconnected,
    /// Explicit recovery replaced the PTY with a new runtime generation.
    Recovered,
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

/// Coarse activity-feed scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationScope {
    /// Recent non-archived activity, regardless of read state.
    #[default]
    Recent,
    /// Only activity the operator has not opened yet.
    Unread,
    /// Only archived notifications.
    Archived,
}

impl NotificationScope {
    /// Whether `record` is visible under this scope.
    #[must_use]
    pub fn matches(self, record: &NotificationRecord) -> bool {
        match self {
            Self::Recent => !matches!(
                record.status,
                NotificationStatus::Archived | NotificationStatus::Deleted
            ),
            Self::Unread => record.status == NotificationStatus::Unread,
            Self::Archived => record.status == NotificationStatus::Archived,
        }
    }
}

fn notification_kind_enabled_mut(
    policy: &mut NotificationKindPolicy,
    kind: NotificationKind,
) -> &mut bool {
    match kind {
        NotificationKind::AgentBlocked => &mut policy.agent_blocked,
        NotificationKind::ApprovalRequired => &mut policy.approval_required,
        NotificationKind::TurnCompleted => &mut policy.turn_completed,
        NotificationKind::SessionFinished => &mut policy.session_finished,
        NotificationKind::Error => &mut policy.error,
        NotificationKind::System => &mut policy.system,
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
    /// Diff review tab state: fetch status, active draft, selection, modals.
    pub review: ReviewTabState,
    /// Durable notification records for this host, keyed by notification id.
    pub notifications: BTreeMap<String, NotificationRecord>,
    pub last_agent_state: Option<AgentStateEvent>,
    pub last_error: Option<String>,
    /// Agent and profile names known to the daemon, seeded from `host.inspect`.
    ///
    /// See [`crate::HostSnapshot::supported_agents`] for its compatibility
    /// contract. Launch decisions must use `runtimes`.
    pub supported_agents: Vec<String>,
    /// Full host runtime inventory used for capability-honest launch choices.
    pub runtimes: Vec<protocol::AgentRuntime>,
    /// Provider names reported by the host's runtime inventory.
    pub notification_providers: Vec<String>,
    pub observation_capabilities: ObservationCapabilities,
}

/// Provider-neutral terminal observation retained for one GUI session pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionObservation {
    /// Most recently loaded terminal screen snapshot.
    pub screen: Option<SessionScreenResult>,
    /// UTF-8-lossy output accumulated from contiguous output pages.
    pub output_text: String,
    /// Runtime identity associated with the accumulated output.
    pub output_runtime: Option<SessionRuntimeIdentity>,
    /// Cursor to use for the next output page.
    pub output_cursor: Option<OutputOffset>,
    /// Retention gap reported by the most recent output response.
    pub output_gap: Option<(OutputOffset, OutputOffset)>,
    /// Most recently completed session wait.
    pub wait: Option<SessionWaitResult>,
}

impl HostView {
    /// Returns whether `agent` is launchable according to the host inventory.
    #[must_use]
    pub fn agent_is_launchable(&self, agent: &str) -> bool {
        self.runtimes
            .iter()
            .any(|runtime| runtime.agent == agent && crate::runtime_is_launchable(runtime))
    }

    /// Returns whether `agent` is a launchable non-shell assistant runtime.
    #[must_use]
    pub fn agent_is_assistant_capable(&self, agent: &str) -> bool {
        self.runtimes
            .iter()
            .any(|runtime| runtime.agent == agent && crate::runtime_is_assistant_capable(runtime))
    }

    /// Returns launchable agent names in host inventory order.
    #[must_use]
    pub fn launchable_agents(&self) -> Vec<String> {
        self.runtimes
            .iter()
            .filter(|runtime| crate::runtime_is_launchable(runtime))
            .map(|runtime| runtime.agent.clone())
            .collect()
    }

    /// Returns launchable assistant runtime names in host inventory order.
    #[must_use]
    pub fn launchable_assistant_agents(&self) -> Vec<String> {
        self.runtimes
            .iter()
            .filter(|runtime| crate::runtime_is_assistant_capable(runtime))
            .map(|runtime| runtime.agent.clone())
            .collect()
    }

    fn connecting() -> Self {
        Self {
            conn: ConnState::Connecting,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            project_details: BTreeMap::new(),
            prompt: PromptState::default(),
            provider: ProviderState::default(),
            review: ReviewTabState::default(),
            notifications: BTreeMap::new(),
            last_agent_state: None,
            last_error: None,
            supported_agents: Vec::new(),
            runtimes: Vec::new(),
            notification_providers: Vec::new(),
            observation_capabilities: ObservationCapabilities::default(),
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
    session_observations: BTreeMap<(HostId, String), SessionObservation>,
    notification_policies: BTreeMap<HostId, NotificationPolicy>,
    runtime_continuity: BTreeMap<(HostId, String), RuntimeContinuity>,
    reconnecting_hosts: BTreeSet<HostId>,
    next_intent_id: u64,
    next_provider_request_id: u64,
}

impl Workspace {
    /// Returns cached provider-neutral terminal observation for a session.
    #[must_use]
    pub fn session_observation(
        &self,
        host_id: &HostId,
        session_id: &SessionId,
    ) -> Option<&SessionObservation> {
        self.session_observations
            .get(&(host_id.clone(), session_id.0.clone()))
    }

    /// Returns the current provider-keyed notification policy for a host.
    #[must_use]
    pub fn notification_policy(&self, host_id: &HostId) -> Option<&NotificationPolicy> {
        self.notification_policies.get(host_id)
    }

    /// Updates one base or provider-specific notification kind in cached state.
    pub fn set_notification_policy_kind(
        &mut self,
        host_id: &HostId,
        provider: Option<&str>,
        kind: NotificationKind,
        enabled: bool,
    ) -> bool {
        let Some(policy) = self.notification_policies.get_mut(host_id) else {
            return false;
        };
        let kind_policy = match provider {
            Some(provider) => policy
                .providers
                .entry(provider.to_owned())
                .or_insert_with(|| policy.enabled.clone()),
            None => &mut policy.enabled,
        };
        *notification_kind_enabled_mut(kind_policy, kind) = enabled;
        true
    }

    /// Return how the current runtime relates to the prior observed generation.
    #[must_use]
    pub fn runtime_continuity(
        &self,
        host_id: &HostId,
        session_id: &SessionId,
    ) -> Option<RuntimeContinuity> {
        self.runtime_continuity
            .get(&(host_id.clone(), session_id.0.clone()))
            .copied()
    }

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
                if self
                    .hosts
                    .get(&host_id)
                    .is_some_and(|host| !host.sessions.is_empty())
                {
                    self.reconnecting_hosts.insert(host_id.clone());
                }
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
                let prior_sessions = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(BTreeMap::new, |host| host.sessions.clone());
                let reconnecting = self.reconnecting_hosts.remove(&host_id);
                let sessions: BTreeMap<String, SessionInfo> = snapshot
                    .sessions
                    .iter()
                    .cloned()
                    .map(|session| (session.id.0.clone(), session))
                    .collect();
                for (session_id, session) in &sessions {
                    let Some(previous) = prior_sessions.get(session_id) else {
                        continue;
                    };
                    let key = (host_id.clone(), session_id.clone());
                    if runtime_generation_changed(previous, session) {
                        self.session_observations.remove(&key);
                        self.runtime_continuity
                            .insert(key, RuntimeContinuity::Recovered);
                    } else if reconnecting && same_runtime_generation(previous, session) {
                        self.runtime_continuity
                            .insert(key, RuntimeContinuity::Reconnected);
                    }
                }
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
                let previous_review = self
                    .hosts
                    .get(&host_id)
                    .map_or_else(ReviewTabState::default, |host| host.review.clone());
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
                        review: previous_review,
                        notifications,
                        last_agent_state: None,
                        last_error: snapshot.project_error,
                        supported_agents: snapshot.supported_agents,
                        runtimes: snapshot.runtimes,
                        notification_providers: snapshot.notification_providers,
                        observation_capabilities: snapshot.observation_capabilities,
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
                let observation_invalidation = self
                    .hosts
                    .get(&host_id)
                    .and_then(|host| observation_invalidation_for_host_event(host, &event));
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
                    &mut self.runtime_continuity,
                );
                if let Some(session_id) = observation_invalidation {
                    self.session_observations.remove(&(host_id, session_id));
                }
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
                let key = (host_id.clone(), session.id.0.clone());
                let runtime_changed = self
                    .hosts
                    .get(&host_id)
                    .and_then(|host| host.sessions.get(&session.id.0))
                    .is_some_and(|previous| runtime_generation_changed(previous, &session));
                let Some(host) = self.host_mut_if_known(&host_id, "session result") else {
                    return;
                };
                host.sessions.insert(session.id.0.clone(), session);
                if runtime_changed {
                    self.session_observations.remove(&key);
                }
            }
            DomainEvent::SessionResumed { host_id, result } => {
                let key = (host_id.clone(), result.session.id.0.clone());
                let runtime_changed = self
                    .hosts
                    .get(&host_id)
                    .and_then(|host| host.sessions.get(&result.session.id.0))
                    .is_some_and(|previous| runtime_generation_changed(previous, &result.session));
                let Some(host) = self.host_mut_if_known(&host_id, "session resume result") else {
                    return;
                };
                let session = result.session;
                host.sessions.insert(session.id.0.clone(), session);
                if runtime_changed {
                    self.session_observations.remove(&key);
                }
            }
            DomainEvent::SessionForked { host_id, result } => {
                let Some(host) = self.host_mut_if_known(&host_id, "session fork result") else {
                    return;
                };
                let session = result.session;
                host.sessions.insert(session.id.0.clone(), session);
            }
            DomainEvent::SessionScreenLoaded { host_id, result } => {
                let key = (host_id, result.session_id.0.clone());
                let observation = self.session_observations.entry(key).or_default();
                if observation.output_runtime.as_ref() != Some(&result.runtime) {
                    observation.output_text.clear();
                    observation.output_runtime = None;
                    observation.output_cursor = None;
                    observation.output_gap = None;
                }
                observation.screen = Some(result);
            }
            DomainEvent::SessionOutputLoaded { host_id, result } => {
                let key = (host_id, result.session_id().0.clone());
                let observation = self.session_observations.entry(key).or_default();
                if observation.output_runtime.as_ref() != Some(result.runtime()) {
                    observation.output_text.clear();
                    observation.output_gap = None;
                    if observation
                        .screen
                        .as_ref()
                        .is_some_and(|screen| &screen.runtime != result.runtime())
                    {
                        observation.screen = None;
                    }
                }
                if let Some(gap) = result.gap() {
                    observation.output_text.clear();
                    observation.output_gap = Some((gap.start_offset(), gap.end_offset()));
                }
                if let Ok(bytes) = BASE64_STANDARD.decode(result.data_base64()) {
                    observation
                        .output_text
                        .push_str(&String::from_utf8_lossy(&bytes));
                }
                observation.output_runtime = Some(result.runtime().clone());
                observation.output_cursor = Some(result.next_offset());
            }
            DomainEvent::SessionObservationRuntimeChanged {
                host_id,
                session_id,
                ..
            } => {
                self.session_observations.remove(&(host_id, session_id.0));
            }
            DomainEvent::SessionWaitCompleted { host_id, result } => {
                let session_id = result.session.id.clone();
                let key = (host_id.clone(), session_id.0.clone());
                let runtime_changed = self
                    .hosts
                    .get(&host_id)
                    .and_then(|host| host.sessions.get(&session_id.0))
                    .is_some_and(|previous| runtime_generation_changed(previous, &result.session));
                if let Some(host) = self.hosts.get_mut(&host_id) {
                    host.sessions
                        .insert(session_id.0.clone(), result.session.clone());
                }
                if runtime_changed {
                    self.session_observations.remove(&key);
                }
                self.session_observations
                    .entry((host_id, session_id.0))
                    .or_default()
                    .wait = Some(result);
            }
            DomainEvent::NotificationPolicyLoaded { host_id, result } => {
                self.notification_policies.insert(host_id, result.policy);
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
                let session_key = session_id.0;
                host.sessions.remove(&session_key);
                self.runtime_continuity
                    .remove(&(host_id.clone(), session_key.clone()));
                self.session_observations.remove(&(host_id, session_key));
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
            DomainEvent::ReviewDiffLoaded {
                host_id,
                request_id,
                diff_text,
                base,
                truncated,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "review diff result") else {
                    return;
                };
                if host.review.diff_request != Some(request_id) {
                    // A stale completion: the operator opened a different
                    // review (or closed this one) before this fetch landed.
                    return;
                }
                host.review.diff_request = None;
                let model = parse_unified_diff(&diff_text);
                host.review.diff = if model.files.is_empty() {
                    ReviewDiffStatus::Empty { base }
                } else {
                    ReviewDiffStatus::Loaded {
                        model,
                        base,
                        truncated,
                    }
                };
            }
            DomainEvent::ReviewDiffFailed {
                host_id,
                request_id,
                error,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "review diff failure") else {
                    return;
                };
                if host.review.diff_request != Some(request_id) {
                    return;
                }
                host.review.diff_request = None;
                host.review.diff = ReviewDiffStatus::Error(error);
            }
            DomainEvent::ReviewDispatched {
                host_id,
                review,
                result,
            } => {
                let Some(host) = self.host_mut_if_known(&host_id, "review dispatch result") else {
                    return;
                };
                let session = result.session;
                host.sessions.insert(session.id.0.clone(), session);
                host.review.active_review = Some(review);
                host.review.dispatch = None;
            }
            DomainEvent::ReviewDispatchFailed { host_id, error } => {
                let Some(host) = self.host_mut_if_known(&host_id, "review dispatch failure") else {
                    return;
                };
                if let Some(dispatch) = &mut host.review.dispatch {
                    dispatch.dispatch_error = Some(error);
                }
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

    /// Select the next visible Linear issue for `host_id`.
    pub fn select_next_linear_issue(&mut self, host_id: &HostId) -> Option<String> {
        self.select_linear_issue_by_keyboard(host_id, SelectionDirection::Next)
    }

    /// Select the previous visible Linear issue for `host_id`.
    pub fn select_previous_linear_issue(&mut self, host_id: &HostId) -> Option<String> {
        self.select_linear_issue_by_keyboard(host_id, SelectionDirection::Previous)
    }

    fn select_linear_issue_by_keyboard(
        &mut self,
        host_id: &HostId,
        direction: SelectionDirection,
    ) -> Option<String> {
        let host = self.hosts.get_mut(host_id)?;
        let state = &mut host.provider.linear;
        let visible = visible_linear_issue_ids(state);
        let current = state.selected_issue_id.as_deref();
        let current_index = visible
            .iter()
            .position(|issue_id| Some(issue_id.as_str()) == current);
        let selected_index = move_selection(current_index, visible.len(), direction)?;
        let selected_id = visible[selected_index].clone();
        state.selected_issue_id = Some(selected_id.clone());
        Some(selected_id)
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
        host.provider.github.selected_issue = None;
    }

    /// Select the next visible GitHub pull request for `host_id`.
    pub fn select_next_github_pull_request(&mut self, host_id: &HostId) -> Option<u64> {
        self.select_github_pull_request_by_keyboard(host_id, SelectionDirection::Next)
    }

    /// Select the previous visible GitHub pull request for `host_id`.
    pub fn select_previous_github_pull_request(&mut self, host_id: &HostId) -> Option<u64> {
        self.select_github_pull_request_by_keyboard(host_id, SelectionDirection::Previous)
    }

    fn select_github_pull_request_by_keyboard(
        &mut self,
        host_id: &HostId,
        direction: SelectionDirection,
    ) -> Option<u64> {
        let host = self.hosts.get_mut(host_id)?;
        let state = &mut host.provider.github;
        let visible = visible_github_pull_request_numbers(state);
        let current_index = visible
            .iter()
            .position(|number| Some(*number) == state.selected_pull_request);
        let selected_index = move_selection(current_index, visible.len(), direction)?;
        let selected_number = visible[selected_index];
        state.selected_pull_request = Some(selected_number);
        Some(selected_number)
    }

    /// Select a GitHub issue in the provider browser for `host_id`.
    pub fn select_github_issue(&mut self, host_id: HostId, number: u64) {
        let host = self.host_for_ui(host_id);
        host.provider.github.selected_issue = Some(number);
        host.provider.github.selected_pull_request = None;
    }

    /// Select the next visible GitHub issue for `host_id`.
    pub fn select_next_github_issue(&mut self, host_id: &HostId) -> Option<u64> {
        self.select_github_issue_by_keyboard(host_id, SelectionDirection::Next)
    }

    /// Select the previous visible GitHub issue for `host_id`.
    pub fn select_previous_github_issue(&mut self, host_id: &HostId) -> Option<u64> {
        self.select_github_issue_by_keyboard(host_id, SelectionDirection::Previous)
    }

    fn select_github_issue_by_keyboard(
        &mut self,
        host_id: &HostId,
        direction: SelectionDirection,
    ) -> Option<u64> {
        let host = self.hosts.get_mut(host_id)?;
        let state = &mut host.provider.github;
        let visible = visible_github_issue_numbers(state);
        let current_index = visible
            .iter()
            .position(|number| Some(*number) == state.selected_issue);
        let selected_index = move_selection(current_index, visible.len(), direction)?;
        let selected_number = visible[selected_index];
        state.selected_issue = Some(selected_number);
        state.selected_pull_request = None;
        Some(selected_number)
    }

    /// Select the next visible GitHub provider row for `host_id`.
    pub fn select_next_github_item(&mut self, host_id: &HostId) -> Option<GitHubProviderSelection> {
        self.select_github_item_by_keyboard(host_id, SelectionDirection::Next)
    }

    /// Select the previous visible GitHub provider row for `host_id`.
    pub fn select_previous_github_item(
        &mut self,
        host_id: &HostId,
    ) -> Option<GitHubProviderSelection> {
        self.select_github_item_by_keyboard(host_id, SelectionDirection::Previous)
    }

    fn select_github_item_by_keyboard(
        &mut self,
        host_id: &HostId,
        direction: SelectionDirection,
    ) -> Option<GitHubProviderSelection> {
        let host = self.hosts.get_mut(host_id)?;
        let state = &mut host.provider.github;
        let visible = visible_github_provider_selections(state);
        let current = match (state.selected_pull_request, state.selected_issue) {
            (Some(number), _) => Some(GitHubProviderSelection::PullRequest(number)),
            (None, Some(number)) => Some(GitHubProviderSelection::Issue(number)),
            (None, None) => None,
        };
        let current_index = visible
            .iter()
            .position(|selection| Some(*selection) == current);
        let selected_index = move_selection(current_index, visible.len(), direction)?;
        let selection = visible[selected_index];
        match selection {
            GitHubProviderSelection::PullRequest(number) => {
                state.selected_pull_request = Some(number);
                state.selected_issue = None;
            }
            GitHubProviderSelection::Issue(number) => {
                state.selected_pull_request = None;
                state.selected_issue = Some(number);
            }
        }
        Some(selection)
    }

    /// Opens (or replaces) `host_id`'s Review tab for a session's worktree
    /// diff and marks the fetch pending. Returns the request id the caller's
    /// async diff fetch must complete with via [`DomainEvent::ReviewDiffLoaded`]
    /// or [`DomainEvent::ReviewDiffFailed`].
    ///
    /// Resumes the most-recently-updated `Draft` review in `store` for this
    /// exact source (same host + session id), comments and all, instead of
    /// minting a fresh one — otherwise a persisted draft would become an
    /// unreachable orphan the moment the operator navigated away and back
    /// (reviews are expected to survive a GUI restart). Mints a new
    /// [`Review`] only when no matching draft exists on disk.
    pub fn begin_review_from_session(
        &mut self,
        host_id: HostId,
        store: &ReviewStore,
        session: &SessionInfo,
        project: impl Into<String>,
    ) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let source = ReviewSource::Session {
            host_id: host_id.clone(),
            session_id: session.id.clone(),
        };
        let review = resume_or_new_review(
            store,
            &source,
            project,
            session.branch.clone().unwrap_or_default(),
        );
        let host = self.host_for_ui(host_id);
        host.review = ReviewTabState {
            diff: ReviewDiffStatus::Fetching,
            active_review: Some(review),
            diff_request: Some(request_id),
            ..ReviewTabState::default()
        };
        request_id
    }

    /// Opens (or replaces) `host_id`'s Review tab for a GitHub pull request
    /// diff. See [`Self::begin_review_from_session`] for the resume
    /// rationale (same host + PR number here), which applies identically.
    pub fn begin_review_from_pull_request(
        &mut self,
        host_id: HostId,
        store: &ReviewStore,
        pr_number: u64,
        project: impl Into<String>,
        branch: impl Into<String>,
    ) -> ProviderRequestId {
        let request_id = self.next_provider_request_id();
        let source = ReviewSource::PullRequest {
            host_id: host_id.clone(),
            pr_number,
        };
        let review = resume_or_new_review(store, &source, project, branch);
        let host = self.host_for_ui(host_id);
        host.review = ReviewTabState {
            diff: ReviewDiffStatus::Fetching,
            active_review: Some(review),
            diff_request: Some(request_id),
            ..ReviewTabState::default()
        };
        request_id
    }

    /// Re-marks `host_id`'s Review tab diff fetch pending without disturbing
    /// the active draft review (its comments, dispatch state, file/line
    /// selection). Returns `None` (no-op) when there is no active review to
    /// refresh.
    pub fn begin_review_diff_refresh(&mut self, host_id: &HostId) -> Option<ProviderRequestId> {
        self.hosts.get(host_id)?.review.active_review.as_ref()?;
        let request_id = self.next_provider_request_id();
        let host = self.hosts.get_mut(host_id)?;
        host.review.diff = ReviewDiffStatus::Fetching;
        host.review.diff_request = Some(request_id);
        Some(request_id)
    }

    /// Selects a file in the Review tab's file list, clearing any line
    /// selection (mirrors clicking a file row rather than a specific line).
    pub fn select_review_file(&mut self, host_id: &HostId, file_index: usize) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        host.review.selected_file = Some(file_index);
        host.review.selected_line = None;
    }

    /// Selects one diff line directly (mouse click on a line).
    pub fn select_review_line(&mut self, host_id: &HostId, target: ReviewLineTarget) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        host.review.selected_file = Some(target.file_index);
        host.review.selected_line = Some(target);
    }

    /// Moves the Review tab's line cursor to the next selectable line,
    /// flowing from one file's lines into the next file's lines at the
    /// boundary (`docs/design/track-d-ui-brief.md` §3.9's "files → hunks →
    /// lines" browsing, folded into one continuous keyboard traversal).
    pub fn select_next_review_line(&mut self, host_id: &HostId) -> Option<ReviewLineTarget> {
        self.select_review_line_by_keyboard(host_id, SelectionDirection::Next)
    }

    /// Moves the Review tab's line cursor to the previous selectable line.
    pub fn select_previous_review_line(&mut self, host_id: &HostId) -> Option<ReviewLineTarget> {
        self.select_review_line_by_keyboard(host_id, SelectionDirection::Previous)
    }

    fn select_review_line_by_keyboard(
        &mut self,
        host_id: &HostId,
        direction: SelectionDirection,
    ) -> Option<ReviewLineTarget> {
        let host = self.hosts.get_mut(host_id)?;
        let ReviewDiffStatus::Loaded { model, .. } = &host.review.diff else {
            return None;
        };
        let targets = flattened_review_line_targets(model);
        let current_index = host
            .review
            .selected_line
            .and_then(|current| targets.iter().position(|target| *target == current));
        let selected_index = move_selection(current_index, targets.len(), direction)?;
        let target = targets[selected_index];
        host.review.selected_file = Some(target.file_index);
        host.review.selected_line = Some(target);
        Some(target)
    }

    /// Opens the inline comment editor for the currently selected line with a
    /// blank draft. Returns `false` (no-op) when no line is selected or the
    /// diff is not loaded.
    pub fn begin_review_comment(&mut self, host_id: &HostId) -> bool {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return false;
        };
        let ReviewDiffStatus::Loaded { model, .. } = &host.review.diff else {
            return false;
        };
        let Some(target) = host.review.selected_line else {
            return false;
        };
        let Some((path, side, line)) = review_line_anchor(model, target) else {
            return false;
        };
        host.review.comment_editor = Some(ReviewCommentEditor {
            path,
            side,
            line,
            draft_text: String::new(),
            editing_index: None,
        });
        true
    }

    /// Opens the inline comment editor pre-filled to edit an existing
    /// comment on the active review. Returns `false` (no-op) when there is no
    /// active review or `index` is out of range.
    pub fn begin_edit_review_comment(&mut self, host_id: &HostId, index: usize) -> bool {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return false;
        };
        let Some(review) = &host.review.active_review else {
            return false;
        };
        let Some(comment) = review.comments.get(index) else {
            return false;
        };
        host.review.comment_editor = Some(ReviewCommentEditor {
            path: comment.path.clone(),
            side: comment.side,
            line: comment.line,
            draft_text: comment.text.clone(),
            editing_index: Some(index),
        });
        true
    }

    /// Updates the open comment editor's draft text. No-op without an open
    /// editor.
    pub fn update_review_comment_draft(&mut self, host_id: &HostId, text: String) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        let Some(editor) = &mut host.review.comment_editor else {
            return;
        };
        editor.draft_text = text;
    }

    /// Closes the comment editor without saving.
    pub fn cancel_review_comment_editor(&mut self, host_id: &HostId) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        host.review.comment_editor = None;
    }

    /// Saves the open comment editor: appends a new comment, or edits the one
    /// at `editing_index` in place, persists the review via `store`, and
    /// closes the editor. No-op returning `Ok(())` when no editor is open or
    /// there is no active review (idempotent under a double-submit).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ReviewStore`] when persisting fails; the
    /// in-memory review still reflects the added/edited comment (only the
    /// disk write failed), so the next successful save includes it.
    pub fn save_review_comment(
        &mut self,
        host_id: &HostId,
        store: &ReviewStore,
    ) -> Result<(), CoreError> {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return Ok(());
        };
        let Some(editor) = host.review.comment_editor.take() else {
            return Ok(());
        };
        let Some(review) = &mut host.review.active_review else {
            return Ok(());
        };
        if let Some(index) = editor.editing_index {
            review.edit_comment(index, editor.draft_text);
        } else {
            review.add_comment(ReviewComment::new(
                editor.path,
                editor.side,
                editor.line,
                editor.draft_text,
            ));
        }
        store.save(review)?;
        Ok(())
    }

    /// Removes the comment at `index` from the active review and persists.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::ReviewStore`] when persisting fails.
    pub fn remove_review_comment(
        &mut self,
        host_id: &HostId,
        store: &ReviewStore,
        index: usize,
    ) -> Result<(), CoreError> {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return Ok(());
        };
        let Some(review) = &mut host.review.active_review else {
            return Ok(());
        };
        review.remove_comment(index);
        store.save(review)?;
        Ok(())
    }

    /// Opens the "Dispatch as session…" modal for the active review, seeded
    /// with a prompt preview render outcome, the resolved agent label, and
    /// whether the source session is currently working.
    pub fn open_review_dispatch_modal(
        &mut self,
        host_id: &HostId,
        prompt_preview: Result<String, String>,
        agent: String,
        source_working: bool,
    ) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        host.review.dispatch = Some(ReviewDispatchModal {
            prompt_preview,
            agent,
            source_working,
            dispatch_error: None,
        });
    }

    /// Closes the dispatch modal without dispatching.
    pub fn close_review_dispatch_modal(&mut self, host_id: &HostId) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        host.review.dispatch = None;
    }

    /// Sets the dispatch modal's agent picker to `agent`, overriding the
    /// source session's profile for the dispatched session. No-op without an
    /// open dispatch modal.
    pub fn set_review_dispatch_agent(&mut self, host_id: &HostId, agent: String) {
        let Some(host) = self.hosts.get_mut(host_id) else {
            return;
        };
        let Some(dispatch) = &mut host.review.dispatch else {
            return;
        };
        dispatch.agent = agent;
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

    /// Notification rows for the activity modal.
    ///
    /// `scope` narrows by lifecycle, `filter` narrows by host as with
    /// [`Workspace::notifications`], and the stable newest-first ordering is
    /// preserved. Read state never moves a row under the operator's cursor.
    #[must_use]
    pub fn inbox_rows(
        &self,
        scope: NotificationScope,
        filter: &NotificationFilter,
    ) -> Vec<NotificationRow> {
        self.notifications(filter)
            .into_iter()
            .filter(|row| scope.matches(&row.record))
            .collect()
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

    /// Return one session's durable activity, newest first.
    #[must_use]
    pub fn session_activity(
        &self,
        host_id: &HostId,
        session_id: &SessionId,
    ) -> Vec<NotificationRecord> {
        let Some(host) = self.hosts.get(host_id) else {
            return Vec::new();
        };
        let mut records: Vec<NotificationRecord> = host
            .notifications
            .values()
            .filter(|record| {
                record.session_id.as_ref() == Some(session_id)
                    && record.status != NotificationStatus::Deleted
            })
            .cloned()
            .collect();
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        records
    }

    /// Build the prioritized native-GUI session list.
    ///
    /// Rows are grouped by operator urgency and sorted by stable host/session
    /// identity within each group. Activity changes may move a row between
    /// groups, but never reorder unrelated rows inside one group.
    #[must_use]
    pub fn session_rows(&self) -> Vec<SessionRow> {
        let mut rows = Vec::new();
        for (host_id, host) in &self.hosts {
            for session in host.sessions.values() {
                let attention = active_session_attention(host, session);
                let access = session_access(session);
                rows.push(SessionRow {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                    name: session.name.clone(),
                    project_id: session.project_id.clone(),
                    project_label: session.project_label.clone(),
                    agent: session.agent.clone(),
                    activity: session.activity,
                    state: session.state,
                    branch: session.branch.clone(),
                    group: session_group(session, access, attention.is_some()),
                    attention,
                    access,
                    can_stop: session_can_stop(session),
                    can_remove: session_can_remove(session),
                });
            }
        }
        rows.sort_by(|left, right| {
            left.group
                .cmp(&right.group)
                .then_with(|| left.host_id.cmp(&right.host_id))
                .then_with(|| left.session_id.0.cmp(&right.session_id.0))
        });
        rows
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionDirection {
    Next,
    Previous,
}

fn move_selection(
    current_index: Option<usize>,
    visible_len: usize,
    direction: SelectionDirection,
) -> Option<usize> {
    if visible_len == 0 {
        return None;
    }
    Some(match (current_index, direction) {
        (None, SelectionDirection::Next) => 0,
        (None | Some(0), SelectionDirection::Previous) => visible_len - 1,
        (Some(index), SelectionDirection::Next) => (index + 1) % visible_len,
        (Some(index), SelectionDirection::Previous) => index - 1,
    })
}

/// Resumes the most-recently-updated `Draft` review in `store` whose
/// `source` matches exactly, or mints a fresh one when none does.
///
/// Comparing `ReviewSource` by `PartialEq` is an exact identity check
/// already (host id together with session id for `Session`, host id together
/// with PR number for `PullRequest`), so no separate lookup key is needed. A
/// corrupt review file surfaces via `ReviewStore::load_all`'s `Err` entries,
/// which this silently skips: it simply cannot match anything, the same as
/// an unrelated review would, never mistaking a broken file for "no draft
/// exists" in a way that could shadow a real one. Surfacing the corrupt-file
/// condition itself remains `ReviewStore`'s own concern, not this resume
/// lookup's.
fn resume_or_new_review(
    store: &ReviewStore,
    source: &ReviewSource,
    project: impl Into<String>,
    branch: impl Into<String>,
) -> Review {
    let existing = store
        .load_all()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|review| &review.source == source && review.status == ReviewStatus::Draft)
        .max_by(|left, right| left.updated_at.cmp(&right.updated_at));
    existing.unwrap_or_else(|| Review::new(source.clone(), project, branch))
}

/// Flattens every selectable line across every file/hunk of `model`, in
/// source order, so the Review tab's keyboard nav can move through one
/// continuous list spanning "files → hunks → lines".
fn flattened_review_line_targets(model: &DiffModel) -> Vec<ReviewLineTarget> {
    let mut targets = Vec::new();
    for (file_index, file) in model.files.iter().enumerate() {
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            for line_index in 0..hunk.lines.len() {
                targets.push(ReviewLineTarget {
                    file_index,
                    hunk_index,
                    line_index,
                });
            }
        }
    }
    targets
}

/// Resolves the `path`/`side`/`line` a new comment on `target` should anchor
/// to: the new-side line number when the line has one (an added or context
/// line), otherwise the old-side line number (a removed line has no new-side
/// counterpart). Returns `None` when `target` does not resolve to a line in
/// `model` (stale selection against a since-changed diff).
fn review_line_anchor(
    model: &DiffModel,
    target: ReviewLineTarget,
) -> Option<(String, ReviewSide, u32)> {
    let file = model.files.get(target.file_index)?;
    let hunk = file.hunks.get(target.hunk_index)?;
    let line = hunk.lines.get(target.line_index)?;
    let (side, number) = match (line.new_line, line.old_line) {
        (Some(new_line), _) => (ReviewSide::New, new_line),
        (None, Some(old_line)) => (ReviewSide::Old, old_line),
        (None, None) => return None,
    };
    Some((file.path.clone(), side, number))
}

fn visible_linear_issue_ids(state: &LinearProviderState) -> Vec<String> {
    state
        .issues
        .iter()
        .filter(|issue| linear_issue_matches_search(issue, &state.search))
        .map(|issue| issue.prompt_item_id().to_owned())
        .collect()
}

fn linear_issue_matches_search(issue: &providers::linear::LinearIssue, search: &str) -> bool {
    let search = search.trim().to_lowercase();
    search.is_empty()
        || issue.title.to_lowercase().contains(&search)
        || issue.identifier.to_lowercase().contains(&search)
        || issue.prompt_item_id().to_lowercase().contains(&search)
        || issue.branch.to_lowercase().contains(&search)
}

fn visible_github_pull_request_numbers(state: &GitHubProviderState) -> Vec<u64> {
    state
        .pull_requests
        .iter()
        .filter(|pull_request| github_pull_request_matches_search(pull_request, &state.search))
        .map(|pull_request| pull_request.number)
        .collect()
}

fn github_pull_request_matches_search(
    pull_request: &providers::github::GitHubPullRequest,
    search: &str,
) -> bool {
    let search = search.trim().to_lowercase();
    search.is_empty()
        || pull_request.title.to_lowercase().contains(&search)
        || pull_request.number.to_string().contains(&search)
        || pull_request.head_ref_name.to_lowercase().contains(&search)
}

fn visible_github_issue_numbers(state: &GitHubProviderState) -> Vec<u64> {
    state
        .issues
        .iter()
        .filter(|issue| github_issue_matches_search(issue, &state.search))
        .map(|issue| issue.number)
        .collect()
}

fn visible_github_provider_selections(state: &GitHubProviderState) -> Vec<GitHubProviderSelection> {
    let pull_requests = state
        .pull_requests
        .iter()
        .filter(|pull_request| github_pull_request_matches_search(pull_request, &state.search))
        .map(|pull_request| GitHubProviderSelection::PullRequest(pull_request.number));
    let issues = state
        .issues
        .iter()
        .filter(|issue| github_issue_matches_search(issue, &state.search))
        .map(|issue| GitHubProviderSelection::Issue(issue.number));
    pull_requests.chain(issues).collect()
}

fn github_issue_matches_search(issue: &providers::github::GitHubIssue, search: &str) -> bool {
    let search = search.trim().to_lowercase();
    search.is_empty()
        || issue.title.to_lowercase().contains(&search)
        || issue.number.to_string().contains(&search)
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

fn session_access(session: &SessionInfo) -> SessionAccess {
    if session.external == Some(true) {
        return SessionAccess::Unavailable;
    }

    if session.state.is_terminal() {
        return if session_can_resume(session) {
            SessionAccess::Resume
        } else {
            SessionAccess::Unavailable
        };
    }

    match session.runtime.as_ref().map(|runtime| runtime.state) {
        None | Some(RuntimeState::Live) if session.state == SessionState::Running => {
            SessionAccess::Attach
        }
        None | Some(RuntimeState::Starting | RuntimeState::Reconnecting) => SessionAccess::Pending,
        Some(RuntimeState::Lost) if session_can_resume(session) => SessionAccess::Resume,
        Some(
            RuntimeState::Terminal
            | RuntimeState::Lost
            | RuntimeState::Conflict
            | RuntimeState::Incompatible
            | RuntimeState::Live,
        ) => SessionAccess::Unavailable,
    }
}

fn session_can_resume(session: &SessionInfo) -> bool {
    session.capabilities.resume
        && (session
            .native_session_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            || session
                .native_session_path
                .as_deref()
                .is_some_and(|value| !value.is_empty()))
}

fn session_can_stop(session: &SessionInfo) -> bool {
    if session.external == Some(true) || session.state.is_terminal() {
        return false;
    }
    session.runtime.as_ref().is_none_or(|runtime| {
        matches!(
            runtime.state,
            RuntimeState::Live | RuntimeState::Starting | RuntimeState::Reconnecting
        )
    })
}

fn session_can_remove(session: &SessionInfo) -> bool {
    if session.external == Some(true) {
        return false;
    }
    if session.runtime.as_ref().is_some_and(|runtime| {
        matches!(
            runtime.state,
            RuntimeState::Conflict | RuntimeState::Incompatible
        )
    }) {
        return false;
    }
    session.state.is_terminal()
        || session_can_stop(session)
        || session
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == RuntimeState::Lost)
}

fn session_group(session: &SessionInfo, access: SessionAccess, needs_you: bool) -> SessionGroup {
    if needs_you {
        return SessionGroup::NeedsYou;
    }
    if session.state == SessionState::Starting
        || session.activity == Some(AgentActivity::Working)
        || access == SessionAccess::Pending
    {
        return SessionGroup::Running;
    }
    if access == SessionAccess::Attach
        && matches!(session.activity, None | Some(AgentActivity::Idle))
    {
        return SessionGroup::Ready;
    }
    SessionGroup::Unavailable
}

/// Session-list group in display priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionGroup {
    /// A session currently waiting for operator input, approval, or failure review.
    NeedsYou,
    /// An attachable live session that is not currently working.
    Ready,
    /// A session that is working, starting, or reconnecting.
    Running,
    /// A terminal, external, conflicting, incompatible, or otherwise unusable session.
    Unavailable,
}

/// Operator access currently available for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAccess {
    /// Attach to the existing live PTY now.
    Attach,
    /// Recover from native metadata, then attach to the new PTY.
    Resume,
    /// Wait for startup or daemon-to-worker reconnection to finish.
    Pending,
    /// No safe open operation is currently available.
    Unavailable,
}

/// Derived row for the prioritized session list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub host_id: HostId,
    pub session_id: SessionId,
    /// Owner-set display name, or `None` to show the session id.
    pub name: Option<String>,
    pub project_id: Option<String>,
    pub project_label: Option<String>,
    pub agent: String,
    pub activity: Option<AgentActivity>,
    pub state: SessionState,
    pub branch: Option<String>,
    pub group: SessionGroup,
    /// Current live owner-attention signal, distinct from unread history.
    pub attention: Option<SessionAttention>,
    pub access: SessionAccess,
    /// Whether a direct stop request is safe for this runtime state.
    pub can_stop: bool,
    /// Whether removal can safely stop or discard the current logical session.
    pub can_remove: bool,
}

/// Current owner-attention signal displayed directly on a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttention {
    /// Attention category used for the compact session-row label.
    pub kind: NotificationKind,
    /// Most relevant current notification title or detector fallback.
    pub title: String,
}

fn active_session_attention(host: &HostView, session: &SessionInfo) -> Option<SessionAttention> {
    let blocked = session.activity == Some(AgentActivity::Blocked);
    let failed = session.state == SessionState::Failed;
    let record = host
        .notifications
        .values()
        .filter(|record| {
            record.session_id.as_ref() == Some(&session.id)
                && matches!(
                    record.status,
                    NotificationStatus::Unread | NotificationStatus::Read
                )
                && (record.kind == NotificationKind::ApprovalRequired
                    || blocked && record.kind == NotificationKind::AgentBlocked
                    || failed
                        && (record.kind == NotificationKind::Error
                            || record.severity == NotificationSeverity::Error))
        })
        .max_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| right.id.0.cmp(&left.id.0))
        });
    record.map_or_else(
        || {
            if blocked {
                Some(SessionAttention {
                    kind: NotificationKind::AgentBlocked,
                    title: "Waiting for input".to_owned(),
                })
            } else if failed {
                Some(SessionAttention {
                    kind: NotificationKind::Error,
                    title: "Session failed".to_owned(),
                })
            } else {
                None
            }
        },
        |record| {
            Some(SessionAttention {
                kind: record.kind,
                title: record.title.clone(),
            })
        },
    )
}

fn apply_host_event(
    host: &mut HostView,
    host_id: &HostId,
    event: HostEvent,
    notifications: &mut Vec<NotificationIntent>,
    toasts: &mut Vec<Toast>,
    next_intent_id: &mut u64,
    runtime_continuity: &mut BTreeMap<(HostId, String), RuntimeContinuity>,
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
            if host
                .sessions
                .get(&session.id.0)
                .is_some_and(|previous| runtime_generation_changed(previous, &session))
            {
                runtime_continuity.insert(
                    (host_id.clone(), session.id.0.clone()),
                    RuntimeContinuity::Recovered,
                );
            }
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::SessionRemoved(session) => {
            host.sessions.remove(&session.id.0);
            runtime_continuity.remove(&(host_id.clone(), session.id.0));
        }
        HostEvent::RuntimeReconnected(session) => {
            runtime_continuity.insert(
                (host_id.clone(), session.id.0.clone()),
                RuntimeContinuity::Reconnected,
            );
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::NativeRecovered(session) => {
            runtime_continuity.insert(
                (host_id.clone(), session.id.0.clone()),
                RuntimeContinuity::Recovered,
            );
            host.sessions.insert(session.id.0.clone(), session);
        }
        HostEvent::RuntimeLost(session) | HostEvent::RuntimeConflict(session) => {
            host.sessions.insert(session.id.0.clone(), session);
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

fn observation_invalidation_for_host_event(host: &HostView, event: &HostEvent) -> Option<String> {
    let session = match event {
        HostEvent::SessionCreated(session)
        | HostEvent::SessionUpdated(session)
        | HostEvent::SessionStopped(session)
        | HostEvent::RuntimeReconnected(session)
        | HostEvent::NativeRecovered(session)
        | HostEvent::RuntimeLost(session)
        | HostEvent::RuntimeConflict(session) => session,
        HostEvent::SessionRemoved(session) => return Some(session.id.0.clone()),
        HostEvent::AgentState(_)
        | HostEvent::NotificationCreated(_)
        | HostEvent::NotificationUpdated(_)
        | HostEvent::NotificationDeleted(_)
        | HostEvent::Other(_) => return None,
    };
    host.sessions
        .get(&session.id.0)
        .is_some_and(|previous| runtime_generation_changed(previous, session))
        .then(|| session.id.0.clone())
}

fn session_runtime_identity(session: &SessionInfo) -> Option<(&str, protocol::RuntimeGeneration)> {
    session.runtime.as_ref().and_then(|runtime| {
        runtime
            .runtime_id
            .as_deref()
            .map(|runtime_id| (runtime_id, runtime.runtime_generation))
    })
}

fn same_runtime_generation(previous: &SessionInfo, current: &SessionInfo) -> bool {
    session_runtime_identity(previous)
        .zip(session_runtime_identity(current))
        .is_some_and(|(previous_identity, current_identity)| previous_identity == current_identity)
}

fn runtime_generation_changed(previous: &SessionInfo, current: &SessionInfo) -> bool {
    session_runtime_identity(previous) != session_runtime_identity(current)
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
        discovered_host_config, discovered_transport_addr, parse_agent_state, parse_event_message,
        subscribe_request, Backoff,
    };
    use crate::link::action_prompt_provider;
    use crate::sdk::notification_seed_queries;
    use crate::{
        render_attach_command, AttachTemplateValues, ConnectionOptions, HostSnapshot,
        HostTransport, DEFAULT_BACKOFF_MAX,
    };

    use super::*;

    fn test_event(name: &str, payload: serde_json::Value) -> Event {
        Event::new(protocol::PROTOCOL_VERSION, name, payload).expect("test event is valid")
    }

    #[test]
    fn workspace_applies_agent_state_to_known_session() {
        let mut workspace = Workspace::default();
        let session = session("s-1", None);
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session]),
        });

        let raw = test_event(
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

        let raw = test_event(
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
    fn session_rows_group_by_priority_and_order_stably_within_groups() {
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

        let rows = workspace.session_rows();
        let ids: Vec<&str> = rows.iter().map(|row| row.session_id.0.as_str()).collect();
        assert_eq!(ids, ["s-1", "s-3", "s-2"]);
        assert_eq!(rows[0].group, SessionGroup::NeedsYou);
        assert_eq!(rows[1].group, SessionGroup::Ready);
        assert_eq!(rows[2].group, SessionGroup::Running);
        assert_eq!(rows[2].name.as_deref(), Some("triage build"));
    }

    #[test]
    fn approval_notification_promotes_linked_session_without_promoting_unread_history() {
        let mut notification = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        notification.kind = NotificationKind::ApprovalRequired;
        notification.session_id = Some(SessionId("s-2".to_owned()));
        let mut history = notification_record(
            "n-2",
            NotificationStatus::Unread,
            NotificationSeverity::Info,
        );
        history.kind = NotificationKind::TurnCompleted;
        history.session_id = Some(SessionId("s-1".to_owned()));
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot_with_notifications(
                "local",
                vec![
                    session("s-1", Some(AgentActivity::Idle)),
                    session("s-2", Some(AgentActivity::Idle)),
                ],
                vec![notification, history],
            ),
        });

        let rows = workspace.session_rows();
        assert_eq!(rows[0].session_id.0, "s-2");
        assert_eq!(rows[0].group, SessionGroup::NeedsYou);
        assert_eq!(
            rows[0].attention.as_ref().map(|value| value.kind),
            Some(NotificationKind::ApprovalRequired)
        );
        assert_eq!(rows[1].group, SessionGroup::Ready);
        assert!(rows[1].attention.is_none());
    }

    #[test]
    fn failed_session_has_current_review_attention_without_a_notification() {
        let mut failed = session("failed", None);
        failed.state = SessionState::Failed;
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![failed]),
        });

        let rows = workspace.session_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, SessionGroup::NeedsYou);
        assert_eq!(
            rows[0].attention.as_ref().map(|attention| attention.kind),
            Some(NotificationKind::Error)
        );
    }

    #[test]
    fn session_access_and_actions_fail_closed_for_unsafe_runtime_states() {
        let mut conflict = session_with_runtime("conflict", "runtime-conflict");
        conflict.runtime.as_mut().expect("runtime").state = RuntimeState::Conflict;
        let mut incompatible = session_with_runtime("incompatible", "runtime-incompatible");
        incompatible.runtime.as_mut().expect("runtime").state = RuntimeState::Incompatible;
        let mut external = session("external", Some(AgentActivity::Idle));
        external.external = Some(true);

        for session in [&conflict, &incompatible, &external] {
            assert_eq!(session_access(session), SessionAccess::Unavailable);
            assert!(!session_can_stop(session));
            assert!(!session_can_remove(session));
        }

        let mut lost = session_with_runtime("lost", "runtime-lost");
        lost.runtime.as_mut().expect("runtime").state = RuntimeState::Lost;
        assert_eq!(session_access(&lost), SessionAccess::Unavailable);
        assert!(!session_can_stop(&lost));
        assert!(session_can_remove(&lost));
    }

    #[test]
    fn terminal_session_requires_capability_and_native_reference_to_resume() {
        let mut terminal = session("terminal", None);
        terminal.state = SessionState::Done;
        assert_eq!(session_access(&terminal), SessionAccess::Unavailable);
        assert!(session_can_remove(&terminal));

        terminal.native_session_id = Some("native-1".to_owned());
        assert_eq!(session_access(&terminal), SessionAccess::Resume);

        terminal.capabilities.resume = false;
        assert_eq!(session_access(&terminal), SessionAccess::Unavailable);
    }

    #[test]
    fn lost_session_can_resume_but_remains_in_unavailable_group() {
        let mut lost = session_with_runtime("lost", "runtime-lost");
        lost.runtime.as_mut().expect("runtime").state = RuntimeState::Lost;
        lost.native_session_path = Some("/tmp/native-session.json".to_owned());

        let access = session_access(&lost);
        assert_eq!(access, SessionAccess::Resume);
        assert_eq!(
            session_group(&lost, access, false),
            SessionGroup::Unavailable
        );
        assert!(session_can_remove(&lost));
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
    fn session_forked_inserts_new_session_snapshot() {
        let mut source = session("s-1", None);
        source.state = SessionState::Running;
        let mut forked = source.clone();
        forked.id = SessionId("s-2".to_owned());
        forked.pid = 99;

        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![source]),
        });

        workspace.apply(DomainEvent::SessionForked {
            host_id: HostId::new("local"),
            result: protocol::SessionForkResult {
                session: forked,
                applied_input: None,
            },
        });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.sessions.contains_key("s-1"));
        let session = host.sessions.get("s-2").expect("forked session");
        assert_eq!(session.state, SessionState::Running);
        assert_eq!(session.pid, 99);
    }

    #[test]
    fn observation_events_reduce_into_headless_state() {
        let host_id = HostId::new("local");
        let session = session("s-observe", None);
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![session.clone()]),
        });
        let runtime = protocol::SessionRuntimeIdentity::new(
            "runtime-observe",
            protocol::RuntimeGeneration::new(3),
        )
        .expect("valid test runtime");
        workspace.apply(DomainEvent::SessionScreenLoaded {
            host_id: host_id.clone(),
            result: protocol::SessionScreenResult {
                session_id: session.id.clone(),
                worker_id: "worker-observe".to_owned(),
                runtime: runtime.clone(),
                watermark: protocol::TerminalWatermark::new(7),
                dimensions: protocol::TerminalDimensions::new(80, 24).expect("valid dimensions"),
                cursor: protocol::TerminalCursor {
                    row: 0,
                    col: 5,
                    visible: true,
                },
                alternate_screen: false,
                title: None,
                progress: None,
                visible_lines: vec!["hello".to_owned()],
            },
        });
        workspace.apply(DomainEvent::SessionOutputLoaded {
            host_id: host_id.clone(),
            result: protocol::SessionOutputResult::new(
                session.id.clone(),
                runtime,
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(5),
                protocol::OutputOffset::new(5),
                "aGVsbG8=",
                None,
                false,
                false,
            )
            .expect("valid output result"),
        });
        workspace.apply(DomainEvent::SessionWaitCompleted {
            host_id: host_id.clone(),
            result: protocol::SessionWaitResult {
                reason: protocol::SessionWaitReason::Timeout,
                session: session.clone(),
                terminal_watermark: Some(protocol::TerminalWatermark::new(7)),
                output_offset: Some(protocol::OutputOffset::new(5)),
            },
        });

        let observation = workspace
            .session_observation(&host_id, &session.id)
            .expect("observation state");
        assert_eq!(observation.output_text, "hello");
        assert_eq!(
            observation
                .screen
                .as_ref()
                .expect("screen snapshot")
                .visible_lines
                .as_slice(),
            ["hello".to_owned()].as_slice()
        );
        assert_eq!(
            observation.wait.as_ref().map(|wait| wait.reason),
            Some(protocol::SessionWaitReason::Timeout)
        );
    }

    #[test]
    fn runtime_identity_changes_and_typed_errors_discard_observation_cursors() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-observe".to_owned());
        let original = session_with_runtime(&session_id.0, "runtime-stable");
        let runtime_one = protocol::SessionRuntimeIdentity::new(
            "runtime-stable",
            protocol::RuntimeGeneration::new(1),
        )
        .expect("valid runtime identity");
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", vec![original.clone()]),
        });
        workspace.apply(DomainEvent::SessionOutputLoaded {
            host_id: host_id.clone(),
            result: protocol::SessionOutputResult::new(
                session_id.clone(),
                runtime_one,
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(1),
                protocol::OutputOffset::new(1),
                "YQ==",
                None,
                false,
                false,
            )
            .expect("valid output result"),
        });
        assert!(workspace
            .session_observation(&host_id, &session_id)
            .and_then(|observation| observation.output_cursor)
            .is_some());

        let mut recovered = original;
        recovered
            .runtime
            .as_mut()
            .expect("runtime")
            .runtime_generation = protocol::RuntimeGeneration::new(2);
        workspace.apply(DomainEvent::HostEvent {
            host_id: host_id.clone(),
            event: HostEvent::SessionUpdated(recovered),
        });
        assert!(workspace
            .session_observation(&host_id, &session_id)
            .is_none());

        let runtime_two = protocol::SessionRuntimeIdentity::new(
            "runtime-stable",
            protocol::RuntimeGeneration::new(2),
        )
        .expect("valid runtime identity");
        workspace.apply(DomainEvent::SessionOutputLoaded {
            host_id: host_id.clone(),
            result: protocol::SessionOutputResult::new(
                session_id.clone(),
                runtime_two,
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(1),
                protocol::OutputOffset::new(1),
                "Yg==",
                None,
                false,
                false,
            )
            .expect("valid output result"),
        });
        workspace.apply(DomainEvent::SessionObservationRuntimeChanged {
            host_id: host_id.clone(),
            session_id: session_id.clone(),
            error: "runtime changed".to_owned(),
        });
        assert!(workspace
            .session_observation(&host_id, &session_id)
            .is_none());
    }

    #[test]
    fn provider_policy_events_reduce_into_headless_state() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot("local", Vec::new()),
        });
        let kind_policy = NotificationKindPolicy {
            agent_blocked: true,
            approval_required: true,
            turn_completed: true,
            session_finished: true,
            error: true,
            system: true,
        };
        workspace.apply(DomainEvent::NotificationPolicyLoaded {
            host_id: host_id.clone(),
            result: protocol::NotificationPolicyResult {
                policy: NotificationPolicy {
                    attention_dedupe_window_secs: 30,
                    attention_debounce_secs: 5,
                    enabled: kind_policy.clone(),
                    providers: BTreeMap::from([("future-agent".to_owned(), kind_policy)]),
                    retention: protocol::NotificationRetentionPolicy::default(),
                },
            },
        });
        assert!(workspace.set_notification_policy_kind(
            &host_id,
            Some("future-agent"),
            NotificationKind::System,
            false,
        ));
        let policy = workspace
            .notification_policy(&host_id)
            .expect("policy state");
        assert!(!policy.providers["future-agent"].system);
        assert!(policy.enabled.system, "provider edit preserves base policy");
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
        let raw = test_event(
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
                supported_agents: Vec::new(),
                runtimes: Vec::new(),
                notification_providers: Vec::new(),
                observation_capabilities: ObservationCapabilities::default(),
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
    fn provider_keyboard_selection_linear_issues_wraps_through_filtered_rows() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_linear_search(host_id.clone(), "launch".to_owned());
        workspace
            .hosts
            .get_mut(&host_id)
            .expect("host")
            .provider
            .linear
            .issues = vec![
            linear_issue("LIN-1", "Fix launcher", "lin-1-fix-launcher"),
            linear_issue("LIN-2", "Update docs", "lin-2-update-docs"),
            linear_issue("OPS-3", "Launch checklist", "ops-3-checklist"),
        ];

        assert_eq!(
            workspace.select_next_linear_issue(&host_id).as_deref(),
            Some("LIN-1")
        );
        assert_eq!(
            workspace.select_next_linear_issue(&host_id).as_deref(),
            Some("OPS-3")
        );
        assert_eq!(
            workspace.select_next_linear_issue(&host_id).as_deref(),
            Some("LIN-1")
        );
        assert_eq!(
            workspace.select_previous_linear_issue(&host_id).as_deref(),
            Some("OPS-3")
        );
    }

    #[test]
    fn provider_keyboard_selection_linear_issues_match_identifier_and_branch() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_linear_search(host_id.clone(), "eng-22".to_owned());
        workspace
            .hosts
            .get_mut(&host_id)
            .expect("host")
            .provider
            .linear
            .issues = vec![
            linear_issue("ENG-21", "Unrelated", "eng-21-old"),
            linear_issue("ENG-22", "Narrow match", "feature/narrow-match"),
        ];

        assert_eq!(
            workspace.select_next_linear_issue(&host_id).as_deref(),
            Some("ENG-22")
        );

        workspace.set_linear_search(host_id.clone(), "narrow-match".to_owned());
        assert_eq!(
            workspace.select_next_linear_issue(&host_id).as_deref(),
            Some("ENG-22")
        );
    }

    #[test]
    fn provider_keyboard_selection_github_pull_requests_wraps_through_filtered_rows() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_github_search(host_id.clone(), "stack".to_owned());
        workspace
            .hosts
            .get_mut(&host_id)
            .expect("host")
            .provider
            .github
            .pull_requests = vec![
            github_pull_request(7, "Stack navigation", "feature/stack-nav"),
            github_pull_request(8, "Release notes", "docs/release"),
            github_pull_request(9, "Provider focus", "stack-provider-focus"),
        ];

        assert_eq!(workspace.select_next_github_pull_request(&host_id), Some(7));
        assert_eq!(workspace.select_next_github_pull_request(&host_id), Some(9));
        assert_eq!(workspace.select_next_github_pull_request(&host_id), Some(7));
        assert_eq!(
            workspace.select_previous_github_pull_request(&host_id),
            Some(9)
        );
    }

    #[test]
    fn provider_keyboard_selection_github_pull_requests_match_number() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_github_search(host_id.clone(), "42".to_owned());
        workspace
            .hosts
            .get_mut(&host_id)
            .expect("host")
            .provider
            .github
            .pull_requests = vec![
            github_pull_request(41, "Unrelated", "feature/unrelated"),
            github_pull_request(42, "Exact number", "feature/exact-number"),
        ];

        assert_eq!(
            workspace.select_next_github_pull_request(&host_id),
            Some(42)
        );
    }

    #[test]
    fn provider_keyboard_selection_github_issues_wraps_through_filtered_rows() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_github_search(host_id.clone(), "nav".to_owned());
        workspace
            .hosts
            .get_mut(&host_id)
            .expect("host")
            .provider
            .github
            .issues = vec![
            github_issue(11, "Keyboard navigation"),
            github_issue(12, "Release docs"),
            github_issue(13, "Navigation focus"),
        ];

        assert_eq!(workspace.select_next_github_issue(&host_id), Some(11));
        assert_eq!(workspace.select_next_github_issue(&host_id), Some(13));
        assert_eq!(workspace.select_next_github_issue(&host_id), Some(11));
        assert_eq!(workspace.select_previous_github_issue(&host_id), Some(13));
    }

    #[test]
    fn provider_keyboard_selection_github_issues_clear_pull_request_selection() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        let host = workspace.host_for_ui(host_id.clone());
        host.provider.github.selected_pull_request = Some(7);
        host.provider.github.issues = vec![github_issue(11, "Keyboard navigation")];

        assert_eq!(workspace.select_next_github_issue(&host_id), Some(11));

        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(host.provider.github.selected_pull_request, None);
        assert_eq!(host.provider.github.selected_issue, Some(11));
    }

    #[test]
    fn provider_keyboard_selection_github_items_moves_between_pull_requests_and_issues() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.set_github_search(host_id.clone(), "nav".to_owned());
        let host = workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.github.pull_requests = vec![
            github_pull_request(7, "Stack navigation", "feature/stack-nav"),
            github_pull_request(8, "Release notes", "docs/release"),
        ];
        host.provider.github.issues = vec![
            github_issue(11, "Keyboard navigation"),
            github_issue(12, "Release docs"),
            github_issue(13, "Navigation focus"),
        ];

        assert_eq!(
            workspace.select_next_github_item(&host_id),
            Some(GitHubProviderSelection::PullRequest(7))
        );
        assert_eq!(
            workspace.select_next_github_item(&host_id),
            Some(GitHubProviderSelection::Issue(11))
        );
        assert_eq!(
            workspace.select_next_github_item(&host_id),
            Some(GitHubProviderSelection::Issue(13))
        );
        assert_eq!(
            workspace.select_next_github_item(&host_id),
            Some(GitHubProviderSelection::PullRequest(7))
        );
        assert_eq!(
            workspace.select_previous_github_item(&host_id),
            Some(GitHubProviderSelection::Issue(13))
        );
    }

    #[test]
    fn provider_keyboard_selection_empty_and_unknown_hosts_noop() {
        let host_id = HostId::new("local");
        let missing_host_id = HostId::new("missing");
        let mut workspace = Workspace::default();
        workspace.set_linear_search(host_id.clone(), "nothing".to_owned());
        workspace.set_github_search(host_id.clone(), "nothing".to_owned());
        let host = workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.linear.selected_issue_id = Some("ENG-404".to_owned());
        host.provider.github.selected_pull_request = Some(404);
        host.provider.github.selected_issue = Some(405);

        assert_eq!(workspace.select_next_linear_issue(&missing_host_id), None);
        assert_eq!(workspace.select_previous_linear_issue(&host_id), None);
        assert_eq!(
            workspace.select_next_github_pull_request(&missing_host_id),
            None
        );
        assert_eq!(
            workspace.select_previous_github_pull_request(&host_id),
            None
        );
        assert_eq!(workspace.select_next_github_issue(&missing_host_id), None);
        assert_eq!(workspace.select_previous_github_issue(&host_id), None);
        let host = workspace.hosts.get(&host_id).expect("host");
        assert_eq!(
            host.provider.linear.selected_issue_id.as_deref(),
            Some("ENG-404")
        );
        assert_eq!(host.provider.github.selected_pull_request, Some(404));
        assert_eq!(host.provider.github.selected_issue, Some(405));
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

        let raw = test_event(
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
        let mut snapshot = snapshot_with_notifications(
            "local",
            vec![session("s-1", None)],
            vec![notification_record(
                "n-1",
                NotificationStatus::Unread,
                NotificationSeverity::ActionRequired,
            )],
        );
        snapshot.notification_providers = vec!["future-agent".to_owned()];
        snapshot.runtimes = vec![protocol::AgentRuntime {
            agent: "hermes-review".to_owned(),
            agent_base: Some(protocol::AgentKind::Hermes),
            available: true,
            path: Some("/usr/bin/hermes".to_owned()),
            version: Some("0.2.0".to_owned()),
            supported: Some(true),
        }];
        workspace.apply(DomainEvent::HostSnapshotLoaded { snapshot });

        let host = workspace.hosts.get(&HostId::new("local")).expect("host");
        assert!(host.notifications.contains_key("n-1"));
        assert_eq!(host.notification_providers, ["future-agent"]);
        assert_eq!(host.launchable_agents(), ["hermes-review"]);
    }

    #[test]
    fn notification_created_event_parses_from_subscription_line() {
        let record = notification_record(
            "n-1",
            NotificationStatus::Unread,
            NotificationSeverity::ActionRequired,
        );
        let event = test_event(
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
        let event = test_event(
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
        let event = test_event(
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
    fn runtime_reconnect_event_parses_from_subscription_line() {
        let expected = session_with_runtime("s-runtime", "runtime-1");
        let event = test_event(
            event::SESSION_RUNTIME_RECONNECTED,
            serde_json::json!({ "session": expected }),
        );
        let line = serde_json::to_string(&event).expect("line");

        let message = parse_event_message(&HostId::new("local"), &line).expect("parse");
        match message {
            DomainEvent::HostEvent {
                event: HostEvent::RuntimeReconnected(parsed),
                ..
            } => assert_eq!(parsed.id, SessionId("s-runtime".to_owned())),
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
    fn reconnect_snapshot_distinguishes_same_runtime_from_recovery() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![session_with_runtime("s-runtime", "runtime-1")],
            ),
        });

        workspace.apply(DomainEvent::HostConnecting {
            host_id: host_id.clone(),
        });
        workspace.apply(DomainEvent::HostSubscribed {
            host_id: host_id.clone(),
        });
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![session_with_runtime("s-runtime", "runtime-1")],
            ),
        });
        assert_eq!(
            workspace.runtime_continuity(&host_id, &SessionId("s-runtime".to_owned())),
            Some(RuntimeContinuity::Reconnected)
        );

        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![session_with_runtime("s-runtime", "runtime-2")],
            ),
        });
        assert_eq!(
            workspace.runtime_continuity(&host_id, &SessionId("s-runtime".to_owned())),
            Some(RuntimeContinuity::Recovered)
        );
    }

    #[test]
    fn runtime_events_update_session_and_continuity() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-runtime".to_owned());
        let mut workspace = Workspace::default();
        workspace.apply(DomainEvent::HostSnapshotLoaded {
            snapshot: snapshot(
                "local",
                vec![session_with_runtime("s-runtime", "runtime-1")],
            ),
        });

        let mut lost = session_with_runtime("s-runtime", "runtime-1");
        let runtime = lost.runtime.as_mut().expect("runtime");
        runtime.state = protocol::RuntimeState::Lost;
        runtime.loss_reason = Some("worker_missing".to_owned());
        workspace.apply(DomainEvent::HostEvent {
            host_id: host_id.clone(),
            event: HostEvent::RuntimeLost(lost),
        });
        assert_eq!(
            workspace
                .hosts
                .get(&host_id)
                .expect("host")
                .sessions
                .get(&session_id.0)
                .and_then(|session| session.runtime.as_ref())
                .map(|runtime| runtime.state),
            Some(protocol::RuntimeState::Lost)
        );

        workspace.apply(DomainEvent::HostEvent {
            host_id: host_id.clone(),
            event: HostEvent::NativeRecovered(session_with_runtime("s-runtime", "runtime-2")),
        });
        assert_eq!(
            workspace.runtime_continuity(&host_id, &session_id),
            Some(RuntimeContinuity::Recovered)
        );
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
    fn activity_scopes_separate_recent_unread_and_archived_records() {
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

        let recent =
            workspace.inbox_rows(NotificationScope::Recent, &NotificationFilter::default());
        let unread =
            workspace.inbox_rows(NotificationScope::Unread, &NotificationFilter::default());
        let archived =
            workspace.inbox_rows(NotificationScope::Archived, &NotificationFilter::default());

        assert_eq!(recent.len(), 3);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].record.id.0, "n-unread");
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].record.id.0, "n-archived-error");
    }

    #[test]
    fn activity_rows_remain_newest_first_regardless_of_read_or_action_state() {
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
        blocked_but_read.created_at = "2026-01-01T00:00:00Z".to_owned();
        let mut unread_system = notification_record(
            "n-unread",
            NotificationStatus::Unread,
            NotificationSeverity::Info,
        );
        unread_system.kind = NotificationKind::System;
        unread_system.created_at = "2026-01-03T00:00:00Z".to_owned();
        let mut read_system = notification_record(
            "n-read",
            NotificationStatus::Read,
            NotificationSeverity::Info,
        );
        read_system.kind = NotificationKind::System;
        read_system.created_at = "2026-01-02T00:00:00Z".to_owned();
        for record in [read_system, unread_system, blocked_but_read] {
            workspace.apply(notification_created("local", record));
        }

        let rows = workspace.inbox_rows(NotificationScope::Recent, &NotificationFilter::default());
        let ids: Vec<&str> = rows.iter().map(|row| row.record.id.0.as_str()).collect();

        assert_eq!(ids, vec!["n-unread", "n-read", "n-blocked"]);
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
        let rows = workspace.inbox_rows(NotificationScope::Recent, &filter);

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
    fn discovered_transport_validates_address_but_caches_stable_identity() {
        let record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: Some("dev.example.netbird.cloud".to_owned()),
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: Some("peer-7".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };

        assert_eq!(
            discovered_transport_addr(&record).expect("transport address"),
            "100.92.30.40:18722".parse().expect("socket address")
        );
        let host = discovered_host_config(&record).expect("discovered host config");
        assert_eq!(host.id.as_str(), "netbird:peer-7");
        assert_eq!(host.attach_host(), "netbird:peer-7@18722");
        assert!(matches!(host.transport, HostTransport::Remote { .. }));
    }

    #[test]
    fn discovered_transport_requires_stable_identity_and_non_zero_port() {
        let mut record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: None,
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: None,
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };

        assert!(matches!(
            discovered_host_config(&record),
            Err(CoreError::MissingDiscoveredStableIdentity)
        ));

        record.peer_id = Some(String::new());
        record.fqdn = Some(String::new());
        assert!(matches!(
            discovered_host_config(&record),
            Err(CoreError::MissingDiscoveredStableIdentity)
        ));

        record.peer_id = None;
        record.fqdn = Some("dev.example.netbird.cloud".to_owned());
        record.port = 0;
        assert!(matches!(
            discovered_host_config(&record),
            Err(CoreError::InvalidDiscoveredPort)
        ));
    }

    #[test]
    fn discovered_reconnect_identity_ignores_ip_changes_and_reassigned_peers() {
        let record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: Some("dev.example.netbird.cloud".to_owned()),
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: Some("stable-key".to_owned()),
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };
        let first = discovered_host_config(&record).expect("first GUI route");

        let mut moved = record.clone();
        moved.address = Some("100.92.30.41".to_owned());
        let moved = discovered_host_config(&moved).expect("moved GUI route");
        assert_eq!(moved, first);

        let mut reassigned = record;
        reassigned.peer_id = Some("different-key".to_owned());
        let reassigned = discovered_host_config(&reassigned).expect("reassigned GUI route");
        assert_ne!(reassigned, first);
        assert_eq!(first.attach_host(), "netbird:stable-key@18722");
        assert_eq!(reassigned.attach_host(), "netbird:different-key@18722");
    }

    #[test]
    fn discovered_reconnect_identity_falls_back_to_fqdn() {
        let record = HostRecord {
            name: Some("dev".to_owned()),
            fqdn: Some("dev.example.netbird.cloud".to_owned()),
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: None,
            class: HostClass::ReachableDaemon {
                daemon_version: "0.5.0".to_owned(),
            },
        };

        let host = discovered_host_config(&record).expect("FQDN GUI route");
        assert_eq!(host.id.as_str(), "netbird:dev.example.netbird.cloud");
        assert_eq!(
            host.attach_host(),
            "netbird:dev.example.netbird.cloud@18722"
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

        assert_eq!(first.method(), method::SUBSCRIBE);
        assert_eq!(first.params(), &Value::Null);
        assert!(first.id().starts_with("sdk-subscribe-"));
        assert!(second.id().starts_with("sdk-subscribe-"));
        assert_ne!(first.id(), second.id());
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
            supported_agents: Vec::new(),
            runtimes: Vec::new(),
            notification_providers: Vec::new(),
            observation_capabilities: ObservationCapabilities::default(),
        }
    }

    fn session(id: &str, activity: Option<AgentActivity>) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            external: Some(false),
            capabilities: protocol::SessionCapabilities {
                resume: true,
                fork: true,
            },
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/repo"),
            cwd_source: Some(protocol::CwdSource::Launch),
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
            active_agent_pid: None,
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
            runtime: None,
        }
    }

    fn session_with_runtime(id: &str, runtime_id: &str) -> SessionInfo {
        let mut session = session(id, None);
        session.runtime = Some(protocol::SessionRuntime {
            state: protocol::RuntimeState::Live,
            runtime_generation: protocol::RuntimeGeneration::new(1),
            worker_id: Some(format!("worker-{runtime_id}")),
            runtime_id: Some(runtime_id.to_owned()),
            started_at: Some("2026-01-01T00:00:00Z".to_owned()),
            last_connected_at: Some("2026-01-01T00:00:01Z".to_owned()),
            loss_reason: None,
        });
        session
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

    fn linear_issue(identifier: &str, title: &str, branch: &str) -> providers::linear::LinearIssue {
        providers::linear::LinearIssue {
            id: format!("opaque-{identifier}"),
            identifier: identifier.to_owned(),
            title: title.to_owned(),
            body: String::new(),
            branch: branch.to_owned(),
            url: format!("https://linear.test/{identifier}"),
            state: None,
            state_type: None,
            assignee: None,
            updated_at: None,
        }
    }

    fn github_pull_request(
        number: u64,
        title: &str,
        head_ref_name: &str,
    ) -> providers::github::GitHubPullRequest {
        providers::github::GitHubPullRequest::new(
            number,
            title,
            "",
            head_ref_name,
            format!("https://github.example/repo/pull/{number}"),
        )
    }

    fn github_issue(number: u64, title: &str) -> providers::github::GitHubIssue {
        providers::github::GitHubIssue {
            number,
            title: title.to_owned(),
            body: String::new(),
            url: format!("https://github.example/repo/issues/{number}"),
            branch: None,
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

    #[test]
    fn review_dispatch_modal_agent_defaults_and_can_be_overridden() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.host_for_ui(host_id.clone());

        workspace.open_review_dispatch_modal(
            &host_id,
            Ok("rendered prompt".to_owned()),
            "codex".to_owned(),
            false,
        );
        assert_eq!(
            workspace
                .hosts
                .get(&host_id)
                .and_then(|host| host.review.dispatch.as_ref())
                .map(|dispatch| dispatch.agent.as_str()),
            Some("codex")
        );

        workspace.set_review_dispatch_agent(&host_id, "shell".to_owned());
        assert_eq!(
            workspace
                .hosts
                .get(&host_id)
                .and_then(|host| host.review.dispatch.as_ref())
                .map(|dispatch| dispatch.agent.as_str()),
            Some("shell")
        );

        workspace.close_review_dispatch_modal(&host_id);
        assert!(workspace
            .hosts
            .get(&host_id)
            .is_some_and(|host| host.review.dispatch.is_none()));
    }

    #[test]
    fn set_review_dispatch_agent_is_a_no_op_without_an_open_modal() {
        let host_id = HostId::new("local");
        let mut workspace = Workspace::default();
        workspace.host_for_ui(host_id.clone());

        workspace.set_review_dispatch_agent(&host_id, "shell".to_owned());

        assert!(workspace
            .hosts
            .get(&host_id)
            .is_some_and(|host| host.review.dispatch.is_none()));
    }

    fn review_resume_store_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-gui-core-state-review-resume-{tag}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn begin_review_from_session_resumes_a_persisted_draft_for_the_same_source() {
        let store = ReviewStore::new(review_resume_store_dir("same-source"));
        let host_id = HostId::new("local");
        let mut source_session = session("s-1", None);
        source_session.branch = Some("feature/x".to_owned());

        // First "app run": open the review, add a comment, and persist it —
        // exactly what `save_review_comment` does, but driven directly here
        // since that method also needs an open comment editor.
        let mut workspace = Workspace::default();
        workspace.begin_review_from_session(host_id.clone(), &store, &source_session, "project-1");
        let review_id = {
            let host = workspace.hosts.get_mut(&host_id).expect("host");
            let review = host
                .review
                .active_review
                .as_mut()
                .expect("fresh draft on first open");
            review.add_comment(ReviewComment::new("src/lib.rs", ReviewSide::New, 1, "lgtm"));
            store.save(review).expect("persist draft with comment");
            review.id.clone()
        };

        // "Restart": a brand-new in-memory `Workspace` (nothing carried over
        // except the on-disk store) opening a review for the exact same
        // source must resume the persisted draft, comment included.
        let mut restarted = Workspace::default();
        restarted.begin_review_from_session(host_id.clone(), &store, &source_session, "project-1");
        let resumed = restarted
            .hosts
            .get(&host_id)
            .expect("host")
            .review
            .active_review
            .as_ref()
            .expect("resumed review");
        assert_eq!(resumed.id, review_id);
        assert_eq!(resumed.comments.len(), 1);
        assert_eq!(resumed.comments[0].text, "lgtm");

        // A different source (different session id) must not resume this
        // unrelated draft — it mints its own fresh, empty one instead.
        let mut other_session = session("s-2", None);
        other_session.branch = Some("feature/y".to_owned());
        let mut other = Workspace::default();
        other.begin_review_from_session(host_id.clone(), &store, &other_session, "project-1");
        let fresh = other
            .hosts
            .get(&host_id)
            .expect("host")
            .review
            .active_review
            .as_ref()
            .expect("fresh draft for a different source");
        assert_ne!(fresh.id, review_id);
        assert!(fresh.comments.is_empty());
    }

    #[test]
    fn begin_review_from_pull_request_resumes_a_persisted_draft_for_the_same_pr() {
        let store = ReviewStore::new(review_resume_store_dir("same-pr"));
        let host_id = HostId::new("local");

        let mut workspace = Workspace::default();
        workspace.begin_review_from_pull_request(
            host_id.clone(),
            &store,
            42,
            "project-1",
            "feature/pr-42",
        );
        let review_id = {
            let host = workspace.hosts.get_mut(&host_id).expect("host");
            let review = host
                .review
                .active_review
                .as_mut()
                .expect("fresh draft on first open");
            review.add_comment(ReviewComment::new("README.md", ReviewSide::Old, 3, "typo"));
            store.save(review).expect("persist draft with comment");
            review.id.clone()
        };

        let mut restarted = Workspace::default();
        restarted.begin_review_from_pull_request(
            host_id.clone(),
            &store,
            42,
            "project-1",
            "feature/pr-42",
        );
        let resumed = restarted
            .hosts
            .get(&host_id)
            .expect("host")
            .review
            .active_review
            .as_ref()
            .expect("resumed review");
        assert_eq!(resumed.id, review_id);
        assert_eq!(resumed.comments.len(), 1);

        // A different PR number on the same host must not resume it.
        let mut other = Workspace::default();
        other.begin_review_from_pull_request(
            host_id.clone(),
            &store,
            43,
            "project-1",
            "feature/pr-43",
        );
        let fresh = other
            .hosts
            .get(&host_id)
            .expect("host")
            .review
            .active_review
            .as_ref()
            .expect("fresh draft for a different PR");
        assert_ne!(fresh.id, review_id);
        assert!(fresh.comments.is_empty());
    }

    #[test]
    fn begin_review_from_session_ignores_a_dispatched_draft_and_mints_a_fresh_one() {
        let store = ReviewStore::new(review_resume_store_dir("dispatched-not-resumed"));
        let host_id = HostId::new("local");
        let mut source_session = session("s-1", None);
        source_session.branch = Some("feature/x".to_owned());

        let mut workspace = Workspace::default();
        workspace.begin_review_from_session(host_id.clone(), &store, &source_session, "project-1");
        let dispatched_id = {
            let host = workspace.hosts.get_mut(&host_id).expect("host");
            let review = host
                .review
                .active_review
                .as_mut()
                .expect("fresh draft on first open");
            review.mark_dispatched(SessionId("s-dispatched".to_owned()));
            store.save(review).expect("persist dispatched review");
            review.id.clone()
        };

        let mut restarted = Workspace::default();
        restarted.begin_review_from_session(host_id.clone(), &store, &source_session, "project-1");
        let fresh = restarted
            .hosts
            .get(&host_id)
            .expect("host")
            .review
            .active_review
            .as_ref()
            .expect("fresh draft, not the dispatched one");
        assert_ne!(fresh.id, dispatched_id);
        assert_eq!(fresh.status, ReviewStatus::Draft);
    }
}
